//! Trusted machine-global configuration for future workstream continuation.
//!
//! This module only parses and validates policy. It does not activate the
//! scheduler, resolve a wrapper, launch a provider, or consume a wake.

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};
#[cfg(test)]
use std::path::Path;
use std::path::{Component, PathBuf};

use serde::Deserialize;

use crate::config::LoadedConfig;

const POLICY_KEY: &str = "workstream_continuation";
const MAX_DEADLINE_SECONDS: u64 = 300;
const MAX_LOG_BYTES: u64 = 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_ID_BYTES: usize = 128;
const MAX_REPOSITORY_COMPONENT_BYTES: usize = 100;

/// A fully enabled, machine-authorized continuation policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkstreamContinuationConfig {
    /// Exact machine tag permitted to consume continuation wakes.
    pub origin_machine: String,
    /// Canonical, sorted GitHub `owner/repository` allowlist.
    pub repositories: Vec<String>,
    /// Protected provider-wrapper contract.
    pub provider_wrapper: ProviderWrapperConfig,
}

impl WorkstreamContinuationConfig {
    /// Return whether an already canonical repository is explicitly allowed.
    #[must_use]
    pub fn allows_repository(&self, repository: &str) -> bool {
        self.repositories
            .binary_search_by(|candidate| candidate.as_str().cmp(repository))
            .is_ok()
    }
}

/// Immutable provider-wrapper identity and bounded execution policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderWrapperConfig {
    /// Absolute wrapper executable path. Runtime must still verify this file.
    pub executable_path: PathBuf,
    /// Lowercase SHA-256 of the expected executable bytes.
    pub executable_sha256: String,
    /// Provider identity expected from the protected route.
    pub provider_id: String,
    /// Capability-registered adapter identity.
    pub adapter_id: String,
    /// Complete provider-call deadline.
    pub deadline_seconds: u64,
    /// Maximum captured stdout bytes.
    pub max_stdout_bytes: u64,
    /// Maximum captured stderr bytes.
    pub max_stderr_bytes: u64,
}

/// Refusal returned for malformed, partial, or unauthorized activation policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkstreamContinuationConfigError(String);

impl Display for WorkstreamContinuationConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for WorkstreamContinuationConfigError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    #[serde(default)]
    activation_enabled: bool,
    #[serde(default)]
    dispatch_enabled: bool,
    origin_machine: Option<String>,
    repositories: Option<Vec<String>>,
    provider_wrapper: Option<RawProviderWrapper>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProviderWrapper {
    executable_path: String,
    executable_sha256: String,
    provider_id: String,
    adapter_id: String,
    deadline_seconds: u64,
    max_stdout_bytes: u64,
    max_stderr_bytes: u64,
}

/// Load continuation policy exclusively from the trusted machine-global layer.
///
/// `config.data` is intentionally ignored because it may contain project and
/// checkout-local overlays. The caller supplies the already authenticated
/// origin-machine tag; an enabled policy for another machine refuses rather
/// than degrading to disabled.
pub fn trusted_workstream_continuation_config(
    config: &LoadedConfig,
    observed_origin_machine: &str,
) -> Result<Option<WorkstreamContinuationConfig>, WorkstreamContinuationConfigError> {
    let trusted =
        LoadedConfig::load_machine_global_from_dir(config.global_dir.clone()).map_err(|error| {
            refusal(format!(
                "load trusted workstream continuation policy: {error}"
            ))
        })?;
    let Some(value) = trusted.get(POLICY_KEY) else {
        return Ok(None);
    };
    let raw: RawPolicy = value
        .clone()
        .try_into()
        .map_err(|error| refusal(format!("decode {POLICY_KEY}: {error}")))?;

    match (raw.activation_enabled, raw.dispatch_enabled) {
        (false, false) => {
            if raw.origin_machine.is_some()
                || raw.repositories.is_some()
                || raw.provider_wrapper.is_some()
            {
                return Err(refusal(format!(
                    "{POLICY_KEY} is disabled but contains activation-only fields"
                )));
            }
            Ok(None)
        }
        (true, true) => validate_enabled_policy(raw, observed_origin_machine).map(Some),
        _ => Err(refusal(format!(
            "{POLICY_KEY} requires activation_enabled and dispatch_enabled to be enabled together"
        ))),
    }
}

