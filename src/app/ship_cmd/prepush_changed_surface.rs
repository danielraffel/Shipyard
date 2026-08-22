//! Default-off prospective changed-surface planning for supervised PR pushes.
//!
//! This module deliberately does not authorize execution. It transports a
//! protected-base plan to a repository pre-push hook, then records a dedupe
//! hint only after GitHub independently confirms the resulting pull request.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use chrono::Utc;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::CliFailure;
use super::super::changed_surface_cmd::{ChangedSurfacePlanArgs, observe_changed_surface_plan};
use crate::changed_surface::{
    ChangedSurfacePolicy, ExactHeadInput, ObservationStatus, PlannedSuite, ProtectedRefStatus,
    SelectionReceipt, plan_selection, policy_digest, policy_from_toml,
};
use crate::config::LoadedConfig;
use crate::executor::dispatch::ResolvedTarget;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};

const MODE_KEY: &str = "changed_surface_prepush.mode";
const RECEIPT_SCHEMA_VERSION: u32 = 1;
const HOOK_RESULT_SCHEMA_VERSION: u32 = 1;
const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
const PROSPECTIVE_PR_SENTINEL: u64 = u64::MAX;
const MAX_HOOK_RESULT_BYTES: u64 = 64 * 1024;
const ABANDONED_TRANSACTION_RETENTION: Duration = Duration::from_hours(24);
const MAX_ABANDONED_REAPS_PER_PUSH: usize = 64;

pub(super) const RECEIPT_PATH_ENV: &str = "SHIPYARD_CHANGED_SURFACE_PREPUSH_RECEIPT_PATH";
pub(super) const RECEIPT_DIGEST_ENV: &str = "SHIPYARD_CHANGED_SURFACE_PREPUSH_RECEIPT_SHA256";
pub(super) const TRANSACTION_NONCE_ENV: &str = "SHIPYARD_CHANGED_SURFACE_PREPUSH_TRANSACTION_NONCE";
pub(super) const RESULT_DIR_ENV: &str = "SHIPYARD_CHANGED_SURFACE_RESULT_DIR";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum PrepushMode {
    #[default]
    Off,
    ShadowCompare,
    Authoritative,
}

#[derive(Debug, Deserialize)]
struct BranchCommit {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct BranchMetadata {
    protected: bool,
    commit: BranchCommit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ProspectiveReceipt {
    schema_version: u32,
    repository: String,
    target: String,
    protected_base_ref: String,
    protected_base_sha: String,
    head_ref: String,
    head_sha: String,
    tree_sha: String,
    merge_base_sha: String,
    changed_paths_digest: String,
    policy_digest: String,
    planner_digest: String,
    coverage_contract_digest: String,
    inventory_digest: String,
    selected_tests_digest: String,
    hook_path: String,
    hook_sha256: String,
    transaction_nonce: String,
    result_dir: PathBuf,
    selection: SelectionReceipt,
}

/// The only hook result that can create a downstream dedupe hint.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HookResult {
    schema_version: u32,
    transaction_nonce: String,
    prospective_receipt_sha256: String,
    update_count: u32,
    update_ref: String,
    head_sha: String,
    tree_sha: String,
    selected_tests_digest: String,
    hook_sha256: String,
}

#[derive(Debug, Serialize)]
struct VerifiedSnapshot<'a> {
    schema_version: u32,
    disposition: &'static str,
    repository: &'a str,
    pull_request: u64,
    target: &'a str,
    head_sha: &'a str,
    tree_sha: &'a str,
    prospective_receipt_sha256: &'a str,
    selected_tests_digest: &'a str,
    transaction_nonce: &'a str,
}

pub(super) struct ProspectivePush {
    receipt: ProspectiveReceipt,
    receipt_path: PathBuf,
    receipt_digest: String,
    supervised_push_succeeded: bool,
}

impl ProspectivePush {
    pub(super) fn environment(&self) -> [(OsString, OsString); 4] {
        [
            (
                OsString::from(RECEIPT_PATH_ENV),
                self.receipt_path.as_os_str().to_owned(),
            ),
            (
                OsString::from(RECEIPT_DIGEST_ENV),
                OsString::from(&self.receipt_digest),
            ),
            (
                OsString::from(TRANSACTION_NONCE_ENV),
                OsString::from(&self.receipt.transaction_nonce),
            ),
            (
                OsString::from(RESULT_DIR_ENV),
                self.receipt.result_dir.as_os_str().to_owned(),
            ),
        ]
    }

