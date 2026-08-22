use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::CliFailure;
use super::super::changed_surface_cmd::{ChangedSurfacePlanArgs, observe_changed_surface_plan};
use crate::changed_surface::{
    ExecutionCommandTransport, ExecutionDisposition, plan_authoritative_execution,
};
use crate::config::LoadedConfig;
use crate::executor::dispatch::{ResolvedTarget, ResolvedValidation};
use crate::queue_request::validation_contract_digest;

const MODE_KEY: &str = "changed_surface_execution.mode";
const ACCEPTED_POLICY_DIGEST_KEY: &str = "changed_surface_execution.accepted_shadow_policy_digest";
const MAX_DIAGNOSTIC_CHARS: usize = 512;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MachineMode {
    #[default]
    Off,
    ShadowCompare,
    Authoritative,
}

impl MachineMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::ShadowCompare => "shadow_compare",
            Self::Authoritative => "authoritative",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MachinePolicy {
    mode: MachineMode,
    accepted_shadow_policy_digest: Option<String>,
}

impl MachinePolicy {
    fn from_global(config: &LoadedConfig) -> Result<Self, CliFailure> {
        let trusted = LoadedConfig::load_machine_global_from_dir(config.global_dir.clone())
            .map_err(|error| {
                CliFailure::new(1, format!("load trusted selector policy: {error}"))
            })?;
        let mode = match trusted.get_str(MODE_KEY) {
            None | Some("off") => MachineMode::Off,
            Some("shadow_compare") => MachineMode::ShadowCompare,
            Some("authoritative") => MachineMode::Authoritative,
            Some(value) => Err(CliFailure::new(
                2,
                format!("invalid trusted {MODE_KEY} value '{value}'"),
            ))?,
        };
        let accepted_shadow_policy_digest = trusted
            .get_str(ACCEPTED_POLICY_DIGEST_KEY)
            .map(ToOwned::to_owned);
        if accepted_shadow_policy_digest
            .as_deref()
            .is_some_and(|digest| {
                digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        {
            return Err(CliFailure::new(
                2,
                format!("invalid trusted {ACCEPTED_POLICY_DIGEST_KEY}"),
            ));
        }
        Ok(Self {
            mode,
            accepted_shadow_policy_digest,
        })
    }

    fn permits_authoritative(&self, policy_digest: &str) -> bool {
        self.mode != MachineMode::Authoritative
            || self.accepted_shadow_policy_digest.as_deref() == Some(policy_digest)
    }
}

#[derive(Debug, Serialize)]
struct ActivationReceipt<'a> {
    schema_version: u32,
    machine_mode: MachineMode,
    plan: &'a crate::changed_surface::AuthoritativeExecutionPlan,
    original_test_command_sha256: String,
    substituted_test_command_sha256: String,
}

#[derive(Debug, Serialize)]
struct FallbackDiagnostic<'a> {
    schema_version: u32,
    repository: &'a str,
    pull_request: u64,
    target: &'a str,
    machine_mode: MachineMode,
    category: &'a str,
    diagnostic: String,
}

