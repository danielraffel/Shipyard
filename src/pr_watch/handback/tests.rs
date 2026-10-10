use std::cell::RefCell;
use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone as _, Utc};
use serde_json::{Value, json};

use super::host::{
    self, HostCommand, HostRunner, INBOX_SCRIPT, Invocation, Liveness, RunError, SHOWN_SCRIPT,
    check_argv, parse_sessions, shell_quote, shell_unquote,
};
use super::owner::{self, Owner, OwnerSource, Route};
use super::{HandbackConfig, HandbackMode, HandbackReport, NEEDS_AGENT_LABEL, actionable};
use crate::pr_watch::flags::{DigestRoute, FlagKind};
use crate::pr_watch::ledger::{Ledger, LedgerEntry};
use crate::pr_watch::{PrHistory, RepoHistory};

const CMUX: &str = host::DEFAULT_CMUX_PATH;
const LIVE_SESSION: &str = "cfc73f94-128a-4c3d-8e69-2f278ff4fd8b";
const LIVE_SURFACE: &str = "ED34849F-E1AB-4B9A-8C0D-318B087A1C44";
const LIVE_WORKSPACE: &str = "8B62DB84-E8DD-468E-ACCB-0689A92A0335";
const DEAD_SESSION: &str = "08fb81d8-38a3-4b20-8bcc-c34ba2a66ffb";
const LOCAL_SESSION: &str = "e9172b3a-7bfc-481f-8298-bb851a741c21";

fn t(hour: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 29, 0, 0, 0).unwrap() + Duration::hours(hour)
}

fn marker(host: &str, session: &str, surface: &str) -> String {
    format!(
        "Summary\n\n<!-- whence {{\"labels\": [\"1·claude\"], \"prov\": {{\"agent\": \"claude\", \"host\": \"{host}\", \"terminal\": \"cmux\", \"terminal_address\": \"{surface}\", \"session\": \"{session}\", \"path\": \"/w/pulp-x\", \"resume\": \"claude \\u002d\\u002dresume {session}\"}}}} -->\n\n---\n### Provenance\n<!-- /whence -->"
    )
}

// ---- owner lookup -------------------------------------------------------------

