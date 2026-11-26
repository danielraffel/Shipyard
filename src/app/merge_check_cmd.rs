//! `shipyard merge-check`: prove a merge of the base into a PR branch did not
//! drop or alter base content outside the PR's own files.
//!
//! Rule, with no thresholds: given base `B`, pre-merge head `P` and post-merge
//! head `H`, `pr_files` is the PR's own change set before the merge
//! (`merge-base(B, P)..P`) and `drift` is `B..H`. The check passes iff
//! `drift` is a subset of `pr_files`.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode};

use serde_json::json;

use super::CliFailure;

pub(super) struct MergeCheckArgs {
    pub(super) base: String,
    pub(super) pre: Option<String>,
    pub(super) head: String,
}

#[derive(Debug, PartialEq, Eq)]
struct ExtraFile {
    path: String,
    added: Option<u64>,
    deleted: Option<u64>,
}

#[derive(Debug)]
struct Report {
    base: String,
    pre: String,
    head: String,
    merge_base: String,
    pr_file_count: usize,
    extra: Vec<ExtraFile>,
    collapsed: Vec<String>,
}

pub(super) fn merge_check_command<W: Write>(
    args: &MergeCheckArgs,
    cwd: &Path,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let report = analyze(cwd, args)?;
    render(&report, json_output, stdout)
}