    pub(super) fn mark_supervised_push_succeeded(&mut self) {
        self.supervised_push_succeeded = true;
    }
}

/// Build a prospective selector receipt from authenticated protected-base and
/// clean local facts. Any observation ambiguity declines the optimization and
/// leaves the ordinary full validation path untouched.
pub(super) fn prepare(
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    repository: &str,
    base_ref: &str,
    head_ref: &str,
    targets: &[ResolvedTarget],
) -> Result<Option<ProspectivePush>, CliFailure> {
    let mode = trusted_mode(config)?;
    if mode == PrepushMode::Off || !cfg!(unix) {
        return Ok(None);
    }
    // Authoritative is intentionally recognized so config drift is visible,
    // but this slice never transports an authoritative pre-push plan.
    if mode == PrepushMode::Authoritative {
        return Ok(None);
    }
    let Some(observed) = observe_prospective(config, cwd, repository, base_ref, head_ref, targets)
    else {
        return Ok(None);
    };
    Ok(persist_or_decline(state_dir, observed))
}

pub(super) fn shadow_enabled(config: &LoadedConfig) -> Result<bool, CliFailure> {
    Ok(cfg!(unix) && trusted_mode(config)? == PrepushMode::ShadowCompare)
}

fn persist_or_decline(state_dir: &Path, receipt: ProspectiveReceipt) -> Option<ProspectivePush> {
    match persist_prospective(state_dir, receipt) {
        Ok(push) => Some(push),
        Err(error) => {
            eprintln!(
                "warning: pre-push changed-surface receipt unavailable; continuing with full validation: {}",
                error.message()
            );
            None
        }
    }
}

#[allow(clippy::too_many_lines)]
fn observe_prospective(
    config: &LoadedConfig,
    cwd: &Path,
    repository: &str,
    base_ref: &str,
    head_ref: &str,
    targets: &[ResolvedTarget],
) -> Option<ProspectiveReceipt> {
    if repository.split('/').count() != 2
        || base_ref.trim().is_empty()
        || head_ref.trim().is_empty()
    {
        return None;
    }
    let client = GhClient::from_loaded_config(config)
        .ok()?
        .with_repo_override(repository)
        .ok()?;
    let endpoint = format!(
        "repos/{repository}/branches/{}",
        percent_encode_component(base_ref)
    );
    let branch: BranchMetadata = gh_api_json(&client, cwd, &endpoint).ok()?;
    if !branch.protected || !valid_sha(&branch.commit.sha) {
        return None;
    }
    let head_sha = git(cwd, &["rev-parse", "HEAD"])?;
    let tree_sha = git(cwd, &["rev-parse", "HEAD^{tree}"])?;
    if !valid_sha(&head_sha)
        || !valid_sha(&tree_sha)
        || !git(cwd, &["status", "--porcelain", "--untracked-files=normal"])?.is_empty()
    {
        return None;
    }
    let merge_base = git(cwd, &["merge-base", &branch.commit.sha, &head_sha])?;
    if merge_base != branch.commit.sha {
        return None;
    }
    let changed_paths = git_nul_paths(
        cwd,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            &format!("{}..{head_sha}", branch.commit.sha),
        ],
    )?;
    let base_tracked_paths = git_nul_paths(
        cwd,
        &["ls-tree", "-r", "--name-only", "-z", &branch.commit.sha],
    )?;
    let protected_config = git(
        cwd,
        &[
            "show",
            &format!("{}:.shipyard/config.toml", branch.commit.sha),
        ],
    )?;
    let target = unique_policy_target(&protected_config, targets)?;
    let policy = policy_from_toml(&protected_config, &target);
    let authenticated_policy = policy.as_ref().ok()?;
    let (hook_path, hook_sha256) =
        observe_hook_implementation(cwd, &branch.commit.sha, authenticated_policy)?;
    let input = ExactHeadInput {
        repository: repository.to_owned(),
        pull_request: PROSPECTIVE_PR_SENTINEL,
        target: target.clone(),
        observed_at: Utc::now(),
        base_ref: base_ref.to_owned(),
        pr_base_sha: branch.commit.sha.clone(),
        protected_ref_sha: branch.commit.sha.clone(),
        protected_ref_status: ProtectedRefStatus::Protected,
        pr_head_sha: head_sha.clone(),
        remote_tree_sha: tree_sha.clone(),
        local_head_sha: head_sha.clone(),
        local_tree_sha: tree_sha.clone(),
        local_merge_base_sha: merge_base.clone(),
        remote_merge_base_sha: merge_base.clone(),
        merge_base_is_ancestor: true,
        checkout_clean: true,
        remote_changed_paths: changed_paths.clone(),
        remote_changed_paths_status: ObservationStatus::Complete,
        local_changed_paths: changed_paths,
        local_changed_paths_status: ObservationStatus::Complete,
        base_tracked_paths,
        base_tracked_paths_status: ObservationStatus::Complete,
        secondary_proofs: Vec::new(),
    };
    let selection = plan_selection(&input, policy.clone()).ok()?;
    if !selection_is_transportable(&selection) {
        return None;
    }
    let raw_policy_digest = raw_policy_digest(&protected_config, &target)?;
    let selected_policy_digest = selection
        .policy_digest
        .clone()
        .unwrap_or_else(|| raw_policy_digest.clone());
    let coverage_contract_digest = policy
        .as_ref()
        .map_or_else(|_| raw_policy_digest.clone(), policy_digest);
    let inventory_digest = policy
        .as_ref()
        .map_or_else(|_| sha256(&[]), test_inventory_digest);
    let selected_tests_digest = digest_nul(&selection.selected_tests);
    let planner_digest = canonical_selection_digest(&selection)?;
    Some(ProspectiveReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repository: repository.to_owned(),
        target,
        protected_base_ref: base_ref.to_owned(),
        protected_base_sha: branch.commit.sha,
        head_ref: format!("refs/heads/{head_ref}"),
        head_sha,
        tree_sha,
        merge_base_sha: merge_base,
        changed_paths_digest: selection.changed_paths_digest.clone(),
        policy_digest: selected_policy_digest,
        planner_digest,
        coverage_contract_digest,
        inventory_digest,
        selected_tests_digest,
        hook_path,
        hook_sha256,
        transaction_nonce: String::new(),
        result_dir: PathBuf::new(),
        selection,
    })
}