#[test]
fn whence_marker_parses_host_session_surface_and_resume() {
    let owner = owner::parse_whence(&marker(
        "m3",
        LIVE_SESSION,
        &LIVE_SURFACE.to_ascii_lowercase(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(owner.source, OwnerSource::Whence);
    assert_eq!(owner.agent, "claude");
    assert_eq!(owner.host.as_deref(), Some("m3"));
    assert_eq!(owner.session, LIVE_SESSION);
    assert_eq!(owner.surface.as_deref(), Some(LIVE_SURFACE));
    assert_eq!(
        owner.resume.as_deref(),
        Some(format!("claude --resume {LIVE_SESSION}").as_str())
    );
    assert_eq!(owner.path.as_deref(), Some("/w/pulp-x"));
}

#[test]
fn a_body_without_a_marker_has_no_owner() {
    assert_eq!(owner::parse_whence("plain body").unwrap(), None);
    assert_eq!(owner::parse_whence("").unwrap(), None);
}

#[test]
fn malformed_markers_are_errors_not_owners() {
    for body in [
        "<!-- whence {not json} -->",
        "<!-- whence {\"prov\": {\"session\": \"x\"}",
        "<!-- whence {\"labels\": []} -->",
        "<!-- whence {\"prov\": {\"host\": \"m3\"}} -->",
        "<!-- whence {\"prov\": {\"session\": \"abc; rm -rf ~\"}} -->",
        "<!-- whence {\"prov\": {\"session\": \"-oProxyCommand=x\"}} -->",
        "<!-- whence {\"prov\": {\"session\": \"ok-1\", \"terminal_address\": \"not-a-uuid\"}} -->",
    ] {
        assert!(owner::parse_whence(body).is_err(), "accepted {body:?}");
    }
}

fn write_steward(state: &std::path::Path, pr: u64, head: &str, origin: &str, session: &str) {
    let dir = state
        .join("merge-steward/handoffs")
        .join(crate::required_check_policy::encode_path_segment("o/r"))
        .join(format!("pr-{pr}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{head}.json")),
        json!({"schema_version": 1, "repo": "o/r", "pr": pr, "head_sha": head,
               "origin_machine": origin, "phase": "managed",
               "agent_route": {"route_id": "route-abc", "provider": "codex"}})
        .to_string(),
    )
    .unwrap();
    let routes = state.join("merge-steward/agent-routes");
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::write(
        routes.join("route-abc.json"),
        json!({"agent": {"provider": "codex", "session_id": session,
                         "surface_id": LIVE_SURFACE}})
        .to_string(),
    )
    .unwrap();
}

#[test]
fn steward_record_wins_and_borrows_the_whence_host_for_the_same_session() {
    let dir = tempfile::tempdir().unwrap();
    let head = "a".repeat(40);
    write_steward(dir.path(), 7, &head, "machine-other", LIVE_SESSION);
    let steward = owner::steward_owner(dir.path(), "o/r", 7, &head, Some("machine-me")).unwrap();
    assert_eq!(steward.source, OwnerSource::Steward);
    assert!(!steward.local);
    assert_eq!(steward.agent, "codex");
    // Wrong head: no record.
    assert!(owner::steward_owner(dir.path(), "o/r", 7, &"b".repeat(40), None).is_none());
    // Local identity match.
    assert!(
        owner::steward_owner(dir.path(), "o/r", 7, &head, Some("machine-other"))
            .unwrap()
            .local
    );
    let whence = owner::parse_whence(&marker("m3", LIVE_SESSION, LIVE_SURFACE)).unwrap();
    let merged = owner::resolve(Some(steward.clone()), whence).unwrap();
    assert_eq!(merged.source, OwnerSource::Steward);
    assert_eq!(merged.host.as_deref(), Some("m3"));
    assert!(merged.resume.is_some());
    // A marker naming another session lends nothing.
    let other = owner::parse_whence(&marker("m5", DEAD_SESSION, LIVE_SURFACE)).unwrap();
    let kept = owner::resolve(Some(steward), other).unwrap();
    assert_eq!(kept.session, LIVE_SESSION);
    assert_eq!(kept.host, None);
    // No steward: the marker.
    let only = owner::parse_whence(&marker("m5", DEAD_SESSION, LIVE_SURFACE)).unwrap();
    assert_eq!(
        owner::resolve(None, only).unwrap().source,
        OwnerSource::Whence
    );
    assert_eq!(owner::resolve(None, None), None);
}

fn owner_on(host: &str) -> Owner {
    Owner {
        source: OwnerSource::Whence,
        agent: "claude".to_owned(),
        host: Some(host.to_owned()),
        local: false,
        session: LIVE_SESSION.to_owned(),
        surface: None,
        resume: None,
        path: None,
    }
}

#[test]
fn hosts_route_through_the_configured_map_only() {
    let hosts: BTreeMap<String, String> = [
        ("Daniels-Mac-Studio-m3", "m3"),
        ("m5s", "local"),
        ("evil", "-oProxyCommand=touch /tmp/x"),
    ]
    .into_iter()
    .map(|(a, b)| (a.to_owned(), b.to_owned()))
    .collect();
    let local = vec!["Daniels-M5-Studio".to_owned()];
    assert_eq!(
        owner::route_for(&owner_on("daniels-mac-studio-M3"), &hosts, &local),
        Ok(Route::Ssh("m3".to_owned()))
    );
    assert_eq!(
        owner::route_for(&owner_on("m5s"), &hosts, &local),
        Ok(Route::Local)
    );
    assert_eq!(
        owner::route_for(&owner_on("Daniels-M5-Studio"), &hosts, &local),
        Ok(Route::Local)
    );
    assert!(owner::route_for(&owner_on("BlackBook-Pro"), &hosts, &local).is_err());
    assert!(owner::route_for(&owner_on("evil"), &hosts, &local).is_err());
    let mut unknown = owner_on("m3");
    unknown.host = None;
    assert!(owner::route_for(&unknown, &hosts, &local).is_err());
}

// ---- liveness -----------------------------------------------------------------

fn sessions_json(rows: &[(&str, &str, bool, &str)]) -> String {
    let rows: Vec<_> = rows
        .iter()
        .map(|(session, lifecycle, pid, surface)| {
            json!({"session_id": session, "agent_lifecycle": lifecycle,
                   "stored_pid_exists": pid, "surface_id": surface,
                   "workspace_id": LIVE_WORKSPACE})
        })
        .collect();
    json!({"sessions": rows}).to_string()
}

#[test]
fn liveness_needs_a_running_or_idle_record_with_a_live_pid() {
    let live = sessions_json(&[(LIVE_SESSION, "running", true, LIVE_SURFACE)]);
    assert_eq!(
        parse_sessions(&live, LIVE_SESSION, None),
        Liveness::Live {
            surface: Some(LIVE_SURFACE.to_owned()),
            workspace: Some(LIVE_WORKSPACE.to_owned())
        }
    );
    let idle = sessions_json(&[(LIVE_SESSION, "idle", true, LIVE_SURFACE)]);
    assert_eq!(
        parse_sessions(&idle, LIVE_SESSION, None),
        Liveness::Live {
            surface: Some(LIVE_SURFACE.to_owned()),
            workspace: Some(LIVE_WORKSPACE.to_owned())
        }
    );
    let idle_gone_pid = sessions_json(&[(LIVE_SESSION, "idle", false, LIVE_SURFACE)]);
    assert_eq!(
        parse_sessions(&idle_gone_pid, LIVE_SESSION, None).name(),
        "dead"
    );
    let gone_pid = sessions_json(&[(LIVE_SESSION, "running", false, LIVE_SURFACE)]);
    assert_eq!(parse_sessions(&gone_pid, LIVE_SESSION, None).name(), "dead");
    let ended = sessions_json(&[(LIVE_SESSION, "ended", true, LIVE_SURFACE)]);
    assert_eq!(parse_sessions(&ended, LIVE_SESSION, None).name(), "dead");
    // Another session's live row does not count.
    let other = sessions_json(&[(DEAD_SESSION, "running", true, LIVE_SURFACE)]);
    assert_eq!(parse_sessions(&other, LIVE_SESSION, None).name(), "unknown");
    assert_eq!(
        parse_sessions("not json", LIVE_SESSION, None).name(),
        "unknown"
    );
    assert_eq!(parse_sessions("{}", LIVE_SESSION, None).name(), "unknown");
    let corroborated = json!({
        "sessions": [],
        "process_alive": true,
        "transcript_mtime": "2026-10-07T02:03:04Z"
    })
    .to_string();
    assert_eq!(
        parse_sessions(&corroborated, LIVE_SESSION, None).name(),
        "live"
    );
    // The expected surface wins among live rows.
    let second = "11111111-2222-3333-4444-555555555555";
    let two = sessions_json(&[
        (LIVE_SESSION, "running", true, second),
        (LIVE_SESSION, "running", true, LIVE_SURFACE),
    ]);
    let Liveness::Live { surface, .. } = parse_sessions(&two, LIVE_SESSION, Some(LIVE_SURFACE))
    else {
        panic!("not live")
    };
    assert_eq!(surface.as_deref(), Some(LIVE_SURFACE));
}

// ---- argv allowlist -------------------------------------------------------------

fn every_command() -> Vec<HostCommand> {
    vec![
        HostCommand::SessionsList {
            session: LIVE_SESSION.to_owned(),
        },
        HostCommand::Notify {
            surface: LIVE_SURFACE.to_owned(),
            title: "Shipyard: PR #1 needs you".to_owned(),
            body: "it's `red`; $(whoami) \"quoted\"\nline".to_owned(),
        },
        HostCommand::SetStatus {
            workspace: LIVE_WORKSPACE.to_owned(),
            key: "shipyard-pr-1".to_owned(),
            value: "PR #1 red".to_owned(),
        },
        HostCommand::ClearStatus {
            workspace: LIVE_WORKSPACE.to_owned(),
            key: "shipyard-pr-1".to_owned(),
        },
        HostCommand::InboxAppend {
            session: LIVE_SESSION.to_owned(),
        },
        HostCommand::LivenessEvidence {
            session: LIVE_SESSION.to_owned(),
            transcript: "/work/transcript.jsonl".to_owned(),
        },
    ]
}

#[test]
fn every_command_the_builder_can_make_passes_the_allowlist() {
    for route in [Route::Local, Route::Ssh("m3".to_owned())] {
        for command in every_command() {
            let Some(invocation) = host::invocation(&route, &command, CMUX, None) else {
                assert!(matches!(
                    (&route, &command),
                    (
                        Route::Local,
                        HostCommand::InboxAppend { .. } | HostCommand::LivenessEvidence { .. }
                    )
                ));
                continue;
            };
            check_argv(&invocation.argv, CMUX)
                .unwrap_or_else(|e| panic!("{e}: {:?}", invocation.argv));
        }
    }
}

#[test]
fn missing_cmux_row_has_a_real_remote_process_and_transcript_probe() {
    let invocation = host::invocation(
        &Route::Ssh("m3".to_owned()),
        &HostCommand::LivenessEvidence {
            session: LIVE_SESSION.to_owned(),
            transcript: "/work/transcript.jsonl".to_owned(),
        },
        CMUX,
        None,
    )
    .expect("ssh evidence probe");
    let command = invocation.argv.join(" ");
    assert!(command.contains("process_alive"));
    assert!(command.contains("transcript.jsonl"));
    check_argv(&invocation.argv, CMUX).expect("evidence probe allowlist");
}

#[test]
fn quoting_round_trips_and_refuses_bare_shell_syntax() {
    let words = vec![
        "a".to_owned(),
        "it's".to_owned(),
        String::new(),
        "$(x); y".to_owned(),
    ];
    let line = words
        .iter()
        .map(|w| shell_quote(w))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(shell_unquote(&line).unwrap(), words);
    for bad in ["'a' ; 'b'", "a", "'a'|'b'", "'a' $(b)", "'unterminated"] {
        assert!(shell_unquote(bad).is_err(), "{bad}");
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn ssh(remote: &str) -> Vec<String> {
    strings(&[
        "ssh",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "--",
        "m3",
        remote,
    ])
}

#[test]
fn forbidden_commands_never_pass_the_allowlist() {
    let quoted = |words: &[&str]| {
        words
            .iter()
            .map(|w| shell_quote(w))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let forbidden: Vec<Vec<String>> = vec![
        strings(&[CMUX, "send", "--surface", LIVE_SURFACE, "fix it"]),
        strings(&[CMUX, "send-key", "--surface", LIVE_SURFACE, "enter"]),
        strings(&[
            "cmux",
            "notify",
            "--surface",
            LIVE_SURFACE,
            "--title",
            "t",
            "--body",
            "b",
        ]),
        strings(&[CMUX, "surface", "resume", "get", "--surface", LIVE_SURFACE]),
        strings(&[
            CMUX,
            "sessions",
            "list",
            "--json",
            "--session",
            "x; rm -rf ~",
        ]),
        strings(&[
            CMUX,
            "set-status",
            "claude_code",
            "x",
            "--workspace",
            LIVE_WORKSPACE,
        ]),
        strings(&["claude", "--resume", LIVE_SESSION]),
        strings(&["codex", "exec", "resume", LIVE_SESSION, "-"]),
        strings(&["gh", "pr", "merge", "1"]),
        strings(&["sh", "-c", INBOX_SCRIPT, "sh", LIVE_SESSION]),
        ssh(&quoted(&[CMUX, "send", "--surface", LIVE_SURFACE, "hi"])),
        ssh(&quoted(&["claude", "--resume", LIVE_SESSION, "--bg"])),
        ssh(&quoted(&["sh", "-c", "rm -rf ~", "sh", LIVE_SESSION])),
        ssh(&quoted(&[
            "sh",
            "-c",
            INBOX_SCRIPT,
            "sh",
            "../../etc/passwd",
        ])),
        ssh(&format!("{} ; reboot", quoted(&[CMUX, "sessions", "list"]))),
        strings(&[
            "ssh",
            "-o",
            "ProxyCommand=x",
            "-o",
            "ConnectTimeout=10",
            "--",
            "m3",
            "'true'",
        ]),
        strings(&[
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "--",
            "-oProxyCommand=x",
            "'true'",
        ]),
        strings(&["ssh", "m3", "true"]),
        vec![],
    ];
    for argv in forbidden {
        assert!(check_argv(&argv, CMUX).is_err(), "allowed {argv:?}");
    }
}

// ---- the pass -----------------------------------------------------------------

fn entry(pr: u64, kind: FlagKind, route: DigestRoute, first_seen: DateTime<Utc>) -> LedgerEntry {
    LedgerEntry {
        pr,
        kind,
        key: "macos|some-test".to_owned(),
        title: format!("PR {pr}"),
        url: format!("https://github.com/o/r/pull/{pr}"),
        verdict: "code failure, not flake".to_owned(),
        evidence: "`some-test` failed on 2 runs".to_owned(),
        head_sha: format!("{pr:0>40}"),
        first_seen_at: first_seen,
        last_seen_at: first_seen,
        addressed_at: None,
        addressed_reason: None,
        digested_at: None,
        route,
        shared_tests: Vec::new(),
        related_prs: Vec::new(),
    }
}

fn open_pr(number: u64, labels: &[&str]) -> PrHistory {
    PrHistory {
        number,
        title: format!("PR {number}"),
        url: format!("https://github.com/o/r/pull/{number}"),
        created_at: Some(t(-48)),
        head_sha: format!("{number:0>40}"),
        labels: labels.iter().map(|l| (*l).to_owned()).collect(),
        timeline_complete: true,
        ..PrHistory::default()
    }
}

/// PR 1: owner live on m3. PR 2: owner dead on m3. PR 3: shared failure
/// (not actionable). PR 4: neighbour ejection (comment only). PR 5: owner
/// live on this host. PR 6: no marker.
fn world(first_seen: DateTime<Utc>) -> (Ledger, RepoHistory) {
    let mut ledger = Ledger::new("o/r", "main");
    let mut add = |id: &str, e: LedgerEntry| {
        ledger.entries.insert(id.to_owned(), e);
    };
    add(
        "1:repeat_test_failure:k",
        entry(
            1,
            FlagKind::RepeatTestFailure,
            DigestRoute::PerPr,
            first_seen,
        ),
    );
    add(
        "2:red_while_armed:k",
        entry(2, FlagKind::RedWhileArmed, DigestRoute::PerPr, first_seen),
    );
    add(
        "3:repeat_test_failure:k",
        entry(
            3,
            FlagKind::RepeatTestFailure,
            DigestRoute::Shared,
            first_seen,
        ),
    );
    add(
        "4:repeated_ejection:",
        entry(
            4,
            FlagKind::RepeatedEjection,
            DigestRoute::CommentOnly,
            first_seen,
        ),
    );
    add(
        "4:rebase_treadmill:",
        entry(4, FlagKind::RebaseTreadmill, DigestRoute::PerPr, first_seen),
    );
    add(
        "5:repeated_ejection:",
        entry(
            5,
            FlagKind::RepeatedEjection,
            DigestRoute::PerPr,
            first_seen,
        ),
    );
    add(
        "6:red_while_armed:k",
        entry(6, FlagKind::RedWhileArmed, DigestRoute::PerPr, first_seen),
    );
    let mut history = RepoHistory {
        repo: "o/r".to_owned(),
        base: "main".to_owned(),
        ..RepoHistory::default()
    };
    for pr in 1..=6 {
        history.prs.insert(pr, open_pr(pr, &[]));
    }
    (ledger, history)
}

fn fake_gh(label_exists: bool) -> impl Fn(&[String]) -> Result<String, String> {
    move |argv: &[String]| {
        let path = argv
            .iter()
            .find(|a| a.starts_with("repos/"))
            .cloned()
            .unwrap_or_default();
        if path.starts_with("repos/o/r/labels/") {
            return if label_exists {
                Ok(json!({"name": NEEDS_AGENT_LABEL}).to_string())
            } else {
                Err("gh: Not Found (HTTP 404)".to_owned())
            };
        }
        let body = match path.as_str() {
            "repos/o/r/pulls/1" | "repos/o/r/pulls/3" | "repos/o/r/pulls/4" => {
                marker("Daniels-Mac-Studio-m3", LIVE_SESSION, LIVE_SURFACE)
            }
            "repos/o/r/pulls/2" => marker("m3", DEAD_SESSION, LIVE_SURFACE),
            "repos/o/r/pulls/5" => marker("m5s", LOCAL_SESSION, LIVE_SURFACE),
            "repos/o/r/pulls/6" => "no marker here".to_owned(),
            _ => return Err(format!("unexpected read {argv:?}")),
        };
        Ok(json!({"body": body, "state": "open", "merged_at": null}).to_string())
    }
}

#[derive(Default)]
struct FakeRunner {
    runs: Vec<Invocation>,
    local_inbox: Vec<(String, String)>,
    fail_notify: bool,
    /// What each session's `<session>.shown.jsonl` tail reads as.
    shown: BTreeMap<String, String>,
    local_shown_reads: Vec<String>,
    omit_cmux_rows: bool,
}

impl HostRunner for FakeRunner {
    fn run(&mut self, invocation: &Invocation) -> Result<String, RunError> {
        check_argv(&invocation.argv, CMUX).map_err(RunError::Failed)?;
        self.runs.push(invocation.clone());
        let joined = invocation.argv.join(" ");
        if joined.contains("process_alive") {
            let process_alive = !joined.contains(DEAD_SESSION);
            return Ok(json!({
                "sessions": [],
                "process_alive": process_alive,
                "transcript_mtime": "2026-10-07T02:03:04Z"
            })
            .to_string());
        }
        if joined.contains("sessions") {
            let rows = if self.omit_cmux_rows {
                vec![]
            } else {
                vec![
                    (LIVE_SESSION, "running", true, LIVE_SURFACE),
                    (LOCAL_SESSION, "running", true, LIVE_SURFACE),
                    (DEAD_SESSION, "running", false, LIVE_SURFACE),
                ]
            };
            return Ok(sessions_json(&rows));
        }
        if self.fail_notify && joined.contains("notify") {
            return Err(RunError::Failed("socket".to_owned()));
        }
        if joined.contains(".shown.jsonl") {
            let session = self
                .shown
                .keys()
                .find(|session| joined.contains(session.as_str()))
                .cloned()
                .unwrap_or_default();
            return Ok(self.shown.get(&session).cloned().unwrap_or_default());
        }
        Ok(String::new())
    }

    fn append_local_inbox(&mut self, session: &str, lines: &str) -> Result<(), String> {
        self.local_inbox
            .push((session.to_owned(), lines.to_owned()));
        Ok(())
    }

    fn read_local_shown(&mut self, session: &str) -> Result<String, String> {
        self.local_shown_reads.push(session.to_owned());
        Ok(self.shown.get(session).cloned().unwrap_or_default())
    }
}

fn config(notify: bool, inbox: bool) -> HandbackConfig {
    HandbackConfig {
        enabled: true,
        notify,
        inbox,
        hosts: [
            ("Daniels-Mac-Studio-m3", "m3"),
            ("m3", "m3"),
            ("m5s", "local"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .collect(),
        ..HandbackConfig::default()
    }
}

struct Pass {
    report: HandbackReport,
    writes: Vec<Vec<String>>,
    reads: Vec<Vec<String>>,
}

fn pass(
    ledger: &mut Ledger,
    history: &RepoHistory,
    now: DateTime<Utc>,
    config: &HandbackConfig,
    mode: HandbackMode,
    runner: &mut FakeRunner,
    label_exists: bool,
) -> Pass {
    let gh = fake_gh(label_exists);
    let reads = std::sync::Mutex::new(Vec::new());
    let reader = |argv: &[String]| {
        reads.lock().unwrap().push(argv.to_vec());
        gh(argv)
    };
    let sent_writes = RefCell::new(Vec::new());
    let writer = |argv: &[String]| {
        sent_writes.borrow_mut().push(argv.to_vec());
        Ok("{}".to_owned())
    };
    let dir = tempfile::tempdir().unwrap();
    let mut deps = super::Deps {
        runner,
        state_dir: dir.path().to_path_buf(),
        local_names: vec!["Daniels-M5-Studio".to_owned()],
        local_machine: None,
    };
    let report = super::run(
        ledger, history, now, config, mode, &reader, &writer, &mut deps,
    );
    Pass {
        report,
        writes: sent_writes.into_inner(),
        reads: reads.into_inner().unwrap(),
    }
}

fn is_label_write(argv: &[String]) -> bool {
    let a: Vec<&str> = argv.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["api", "--method", "POST", path, "-f", field] => {
            path.starts_with("repos/o/r/issues/")
                && path.ends_with("/labels")
                && *field == format!("labels[]={NEEDS_AGENT_LABEL}")
        }
        ["api", "--method", "DELETE", path] => {
            path.starts_with("repos/o/r/issues/")
                && path.ends_with("/labels/shipyard%3Aneeds-agent")
        }
        _ => false,
    }
}

#[test]
fn only_owner_actionable_flags_count() {
    let (ledger, _) = world(t(0));
    let actionable_prs: Vec<u64> = ledger
        .entries
        .values()
        .filter(|e| actionable(e))
        .map(|e| e.pr)
        .collect();
    assert_eq!(actionable_prs, vec![1, 2, 5, 6]);
}

#[test]
fn mode_off_does_nothing_at_all() {
    let (mut ledger, history) = world(t(0));
    let before = ledger.clone();
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &config(true, true),
        HandbackMode::Off,
        &mut runner,
        true,
    );
    assert!(out.report.actions.is_empty() && out.report.owners.is_empty());
    assert!(out.reads.is_empty() && out.writes.is_empty() && runner.runs.is_empty());
    assert_eq!(ledger, before);
}

#[test]
fn config_off_via_from_config_defaults() {
    let config = HandbackConfig::default();
    assert!(!config.enabled && !config.notify && !config.inbox && !config.status);
}

#[test]
fn plan_mode_probes_read_only_and_sends_nothing() {
    let (mut ledger, history) = world(t(0));
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &config(true, true),
        HandbackMode::Plan,
        &mut runner,
        true,
    );
    assert!(out.writes.is_empty(), "plan wrote {:?}", out.writes);
    assert!(runner.local_inbox.is_empty());
    for run in &runner.runs {
        assert!(
            run.argv.last().unwrap().contains("sessions")
                || run.argv.contains(&"sessions".to_owned()),
            "plan ran a non-probe {:?}",
            run.argv
        );
    }
    assert!(out.report.actions.iter().all(|a| !a.sent));
    assert!(out.report.actions.iter().any(|a| a.action == "notify"));
    assert!(out.report.actions.iter().any(|a| a.action == "add_label"));
    assert!(ledger.handback.delivered.is_empty() && ledger.handback.labels.is_empty());
    // Owners are observations and are kept.
    assert_eq!(ledger.handback.owners.len(), 4);
}

#[test]
fn deliver_notifies_live_owners_once_per_episode() {
    let (mut ledger, history) = world(t(0));
    let cfg = config(true, true);
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    // Labels: exactly the actionable PRs, and only label endpoints.
    let labelled: Vec<u64> = out
        .report
        .actions
        .iter()
        .filter(|a| a.action == "add_label" && a.sent)
        .flat_map(|a| a.prs.clone())
        .collect();
    // PR 6 has no owner, so no label.
    assert_eq!(labelled, vec![1, 2, 5]);
    assert!(
        out.writes.iter().all(|argv| is_label_write(argv)),
        "{:?}",
        out.writes
    );
    // Tier 1 for the live owners (PR 1 over ssh, PR 5 locally); none for PR 2
    // (dead) or PR 6 (no owner).
    let mut notified: Vec<Vec<u64>> = out
        .report
        .actions
        .iter()
        .filter(|a| a.action == "notify" && a.sent)
        .map(|a| a.prs.clone())
        .collect();
    notified.sort();
    assert_eq!(notified, vec![vec![1], vec![5]]);
    assert_eq!(runner.local_inbox.len(), 1);
    assert_eq!(runner.local_inbox[0].0, LOCAL_SESSION);
    let line: serde_json::Value =
        serde_json::from_str(runner.local_inbox[0].1.lines().next().unwrap()).unwrap();
    assert_eq!(line["pr"], 5);
    assert_eq!(line["schema"], super::INBOX_SCHEMA);
    let remote_inbox = runner
        .runs
        .iter()
        .find(|r| r.argv.last().unwrap().contains("inbox"))
        .expect("remote inbox append");
    assert_eq!(remote_inbox.argv[6], "m3");
    assert!(remote_inbox.stdin.as_deref().unwrap().contains("\"pr\":1"));
    let views: BTreeMap<u64, u8> = out.report.owners.iter().map(|v| (v.pr, v.tier)).collect();
    assert_eq!(views[&1], 1);
    assert_eq!(views[&2], 2);
    assert_eq!(views[&6], 2);
    assert_eq!(ledger.handback.delivered.len(), 2);

    // Same episode, later pass: nothing re-sent.
    let mut again = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(3),
        &cfg,
        HandbackMode::Deliver,
        &mut again,
        true,
    );
    assert!(
        out.report
            .actions
            .iter()
            .all(|a| a.action != "notify" && a.action != "inbox")
    );
    // Only read-only probes: liveness, and the acknowledgement read-back.
    assert!(again.runs.iter().all(|r| {
        let joined = r.argv.join(" ");
        joined.contains("sessions") || joined.contains(".shown.jsonl")
    }));
    assert!(again.local_inbox.is_empty());
    assert!(out.writes.is_empty(), "labels re-added {:?}", out.writes);
}

#[test]
fn a_new_episode_waits_for_the_session_rate_limit() {
    let (mut ledger, history) = world(t(0));
    let cfg = config(true, false);
    let mut runner = FakeRunner::default();
    pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    // PR 1 gets a new episode ten minutes later.
    let later = t(1) + Duration::minutes(10);
    ledger
        .entries
        .get_mut("1:repeat_test_failure:k")
        .unwrap()
        .first_seen_at = later;
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        later,
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert!(out.report.actions.iter().all(|a| a.action != "notify"));
    let held = out.report.owners.iter().find(|v| v.pr == 1).unwrap();
    assert!(
        held.held.as_deref().unwrap().contains("rate limit"),
        "{held:?}"
    );
    // After the interval it goes out.
    let after = t(1) + Duration::minutes(45);
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        after,
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    let sent: Vec<&super::HandbackAction> = out
        .report
        .actions
        .iter()
        .filter(|a| a.action == "notify" && a.sent)
        .collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].prs, vec![1]);
}

#[test]
fn a_failed_delivery_is_retried_not_recorded() {
    let (mut ledger, history) = world(t(0));
    let cfg = config(true, false);
    let mut runner = FakeRunner {
        fail_notify: true,
        ..FakeRunner::default()
    };
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert!(
        out.report
            .actions
            .iter()
            .any(|a| a.action == "notify" && a.error.is_some())
    );
    assert!(ledger.handback.delivered.is_empty());
    assert!(ledger.handback.sessions.is_empty());
}

#[test]
fn labels_are_only_added_when_defined_and_only_ours_are_removed() {
    let (mut ledger, mut history) = world(t(0));
    let cfg = config(false, false);
    // The label is missing from the repository: report, never create.
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        false,
    );
    assert!(out.writes.is_empty(), "{:?}", out.writes);
    assert!(
        out.report.gaps.iter().any(|g| g.contains("does not exist")),
        "{:?}",
        out.report.gaps
    );

    // PR 2 already carries someone else's label; PR 1 does not.
    history.prs.get_mut(&2).unwrap().labels = vec![NEEDS_AGENT_LABEL.to_owned()];
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    let added: Vec<u64> = out
        .report
        .actions
        .iter()
        .filter(|a| a.action == "add_label")
        .flat_map(|a| a.prs.clone())
        .collect();
    // PR 6 has no owner (no marker): it gets no label.
    assert_eq!(added, vec![1, 5]);
    for pr in [1, 5] {
        history.prs.get_mut(&pr).unwrap().labels = vec![NEEDS_AGENT_LABEL.to_owned()];
    }

    // Every flag addressed: remove ours (1, 5), never PR 2's.
    for entry in ledger.entries.values_mut() {
        entry.addressed_at = Some(t(2));
    }
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(2),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    let removed: Vec<u64> = out
        .report
        .actions
        .iter()
        .filter(|a| a.action == "remove_label" && a.sent)
        .flat_map(|a| a.prs.clone())
        .collect();
    assert_eq!(removed, vec![1, 5]);
    assert!(out.writes.iter().all(|argv| is_label_write(argv)));
    assert!(ledger.handback.labels.is_empty());
}

