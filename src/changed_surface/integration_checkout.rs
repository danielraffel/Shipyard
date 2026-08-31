//! Content-addressed, restart-safe checkouts for stale-base shadow execution.
//!
//! These checkouts are never merge authority. They only provide an isolated
//! filesystem whose `HEAD` and tree exactly match the synthetic integration
//! identity recorded by a stale-base shadow receipt.

use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use super::trial::{
    ReceiptFile, TrialIdentity, TrialState, evaluate_stale_base_execution,
    validate_stale_activation_for_cleanup,
};
use super::{StaleBaseShadowReceipt, stale_base_context_digest};

const CLEANUP_RECEIPT_NAME: &str = "stale-cleanup-shadow_compare.json";
const CLEANUP_PENDING_NAME: &str = ".stale-cleanup-shadow_compare.pending";
const ACTIVATION_RECEIPT_NAME: &str = "stale-activation-shadow_compare.json";
const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CheckoutMarker {
    schema_version: u32,
    source_git_common_dir: String,
    repository: String,
    pull_request: u64,
    target: String,
    stale_head_sha: String,
    live_base_sha: String,
    integration_commit_sha: String,
    integration_tree_sha: String,
    context_digest: String,
}

/// Exact checkout identity retained for execution and later cleanup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IntegrationCheckout {
    pub(crate) path: PathBuf,
    source_repo: PathBuf,
    evidence_dir: PathBuf,
    lock_path: PathBuf,
    marker: CheckoutMarker,
    receipt: StaleBaseShadowReceipt,
}

/// Durable exact identity needed to restore checkout custody in the daemon.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationCheckoutSnapshot {
    source_repo: PathBuf,
    checkout_parent: PathBuf,
    receipt: StaleBaseShadowReceipt,
}

impl IntegrationCheckout {
    pub(crate) fn snapshot(&self) -> IntegrationCheckoutSnapshot {
        IntegrationCheckoutSnapshot {
            source_repo: self.source_repo.clone(),
            checkout_parent: self
                .path
                .parent()
                .expect("materialized checkout has a parent")
                .to_path_buf(),
            receipt: self.receipt.clone(),
        }
    }
}

impl IntegrationCheckoutSnapshot {
    pub(crate) fn restore(&self) -> Result<IntegrationCheckout, String> {
        checkout_from_receipt(&self.source_repo, &self.checkout_parent, &self.receipt)
    }
}

/// Plan exact checkout custody without creating a worktree. This is the queue
/// boundary: the daemon materializes only after it owns the execution fence.
pub(crate) fn plan(
    source_repo: &Path,
    checkout_parent: &Path,
    receipt: &StaleBaseShadowReceipt,
) -> Result<IntegrationCheckout, String> {
    checkout_from_receipt(source_repo, checkout_parent, receipt)
}

#[derive(Debug, Serialize)]
struct CleanupReceipt<'a> {
    schema_version: u32,
    context_digest: &'a str,
    integration_commit_sha: &'a str,
    integration_tree_sha: &'a str,
    disposition: &'static str,
}

/// Materialize or reconcile one exact content-addressed linked worktree.
///
/// An interrupted prior materialization is reusable only when its Git
/// identity and marker match every requested field. Ambiguity is preserved on
/// disk and returned as an error; callers must keep ordinary full validation.
#[cfg(test)]
fn materialize(
    source_repo: &Path,
    checkout_parent: &Path,
    receipt: &StaleBaseShadowReceipt,
) -> Result<IntegrationCheckout, String> {
    ensure_real_directory(checkout_parent)?;
    let checkout = checkout_from_receipt(source_repo, checkout_parent, receipt)?;
    let _materialize_guard = acquire_fence(&checkout.lock_path)?;
    ensure_materialized(&checkout)?;
    Ok(checkout)
}

fn ensure_materialized(checkout: &IntegrationCheckout) -> Result<(), String> {
    ensure_real_directory(
        checkout
            .path
            .parent()
            .ok_or_else(|| "integration checkout has no parent".to_owned())?,
    )?;
    match fs::symlink_metadata(&checkout.path) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {}
        Ok(_) => return Err("integration checkout path is not a real directory".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let checkout_path = git_cli_path(&checkout.path);
            let status = Command::new("git")
                .args(["worktree", "add", "--detach"])
                .arg(&checkout_path)
                .arg(&checkout.marker.integration_commit_sha)
                .current_dir(&checkout.source_repo)
                .status()
                .map_err(|error| format!("create integration checkout: {error}"))?;
            if !status.success() {
                return Err("create integration checkout: git worktree add failed".to_owned());
            }
        }
        Err(error) => return Err(format!("inspect integration checkout: {error}")),
    }

    verify_checkout(&checkout.path, &checkout.marker)?;
    initialize_submodules(&checkout.path)?;
    verify_checkout(&checkout.path, &checkout.marker)?;
    persist_or_verify_marker(&checkout.path, &checkout.marker)?;
    Ok(())
}