fn selection_is_transportable(selection: &SelectionReceipt) -> bool {
    selection.planned_suite == PlannedSuite::Bounded
}

fn persist_prospective(
    state_dir: &Path,
    mut receipt: ProspectiveReceipt,
) -> Result<ProspectivePush, CliFailure> {
    let transactions = state_dir
        .join("changed-surface-prepush")
        .join("transactions");
    fs::create_dir_all(&transactions)
        .map_err(|error| CliFailure::new(1, format!("create pre-push state: {error}")))?;
    if let Err(error) = reap_abandoned_transactions(&transactions, ABANDONED_TRANSACTION_RETENTION)
    {
        eprintln!("warning: could not reap abandoned pre-push transactions: {error}");
    }
    let temporary = tempfile::Builder::new()
        .prefix("transaction-")
        .tempdir_in(&transactions)
        .map_err(|error| CliFailure::new(1, format!("create pre-push transaction: {error}")))?;
    let transaction_dir = temporary.keep();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&transaction_dir, fs::Permissions::from_mode(0o700))
            .map_err(|error| CliFailure::new(1, format!("secure pre-push transaction: {error}")))?;
    }
    transaction_dir
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.strip_prefix("transaction-"))
        .filter(|nonce| !nonce.is_empty())
        .ok_or_else(|| CliFailure::new(1, "pre-push transaction nonce is unavailable"))?
        .clone_into(&mut receipt.transaction_nonce);
    receipt.result_dir = transaction_dir.join("result");
    fs::create_dir(&receipt.result_dir).map_err(|error| {
        CliFailure::new(1, format!("create pre-push result directory: {error}"))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&receipt.result_dir, fs::Permissions::from_mode(0o700)).map_err(
            |error| CliFailure::new(1, format!("secure pre-push result directory: {error}")),
        )?;
    }
    let payload = serde_json::to_vec(&receipt)
        .map_err(|error| CliFailure::new(1, format!("serialize pre-push receipt: {error}")))?;
    let receipt_digest = sha256(&payload);
    let receipt_path = transaction_dir.join("prospective-receipt.json");
    create_immutable(&receipt_path, &payload)?;
    Ok(ProspectivePush {
        receipt,
        receipt_path,
        receipt_digest,
        supervised_push_succeeded: false,
    })
}