#[test]
fn no_owner_means_no_label_and_an_unreadable_marker_keeps_ours() {
    let (mut ledger, history) = world(t(0));
    let cfg = config(false, false);
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    let posted = |writes: &[Vec<String>]| -> Vec<String> {
        writes
            .iter()
            .filter(|argv| argv.iter().any(|a| a == "POST"))
            .map(|argv| argv[3].clone())
            .collect()
    };
    // PR 6's body carries no whence marker and there is no steward record.
    assert!(
        !posted(&out.writes).contains(&"repos/o/r/issues/6/labels".to_owned()),
        "{:?}",
        out.writes
    );
    assert!(!ledger.handback.labels.contains_key(&6));
    assert_eq!(ledger.handback.owners[&6].state, "none");

    // PR 1 is labelled; next pass its body cannot be read. The label stays
    // (no DELETE), and nothing new is added for it.
    assert!(ledger.handback.labels.contains_key(&1));
    let mut history = history;
    history.prs.get_mut(&1).unwrap().labels = vec![NEEDS_AGENT_LABEL.to_owned()];
    let gh = fake_gh(true);
    let reader = |argv: &[String]| {
        if argv.last().map(String::as_str) == Some("repos/o/r/pulls/1") {
            return Err("gh: HTTP 502".to_owned());
        }
        gh(argv)
    };
    let sent = RefCell::new(Vec::new());
    let writer = |argv: &[String]| {
        sent.borrow_mut().push(argv.to_vec());
        Ok("{}".to_owned())
    };
    let dir = tempfile::tempdir().unwrap();
    let mut runner = FakeRunner::default();
    let mut deps = super::Deps {
        runner: &mut runner,
        state_dir: dir.path().to_path_buf(),
        local_names: vec!["Daniels-M5-Studio".to_owned()],
        local_machine: None,
    };
    let report = super::run(
        &mut ledger,
        &history,
        t(2),
        &cfg,
        HandbackMode::Deliver,
        &reader,
        &writer,
        &mut deps,
    );
    let sent = sent.into_inner();
    assert!(
        !sent
            .iter()
            .any(|argv| argv.iter().any(|a| a.contains("issues/1/"))),
        "{sent:?}"
    );
    assert!(ledger.handback.labels.contains_key(&1));
    assert!(
        report
            .gaps
            .iter()
            .any(|g| g.starts_with("#1: whence marker"))
    );
}

