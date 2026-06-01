//! Self-hosted runner provisioning: pure logic shared by the CLI handler.
//!
//! `shipyard runner register|list|remove` need to name runners consistently
//! across a multi-Mac fleet, pick the next free index, derive labels that a
//! repo's workflow will actually select, parse the GitHub Actions runners API,
//! render the cross-repo pool view, and reconcile local runner directories
//! against what GitHub thinks is registered. All of that is shell-free and
//! lives here; the CLI side (`src/app/runner_provision_cmd.rs`) is the only
//! place that shells out to `gh`, `config.sh`, and `svc.sh`.
//!
//! ## Naming model
//!
//! Runners are named `<repo>-<machine-tag>-<NN>`, e.g. `pulp-studio-01`. The
//! machine tag is an explicit per-box label (`studio`, `m1`, `m5`) read from
//! Shipyard state, never derived from the hostname — two MacBook Pros can share
//! a hostname, so hostname-derived tags would collide. The shared label
//! `<repo>-build` is what a repo's workflow selects for normal routing; the
//! host label `<repo>-build-<machine-tag>` lets you pin work to one machine.

use std::fmt::Write as _;

use serde::Deserialize;

/// Default labels GitHub injects automatically for an Apple-silicon macOS
/// runner. We still pass them to `config.sh --labels` so the registered set is
/// explicit and matches the existing fleet convention; GitHub de-duplicates the
/// auto-added ones.
const BASE_LABELS: [&str; 3] = ["self-hosted", "macos", "arm64"];

/// One label entry from the GitHub Actions runners API.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiLabel {
    /// Label text, e.g. `pulp-build`.
    pub name: String,
}

/// A self-hosted runner as reported by `GET /repos/{slug}/actions/runners`.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiRunner {
    /// Registered runner name, e.g. `pulp-studio-01`.
    pub name: String,
    /// `"online"` or `"offline"`.
    #[serde(default)]
    pub status: String,
    /// Whether the runner is currently executing a job.
    #[serde(default)]
    pub busy: bool,
    /// Labels attached to the runner.
    #[serde(default)]
    pub labels: Vec<ApiLabel>,
}

impl ApiRunner {
    /// Label names as plain strings, lowercased for stable comparison.
    #[must_use]
    pub fn label_names(&self) -> Vec<String> {
        self.labels
            .iter()
            .map(|label| label.name.to_lowercase())
            .collect()
    }
}

/// The envelope returned by the runners list endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct RunnersResponse {
    /// Total runners registered on the repo.
    #[serde(default)]
    pub total_count: u32,
    /// The runner records.
    #[serde(default)]
    pub runners: Vec<ApiRunner>,
}

/// Parse the runners list JSON returned by `gh api`.
///
/// # Errors
/// Returns the serde error message when the payload is not the expected shape.
pub fn parse_runners_response(raw: &str) -> Result<RunnersResponse, String> {
    serde_json::from_str(raw).map_err(|error| format!("runner list JSON parse failed: {error}"))
}

/// The short repo name (segment after `/`) for an `owner/repo` slug.
///
/// Falls back to the whole string when there is no slash.
#[must_use]
pub fn short_repo(slug: &str) -> &str {
    slug.rsplit('/').next().unwrap_or(slug)
}

/// Validate a machine tag: lowercase ASCII alphanumerics and dashes, non-empty,
/// not leading/trailing/doubled dashes.
///
/// # Errors
/// Returns a human-readable reason when the tag is unusable in a runner name.
pub fn validate_machine_tag(tag: &str) -> Result<(), String> {
    if tag.is_empty() {
        return Err("machine tag is empty".to_owned());
    }
    if tag.starts_with('-') || tag.ends_with('-') || tag.contains("--") {
        return Err(format!(
            "machine tag `{tag}` must not start, end, or repeat a dash"
        ));
    }
    if !tag
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "machine tag `{tag}` must be lowercase letters, digits, and dashes only"
        ));
    }
    Ok(())
}

