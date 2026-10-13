use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use super::{
    CheckoutState, FLEET_SIBLINGS, Finding, Operation, STALE_AFTER, checkouts, entry, examine,
    git_unmerged, remedy, remedy_chain, shell_quote,
};

const LATER: Duration = Duration::from_hours(2);

/// A primary checkout with an empty `.git` directory: enough for the marker
/// reads, with the unmerged read supplied by the test.
fn checkout() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join(".git")).expect(".git");
    fs::write(dir.path().join(".git/index"), b"").expect("index");
    dir
}

#[allow(clippy::unnecessary_wraps)] // the signature `examine` takes
fn none(_: &Path) -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

fn later() -> SystemTime {
    SystemTime::now() + LATER
}

fn stuck(state: CheckoutState) -> Vec<Finding> {
    match state {
        CheckoutState::Stuck(findings) => findings,
        other => panic!("expected stuck, got {other:?}"),
    }
}

#[test]
fn each_marker_names_its_operation_and_abort() {
    for (marker, is_dir, operation, abort) in [
        (
            "CHERRY_PICK_HEAD",
            false,
            Operation::CherryPick,
            "cherry-pick --abort",
        ),
        ("MERGE_HEAD", false, Operation::Merge, "merge --abort"),
        ("REVERT_HEAD", false, Operation::Revert, "revert --abort"),
        ("rebase-merge", true, Operation::Rebase, "rebase --abort"),
        ("rebase-apply", true, Operation::Rebase, "rebase --abort"),
    ] {
        let dir = checkout();
        let path = dir.path().join(".git").join(marker);
        if is_dir {
            fs::create_dir(&path).expect("marker dir");
        } else {
            fs::write(&path, "0123\n").expect("marker");
        }
        let findings = stuck(examine(dir.path(), later(), none));
        assert_eq!(findings.len(), 1, "{marker}: {findings:?}");
        assert_eq!(findings[0].operation, operation, "{marker}");
        assert_eq!(findings[0].marker, marker);
        assert!(findings[0].age >= Duration::from_mins(119), "{marker}");
        let commands = remedy(dir.path(), &findings[0], "stamp");
        assert_eq!(
            commands.last().map(String::as_str),
            Some(
                format!(
                    "git -C {} {abort}",
                    shell_quote(&dir.path().display().to_string())
                )
                .as_str()
            ),
            "{marker}: the abort is the last command"
        );
    }
}

#[test]
fn a_rebase_apply_from_am_is_aborted_with_am() {
    let dir = checkout();
    fs::create_dir_all(dir.path().join(".git/rebase-apply")).expect("rebase-apply");
    fs::write(dir.path().join(".git/rebase-apply/applying"), b"").expect("applying");
    let findings = stuck(examine(dir.path(), later(), none));
    assert_eq!(findings[0].operation, Operation::Am);
    assert!(
        remedy(dir.path(), &findings[0], "s")
            .last()
            .is_some_and(|command| command.ends_with(" am --abort"))
    );
}

/// The backup comes first: a branch at HEAD, the working tree's patch, the
/// status, and a copy of the operation's own state, before any abort.
#[test]
fn the_backup_precedes_the_abort() {
    let dir = checkout();
    fs::write(dir.path().join(".git/CHERRY_PICK_HEAD"), "abc\n").expect("marker");
    let findings = stuck(examine(dir.path(), later(), none));
    let commands = remedy(dir.path(), &findings[0], "20261013T1200");
    let position = |needle: &str| {
        commands
            .iter()
            .position(|command| command.contains(needle))
            .unwrap_or_else(|| panic!("{needle} missing from {commands:#?}"))
    };
    let abort = position("cherry-pick --abort");
    assert_eq!(abort, commands.len() - 1);
    for backup in [
        "branch shipyard-backup/cherry-pick-20261013T1200 HEAD",
        "diff --binary HEAD >",
        "status --porcelain=v2 >",
        "/.git/CHERRY_PICK_HEAD",
        "MERGE_MSG",
    ] {
        assert!(
            position(backup) < abort,
            "{backup} must come before the abort"
        );
    }
}

#[test]
fn an_operation_under_an_hour_old_is_not_reported() {
    let dir = checkout();
    fs::write(dir.path().join(".git/MERGE_HEAD"), "abc\n").expect("marker");
    assert_eq!(STALE_AFTER, Duration::from_hours(1));
    let now = SystemTime::now() + Duration::from_mins(55);
    assert_eq!(
        examine(dir.path(), now, none),
        CheckoutState::Clean { recent: 1 }
    );
    // Control: the same marker past the threshold is reported.
    let now = SystemTime::now() + STALE_AFTER + Duration::from_mins(5);
    assert_eq!(stuck(examine(dir.path(), now, none)).len(), 1);
}

#[test]
fn a_clean_checkout_is_clean() {
    let dir = checkout();
    assert_eq!(
        examine(dir.path(), later(), none),
        CheckoutState::Clean { recent: 0 }
    );
}