#[test]
fn a_label_someone_removed_is_not_put_back_this_episode() {
    let (mut ledger, mut history) = world(t(0));
    let cfg = config(false, false);
    let mut runner = FakeRunner::default();
    pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    // Nobody reflected the label on the PR (a person removed it).
    history.prs.get_mut(&1).unwrap().labels.clear();
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(2),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert!(out.writes.is_empty(), "{:?}", out.writes);
    assert!(ledger.handback.labels[&1].removed_by_other);
}

#[test]
fn our_label_comes_off_a_merged_closed_or_unobserved_pull_request() {
    let (mut ledger, mut history) = world(t(0));
    let cfg = config(false, false);
    let mut runner = FakeRunner::default();
    pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert_eq!(
        ledger.handback.labels.keys().copied().collect::<Vec<_>>(),
        vec![1, 2, 5]
    );
    for pr in [1, 2, 5] {
        history.prs.get_mut(&pr).unwrap().labels = vec![NEEDS_AGENT_LABEL.to_owned()];
    }
    // The flags still hold, but PR 1 merged and PR 2 left the history window:
    // neither is open, so the label must come off. PR 5 is still open and
    // flagged, so it keeps its label. (PR 6 has no owner, so it was never
    // labelled.)
    history.prs.get_mut(&1).unwrap().merged_at = Some(t(2));
    history.prs.remove(&2);
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(3),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    let deleted: Vec<String> = out
        .writes
        .iter()
        .filter(|argv| argv.iter().any(|a| a == "DELETE"))
        .map(|argv| argv.last().cloned().unwrap_or_default())
        .collect();
    assert_eq!(
        deleted,
        vec![
            "repos/o/r/issues/1/labels/shipyard%3Aneeds-agent".to_owned(),
            "repos/o/r/issues/2/labels/shipyard%3Aneeds-agent".to_owned(),
        ]
    );
    assert!(out.writes.iter().all(|argv| is_label_write(argv)));
    assert_eq!(
        ledger.handback.labels.keys().copied().collect::<Vec<_>>(),
        vec![5]
    );

    // A terminal pull request whose snapshot predates our add (no label
    // shown) still gets the DELETE; one a person cleared does not.
    let (mut ledger, mut history) = world(t(0));
    let mut runner = FakeRunner::default();
    pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    ledger.handback.labels.get_mut(&5).unwrap().removed_by_other = true;
    for pr in [1, 2, 5] {
        history.prs.get_mut(&pr).unwrap().closed_at = Some(t(2));
    }
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(3),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    let deleted: Vec<u64> = out
        .report
        .actions
        .iter()
        .filter(|a| a.action == "remove_label" && a.sent)
        .flat_map(|a| a.prs.clone())
        .collect();
    assert_eq!(deleted, vec![1, 2]);
    assert!(ledger.handback.labels.is_empty());
}

