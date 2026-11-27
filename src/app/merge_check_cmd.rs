//! `shipyard merge-check`: prove a merge of the base into a PR branch did not
//! drop or alter base content outside the PR's own files.
//!
//! Rule, with no thresholds: given base `B`, pre-merge head `P` and post-merge
//! head `H`, `pr_files` is the PR's own change set before the merge
//! (`merge-base(B, P)..P`) and `drift` is `B..H`. Every drifted file outside
//! `pr_files` is classified. It is a `base_loss` (exit 1) when the head lacks
//! an occurrence of a line the base added, reverts the base's whole change,
//! reverts a base mode change, or differs from a binary file the base
//! changed. Otherwise it is a `merge_only_edit`: a warning, and the check
//! still passes. Files inside `pr_files` are not checked line by line, so a
//! base change inside a file the PR also touched is the PR author's to
//! resolve; a PR file that now equals the base is reported as collapsed.

use std::collections::{BTreeMap, BTreeSet};
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
    /// The base's change to this file was dropped by the merge.
    loss: bool,
    /// How many of the base's added lines are missing from the head.
    missing: u64,
    /// The first few of those lines, in the order the base added them.
    missing_sample: Vec<String>,
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
        Some(spec) => {
            let pre = rev_parse(cwd, "pre-merge head", spec)?;
            // An unrelated or post-merge ref would pick the wrong merge base
            // and exempt the wrong files.
            // A ref that already contains the base (the merge itself or a
            // later commit) would put the base's own changes in `pr_files`.
            if pre == head || !is_ancestor(cwd, &pre, &head)? || is_ancestor(cwd, &base, &pre)? {
                return Err(CliFailure::new(
                    2,
                    format!(
                        "pre-merge head `{spec}` must be a proper ancestor of head {head} \
                         that does not already contain base {base}"
                    ),
                ));
            }
            pre
        }
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
        .map(|file| {
            let (loss, missing, missing_sample) =
                classify(cwd, &merge_base, &base, &head, &file.path)?;
            Ok(ExtraFile {
                path: file.path.clone(),
                added: file.added,
                deleted: file.deleted,
                loss,
                missing,
                missing_sample,
            })
        })
        .collect::<Result<Vec<_>, CliFailure>>()?;
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
    let out = git(
        cwd,
        &[
            "diff",
            "--no-relative",
            "--name-status",
            "-z",
            "-M",
            merge_base,
            pre,
        ],
    )?;
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
    let out = git(
        cwd,
        &[
            "diff",
            "--no-relative",
            "--numstat",
            "-z",
            "--no-renames",
            from,
            to,
        ],
    )?;
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
            loss: false,
            missing: 0,
            missing_sample: Vec::new(),
        });
    }
    Ok(files)
}

/// The `(mode, object id)` of `path` at `rev`, or `None` when the path does
/// not exist there. A git failure is an error, never an absent path.
fn entry(cwd: &Path, rev: &str, path: &str) -> Result<Option<(String, String)>, CliFailure> {
    let out = git(cwd, &["ls-tree", "-z", "--full-tree", rev, "--", path])?;
    for record in out.split(|byte| *byte == 0) {
        let text = String::from_utf8_lossy(record);
        let Some((meta, name)) = text.split_once('\t') else {
            continue;
        };
        if name != path {
            continue;
        }
        let mut fields = meta.split(' ');
        let (Some(mode), Some(_kind), Some(oid)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(CliFailure::new(
                2,
                format!("unreadable git ls-tree record: {text}"),
            ));
        };
        return Ok(Some((mode.to_owned(), oid.to_owned())));
    }
    Ok(None)
}

fn count_lines(content: &[u8]) -> BTreeMap<Vec<u8>, u64> {
    let mut counts: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    for line in content.split(|byte| *byte == b'\n') {
        *counts.entry(line.to_vec()).or_default() += 1;
    }
    counts
}

/// The lines a `-U0` diff adds, in order, and whether git reported it as a
/// binary change. Only hunk bodies count, so an added line whose content
/// starts with `++` is not mistaken for the `+++` file header.
fn added_lines(diff: &[u8]) -> (Vec<Vec<u8>>, bool) {
    let mut added = Vec::new();
    let mut binary = false;
    let mut in_hunk = false;
    for line in diff.split(|byte| *byte == b'\n') {
        if line.starts_with(b"diff --git ") {
            in_hunk = false;
        } else if line.starts_with(b"@@") {
            in_hunk = true;
        } else if !in_hunk {
            binary |= line.starts_with(b"Binary files ");
        } else if line.first() == Some(&b'+') {
            added.push(line[1..].to_vec());
        }
    }
    (added, binary)
}