fn checkout_from_receipt(
    source_repo: &Path,
    checkout_parent: &Path,
    receipt: &StaleBaseShadowReceipt,
) -> Result<IntegrationCheckout, String> {
    let integration_commit = receipt
        .integration_commit_sha
        .as_deref()
        .ok_or_else(|| "stale shadow receipt has no integration commit".to_owned())?;
    let integration_tree = receipt
        .integration_tree_sha
        .as_deref()
        .ok_or_else(|| "stale shadow receipt has no integration tree".to_owned())?;
    let context_digest = stale_base_context_digest(receipt);
    let source_repo = source_repo
        .canonicalize()
        .map_err(|error| format!("canonicalize integration source: {error}"))?;
    let source_common_dir = git_path(&source_repo, &["rev-parse", "--git-common-dir"])?;
    let source_common_dir = canonical_git_path(&source_repo, &source_common_dir)?;
    let parent_name = checkout_parent
        .file_name()
        .ok_or_else(|| "integration checkout root has no final component".to_owned())?;
    let checkout_parent = checkout_parent
        .parent()
        .ok_or_else(|| "integration checkout root has no evidence parent".to_owned())?
        .canonicalize()
        .map_err(|error| format!("canonicalize integration evidence root: {error}"))?
        .join(parent_name);
    let path = checkout_parent.join(format!("shadow-{context_digest}"));
    Ok(IntegrationCheckout {
        path,
        source_repo,
        evidence_dir: checkout_parent
            .parent()
            .ok_or_else(|| "integration checkout root has no evidence parent".to_owned())?
            .to_path_buf(),
        lock_path: checkout_parent.join(format!("shadow-{context_digest}.lock")),
        marker: CheckoutMarker {
            schema_version: 1,
            source_git_common_dir: source_common_dir.to_string_lossy().into_owned(),
            repository: receipt.repository.clone(),
            pull_request: receipt.pull_request,
            target: receipt.target.clone(),
            stale_head_sha: receipt.head_sha.clone(),
            live_base_sha: receipt.live_protected_base_sha.clone(),
            integration_commit_sha: integration_commit.to_owned(),
            integration_tree_sha: integration_tree.to_owned(),
            context_digest,
        },
        receipt: receipt.clone(),
    })
}

/// Hold the exact checkout's execution fence and prove its content before a
/// command can start. The returned file must remain alive through execution,
/// post-execution verification, and cleanup.
pub(crate) fn prepare_for_execution(checkout: &IntegrationCheckout) -> Result<fs::File, String> {
    ensure_real_directory(
        checkout
            .path
            .parent()
            .ok_or_else(|| "integration checkout has no parent".to_owned())?,
    )?;
    let lock = acquire_fence(&checkout.lock_path)?;
    if cleanup_state_exists(checkout)? {
        return Err(
            "integration checkout cleanup is pending or complete; refusing replay".to_owned(),
        );
    }
    ensure_materialized(checkout)?;
    verify_checkout(&checkout.path, &checkout.marker)?;
    verify_marker(&checkout.path, &checkout.marker)?;
    restore_pristine_checkout(&checkout.path)?;
    verify_pristine(&checkout.path)?;
    Ok(lock)
}

fn cleanup_state_exists(checkout: &IntegrationCheckout) -> Result<bool, String> {
    for name in [CLEANUP_PENDING_NAME, CLEANUP_RECEIPT_NAME] {
        match fs::symlink_metadata(checkout.evidence_dir.join(name)) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "inspect integration checkout cleanup state {name}: {error}"
                ));
            }
        }
    }
    Ok(false)
}

fn acquire_fence(path: &Path) -> Result<fs::File, String> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| format!("open integration execution fence: {error}"))?;
    lock.try_lock_exclusive()
        .map_err(|error| format!("integration checkout is already owned: {error}"))?;
    Ok(lock)
}

/// Recheck exact content while the caller still holds the execution fence.
pub(crate) fn verify_after_execution(checkout: &IntegrationCheckout) -> Result<(), String> {
    verify_checkout(&checkout.path, &checkout.marker)?;
    verify_marker(&checkout.path, &checkout.marker)?;
    verify_tracked_and_unignored(&checkout.path)
}

/// Remove only the exact linked worktree whose immutable marker and Git
/// identity still match. A mismatch refuses without deleting anything.
pub(crate) fn cleanup(checkout: &IntegrationCheckout) -> Result<(), String> {
    if checkout.path.exists() {
        verify_checkout(&checkout.path, &checkout.marker)?;
        verify_marker(&checkout.path, &checkout.marker)?;
        verify_tracked_and_unignored(&checkout.path)?;
    } else if marker_path(&checkout.path).exists() {
        verify_marker(&checkout.path, &checkout.marker)?;
    } else if !checkout.evidence_dir.join(CLEANUP_PENDING_NAME).exists()
        && !checkout.evidence_dir.join(CLEANUP_RECEIPT_NAME).exists()
    {
        return Err("absent integration checkout has no resumable cleanup evidence".to_owned());
    }
    persist_cleanup_pending(checkout)?;
    if checkout.path.exists() {
        let checkout_path = git_cli_path(&checkout.path);
        let status = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&checkout_path)
            .current_dir(&checkout.source_repo)
            .status()
            .map_err(|error| format!("remove integration checkout: {error}"))?;
        if !status.success() || checkout.path.exists() {
            return Err("remove integration checkout: exact worktree remains".to_owned());
        }
    }
    let marker_path = marker_path(&checkout.path);
    if marker_path.exists() {
        fs::remove_file(&marker_path)
            .map_err(|error| format!("remove completed integration marker: {error}"))?;
        sync_directory(
            marker_path
                .parent()
                .ok_or_else(|| "integration marker has no parent directory".to_owned())?,
        )?;
    }
    publish_cleanup_receipt(checkout)?;
    Ok(())
}

