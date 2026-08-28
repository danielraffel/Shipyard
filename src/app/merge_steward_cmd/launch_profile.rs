use super::{CliFailure, is_full_sha};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

const MAX_PROFILE_BYTES: u64 = 64 * 1024;
const MAX_ARGV_ITEMS: usize = 64;
const MAX_ARG_BYTES: usize = 4 * 1024;
const MAX_ARGV_BYTES: usize = 16 * 1024;
const MAX_METADATA_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 4 * 1024;

/// Private, provider- and terminal-neutral process restoration contract.
///
/// Shipyard persists this data but does not interpret or execute it. A trusted
/// executor must bind the exact profile and work-item generation to its own
/// process-launch contract before using either argv.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LaunchProfileV1 {
    pub(super) schema_version: u32,
    pub(super) launch_argv: Vec<String>,
    pub(super) resume_argv: Vec<String>,
    pub(super) provider: ProviderMetadataV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) session: Option<SessionProvenanceV1>,
    pub(super) checkpoint: CheckpointProvenanceV1,
    pub(super) worktree: WorktreeProvenanceV1,
    pub(super) recovery_policy: RecoveryPolicyV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionProvenanceV1 {
    pub(super) agent_provider: String,
    pub(super) provider_session_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderMetadataV1 {
    #[serde(rename = "provider_id")]
    pub(super) provider: String,
    #[serde(
        default,
        rename = "account_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) account: Option<String>,
    #[serde(default, rename = "model_id", skip_serializing_if = "Option::is_none")]
    pub(super) model: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointProvenanceV1 {
    pub(super) checkpoint_id: String,
    pub(super) generation: u64,
    pub(super) digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorktreeProvenanceV1 {
    pub(super) repository: String,
    pub(super) path: String,
    pub(super) head_sha: String,
    pub(super) lineage_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RecoveryPolicyV1 {
    ExactSessionOnly,
    ExactSessionThenFreshCheckpoint,
    FreshCheckpointOnly,
}

impl crate::work_ledger::FreshAgentLaunchProfile for LaunchProfileV1 {
    fn provider_id(&self) -> &str {
        &self.provider.provider
    }

    fn launch_argv(&self) -> &[String] {
        &self.launch_argv
    }

    fn profile_digest(&self) -> crate::work_ledger::WorkLedgerResult<String> {
        launch_profile_digest(self).map_err(|error| {
            crate::work_ledger::WorkLedgerError::Refused(format!(
                "launch profile is invalid: {}",
                error.message()
            ))
        })
    }

    fn permits_fresh_agent(&self) -> bool {
        matches!(
            self.recovery_policy,
            RecoveryPolicyV1::ExactSessionThenFreshCheckpoint
                | RecoveryPolicyV1::FreshCheckpointOnly
        )
    }
}

pub(super) fn load_launch_profile(path: &Path) -> Result<LaunchProfileV1, CliFailure> {
    let metadata = fs::metadata(path)
        .map_err(|error| CliFailure::new(1, format!("read launch profile metadata: {error}")))?;
    if !metadata.is_file() || metadata.len() > MAX_PROFILE_BYTES {
        return Err(CliFailure::new(
            1,
            "launch profile must be a regular JSON file no larger than 64 KiB",
        ));
    }
    let bytes = fs::read(path)
        .map_err(|error| CliFailure::new(1, format!("read launch profile: {error}")))?;
    let profile: LaunchProfileV1 = serde_json::from_slice(&bytes)
        .map_err(|error| CliFailure::new(1, format!("invalid launch profile JSON: {error}")))?;
    validate_launch_profile(&profile)?;
    Ok(profile)
}

pub(super) fn validate_launch_profile(profile: &LaunchProfileV1) -> Result<(), CliFailure> {
    if profile.schema_version != 1 {
        return Err(CliFailure::new(1, "unsupported launch profile schema"));
    }
    validate_argv("launch", &profile.launch_argv)?;
    validate_argv("resume", &profile.resume_argv)?;
    validate_metadata("provider ID", &profile.provider.provider)?;
    if let Some(account_id) = profile.provider.account.as_deref() {
        validate_metadata("account ID", account_id)?;
    }
    if let Some(model_id) = profile.provider.model.as_deref() {
        validate_metadata("model ID", model_id)?;
    }
    if let Some(session) = profile.session.as_ref() {
        validate_metadata("session agent provider", &session.agent_provider)?;
        validate_metadata("provider session ID", &session.provider_session_id)?;
    }
    validate_metadata("checkpoint ID", &profile.checkpoint.checkpoint_id)?;
    if profile.checkpoint.generation == 0 {
        return Err(CliFailure::new(1, "checkpoint generation must be positive"));
    }
    validate_sha256("checkpoint digest", &profile.checkpoint.digest)?;
    validate_repository(&profile.worktree.repository)?;
    if !Path::new(&profile.worktree.path).is_absolute()
        || profile.worktree.path.len() > MAX_PATH_BYTES
        || profile.worktree.path.contains('\0')
    {
        return Err(CliFailure::new(
            1,
            "worktree path must be a bounded absolute path without NUL bytes",
        ));
    }
    if !is_full_sha(&profile.worktree.head_sha) {
        return Err(CliFailure::new(
            1,
            "worktree head must be a full 40-character SHA-1",
        ));
    }
    validate_metadata("worktree lineage ID", &profile.worktree.lineage_id)?;
    Ok(())
}

pub(super) fn launch_profile_digest(profile: &LaunchProfileV1) -> Result<String, CliFailure> {
    validate_launch_profile(profile)?;
    let bytes = serde_json::to_vec(profile)
        .map_err(|error| CliFailure::new(1, format!("serialize launch profile: {error}")))?;
    let mut digest = Sha256::new();
    digest.update(b"shipyard-launch-profile-v1\0");
    digest.update(bytes);
    Ok(hex::encode(digest.finalize()))
}

pub(super) fn launch_profile_integrity_hash(
    profile_digest: &str,
    generation: u64,
    revision: u64,
    route_id: Option<&str>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"shipyard-launch-profile-envelope-v1\0");
    for field in [profile_digest, route_id.unwrap_or_default()] {
        digest.update(field.len().to_be_bytes());
        digest.update(field.as_bytes());
    }
    digest.update(generation.to_be_bytes());
    digest.update(revision.to_be_bytes());
    hex::encode(digest.finalize())
}

fn validate_argv(label: &str, argv: &[String]) -> Result<(), CliFailure> {
    if argv.is_empty() || argv.len() > MAX_ARGV_ITEMS || argv[0].is_empty() {
        return Err(CliFailure::new(
            1,
            format!(
                "{label} argv must contain 1-{MAX_ARGV_ITEMS} items and a non-empty executable"
            ),
        ));
    }
    let mut total = 0usize;
    for item in argv {
        if item.len() > MAX_ARG_BYTES || item.contains('\0') {
            return Err(CliFailure::new(
                1,
                format!("{label} argv contains an invalid or oversized item"),
            ));
        }
        total = total.saturating_add(item.len());
    }
    if total > MAX_ARGV_BYTES {
        return Err(CliFailure::new(1, format!("{label} argv exceeds 16 KiB")));
    }
    Ok(())
}

fn validate_metadata(label: &str, value: &str) -> Result<(), CliFailure> {
    if value.is_empty()
        || value.len() > MAX_METADATA_BYTES
        || value.contains('\0')
        || value.chars().any(char::is_control)
    {
        return Err(CliFailure::new(
            1,
            format!("{label} must be 1-{MAX_METADATA_BYTES} non-control bytes"),
        ));
    }
    Ok(())
}

fn validate_repository(value: &str) -> Result<(), CliFailure> {
    let mut parts = value.split('/');
    let valid = matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(repo), None)
        if !owner.is_empty() && !repo.is_empty()
            && [owner, repo].into_iter().all(|part| part.chars().all(|ch|
                ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))));
    if valid {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            "worktree repository must be an owner/repo slug",
        ))
    }
}