// Keep the fail-open-to-full branches adjacent to their exact diagnostics and
// mutation point; splitting them risks one error path accidentally substituting
// a command or losing its durable reason.
#[allow(clippy::too_many_lines)]
pub(super) fn apply_changed_surface_execution(
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    repo: &str,
    pr: Option<u64>,
    targets: &mut [ResolvedTarget],
) -> Result<(), CliFailure> {
    let machine = MachinePolicy::from_global(config)?;
    if machine.mode == MachineMode::Off || !cfg!(unix) {
        return Ok(());
    }
    let Some(pr) = pr.filter(|number| *number != 0) else {
        return Ok(());
    };

    for target in targets {
        // This merged-layer check is only a negative performance prefilter.
        // Authorization is always reparsed from the authenticated base below.
        if config
            .get("targets")
            .and_then(toml::Value::as_table)
            .and_then(|targets| targets.get(&target.name))
            .and_then(toml::Value::as_table)
            .and_then(|target| target.get("changed_surface_selection"))
            .is_none()
        {
            continue;
        }
        let ResolvedValidation::Local(validation) = &target.validation else {
            continue;
        };
        if validation.command.is_some() || !validation.stages.contains_key("test") {
            continue;
        }
        let Some(contract_digest) = validation_contract_digest(target) else {
            continue;
        };
        let observation = match observe_changed_surface_plan(
            &ChangedSurfacePlanArgs {
                target: target.name.clone(),
                pr,
                repo: (!repo.is_empty()).then(|| repo.to_owned()),
            },
            config,
            cwd,
            state_dir,
        ) {
            Ok(observation) => observation,
            Err(error) => {
                persist_fallback_diagnostic(
                    &result_dir(state_dir, repo, pr, "unresolved", &target.name),
                    &FallbackDiagnostic {
                        schema_version: 1,
                        repository: repo,
                        pull_request: pr,
                        target: &target.name,
                        machine_mode: machine.mode,
                        category: "observation_error",
                        diagnostic: bounded_diagnostic(&error.message),
                    },
                )?;
                continue;
            }
        };
        let Ok(policy) = observation.policy.as_ref() else {
            persist_fallback_diagnostic(
                &result_dir(
                    state_dir,
                    repo,
                    pr,
                    &observation.receipt.head_sha,
                    &target.name,
                ),
                &FallbackDiagnostic {
                    schema_version: 1,
                    repository: repo,
                    pull_request: pr,
                    target: &target.name,
                    machine_mode: machine.mode,
                    category: "protected_policy_error",
                    diagnostic: bounded_diagnostic(
                        observation
                            .policy
                            .as_ref()
                            .expect_err("checked policy error"),
                    ),
                },
            )?;
            continue;
        };
        let disposition = match plan_authoritative_execution(
            &observation.receipt,
            &observation.input,
            policy,
            true,
            ExecutionCommandTransport::PosixShell,
            &contract_digest,
            &observation.workflow_digest,
        ) {
            Ok(disposition) => disposition,
            Err(error) => {
                persist_fallback_diagnostic(
                    &result_dir(
                        state_dir,
                        repo,
                        pr,
                        &observation.receipt.head_sha,
                        &target.name,
                    ),
                    &FallbackDiagnostic {
                        schema_version: 1,
                        repository: repo,
                        pull_request: pr,
                        target: &target.name,
                        machine_mode: machine.mode,
                        category: "promotion_error",
                        diagnostic: bounded_diagnostic(&error.to_string()),
                    },
                )?;
                continue;
            }
        };
        let plan = match disposition {
            ExecutionDisposition::Bounded(plan) => plan,
            ExecutionDisposition::Full { reason } => {
                persist_fallback_diagnostic(
                    &result_dir(
                        state_dir,
                        repo,
                        pr,
                        &observation.receipt.head_sha,
                        &target.name,
                    ),
                    &FallbackDiagnostic {
                        schema_version: 1,
                        repository: repo,
                        pull_request: pr,
                        target: &target.name,
                        machine_mode: machine.mode,
                        category: "full_fallback",
                        diagnostic: bounded_diagnostic(&format!("{reason:?}")),
                    },
                )?;
                continue;
            }
            ExecutionDisposition::Blocked { reason } => {
                persist_fallback_diagnostic(
                    &result_dir(
                        state_dir,
                        repo,
                        pr,
                        &observation.receipt.head_sha,
                        &target.name,
                    ),
                    &FallbackDiagnostic {
                        schema_version: 1,
                        repository: repo,
                        pull_request: pr,
                        target: &target.name,
                        machine_mode: machine.mode,
                        category: "blocked",
                        diagnostic: bounded_diagnostic(&reason),
                    },
                )?;
                return Err(CliFailure::new(1, bounded_diagnostic(&reason)));
            }
        };
        if !machine.permits_authoritative(&plan.policy_digest) {
            persist_fallback_diagnostic(
                &result_dir(state_dir, repo, pr, &plan.head_sha, &target.name),
                &FallbackDiagnostic {
                    schema_version: 1,
                    repository: repo,
                    pull_request: pr,
                    target: &target.name,
                    machine_mode: machine.mode,
                    category: "graduation_fence",
                    diagnostic:
                        "authoritative mode requires the exact reviewed shadow policy digest"
                            .to_owned(),
                },
            )?;
            continue;
        }
        let original = validation
            .stages
            .get("test")
            .expect("checked local test stage")
            .clone();
        let result_dir = result_dir(state_dir, repo, pr, &plan.head_sha, &target.name);
        let compare = if machine.mode == MachineMode::ShadowCompare {
            "1"
        } else {
            "0"
        };
        let substituted = format!(
            "SHIPYARD_CHANGED_SURFACE_RESULT_DIR={} SHIPYARD_CHANGED_SURFACE_COMPARE_FULL={} {}",
            shell_quote(&result_dir),
            compare,
            plan.command
        );
        persist_activation(
            &result_dir,
            &ActivationReceipt {
                schema_version: 1,
                machine_mode: machine.mode,
                plan: &plan,
                original_test_command_sha256: sha256(original.as_bytes()),
                substituted_test_command_sha256: sha256(substituted.as_bytes()),
            },
        )?;
        let ResolvedValidation::Local(validation) = &mut target.validation else {
            unreachable!();
        };
        validation.stages.insert("test".to_owned(), substituted);
    }
    Ok(())
}

