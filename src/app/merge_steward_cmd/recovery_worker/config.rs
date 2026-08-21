use super::{
    BTreeMap, BTreeSet, CliFailure, DEFAULT_FIRST_LINE_MODEL, DEFAULT_MAX_LOG_TAIL_BYTES,
    DEFAULT_TIMEOUT_SECONDS, DISABLED_CODEX_FEATURES, FORCED_REASONING_CONFIG, LoadedConfig,
    MAX_LOG_TAIL_BYTES, MAX_TIMEOUT_SECONDS, POLICY_KEY, Path, PathBuf,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;

const POLICY_FIELDS: &[&str] = &[
    "enabled",
    "provider",
    "first_line_model",
    "escalation_model",
    "codex_binary",
    "codex_home",
    "timeout_seconds",
    "max_attempts_per_head",
    "max_log_tail_bytes",
    "allowed_repositories",
    "repo_paths",
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct RecoveryWorkerPolicy {
    pub(super) enabled: bool,
    pub(super) provider: String,
    pub(super) first_line_model: String,
    pub(super) escalation_model: Option<String>,
    pub(super) codex_binary: PathBuf,
    pub(super) codex_home: PathBuf,
    pub(super) timeout_seconds: u64,
    pub(super) max_attempts_per_head: u32,
    pub(super) max_log_tail_bytes: usize,
    pub(super) allowed_repositories: BTreeSet<String>,
    pub(super) repo_paths: BTreeMap<String, PathBuf>,
}

pub(super) struct ClaimPolicySnapshot {
    pub(super) policy: RecoveryWorkerPolicy,
    pub(super) signature: String,
    pub(super) config: LoadedConfig,
}

pub(super) enum ClaimPolicyRefresh {
    Current(Box<ClaimPolicySnapshot>),
    Drifted { observed_signature: String },
}

impl RecoveryWorkerPolicy {
    pub(super) fn load(global_dir: &Path) -> Result<(Self, String, LoadedConfig), CliFailure> {
        let config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf()).map_err(
            |error| {
                CliFailure::new(
                    1,
                    format!("failed to load trusted recovery-worker policy: {error}"),
                )
            },
        )?;
        let policy = Self::from_config(&config)?;
        let signature = policy.signature()?;
        Ok((policy, signature, config))
    }

    pub(super) fn signature(&self) -> Result<String, CliFailure> {
        let encoded = serde_json::to_vec(self).map_err(|error| {
            CliFailure::new(1, format!("failed to sign recovery-worker policy: {error}"))
        })?;
        Ok(format!("{:x}", Sha256::digest(encoded)))
    }

    pub(super) fn refresh_for_claim(
        global_dir: &Path,
        expected_signature: &str,
    ) -> Result<ClaimPolicyRefresh, CliFailure> {
        let (policy, signature, config) = Self::load(global_dir)?;
        if signature == expected_signature {
            Ok(ClaimPolicyRefresh::Current(Box::new(ClaimPolicySnapshot {
                policy,
                signature,
                config,
            })))
        } else {
            Ok(ClaimPolicyRefresh::Drifted {
                observed_signature: signature,
            })
        }
    }

    pub(super) fn from_config(config: &LoadedConfig) -> Result<Self, CliFailure> {
        let section = config
            .get(POLICY_KEY)
            .and_then(toml::Value::as_table)
            .ok_or_else(|| {
                CliFailure::new(
                    1,
                    format!("trusted machine-global config is missing [{POLICY_KEY}]"),
                )
            })?;
        validate_policy_fields(section)?;
        let enabled = bool_field(section, "enabled")?;
        let provider = required_string(section, "provider")?;
        validate_token("provider", &provider)?;
        if provider != "codex" {
            return Err(CliFailure::new(
                1,
                format!("phase-1 [{POLICY_KEY}] requires provider=\"codex\""),
            ));
        }
        let first_line_model = optional_string(section, "first_line_model")?
            .unwrap_or_else(|| DEFAULT_FIRST_LINE_MODEL.to_owned());
        validate_token("first_line_model", &first_line_model)?;
        let escalation_model = optional_string(section, "escalation_model")?;
        if let Some(model) = &escalation_model {
            validate_token("escalation_model", model)?;
        }
        let codex_binary = required_absolute_path(section, "codex_binary")?;
        validate_codex_binary(&codex_binary)?;
        let codex_home = required_absolute_path(section, "codex_home")?;
        let timeout_seconds = positive_u64_field(
            section,
            "timeout_seconds",
            DEFAULT_TIMEOUT_SECONDS,
            MAX_TIMEOUT_SECONDS,
        )?;
        let max_attempts_per_head =
            u32::try_from(positive_u64_field(section, "max_attempts_per_head", 1, 1)?)
                .map_err(|_| CliFailure::new(1, "max_attempts_per_head does not fit u32"))?;
        let max_log_tail_bytes = usize::try_from(positive_u64_field(
            section,
            "max_log_tail_bytes",
            DEFAULT_MAX_LOG_TAIL_BYTES as u64,
            MAX_LOG_TAIL_BYTES as u64,
        )?)
        .map_err(|_| CliFailure::new(1, "max_log_tail_bytes does not fit usize"))?;
        let allowed_repositories = string_array(section, "allowed_repositories")?
            .into_iter()
            .map(|repo| {
                validate_repo_slug(&repo)?;
                Ok(repo.to_ascii_lowercase())
            })
            .collect::<Result<BTreeSet<_>, CliFailure>>()?;
        let repo_paths = path_map(section, "repo_paths")?;
        validate_repository_map(&allowed_repositories, &repo_paths)?;
        if enabled && allowed_repositories.is_empty() {
            return Err(CliFailure::new(
                1,
                "enabled recovery-worker policy requires allowed_repositories",
            ));
        }
        Ok(Self {
            enabled,
            provider,
            first_line_model,
            escalation_model,
            codex_binary,
            codex_home,
            timeout_seconds,
            max_attempts_per_head,
            max_log_tail_bytes,
            allowed_repositories,
            repo_paths,
        })
    }

    pub(super) fn repo_path(&self, repo: &str) -> Result<&Path, CliFailure> {
        let repo = repo.to_ascii_lowercase();
        if !self.allowed_repositories.contains(&repo) {
            return Err(CliFailure::new(
                1,
                format!("recovery request repository `{repo}` is not allowed"),
            ));
        }
        self.repo_paths
            .get(&repo)
            .map(PathBuf::as_path)
            .ok_or_else(|| {
                CliFailure::new(
                    1,
                    format!("trusted recovery-worker policy has no path for `{repo}`"),
                )
            })
    }

    pub(super) fn argv(&self) -> Vec<String> {
        let mut argv = vec![
            self.codex_binary.display().to_string(),
            "exec".to_owned(),
            "-c".to_owned(),
            FORCED_REASONING_CONFIG.to_owned(),
            "--ephemeral".to_owned(),
            "--ignore-user-config".to_owned(),
            "--ignore-rules".to_owned(),
            "--strict-config".to_owned(),
            "--sandbox".to_owned(),
            "read-only".to_owned(),
            "--skip-git-repo-check".to_owned(),
            "--color".to_owned(),
            "never".to_owned(),
        ];
        for feature in DISABLED_CODEX_FEATURES {
            argv.extend(["--disable".to_owned(), (*feature).to_owned()]);
        }
        argv.extend([
            "--model".to_owned(),
            self.first_line_model.clone(),
            "-".to_owned(),
        ]);
        argv
    }
}

pub(super) fn enqueue_policy(
    config: &LoadedConfig,
) -> Result<Option<RecoveryWorkerPolicy>, CliFailure> {
    let Some(value) = config.get(POLICY_KEY) else {
        return Ok(None);
    };
    let section = value.as_table().ok_or_else(|| {
        CliFailure::new(
            1,
            format!("trusted machine-global [{POLICY_KEY}] must be a table"),
        )
    })?;
    validate_policy_fields(section)?;
    let enabled = bool_field(section, "enabled")?;
    if !enabled && section.len() == 1 {
        return Ok(None);
    }
    let policy = RecoveryWorkerPolicy::from_config(config)?;
    if enabled { Ok(Some(policy)) } else { Ok(None) }
}

fn validate_policy_fields(section: &toml::Table) -> Result<(), CliFailure> {
    if section.contains_key("command") {
        return Err(CliFailure::new(
            1,
            format!(
                "[{POLICY_KEY}].command is forbidden; Shipyard constructs the complete phase-1 argv"
            ),
        ));
    }
    let unknown = section
        .keys()
        .filter(|key| !POLICY_FIELDS.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!(
                "[{POLICY_KEY}] contains unsupported field(s): {}",
                unknown.join(", ")
            ),
        ))
    }
}

