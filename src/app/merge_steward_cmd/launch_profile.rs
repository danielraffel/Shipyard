use super::{CliFailure, is_full_sha};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::provider_wrapper::{
    ProviderReasoningEffortV1, provider_reasoning_effort_name, subrouter_account_environment_key,
};

const MAX_PROFILE_BYTES: u64 = 64 * 1024;
const MAX_ARGV_ITEMS: usize = 64;
const MAX_ARG_BYTES: usize = 4 * 1024;
const MAX_ARGV_BYTES: usize = 16 * 1024;
const MAX_METADATA_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 4 * 1024;
const MAX_CONTEXT_URL_BYTES: usize = 4 * 1024;
const PROTECTED_PROFILE_PREFIX: &[u8] = b"shipyard-launch-profile-v1\0";

/// Private, provider- and terminal-neutral process restoration contract.
///
/// Generic handoff storage does not interpret or execute this data. The native
/// fresh-agent publication path separately accepts only a strict prompt-free
/// grammar before projecting typed metadata into its provider request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchProfileV1 {
    pub(super) schema_version: u32,
    pub(super) launch_argv: Vec<String>,
    pub(super) resume_argv: Vec<String>,
    /// Private Subrouter routing headers/environment restored with either argv.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) route_environment: BTreeMap<String, String>,
    pub(super) provider: ProviderMetadataV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) session: Option<SessionProvenanceV1>,
    pub(super) checkpoint: CheckpointProvenanceV1,
    pub(super) worktree: WorktreeProvenanceV1,
    /// Immutable inputs a fresh process must reconstruct before it can accept
    /// ownership. Older profiles decode without this field but cannot authorize
    /// fresh-agent dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) continuation_bootstrap: Option<ContinuationBootstrapV1>,
    pub(super) recovery_policy: RecoveryPolicyV1,
}

impl LaunchProfileV1 {
    pub(crate) fn worktree_path(&self) -> &str {
        &self.worktree.path
    }

    pub(crate) fn validate_native_fresh_agent_grammar(&self) -> Result<(), CliFailure> {
        if let Some(model_id) = self.provider.model.as_deref() {
            validate_provider_option("model ID", model_id)?;
        }
        validate_route_account_selection(self)?;
        validate_native_argv(self, &self.launch_argv, false)?;
        validate_native_argv(self, &self.resume_argv, true)
    }

    pub(crate) fn protected_resume_route(
        &self,
        profile_digest: String,
    ) -> crate::provider_wrapper::ProtectedProviderRouteV1 {
        crate::provider_wrapper::ProtectedProviderRouteV1 {
            argv: self.resume_argv.clone(),
            environment: self.route_environment.clone(),
            account_id: self.provider.account.clone(),
            native_session_id: self
                .session
                .as_ref()
                .map(|session| session.provider_session_id.clone())
                .unwrap_or_default(),
            profile_digest,
        }
    }
}