fn result_dir(state_dir: &Path, repo: &str, pr: u64, head: &str, target: &str) -> PathBuf {
    state_dir
        .join("changed-surface-results")
        .join(path_component(repo))
        .join(pr.to_string())
        .join(path_component(head))
        .join(path_component(target))
}

fn path_component(value: &str) -> String {
    let canonical: String = value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => byte as char,
            _ => '_',
        })
        .take(48)
        .collect();
    format!("{canonical}-{}", sha256(value.as_bytes()))
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn persist_activation(path: &Path, receipt: &ActivationReceipt<'_>) -> Result<(), CliFailure> {
    fs::create_dir_all(path).map_err(|error| {
        CliFailure::new(1, format!("create selector evidence directory: {error}"))
    })?;
    let payload = serde_json::to_vec_pretty(receipt)
        .map_err(|error| CliFailure::new(1, format!("serialize selector activation: {error}")))?;
    let destination = path.join(format!("activation-{}.json", receipt.machine_mode.as_str()));
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
    {
        Ok(mut file) => {
            file.write_all(&payload)
                .and_then(|()| file.write_all(b"\n"))
                .and_then(|()| file.sync_all())
                .map_err(|error| {
                    CliFailure::new(1, format!("write selector activation: {error}"))
                })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(&destination).map_err(|error| {
                CliFailure::new(1, format!("read existing selector activation: {error}"))
            })?;
            let mut expected = payload;
            expected.push(b'\n');
            if existing != expected {
                return Err(CliFailure::new(
                    1,
                    "immutable selector activation receipt already exists with different bytes",
                ));
            }
        }
        Err(error) => {
            return Err(CliFailure::new(
                1,
                format!("create selector activation receipt: {error}"),
            ));
        }
    }
    sync_directory(path)?;
    Ok(())
}

fn persist_fallback_diagnostic(
    path: &Path,
    diagnostic: &FallbackDiagnostic<'_>,
) -> Result<(), CliFailure> {
    fs::create_dir_all(path).map_err(|error| {
        CliFailure::new(1, format!("create selector diagnostic directory: {error}"))
    })?;
    let payload = serde_json::to_vec(diagnostic)
        .map_err(|error| CliFailure::new(1, format!("serialize selector diagnostic: {error}")))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for sequence in 0..100_u8 {
        let destination = path.join(format!(
            "fallback-{nanos}-{}-{sequence}.json",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
        {
            Ok(mut file) => {
                file.write_all(&payload)
                    .and_then(|()| file.write_all(b"\n"))
                    .and_then(|()| file.sync_all())
                    .map_err(|error| {
                        CliFailure::new(1, format!("write selector diagnostic: {error}"))
                    })?;
                sync_directory(path)?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(CliFailure::new(
                    1,
                    format!("create selector diagnostic: {error}"),
                ));
            }
        }
    }
    Err(CliFailure::new(
        1,
        "cannot allocate immutable selector diagnostic",
    ))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), CliFailure> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| CliFailure::new(1, format!("sync selector evidence directory: {error}")))
}