pub(super) fn bool_field(table: &toml::Table, key: &str) -> Result<bool, CliFailure> {
    table
        .get(key)
        .and_then(toml::Value::as_bool)
        .ok_or_else(|| CliFailure::new(1, format!("[{POLICY_KEY}].{key} must be a boolean")))
}

pub(super) fn required_string(table: &toml::Table, key: &str) -> Result<String, CliFailure> {
    optional_string(table, key)?.ok_or_else(|| {
        CliFailure::new(
            1,
            format!("[{POLICY_KEY}].{key} must be a non-empty string"),
        )
    })
}

pub(super) fn optional_string(
    table: &toml::Table,
    key: &str,
) -> Result<Option<String>, CliFailure> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CliFailure::new(
                1,
                format!("[{POLICY_KEY}].{key} must be a non-empty string"),
            )
        })?;
    Ok(Some(value.to_owned()))
}

pub(super) fn string_array(table: &toml::Table, key: &str) -> Result<Vec<String>, CliFailure> {
    let values = table
        .get(key)
        .and_then(toml::Value::as_array)
        .ok_or_else(|| CliFailure::new(1, format!("[{POLICY_KEY}].{key} must be an array")))?;
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let value = value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                CliFailure::new(
                    1,
                    format!("[{POLICY_KEY}].{key} entries must be non-empty strings"),
                )
            })?;
        out.push(value.to_owned());
    }
    Ok(out)
}

