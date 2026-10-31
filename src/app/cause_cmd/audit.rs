//! Read-only integrity checks for the local cause archive.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::super::CliFailure;
use super::super::cli::CauseAuditArgs;
use crate::diagnose::cause::{self, CiCause};

const SCHEMA: &str = "shipyard.cause-audit/v1";

#[derive(Debug, Serialize)]
struct AuditReport {
    schema: &'static str,
    dir: String,
    files_scanned: usize,
    cause_rows: usize,
    fixlink_rows: usize,
    findings: Vec<Finding>,
    ok: bool,
}

#[derive(Debug, Serialize, Eq, PartialEq, Ord, PartialOrd)]
struct Finding {
    file: String,
    line: usize,
    kind: &'static str,
    detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct RedKey {
    repo: String,
    run_id: u64,
    attempt: u64,
}

fn finding(
    path: &Path,
    dir: &Path,
    line: usize,
    kind: &'static str,
    detail: impl Into<String>,
) -> Finding {
    Finding {
        file: path
            .strip_prefix(dir)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned(),
        line,
        kind,
        detail: detail.into(),
    }
}

fn jsonl_files(dir: &Path) -> Result<Vec<PathBuf>, CliFailure> {
    let mut paths = Vec::new();
    let causes = dir.join("causes");
    if causes.exists() {
        for entry in fs::read_dir(&causes).map_err(|error| {
            CliFailure::new(2, format!("cannot read {}: {error}", causes.display()))
        })? {
            let path = entry
                .map_err(|error| CliFailure::new(2, error.to_string()))?
                .path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                paths.push(path);
            }
        }
    }
    let fixlinks = dir.join("fixlinks.jsonl");
    if fixlinks.exists() {
        paths.push(fixlinks);
    }
    paths.sort();
    Ok(paths)
}

fn cause_row(value: Value) -> Option<CiCause> {
    let candidate = if value.get("schema").and_then(Value::as_str) == Some(cause::SCHEMA) {
        value
    } else {
        value.get("cause").filter(|cause| !cause.is_null())?.clone()
    };
    let record: CiCause = serde_json::from_value(candidate).ok()?;
    (record.schema == cause::SCHEMA).then_some(record)
}

#[derive(Default)]
struct ArchiveRows {
    files_scanned: usize,
    causes: Vec<(CiCause, PathBuf, usize)>,
    fixlinks: Vec<(RedKey, PathBuf, usize, Option<String>, bool)>,
    findings: Vec<Finding>,
}

fn read_archive(dir: &Path) -> Result<ArchiveRows, CliFailure> {
    if !dir.is_dir() {
        return Err(CliFailure::new(
            2,
            format!("cause archive directory does not exist: {}", dir.display()),
        ));
    }
    super::require_archive_parent(dir).map_err(|error| CliFailure::new(2, error))?;
    let paths = jsonl_files(dir)?;
    let mut findings = Vec::new();
    let mut causes = Vec::new();
    let mut fixlinks = Vec::new();
    let mut files_scanned = 0;
    for path in paths {
        files_scanned += 1;
        let text = fs::read_to_string(&path).map_err(|error| {
            CliFailure::new(2, format!("cannot read {}: {error}", path.display()))
        })?;
        let is_fixlinks = path.file_name().and_then(|name| name.to_str()) == Some("fixlinks.jsonl");
        for (index, line) in text.lines().enumerate() {
            let line_number = index + 1;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                findings.push(finding(
                    &path,
                    dir,
                    line_number,
                    "invalid_json",
                    "line is not valid JSON",
                ));
                continue;
            };
            if is_fixlinks {
                if !super::advice::valid_fixlink(&value) {
                    findings.push(finding(
                        &path,
                        dir,
                        line_number,
                        "invalid_fixlink",
                        "row is not a complete, safe ci.cause/v1 fix-link",
                    ));
                    continue;
                }
                let (Some(repo), Some(run_id), Some(attempt)) = (
                    value["repo"].as_str(),
                    value["red_run_id"].as_u64(),
                    value["red_attempt"].as_u64(),
                ) else {
                    findings.push(finding(
                        &path,
                        dir,
                        line_number,
                        "invalid_fixlink",
                        "fix-link lacks repo, red_run_id, or red_attempt",
                    ));
                    continue;
                };
                fixlinks.push((
                    RedKey {
                        repo: repo.to_owned(),
                        run_id,
                        attempt,
                    },
                    path.clone(),
                    line_number,
                    value["derivation"].as_str().map(str::to_owned),
                    is_proven_rerun_green(&value),
                ));
            } else if let Some(record) = cause_row(value) {
                causes.push((record, path.clone(), line_number));
            } else {
                findings.push(finding(
                    &path,
                    dir,
                    line_number,
                    "invalid_cause",
                    "row is not a valid ci.cause/v1 record",
                ));
            }
        }
    }

    Ok(ArchiveRows {
        files_scanned,
        causes,
        fixlinks,
        findings,
    })
}