/// Finish a crash-interrupted cleanup transaction without recreating or
/// re-running an already removed integration checkout.
pub(crate) fn reconcile_pending_cleanup(
    source_repo: &Path,
    checkout_parent: &Path,
    receipt: &StaleBaseShadowReceipt,
) -> Result<bool, String> {
    let evidence_dir = checkout_parent
        .parent()
        .ok_or_else(|| "integration checkout root has no evidence parent".to_owned())?;
    let pending_exists = evidence_dir.join(CLEANUP_PENDING_NAME).exists();
    let activation_path = evidence_dir.join(ACTIVATION_RECEIPT_NAME);
    if !pending_exists && !activation_path.exists() {
        return Ok(false);
    }
    if !pending_exists {
        let (stale_name, stale_bytes) =
            read_single_prefixed_receipt(evidence_dir, "stale-base-shadow-")?;
        let persisted_stale: StaleBaseShadowReceipt = serde_json::from_slice(&stale_bytes)
            .map_err(|error| format!("decode persisted stale integration receipt: {error}"))?;
        if &persisted_stale != receipt {
            return Err(format!(
                "persisted stale integration receipt {stale_name} disagrees with exact request"
            ));
        }
        let activation = read_regular_receipt(&activation_path)?;
        validate_stale_activation_for_cleanup(receipt, &stale_bytes, &activation)?;
    }
    let checkout = checkout_from_receipt(source_repo, checkout_parent, receipt)?;
    let _guard = acquire_fence(&checkout.lock_path)?;
    cleanup(&checkout)?;
    Ok(true)
}

/// Prove that the repository adapter emitted one exact-bound selected/full
/// comparison before its exit status may satisfy the ordinary target gate.
pub(crate) fn verify_completed_execution(checkout: &IntegrationCheckout) -> Result<(), String> {
    if checkout.path.exists() || marker_path(&checkout.path).exists() {
        return Err("integration checkout cleanup is not durably complete".to_owned());
    }
    let stale = read_single_prefixed_receipt(&checkout.evidence_dir, "stale-base-shadow-")?;
    let activation = read_regular_receipt(&checkout.evidence_dir.join(ACTIVATION_RECEIPT_NAME))?;
    let cleanup = read_regular_receipt(&checkout.evidence_dir.join(CLEANUP_RECEIPT_NAME))?;
    let result = read_single_prefixed_receipt(&checkout.evidence_dir, "result-")?;
    let status = evaluate_stale_base_execution(
        &TrialIdentity {
            repository: checkout.marker.repository.clone(),
            pull_request: checkout.marker.pull_request,
            target: checkout.marker.target.clone(),
            head_sha: checkout.marker.stale_head_sha.clone(),
        },
        ReceiptFile {
            name: &stale.0,
            bytes: &stale.1,
        },
        ReceiptFile {
            name: ACTIVATION_RECEIPT_NAME,
            bytes: &activation,
        },
        ReceiptFile {
            name: CLEANUP_RECEIPT_NAME,
            bytes: &cleanup,
        },
        &[ReceiptFile {
            name: &result.0,
            bytes: &result.1,
        }],
    );
    if status.state == TrialState::Terminal
        && matches!(
            status.shadow_disposition,
            Some(
                super::StaleBaseShadowDisposition::Recomputed
                    | super::StaleBaseShadowDisposition::Reused
            )
        )
        && status.reason.ends_with("_selected_pass")
    {
        Ok(())
    } else {
        Err(format!(
            "stale integration comparison evidence refused: {}",
            status.reason
        ))
    }
}

fn read_single_prefixed_receipt(
    directory: &Path,
    prefix: &str,
) -> Result<(String, Vec<u8>), String> {
    let mut matches = Vec::new();
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("read integration evidence directory: {error}"))?
    {
        let entry = entry.map_err(|error| format!("read integration evidence entry: {error}"))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "integration evidence name is not UTF-8".to_owned())?;
        if name.starts_with(prefix) && name.as_bytes().ends_with(b".json") {
            matches.push((name, entry.path()));
        }
    }
    if matches.len() != 1 {
        return Err(format!(
            "expected one {prefix} integration receipt, observed {}",
            matches.len()
        ));
    }
    let (name, path) = matches.pop().expect("checked one receipt");
    Ok((name, read_regular_receipt(&path)?))
}

fn read_regular_receipt(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect integration receipt: {error}"))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_RECEIPT_BYTES
    {
        return Err("integration receipt is not a bounded regular file".to_owned());
    }
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
        options.custom_flags(0x0020_0000);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("open integration receipt: {error}"))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("inspect opened integration receipt: {error}"))?;
    if !opened.is_file() || opened.len() > MAX_RECEIPT_BYTES {
        return Err("opened integration receipt is not a bounded regular file".to_owned());
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read integration receipt: {error}"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_RECEIPT_BYTES {
        return Err("integration receipt exceeds size limit".to_owned());
    }
    Ok(bytes)
}

fn ensure_real_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("create integration checkout root: {error}"))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect integration checkout root: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("integration checkout root is not a real directory".to_owned());
    }
    Ok(())
}

fn verify_checkout(path: &Path, marker: &CheckoutMarker) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect integration checkout: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("integration checkout path is not a real directory".to_owned());
    }
    exact_git(path, &["rev-parse", "HEAD"], &marker.integration_commit_sha)?;
    exact_git(
        path,
        &["rev-parse", "HEAD^{tree}"],
        &marker.integration_tree_sha,
    )?;
    exact_git(path, &["rev-parse", "HEAD^1"], &marker.live_base_sha)?;
    exact_git(path, &["rev-parse", "HEAD^2"], &marker.stale_head_sha)?;
    let common_dir = git_path(path, &["rev-parse", "--git-common-dir"])?;
    let common_dir = canonical_git_path(path, &common_dir)?;
    if common_dir != Path::new(&marker.source_git_common_dir) {
        return Err("integration checkout belongs to a different repository".to_owned());
    }
    Ok(())
}