/// Verify the hook output and independently re-observed PR, then persist only
/// the narrow dedupe disposition consumed by future queue integration.
pub(super) fn verify_after_push(
    prospective: &ProspectivePush,
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    repository: &str,
    pull_request: u64,
    head_branch: &str,
) -> Result<(), CliFailure> {
    if !prospective.supervised_push_succeeded {
        return Err(CliFailure::new(
            1,
            "pre-push receipt lacks a successful supervised push",
        ));
    }
    let verification = verify_after_push_inner(
        prospective,
        config,
        cwd,
        state_dir,
        repository,
        pull_request,
        head_branch,
    );
    if let Err(error) = remove_transaction(prospective) {
        eprintln!("warning: could not remove completed pre-push transaction: {error}");
    }
    verification
}

#[allow(clippy::too_many_arguments)]
fn verify_after_push_inner(
    prospective: &ProspectivePush,
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    repository: &str,
    pull_request: u64,
    head_branch: &str,
) -> Result<(), CliFailure> {
    if prospective.receipt.head_ref != format!("refs/heads/{head_branch}") {
        return Err(CliFailure::new(1, "post-push head-ref identity drift"));
    }
    verify_hook_implementation(cwd, prospective)?;
    let observation = observe_changed_surface_plan(
        &ChangedSurfacePlanArgs {
            target: prospective.receipt.target.clone(),
            pr: pull_request,
            repo: Some(repository.to_owned()),
        },
        config,
        cwd,
        state_dir,
    )?;
    verify_postpush_identity(
        &prospective.receipt,
        &observation.receipt,
        observation.policy.as_ref().ok(),
    )?;
    let result = load_hook_result(&prospective.receipt.result_dir)?;
    verify_hook_result(prospective, &result)?;
    persist_verified_snapshot(prospective, state_dir, repository, pull_request)
}

fn remove_transaction(prospective: &ProspectivePush) -> std::io::Result<()> {
    let transaction_dir = prospective
        .receipt_path
        .parent()
        .ok_or_else(|| std::io::Error::other("receipt has no transaction directory"))?;
    fs::remove_dir_all(transaction_dir)
}

fn reap_abandoned_transactions(transactions: &Path, retention: Duration) -> std::io::Result<()> {
    let now = SystemTime::now();
    let mut reaped = 0;
    for entry in fs::read_dir(transactions)? {
        if reaped == MAX_ABANDONED_REAPS_PER_PUSH {
            break;
        }
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("transaction-") || name.len() <= "transaction-".len() {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.file_type().is_dir() {
            continue;
        }
        let elapsed = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok());
        if elapsed.is_some_and(|elapsed| elapsed >= retention) {
            fs::remove_dir_all(entry.path())?;
            reaped += 1;
        }
    }
    Ok(())
}

fn persist_verified_snapshot(
    prospective: &ProspectivePush,
    state_dir: &Path,
    repository: &str,
    pull_request: u64,
) -> Result<(), CliFailure> {
    if !prospective.supervised_push_succeeded {
        return Err(CliFailure::new(
            1,
            "pre-push snapshot requires parent-observed push success",
        ));
    }
    let snapshot = VerifiedSnapshot {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        disposition: "full_only_due_exact_prepush_shadow",
        repository,
        pull_request,
        target: &prospective.receipt.target,
        head_sha: &prospective.receipt.head_sha,
        tree_sha: &prospective.receipt.tree_sha,
        prospective_receipt_sha256: &prospective.receipt_digest,
        selected_tests_digest: &prospective.receipt.selected_tests_digest,
        transaction_nonce: &prospective.receipt.transaction_nonce,
    };
    let path = snapshot_path(
        state_dir,
        repository,
        pull_request,
        &prospective.receipt.head_sha,
        &prospective.receipt.target,
        &prospective.receipt.transaction_nonce,
    );
    let payload = serde_json::to_vec(&snapshot)
        .map_err(|error| CliFailure::new(1, format!("serialize pre-push snapshot: {error}")))?;
    create_immutable_idempotent(&path, &payload)
}

