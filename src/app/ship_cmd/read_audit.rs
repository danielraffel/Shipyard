//! The read-audit report a keyed plan hands to the project's key code.
//!
//! The key code treats an executable as keyable only when the last clean
//! read audit of the protected branch covered it, so a keyed plan needs that
//! report. The planner takes the newest successful run of the audit workflow
//! on the protected branch whose report the key code would accept (its schema,
//! a `clean` stage-0 verdict, and a published covered list), caches the
//! report once per run under the state directory, and stages a copy beside
//! the plan's binding. Without one the plan still runs keyed and
//! the key code keys nothing; the binding says why.

use std::fs;
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::changed_surface::executable_reuse::AuditBinding;

/// The workflow whose `read-audit` artifact carries the report.
pub(super) const AUDIT_WORKFLOW: &str = "read-audit-nightly.yml";
/// The artifact name and the report file inside it.
const AUDIT_ARTIFACT: &str = "read-audit";
const AUDIT_FILE: &str = "read-audit.json";
/// Newest successful runs looked at per plan.
const RUNS_LISTED: usize = 20;
/// Runs downloaded per plan before giving up; cached verdicts cost nothing.
const MAX_DOWNLOADS: usize = 3;

/// What a plan binds about the audit, and the report bytes to stage.
pub(super) struct Fetched {
    pub(super) audit: AuditBinding,
    pub(super) report: Vec<u8>,
}

impl Fetched {
    fn none(reason: &str) -> Self {
        Self {
            audit: AuditBinding::None {
                reason: reason.to_owned(),
            },
            report: Vec::new(),
        }
    }
}