#[test]
fn a_label_is_not_posted_on_a_pull_request_that_merged_after_the_snapshot() {
    // The snapshot (history) still shows PR 1 open and flagged, but GitHub
    // now reports it merged, as with a pass that started minutes earlier.
    let (mut ledger, history) = world(t(0));
    let cfg = config(false, false);
    let gh = fake_gh(true);
    // Owners are resolved before tier 0, so PR 5's first read (its whence
    // marker) succeeds and only the open-state re-read before the add fails.
    let pr5_reads = std::sync::atomic::AtomicUsize::new(0);
    let reader = |argv: &[String]| {
        if argv.last().map(String::as_str) == Some("repos/o/r/pulls/1") {
            let open = gh(argv)?;
            let mut value: Value = serde_json::from_str(&open).unwrap();
            value["state"] = json!("closed");
            value["merged_at"] = json!("2026-10-06T12:00:18Z");
            return Ok(value.to_string());
        }
        if argv.last().map(String::as_str) == Some("repos/o/r/pulls/5")
            && pr5_reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0
        {
            return Err("gh: HTTP 502".to_owned());
        }
        gh(argv)
    };
    let sent = RefCell::new(Vec::new());
    let writer = |argv: &[String]| {
        sent.borrow_mut().push(argv.to_vec());
        Ok("{}".to_owned())
    };
    let dir = tempfile::tempdir().unwrap();
    let mut runner = FakeRunner::default();
    let mut deps = super::Deps {
        runner: &mut runner,
        state_dir: dir.path().to_path_buf(),
        local_names: vec!["Daniels-M5-Studio".to_owned()],
        local_machine: None,
    };
    let report = super::run(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &reader,
        &writer,
        &mut deps,
    );
    let posted: Vec<String> = sent
        .into_inner()
        .iter()
        .filter(|argv| argv.iter().any(|a| a == "POST"))
        .map(|argv| argv[3].clone())
        .collect();
    // PR 1 merged: refused. PR 5's state is unreadable: not posted blind.
    // PR 6 has no owner, so it is never a label candidate.
    assert_eq!(posted, vec!["repos/o/r/issues/2/labels".to_owned()]);
    let refused = |pr: u64| {
        report
            .actions
            .iter()
            .find(|a| a.action == "add_label" && a.prs == vec![pr])
            .and_then(|a| a.error.clone())
            .unwrap_or_default()
    };
    assert!(refused(1).contains("no longer open"), "{}", refused(1));
    assert!(refused(5).contains("unreadable"), "{}", refused(5));
    assert_eq!(
        ledger.handback.labels.keys().copied().collect::<Vec<_>>(),
        vec![2]
    );
}