/// Exact, provider-neutral expectation for a handle-only fresh-agent resume.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ContinuationBootstrapV1 {
    pub(super) workstream_handle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) context_url: Option<String>,
    pub(super) plan_sha256: String,
    pub(super) root_revision: u64,
    pub(super) issue_revision: u64,
    pub(super) projection_revision: u64,
    pub(super) material_event_revision: u64,
    pub(super) checkpoint_id: String,
    pub(super) checkpoint_generation: u64,
    pub(super) checkpoint_digest: String,
    pub(super) repository: String,
    pub(super) head_sha: String,
    pub(super) expected_resume_context_digest: String,
    pub(super) success_continuation_digest: String,
    pub(super) failure_continuation_digest: String,
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
    #[serde(
        default,
        rename = "reasoning_effort",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) reasoning_effort: Option<ProviderReasoningEffortV1>,
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

    fn provider_launch_options(&self) -> crate::work_ledger::FreshAgentProviderLaunchOptions {
        crate::work_ledger::FreshAgentProviderLaunchOptions {
            model_id: self.provider.model.clone(),
            reasoning_effort: self.provider.reasoning_effort,
        }
    }

    fn profile_digest(&self) -> crate::work_ledger::WorkLedgerResult<String> {
        launch_profile_digest(self).map_err(|error| {
            crate::work_ledger::WorkLedgerError::Refused(format!(
                "launch profile is invalid: {}",
                error.message()
            ))
        })
    }

    fn protected_profile_bytes(&self) -> crate::work_ledger::WorkLedgerResult<Vec<u8>> {
        launch_profile_protected_bytes(self).map_err(|error| {
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
        ) && self
            .continuation_bootstrap
            .as_ref()
            .is_some_and(|bootstrap| validate_continuation_bootstrap(self, bootstrap).is_ok())
    }

    fn resume_expectation(&self) -> Option<crate::work_ledger::FreshAgentResumeExpectation<'_>> {
        let bootstrap = self
            .continuation_bootstrap
            .as_ref()
            .filter(|bootstrap| validate_continuation_bootstrap(self, bootstrap).is_ok())?;
        Some(crate::work_ledger::FreshAgentResumeExpectation {
            workstream_handle: &bootstrap.workstream_handle,
            context_url: bootstrap.context_url.as_deref(),
            plan_sha256: &bootstrap.plan_sha256,
            root_revision: bootstrap.root_revision,
            issue_revision: bootstrap.issue_revision,
            projection_revision: bootstrap.projection_revision,
            material_event_revision: bootstrap.material_event_revision,
            checkpoint_id: &bootstrap.checkpoint_id,
            checkpoint_generation: bootstrap.checkpoint_generation,
            checkpoint_digest: &bootstrap.checkpoint_digest,
            repository: &bootstrap.repository,
            head_sha: &bootstrap.head_sha,
            expected_resume_context_digest: &bootstrap.expected_resume_context_digest,
            success_continuation_digest: &bootstrap.success_continuation_digest,
            failure_continuation_digest: &bootstrap.failure_continuation_digest,
        })
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
    validate_route_environment(profile)?;
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
    if let Some(bootstrap) = profile.continuation_bootstrap.as_ref() {
        validate_continuation_bootstrap(profile, bootstrap)?;
    }
    Ok(())
}

fn validate_native_argv(
    profile: &LaunchProfileV1,
    argv: &[String],
    resume: bool,
) -> Result<(), CliFailure> {
    let executable = argv
        .first()
        .map(String::as_str)
        .map(Path::new)
        .and_then(|path| path.file_name())
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let provider = profile.provider.provider.as_str();
    if executable != "subrouter" || argv.get(1).map(String::as_str) != Some(provider) {
        return Err(CliFailure::new(
            1,
            "native fresh-agent argv requires an exact Subrouter provider wrapper",
        ));
    }
    let expected_session = profile
        .session
        .as_ref()
        .map(|session| session.provider_session_id.as_str());
    let tail = &argv[2..];
    let matches_exact = |needle: &str| tail.iter().filter(|value| value.as_str() == needle).count();
    if tail
        .iter()
        .any(|value| value.chars().any(char::is_whitespace))
    {
        return Err(CliFailure::new(
            1,
            "native fresh-agent argv contains a prompt-bearing argument",
        ));
    }
    if profile
        .provider
        .model
        .as_deref()
        .is_some_and(|model| matches_exact(model) != 1)
        || profile.provider.reasoning_effort.is_some_and(|effort| {
            let effort = provider_reasoning_effort_name(effort);
            !tail
                .iter()
                .any(|value| value == effort || value.contains(&format!("=\"{effort}\"")))
        })
        || expected_session.is_some_and(|session| matches_exact(session) != usize::from(resume))
        || (resume && expected_session.is_none())
    {
        return Err(CliFailure::new(
            1,
            "native fresh-agent argv does not exactly match validated provider metadata",
        ));
    }
    Ok(())
}

fn validate_route_environment(profile: &LaunchProfileV1) -> Result<(), CliFailure> {
    if profile.route_environment.len() > 16 {
        return Err(CliFailure::new(1, "native route environment is too large"));
    }
    for (name, value) in &profile.route_environment {
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            || !name.starts_with("SUBROUTER_")
            || value.is_empty()
            || value.len() > MAX_ARG_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(CliFailure::new(1, "native route environment is invalid"));
        }
    }
    Ok(())
}

