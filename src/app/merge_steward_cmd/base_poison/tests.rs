use super::*;

#[test]
fn hold_and_jump_follow_the_base_config() {
    let parse = |text: &str, merge_queue: bool| {
        BasePoisonPolicy::from_table(&text.parse::<toml::Table>().expect("toml"), merge_queue)
    };
    let default = parse("", true);
    assert_eq!(default.hold, PoisonHold::DryRun);
    assert_eq!(default.auto_jump, AutoJump::Off);
    let on = parse(
        "[base_health]\nmerge_group_hold = \"on\"\nauto_jump = \"on\"\n",
        true,
    );
    assert_eq!((on.hold, on.auto_jump), (PoisonHold::On, AutoJump::On));
    // A typo never switches anything on: the hold stays dry-run, the jump off.
    let typo = parse(
        "[base_health]\nmerge_group_hold = \"yes\"\nauto_jump = \"onn\"\n",
        true,
    );
    assert_eq!(
        (typo.hold, typo.auto_jump),
        (PoisonHold::DryRun, AutoJump::Off)
    );
    assert!(typo.read_error.is_some());
    // A base without a merge queue forms no merge groups to read.
    assert_eq!(
        parse("[base_health]\nmerge_group_hold = \"on\"\n", false).hold,
        PoisonHold::Off
    );
}

#[test]
fn the_guards_carry_the_base_poison_policy() {
    let table = "[base_health]\nmerge_group_hold = \"on\"\n"
        .parse::<toml::Table>()
        .expect("toml");
    let guards = crate::merge_steward::EjectionGuards::from_table(&table, true);
    assert_eq!(guards.base_poison.hold, PoisonHold::On);
}

#[test]
fn a_priority_line_moves_first_once() {
    let dir = tempfile::tempdir().expect("temp");
    let file = dir.path().join("priority.txt");
    fs::write(&file, "o/r#1\no/r#2\n# comment\no/r#3\n").expect("seed");
    assert!(prioritize_line(&file, "o/r#3").expect("write"));
    assert_eq!(
        fs::read_to_string(&file).expect("read"),
        "o/r#3\no/r#1\no/r#2\n# comment\n"
    );
    assert!(!prioritize_line(&file, "o/r#3").expect("idempotent"));
}

#[cfg(unix)]
#[test]
fn a_review_kick_runs_once_per_head_and_retries_after_a_failure() {
    let dir = tempfile::tempdir().expect("temp");
    let log = dir.path().join("kicks");
    let script = dir.path().join("kick");
    crate::test_support::write_executable_script(
        &script,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n[ -e '{}' ] && exit 1\nexit 0\n",
            log.display(),
            dir.path().join("fail").display()
        ),
    );
    let hooks = ReviewHooks {
        kick: vec![script.display().to_string(), "reviewer".to_owned()],
        priority_file: Some(dir.path().join("priority.txt")),
    };
    let state = dir.path().join("state");
    let head_a = "a".repeat(40);
    let head_b = "b".repeat(40);
    let kicks = || fs::read_to_string(&log).unwrap_or_default().lines().count();
    prioritize_review(&hooks, &state, "o/r", 7, &head_a, "ctx");
    prioritize_review(&hooks, &state, "o/r", 7, &head_a, "ctx");
    assert_eq!(kicks(), 1, "one kick per head");
    assert!(
        fs::read_to_string(&log)
            .expect("log")
            .contains("reviewer base poisoned (ctx); fix o/r#7 at aaaaaaaaaaaa")
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("priority.txt")).expect("priority"),
        "o/r#7\n"
    );
    // A new head is a new review: kicked again.
    prioritize_review(&hooks, &state, "o/r", 7, &head_b, "ctx");
    assert_eq!(kicks(), 2);
    // A failed kick is not a kick made: the next pass retries it.
    fs::write(dir.path().join("fail"), "").expect("fail switch");
    let head_c = "c".repeat(40);
    let lines = prioritize_review(&hooks, &state, "o/r", 7, &head_c, "ctx");
    assert!(
        lines.iter().any(|line| line.contains("failed")),
        "{lines:?}"
    );
    fs::remove_file(dir.path().join("fail")).expect("unfail");
    prioritize_review(&hooks, &state, "o/r", 7, &head_c, "ctx");
    assert_eq!(kicks(), 4);
}

#[test]
fn review_hooks_are_read_from_the_machine_global_layer_only() {
    let dir = tempfile::tempdir().expect("temp");
    fs::write(
        dir.path().join("config.toml"),
        "[base_health]\nfix_review_kick = [\"fleet-pass-kick\", \"reviewer\"]\nfix_review_priority_file = \"/tmp/p.txt\"\n",
    )
    .expect("config");
    let hooks = ReviewHooks::from_machine_global(dir.path());
    assert_eq!(hooks.kick, vec!["fleet-pass-kick", "reviewer"]);
    assert_eq!(hooks.priority_file, Some(PathBuf::from("/tmp/p.txt")));
    // Control: an empty machine layer configures nothing.
    let empty = tempfile::tempdir().expect("temp");
    assert_eq!(
        ReviewHooks::from_machine_global(empty.path()),
        ReviewHooks::default()
    );
    for key in [FIX_REVIEW_KICK_KEY, FIX_REVIEW_PRIORITY_FILE_KEY] {
        assert!(crate::config::MACHINE_ONLY_KEYS.contains(&key), "{key}");
    }
}

#[test]
fn only_a_definitive_github_refusal_of_the_jump_is_retried_plainly() {
    let forbidden: Result<String, String> = Ok(
        r#"{"data":{"enqueuePullRequest":null},"errors":[{"type":"FORBIDDEN","message":"no jump"}]}"#
            .to_owned(),
    );
    assert!(jump_was_refused(&forbidden));
    let accepted: Result<String, String> =
        Ok(r#"{"data":{"enqueuePullRequest":{"mergeQueueEntry":{"position":1}}}}"#.to_owned());
    assert!(!jump_was_refused(&accepted));
    let transport: Result<String, String> = Err("gh: connection reset".to_owned());
    assert!(!jump_was_refused(&transport));
}

#[test]
fn a_dry_run_names_what_the_fast_track_would_do_and_nothing_when_clear() {
    let poison = RepoPoison {
        policy: BasePoisonPolicy::for_base(true),
        health: MergeGroupHealth::Poisoned {
            contexts: vec![crate::base_health::merge_group::PoisonedContext {
                context: "ctx".to_owned(),
                first_error: None,
                groups: Vec::new(),
                latest_failure_at: None,
                check_id: None,
            }],
        },
        fixes: BTreeMap::from([(7, vec!["label `shipyard:unblock`".to_owned()])]),
        fix_read_error: None,
    };
    assert_eq!(
        would_fast_track(&poison),
        vec!["#7 would fast-track (review first, CI to the front): label `shipyard:unblock`"]
    );
    let clear = RepoPoison {
        health: MergeGroupHealth::Clear {
            detail: String::new(),
        },
        ..poison
    };
    assert!(would_fast_track(&clear).is_empty());
}