#[test]
fn a_dead_owner_turns_unowned_after_the_threshold_and_reaches_the_digest() {
    let (mut ledger, history) = world(t(0));
    let cfg = config(true, true);
    let mut runner = FakeRunner::default();
    pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert!(!ledger.handback.owners[&2].unowned);
    let mut runner = FakeRunner::default();
    pass(
        &mut ledger,
        &history,
        t(3),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    let record = &ledger.handback.owners[&2];
    assert_eq!(record.state, "dead");
    assert!(record.unowned);
    assert_eq!(record.since, t(1));
    assert!(ledger.handback.owners[&6].unowned);
    assert!(!ledger.handback.owners[&1].unowned);
    let policy = crate::pr_watch::digest::DigestPolicy::default();
    let selection = crate::pr_watch::digest::select(&ledger, t(3), policy).unwrap();
    let payload = crate::pr_watch::digest::payload(&ledger, &selection, t(3), policy);
    let line = payload.flags.iter().find(|f| f.pr == 2).unwrap();
    let owner = line.owner.as_ref().unwrap();
    assert!(owner.unowned);
    assert_eq!(owner.state, "dead");
    assert_eq!(
        owner.resume.as_deref(),
        Some(format!("claude --resume {DEAD_SESSION}").as_str())
    );
    let live_line = payload.flags.iter().find(|f| f.pr == 1).unwrap();
    assert!(!live_line.owner.as_ref().unwrap().unowned);
}

#[test]
fn an_unmapped_host_is_unreachable_not_guessed() {
    let (mut ledger, history) = world(t(0));
    let mut cfg = config(true, true);
    cfg.hosts.clear();
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert!(runner.runs.is_empty(), "{:?}", runner.runs);
    assert!(
        out.report
            .actions
            .iter()
            .all(|a| a.action.contains("label"))
    );
    assert_eq!(ledger.handback.owners[&1].state, "unreachable");
}

#[test]
fn every_host_argv_a_pass_produces_is_allowlisted() {
    let (mut ledger, history) = world(t(0));
    let mut cfg = config(true, true);
    cfg.status = true;
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert!(
        out.report
            .actions
            .iter()
            .any(|a| a.action == "set_status" && a.sent)
    );
    for action in &out.report.actions {
        if let Some(argv) = &action.argv
            && !action.action.contains("label")
        {
            check_argv(argv, CMUX).unwrap();
        }
    }
    // Clearing the pills once the flags are addressed.
    for entry in ledger.entries.values_mut() {
        entry.addressed_at = Some(t(2));
    }
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(2),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert!(
        out.report
            .actions
            .iter()
            .any(|a| a.action == "clear_status" && a.sent)
    );
    assert!(ledger.handback.statuses.is_empty());
    for run in &runner.runs {
        check_argv(&run.argv, CMUX).unwrap();
    }
}

#[test]
fn a_plan_shows_channels_the_config_leaves_off_and_delivery_skips_them() {
    let (mut ledger, history) = world(t(0));
    let cfg = config(false, false);
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Plan,
        &mut runner,
        true,
    );
    let notify = out
        .report
        .actions
        .iter()
        .find(|a| a.action == "notify")
        .unwrap();
    assert!(
        notify.summary.ends_with("[off in config]"),
        "{}",
        notify.summary
    );
    assert!(out.report.actions.iter().any(|a| a.action == "inbox"));
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert!(
        out.report
            .actions
            .iter()
            .all(|a| a.action.contains("label"))
    );
    assert!(runner.local_inbox.is_empty());
    assert!(ledger.handback.delivered.is_empty());
}