fn validate_route_account_selection(profile: &LaunchProfileV1) -> Result<(), CliFailure> {
    let account_values = profile
        .route_environment
        .iter()
        .filter(|(name, _)| name.ends_with("_ACCOUNT_ID"))
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>();
    let expected_account_key = subrouter_account_environment_key(&profile.provider.provider);
    match profile.provider.account.as_deref() {
        Some(account)
            if account_values == [account]
                && profile
                    .route_environment
                    .get(&expected_account_key)
                    .map(String::as_str)
                    == Some(account) =>
        {
            Ok(())
        }
        None if account_values.is_empty() => Ok(()),
        _ => Err(CliFailure::new(
            1,
            "native route environment does not exactly bind the selected account",
        )),
    }
}

fn validate_provider_option(label: &str, value: &str) -> Result<(), CliFailure> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_' | b'-' | b':')
        })
    {
        return Err(CliFailure::new(
            1,
            format!("{label} must be a bounded canonical provider token"),
        ));
    }
    Ok(())
}

fn validate_continuation_bootstrap(
    profile: &LaunchProfileV1,
    bootstrap: &ContinuationBootstrapV1,
) -> Result<(), CliFailure> {
    if bootstrap.workstream_handle.is_empty()
        || bootstrap.workstream_handle.len() > 124
        || bootstrap.workstream_handle.chars().any(char::is_whitespace)
        || bootstrap.workstream_handle.chars().any(char::is_control)
    {
        return Err(CliFailure::new(
            1,
            "bootstrap workstream handle must be 1-124 non-whitespace characters",
        ));
    }
    if let Some(context_url) = bootstrap.context_url.as_deref() {
        validate_context_url(context_url)?;
    }
    validate_lower_sha256("bootstrap plan digest", &bootstrap.plan_sha256)?;
    if bootstrap.projection_revision == 0 {
        return Err(CliFailure::new(
            1,
            "bootstrap projection revision must be positive",
        ));
    }
    validate_metadata("bootstrap checkpoint ID", &bootstrap.checkpoint_id)?;
    if bootstrap.checkpoint_generation == 0 {
        return Err(CliFailure::new(
            1,
            "bootstrap checkpoint generation must be positive",
        ));
    }
    for (label, digest) in [
        ("bootstrap checkpoint digest", &bootstrap.checkpoint_digest),
        (
            "bootstrap expected resume-context digest",
            &bootstrap.expected_resume_context_digest,
        ),
        (
            "bootstrap success continuation digest",
            &bootstrap.success_continuation_digest,
        ),
        (
            "bootstrap failure continuation digest",
            &bootstrap.failure_continuation_digest,
        ),
    ] {
        validate_lower_sha256(label, digest)?;
    }
    validate_repository(&bootstrap.repository)?;
    if !is_full_sha(&bootstrap.head_sha)
        || !bootstrap
            .head_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CliFailure::new(
            1,
            "bootstrap head must be a full lowercase 40-character SHA-1",
        ));
    }
    if bootstrap.checkpoint_id != profile.checkpoint.checkpoint_id
        || bootstrap.checkpoint_generation != profile.checkpoint.generation
        || bootstrap.checkpoint_digest != profile.checkpoint.digest
        || bootstrap.repository != profile.worktree.repository
        || bootstrap.head_sha != profile.worktree.head_sha
    {
        return Err(CliFailure::new(
            1,
            "bootstrap checkpoint, repository, or head does not match launch provenance",
        ));
    }
    Ok(())
}

fn validate_context_url(value: &str) -> Result<(), CliFailure> {
    let remainder = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"));
    let authority = remainder.and_then(|remainder| remainder.split(['/', '?', '#']).next());
    if value.len() > MAX_CONTEXT_URL_BYTES
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
        || authority.is_none_or(str::is_empty)
        || authority.is_some_and(|authority| authority.contains('@'))
        || value.contains('#')
    {
        return Err(CliFailure::new(
            1,
            "bootstrap context URL must be a bounded canonical HTTP(S) URL without userinfo or fragment",
        ));
    }
    Ok(())
}