/// The runner name for a repo + machine tag + 1-based index, e.g.
/// `pulp-studio-01`.
#[must_use]
pub fn runner_name(repo_short: &str, machine_tag: &str, index: u32) -> String {
    format!("{repo_short}-{machine_tag}-{index:02}")
}

/// The default label set for a repo + machine tag: the three auto-added base
/// labels plus the shared `<repo>-build` routing label and the per-host
/// `<repo>-build-<tag>` pin label.
#[must_use]
pub fn default_labels(repo_short: &str, machine_tag: &str) -> Vec<String> {
    let mut labels: Vec<String> = BASE_LABELS.iter().map(|s| (*s).to_owned()).collect();
    labels.push(format!("{repo_short}-build"));
    labels.push(format!("{repo_short}-build-{machine_tag}"));
    labels
}

/// The next free 1-based index for `<repo>-<tag>-NN`, given existing runner
/// names (from any machine). Returns `max + 1`, or `1` when none match.
///
/// This lets `register` be re-run to append capacity (`-03`, `-04`) without
/// colliding with names already taken on this or another box.
#[must_use]
pub fn next_index(existing_names: &[String], repo_short: &str, machine_tag: &str) -> u32 {
    let prefix = format!("{repo_short}-{machine_tag}-");
    let highest = existing_names
        .iter()
        .filter_map(|name| name.strip_prefix(&prefix))
        .filter(|suffix| !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()))
        .filter_map(|suffix| suffix.parse::<u32>().ok())
        .max();
    highest.map_or(1, |n| n + 1)
}

/// Infer the machine tag for a runner from its labels first (a
/// `<repo>-build-<tag>` label is authoritative), then from the middle segment
/// of a `<repo>-<tag>-NN` name. Returns `None` when neither yields a tag.
#[must_use]
pub fn infer_machine_tag(name: &str, label_names: &[String]) -> Option<String> {
    for label in label_names {
        if let Some(idx) = label.find("-build-") {
            let tag = &label[idx + "-build-".len()..];
            if !tag.is_empty() {
                return Some(tag.to_owned());
            }
        }
    }
    // Fall back to the name's middle segment(s): repo-<tag>-NN where the last
    // segment is the numeric index. Anything between the first and last segment
    // is treated as the tag (tags themselves never contain the index).
    let segments: Vec<&str> = name.split('-').collect();
    if segments.len() >= 3 {
        let last = segments[segments.len() - 1];
        if last.chars().all(|c| c.is_ascii_digit()) {
            let tag = segments[1..segments.len() - 1].join("-");
            if !tag.is_empty() {
                return Some(tag);
            }
        }
    }
    None
}

/// A single row in the `shipyard runner list` pool view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolRow {
    /// Runner name.
    pub name: String,
    /// Short repo name the runner is registered to.
    pub repo: String,
    /// `"online"` / `"offline"`.
    pub status: String,
    /// `true` when executing a job.
    pub busy: bool,
    /// Inferred machine tag, or `"?"` when unknown.
    pub machine: String,
    /// Comma-joined label names.
    pub labels: String,
}

/// Build pool rows for one repo's runners.
#[must_use]
pub fn pool_rows(repo_short: &str, runners: &[ApiRunner]) -> Vec<PoolRow> {
    runners
        .iter()
        .map(|runner| {
            let label_names = runner.label_names();
            PoolRow {
                name: runner.name.clone(),
                repo: repo_short.to_owned(),
                status: if runner.status.is_empty() {
                    "unknown".to_owned()
                } else {
                    runner.status.clone()
                },
                busy: runner.busy,
                machine: infer_machine_tag(&runner.name, &label_names)
                    .unwrap_or_else(|| "?".to_owned()),
                labels: label_names.join(","),
            }
        })
        .collect()
}