pub(super) fn path_map(
    table: &toml::Table,
    key: &str,
) -> Result<BTreeMap<String, PathBuf>, CliFailure> {
    let values = table
        .get(key)
        .and_then(toml::Value::as_table)
        .ok_or_else(|| CliFailure::new(1, format!("[{POLICY_KEY}].{key} must be a table")))?;
    let mut out = BTreeMap::new();
    for (repo, value) in values {
        validate_repo_slug(repo)?;
        let path = value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                CliFailure::new(
                    1,
                    format!("[{POLICY_KEY}].{key}.{repo:?} must be a non-empty path string"),
                )
            })?;
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(CliFailure::new(
                1,
                format!("[{POLICY_KEY}].{key}.{repo:?} must be absolute"),
            ));
        }
        let normalized_repo = repo.to_ascii_lowercase();
        if out.insert(normalized_repo.clone(), path).is_some() {
            return Err(CliFailure::new(
                1,
                format!("[{POLICY_KEY}].{key} contains duplicate repository `{normalized_repo}`"),
            ));
        }
    }
    Ok(out)
}

pub(super) fn required_absolute_path(
    table: &toml::Table,
    key: &str,
) -> Result<PathBuf, CliFailure> {
    let value = required_string(table, key)?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(CliFailure::new(
            1,
            format!("[{POLICY_KEY}].{key} must be absolute"),
        ));
    }
    Ok(path)
}

pub(super) fn validate_codex_binary(path: &std::path::Path) -> Result<(), CliFailure> {
    let filename = path.file_name().and_then(OsStr::to_str);
    if !matches!(filename, Some("codex" | "codex.exe")) {
        return Err(CliFailure::new(
            1,
            format!("[{POLICY_KEY}].codex_binary must name the direct codex executable"),
        ));
    }
    crate::native_executable::validate_native_executable(path).map_err(|error| {
        CliFailure::new(
            1,
            format!("[{POLICY_KEY}].codex_binary must be a direct native executable: {error}"),
        )
    })?;
    Ok(())
}

pub(super) fn positive_u64_field(
    table: &toml::Table,
    key: &str,
    default: u64,
    maximum: u64,
) -> Result<u64, CliFailure> {
    let value = match table.get(key) {
        None => default,
        Some(value) => value
            .as_integer()
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| {
                CliFailure::new(
                    1,
                    format!("[{POLICY_KEY}].{key} must be a positive integer"),
                )
            })?,
    };
    if value == 0 || value > maximum {
        return Err(CliFailure::new(
            1,
            format!("[{POLICY_KEY}].{key} must be in 1..={maximum}"),
        ));
    }
    Ok(value)
}

pub(super) fn validate_token(field: &str, value: &str) -> Result<(), CliFailure> {
    if value.len() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".:/_-".contains(character))
    {
        return Err(CliFailure::new(
            1,
            format!("[{POLICY_KEY}].{field} contains unsupported characters"),
        ));
    }
    Ok(())
}

pub(super) fn validate_repo_slug(repo: &str) -> Result<(), CliFailure> {
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(CliFailure::new(
            1,
            format!("invalid repository slug `{repo}`"),
        ));
    };
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return Err(CliFailure::new(
            1,
            format!("invalid repository slug `{repo}`"),
        ));
    }
    Ok(())
}

pub(super) fn validate_repository_map(
    allowed: &BTreeSet<String>,
    paths: &BTreeMap<String, PathBuf>,
) -> Result<(), CliFailure> {
    let configured = paths.keys().cloned().collect::<BTreeSet<_>>();
    if configured != *allowed {
        return Err(CliFailure::new(
            1,
            format!("[{POLICY_KEY}].repo_paths keys must exactly match allowed_repositories"),
        ));
    }
    Ok(())
}