fn git(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, CliFailure> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .map_err(|error| CliFailure::new(2, format!("could not run git: {error}")))?;
    if !output.status.success() {
        return Err(CliFailure::new(
            2,
            format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(output.stdout)
}

fn rev_parse(cwd: &Path, name: &str, spec: &str) -> Result<String, CliFailure> {
    let commit = format!("{spec}^{{commit}}");
    let out = git(cwd, &["rev-parse", "--verify", "--quiet", &commit])
        .map_err(|_| CliFailure::new(2, format!("{name} `{spec}` is not a commit")))?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

fn is_ancestor(cwd: &Path, ancestor: &str, descendant: &str) -> Result<bool, CliFailure> {
    let status = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .map_err(|error| CliFailure::new(2, format!("could not run git: {error}")))?;
    match status.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(CliFailure::new(
            2,
            format!(
                "git merge-base --is-ancestor failed: {}",
                String::from_utf8_lossy(&status.stderr).trim()
            ),
        )),
    }
}

fn analyze(cwd: &Path, args: &MergeCheckArgs) -> Result<Report, CliFailure> {
    let base = rev_parse(cwd, "base", &args.base)?;
    let head = rev_parse(cwd, "head", &args.head)?;
    let pre = match &args.pre {
        Some(pre) => rev_parse(cwd, "pre-merge head", pre)?,
        None => default_pre(cwd, &head, &base)?,
    };
    // `B..H` only means "drift" when H contains B. A base that moved on after
    // the merge would otherwise report its newer commits as dropped content.
    if !is_ancestor(cwd, &base, &head)? {
        return Err(CliFailure::new(
            2,
            format!(
                "head {head} does not contain base {base}; pass --base <the base commit that \
                 was merged>"
            ),
        ));
    }
    let merge_base = String::from_utf8_lossy(&git(cwd, &["merge-base", &base, &pre])?)
        .trim()
        .to_owned();
    if merge_base.is_empty() {
        return Err(CliFailure::new(
            2,
            "base and pre-merge head share no history",
        ));
    }
    let pr_files = pr_files(cwd, &merge_base, &pre)?;
    // Renames are not detected here: a rename is a deletion plus an addition,
    // and both paths are in `pr_files`.
    let drift = numstat(cwd, &base, &head)?;
    let extra = drift
        .iter()
        .filter(|file| !pr_files.contains(&file.path))
        .map(|file| ExtraFile {
            path: file.path.clone(),
            added: file.added,
            deleted: file.deleted,
        })
        .collect();
    let drifted: BTreeSet<&str> = drift.iter().map(|file| file.path.as_str()).collect();
    let collapsed = pr_files
        .iter()
        .filter(|path| !drifted.contains(path.as_str()))
        .cloned()
        .collect();
    Ok(Report {
        base,
        pre,
        head,
        merge_base,
        pr_file_count: pr_files.len(),
        extra,
        collapsed,
    })
}

/// `HEAD^1` when `head` is a merge whose second parent is the base tip.
fn default_pre(cwd: &Path, head: &str, base: &str) -> Result<String, CliFailure> {
    let out = git(cwd, &["rev-list", "--parents", "-n", "1", head])?;
    let line = String::from_utf8_lossy(&out);
    let parents: Vec<&str> = line.split_whitespace().skip(1).collect();
    if parents.len() == 2 && parents[1] == base {
        return Ok(parents[0].to_owned());
    }
    Err(CliFailure::new(
        2,
        "HEAD is not a merge commit whose second parent is the base tip; pass --pre <the \
         pre-merge head>",
    ))
}

/// Paths the PR changed before the merge, both sides of every rename.
fn pr_files(cwd: &Path, merge_base: &str, pre: &str) -> Result<BTreeSet<String>, CliFailure> {
    let out = git(cwd, &["diff", "--name-status", "-z", "-M", merge_base, pre])?;
    let mut fields = out
        .split(|byte| *byte == 0)
        .map(|field| String::from_utf8_lossy(field).into_owned());
    let mut files = BTreeSet::new();
    while let Some(status) = fields.next() {
        if status.is_empty() {
            continue;
        }
        let paths = if status.starts_with('R') || status.starts_with('C') {
            2
        } else {
            1
        };
        for _ in 0..paths {
            let path = fields
                .next()
                .ok_or_else(|| CliFailure::new(2, "unexpected end of git diff output"))?;
            files.insert(path);
        }
    }
    Ok(files)
}

fn numstat(cwd: &Path, from: &str, to: &str) -> Result<Vec<ExtraFile>, CliFailure> {
    let out = git(cwd, &["diff", "--numstat", "-z", "--no-renames", from, to])?;
    let mut files = Vec::new();
    for record in out.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(record);
        let mut parts = text.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(CliFailure::new(
                2,
                format!("unreadable git numstat record: {text}"),
            ));
        };
        files.push(ExtraFile {
            path: path.to_owned(),
            added: added.parse().ok(),
            deleted: deleted.parse().ok(),
        });
    }
    Ok(files)
}

fn lines(value: Option<u64>) -> String {
    value.map_or_else(|| "bin".to_owned(), |count| count.to_string())
}

fn render<W: Write>(
    report: &Report,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let pass = report.extra.is_empty();
    let io = |error: std::io::Error| CliFailure::new(2, format!("write failed: {error}"));
    if json_output {
        let value = json!({
            "pass": pass,
            "base": report.base,
            "pre": report.pre,
            "head": report.head,
            "merge_base": report.merge_base,
            "pr_file_count": report.pr_file_count,
            "extra": report.extra.iter().map(|file| json!({
                "path": file.path,
                "added": file.added,
                "deleted": file.deleted,
            })).collect::<Vec<_>>(),
            "collapsed_pr_files": report.collapsed,
        });
        writeln!(stdout, "{value}").map_err(io)?;
    } else {
        if pass {
            writeln!(
                stdout,
                "merge-check: PASS. {} left the base unchanged outside the PR's {} files.",
                &report.head[..report.head.len().min(12)],
                report.pr_file_count
            )
            .map_err(io)?;
        } else {
            writeln!(
                stdout,
                "merge-check: FAIL. {} files outside the PR's own change set differ from the \
                 base:",
                report.extra.len()
            )
            .map_err(io)?;
            writeln!(stdout, "{:>8} {:>8}  FILE", "+", "-").map_err(io)?;
            for file in &report.extra {
                writeln!(
                    stdout,
                    "{:>8} {:>8}  {}",
                    lines(file.added),
                    lines(file.deleted),
                    file.path
                )
                .map_err(io)?;
            }
            writeln!(
                stdout,
                "(+/- are lines the merge result adds to / removes from the base.)"
            )
            .map_err(io)?;
        }
        if !report.collapsed.is_empty() {
            writeln!(
                stdout,
                "warning: {} PR files now equal the base, so the PR's own change to them is \
                 gone. This is legitimate when the resolution took the base side or the base \
                 already carries the change:",
                report.collapsed.len()
            )
            .map_err(io)?;
            for path in &report.collapsed {
                writeln!(stdout, "  {path}").map_err(io)?;
            }
        }
    }
    Ok(if pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("run git");
        assert!(
            output.status.success() || args.first() == Some(&"merge"),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("write");
    }

    fn commit(dir: &Path, message: &str) {
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-q", "-m", message]);
    }

    /// A repo whose `main` has `base.txt`, `both.txt`, `old.txt`, with a PR
    /// branch `pr` forked from it. Returns the temp dir.
    fn fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        run(dir, &["init", "-q", "-b", "main"]);
        let ten = |tag: &str| {
            "0123456789"
                .chars()
                .map(|n| [tag, &n.to_string(), "\n"].concat())
                .collect::<String>()
        };
        write(dir, "base.txt", &ten("b"));
        write(dir, "both.txt", &ten("x"));
        write(dir, "old.txt", &ten("o"));
        write(dir, "pr.txt", &ten("p"));
        commit(dir, "root");
        run(dir, &["checkout", "-q", "-b", "pr"]);
        temp
    }

    fn args(pre: Option<&str>) -> MergeCheckArgs {
        MergeCheckArgs {
            base: "main".to_owned(),
            pre: pre.map(str::to_owned),
            head: "HEAD".to_owned(),
        }
    }

    fn check(dir: &Path, args: &MergeCheckArgs) -> (Result<ExitCode, CliFailure>, String) {
        let mut out = Vec::new();
        let result = merge_check_command(args, dir, true, &mut out);
        (result, String::from_utf8(out).expect("utf8"))
    }

    fn verdict(out: &str) -> serde_json::Value {
        serde_json::from_str(out).expect("json")
    }

    /// Land a base-only change on main, then return to `pr`.
    fn base_changes(dir: &Path, file: &str, body: &str) {
        run(dir, &["checkout", "-q", "main"]);
        write(dir, file, body);
        commit(dir, "base change");
        run(dir, &["checkout", "-q", "pr"]);
    }

    #[test]
    fn clean_merge_passes() {
        let temp = fixture();
        let dir = temp.path();
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        base_changes(dir, "base.txt", "base moved\n");
        run(dir, &["merge", "-q", "--no-ff", "-m", "merge", "main"]);
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::SUCCESS);
        let v = verdict(&out);
        assert_eq!(v["pass"], true);
        assert_eq!(v["extra"].as_array().expect("array").len(), 0);
    }

    #[test]
    fn resolution_dropping_a_base_only_change_fails() {
        let temp = fixture();
        let dir = temp.path();
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        base_changes(dir, "base.txt", "base moved\n");
        // `-s ours` records the merge but keeps the PR tree: base.txt is lost.
        run(
            dir,
            &[
                "merge", "-q", "--no-ff", "-s", "ours", "-m", "merge", "main",
            ],
        );
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::from(1));
        let v = verdict(&out);
        assert_eq!(v["pass"], false);
        assert_eq!(v["extra"][0]["path"], "base.txt");
        assert_eq!(v["extra"][0]["added"], 10);
        assert_eq!(v["extra"][0]["deleted"], 1);
    }

    #[test]
    fn a_file_both_sides_changed_passes() {
        let temp = fixture();
        let dir = temp.path();
        let original = std::fs::read_to_string(dir.join("both.txt")).expect("read");
        write(dir, "both.txt", &original.replacen("x0", "pr-edit", 1));
        commit(dir, "pr");
        base_changes(dir, "both.txt", &original.replacen("x9", "base-edit", 1));
        run(dir, &["merge", "-q", "--no-ff", "-m", "merge", "main"]);
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::SUCCESS);
        assert_eq!(verdict(&out)["pass"], true);
    }

    #[test]
    fn a_pr_file_collapsed_to_base_warns_but_passes() {
        let temp = fixture();
        let dir = temp.path();
        write(dir, "pr.txt", "pr side\n");
        write(dir, "other.txt", "kept\n");
        commit(dir, "pr");
        base_changes(dir, "pr.txt", "base side\n");
        run(dir, &["merge", "--no-ff", "-m", "merge", "main"]);
        run(dir, &["checkout", "--theirs", "pr.txt"]);
        commit(dir, "merge");
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::SUCCESS);
        let v = verdict(&out);
        assert_eq!(v["pass"], true);
        assert_eq!(v["collapsed_pr_files"], json!(["pr.txt"]));
    }

    #[test]
    fn non_merge_head_without_pre_is_an_error() {
        let temp = fixture();
        let dir = temp.path();
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        let (result, _) = check(dir, &args(None));
        assert_eq!(result.expect_err("error").code, 2);
    }

    #[test]
    fn an_explicit_pre_works_on_a_non_merge_head() {
        let temp = fixture();
        let dir = temp.path();
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        let pre = run(dir, &["rev-parse", "HEAD"]);
        base_changes(dir, "base.txt", "base moved\n");
        run(dir, &["merge", "-q", "--no-ff", "-m", "merge", "main"]);
        // A follow-up commit makes HEAD a non-merge commit that still has the
        // base as an ancestor.
        write(dir, "pr.txt", "changed again\n");
        commit(dir, "follow-up");
        let (result, out) = check(dir, &args(Some(&pre)));
        assert_eq!(result.expect("ok"), ExitCode::SUCCESS, "{out}");
    }

    #[test]
    fn a_base_that_moved_after_the_merge_is_an_error_not_a_false_failure() {
        let temp = fixture();
        let dir = temp.path();
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        base_changes(dir, "base.txt", "base moved\n");
        let pre = run(dir, &["rev-parse", "HEAD"]);
        run(dir, &["merge", "-q", "--no-ff", "-m", "merge", "main"]);
        base_changes(dir, "base.txt", "base moved again\n");
        let (result, _) = check(dir, &args(Some(&pre)));
        assert_eq!(result.expect_err("error").code, 2);
    }

    #[test]
    fn a_pr_rename_is_part_of_the_pr_change_set() {
        let temp = fixture();
        let dir = temp.path();
        run(dir, &["mv", "old.txt", "renamed.txt"]);
        commit(dir, "rename");
        base_changes(dir, "base.txt", "base moved\n");
        run(dir, &["merge", "-q", "--no-ff", "-m", "merge", "main"]);
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::SUCCESS, "{out}");
    }

    #[test]
    fn dropping_a_base_change_is_still_caught_beside_a_rename() {
        let temp = fixture();
        let dir = temp.path();
        run(dir, &["mv", "old.txt", "renamed.txt"]);
        commit(dir, "rename");
        base_changes(dir, "base.txt", "base moved\n");
        run(
            dir,
            &[
                "merge", "-q", "--no-ff", "-s", "ours", "-m", "merge", "main",
            ],
        );
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::from(1));
        let v = verdict(&out);
        let paths: Vec<_> = v["extra"]
            .as_array()
            .expect("array")
            .iter()
            .map(|file| file["path"].as_str().expect("path"))
            .collect();
        assert_eq!(paths, ["base.txt"]);
    }

    #[test]
    fn human_output_lists_extra_files_in_a_table() {
        let report = Report {
            base: "a".repeat(40),
            pre: "b".repeat(40),
            head: "c".repeat(40),
            merge_base: "d".repeat(40),
            pr_file_count: 1,
            extra: vec![ExtraFile {
                path: "src/x.rs".to_owned(),
                added: Some(3),
                deleted: Some(1800),
            }],
            collapsed: vec![],
        };
        let mut out = Vec::new();
        assert_eq!(
            render(&report, false, &mut out).expect("render"),
            ExitCode::from(1)
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("1800  src/x.rs"), "{text}");
    }
}