/// Render the pool rows as an aligned text table, grouped by machine tag.
/// Rows are sorted by machine, then repo, then name for stable output.
#[must_use]
pub fn format_pool_table(rows: &[PoolRow]) -> String {
    if rows.is_empty() {
        return "No self-hosted runners found.".to_owned();
    }
    let mut sorted = rows.to_vec();
    sorted.sort_by(|a, b| {
        a.machine
            .cmp(&b.machine)
            .then(a.repo.cmp(&b.repo))
            .then(a.name.cmp(&b.name))
    });

    let name_w = sorted
        .iter()
        .map(|r| r.name.len())
        .chain(std::iter::once("RUNNER".len()))
        .max()
        .unwrap_or(6);
    let repo_w = sorted
        .iter()
        .map(|r| r.repo.len())
        .chain(std::iter::once("REPO".len()))
        .max()
        .unwrap_or(4);
    let machine_w = sorted
        .iter()
        .map(|r| r.machine.len())
        .chain(std::iter::once("MACHINE".len()))
        .max()
        .unwrap_or(7);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<name_w$}  {:<machine_w$}  {:<repo_w$}  {:<8}  LABELS",
        "RUNNER", "MACHINE", "REPO", "STATE"
    );
    for r in &sorted {
        let state = format!("{}/{}", r.status, if r.busy { "busy" } else { "idle" });
        let _ = writeln!(
            out,
            "{:<name_w$}  {:<machine_w$}  {:<repo_w$}  {:<8}  {}",
            r.name, r.machine, r.repo, state, r.labels
        );
    }
    out.trim_end().to_owned()
}