/// Decide whether the merge dropped the base's change to a file the PR did
/// not touch. Returns `(loss, missing_lines, sample)`.
///
/// Loss: the head's file has fewer occurrences of a line the base added than
/// the base's file has (compared as a line multiset, so reordering is fine and
/// a pre-existing copy cannot stand in for a new one); the head's file is the
/// merge base's version while the base changed it (a full revert); the head
/// reverts a base mode change; or the base changed a binary file and the head
/// differs from it (binary content cannot be shown to be kept). Otherwise the
/// head keeps everything the base added and the difference is an edit made
/// inside the merge commit.
fn classify(
    cwd: &Path,
    merge_base: &str,
    base: &str,
    head: &str,
    path: &str,
) -> Result<(bool, u64, Vec<String>), CliFailure> {
    let base_entry = entry(cwd, base, path)?;
    let merge_base_entry = entry(cwd, merge_base, path)?;
    let head_entry = entry(cwd, head, path)?;
    let oid = |e: &Option<(String, String)>| e.as_ref().map(|(_, oid)| oid.clone());
    let mode = |e: &Option<(String, String)>| e.as_ref().map(|(mode, _)| mode.clone());
    if oid(&head_entry) == oid(&merge_base_entry) && oid(&base_entry) != oid(&merge_base_entry) {
        return Ok((true, 0, Vec::new()));
    }
    // The head deleted a file the base changed (content or mode only).
    if head_entry.is_none() && base_entry.is_some() && base_entry != merge_base_entry {
        return Ok((true, 0, Vec::new()));
    }
    if base_entry.is_some()
        && head_entry.is_some()
        && mode(&base_entry) != mode(&merge_base_entry)
        && mode(&head_entry) == mode(&merge_base_entry)
    {
        return Ok((true, 0, Vec::new()));
    }
    let diff = git(
        cwd,
        &[
            // A plain patch whatever the user's config: color, an external
            // diff or a textconv filter would hide the `@@` / `+` prefixes.
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-relative",
            "--no-renames",
            "-U0",
            merge_base,
            base,
            "--",
            // Paths come from the repository root; `cwd` may be a subdirectory.
            &format!(":(top,literal){path}"),
        ],
    )?;
    let (added, binary) = added_lines(&diff);
    if binary && oid(&head_entry) != oid(&base_entry) {
        return Ok((true, 0, Vec::new()));
    }
    let base_counts = match &base_entry {
        Some(_) => count_lines(&git(cwd, &["cat-file", "blob", &format!("{base}:{path}")])?),
        None => BTreeMap::new(),
    };
    let head_counts = match &head_entry {
        Some(_) => count_lines(&git(cwd, &["cat-file", "blob", &format!("{head}:{path}")])?),
        None => BTreeMap::new(),
    };
    // For each line the base added, the head must hold as many occurrences as
    // the base does; a shortfall (capped at what the base added) is missing.
    let mut added_counts: BTreeMap<&[u8], u64> = BTreeMap::new();
    for line in &added {
        *added_counts.entry(line.as_slice()).or_default() += 1;
    }
    let mut shortfall: BTreeMap<&[u8], u64> = BTreeMap::new();
    for (line, count) in &added_counts {
        let want = base_counts.get(*line).copied().unwrap_or(0);
        let have = head_counts.get(*line).copied().unwrap_or(0);
        let short = want.saturating_sub(have).min(*count);
        if short > 0 {
            shortfall.insert(line, short);
        }
    }
    let mut missing = 0;
    let mut sample = Vec::new();
    for line in &added {
        if let Some(short) = shortfall.get_mut(line.as_slice())
            && *short > 0
        {
            *short -= 1;
            missing += 1;
            if sample.len() < 5 {
                sample.push(String::from_utf8_lossy(line).into_owned());
            }
        }
    }
    Ok((missing > 0, missing, sample))
}

fn lines(value: Option<u64>) -> String {
    value.map_or_else(|| "bin".to_owned(), |count| count.to_string())
}