fn validate_enabled_policy(
    raw: RawPolicy,
    observed_origin_machine: &str,
) -> Result<WorkstreamContinuationConfig, WorkstreamContinuationConfigError> {
    validate_machine_tag("observed origin machine", observed_origin_machine)?;
    let origin_machine = required(raw.origin_machine, "origin_machine")?;
    validate_machine_tag("configured origin machine", &origin_machine)?;
    if origin_machine != observed_origin_machine {
        return Err(refusal(format!(
            "{POLICY_KEY}.origin_machine does not match this machine"
        )));
    }

    let repositories = raw
        .repositories
        .ok_or_else(|| refusal(format!("{POLICY_KEY}.repositories is required")))?;
    validate_repositories(&repositories)?;
    let provider_wrapper = validate_provider_wrapper(
        raw.provider_wrapper
            .ok_or_else(|| refusal(format!("{POLICY_KEY}.provider_wrapper is required")))?,
    )?;

    Ok(WorkstreamContinuationConfig {
        origin_machine,
        repositories,
        provider_wrapper,
    })
}

fn validate_provider_wrapper(
    raw: RawProviderWrapper,
) -> Result<ProviderWrapperConfig, WorkstreamContinuationConfigError> {
    let executable_path = PathBuf::from(&raw.executable_path);
    let normalized_path = executable_path.components().collect::<PathBuf>();
    if raw.executable_path.is_empty()
        || raw.executable_path.len() > MAX_PATH_BYTES
        || raw.executable_path.chars().any(char::is_control)
        || !executable_path.is_absolute()
        || executable_path != normalized_path
        || executable_path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(refusal(format!(
            "{POLICY_KEY}.provider_wrapper.executable_path must be a bounded normalized absolute path"
        )));
    }
    validate_sha256("provider wrapper executable_sha256", &raw.executable_sha256)?;
    validate_id("provider_id", &raw.provider_id)?;
    validate_id("adapter_id", &raw.adapter_id)?;
    if !(1..=MAX_DEADLINE_SECONDS).contains(&raw.deadline_seconds) {
        return Err(refusal(format!(
            "{POLICY_KEY}.provider_wrapper.deadline_seconds must be between 1 and {MAX_DEADLINE_SECONDS}"
        )));
    }
    for (field, value) in [
        ("max_stdout_bytes", raw.max_stdout_bytes),
        ("max_stderr_bytes", raw.max_stderr_bytes),
    ] {
        if !(1..=MAX_LOG_BYTES).contains(&value) {
            return Err(refusal(format!(
                "{POLICY_KEY}.provider_wrapper.{field} must be between 1 and {MAX_LOG_BYTES}"
            )));
        }
    }
    Ok(ProviderWrapperConfig {
        executable_path,
        executable_sha256: raw.executable_sha256,
        provider_id: raw.provider_id,
        adapter_id: raw.adapter_id,
        deadline_seconds: raw.deadline_seconds,
        max_stdout_bytes: raw.max_stdout_bytes,
        max_stderr_bytes: raw.max_stderr_bytes,
    })
}

fn validate_repositories(repositories: &[String]) -> Result<(), WorkstreamContinuationConfigError> {
    if repositories.is_empty() {
        return Err(refusal(format!(
            "{POLICY_KEY}.repositories must be nonempty"
        )));
    }
    let mut canonical = BTreeSet::new();
    for repository in repositories {
        let mut parts = repository.split('/');
        let owner = parts.next().unwrap_or_default();
        let name = parts.next().unwrap_or_default();
        if parts.next().is_some()
            || !valid_repository_component(owner)
            || !valid_repository_component(name)
            || repository != &repository.to_ascii_lowercase()
        {
            return Err(refusal(format!(
                "{POLICY_KEY}.repositories must contain canonical lowercase owner/repository identities"
            )));
        }
        if !canonical.insert(repository.as_str()) {
            return Err(refusal(format!(
                "{POLICY_KEY}.repositories contains a duplicate identity"
            )));
        }
    }
    if canonical
        .into_iter()
        .ne(repositories.iter().map(String::as_str))
    {
        return Err(refusal(format!(
            "{POLICY_KEY}.repositories must be sorted canonically"
        )));
    }
    Ok(())
}