fn load_hook_result(result_dir: &Path) -> Result<HookResult, CliFailure> {
    let result_path = result_dir.join("hook-result.json");
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        // FILE_FLAG_OPEN_REPARSE_POINT opens the link itself rather than its
        // target; the descriptor metadata check below then rejects it.
        options.custom_flags(0x0020_0000);
    }
    let file = options
        .open(&result_path)
        .map_err(|error| CliFailure::new(1, format!("open pre-push hook result: {error}")))?;
    let metadata = file
        .metadata()
        .map_err(|error| CliFailure::new(1, format!("inspect pre-push hook result: {error}")))?;
    if !metadata.is_file() || metadata.len() > MAX_HOOK_RESULT_BYTES {
        return Err(CliFailure::new(
            1,
            "pre-push hook result is missing or oversized",
        ));
    }
    let mut payload = Vec::new();
    file.take(MAX_HOOK_RESULT_BYTES + 1)
        .read_to_end(&mut payload)
        .map_err(|error| CliFailure::new(1, format!("read pre-push hook result: {error}")))?;
    if u64::try_from(payload.len()).unwrap_or(u64::MAX) > MAX_HOOK_RESULT_BYTES {
        return Err(CliFailure::new(1, "pre-push hook result grew oversized"));
    }
    serde_json::from_slice(&payload)
        .map_err(|error| CliFailure::new(1, format!("parse pre-push hook result: {error}")))
}

fn verify_postpush_identity(
    prospective: &ProspectiveReceipt,
    observed: &SelectionReceipt,
    observed_policy: Option<&ChangedSurfacePolicy>,
) -> Result<(), CliFailure> {
    let observed_planner_digest = canonical_selection_digest(observed)
        .ok_or_else(|| CliFailure::new(1, "canonicalize post-push selector plan"))?;
    let observed_coverage_digest = observed_policy.map_or_else(|| sha256(&[]), policy_digest);
    let observed_inventory_digest =
        observed_policy.map_or_else(|| sha256(&[]), test_inventory_digest);
    let mismatches = [
        (
            "repository",
            prospective.repository.as_str(),
            observed.repository.as_str(),
        ),
        (
            "target",
            prospective.target.as_str(),
            observed.target.as_str(),
        ),
        (
            "base ref",
            prospective.protected_base_ref.as_str(),
            observed.protected_ref.as_str(),
        ),
        (
            "base SHA",
            prospective.protected_base_sha.as_str(),
            observed.pr_base_sha.as_str(),
        ),
        (
            "protected SHA",
            prospective.protected_base_sha.as_str(),
            observed.protected_ref_sha.as_str(),
        ),
        (
            "merge base",
            prospective.merge_base_sha.as_str(),
            observed.merge_base_sha.as_str(),
        ),
        (
            "head SHA",
            prospective.head_sha.as_str(),
            observed.head_sha.as_str(),
        ),
        (
            "tree SHA",
            prospective.tree_sha.as_str(),
            observed.tree_sha.as_str(),
        ),
        (
            "path digest",
            prospective.changed_paths_digest.as_str(),
            observed.changed_paths_digest.as_str(),
        ),
        (
            "policy digest",
            prospective.policy_digest.as_str(),
            observed.policy_digest.as_deref().unwrap_or(""),
        ),
        (
            "planner digest",
            prospective.planner_digest.as_str(),
            observed_planner_digest.as_str(),
        ),
        (
            "coverage contract",
            prospective.coverage_contract_digest.as_str(),
            observed_coverage_digest.as_str(),
        ),
        (
            "test inventory",
            prospective.inventory_digest.as_str(),
            observed_inventory_digest.as_str(),
        ),
        (
            "selected tests",
            prospective.selected_tests_digest.as_str(),
            digest_nul(&observed.selected_tests).as_str(),
        ),
    ]
    .into_iter()
    .filter_map(|(name, expected, actual)| (expected != actual).then_some(name))
    .collect::<Vec<_>>();
    if prospective.selection.changed_paths != observed.changed_paths {
        return Err(CliFailure::new(1, "post-push changed-path bytes drifted"));
    }
    if prospective.selection.selected_tests != observed.selected_tests {
        return Err(CliFailure::new(1, "post-push selected-test bytes drifted"));
    }
    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!(
                "post-push selector identity drift: {}",
                mismatches.join(", ")
            ),
        ))
    }
}

fn verify_hook_result(
    prospective: &ProspectivePush,
    result: &HookResult,
) -> Result<(), CliFailure> {
    if result.schema_version != HOOK_RESULT_SCHEMA_VERSION
        || result.transaction_nonce != prospective.receipt.transaction_nonce
        || result.prospective_receipt_sha256 != prospective.receipt_digest
        || result.update_count != 1
        || result.update_ref != prospective.receipt.head_ref
        || result.head_sha != prospective.receipt.head_sha
        || result.tree_sha != prospective.receipt.tree_sha
        || result.selected_tests_digest != prospective.receipt.selected_tests_digest
        || result.hook_sha256 != prospective.receipt.hook_sha256
        || prospective.receipt.selection.planned_suite != PlannedSuite::Bounded
    {
        return Err(CliFailure::new(1, "pre-push hook result identity mismatch"));
    }
    Ok(())
}