/// Name the lines main added that the merge lost or changed, and the remedy.
fn render_loss_detail<W: Write>(file: &ExtraFile, stdout: &mut W) -> Result<(), CliFailure> {
    let io = |error: std::io::Error| CliFailure::new(2, format!("write failed: {error}"));
    if file.missing_sample.is_empty() {
        return Ok(());
    }
    writeln!(
        stdout,
        "\n{}: {} lines main added are missing or changed, first {}:",
        file.path,
        file.missing,
        file.missing_sample.len()
    )
    .map_err(io)?;
    for line in &file.missing_sample {
        writeln!(stdout, "  + {line}").map_err(io)?;
    }
    writeln!(
        stdout,
        "Fix: redo the merge taking main's version of {}. If changing it is intended, make \
         that edit as a separate commit after the merge so review sees it.",
        file.path
    )
    .map_err(io)?;
    Ok(())
}

fn render<W: Write>(
    report: &Report,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let losses = report.extra.iter().filter(|file| file.loss).count();
    let pass = losses == 0;
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
                "class": if file.loss { "base_loss" } else { "merge_only_edit" },
                "missing_base_lines": file.missing,
                "missing_sample": file.missing_sample,
            })).collect::<Vec<_>>(),
            "collapsed_pr_files": report.collapsed,
        });
        writeln!(stdout, "{value}").map_err(io)?;
    } else {
        if pass {
            writeln!(
                stdout,
                "merge-check: PASS. {} kept all base content outside the PR's {} files.",
                &report.head[..report.head.len().min(12)],
                report.pr_file_count
            )
            .map_err(io)?;
        } else {
            writeln!(
                stdout,
                "merge-check: FAIL. {losses} files outside the PR's own change set lost base \
                 content in the merge:"
            )
            .map_err(io)?;
        }
        let table = |stdout: &mut W, loss: bool| -> Result<(), CliFailure> {
            writeln!(stdout, "{:>8} {:>8} {:>8}  FILE", "+", "-", "MISSING").map_err(io)?;
            for file in report.extra.iter().filter(|file| file.loss == loss) {
                writeln!(
                    stdout,
                    "{:>8} {:>8} {:>8}  {}",
                    lines(file.added),
                    lines(file.deleted),
                    file.missing,
                    file.path
                )
                .map_err(io)?;
            }
            Ok(())
        };
        if losses > 0 {
            table(stdout, true)?;
            writeln!(
                stdout,
                "(+/- are lines the merge result adds to / removes from the base; MISSING is \
                 lines the base added that the merge result lacks.)"
            )
            .map_err(io)?;
            for file in report.extra.iter().filter(|file| file.loss) {
                render_loss_detail(file, stdout)?;
            }
            writeln!(stdout).map_err(io)?;
        }
        let edits = report.extra.len() - losses;
        if edits > 0 {
            writeln!(
                stdout,
                "warning: {edits} files outside the PR's change set carry edits made in the \
                 merge commit; every line the base added is kept:"
            )
            .map_err(io)?;
            table(stdout, false)?;
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

    fn base_original(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("base.txt")).expect("read")
    }

    #[test]
    fn restoring_the_old_version_of_an_untouched_file_is_a_base_loss() {
        let temp = fixture();
        let dir = temp.path();
        let original = base_original(dir);
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        base_changes(dir, "base.txt", &format!("{original}n1\nn2\n"));
        run(dir, &["merge", "--no-commit", "--no-ff", "main"]);
        write(dir, "base.txt", &original);
        commit(dir, "merge");
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::from(1));
        let v = verdict(&out);
        assert_eq!(v["extra"][0]["class"], "base_loss");
    }

    #[test]
    fn reverting_a_base_change_that_only_removed_lines_is_a_base_loss() {
        let temp = fixture();
        let dir = temp.path();
        let original = base_original(dir);
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        base_changes(dir, "base.txt", "b0\n");
        run(dir, &["merge", "--no-commit", "--no-ff", "main"]);
        write(dir, "base.txt", &original);
        commit(dir, "merge");
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::from(1), "{out}");
        assert_eq!(verdict(&out)["extra"][0]["class"], "base_loss");
    }

    #[test]
    fn a_partial_base_loss_fails() {
        let temp = fixture();
        let dir = temp.path();
        let original = base_original(dir);
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        base_changes(dir, "base.txt", &format!("{original}n1\nn2\nn3\n"));
        run(dir, &["merge", "--no-commit", "--no-ff", "main"]);
        // Keep one of the three lines main added, in a different place.
        write(dir, "base.txt", &format!("n2\n{original}"));
        commit(dir, "merge");
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::from(1));
        let v = verdict(&out);
        assert_eq!(v["pass"], false);
        assert_eq!(v["extra"][0]["missing_base_lines"], 2);
    }

    #[test]
    fn an_edit_inside_the_merge_commit_warns_and_passes() {
        let temp = fixture();
        let dir = temp.path();
        let original = base_original(dir);
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        base_changes(dir, "base.txt", &format!("{original}n1\n"));
        run(dir, &["merge", "--no-commit", "--no-ff", "main"]);
        // The author's own addition, committed with the merge; reordered too.
        write(dir, "base.txt", &format!("extra\nn1\n{original}"));
        commit(dir, "merge");
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::SUCCESS, "{out}");
        let v = verdict(&out);
        assert_eq!(v["pass"], true);
        assert_eq!(v["extra"][0]["class"], "merge_only_edit");
        assert_eq!(v["extra"][0]["missing_base_lines"], 0);
        let mut human = Vec::new();
        merge_check_command(&args(None), dir, false, &mut human).expect("ok");
        assert!(String::from_utf8_lossy(&human).contains("warning: 1 files"));
    }

    #[test]
    fn a_rewrite_of_main_lines_names_the_file_the_lines_and_the_remedy() {
        let temp = fixture();
        let dir = temp.path();
        let original = base_original(dir);
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        base_changes(dir, "base.txt", &format!("{original}n1\nn2\n"));
        run(dir, &["merge", "--no-commit", "--no-ff", "main"]);
        // The merge commit rewrites one of main's lines instead of deleting it.
        write(dir, "base.txt", &format!("{original}n1\nn2-rewritten\n"));
        commit(dir, "merge");
        let mut out = Vec::new();
        let code = merge_check_command(&args(None), dir, false, &mut out).expect("ok");
        assert_eq!(code, ExitCode::from(1));
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("base.txt: 1 lines main added"), "{text}");
        assert!(text.contains("  + n2\n"), "{text}");
        assert!(
            text.contains("redo the merge taking main's version of base.txt"),
            "{text}"
        );
        assert!(text.contains("separate commit after the merge"), "{text}");
    }

    #[test]
    fn a_pre_existing_copy_does_not_mask_a_lost_base_added_line() {
        // 1692-r2-f1: merge base `shared old`, base adds a second `shared`,
        // the merge drops it and adds `extra`.
        let temp = fixture();
        let dir = temp.path();
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        run(dir, &["checkout", "-q", "main"]);
        write(dir, "dup.txt", "shared\nold\n");
        commit(dir, "dup root");
        run(dir, &["checkout", "-q", "pr"]);
        run(dir, &["merge", "-q", "--no-ff", "-m", "sync", "main"]);
        base_changes(dir, "dup.txt", "shared\nold\nshared\n");
        run(dir, &["merge", "--no-commit", "--no-ff", "main"]);
        write(dir, "dup.txt", "shared\nold\nextra\n");
        commit(dir, "merge");
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::from(1), "{out}");
        let v = verdict(&out);
        assert_eq!(v["extra"][0]["path"], "dup.txt");
        assert_eq!(v["extra"][0]["class"], "base_loss");
        assert_eq!(v["extra"][0]["missing_base_lines"], 1);
        assert_eq!(v["extra"][0]["missing_sample"], json!(["shared"]));
    }

    #[test]
    fn an_added_line_starting_with_plus_plus_is_counted() {
        // 1692-r2-f2: `++counter;` is hunk content, not a `+++` header.
        let temp = fixture();
        let dir = temp.path();
        let original = base_original(dir);
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        base_changes(dir, "base.txt", &format!("{original}++counter;\n"));
        run(dir, &["merge", "--no-commit", "--no-ff", "main"]);
        write(dir, "base.txt", &format!("{original}extra\n"));
        commit(dir, "merge");
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::from(1), "{out}");
        assert_eq!(
            verdict(&out)["extra"][0]["missing_sample"],
            json!(["++counter;"])
        );
    }

    #[test]
    fn a_pre_that_is_not_an_ancestor_of_the_head_is_an_error() {
        // 1692-r2-f4
        let temp = fixture();
        let dir = temp.path();
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        base_changes(dir, "base.txt", "base moved\n");
        run(dir, &["merge", "-q", "--no-ff", "-m", "merge", "main"]);
        let head = run(dir, &["rev-parse", "HEAD"]);
        run(dir, &["checkout", "-q", "-b", "unrelated", "main~1"]);
        write(dir, "other.txt", "elsewhere\n");
        commit(dir, "unrelated");
        let unrelated = run(dir, &["rev-parse", "HEAD"]);
        run(dir, &["checkout", "-q", "pr"]);
        for pre in [unrelated.as_str(), head.as_str()] {
            let (result, _) = check(dir, &args(Some(pre)));
            assert_eq!(result.expect_err("error").code, 2, "pre {pre}");
        }
    }

    #[test]
    fn unreadable_evidence_is_an_error_not_an_absent_file() {
        // 1692-r2-f5: a missing object must not read as "file absent".
        let temp = fixture();
        let dir = temp.path();
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        base_changes(dir, "base.txt", "base moved\n");
        run(
            dir,
            &[
                "merge", "-q", "--no-ff", "-s", "ours", "-m", "merge", "main",
            ],
        );
        let tree = run(dir, &["rev-parse", "main^{tree}"]);
        let object = dir.join(".git/objects").join(&tree[..2]).join(&tree[2..]);
        std::fs::remove_file(object).expect("remove tree object");
        let merge_base = run(dir, &["merge-base", "main", "HEAD^1"]);
        let head = run(dir, &["rev-parse", "HEAD"]);
        let main = run(dir, &["rev-parse", "main"]);
        let result = classify(dir, &merge_base, &main, &head, "base.txt");
        assert_eq!(result.expect_err("error").code, 2);
    }

    #[cfg(unix)]
    fn set_mode(dir: &Path, name: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(mode))
            .expect("chmod");
    }

    #[cfg(unix)]
    #[test]
    fn reverting_a_base_mode_change_is_a_base_loss() {
        // 1692-r2-f6
        let temp = fixture();
        let dir = temp.path();
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        run(dir, &["checkout", "-q", "main"]);
        set_mode(dir, "base.txt", 0o755);
        commit(dir, "make executable");
        run(dir, &["checkout", "-q", "pr"]);
        run(dir, &["merge", "--no-commit", "--no-ff", "main"]);
        set_mode(dir, "base.txt", 0o644);
        commit(dir, "merge");
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::from(1), "{out}");
        assert_eq!(verdict(&out)["extra"][0]["class"], "base_loss");
    }

    #[test]
    fn a_partially_lost_binary_change_is_a_base_loss() {
        // 1692-r2-f6
        let temp = fixture();
        let dir = temp.path();
        std::fs::write(dir.join("blob.bin"), b"\x00\x01root\x00").expect("write");
        commit(dir, "binary");
        run(dir, &["checkout", "-q", "main"]);
        run(dir, &["merge", "-q", "--no-ff", "-m", "sync", "pr"]);
        run(dir, &["checkout", "-q", "pr"]);
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        run(dir, &["checkout", "-q", "main"]);
        std::fs::write(dir.join("blob.bin"), b"\x00\x01base\x00").expect("write");
        commit(dir, "base binary");
        run(dir, &["checkout", "-q", "pr"]);
        run(dir, &["merge", "--no-commit", "--no-ff", "main"]);
        std::fs::write(dir.join("blob.bin"), b"\x00\x01other\x00").expect("write");
        commit(dir, "merge");
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::from(1), "{out}");
        assert_eq!(verdict(&out)["extra"][0]["path"], "blob.bin");
        assert_eq!(verdict(&out)["extra"][0]["class"], "base_loss");
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_file_whose_base_change_was_mode_only_is_a_base_loss() {
        let temp = fixture();
        let dir = temp.path();
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        run(dir, &["checkout", "-q", "main"]);
        set_mode(dir, "base.txt", 0o755);
        commit(dir, "make executable");
        run(dir, &["checkout", "-q", "pr"]);
        run(dir, &["merge", "--no-commit", "--no-ff", "main"]);
        run(dir, &["rm", "-q", "-f", "base.txt"]);
        commit(dir, "merge");
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::from(1), "{out}");
        assert_eq!(verdict(&out)["extra"][0]["class"], "base_loss");
    }

    #[test]
    fn colored_diff_config_does_not_hide_a_partial_loss() {
        let temp = fixture();
        let dir = temp.path();
        run(dir, &["config", "color.ui", "always"]);
        run(dir, &["config", "color.diff", "always"]);
        let original = base_original(dir);
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        base_changes(dir, "base.txt", &format!("{original}n1\nn2\n"));
        run(dir, &["merge", "--no-commit", "--no-ff", "main"]);
        write(dir, "base.txt", &format!("{original}n1\n"));
        commit(dir, "merge");
        let (result, out) = check(dir, &args(None));
        assert_eq!(result.expect("ok"), ExitCode::from(1), "{out}");
        assert_eq!(verdict(&out)["extra"][0]["missing_base_lines"], 1);
    }

    #[test]
    fn a_partial_loss_is_caught_from_a_subdirectory() {
        let temp = fixture();
        let dir = temp.path();
        std::fs::create_dir(dir.join("sub")).expect("mkdir");
        write(dir, "sub/keep.txt", "k\n");
        write(dir, "sub/*.txt", "glob\n");
        commit(dir, "sub");
        run(dir, &["checkout", "-q", "main"]);
        run(dir, &["merge", "-q", "--no-ff", "-m", "sync", "pr"]);
        run(dir, &["checkout", "-q", "pr"]);
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        run(dir, &["checkout", "-q", "main"]);
        write(dir, "sub/keep.txt", "k\nn1\nn2\n");
        write(dir, "sub/*.txt", "glob\ng1\n");
        commit(dir, "base change");
        run(dir, &["checkout", "-q", "pr"]);
        run(dir, &["merge", "--no-commit", "--no-ff", "main"]);
        write(dir, "sub/keep.txt", "k\nn1\nextra\n");
        write(dir, "sub/*.txt", "glob\nother\n");
        commit(dir, "merge");
        let (result, out) = check(&dir.join("sub"), &args(None));
        assert_eq!(result.expect("ok"), ExitCode::from(1), "{out}");
        let v = verdict(&out);
        let missing: BTreeMap<String, u64> = v["extra"]
            .as_array()
            .expect("array")
            .iter()
            .map(|file| {
                (
                    file["path"].as_str().expect("path").to_owned(),
                    file["missing_base_lines"].as_u64().expect("count"),
                )
            })
            .collect();
        assert_eq!(missing.get("sub/keep.txt"), Some(&1), "{out}");
        assert_eq!(missing.get("sub/*.txt"), Some(&1), "{out}");
    }

    #[test]
    fn a_post_merge_pre_is_an_error() {
        let temp = fixture();
        let dir = temp.path();
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        base_changes(dir, "base.txt", "base moved\n");
        run(
            dir,
            &[
                "merge", "-q", "--no-ff", "-s", "ours", "-m", "merge", "main",
            ],
        );
        let merge = run(dir, &["rev-parse", "HEAD"]);
        write(dir, "pr.txt", "follow-up\n");
        commit(dir, "follow-up");
        let (result, _) = check(dir, &args(Some(&merge)));
        assert_eq!(result.expect_err("error").code, 2);
    }

    #[test]
    fn relative_diff_config_does_not_hide_a_loss_outside_the_subdirectory() {
        let temp = fixture();
        let dir = temp.path();
        run(dir, &["config", "diff.relative", "true"]);
        std::fs::create_dir(dir.join("sub")).expect("mkdir");
        write(dir, "sub/keep.txt", "k\n");
        commit(dir, "sub");
        write(dir, "pr.txt", "changed\n");
        commit(dir, "pr");
        base_changes(dir, "base.txt", "base moved\n");
        run(
            dir,
            &[
                "merge", "-q", "--no-ff", "-s", "ours", "-m", "merge", "main",
            ],
        );
        let (result, out) = check(&dir.join("sub"), &args(None));
        assert_eq!(result.expect("ok"), ExitCode::from(1), "{out}");
        assert_eq!(verdict(&out)["extra"][0]["path"], "base.txt");
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
                loss: true,
                missing: 7,
                missing_sample: vec!["x".to_owned()],
            }],
            collapsed: vec![],
        };
        let mut out = Vec::new();
        assert_eq!(
            render(&report, false, &mut out).expect("render"),
            ExitCode::from(1)
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("1800        7  src/x.rs"), "{text}");
    }
}
