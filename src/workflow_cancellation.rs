//! Fail-closed policy for automated GitHub Actions run cancellation.

use std::path::Path;

use crate::cloud::QueuedRun;

/// Whether a repository-wide cleanup may consider this run for cancellation.
///
/// Broad cleanup is intentionally limited to review-validation events. Release
/// workflows are protected by both display name and workflow filename so a
/// future trigger change cannot accidentally make them eligible.
#[must_use]
pub fn is_bulk_run_cancellation_safe(run: &QueuedRun) -> bool {
    matches!(run.event.as_str(), "pull_request" | "merge_group")
        && !is_protected_release_workflow(run)
}

/// Whether this run belongs to an immutable release workflow that automated
/// cleanup must never cancel.
#[must_use]
pub fn is_protected_release_workflow(run: &QueuedRun) -> bool {
    let workflow_name = run.workflow_name.trim();
    if workflow_name.eq_ignore_ascii_case("Release CLI")
        || workflow_name.eq_ignore_ascii_case("Sign and Release")
    {
        return true;
    }

    let path_without_ref = run
        .path
        .split_once('@')
        .map_or(run.path.as_str(), |(path, _)| path);
    let filename = Path::new(path_without_ref)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    filename.eq_ignore_ascii_case("release-cli.yml")
        || filename.eq_ignore_ascii_case("release-cli.yaml")
        || filename.eq_ignore_ascii_case("sign-and-release.yml")
        || filename.eq_ignore_ascii_case("sign-and-release.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: u64, name: &str, event: &str, path: &str) -> QueuedRun {
        QueuedRun {
            database_id: id,
            name: name.to_owned(),
            head_branch: "main".to_owned(),
            event: event.to_owned(),
            created_at: String::new(),
            run_started_at: None,
            workflow_name: name.to_owned(),
            url: None,
            path: path.to_owned(),
            status: "queued".to_owned(),
            conclusion: None,
        }
    }

    #[test]
    fn bulk_cancellation_is_review_only_and_protects_release_workflows() {
        assert!(is_bulk_run_cancellation_safe(&run(
            1,
            "CI",
            "pull_request",
            ".github/workflows/ci.yml",
        )));
        assert!(!is_bulk_run_cancellation_safe(&run(
            2,
            "CI",
            "workflow_dispatch",
            ".github/workflows/ci.yml",
        )));
        assert!(!is_bulk_run_cancellation_safe(&run(
            3,
            "Release CLI",
            "pull_request",
            ".github/workflows/renamed.yml",
        )));
        assert!(!is_bulk_run_cancellation_safe(&run(
            4,
            "renamed",
            "merge_group",
            ".github/workflows/sign-and-release.yml@refs/heads/main",
        )));
    }
}