pub(super) fn launch_profile_digest(profile: &LaunchProfileV1) -> Result<String, CliFailure> {
    let bytes = launch_profile_protected_bytes(profile)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn launch_profile_protected_bytes(profile: &LaunchProfileV1) -> Result<Vec<u8>, CliFailure> {
    validate_launch_profile(profile)?;
    let canonical = serde_json::to_vec(profile)
        .map_err(|error| CliFailure::new(1, format!("serialize launch profile: {error}")))?;
    let mut bytes = Vec::with_capacity(PROTECTED_PROFILE_PREFIX.len() + canonical.len());
    bytes.extend_from_slice(PROTECTED_PROFILE_PREFIX);
    bytes.extend_from_slice(&canonical);
    Ok(bytes)
}

/// Decode only the canonical domain-separated bytes protected by the ledger.
#[allow(dead_code)] // Activated by the daemon wake-loop integration slice.
pub(crate) fn decode_protected_launch_profile(
    bytes: &[u8],
) -> crate::work_ledger::WorkLedgerResult<LaunchProfileV1> {
    let canonical = bytes
        .strip_prefix(PROTECTED_PROFILE_PREFIX)
        .ok_or_else(|| {
            crate::work_ledger::WorkLedgerError::Refused(
                "protected launch profile has the wrong domain".to_owned(),
            )
        })?;
    if canonical.starts_with(PROTECTED_PROFILE_PREFIX) {
        return Err(crate::work_ledger::WorkLedgerError::Refused(
            "protected launch profile has repeated domain separation".to_owned(),
        ));
    }
    let profile: LaunchProfileV1 = serde_json::from_slice(canonical).map_err(|_| {
        crate::work_ledger::WorkLedgerError::Refused(
            "protected launch profile JSON is invalid".to_owned(),
        )
    })?;
    validate_launch_profile(&profile).map_err(|error| {
        crate::work_ledger::WorkLedgerError::Refused(format!(
            "protected launch profile is invalid: {}",
            error.message()
        ))
    })?;
    let recomputed = launch_profile_protected_bytes(&profile).map_err(|error| {
        crate::work_ledger::WorkLedgerError::Refused(format!(
            "protected launch profile cannot be canonicalized: {}",
            error.message()
        ))
    })?;
    if recomputed != bytes {
        return Err(crate::work_ledger::WorkLedgerError::Refused(
            "protected launch profile bytes are not canonical".to_owned(),
        ));
    }
    Ok(profile)
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

fn validate_lower_sha256(label: &str, value: &str) -> Result<(), CliFailure> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!("{label} must be 64 lowercase hexadecimal characters"),
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
            route_environment: BTreeMap::from([(
                "SUBROUTER_SUBSCRIPTION_ROUTER_ACCOUNT_ID".into(),
                "account-a".into(),
            )]),
            provider: ProviderMetadataV1 {
                provider: "subscription-router".into(),
                account: Some("account-a".into()),
                model: Some("model-x".into()),
                reasoning_effort: None,
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
            continuation_bootstrap: Some(ContinuationBootstrapV1 {
                workstream_handle: "GEN-43".into(),
                context_url: Some("https://linear.example/GEN-43".into()),
                plan_sha256: "f".repeat(64),
                root_revision: 0,
                issue_revision: 0,
                projection_revision: 4,
                material_event_revision: 0,
                checkpoint_id: "checkpoint-7".into(),
                checkpoint_generation: 3,
                checkpoint_digest: "a".repeat(64),
                repository: "owner/repo".into(),
                head_sha: "b".repeat(40),
                expected_resume_context_digest: "c".repeat(64),
                success_continuation_digest: "d".repeat(64),
                failure_continuation_digest: "e".repeat(64),
            }),
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
    fn wake_consumer_projects_only_validated_provider_metadata() {
        use crate::work_ledger::FreshAgentLaunchProfile;

        let mut profile = profile();
        profile.provider.reasoning_effort = Some(ProviderReasoningEffortV1::Medium);
        assert_eq!(
            FreshAgentLaunchProfile::provider_launch_options(&profile).model_id,
            Some("model-x".to_owned())
        );
        assert_eq!(
            FreshAgentLaunchProfile::provider_launch_options(&profile).reasoning_effort,
            Some(ProviderReasoningEffortV1::Medium)
        );
        assert_eq!(
            FreshAgentLaunchProfile::profile_digest(&profile).expect("consumer digest"),
            launch_profile_digest(&profile).expect("stored digest")
        );
        assert!(FreshAgentLaunchProfile::permits_fresh_agent(&profile));
    }

    #[test]
    fn native_grammar_is_prompt_free_and_exactly_matches_metadata() {
        let mut codex = profile();
        codex.provider.provider = "codex".into();
        codex.route_environment =
            BTreeMap::from([("SUBROUTER_CODEX_ACCOUNT_ID".into(), "account-a".into())]);
        codex.provider.reasoning_effort = Some(ProviderReasoningEffortV1::Medium);
        codex.launch_argv = vec![
            "subrouter".into(),
            "codex".into(),
            "--model".into(),
            "model-x".into(),
            "-c".into(),
            "model_reasoning_effort=\"medium\"".into(),
        ];
        codex.resume_argv = vec![
            "subrouter".into(),
            "codex".into(),
            "resume".into(),
            "--model".into(),
            "model-x".into(),
            "-c".into(),
            "model_reasoning_effort=\"medium\"".into(),
            "session-7".into(),
        ];
        codex
            .validate_native_fresh_agent_grammar()
            .expect("codex grammar");

        let mut claude = profile();
        claude.provider.provider = "claude".into();
        claude.route_environment =
            BTreeMap::from([("SUBROUTER_CLAUDE_ACCOUNT_ID".into(), "account-a".into())]);
        claude.provider.reasoning_effort = Some(ProviderReasoningEffortV1::High);
        claude.launch_argv = vec![
            "subrouter".into(),
            "claude".into(),
            "--model".into(),
            "model-x".into(),
            "--effort".into(),
            "high".into(),
        ];
        claude.resume_argv = vec![
            "subrouter".into(),
            "claude".into(),
            "--model".into(),
            "model-x".into(),
            "--effort".into(),
            "high".into(),
            "--resume".into(),
            "session-7".into(),
        ];
        claude
            .validate_native_fresh_agent_grammar()
            .expect("claude grammar");

        let mut future_claude = claude.clone();
        future_claude.provider.reasoning_effort = Some(ProviderReasoningEffortV1::Ultra);
        future_claude.launch_argv[5] = "ultra".into();
        future_claude.resume_argv[5] = "ultra".into();
        future_claude
            .validate_native_fresh_agent_grammar()
            .expect("protected profile owns future provider grammar");

        let mut qwen = codex.clone();
        qwen.provider.provider = "qwen".into();
        qwen.provider.reasoning_effort = None;
        qwen.route_environment =
            BTreeMap::from([("SUBROUTER_QWEN_ACCOUNT_ID".into(), "account-a".into())]);
        qwen.launch_argv = vec![
            "subrouter".into(),
            "qwen".into(),
            "--model".into(),
            "model-x".into(),
        ];
        qwen.resume_argv = vec![
            "subrouter".into(),
            "qwen".into(),
            "resume".into(),
            "--model".into(),
            "model-x".into(),
            "session-7".into(),
        ];
        qwen.validate_native_fresh_agent_grammar()
            .expect("registered provider grammar requires no lifecycle branch");

        let mut unrecognized_effort = codex.clone();
        unrecognized_effort.launch_argv[5] = "model_reasoning_effort=\"wrong\"".into();
        assert!(
            unrecognized_effort
                .validate_native_fresh_agent_grammar()
                .is_err()
        );
        codex.launch_argv.push("raw prompt with a secret".into());
        assert!(codex.validate_native_fresh_agent_grammar().is_err());
        assert_eq!(codex.provider.model.as_deref(), Some("model-x"));
        assert!(Path::new(codex.worktree_path()).is_absolute());
    }

    #[test]
    fn protected_decoder_requires_one_exact_domain_and_canonical_json() {
        let profile = profile();
        let bytes = launch_profile_protected_bytes(&profile).expect("protected bytes");
        assert_eq!(
            decode_protected_launch_profile(&bytes).expect("strict decode"),
            profile
        );
        assert!(decode_protected_launch_profile(&bytes[PROTECTED_PROFILE_PREFIX.len()..]).is_err());

        let mut doubled = PROTECTED_PROFILE_PREFIX.to_vec();
        doubled.extend_from_slice(&bytes);
        assert!(decode_protected_launch_profile(&doubled).is_err());

        let mut noncanonical = PROTECTED_PROFILE_PREFIX.to_vec();
        noncanonical.extend_from_slice(
            serde_json::to_string_pretty(&profile)
                .expect("pretty profile")
                .as_bytes(),
        );
        assert!(decode_protected_launch_profile(&noncanonical).is_err());
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

    #[test]
    fn legacy_profile_decodes_but_cannot_authorize_fresh_dispatch() {
        use crate::work_ledger::FreshAgentLaunchProfile;

        let mut value = serde_json::to_value(profile()).expect("value");
        value
            .as_object_mut()
            .expect("profile object")
            .remove("continuation_bootstrap");
        value
            .as_object_mut()
            .expect("profile object")
            .remove("route_environment");
        let mut decoded: LaunchProfileV1 = serde_json::from_value(value).expect("legacy decode");
        validate_launch_profile(&decoded).expect("legacy profile remains valid");
        assert!(decoded.route_environment.is_empty());
        assert!(decoded.validate_native_fresh_agent_grammar().is_err());
        assert!(!FreshAgentLaunchProfile::permits_fresh_agent(&decoded));
        decoded.recovery_policy = RecoveryPolicyV1::FreshCheckpointOnly;
        assert!(!FreshAgentLaunchProfile::permits_fresh_agent(&decoded));
    }

    #[test]
    fn fresh_checkpoint_dispatch_requires_and_exposes_exact_bootstrap() {
        use crate::work_ledger::FreshAgentLaunchProfile;

        let mut complete = profile();
        complete.recovery_policy = RecoveryPolicyV1::FreshCheckpointOnly;
        assert!(FreshAgentLaunchProfile::permits_fresh_agent(&complete));
        let expectation = FreshAgentLaunchProfile::resume_expectation(&complete)
            .expect("typed resume expectation");
        assert_eq!(expectation.workstream_handle, "GEN-43");
        assert_eq!(
            expectation.context_url,
            Some("https://linear.example/GEN-43")
        );
        assert_eq!(expectation.plan_sha256, "f".repeat(64));
        assert_eq!(expectation.root_revision, 0);
        assert_eq!(expectation.issue_revision, 0);
        assert_eq!(expectation.projection_revision, 4);
        assert_eq!(expectation.material_event_revision, 0);
        assert_eq!(expectation.checkpoint_id, "checkpoint-7");
        assert_eq!(expectation.checkpoint_generation, 3);
        assert_eq!(expectation.checkpoint_digest, "a".repeat(64));
        assert_eq!(expectation.repository, "owner/repo");
        assert_eq!(expectation.head_sha, "b".repeat(40));
        assert_eq!(expectation.expected_resume_context_digest, "c".repeat(64));
        assert_eq!(expectation.success_continuation_digest, "d".repeat(64));
        assert_eq!(expectation.failure_continuation_digest, "e".repeat(64));

        complete.continuation_bootstrap = None;
        assert!(!FreshAgentLaunchProfile::permits_fresh_agent(&complete));
        assert!(FreshAgentLaunchProfile::resume_expectation(&complete).is_none());
    }

    #[test]
    fn bootstrap_is_strict_and_bound_to_exact_launch_provenance() {
        let base = profile();
        let mut invalid = base.clone();
        invalid
            .continuation_bootstrap
            .as_mut()
            .expect("bootstrap")
            .workstream_handle = "GEN 43".into();
        assert!(validate_launch_profile(&invalid).is_err());

        let mutations: [fn(&mut ContinuationBootstrapV1); 9] = [
            |bootstrap| bootstrap.plan_sha256 = "F".repeat(64),
            |bootstrap| bootstrap.projection_revision = 0,
            |bootstrap| bootstrap.checkpoint_generation += 1,
            |bootstrap| bootstrap.checkpoint_digest = "f".repeat(64),
            |bootstrap| bootstrap.repository = "owner/other".into(),
            |bootstrap| bootstrap.head_sha = "f".repeat(40),
            |bootstrap| bootstrap.expected_resume_context_digest = "F".repeat(64),
            |bootstrap| bootstrap.success_continuation_digest = "short".into(),
            |bootstrap| bootstrap.failure_continuation_digest = "short".into(),
        ];
        for mutation in mutations {
            let mut invalid = base.clone();
            mutation(invalid.continuation_bootstrap.as_mut().expect("bootstrap"));
            assert!(validate_launch_profile(&invalid).is_err());
        }

        let mut invalid_url = base.clone();
        invalid_url
            .continuation_bootstrap
            .as_mut()
            .expect("bootstrap")
            .context_url = Some("file:///tmp/context".into());
        assert!(validate_launch_profile(&invalid_url).is_err());
        for url in [
            "https://",
            "https://user@example.test/GEN-43",
            "https://linear.example/GEN-43#mutable-fragment",
        ] {
            let mut invalid = base.clone();
            invalid
                .continuation_bootstrap
                .as_mut()
                .expect("bootstrap")
                .context_url = Some(url.into());
            assert!(validate_launch_profile(&invalid).is_err());
        }

        let mut value = serde_json::to_value(base).expect("value");
        value["continuation_bootstrap"]["forged"] = serde_json::json!(true);
        assert!(serde_json::from_value::<LaunchProfileV1>(value).is_err());

        for required in [
            "workstream_handle",
            "plan_sha256",
            "root_revision",
            "issue_revision",
            "projection_revision",
            "material_event_revision",
            "checkpoint_id",
            "checkpoint_generation",
            "checkpoint_digest",
            "repository",
            "head_sha",
            "expected_resume_context_digest",
            "success_continuation_digest",
            "failure_continuation_digest",
        ] {
            let mut value = serde_json::to_value(profile()).expect("value");
            value["continuation_bootstrap"]
                .as_object_mut()
                .expect("bootstrap object")
                .remove(required);
            assert!(
                serde_json::from_value::<LaunchProfileV1>(value).is_err(),
                "missing bootstrap field {required} must fail"
            );
        }
    }

    #[test]
    fn bootstrap_fields_are_exact_and_digest_bound() {
        let profile = profile();
        let bootstrap = profile.continuation_bootstrap.as_ref().expect("bootstrap");
        assert_eq!(bootstrap.workstream_handle, "GEN-43");
        assert_eq!(bootstrap.plan_sha256, "f".repeat(64));
        assert_eq!(bootstrap.root_revision, 0);
        assert_eq!(bootstrap.issue_revision, 0);
        assert_eq!(bootstrap.projection_revision, 4);
        assert_eq!(bootstrap.material_event_revision, 0);
        assert_eq!(bootstrap.checkpoint_id, profile.checkpoint.checkpoint_id);
        assert_eq!(
            bootstrap.checkpoint_generation,
            profile.checkpoint.generation
        );
        assert_eq!(bootstrap.checkpoint_digest, profile.checkpoint.digest);
        assert_eq!(bootstrap.repository, profile.worktree.repository);
        assert_eq!(bootstrap.head_sha, profile.worktree.head_sha);

        let original = launch_profile_digest(&profile).expect("original digest");
        let protected = launch_profile_protected_bytes(&profile).expect("protected bytes");
        assert_eq!(
            protected.strip_prefix(b"shipyard-launch-profile-v1\0"),
            Some(
                serde_json::to_vec(&profile)
                    .expect("canonical json")
                    .as_slice()
            )
        );
        assert_eq!(original, hex::encode(Sha256::digest(&protected)));
        assert!(
            !protected
                .strip_prefix(b"shipyard-launch-profile-v1\0")
                .expect("single prefix")
                .starts_with(b"shipyard-launch-profile-v1\0")
        );
        let mut changed = profile.clone();
        changed
            .continuation_bootstrap
            .as_mut()
            .expect("bootstrap")
            .projection_revision += 1;
        assert_ne!(
            original,
            launch_profile_digest(&changed).expect("changed digest")
        );

        let mut changed_plan = profile;
        changed_plan
            .continuation_bootstrap
            .as_mut()
            .expect("bootstrap")
            .plan_sha256 = "0".repeat(64);
        assert_ne!(
            original,
            launch_profile_digest(&changed_plan).expect("changed plan digest")
        );
    }
}