// Changed-surface execution is currently Unix-only. Windows does not permit
// opening a directory as a File for fsync, so keep its compiled test surface
// explicit without pretending to provide a durability guarantee there.
#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), CliFailure> {
    Ok(())
}

fn bounded_diagnostic(value: &str) -> String {
    value.chars().take(MAX_DIAGNOSTIC_CHARS).collect()
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::{
        FallbackDiagnostic, MachineMode, MachinePolicy, bounded_diagnostic, path_component,
        persist_fallback_diagnostic, shell_quote,
    };
    use crate::config::{LoadedConfig, LocalOverlaySource};
    use std::fs;

    #[test]
    fn machine_mode_is_default_off_and_ignores_repo_layers() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path()).unwrap();
        let mut config = LoadedConfig::load(
            Some(temp.path().to_path_buf()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        config.data.insert(
            "changed_surface_execution".to_owned(),
            toml::toml! { mode = "authoritative" }.into(),
        );
        assert_eq!(
            MachinePolicy::from_global(&config).unwrap().mode,
            MachineMode::Off
        );
    }

    #[test]
    fn path_inputs_are_bounded_before_shell_use() {
        assert_ne!(path_component("a/b"), path_component("a_b"));
        assert_eq!(path_component("a/b"), path_component("a/b"));
        assert_eq!(shell_quote(std::path::Path::new("a'b")), "'a'\"'\"'b'");
    }

    #[test]
    fn invalid_global_mode_fails_closed_instead_of_enabling() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            "[changed_surface_execution]\nmode = 'fast'\n",
        )
        .unwrap();
        let config = LoadedConfig::load(
            Some(temp.path().to_path_buf()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        let error = MachinePolicy::from_global(&config).unwrap_err();
        assert!(error.message.contains("invalid trusted"));
    }

    #[test]
    fn accepted_shadow_digest_requires_canonical_lowercase_sha256() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            format!(
                "[changed_surface_execution]\nmode = 'authoritative'\naccepted_shadow_policy_digest = '{}'\n",
                "A".repeat(64)
            ),
        )
        .unwrap();
        let config = LoadedConfig::load(
            Some(temp.path().to_path_buf()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        assert!(MachinePolicy::from_global(&config).is_err());
    }

    #[test]
    fn authoritative_requires_exact_accepted_shadow_policy_digest() {
        let digest = "a".repeat(64);
        let missing = MachinePolicy {
            mode: MachineMode::Authoritative,
            accepted_shadow_policy_digest: None,
        };
        assert!(!missing.permits_authoritative(&digest));
        let accepted = MachinePolicy {
            mode: MachineMode::Authoritative,
            accepted_shadow_policy_digest: Some(digest.clone()),
        };
        assert!(accepted.permits_authoritative(&digest));
        assert!(!accepted.permits_authoritative(&"b".repeat(64)));
        let shadow = MachinePolicy {
            mode: MachineMode::ShadowCompare,
            accepted_shadow_policy_digest: None,
        };
        assert!(shadow.permits_authoritative(&digest));
    }

    #[test]
    fn fallback_diagnostics_are_bounded_append_only_receipts() {
        let temp = tempfile::tempdir().unwrap();
        let diagnostic = FallbackDiagnostic {
            schema_version: 1,
            repository: "owner/repo",
            pull_request: 7,
            target: "mac",
            machine_mode: MachineMode::ShadowCompare,
            category: "observation_error",
            diagnostic: bounded_diagnostic(&"x".repeat(2_000)),
        };
        assert_eq!(diagnostic.diagnostic.chars().count(), 512);
        persist_fallback_diagnostic(temp.path(), &diagnostic).unwrap();
        persist_fallback_diagnostic(temp.path(), &diagnostic).unwrap();
        assert_eq!(
            fs::read_dir(temp.path())
                .unwrap()
                .filter_map(Result::ok)
                .count(),
            2
        );
    }
}
