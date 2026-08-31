//! Content-addressed, restart-safe checkouts for stale-base shadow execution.
//!
//! These checkouts are never merge authority. They only provide an isolated
//! filesystem whose `HEAD` and tree exactly match the synthetic integration
//! identity recorded by a stale-base shadow receipt.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use super::{StaleBaseShadowReceipt, stale_base_context_digest};

const CLEANUP_RECEIPT_NAME: &str = "stale-cleanup-shadow_compare.json";

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
pub(crate) fn materialize(
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
    ensure_real_directory(checkout_parent)?;
    let checkout = checkout_parent.join(format!("shadow-{context_digest}"));
    let evidence_dir = checkout_parent
        .parent()
        .ok_or_else(|| "integration checkout root has no evidence parent".to_owned())?
        .to_path_buf();
    let marker = CheckoutMarker {
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
    };

    match fs::symlink_metadata(&checkout) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {}
        Ok(_) => return Err("integration checkout path is not a real directory".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let status = Command::new("git")
                .args(["worktree", "add", "--detach"])
                .arg(&checkout)
                .arg(integration_commit)
                .current_dir(&source_repo)
                .status()
                .map_err(|error| format!("create integration checkout: {error}"))?;
            if !status.success() {
                return Err("create integration checkout: git worktree add failed".to_owned());
            }
        }
        Err(error) => return Err(format!("inspect integration checkout: {error}")),
    }

    verify_checkout(&checkout, &marker)?;
    initialize_submodules(&checkout)?;
    verify_checkout(&checkout, &marker)?;
    persist_or_verify_marker(&checkout, &marker)?;
    Ok(IntegrationCheckout {
        path: checkout,
        source_repo,
        evidence_dir,
        lock_path: checkout_parent.join(format!("shadow-{}.lock", marker.context_digest)),
        marker,
    })
}

/// Hold the exact checkout's execution fence and prove its content before a
/// command can start. The returned file must remain alive through execution,
/// post-execution verification, and cleanup.
pub(crate) fn prepare_for_execution(checkout: &IntegrationCheckout) -> Result<fs::File, String> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&checkout.lock_path)
        .map_err(|error| format!("open integration execution fence: {error}"))?;
    lock.try_lock_exclusive()
        .map_err(|error| format!("integration checkout is already owned: {error}"))?;
    verify_checkout(&checkout.path, &checkout.marker)?;
    verify_marker(&checkout.path, &checkout.marker)?;
    restore_pristine_checkout(&checkout.path)?;
    verify_pristine(&checkout.path)?;
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
        let status = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&checkout.path)
            .current_dir(&checkout.source_repo)
            .status()
            .map_err(|error| format!("remove integration checkout: {error}"))?;
        if !status.success() || checkout.path.exists() {
            return Err("remove integration checkout: exact worktree remains".to_owned());
        }
    } else {
        verify_marker(&checkout.path, &checkout.marker)?;
    }
    persist_cleanup_receipt(checkout)?;
    let marker_path = marker_path(&checkout.path);
    fs::remove_file(&marker_path)
        .map_err(|error| format!("remove completed integration marker: {error}"))?;
    sync_directory(
        marker_path
            .parent()
            .ok_or_else(|| "integration marker has no parent directory".to_owned())?,
    )?;
    Ok(())
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
    if status.is_empty() {
        Ok(())
    } else {
        Err("integration checkout contains tracked or unignored content drift".to_owned())
    }
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
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker_path)
    {
        Ok(mut file) => {
            file.write_all(&payload)
                .and_then(|()| file.sync_all())
                .map_err(|error| format!("write integration marker: {error}"))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::read(&marker_path)
                .map_err(|error| format!("read integration marker: {error}"))?
                != payload
            {
                return Err("integration checkout marker disagrees with exact request".to_owned());
            }
        }
        Err(error) => return Err(format!("create integration marker: {error}")),
    }
    sync_directory(
        marker_path
            .parent()
            .ok_or_else(|| "integration marker has no parent directory".to_owned())?,
    )?;
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

fn persist_cleanup_receipt(checkout: &IntegrationCheckout) -> Result<(), String> {
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
    let destination = checkout.evidence_dir.join(CLEANUP_RECEIPT_NAME);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
    {
        Ok(mut file) => file
            .write_all(&payload)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("write integration cleanup receipt: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::read(&destination)
                .map_err(|error| format!("read integration cleanup receipt: {error}"))?
                != payload
            {
                return Err("integration cleanup receipt disagrees with exact checkout".to_owned());
            }
        }
        Err(error) => return Err(format!("create integration cleanup receipt: {error}")),
    }
    sync_directory(&checkout.evidence_dir)?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync integration evidence directory: {error}"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changed_surface::{
        MergeAuthority, StaleBaseShadowDisposition, StaleBaseShadowReceipt,
    };
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
        let guard = prepare_for_execution(&reconciled).unwrap();
        assert!(prepare_for_execution(&reconciled).is_err());
        drop(guard);
        cleanup(&reconciled).unwrap();
        assert!(!checkout.path.exists());
        assert!(reconciled.evidence_dir.join(CLEANUP_RECEIPT_NAME).is_file());
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
        fs::write(checkout.path.join("base.txt"), "tampered\n").unwrap();
        fs::write(checkout.path.join("untracked.txt"), "tampered\n").unwrap();
        let guard = prepare_for_execution(&checkout).unwrap();
        assert_eq!(
            fs::read_to_string(checkout.path.join("base.txt")).unwrap(),
            "base\n"
        );
        assert!(!checkout.path.join("untracked.txt").exists());
        drop(guard);
        cleanup(&checkout).unwrap();
    }
}