fn audit(args: &CauseAuditArgs) -> Result<AuditReport, CliFailure> {
    let _lock = super::lock_archive(&args.dir, false).map_err(|error| CliFailure::new(1, error))?;
    let ArchiveRows {
        files_scanned,
        causes,
        fixlinks,
        mut findings,
    } = read_archive(&args.dir)?;
    let mut seen = BTreeMap::<String, (PathBuf, usize)>::new();
    let mut red_keys = BTreeSet::new();
    for (record, path, line) in &causes {
        let id = record.id();
        if let Some((previous_path, previous_line)) = seen.insert(id.clone(), (path.clone(), *line))
        {
            findings.push(finding(
                path,
                &args.dir,
                *line,
                "duplicate_cause",
                format!(
                    "same cause id as {}:{}",
                    previous_path
                        .strip_prefix(&args.dir)
                        .unwrap_or(&previous_path)
                        .display(),
                    previous_line
                ),
            ));
        }
        if let (Some(run_id), Some(attempt)) = (record.run_id, record.attempt) {
            red_keys.insert(RedKey {
                repo: record.repo.clone(),
                run_id,
                attempt,
            });
        }
    }
    let mut seen_fixlinks = BTreeMap::<RedKey, Vec<(PathBuf, usize, Option<String>)>>::new();
    for (key, path, line, derivation, proven_rerun_green) in fixlinks {
        let previous = seen_fixlinks.entry(key.clone()).or_default();
        let duplicate = previous.iter().any(|(_, _, prior_derivation)| {
            !allowed_fixlink_pair(prior_derivation.as_deref(), derivation.as_deref())
        });
        if let Some((previous_path, previous_line, _)) = previous.first()
            && duplicate
        {
            findings.push(finding(
                &path,
                &args.dir,
                line,
                "duplicate_fixlink",
                format!(
                    "same red run as {}:{}",
                    previous_path
                        .strip_prefix(&args.dir)
                        .unwrap_or(previous_path)
                        .display(),
                    previous_line
                ),
            ));
        }
        previous.push((path.clone(), line, derivation));
        // A rerun can legitimately have no cause row: GitHub may have
        // deleted the original failed job log before the archive pass, while
        // the later green rerun remains available as a fix-link. Keep the
        // stronger check for compare-derived links, which must point at an
        // archived red attempt.
        if !red_keys.contains(&key) && !proven_rerun_green {
            findings.push(finding(
                &path,
                &args.dir,
                line,
                "orphan_fixlink",
                "no archived cause has this red run and attempt",
            ));
        }
    }
    findings.sort();
    Ok(AuditReport {
        schema: SCHEMA,
        dir: args.dir.display().to_string(),
        files_scanned,
        cause_rows: causes.len(),
        fixlink_rows: seen_fixlinks.len(),
        ok: findings.is_empty(),
        findings,
    })
}