fn verify_tracked_and_unignored(path: &Path) -> Result<(), String> {
    let status = git_path(path, &["status", "--porcelain=v1", "--untracked-files=all"])?;
    if !status.is_empty() {
        return Err("integration checkout contains tracked or unignored content drift".to_owned());
    }
    let submodules = git_path(
        path,
        &[
            "submodule",
            "foreach",
            "--recursive",
            "--quiet",
            "git status --porcelain=v1 --untracked-files=all",
        ],
    )?;
    if !submodules.is_empty() {
        return Err("integration submodule contains tracked or untracked content drift".to_owned());
    }
    let identities = git_path(path, &["submodule", "status", "--recursive"])?;
    if identities
        .lines()
        .any(|line| !line.starts_with(' ') || line.len() < 42)
    {
        return Err("integration submodule commit identity is incomplete or drifted".to_owned());
    }
    Ok(())
}

fn verify_pristine(path: &Path) -> Result<(), String> {
    let status = git_path(
        path,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )?;
    if !status.is_empty() {
        return Err("integration checkout contains ignored or untracked residue".to_owned());
    }
    let submodules = git_path(
        path,
        &[
            "submodule",
            "foreach",
            "--recursive",
            "--quiet",
            "git status --porcelain=v1 --untracked-files=all --ignored=matching",
        ],
    )?;
    if !submodules.is_empty() {
        return Err("integration submodule contains ignored or untracked residue".to_owned());
    }
    Ok(())
}

fn restore_pristine_checkout(path: &Path) -> Result<(), String> {
    git_success(
        path,
        &["reset", "--hard", "HEAD"],
        "reset integration checkout",
    )?;
    git_success(path, &["clean", "-ffdx"], "clean integration checkout")?;
    git_success(
        path,
        &[
            "submodule",
            "foreach",
            "--recursive",
            "--quiet",
            "git reset --hard HEAD && git clean -ffdx",
        ],
        "clean integration submodules",
    )
}

fn git_success(cwd: &Path, args: &[&str], context: &str) -> Result<(), String> {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .map_err(|error| format!("{context}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{context}: git failed"))
    }
}

fn initialize_submodules(path: &Path) -> Result<(), String> {
    let status = Command::new("git")
        .args([
            "submodule",
            "update",
            "--init",
            "--recursive",
            "--jobs",
            "4",
        ])
        .current_dir(path)
        .status()
        .map_err(|error| format!("initialize integration submodules: {error}"))?;
    if !status.success() {
        return Err("initialize integration submodules: git failed".to_owned());
    }
    let output = git_path(path, &["submodule", "status", "--recursive"])?;
    if output
        .lines()
        .any(|line| matches!(line.as_bytes().first(), Some(b'-' | b'+' | b'U')))
    {
        return Err("integration submodule identity is incomplete or dirty".to_owned());
    }
    Ok(())
}

fn persist_or_verify_marker(path: &Path, marker: &CheckoutMarker) -> Result<(), String> {
    let marker_path = marker_path(path);
    let mut payload = serde_json::to_vec_pretty(marker)
        .map_err(|error| format!("serialize integration marker: {error}"))?;
    payload.push(b'\n');
    match atomicwrites::AtomicFile::new(&marker_path, atomicwrites::DisallowOverwrite)
        .write(|file| file.write_all(&payload))
    {
        Ok(()) => {}
        Err(atomicwrites::Error::Internal(error))
            if error.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            if fs::read(&marker_path)
                .map_err(|error| format!("read integration marker: {error}"))?
                != payload
            {
                return Err("integration checkout marker disagrees with exact request".to_owned());
            }
        }
        Err(error) => return Err(format!("create integration marker: {error}")),
    }
    Ok(())
}

fn verify_marker(path: &Path, marker: &CheckoutMarker) -> Result<(), String> {
    let marker_path = marker_path(path);
    let metadata = fs::symlink_metadata(&marker_path)
        .map_err(|error| format!("inspect integration marker: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("integration checkout marker is not a regular file".to_owned());
    }
    let decoded: CheckoutMarker = serde_json::from_slice(
        &fs::read(&marker_path).map_err(|error| format!("read integration marker: {error}"))?,
    )
    .map_err(|error| format!("decode integration marker: {error}"))?;
    if &decoded != marker {
        return Err("integration checkout marker disagrees with exact request".to_owned());
    }
    Ok(())
}

fn marker_path(checkout: &Path) -> PathBuf {
    let name = checkout
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("invalid-checkout");
    checkout.with_file_name(format!(".{name}.json"))
}

fn cleanup_payload(checkout: &IntegrationCheckout) -> Result<Vec<u8>, String> {
    let receipt = CleanupReceipt {
        schema_version: 1,
        context_digest: &checkout.marker.context_digest,
        integration_commit_sha: &checkout.marker.integration_commit_sha,
        integration_tree_sha: &checkout.marker.integration_tree_sha,
        disposition: "cleaned",
    };
    let mut payload = serde_json::to_vec_pretty(&receipt)
        .map_err(|error| format!("serialize integration cleanup receipt: {error}"))?;
    payload.push(b'\n');
    Ok(payload)
}

fn persist_cleanup_pending(checkout: &IntegrationCheckout) -> Result<(), String> {
    let payload = cleanup_payload(checkout)?;
    let destination = checkout.evidence_dir.join(CLEANUP_PENDING_NAME);
    if destination.exists() {
        if read_regular_receipt(&destination)? != payload {
            return Err(
                "pending integration cleanup receipt disagrees with exact checkout".to_owned(),
            );
        }
        return Ok(());
    }
    atomicwrites::AtomicFile::new(&destination, atomicwrites::DisallowOverwrite)
        .write(|file| file.write_all(&payload))
        .map_err(|error| format!("publish pending cleanup intent: {error}"))
}

fn publish_cleanup_receipt(checkout: &IntegrationCheckout) -> Result<(), String> {
    let payload = cleanup_payload(checkout)?;
    let pending = checkout.evidence_dir.join(CLEANUP_PENDING_NAME);
    let destination = checkout.evidence_dir.join(CLEANUP_RECEIPT_NAME);
    if destination.exists() {
        if read_regular_receipt(&destination)? != payload {
            return Err("integration cleanup receipt disagrees with exact checkout".to_owned());
        }
        if pending.exists() {
            fs::remove_file(&pending)
                .map_err(|error| format!("remove completed cleanup pending receipt: {error}"))?;
            sync_directory(&checkout.evidence_dir)?;
        }
        return Ok(());
    }
    atomicwrites::move_atomic(&pending, &destination)
        .map_err(|error| format!("publish integration cleanup receipt: {error}"))?;
    Ok(())
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync integration evidence directory: {error}"))
}

#[cfg(windows)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "Windows durable publication uses MoveFileExW with WRITE_THROUGH; only marker deletion reaches this recovery-safe barrier"
)]
fn sync_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn exact_git(cwd: &Path, args: &[&str], expected: &str) -> Result<(), String> {
    let actual = git_path(cwd, args)?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "integration checkout Git identity mismatch: expected {expected}, got {actual}"
        ))
    }
}