/// Local runner directories on this machine that GitHub no longer knows about —
/// i.e. orphaned `~/actions-runner-*` checkouts whose configured name is absent
/// from the live runner set. `local_names` are the configured agent names read
/// from each local `.runner` file; `github_names` is every runner GitHub
/// reports for the relevant repos.
#[must_use]
pub fn orphan_local_runners(local_names: &[String], github_names: &[String]) -> Vec<String> {
    local_names
        .iter()
        .filter(|name| !github_names.iter().any(|g| g.eq_ignore_ascii_case(name)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_repo_extracts_segment() {
        assert_eq!(short_repo("danielraffel/pulp"), "pulp");
        assert_eq!(short_repo("pulp"), "pulp");
    }

    #[test]
    fn runner_name_zero_pads_index() {
        assert_eq!(runner_name("pulp", "studio", 1), "pulp-studio-01");
        assert_eq!(runner_name("pulp", "studio", 12), "pulp-studio-12");
    }

    #[test]
    fn default_labels_include_routing_and_host_labels() {
        let labels = default_labels("pulp", "studio");
        assert_eq!(
            labels,
            vec![
                "self-hosted",
                "macos",
                "arm64",
                "pulp-build",
                "pulp-build-studio"
            ]
        );
    }

    #[test]
    fn next_index_starts_at_one_when_none_match() {
        assert_eq!(next_index(&[], "pulp", "studio"), 1);
        assert_eq!(
            next_index(&["pulp-m1-01".to_owned()], "pulp", "studio"),
            1,
            "other-machine runners must not advance the studio index"
        );
    }

    #[test]
    fn next_index_continues_after_highest() {
        let existing = vec![
            "pulp-studio-01".to_owned(),
            "pulp-studio-03".to_owned(),
            "pulp-studio-02".to_owned(),
            "pulp-m1-09".to_owned(),
        ];
        assert_eq!(next_index(&existing, "pulp", "studio"), 4);
    }

    #[test]
    fn next_index_ignores_non_numeric_suffix() {
        let existing = vec!["pulp-studio-build".to_owned(), "pulp-studio-2x".to_owned()];
        assert_eq!(next_index(&existing, "pulp", "studio"), 1);
    }

    #[test]
    fn infer_machine_tag_prefers_host_label() {
        let labels = vec!["pulp-build".to_owned(), "pulp-build-studio".to_owned()];
        assert_eq!(
            infer_machine_tag("anything", &labels),
            Some("studio".to_owned())
        );
    }

    #[test]
    fn infer_machine_tag_falls_back_to_name() {
        assert_eq!(
            infer_machine_tag("pulp-m1-02", &[]),
            Some("m1".to_owned())
        );
    }

    #[test]
    fn infer_machine_tag_handles_multi_segment_tag() {
        assert_eq!(
            infer_machine_tag("pulp-mac-studio-02", &[]),
            Some("mac-studio".to_owned())
        );
    }

    #[test]
    fn infer_machine_tag_none_when_no_index_or_label() {
        assert_eq!(infer_machine_tag("daniels-macbook-shipyard", &[]), None);
    }

    #[test]
    fn validate_machine_tag_accepts_clean_tags() {
        assert!(validate_machine_tag("studio").is_ok());
        assert!(validate_machine_tag("m5").is_ok());
        assert!(validate_machine_tag("mac-studio").is_ok());
    }

    #[test]
    fn validate_machine_tag_rejects_bad_tags() {
        assert!(validate_machine_tag("").is_err());
        assert!(validate_machine_tag("Studio").is_err());
        assert!(validate_machine_tag("-m1").is_err());
        assert!(validate_machine_tag("m1-").is_err());
        assert!(validate_machine_tag("mac--studio").is_err());
        assert!(validate_machine_tag("m 1").is_err());
    }

    #[test]
    fn parse_runners_response_reads_github_shape() {
        let raw = r#"{
            "total_count": 2,
            "runners": [
                {"id": 1, "name": "pulp-studio-01", "status": "online", "busy": true,
                 "labels": [{"name": "self-hosted"}, {"name": "pulp-build"}]},
                {"id": 2, "name": "pulp-studio-02", "status": "offline", "busy": false,
                 "labels": [{"name": "pulp-build-studio"}]}
            ]
        }"#;
        let parsed = parse_runners_response(raw).expect("parse");
        assert_eq!(parsed.total_count, 2);
        assert_eq!(parsed.runners.len(), 2);
        assert_eq!(parsed.runners[0].name, "pulp-studio-01");
        assert!(parsed.runners[0].busy);
        assert_eq!(
            parsed.runners[0].label_names(),
            vec!["self-hosted", "pulp-build"]
        );
    }

    #[test]
    fn pool_rows_infer_machine_and_state() {
        let runners = vec![ApiRunner {
            name: "pulp-studio-01".to_owned(),
            status: "online".to_owned(),
            busy: true,
            labels: vec![ApiLabel {
                name: "pulp-build-studio".to_owned(),
            }],
        }];
        let rows = pool_rows("pulp", &runners);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].machine, "studio");
        assert_eq!(rows[0].repo, "pulp");
        assert!(rows[0].busy);
    }

    #[test]
    fn format_pool_table_groups_and_aligns() {
        let rows = vec![
            PoolRow {
                name: "pulp-studio-01".to_owned(),
                repo: "pulp".to_owned(),
                status: "online".to_owned(),
                busy: false,
                machine: "studio".to_owned(),
                labels: "pulp-build".to_owned(),
            },
            PoolRow {
                name: "pulp-m1-01".to_owned(),
                repo: "pulp".to_owned(),
                status: "online".to_owned(),
                busy: true,
                machine: "m1".to_owned(),
                labels: "pulp-build".to_owned(),
            },
        ];
        let table = format_pool_table(&rows);
        // m1 sorts before studio.
        let m1_pos = table.find("pulp-m1-01").expect("m1 row");
        let studio_pos = table.find("pulp-studio-01").expect("studio row");
        assert!(m1_pos < studio_pos);
        assert!(table.contains("online/busy"));
        assert!(table.contains("online/idle"));
    }

    #[test]
    fn format_pool_table_empty_is_friendly() {
        assert_eq!(format_pool_table(&[]), "No self-hosted runners found.");
    }

    #[test]
    fn orphan_local_runners_flags_dirs_missing_from_github() {
        let local = vec![
            "pulp-studio-01".to_owned(),
            "pulp-studio-02".to_owned(),
            "Daniels-MacBook-Pro".to_owned(),
        ];
        let github = vec!["pulp-studio-01".to_owned(), "pulp-studio-02".to_owned()];
        assert_eq!(
            orphan_local_runners(&local, &github),
            vec!["Daniels-MacBook-Pro".to_owned()]
        );
    }
}