/// Conflicted entries with no operation marker are still reported, aged by
/// the index, and listed by path.
#[test]
fn unmerged_entries_without_a_marker_are_reported() {
    let dir = checkout();
    let findings = stuck(examine(dir.path(), later(), |_| {
        Ok(vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()])
    }));
    assert_eq!(findings[0].operation, Operation::UnmergedEntries);
    assert_eq!(findings[0].unmerged, ["src/a.rs", "src/b.rs"]);
    let commands = remedy(dir.path(), &findings[0], "s");
    assert!(
        commands
            .last()
            .is_some_and(|c| c.ends_with(" reset --merge"))
    );
    assert!(commands.iter().any(|c| c.contains("diff --binary HEAD >")));
}

/// A checkout that cannot be read is never reported clean.
#[test]
fn an_unreadable_checkout_is_not_clean() {
    let missing = tempfile::tempdir().expect("tempdir");
    assert!(matches!(
        examine(missing.path(), later(), none),
        CheckoutState::Unreadable(_)
    ));
    let dir = checkout();
    let state = examine(dir.path(), later(), |_| Err("git failed".to_owned()));
    assert_eq!(state, CheckoutState::Unreadable("git failed".to_owned()));
    let rendered = entry(dir.path(), &state, "s");
    assert!(!rendered.ok);
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .is_ok_and(|output| output.status.success())
}

/// A real cherry-pick stopped on a conflict: found with its conflicted path,
/// rendered with the backup and abort commands, and left in place.
#[test]
fn a_real_conflicted_cherry_pick_is_reported_and_left_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    assert!(git(repo, &["init", "-q", "-b", "main"]), "git init");
    fs::write(repo.join("file.txt"), "base\n").expect("write");
    assert!(git(repo, &["add", "file.txt"]) && git(repo, &["commit", "-qm", "base"]));
    assert!(git(repo, &["checkout", "-qb", "side"]));
    fs::write(repo.join("file.txt"), "side\n").expect("write");
    assert!(git(repo, &["commit", "-qam", "side"]));
    assert!(git(repo, &["checkout", "-q", "main"]));
    fs::write(repo.join("file.txt"), "main\n").expect("write");
    assert!(git(repo, &["commit", "-qam", "main"]));
    assert!(
        !git(repo, &["cherry-pick", "side"]),
        "the pick must conflict"
    );
    assert!(repo.join(".git/CHERRY_PICK_HEAD").exists(), "control");

    let state = examine(repo, later(), git_unmerged);
    let findings = stuck(state.clone());
    assert_eq!(findings[0].operation, Operation::CherryPick);
    assert_eq!(findings[0].unmerged, ["file.txt"]);
    let rendered = entry(repo, &state, "s");
    assert!(!rendered.ok);
    assert_eq!(
        rendered.error.as_deref(),
        Some("cherry-pick left in progress")
    );
    let detail = rendered.detail.expect("detail");
    assert!(detail.contains("unmerged: file.txt"), "{detail}");
    assert!(detail.contains("cherry-pick --abort"), "{detail}");
    // Report only: the operation is still in progress afterwards.
    assert!(repo.join(".git/CHERRY_PICK_HEAD").exists());
    assert!(!git_unmerged(repo).expect("read").is_empty());
}

/// The fleet siblings of the current primary checkout are examined when they
/// exist; configured paths are added once each, `~` expanded.
#[test]
fn checkouts_include_existing_siblings_and_configured_paths() {
    let root = tempfile::tempdir().expect("tempdir");
    let current = root.path().join("Shipyard");
    fs::create_dir(&current).expect("current");
    assert!(git(&current, &["init", "-q"]));
    fs::create_dir_all(root.path().join("pulp/.git")).expect("pulp");
    fs::create_dir_all(root.path().join("not-fleet/.git")).expect("other");
    let home = root.path().join("home");
    fs::create_dir_all(home.join("extra/.git")).expect("extra");
    let found = checkouts(
        &current,
        &["~/extra".to_owned(), current.display().to_string()],
        &home,
    );
    let canonical = |path: &Path| fs::canonicalize(path).expect("canonical");
    let found: Vec<_> = found.iter().map(|path| canonical(path)).collect();
    assert!(
        found.contains(&canonical(&root.path().join("pulp"))),
        "{found:?}"
    );
    assert!(found.contains(&canonical(&current)), "{found:?}");
    assert!(found.contains(&canonical(&home.join("extra"))), "{found:?}");
    assert!(!found.contains(&canonical(&root.path().join("not-fleet"))));
    assert_eq!(found.len(), 3, "each once: {found:?}");
    assert!(FLEET_SIBLINGS.contains(&"vellum"));
}