/// Find the report a keyed plan against `base_sha` binds. `gh` runs one `gh`
/// command for `repository` and returns its stdout. Never fails: every
/// problem becomes `AuditBinding::None` with its reason.
pub(super) fn fetch_clean_report<G>(
    gh: &G,
    repository: &str,
    branch: &str,
    base_sha: &str,
    cache: &Path,
) -> Fetched
where
    G: Fn(&[String]) -> Result<String, String> + ?Sized,
{
    let listing = match gh(&[
        "api".to_owned(),
        format!(
            "repos/{repository}/actions/workflows/{AUDIT_WORKFLOW}/runs?branch={branch}&status=success&per_page={RUNS_LISTED}"
        ),
    ]) {
        Ok(listing) => listing,
        Err(error) => return Fetched::none(failure_reason(&error)),
    };
    let Ok(listing) = serde_json::from_str::<Value>(&listing) else {
        return Fetched::none("fetch_failed");
    };
    let runs = listing
        .get("workflow_runs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut downloads = 0;
    for run in &runs {
        let (Some(id), Some(commit)) = (
            run.get("id").and_then(Value::as_u64),
            run.get("head_sha").and_then(Value::as_str),
        ) else {
            continue;
        };
        let dir = cache.join(id.to_string());
        let report = if let Some(cached) = cached_verdict(&dir) {
            cached
        } else {
            if downloads == MAX_DOWNLOADS {
                break;
            }
            downloads += 1;
            match download(gh, repository, id, &dir) {
                Ok(report) => report,
                Err(error) => return Fetched::none(failure_reason(&error)),
            }
        };
        let Some(report) = report else {
            continue;
        };
        let commits_behind = match commits_behind(gh, repository, commit, base_sha) {
            Ok(count) => count,
            Err(error) => return Fetched::none(failure_reason(&error)),
        };
        return Fetched {
            audit: AuditBinding::Staged {
                run_id: id.to_string(),
                audit_commit: commit.to_owned(),
                commits_behind,
                report_sha256: format!("{:x}", Sha256::digest(&report)),
            },
            report,
        };
    }
    Fetched::none("no_clean_run")
}

/// A run already looked at: `Some(Some(bytes))` for a clean report,
/// `Some(None)` for one that was not clean, `None` when not cached yet.
#[allow(clippy::option_option)]
fn cached_verdict(dir: &Path) -> Option<Option<Vec<u8>>> {
    if dir.join("not-clean").exists() {
        return Some(None);
    }
    fs::read(dir.join(AUDIT_FILE)).ok().map(Some)
}

/// The read-audit report schema the key code accepts.
const READ_AUDIT_SCHEMA: &str = "pulp-read-audit/v1";

/// Whether the key code would accept `report` as vouching for executables:
/// the accepted schema, a `clean` stage-0 verdict, and a published covered
/// list. Staging anything weaker would bind `staged` while keying nothing.
fn vouches(report: &Value) -> bool {
    report.get("schema").and_then(Value::as_str) == Some(READ_AUDIT_SCHEMA)
        && report.pointer("/stage0/verdict").and_then(Value::as_str) == Some("clean")
        && report
            .pointer("/stage0/covered")
            .is_some_and(Value::is_array)
}

/// Download one run's report and cache its verdict. `Ok(None)` when the
/// report is not clean.
fn download<G>(gh: &G, repository: &str, id: u64, dir: &Path) -> Result<Option<Vec<u8>>, String>
where
    G: Fn(&[String]) -> Result<String, String> + ?Sized,
{
    let staging = dir.with_extension("download");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    gh(&[
        "run".to_owned(),
        "download".to_owned(),
        id.to_string(),
        "--repo".to_owned(),
        repository.to_owned(),
        "--name".to_owned(),
        AUDIT_ARTIFACT.to_owned(),
        "--dir".to_owned(),
        staging.to_string_lossy().into_owned(),
    ])?;
    let bytes = fs::read(staging.join(AUDIT_FILE))
        .map_err(|error| format!("the {AUDIT_ARTIFACT} artifact has no {AUDIT_FILE}: {error}"))?;
    let clean = serde_json::from_slice::<Value>(&bytes).is_ok_and(|report| vouches(&report));
    fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    if clean {
        fs::write(dir.join(AUDIT_FILE), &bytes).map_err(|error| error.to_string())?;
    } else {
        fs::write(dir.join("not-clean"), b"").map_err(|error| error.to_string())?;
    }
    let _ = fs::remove_dir_all(&staging);
    Ok(clean.then_some(bytes))
}

/// How many commits `base_sha` is ahead of `audit_commit`.
fn commits_behind<G>(
    gh: &G,
    repository: &str,
    audit_commit: &str,
    base_sha: &str,
) -> Result<u64, String>
where
    G: Fn(&[String]) -> Result<String, String> + ?Sized,
{
    let compared = gh(&[
        "api".to_owned(),
        format!("repos/{repository}/compare/{audit_commit}...{base_sha}"),
        "--jq".to_owned(),
        ".ahead_by".to_owned(),
    ])?;
    compared
        .trim()
        .parse()
        .map_err(|_| format!("the compare API stated no ahead_by: {compared:?}"))
}

/// `no_credentials` for an authentication refusal, `fetch_failed` otherwise.
fn failure_reason(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    if [
        "401",
        "403",
        "authentication",
        "gh auth login",
        "bad credentials",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        "no_credentials"
    } else {
        "fetch_failed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// The report a fake run serves: `clean` and `findings` are well formed;
    /// `old_schema` and `uncovered` are clean verdicts the key code rejects.
    fn report(verdict: &str) -> Value {
        match verdict {
            "old_schema" => serde_json::json!({
                "schema": "pulp-read-audit/v0",
                "stage0": {"verdict": "clean", "covered": []},
            }),
            "uncovered" => serde_json::json!({
                "schema": READ_AUDIT_SCHEMA,
                "stage0": {"verdict": "clean"},
            }),
            verdict => serde_json::json!({
                "schema": READ_AUDIT_SCHEMA,
                "stage0": {"verdict": verdict, "covered": ["pulp-test-a"]},
            }),
        }
    }

    /// A `gh` that lists `runs` and serves each run's report from `reports`.
    fn fake<'a>(
        runs: &'a [(u64, &'a str)],
        reports: &'a [(u64, Option<&'a str>)],
        calls: &'a RefCell<Vec<String>>,
    ) -> impl Fn(&[String]) -> Result<String, String> + 'a {
        move |args: &[String]| {
            calls.borrow_mut().push(args.join(" "));
            match args.first().map(String::as_str) {
                Some("api") if args[1].contains("/runs?") => Ok(serde_json::json!({
                    "workflow_runs": runs.iter().map(|(id, sha)| serde_json::json!({"id": id, "head_sha": sha})).collect::<Vec<_>>()
                })
                .to_string()),
                Some("api") if args[1].contains("/compare/") => Ok("7\n".to_owned()),
                Some("run") => {
                    let id: u64 = args[2].parse().expect("id");
                    let dir = std::path::PathBuf::from(&args[8]);
                    match reports.iter().find(|(run, _)| *run == id).and_then(|(_, r)| *r) {
                        Some(verdict) => {
                            fs::write(dir.join(AUDIT_FILE), report(verdict).to_string())
                                .expect("write");
                            Ok(String::new())
                        }
                        None => Err("no artifact".to_owned()),
                    }
                }
                _ => Err("unexpected".to_owned()),
            }
        }
    }

    #[test]
    fn the_newest_clean_run_is_staged_with_its_commit_and_distance() {
        let temp = tempfile::tempdir().expect("tempdir");
        let calls = RefCell::new(Vec::new());
        let runs = [
            (30, "c".repeat(40)),
            (20, "b".repeat(40)),
            (10, "a".repeat(40)),
        ];
        let runs: Vec<(u64, &str)> = runs.iter().map(|(id, sha)| (*id, sha.as_str())).collect();
        let gh = fake(
            &runs,
            &[
                (30, Some("findings")),
                (20, Some("clean")),
                (10, Some("clean")),
            ],
            &calls,
        );
        let fetched = fetch_clean_report(&gh, "o/r", "main", &"f".repeat(40), temp.path());
        let AuditBinding::Staged {
            run_id,
            audit_commit,
            commits_behind,
            report_sha256,
        } = &fetched.audit
        else {
            panic!("expected a staged report: {:?}", fetched.audit);
        };
        assert_eq!(run_id, "20", "the newest CLEAN run, not the newest run");
        assert_eq!(audit_commit, &"b".repeat(40));
        assert_eq!(*commits_behind, 7);
        assert_eq!(
            report_sha256,
            &format!("{:x}", Sha256::digest(&fetched.report))
        );
        // A second plan reads both verdicts from the cache.
        calls.borrow_mut().clear();
        let again = fetch_clean_report(&gh, "o/r", "main", &"f".repeat(40), temp.path());
        assert_eq!(again.audit, fetched.audit);
        assert!(
            !calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("run download")),
            "{:?}",
            calls.borrow()
        );
    }

    #[test]
    fn no_clean_run_and_failures_bind_none_with_the_reason() {
        let temp = tempfile::tempdir().expect("tempdir");
        let calls = RefCell::new(Vec::new());
        let sha = "a".repeat(40);
        let runs = [(1, sha.as_str())];
        let gh = fake(&runs, &[(1, Some("findings"))], &calls);
        let fetched = fetch_clean_report(&gh, "o/r", "main", &sha, temp.path());
        assert_eq!(
            fetched.audit,
            AuditBinding::None {
                reason: "no_clean_run".to_owned()
            }
        );
        assert!(fetched.report.is_empty());

        let auth = |_: &[String]| Err("HTTP 401: Bad credentials".to_owned());
        let fetched = fetch_clean_report(&auth, "o/r", "main", &sha, temp.path());
        assert_eq!(
            fetched.audit,
            AuditBinding::None {
                reason: "no_credentials".to_owned()
            }
        );
        let down = |_: &[String]| Err("connection reset".to_owned());
        let fetched = fetch_clean_report(&down, "o/r", "main", &sha, temp.path());
        assert_eq!(
            fetched.audit,
            AuditBinding::None {
                reason: "fetch_failed".to_owned()
            }
        );
    }

    #[test]
    fn a_clean_verdict_the_key_code_would_reject_is_not_staged() {
        let temp = tempfile::tempdir().expect("tempdir");
        let calls = RefCell::new(Vec::new());
        let shas: Vec<String> = (1..=3).map(|n| format!("{n:040}")).collect();
        let runs: Vec<(u64, &str)> = vec![
            (3, shas[2].as_str()),
            (2, shas[1].as_str()),
            (1, shas[0].as_str()),
        ];
        let gh = fake(
            &runs,
            &[
                (3, Some("old_schema")),
                (2, Some("uncovered")),
                (1, Some("clean")),
            ],
            &calls,
        );
        let fetched = fetch_clean_report(&gh, "o/r", "main", &"f".repeat(40), temp.path());
        let AuditBinding::Staged { run_id, .. } = &fetched.audit else {
            panic!("expected run 1 staged: {:?}", fetched.audit);
        };
        assert_eq!(run_id, "1");
        assert!(vouches(
            &serde_json::from_slice(&fetched.report).expect("report")
        ));
        for rejected in ["old_schema", "uncovered", "findings"] {
            assert!(!vouches(&report(rejected)), "{rejected}");
        }
    }

    #[test]
    fn downloads_per_plan_are_bounded() {
        let temp = tempfile::tempdir().expect("tempdir");
        let calls = RefCell::new(Vec::new());
        let shas: Vec<String> = (0..6).map(|n| format!("{n:040}")).collect();
        let runs: Vec<(u64, &str)> = shas
            .iter()
            .enumerate()
            .map(|(n, sha)| (100 - n as u64, sha.as_str()))
            .collect();
        let reports: Vec<(u64, Option<&str>)> =
            runs.iter().map(|(id, _)| (*id, Some("findings"))).collect();
        let gh = fake(&runs, &reports, &calls);
        let fetched = fetch_clean_report(&gh, "o/r", "main", &"f".repeat(40), temp.path());
        assert_eq!(
            fetched.audit,
            AuditBinding::None {
                reason: "no_clean_run".to_owned()
            }
        );
        let downloads = calls
            .borrow()
            .iter()
            .filter(|call| call.starts_with("run download"))
            .count();
        assert_eq!(downloads, MAX_DOWNLOADS);
    }
}