fn observe_hook_implementation(
    cwd: &Path,
    protected_base_sha: &str,
    policy: &ChangedSurfacePolicy,
) -> Option<(String, String)> {
    let hooks_dir = git(cwd, &["config", "--path", "core.hooksPath"])?;
    let hooks_path = Path::new(&hooks_dir);
    if hooks_path.is_absolute()
        || hooks_path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        })
    {
        return None;
    }
    let hook_path = hooks_path.join("pre-push");
    let hook = hook_path.to_str()?.trim_start_matches("./").to_owned();
    if hook.is_empty() || !policy_covers_hook(policy, &hook) {
        return None;
    }
    let metadata = fs::symlink_metadata(cwd.join(&hook)).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    let protected_mode = git(cwd, &["ls-tree", protected_base_sha, "--", &hook])?;
    if !protected_mode.starts_with("100755 blob ") {
        return None;
    }
    let current = fs::read(cwd.join(&hook)).ok()?;
    let protected = git_bytes(cwd, &["show", &format!("{protected_base_sha}:{hook}")])?;
    (current == protected).then(|| (hook, sha256(&protected)))
}

fn verify_hook_implementation(cwd: &Path, prospective: &ProspectivePush) -> Result<(), CliFailure> {
    let protected_config = git(
        cwd,
        &[
            "show",
            &format!(
                "{}:.shipyard/config.toml",
                prospective.receipt.protected_base_sha
            ),
        ],
    )
    .ok_or_else(|| CliFailure::new(1, "re-read protected hook policy"))?;
    let policy = policy_from_toml(&protected_config, &prospective.receipt.target)
        .map_err(|error| CliFailure::new(1, format!("re-read protected hook policy: {error}")))?;
    let (path, digest) =
        observe_hook_implementation(cwd, &prospective.receipt.protected_base_sha, &policy)
            .ok_or_else(|| {
                CliFailure::new(1, "pre-push hook implementation is not authenticated")
            })?;
    if path != prospective.receipt.hook_path || digest != prospective.receipt.hook_sha256 {
        return Err(CliFailure::new(1, "pre-push hook implementation drifted"));
    }
    Ok(())
}

fn policy_covers_hook(policy: &ChangedSurfacePolicy, hook: &str) -> bool {
    policy
        .policy_paths
        .iter()
        .chain(&policy.test_topology_paths)
        .filter_map(|pattern| glob::Pattern::new(pattern).ok())
        .any(|pattern| pattern.matches(hook))
}

fn trusted_mode(config: &LoadedConfig) -> Result<PrepushMode, CliFailure> {
    let trusted = LoadedConfig::load_machine_global_from_dir(config.global_dir.clone())
        .map_err(|error| CliFailure::new(1, format!("load trusted pre-push policy: {error}")))?;
    match trusted.get_str(MODE_KEY) {
        None | Some("off") => Ok(PrepushMode::Off),
        Some("shadow_compare") => Ok(PrepushMode::ShadowCompare),
        Some("authoritative") => Ok(PrepushMode::Authoritative),
        Some(value) => Err(CliFailure::new(
            2,
            format!("invalid trusted {MODE_KEY} value '{value}'"),
        )),
    }
}