/// The block is one `&&` chain ending in the abort, so a failed backup step
/// stops it before the abort; optional copies never fail the chain.
#[test]
fn the_remedy_is_one_chain_ending_in_the_abort() {
    let dir = checkout();
    fs::write(dir.path().join(".git/CHERRY_PICK_HEAD"), "abc\n").expect("marker");
    let findings = stuck(examine(dir.path(), later(), none));
    let chain = remedy_chain(dir.path(), &findings[0], "s");
    let lines: Vec<&str> = chain.lines().collect();
    assert!(lines.len() > 3, "{chain}");
    let (last, rest) = lines.split_last().expect("lines");
    for line in rest {
        assert!(
            line.ends_with(" &&"),
            "every step but the last chains: {line}"
        );
        assert!(
            !line.contains("] &&"),
            "an optional copy must not fail the chain: {line}"
        );
    }
    assert!(
        last.trim_start().ends_with("cherry-pick --abort"),
        "{chain}"
    );
    assert_eq!(chain.matches("--abort").count(), 1, "{chain}");
    assert!(chain.contains("{ [ ! -e "), "{chain}");
    let rendered = entry(dir.path(), &CheckoutState::Stuck(findings), "s")
        .detail
        .expect("detail");
    assert!(rendered.contains(&chain), "{rendered}");
    assert!(rendered.contains("POSIX shell"), "{rendered}");
}

/// The checkout's name is reduced to a safe file name in the backup path.
#[test]
fn the_backup_directory_name_is_sanitised() {
    let root = tempfile::tempdir().expect("tempdir");
    let odd = root.path().join("my repo;$(x)");
    fs::create_dir_all(odd.join(".git")).expect(".git");
    fs::write(odd.join(".git/MERGE_HEAD"), "abc\n").expect("marker");
    let findings = stuck(examine(&odd, later(), none));
    let commands = remedy(&odd, &findings[0], "s");
    assert_eq!(
        commands[0],
        "mkdir -p \"$HOME/.shipyard-git-backups/my_repo___x_-s\""
    );
}

/// Only a marker that does not exist is absent; one that cannot be read
/// makes the checkout unreadable, never clean.
#[cfg(unix)]
#[test]
fn an_unreadable_marker_is_not_absent() {
    use std::os::unix::fs::PermissionsExt;
    let dir = checkout();
    let git_dir = dir.path().join(".git");
    fs::write(git_dir.join("MERGE_HEAD"), "abc\n").expect("marker");
    fs::set_permissions(&git_dir, fs::Permissions::from_mode(0o600)).expect("chmod");
    let state = examine(dir.path(), later(), none);
    fs::set_permissions(&git_dir, fs::Permissions::from_mode(0o755)).expect("restore");
    assert!(matches!(state, CheckoutState::Unreadable(_)), "{state:?}");
}

/// The chain is executed, not only read: when a backup step fails, sh stops
/// before the abort. `&&` and `||` group left to right in sh, so an ungrouped
/// optional copy would reopen the chain and let the abort run.
#[cfg(unix)]
#[test]
fn a_failed_backup_step_stops_the_chain_before_the_abort() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).expect("repo");
    assert!(git(&repo, &["init", "-q", "-b", "main"]), "git init");
    fs::write(repo.join("file.txt"), "base\n").expect("write");
    assert!(git(&repo, &["add", "file.txt"]) && git(&repo, &["commit", "-qm", "base"]));
    fs::write(repo.join(".git/CHERRY_PICK_HEAD"), "abc\n").expect("marker");
    fs::write(repo.join(".git/MERGE_MSG"), "msg\n").expect("message");
    // The backup branch already exists, so `git branch` fails.
    assert!(git(
        &repo,
        &["branch", "shipyard-backup/cherry-pick-s", "HEAD"]
    ));
    let findings = stuck(examine(&repo, later(), none));
    let home = dir.path().join("home");
    fs::create_dir(&home).expect("home");
    let marker = dir.path().join("ABORTED");
    // The abort is replaced by a trace, so the test aborts nothing itself.
    let chain = remedy_chain(&repo, &findings[0], "s").replace(
        &format!(
            "git -C {} cherry-pick --abort",
            shell_quote(&repo.display().to_string())
        ),
        &format!("touch {}", shell_quote(&marker.display().to_string())),
    );
    assert!(chain.ends_with(&format!(
        "touch {}",
        shell_quote(&marker.display().to_string())
    )));
    let run = |chain: &str| {
        Command::new("sh")
            .args(["-c", chain])
            .env("HOME", &home)
            .output()
            .expect("sh")
    };
    let failed = run(&chain);
    assert!(!failed.status.success(), "the chain must fail");
    assert!(
        !marker.exists(),
        "the abort ran after a failed backup step: {chain}"
    );
    assert!(repo.join(".git/CHERRY_PICK_HEAD").exists());
    assert!(repo.join(".git/MERGE_MSG").exists());
    // Control: with the backup branch gone, the same chain runs to the end.
    assert!(git(
        &repo,
        &["branch", "-D", "shipyard-backup/cherry-pick-s"]
    ));
    let passed = run(&chain);
    assert!(
        passed.status.success(),
        "{}",
        String::from_utf8_lossy(&passed.stderr)
    );
    assert!(marker.exists(), "the control chain must reach the abort");
}