pub(super) fn is_proven_rerun_green(value: &Value) -> bool {
    let Some(head_sha) = value["head_sha"].as_str() else {
        return false;
    };
    let Some(fix_head_sha) = value["fix_head_sha"].as_str() else {
        return false;
    };
    value["derivation"].as_str() == Some("rerun_green")
        && value["fix_run_id"] == value["red_run_id"]
        && head_sha == fix_head_sha
        && head_sha.len() == 40
        && head_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        && value["files_raw"].as_array().is_some_and(Vec::is_empty)
        && value["own_files"].as_array().is_some_and(Vec::is_empty)
        && value["files_truncated"].as_bool() == Some(false)
        && value["own_files_truncated"].as_bool() == Some(false)
        && value["own_files_method"].as_str() == Some("rerun_same_head")
}

fn allowed_fixlink_pair(first: Option<&str>, second: Option<&str>) -> bool {
    matches!(
        (first, second),
        (Some("compare_red_head_to_fix_head"), Some("rerun_green"))
            | (Some("rerun_green"), Some("compare_red_head_to_fix_head"))
    )
}

pub(super) fn command<W: Write>(
    args: &CauseAuditArgs,
    json: bool,
    out: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let report = audit(args)?;
    if json {
        writeln!(
            out,
            "{}",
            serde_json::to_string(&report).unwrap_or_default()
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            out,
            "audit: {} file(s), {} cause row(s), {} fix-link(s), {} finding(s)",
            report.files_scanned,
            report.cause_rows,
            report.fixlink_rows,
            report.findings.len()
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for item in &report.findings {
            writeln!(
                out,
                "{}:{} {}: {}",
                item.file, item.line, item.kind, item.detail
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(std::process::ExitCode::from(u8::from(!report.ok)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn legacy_root_causes_are_not_reported_as_an_empty_clean_archive() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("causes")).unwrap();
        fs::write(dir.path().join("2026-10.jsonl"), cause(1)).unwrap();
        let Err(error) = read_archive(dir.path()) else {
            panic!("legacy archive must be refused");
        };
        assert_eq!(error.code, 2);
        assert!(error.message.contains("legacy cause archive files"));
    }

    fn cause(id: u64) -> String {
        serde_json::json!({
            "schema": cause::SCHEMA, "repo": "o/r", "run_id": id, "attempt": 1,
            "job_id": id + 100, "job": "linux", "failure_class": "test", "format": "ctest",
            "signature": "test_x", "confidence": "high", "source": "extractor", "extractor": "test",
            "retrospective": true, "kind": "test", "location": "", "evidence": {"log_line": 1, "excerpt": []}
        }).to_string()
    }

    fn fixlink(red_run_id: u64, derivation: &str) -> Value {
        serde_json::json!({
            "schema": cause::SCHEMA,
            "source": "fix-link",
            "repo": "o/r",
            "red_run_id": red_run_id,
            "red_attempt": 1,
            "fix_run_id": if derivation == "rerun_green" {
                red_run_id
            } else {
                red_run_id + 1
            },
            "head_sha": "a".repeat(40),
            "fix_head_sha": if derivation == "rerun_green" {
                "a".repeat(40)
            } else {
                "b".repeat(40)
            },
            "derivation": derivation,
            "red_time": "2026-10-13T00:00:00Z",
            "pushed_at": "2026-10-13T01:00:00Z",
            "branch": "main",
            "rule": "test",
            "collector": "test",
            "pr": null,
            "files_raw": [],
            "own_files": [],
            "files_truncated": false,
            "own_files_truncated": false,
            "own_files_method": if derivation == "rerun_green" {
                "rerun_same_head"
            } else {
                "first_parent_non_merge_commits"
            },
        })
    }

    #[test]
    fn reports_malformed_duplicate_and_orphan_rows_without_writing() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("causes")).expect("causes");
        fs::write(
            dir.path().join("causes/2026-10.jsonl"),
            format!("{}\n{}\nnot-json\n", cause(1), cause(1)),
        )
        .expect("causes file");
        let duplicate = fixlink(2, "compare_red_head_to_fix_head");
        fs::write(
            dir.path().join("fixlinks.jsonl"),
            format!("{duplicate}\n{duplicate}\n"),
        )
        .expect("fixlinks");
        let before = fs::read_dir(dir.path()).expect("before").count();
        let report = audit(&CauseAuditArgs {
            dir: dir.path().to_path_buf(),
        })
        .expect("audit");
        assert!(!report.ok);
        assert_eq!(
            report
                .findings
                .iter()
                .filter(|f| f.kind == "invalid_json")
                .count(),
            1
        );
        assert_eq!(
            report
                .findings
                .iter()
                .filter(|f| f.kind == "duplicate_cause")
                .count(),
            1
        );
        assert_eq!(
            report
                .findings
                .iter()
                .filter(|f| f.kind == "duplicate_fixlink")
                .count(),
            1
        );
        assert_eq!(
            report
                .findings
                .iter()
                .filter(|f| f.kind == "orphan_fixlink")
                .count(),
            2
        );
        // Readers never create lock files or otherwise mutate a legacy archive.
        assert_eq!(fs::read_dir(dir.path()).expect("after").count(), before);
    }

    #[test]
    fn clean_archive_is_stable_and_passes() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("causes")).expect("causes");
        fs::write(dir.path().join("causes/2026-10.jsonl"), cause(1)).expect("causes file");
        let report = audit(&CauseAuditArgs {
            dir: dir.path().to_path_buf(),
        })
        .expect("audit");
        assert!(report.ok);
        assert!(report.findings.is_empty());
        assert_eq!((report.cause_rows, report.fixlink_rows), (1, 0));
    }

    #[test]
    fn accepts_compare_and_rerun_fixlinks_for_one_red_attempt() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("causes")).expect("causes");
        fs::write(dir.path().join("causes/2026-10.jsonl"), cause(1)).expect("cause");
        let compare = fixlink(1, "compare_red_head_to_fix_head");
        let rerun = fixlink(1, "rerun_green");
        fs::write(
            dir.path().join("fixlinks.jsonl"),
            format!("{compare}\n{rerun}\n"),
        )
        .expect("fixlinks");
        let report = audit(&CauseAuditArgs {
            dir: dir.path().to_path_buf(),
        })
        .expect("audit");
        assert!(report.ok);
        assert_eq!((report.cause_rows, report.fixlink_rows), (1, 1));
    }

    #[test]
    fn accepts_rerun_fixlink_when_original_log_has_no_cause_row() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("fixlinks.jsonl"),
            r#"{"schema":"ci.cause/v1","source":"fix-link","repo":"o/r","red_run_id":2,"red_attempt":1,"derivation":"rerun_green","fix_run_id":2,"fix_head_sha":"0123456789abcdef0123456789abcdef01234567","head_sha":"0123456789abcdef0123456789abcdef01234567","red_time":"2026-10-13T00:00:00Z","pushed_at":"2026-10-13T00:30:00Z","branch":"main","rule":"test","collector":"test","pr":null,"files_raw":[],"own_files":[],"files_truncated":false,"own_files_truncated":false,"own_files_method":"rerun_same_head"}
"#,
        )
        .expect("fixlinks");
        let report = audit(&CauseAuditArgs {
            dir: dir.path().to_path_buf(),
        })
        .expect("audit");
        assert!(report.ok);
        assert_eq!((report.cause_rows, report.fixlink_rows), (0, 1));
    }

    #[test]
    fn rejects_unproven_orphan_rerun_fixlink() {
        let dir = tempdir().expect("tempdir");
        let mut unproven = fixlink(2, "rerun_green");
        unproven["fix_run_id"] = serde_json::json!(3);
        fs::write(dir.path().join("fixlinks.jsonl"), unproven.to_string()).expect("fixlinks");
        let report = audit(&CauseAuditArgs {
            dir: dir.path().to_path_buf(),
        })
        .expect("audit");
        assert!(!report.ok);
        assert_eq!(report.findings[0].kind, "orphan_fixlink");
    }
}