fn validate_sha256(label: &str, value: &str) -> Result<(), CliFailure> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!("{label} must be 64 hexadecimal characters"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> LaunchProfileV1 {
        LaunchProfileV1 {
            schema_version: 1,
            launch_argv: vec!["provider-router".into(), "agent".into(), "--new".into()],
            resume_argv: vec![
                "provider-router".into(),
                "agent".into(),
                "--resume".into(),
                "session-7".into(),
            ],
            provider: ProviderMetadataV1 {
                provider: "subscription-router".into(),
                account: Some("account-a".into()),
                model: Some("model-x".into()),
            },
            session: Some(SessionProvenanceV1 {
                agent_provider: "agent-protocol".into(),
                provider_session_id: "session-7".into(),
            }),
            checkpoint: CheckpointProvenanceV1 {
                checkpoint_id: "checkpoint-7".into(),
                generation: 3,
                digest: "a".repeat(64),
            },
            worktree: WorktreeProvenanceV1 {
                repository: "owner/repo".into(),
                path: std::env::temp_dir()
                    .join("worktrees")
                    .join("feature")
                    .to_string_lossy()
                    .into_owned(),
                head_sha: "b".repeat(40),
                lineage_id: "lineage-7".into(),
            },
            recovery_policy: RecoveryPolicyV1::ExactSessionThenFreshCheckpoint,
        }
    }

    #[test]
    fn profile_is_terminal_and_provider_neutral() {
        let profile = profile();
        validate_launch_profile(&profile).expect("valid neutral profile");
        let encoded = serde_json::to_string(&profile).expect("serialize");
        for runtime_specific in ["cmux", "herdr", "codex"] {
            assert!(!encoded.to_ascii_lowercase().contains(runtime_specific));
        }
    }

    #[test]
    fn exact_argv_boundaries_and_digest_are_deterministic() {
        let profile = profile();
        assert_eq!(
            launch_profile_digest(&profile).expect("digest"),
            launch_profile_digest(&profile).expect("replay digest")
        );
        let decoded: LaunchProfileV1 =
            serde_json::from_slice(&serde_json::to_vec(&profile).expect("encode")).expect("decode");
        assert_eq!(decoded.launch_argv, profile.launch_argv);
        assert_eq!(decoded.resume_argv, profile.resume_argv);
    }

    #[test]
    fn wake_consumer_reads_the_exact_launch_array_without_translation() {
        use crate::work_ledger::FreshAgentLaunchProfile;

        let profile = profile();
        assert_eq!(
            FreshAgentLaunchProfile::launch_argv(&profile),
            profile.launch_argv.as_slice()
        );
        assert_eq!(
            FreshAgentLaunchProfile::profile_digest(&profile).expect("consumer digest"),
            launch_profile_digest(&profile).expect("stored digest")
        );
        assert!(FreshAgentLaunchProfile::permits_fresh_agent(&profile));
    }

    #[test]
    fn incomplete_or_unknown_profiles_fail_closed() {
        let mut value = serde_json::to_value(profile()).expect("value");
        value["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<LaunchProfileV1>(value).is_err());

        let mut invalid = profile();
        invalid.checkpoint.generation = 0;
        assert!(validate_launch_profile(&invalid).is_err());
        invalid = profile();
        invalid.resume_argv.clear();
        assert!(validate_launch_profile(&invalid).is_err());
        invalid = profile();
        invalid.worktree.path = "relative/path".into();
        assert!(validate_launch_profile(&invalid).is_err());
    }
}