fn valid_repository_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_REPOSITORY_COMPONENT_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn validate_machine_tag(label: &str, value: &str) -> Result<(), WorkstreamContinuationConfigError> {
    crate::runner_provision::validate_machine_tag(value)
        .map_err(|error| refusal(format!("invalid {label}: {error}")))
}

fn validate_id(field: &str, value: &str) -> Result<(), WorkstreamContinuationConfigError> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || !value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
    {
        return Err(refusal(format!(
            "{POLICY_KEY}.provider_wrapper.{field} is invalid"
        )));
    }
    Ok(())
}

fn validate_sha256(label: &str, value: &str) -> Result<(), WorkstreamContinuationConfigError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(refusal(format!("{label} must be a lowercase SHA-256")));
    }
    Ok(())
}

fn required(
    value: Option<String>,
    field: &str,
) -> Result<String, WorkstreamContinuationConfigError> {
    value.ok_or_else(|| refusal(format!("{POLICY_KEY}.{field} is required")))
}

fn refusal(message: String) -> WorkstreamContinuationConfigError {
    WorkstreamContinuationConfigError(message)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::config::LocalOverlaySource;

    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn enabled_policy(repositories: &str) -> String {
        format!(
            r#"[workstream_continuation]
activation_enabled = true
dispatch_enabled = true
origin_machine = "m5"
repositories = {repositories}

[workstream_continuation.provider_wrapper]
executable_path = "/opt/shipyard/bin/workstream-provider"
executable_sha256 = "{DIGEST}"
provider_id = "codex"
adapter_id = "codex-cli-v1"
deadline_seconds = 120
max_stdout_bytes = 16384
max_stderr_bytes = 16384
"#
        )
    }

    fn layered_config(
        global: Option<&str>,
        project: Option<&str>,
        local: Option<&str>,
    ) -> (TempDir, LoadedConfig) {
        let temp = TempDir::new().expect("temp");
        let global_dir = temp.path().join("global");
        let project_dir = temp.path().join("project");
        let local_dir = temp.path().join("local");
        for (directory, contents) in [
            (&global_dir, global),
            (&project_dir, project),
            (&local_dir, local),
        ] {
            fs::create_dir_all(directory).expect("config directory");
            if let Some(contents) = contents {
                fs::write(directory.join("config.toml"), contents).expect("config");
            }
        }
        let loaded = LoadedConfig::load(
            Some(global_dir),
            Some(project_dir),
            Some(local_dir),
            LocalOverlaySource::Direct,
        )
        .expect("layered config");
        (temp, loaded)
    }

    #[test]
    fn absent_or_explicitly_disabled_policy_is_default_off() {
        let (_temp, absent) = layered_config(None, None, None);
        assert_eq!(
            trusted_workstream_continuation_config(&absent, "m5").expect("absent"),
            None
        );
        let (_temp, disabled) = layered_config(
            Some(
                "[workstream_continuation]\nactivation_enabled = false\ndispatch_enabled = false\n",
            ),
            None,
            None,
        );
        assert_eq!(
            trusted_workstream_continuation_config(&disabled, "m5").expect("disabled"),
            None
        );
    }

    #[test]
    fn exact_global_policy_enables_only_on_its_origin_machine() {
        let (_temp, config) = layered_config(
            Some(&enabled_policy(
                "[\"generous-corp/pulp\", \"generous-corp/shipyard\"]",
            )),
            None,
            None,
        );
        let policy = trusted_workstream_continuation_config(&config, "m5")
            .expect("policy")
            .expect("enabled");
        assert_eq!(policy.origin_machine, "m5");
        assert_eq!(
            policy.repositories,
            ["generous-corp/pulp", "generous-corp/shipyard"]
        );
        assert_eq!(
            policy.provider_wrapper.executable_path,
            Path::new("/opt/shipyard/bin/workstream-provider")
        );
        assert_eq!(policy.provider_wrapper.executable_sha256, DIGEST);
        assert_eq!(policy.provider_wrapper.deadline_seconds, 120);
        assert!(policy.allows_repository("generous-corp/shipyard"));
        assert!(!policy.allows_repository("attacker/repository"));

        let error =
            trusted_workstream_continuation_config(&config, "m3").expect_err("wrong machine");
        assert!(error.to_string().contains("does not match this machine"));
    }

    #[test]
    fn project_and_local_layers_cannot_enable_or_broaden_policy() {
        let overlay = enabled_policy("[\"attacker/repository\"]");
        let (_temp, no_global) = layered_config(None, Some(&overlay), Some(&overlay));
        assert_eq!(
            trusted_workstream_continuation_config(&no_global, "m5").expect("overlays ignored"),
            None
        );

        let global = enabled_policy("[\"generous-corp/shipyard\"]");
        let (_temp, layered) = layered_config(Some(&global), Some(&overlay), Some(&overlay));
        let policy = trusted_workstream_continuation_config(&layered, "m5")
            .expect("trusted global")
            .expect("enabled");
        assert_eq!(policy.repositories, ["generous-corp/shipyard"]);
        assert_eq!(policy.provider_wrapper.provider_id, "codex");
    }

    #[test]
    fn partial_activation_and_disabled_latent_fields_refuse() {
        for body in [
            "[workstream_continuation]\nactivation_enabled = true\ndispatch_enabled = false\n",
            "[workstream_continuation]\nactivation_enabled = false\ndispatch_enabled = false\norigin_machine = \"m5\"\n",
            "[workstream_continuation]\nactivation_enabled = true\ndispatch_enabled = true\norigin_machine = \"m5\"\n",
        ] {
            let (_temp, config) = layered_config(Some(body), None, None);
            assert!(
                trusted_workstream_continuation_config(&config, "m5").is_err(),
                "accepted partial policy: {body}"
            );
        }
    }

    #[test]
    fn malformed_allowlist_wrapper_and_bounds_refuse() {
        let valid = enabled_policy("[\"generous-corp/shipyard\"]");
        let cases = [
            valid.replace("[\"generous-corp/shipyard\"]", "[]"),
            valid.replace(
                "[\"generous-corp/shipyard\"]",
                "[\"generous-corp/shipyard\", \"generous-corp/shipyard\"]",
            ),
            valid.replace(
                "generous-corp/shipyard",
                "https://github.com/generous-corp/shipyard",
            ),
            valid.replace("generous-corp/shipyard", "Generous-Corp/Shipyard"),
            valid.replace("[\"generous-corp/shipyard\"]", "[\"z/repo\", \"a/repo\"]"),
            valid.replace("/opt/shipyard/bin/workstream-provider", "relative/provider"),
            valid.replace(
                "/opt/shipyard/bin/workstream-provider",
                "/opt/shipyard/bin/workstream-provider\\nforged",
            ),
            valid.replace(
                "/opt/shipyard/bin/workstream-provider",
                "/opt/shipyard/../bin/workstream-provider",
            ),
            valid.replace(DIGEST, &"A".repeat(64)),
            valid.replace("provider_id = \"codex\"\n", ""),
            valid.replace("adapter_id = \"codex-cli-v1\"", "adapter_id = \"codex!\""),
            valid.replace("deadline_seconds = 120", "deadline_seconds = 0"),
            valid.replace("deadline_seconds = 120", "deadline_seconds = 301"),
            valid.replace("max_stdout_bytes = 16384", "max_stdout_bytes = 1048577"),
            valid.replace("max_stderr_bytes = 16384", "max_stderr_bytes = 0"),
            format!("{valid}\nunknown = true\n"),
        ];
        for body in cases {
            let (_temp, config) = layered_config(Some(&body), None, None);
            assert!(
                trusted_workstream_continuation_config(&config, "m5").is_err(),
                "accepted invalid policy: {body}"
            );
        }
    }
}