#[test]
fn the_label_sweep_touches_only_closed_pull_requests_carrying_the_label() {
    let label = json!([{"name": NEEDS_AGENT_LABEL}]);
    let other = json!([{"name": "bug"}]);
    let page = json!([
        {"number": 11, "state": "closed", "closed_at": "2026-10-06T12:00:18Z",
         "labels": label, "pull_request": {"merged_at": "2026-10-06T12:00:00Z"}},
        {"number": 12, "state": "closed", "closed_at": "2026-10-05T09:00:00Z",
         "labels": label, "pull_request": {"merged_at": null}},
        // An issue, not a pull request.
        {"number": 13, "state": "closed", "labels": label},
        // A pull request the listing returned without the label.
        {"number": 14, "state": "closed", "labels": other,
         "pull_request": {"merged_at": null}},
        // Open: the regular pass judges it, the sweep never does.
        {"number": 15, "state": "open", "labels": label,
         "pull_request": {"merged_at": null}},
        {"number": 16, "state": "closed", "closed_at": "2026-10-04T08:00:00Z",
         "labels": label, "pull_request": {"merged_at": "2026-10-04T08:00:00Z"}}
    ]);
    let reads = std::sync::Mutex::new(Vec::new());
    let gh = |argv: &[String]| {
        reads.lock().unwrap().push(argv.to_vec());
        if argv[1].ends_with("&page=1") {
            Ok(page.to_string())
        } else {
            Ok("[]".to_owned())
        }
    };
    // Plan: lists, never writes.
    let planned = super::sweep_labels("o/r", &gh, None).unwrap();
    let numbers: Vec<u64> = planned.iter().map(|s| s.pr).collect();
    assert_eq!(numbers, vec![11, 12, 16]);
    assert_eq!(planned[0].state, "merged");
    assert_eq!(planned[1].state, "closed");
    assert!(planned.iter().all(|s| !s.removed && s.error.is_none()));
    assert!(
        reads.lock().unwrap()[0][1]
            .starts_with("repos/o/r/issues?labels=shipyard%3Aneeds-agent&state=closed"),
        "{:?}",
        reads.lock().unwrap()
    );

    // Apply: exactly one DELETE per stale pull request; a 404 counts as gone,
    // any other failure is reported.
    let sent = RefCell::new(Vec::new());
    let write = |argv: &[String]| {
        sent.borrow_mut().push(argv.to_vec());
        match argv.last().map(String::as_str) {
            Some("repos/o/r/issues/12/labels/shipyard%3Aneeds-agent") => {
                Err("gh: Not Found (HTTP 404)".to_owned())
            }
            Some("repos/o/r/issues/16/labels/shipyard%3Aneeds-agent") => {
                Err("gh: HTTP 502".to_owned())
            }
            _ => Ok("[]".to_owned()),
        }
    };
    let applied = super::sweep_labels("o/r", &gh, Some(&write)).unwrap();
    let sent = sent.into_inner();
    assert_eq!(sent.len(), 3);
    assert!(
        sent.iter()
            .all(|argv| is_label_write(argv) && argv.contains(&"DELETE".to_owned()))
    );
    assert!(applied[0].removed && applied[1].removed);
    assert!(!applied[2].removed);
    assert!(
        applied[2]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("502"))
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One lifecycle, read top to bottom.
fn wake_events_record_raise_send_failure_and_resolution_and_plan_records_none() {
    let changes = |events: &[crate::pr_watch::ledger::LedgerEvent]| -> Vec<String> {
        events
            .iter()
            .map(|e| format!("{} {}", e.change, e.id))
            .collect()
    };
    // Plan mode: no events, no wake state.
    let (mut ledger, history) = world(t(0));
    let cfg = config(true, true);
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Plan,
        &mut runner,
        true,
    );
    assert!(out.report.events.is_empty());
    assert!(ledger.handback.wakes.is_empty());

    // Deliver: every owner-actionable open episode is raised; live owners'
    // episodes (1 on m3, 5 local) are sent with their session and route.
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    let got = changes(&out.report.events);
    for id in [
        "1:repeat_test_failure:k",
        "2:red_while_armed:k",
        "5:repeated_ejection:",
        "6:red_while_armed:k",
    ] {
        assert!(got.contains(&format!("wake.raised {id}")), "{got:?}");
    }
    assert!(
        got.contains(&"wake.sent 1:repeat_test_failure:k".to_owned()),
        "{got:?}"
    );
    assert!(
        got.contains(&"wake.sent 5:repeated_ejection:".to_owned()),
        "{got:?}"
    );
    // Not actionable (shared failure, neighbour ejection, treadmill): never raised.
    assert!(
        !got.iter().any(|c| c.contains(" 3:") || c.contains(" 4:")),
        "{got:?}"
    );
    let sent = out
        .report
        .events
        .iter()
        .find(|e| e.change == "wake.sent" && e.id == "1:repeat_test_failure:k")
        .and_then(|e| e.detail.clone())
        .unwrap();
    assert_eq!(sent["session"], json!(LIVE_SESSION));
    assert_eq!(sent["host"], json!("ssh m3"));
    assert_eq!(sent["rung"], json!("1"));
    assert!(
        ledger.handback.wakes["1:repeat_test_failure:k"]
            .sent_at
            .is_some()
    );
    assert!(
        ledger.handback.wakes["2:red_while_armed:k"]
            .sent_at
            .is_none()
    );

    // An unchanged episode is not raised again.
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(2),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert!(
        !changes(&out.report.events)
            .iter()
            .any(|c| c.starts_with("wake.raised")),
        "{:?}",
        out.report.events
    );

    // Addressed: resolved, with how; the record is gone.
    ledger
        .entries
        .get_mut("1:repeat_test_failure:k")
        .unwrap()
        .addressed_at = Some(t(3));
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(3),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    let resolved = out
        .report
        .events
        .iter()
        .find(|e| e.change == "wake.resolved")
        .unwrap();
    assert_eq!(resolved.id, "1:repeat_test_failure:k");
    assert_eq!(resolved.detail.as_ref().unwrap()["how"], json!("addressed"));
    assert!(runner.runs.iter().any(|invocation| {
        invocation
            .stdin
            .as_deref()
            .is_some_and(|text| text.contains("\"retract\":\"1:repeat_test_failure:k@"))
    }));
    assert!(
        !ledger
            .handback
            .wakes
            .contains_key("1:repeat_test_failure:k")
    );
}

#[test]
fn a_delivery_whose_every_channel_failed_is_a_wake_failure() {
    let (mut ledger, history) = world(t(0));
    let cfg = config(true, false);
    let mut runner = FakeRunner {
        fail_notify: true,
        ..FakeRunner::default()
    };
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    let failed: Vec<&str> = out
        .report
        .events
        .iter()
        .filter(|e| e.change == "wake.failed")
        .map(|e| e.id.as_str())
        .collect();
    assert!(failed.contains(&"1:repeat_test_failure:k"), "{failed:?}");
    assert!(
        !out.report.events.iter().any(|e| e.change == "wake.sent"),
        "{:?}",
        out.report.events
    );
}

#[test]
fn a_wake_with_no_channel_configured_is_unsent_not_failed() {
    let (mut ledger, history) = world(t(0));
    let cfg = config(false, false);
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert!(
        !out.report.events.iter().any(|e| e.change == "wake.failed"),
        "{:?}",
        out.report.events
    );
    let unsent: Vec<&str> = out
        .report
        .events
        .iter()
        .filter(|e| e.change == "wake.unsent")
        .map(|e| e.id.as_str())
        .collect();
    // The live owners' episodes; a dead owner's is never attempted either,
    // but it is the report's `unsent` list (by owner state), not an event.
    assert_eq!(
        unsent,
        vec!["1:repeat_test_failure:k", "5:repeated_ejection:"]
    );
    let raised = out
        .report
        .events
        .iter()
        .find(|e| e.change == "wake.raised" && e.id == "2:red_while_armed:k")
        .and_then(|e| e.detail.clone())
        .unwrap();
    assert_eq!(raised["owner"], json!("dead"));
    // Logged once per episode, not every pass.
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(2),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert!(!out.report.events.iter().any(|e| e.change == "wake.unsent"));
}