fn unique_policy_target(contents: &str, targets: &[ResolvedTarget]) -> Option<String> {
    let table = contents.parse::<toml::Table>().ok()?;
    let configured = table.get("targets")?.as_table()?;
    let resolved = targets
        .iter()
        .map(|target| target.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut candidates = configured
        .iter()
        .filter(|(name, value)| {
            resolved.contains(name.as_str())
                && value
                    .as_table()
                    .and_then(|target| target.get("changed_surface_selection"))
                    .is_some()
        })
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    candidates.sort();
    (candidates.len() == 1).then(|| candidates.remove(0))
}

fn raw_policy_digest(contents: &str, target: &str) -> Option<String> {
    let table = contents.parse::<toml::Table>().ok()?;
    let value = table
        .get("targets")?
        .get(target)?
        .get("changed_surface_selection")?;
    serde_json::to_vec(value).ok().map(|bytes| sha256(&bytes))
}

fn test_inventory_digest(policy: &ChangedSurfacePolicy) -> String {
    let mut inventory = policy.baseline_tests.clone();
    for family in &policy.families {
        inventory.extend(family.tests.iter().cloned());
        inventory.extend(family.extended_tests.iter().cloned());
    }
    digest_nul(&inventory)
}

fn canonical_selection_digest(receipt: &SelectionReceipt) -> Option<String> {
    let mut value = serde_json::to_value(receipt).ok()?;
    value["pull_request"] = serde_json::Value::from(PROSPECTIVE_PR_SENTINEL);
    value["elapsed_ms"] = serde_json::Value::from(0);
    serde_json::to_vec(&value).ok().map(|bytes| sha256(&bytes))
}

fn gh_api_json<T: DeserializeOwned>(
    client: &GhClient,
    cwd: &Path,
    endpoint: &str,
) -> Result<T, CliFailure> {
    let output = client
        .prepare_command(
            cwd,
            None,
            GhSupervision::Unsupervised,
            GhAuthPolicy::Default,
        )
        .map_err(|error| CliFailure::new(1, format!("prepare protected-base query: {error}")))?
        .args(["api", "--method", "GET", endpoint])
        .output()
        .map_err(|error| CliFailure::new(1, format!("start protected-base query: {error}")))?;
    if !output.status.success() {
        return Err(CliFailure::new(1, "protected-base query failed"));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| CliFailure::new(1, format!("parse protected-base query: {error}")))
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_nul_paths(cwd: &Path, args: &[&str]) -> Option<Vec<String>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8(path.to_vec()).ok())
        .collect()
}

fn git_bytes(cwd: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn create_immutable(path: &Path, payload: &[u8]) -> Result<(), CliFailure> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| CliFailure::new(1, format!("create immutable receipt: {error}")))?;
    file.write_all(payload)
        .and_then(|()| file.sync_all())
        .map_err(|error| CliFailure::new(1, format!("write immutable receipt: {error}")))?;
    #[cfg(unix)]
    sync_directory(path.parent().expect("receipt has parent"))?;
    Ok(())
}

fn create_immutable_idempotent(path: &Path, payload: &[u8]) -> Result<(), CliFailure> {
    let parent = path
        .parent()
        .ok_or_else(|| CliFailure::new(1, "snapshot path has no parent"))?;
    fs::create_dir_all(parent)
        .map_err(|error| CliFailure::new(1, format!("create snapshot directory: {error}")))?;
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => file
            .write_all(payload)
            .and_then(|()| file.sync_all())
            .map_err(|error| CliFailure::new(1, format!("write pre-push snapshot: {error}")))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(path)
                .map_err(|error| CliFailure::new(1, format!("read pre-push snapshot: {error}")))?;
            if existing != payload {
                return Err(CliFailure::new(1, "immutable pre-push snapshot differs"));
            }
        }
        Err(error) => {
            return Err(CliFailure::new(
                1,
                format!("create pre-push snapshot: {error}"),
            ));
        }
    }
    #[cfg(unix)]
    sync_directory(parent)?;
    Ok(())
}

fn snapshot_path(
    state_dir: &Path,
    repository: &str,
    pull_request: u64,
    head: &str,
    target: &str,
    transaction_nonce: &str,
) -> PathBuf {
    state_dir
        .join("changed-surface-prepush")
        .join("verified")
        .join(path_component(repository))
        .join(pull_request.to_string())
        .join(path_component(head))
        .join(path_component(target))
        .join(format!("{}.json", path_component(transaction_nonce)))
}

fn path_component(value: &str) -> String {
    let prefix: String = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || "._-".contains(*character))
        .take(32)
        .collect();
    format!("{prefix}-{}", sha256(value.as_bytes()))
}

fn digest_nul(values: &[String]) -> String {
    let mut normalized = values.to_vec();
    normalized.sort();
    normalized.dedup();
    let mut hasher = Sha256::new();
    for value in normalized {
        hasher.update(value.as_bytes());
        hasher.update(b"\0");
    }
    hex::encode(hasher.finalize())
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn percent_encode_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn valid_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), CliFailure> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| CliFailure::new(1, format!("sync receipt directory: {error}")))
}

#[cfg(test)]
#[path = "prepush_changed_surface/tests.rs"]
mod tests;