fn git_path(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("run git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!("git {} failed", args.join(" ")));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|error| format!("decode git {}: {error}", args.join(" ")))
}

fn canonical_git_path(cwd: &Path, value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    path.canonicalize()
        .map_err(|error| format!("canonicalize Git common directory: {error}"))
}

#[cfg(not(windows))]
fn git_cli_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Git for Windows does not accept Win32 verbatim paths as worktree path
/// arguments. Keep the canonical path for filesystem and custody checks, but
/// remove only that transport prefix at the Git CLI boundary.
#[cfg(windows)]
fn git_cli_path(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};

    const VERBATIM: &[u16] = &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const VERBATIM_UNC: &[u16] = &[
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ];

    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let ordinary = if let Some(remainder) = encoded.strip_prefix(VERBATIM_UNC) {
        let mut ordinary = vec![b'\\' as u16, b'\\' as u16];
        ordinary.extend_from_slice(remainder);
        ordinary
    } else if let Some(remainder) = encoded.strip_prefix(VERBATIM) {
        if remainder.len() >= 3
            && remainder[0] <= u16::from(u8::MAX)
            && (remainder[0] as u8).is_ascii_alphabetic()
            && remainder[1] == b':' as u16
            && remainder[2] == b'\\' as u16
        {
            remainder.to_vec()
        } else {
            return path.to_path_buf();
        }
    } else {
        return path.to_path_buf();
    };
    PathBuf::from(OsString::from_wide(&ordinary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changed_surface::{
        MergeAuthority, PlannedSuite, SelectionOutcomes, SelectionReceipt, SelectionTier,
        StaleBaseShadowDisposition, StaleBaseShadowReceipt,
    };
    use serde::Serialize;
    use sha2::{Digest as _, Sha256};
    use std::collections::BTreeMap;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {}", args.join(" "));
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn fixture() -> (tempfile::TempDir, StaleBaseShadowReceipt) {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(
            temp.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        git(temp.path(), &["config", "user.name", "Test"]);
        fs::write(temp.path().join("base.txt"), "base\n").unwrap();
        fs::write(temp.path().join(".gitignore"), "build/\n").unwrap();
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-qm", "base"]);
        let old = git(temp.path(), &["rev-parse", "HEAD"]);
        git(temp.path(), &["checkout", "-qb", "pr-head"]);
        fs::write(temp.path().join("head.txt"), "head\n").unwrap();
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-qm", "head"]);
        let head = git(temp.path(), &["rev-parse", "HEAD"]);
        let head_tree = git(temp.path(), &["rev-parse", "HEAD^{tree}"]);
        git(temp.path(), &["checkout", "-q", "--detach", &old]);
        fs::write(temp.path().join("live.txt"), "live\n").unwrap();
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-qm", "live"]);
        let live = git(temp.path(), &["rev-parse", "HEAD"]);
        let tree = git(temp.path(), &["merge-tree", "--write-tree", &live, &head]);
        let commit = {
            let output = Command::new("git")
                .args(["commit-tree", &tree, "-p", &live, "-p", &head])
                .current_dir(temp.path())
                .env("GIT_AUTHOR_NAME", "Shipyard integration")
                .env("GIT_AUTHOR_EMAIL", "shipyard@example.invalid")
                .env("GIT_AUTHOR_DATE", "@0 +0000")
                .env("GIT_COMMITTER_NAME", "Shipyard integration")
                .env("GIT_COMMITTER_EMAIL", "shipyard@example.invalid")
                .env("GIT_COMMITTER_DATE", "@0 +0000")
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        (
            temp,
            StaleBaseShadowReceipt {
                schema_version: 1,
                disposition: StaleBaseShadowDisposition::Recomputed,
                merge_authority: MergeAuthority::BlockedUntilCurrentMergeTree,
                repository: "owner/repo".to_owned(),
                pull_request: 7,
                target: "mac".to_owned(),
                head_sha: head,
                head_tree_sha: head_tree,
                old_protected_base_sha: old.clone(),
                live_protected_base_sha: live,
                merge_base_sha: old,
                integration_tree_sha: Some(tree),
                integration_commit_sha: Some(commit),
                changed_paths_digest: "a".repeat(64),
                protected_base_delta_digest: "b".repeat(64),
                old_policy_digest: Some("c".repeat(64)),
                live_policy_digest: Some("c".repeat(64)),
                old_workflow_digest: "d".repeat(64),
                live_workflow_digest: "d".repeat(64),
                validation_contract_digest: "e".repeat(64),
                integration_changed_paths_digest: "f".repeat(64),
                shadow_selection: None,
                reason: "bounded_shadow_recomputed".to_owned(),
            },
        )
    }

    #[allow(clippy::too_many_lines)]
    fn persist_valid_activation(evidence_dir: &Path, receipt: &mut StaleBaseShadowReceipt) {
        #[derive(Serialize)]
        struct Payload<'a> {
            schema_version: u32,
            repository: &'a str,
            pull_request: u64,
            target: &'a str,
            base_sha: &'a str,
            head_sha: &'a str,
            tree_sha: &'a str,
            policy_digest: &'a str,
            selection_receipt_digest: &'a str,
            validation_contract_digest: &'a str,
            workflow_digest: &'a str,
            selected_tests_digest: &'a str,
            selected_tests: &'a [String],
        }

        let context_digest = stale_base_context_digest(receipt);
        let mut selection = SelectionReceipt {
            schema_version: 1,
            exact_head_verified: true,
            shadow_only: true,
            repository: receipt.repository.clone(),
            pull_request: receipt.pull_request,
            target: receipt.target.clone(),
            protected_ref: "refs/heads/main".to_owned(),
            pr_base_sha: receipt.live_protected_base_sha.clone(),
            protected_ref_sha: receipt.live_protected_base_sha.clone(),
            merge_base_sha: receipt.live_protected_base_sha.clone(),
            head_sha: receipt.integration_commit_sha.clone().unwrap(),
            tree_sha: receipt.integration_tree_sha.clone().unwrap(),
            changed_paths_digest: receipt.integration_changed_paths_digest.clone(),
            shadow_context_digest: Some(context_digest.clone()),
            policy_digest: receipt.live_policy_digest.clone(),
            build_type: None,
            build_flags: Vec::new(),
            changed_paths: vec!["head.txt".to_owned()],
            selected_families: vec!["head".to_owned()],
            selected_tests: vec!["test-head".to_owned()],
            selected_build_targets: Vec::new(),
            baseline_tests: vec!["test-head".to_owned()],
            family_coverage: BTreeMap::from([("head".to_owned(), 1)]),
            secondary_proofs: Vec::new(),
            planned_suite: PlannedSuite::Bounded,
            selection_tier: SelectionTier::Affected,
            authoritative_suite: PlannedSuite::Full,
            outcomes: SelectionOutcomes {
                planner: "bounded".to_owned(),
                authoritative_execution: "full".to_owned(),
            },
            selected_count: Some(1),
            full_count: Some(10),
            fallback_reason: None,
            fallback_detail: None,
            elapsed_ms: 0,
        };
        receipt.shadow_selection = Some(Box::new(selection.clone()));
        selection.shadow_context_digest = None;
        let selection_receipt_digest = sha(&serde_json::to_vec(&selection).unwrap());
        let selected_tests_digest = sha(b"test-head\n");
        let policy_digest = receipt.live_policy_digest.clone().unwrap();

        let integration_commit = receipt.integration_commit_sha.as_deref().unwrap();
        let integration_tree = receipt.integration_tree_sha.as_deref().unwrap();
        let payload_digest = sha(&serde_json::to_vec(&Payload {
            schema_version: 1,
            repository: &receipt.repository,
            pull_request: receipt.pull_request,
            target: &receipt.target,
            base_sha: &receipt.live_protected_base_sha,
            head_sha: integration_commit,
            tree_sha: integration_tree,
            policy_digest: &policy_digest,
            selection_receipt_digest: &selection_receipt_digest,
            validation_contract_digest: &receipt.validation_contract_digest,
            workflow_digest: &receipt.live_workflow_digest,
            selected_tests_digest: &selected_tests_digest,
            selected_tests: &selection.selected_tests,
        })
        .unwrap());
        let mut stale_bytes = serde_json::to_vec_pretty(receipt).unwrap();
        stale_bytes.push(b'\n');
        fs::write(
            evidence_dir.join("stale-base-shadow-test.json"),
            &stale_bytes,
        )
        .unwrap();
        let activation = serde_json::json!({
            "schema_version": 1,
            "machine_mode": "shadow_compare",
            "merge_authority": "blocked_until_current_merge_tree",
            "stale_context_digest": context_digest,
            "stale_receipt_sha256": sha(&stale_bytes),
            "plan": {
                "schema_version": 1,
                "repository": receipt.repository,
                "pull_request": receipt.pull_request,
                "target": receipt.target,
                "base_sha": receipt.live_protected_base_sha,
                "head_sha": integration_commit,
                "tree_sha": integration_tree,
                "policy_digest": policy_digest,
                "changed_paths_digest": receipt.integration_changed_paths_digest,
                "validation_contract_digest": receipt.validation_contract_digest,
                "workflow_digest": receipt.live_workflow_digest,
                "selection_receipt_digest": selection_receipt_digest,
                "selected_tests_digest": selected_tests_digest,
                "selected_build_targets_digest": null,
                "execution_payload_digest": payload_digest,
                "selected_count": 1,
                "selected_build_target_count": 0,
                "selection_tier": "affected",
                "stage": "test"
            }
        });
        fs::write(
            evidence_dir.join(ACTIVATION_RECEIPT_NAME),
            serde_json::to_vec_pretty(&activation).unwrap(),
        )
        .unwrap();
    }

    fn sha(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[cfg(windows)]
    #[test]
    fn git_cli_paths_drop_only_the_windows_verbatim_prefix() {
        assert_eq!(
            git_cli_path(Path::new(r"\\?\C:\work\shadow")),
            PathBuf::from(r"C:\work\shadow")
        );
        assert_eq!(
            git_cli_path(Path::new(r"\\?\UNC\server\share\shadow")),
            PathBuf::from(r"\\server\share\shadow")
        );
        assert_eq!(
            git_cli_path(Path::new(r"C:\work\shadow")),
            PathBuf::from(r"C:\work\shadow")
        );
        assert_eq!(
            git_cli_path(Path::new(
                r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\shadow"
            )),
            PathBuf::from(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\shadow")
        );
    }

    #[test]
    fn materialize_reconcile_and_cleanup_exact_integration_checkout() {
        let (repo, receipt) = fixture();
        let parent = repo.path().join("isolated");
        let checkout = materialize(repo.path(), &parent, &receipt).unwrap();
        assert_eq!(
            git(&checkout.path, &["rev-parse", "HEAD"]),
            receipt.integration_commit_sha.as_deref().unwrap()
        );
        let reconciled = materialize(repo.path(), &parent, &receipt).unwrap();
        assert_eq!(reconciled.path, checkout.path);
        let snapshot: IntegrationCheckoutSnapshot =
            serde_json::from_slice(&serde_json::to_vec(&reconciled.snapshot()).unwrap()).unwrap();
        let restored = snapshot.restore().unwrap();
        assert_eq!(restored.path, checkout.path);
        assert_eq!(restored.receipt, receipt);
        let guard = prepare_for_execution(&reconciled).unwrap();
        assert!(prepare_for_execution(&reconciled).is_err());
        assert!(materialize(repo.path(), &parent, &receipt).is_err());
        drop(guard);
        cleanup(&reconciled).unwrap();
        assert!(!checkout.path.exists());
        assert!(reconciled.evidence_dir.join(CLEANUP_RECEIPT_NAME).is_file());
    }

    #[test]
    fn durable_snapshot_defers_materialization_until_execution_fence() {
        let (repo, receipt) = fixture();
        let parent = repo.path().join("isolated");
        let planned = plan(repo.path(), &parent, &receipt).unwrap();
        assert!(!planned.path.exists());
        let snapshot: IntegrationCheckoutSnapshot =
            serde_json::from_slice(&serde_json::to_vec(&planned.snapshot()).unwrap()).unwrap();
        let restored = snapshot.restore().unwrap();
        assert!(!restored.path.exists());

        let guard = prepare_for_execution(&restored).unwrap();
        assert!(restored.path.exists());
        drop(guard);
        cleanup(&restored).unwrap();
    }

    #[test]
    fn terminal_cleanup_prevents_snapshot_replay_and_rematerialization() {
        let (repo, receipt) = fixture();
        let parent = repo.path().join("isolated");
        let checkout = materialize(repo.path(), &parent, &receipt).unwrap();
        let snapshot = checkout.snapshot();
        cleanup(&checkout).unwrap();
        assert!(!checkout.path.exists());

        let restored = snapshot.restore().unwrap();
        let error = prepare_for_execution(&restored).unwrap_err();
        assert!(error.contains("cleanup is pending or complete"));
        assert!(!restored.path.exists());
    }

    #[test]
    fn pending_cleanup_prevents_execution_replay() {
        let (repo, receipt) = fixture();
        let parent = repo.path().join("isolated");
        let checkout = materialize(repo.path(), &parent, &receipt).unwrap();
        persist_cleanup_pending(&checkout).unwrap();

        let error = prepare_for_execution(&checkout).unwrap_err();
        assert!(error.contains("cleanup is pending or complete"));
        assert!(checkout.path.exists());
        assert!(reconcile_pending_cleanup(repo.path(), &parent, &receipt).unwrap());
        assert!(!checkout.path.exists());
    }

    #[test]
    fn marker_disagreement_refuses_cleanup_without_deletion() {
        let (repo, receipt) = fixture();
        let parent = repo.path().join("isolated");
        let checkout = materialize(repo.path(), &parent, &receipt).unwrap();
        fs::write(marker_path(&checkout.path), "{}\n").unwrap();
        let error = cleanup(&checkout).unwrap_err();
        assert!(error.contains("decode integration marker") || error.contains("disagrees"));
        assert!(checkout.path.exists());
        assert!(!checkout.evidence_dir.join(CLEANUP_RECEIPT_NAME).exists());
    }

    #[test]
    fn restart_reconciliation_removes_ignored_attempt_residue_before_execution() {
        let (repo, receipt) = fixture();
        let parent = repo.path().join("isolated");
        let checkout = materialize(repo.path(), &parent, &receipt).unwrap();
        fs::create_dir_all(checkout.path.join("build")).unwrap();
        fs::write(checkout.path.join("build/stale.o"), "stale\n").unwrap();
        let guard = prepare_for_execution(&checkout).unwrap();
        assert!(!checkout.path.join("build/stale.o").exists());
        drop(guard);
        cleanup(&checkout).unwrap();
    }

    #[test]
    fn restart_reconciliation_restores_tracked_and_unignored_content() {
        let (repo, receipt) = fixture();
        let parent = repo.path().join("isolated");
        let checkout = materialize(repo.path(), &parent, &receipt).unwrap();
        let expected_tracked = fs::read(checkout.path.join("base.txt")).unwrap();
        fs::write(checkout.path.join("base.txt"), "tampered\n").unwrap();
        fs::write(checkout.path.join("untracked.txt"), "tampered\n").unwrap();
        let guard = prepare_for_execution(&checkout).unwrap();
        assert_eq!(
            fs::read(checkout.path.join("base.txt")).unwrap(),
            expected_tracked
        );
        assert!(!checkout.path.join("untracked.txt").exists());
        drop(guard);
        cleanup(&checkout).unwrap();
    }

    #[test]
    fn post_execution_check_detects_submodule_drift_hidden_by_ignore_all() {
        let child = tempfile::tempdir().unwrap();
        git(child.path(), &["init", "-q"]);
        git(
            child.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        git(child.path(), &["config", "user.name", "Test"]);
        fs::write(child.path().join("tracked.txt"), "clean\n").unwrap();
        git(child.path(), &["add", "."]);
        git(child.path(), &["commit", "-qm", "child"]);
        let first_child_commit = git(child.path(), &["rev-parse", "HEAD"]);
        fs::write(child.path().join("tracked.txt"), "newer\n").unwrap();
        git(child.path(), &["add", "."]);
        git(child.path(), &["commit", "-qm", "newer child"]);

        let parent = tempfile::tempdir().unwrap();
        git(parent.path(), &["init", "-q"]);
        git(
            parent.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        git(parent.path(), &["config", "user.name", "Test"]);
        git(
            parent.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                child.path().to_str().unwrap(),
                "vendor/child",
            ],
        );
        git(
            parent.path(),
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.vendor/child.ignore",
                "all",
            ],
        );
        git(parent.path(), &["add", "."]);
        git(parent.path(), &["commit", "-qm", "parent"]);
        fs::write(parent.path().join("vendor/child/tracked.txt"), "dirty\n").unwrap();

        assert!(git(parent.path(), &["status", "--porcelain=v1"]).is_empty());
        let error = verify_tracked_and_unignored(parent.path()).unwrap_err();
        assert!(error.contains("integration submodule"));

        git(
            parent.path().join("vendor/child").as_path(),
            &["reset", "--hard"],
        );
        git(
            parent.path().join("vendor/child").as_path(),
            &["checkout", "-q", &first_child_commit],
        );
        assert!(git(parent.path(), &["status", "--porcelain=v1"]).is_empty());
        let error = verify_tracked_and_unignored(parent.path()).unwrap_err();
        assert!(error.contains("commit identity"));
    }

    #[test]
    fn restart_finishes_cleanup_after_marker_removal_before_receipt_publish() {
        let (repo, receipt) = fixture();
        let parent = repo.path().join("isolated");
        let checkout = materialize(repo.path(), &parent, &receipt).unwrap();
        persist_cleanup_pending(&checkout).unwrap();
        let checkout_path = git_cli_path(&checkout.path);
        let status = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&checkout_path)
            .current_dir(repo.path())
            .status()
            .unwrap();
        assert!(status.success());
        fs::remove_file(marker_path(&checkout.path)).unwrap();
        sync_directory(&parent).unwrap();

        assert!(reconcile_pending_cleanup(repo.path(), &parent, &receipt).unwrap());
        assert!(checkout.evidence_dir.join(CLEANUP_RECEIPT_NAME).is_file());
        assert!(!checkout.evidence_dir.join(CLEANUP_PENDING_NAME).exists());
    }

    #[test]
    fn restart_finishes_cleanup_when_intent_precedes_checkout_removal() {
        let (repo, receipt) = fixture();
        let parent = repo.path().join("isolated");
        let checkout = materialize(repo.path(), &parent, &receipt).unwrap();
        persist_cleanup_pending(&checkout).unwrap();

        assert!(reconcile_pending_cleanup(repo.path(), &parent, &receipt).unwrap());
        assert!(!checkout.path.exists());
        assert!(checkout.evidence_dir.join(CLEANUP_RECEIPT_NAME).is_file());
        assert!(!checkout.evidence_dir.join(CLEANUP_PENDING_NAME).exists());
    }

    #[test]
    fn restart_reconciles_exact_activated_checkout_before_cleanup_intent() {
        let (repo, mut receipt) = fixture();
        let parent = repo.path().join("isolated");
        let checkout = materialize(repo.path(), &parent, &receipt).unwrap();
        persist_valid_activation(repo.path(), &mut receipt);

        assert!(reconcile_pending_cleanup(repo.path(), &parent, &receipt).unwrap());
        assert!(!checkout.path.exists());
        assert!(checkout.evidence_dir.join(CLEANUP_RECEIPT_NAME).is_file());
        assert!(!checkout.evidence_dir.join(CLEANUP_PENDING_NAME).exists());
    }

    #[test]
    fn restart_refuses_unbound_activation_without_deleting_checkout() {
        let (repo, receipt) = fixture();
        let parent = repo.path().join("isolated");
        let checkout = materialize(repo.path(), &parent, &receipt).unwrap();
        fs::write(repo.path().join(ACTIVATION_RECEIPT_NAME), b"{}\n").unwrap();

        assert!(reconcile_pending_cleanup(repo.path(), &parent, &receipt).is_err());
        assert!(checkout.path.exists());
        assert!(!checkout.evidence_dir.join(CLEANUP_RECEIPT_NAME).exists());
    }
}