#[test]
fn a_wake_is_seen_only_when_the_owner_session_displayed_that_episode() {
    let (mut ledger, history) = world(t(0));
    let cfg = config(false, true);
    let mut runner = FakeRunner::default();
    pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    let id = "1:repeat_test_failure:k";
    let episode = ledger.handback.wakes[id].episode;
    let key = format!("{id}@{}", episode.format("%Y-%m-%dT%H:%M:%SZ"));
    assert!(ledger.handback.wakes[id].sent_to.is_some());

    // Delivered but not displayed: no `seen`, however much time passes.
    let mut runner = FakeRunner::default();
    runner.shown.insert(
        LIVE_SESSION.to_owned(),
        // Another episode of the same entry, and another entry: neither counts.
        format!(
            "{}\n{}\nnot json\n",
            json!({"id": format!("{id}@2026-01-01T00:00:00Z"), "shown_at": "2026-10-07T00:10:00Z"}),
            json!({"id": "2:red_while_armed:k@x", "shown_at": "2026-10-07T00:10:00Z"}),
        ),
    );
    let out = pass(
        &mut ledger,
        &history,
        t(5),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert!(
        !out.report.events.iter().any(|e| e.change == "wake.seen"),
        "{:?}",
        out.report.events
    );
    // The read went over the delivery's route and passed the allowlist.
    assert!(
        runner
            .runs
            .iter()
            .any(|r| r.argv[0] == "ssh" && r.argv.last().is_some_and(|w| w.contains("shown.jsonl")))
    );
    // PR 5's owner is on this host: read as a file, not a process.
    assert_eq!(runner.local_shown_reads, vec![LOCAL_SESSION.to_owned()]);

    // Displayed: `seen` at the hook's own time, once.
    let mut runner = FakeRunner::default();
    runner.shown.insert(
        LIVE_SESSION.to_owned(),
        json!({"id": key, "shown_at": "2026-10-07T02:03:04Z"}).to_string(),
    );
    let out = pass(
        &mut ledger,
        &history,
        t(6),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    let seen: Vec<_> = out
        .report
        .events
        .iter()
        .filter(|e| e.change == "wake.seen")
        .collect();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].id, id);
    assert_eq!(seen[0].at.to_rfc3339(), "2026-10-07T02:03:04+00:00");
    assert_eq!(
        seen[0].detail.as_ref().unwrap()["session"],
        json!(LIVE_SESSION)
    );
    assert!(ledger.handback.wakes[id].seen_at.is_some());
    let mut runner = FakeRunner::default();
    runner.shown.insert(
        LIVE_SESSION.to_owned(),
        json!({"id": key, "shown_at": "2026-10-07T02:03:04Z"}).to_string(),
    );
    let out = pass(
        &mut ledger,
        &history,
        t(7),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert!(!out.report.events.iter().any(|e| e.change == "wake.seen"));
    assert!(
        !out.report
            .events
            .iter()
            .any(|e| { e.id == id && matches!(e.change.as_str(), "wake.sent" | "wake.escalated") })
    );
    assert!(!runner.runs.iter().any(|invocation| {
        let joined = invocation.argv.join(" ");
        joined.contains(" notify ") || joined.contains(INBOX_SCRIPT)
    }));
}

#[test]
fn seen_at_alone_blocks_an_due_retry() {
    let (mut ledger, history) = world(t(0));
    let mut cfg = config(false, true);
    cfg.retry_after = chrono::Duration::hours(1);
    let id = "1:repeat_test_failure:k";
    ledger.handback.wakes.insert(
        id.to_owned(),
        super::WakeRecord {
            pr: 1,
            episode: t(0),
            raised_at: t(0),
            sent_at: Some(t(0)),
            unsent: None,
            inbox: None,
            sent_to: Some((LIVE_SESSION.to_owned(), Route::Ssh("m3".to_owned()))),
            seen_at: Some(t(0)),
            send_count: 1,
            last_sent_at: Some(t(0)),
            escalated: false,
        },
    );
    let mut runner = FakeRunner::default();
    let out = pass(
        &mut ledger,
        &history,
        t(3),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert!(!out.report.events.iter().any(|event| {
        event.id == id && matches!(event.change.as_str(), "wake.sent" | "wake.escalated")
    }));
}

#[test]
fn dead_process_with_missing_cmux_row_is_not_live() {
    let (mut ledger, history) = world(t(0));
    let cfg = config(false, false);
    let mut runner = FakeRunner {
        omit_cmux_rows: true,
        ..FakeRunner::default()
    };
    pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert_eq!(ledger.handback.owners[&2].state, "unknown");
    assert!(runner.runs.iter().any(|invocation| {
        invocation.argv.join(" ").contains("process_alive")
            && invocation.argv.join(" ").contains(DEAD_SESSION)
    }));
}

#[test]
fn missing_cmux_row_runs_evidence_probe_and_can_prove_live() {
    let (mut ledger, history) = world(t(0));
    let cfg = config(false, false);
    let mut runner = FakeRunner {
        omit_cmux_rows: true,
        ..FakeRunner::default()
    };
    pass(
        &mut ledger,
        &history,
        t(1),
        &cfg,
        HandbackMode::Deliver,
        &mut runner,
        true,
    );
    assert_eq!(ledger.handback.owners[&1].state, "live");
    assert!(runner.runs.iter().any(|invocation| {
        invocation.argv.join(" ").contains("process_alive")
            && invocation.argv.join(" ").contains(LIVE_SESSION)
    }));
}

#[test]
fn liveness_allowlist_rejects_transcript_path_traversal() {
    let invocation = host::invocation(
        &Route::Ssh("m3".to_owned()),
        &HostCommand::LivenessEvidence {
            session: LIVE_SESSION.to_owned(),
            transcript: "/work/../etc/transcript.jsonl".to_owned(),
        },
        CMUX,
        None,
    )
    .expect("ssh evidence probe");
    assert!(check_argv(&invocation.argv, CMUX).is_err());
}

#[test]
fn unanswered_wake_retries_until_budget_then_escalates_once() {
    let (mut ledger, history) = world(t(0));
    let mut cfg = config(false, true);
    cfg.retry_after = chrono::Duration::hours(1);
    cfg.max_unseen_sends = 3;
    for (at, expected_sends, expected_escalations) in
        [(1, 1, 0), (1, 1, 0), (2, 2, 0), (3, 3, 1), (4, 3, 0)]
    {
        let mut runner = FakeRunner::default();
        let out = pass(
            &mut ledger,
            &history,
            t(at),
            &cfg,
            HandbackMode::Deliver,
            &mut runner,
            true,
        );
        let id = "1:repeat_test_failure:k";
        assert_eq!(ledger.handback.wakes[id].send_count, expected_sends);
        assert_eq!(
            out.report
                .events
                .iter()
                .filter(|e| e.change == "wake.escalated" && e.id == id)
                .count(),
            expected_escalations
        );
    }
}

#[test]
fn the_acknowledgement_read_is_one_fixed_script_and_nothing_else() {
    let remote = |script: &str, session: &str| {
        vec![
            "ssh".to_owned(),
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "-o".to_owned(),
            host::SSH_CONNECT_TIMEOUT.to_owned(),
            "--".to_owned(),
            "m3".to_owned(),
            ["sh", "-c", script, "sh", session]
                .iter()
                .map(|w| shell_quote(w))
                .collect::<Vec<_>>()
                .join(" "),
        ]
    };
    assert!(check_argv(&remote(SHOWN_SCRIPT, LIVE_SESSION), CMUX).is_ok());
    assert!(check_argv(&remote(SHOWN_SCRIPT, "../../etc/passwd"), CMUX).is_err());
    assert!(check_argv(&remote("cat ~/.ssh/id_ed25519", LIVE_SESSION), CMUX).is_err());
    assert!(
        check_argv(
            &remote(&SHOWN_SCRIPT.replace("tail -n 500", "cat"), LIVE_SESSION),
            CMUX
        )
        .is_err()
    );
    let planned = host::invocation(
        &Route::Ssh("m3".to_owned()),
        &HostCommand::ShownRead {
            session: LIVE_SESSION.to_owned(),
        },
        CMUX,
        None,
    )
    .unwrap();
    assert_eq!(planned.argv, remote(SHOWN_SCRIPT, LIVE_SESSION));
    assert!(
        host::invocation(
            &Route::Local,
            &HostCommand::ShownRead {
                session: LIVE_SESSION.to_owned(),
            },
            CMUX,
            None,
        )
        .is_none()
    );
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        host::read_shown_file(dir.path(), LIVE_SESSION, 2).unwrap(),
        ""
    );
    std::fs::write(
        dir.path().join(format!("{LIVE_SESSION}.shown.jsonl")),
        "a\nb\nc\n",
    )
    .unwrap();
    assert_eq!(
        host::read_shown_file(dir.path(), LIVE_SESSION, 2).unwrap(),
        "b\nc"
    );
    assert!(host::read_shown_file(dir.path(), "../x", 2).is_err());
}
