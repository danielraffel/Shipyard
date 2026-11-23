//! Solo-entry admission: a release reconcile lands with nothing ahead of it.
use std::cell::RefCell;

use super::*;
use crate::app::merge_steward_cmd::pr_mutations::mutate_pr_with_recovery;
use crate::app::merge_steward_cmd::solo_entry::{
    Gate, ObserveInput, Phase, Reads, RepoSetting, Settings, advance_window, file_version, observe,
    pre_enqueue_refusal, release_pending, verify_after_enqueue, window_key, window_open,
};

const SOLO_BRANCH: &str = "automation/pulp-observatory-coalesced";
const SOLO: u64 = 7;
const PRODUCT: u64 = 42;

fn settings_for(repo: &str) -> Settings {
    let raw = format!(
        "[\"{repo}\"]\nbranches = [\"{SOLO_BRANCH}\"]\nversion_file = \"CMakeLists.txt\"\n"
    );
    Settings::from_value(Some(&toml::Value::Table(raw.parse().expect("toml"))))
}

fn pr_row(number: u64, branch: &str, draft: bool, conclusion: &str) -> Value {
    serde_json::json!({
        "id": format!("PR_node_{number}"),
        "number": number,
        "state": "OPEN",
        "isDraft": draft,
        "baseRefName": "main",
        "headRefOid": format!("{number:0>40}"),
        "headRefName": branch,
        "mergeStateStatus": "CLEAN",
        "autoMergeRequest": null,
        "labels": [],
        "statusCheckRollup": [{
            "__typename": "CheckRun",
            "name": "macos",
            "status": "COMPLETED",
            "conclusion": conclusion,
            "completedAt": "2026-11-22T00:00:00Z",
            "detailsUrl": "https://github.com/owner/repo/actions/runs/100"
        }]
    })
}

fn pr(
    number: u64,
    branch: &str,
    draft: bool,
    conclusion: &str,
    queue: &[(u64, u64)],
) -> ObservedPr {
    parse_pr(
        &pr_row(number, branch, draft, conclusion),
        &queue.iter().copied().collect(),
    )
    .expect("PR")
}

fn observation_with(prs: Vec<ObservedPr>) -> RepoObservation {
    let mut observation = observation_for(prs[0].clone(), true);
    observation.prs = prs;
    observation
}

/// [`Reads`] from fixed answers, recording every call.
struct FakeReads {
    pending: Result<Option<String>, String>,
    approved: bool,
    queue: Result<BTreeMap<u64, u64>, String>,
    calls: RefCell<Vec<String>>,
    dequeued: RefCell<Vec<u64>>,
}

impl FakeReads {
    fn release(queue: &[(u64, u64)]) -> Self {
        Self {
            pending: Ok(Some("v0.2.2".to_owned())),
            approved: true,
            queue: Ok(queue.iter().copied().collect()),
            calls: RefCell::default(),
            dequeued: RefCell::default(),
        }
    }
}

impl Reads for FakeReads {
    fn pending_release(&self, _setting: &RepoSetting) -> Result<Option<String>, String> {
        self.calls.borrow_mut().push("release".to_owned());
        self.pending.clone()
    }
    fn approved(&self, pr: &ObservedPr, _ledger: &StewardLedger) -> bool {
        self.calls
            .borrow_mut()
            .push(format!("approved:{}", pr.fact.number));
        self.approved
    }
    fn queue(&self) -> Result<BTreeMap<u64, u64>, String> {
        self.calls.borrow_mut().push("queue".to_owned());
        self.queue.clone()
    }
    fn dequeue(&self, pr: &ObservedPr, _ledger: &mut StewardLedger) -> Result<(), String> {
        self.dequeued.borrow_mut().push(pr.fact.number);
        Ok(())
    }
}

fn run(
    settings: &Settings,
    observation: &RepoObservation,
    reads: &FakeReads,
    ledger: &mut StewardLedger,
    now: DateTime<Utc>,
) -> Gate {
    observe(
        reads,
        &ObserveInput {
            settings,
            observation,
            policy: &queue_policy(),
            apply: true,
            deadline: None,
            now,
        },
        ledger,
    )
}

fn now() -> DateTime<Utc> {
    "2026-11-23T01:00:00Z".parse().expect("time")
}

/// The steward's enqueue path for `pr` under `gate`, against a `gh` that
/// records every call and refuses to answer any.
#[cfg(unix)]
fn enqueue_under(gate: &Gate, pr: &ObservedPr) -> (Option<String>, String) {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!("printf '%s\\n' \"$*\" >> '{}'\nexit 2", log.display()),
    );
    let observation = observation_with(vec![pr.clone()]);
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let mut context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    context.solo_entry = Some(gate);
    let (mutation, _) = mutate_pr_with_recovery(
        &context,
        pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut StewardLedger::default(),
        false,
    );
    (mutation, fs::read_to_string(log).unwrap_or_default())
}

#[test]
fn without_a_setting_nothing_is_read_and_nothing_is_held() {
    let product = pr(PRODUCT, "feature", false, "SUCCESS", &[]);
    let solo = pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[]);
    let observation = observation_with(vec![product, solo]);
    let reads = FakeReads::release(&[(99, 1)]);
    let mut ledger = StewardLedger::default();
    // A window left by a setting since removed (a rollback) is forgotten.
    ledger.solo_entry_windows.insert(
        window_key("owner/repo", "main"),
        crate::app::merge_steward_cmd::solo_entry::Window {
            version: "v0.2.2".to_owned(),
            opened_at: now(),
            deadline: now() + chrono::Duration::minutes(120),
            expired: false,
        },
    );

    // Another repository's setting does not reach this one.
    for settings in [Settings::default(), settings_for("Generous-Corp/vellum")] {
        let gate = run(&settings, &observation, &reads, &mut ledger, now());
        assert_eq!(gate, Gate::Off);
        assert_eq!(gate.line(), None);
        assert!(gate.backstop_allowed());
        assert_eq!(gate.enqueue_hold(PRODUCT), None);
        assert_eq!(gate.enqueue_hold(SOLO), None);
    }
    assert!(reads.calls.borrow().is_empty(), "{:?}", reads.calls);
    assert!(ledger.solo_entry_windows.is_empty());
}

#[test]
fn no_release_pending_means_the_mode_is_inactive() {
    let observation = observation_with(vec![
        pr(PRODUCT, "feature", false, "SUCCESS", &[]),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[]),
    ]);
    let mut reads = FakeReads::release(&[(99, 1)]);
    reads.pending = Ok(None);
    let mut ledger = StewardLedger::default();
    let gate = run(
        &settings_for("owner/repo"),
        &observation,
        &reads,
        &mut ledger,
        now(),
    );
    assert!(matches!(gate, Gate::Inactive { .. }), "{gate:?}");
    assert!(gate.line().unwrap().contains("no release pending"));
    assert!(gate.backstop_allowed());
    assert_eq!(gate.enqueue_hold(PRODUCT), None);
    assert!(ledger.solo_entry_windows.is_empty());

    // An unreadable release state holds nothing either.
    reads.pending = Err("HTTP 502".to_owned());
    let gate = run(
        &settings_for("owner/repo"),
        &observation,
        &reads,
        &mut ledger,
        now(),
    );
    assert!(matches!(gate, Gate::Inactive { .. }), "{gate:?}");
    assert_eq!(gate.enqueue_hold(PRODUCT), None);
}

#[test]
fn a_ready_solo_pr_with_a_non_empty_queue_holds_everything_and_waits() {
    let observation = observation_with(vec![
        pr(PRODUCT, "feature", false, "SUCCESS", &[]),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[]),
    ]);
    let reads = FakeReads::release(&[(99, 1), (98, 2)]);
    let mut ledger = StewardLedger::default();
    let gate = run(
        &settings_for("owner/repo"),
        &observation,
        &reads,
        &mut ledger,
        now(),
    );
    assert_eq!(
        gate.enqueue_hold(PRODUCT).as_deref(),
        Some("held_solo_entry:#7")
    );
    assert_eq!(
        gate.enqueue_hold(SOLO).as_deref(),
        Some("solo_entry_waiting:2_ahead")
    );
    assert!(!gate.backstop_allowed(), "nothing is armed either");
    let line = gate.line().expect("line");
    assert!(
        line.contains("#7") && line.contains("2 queue entries ahead"),
        "{line}"
    );
    assert!(reads.dequeued.borrow().is_empty());
    assert!(window_open(&ledger, "owner/repo", "main", now()));
}

#[cfg(unix)]
#[test]
fn held_rows_reach_no_github_call_on_the_enqueue_path() {
    let gate = Gate::Hold {
        solo: SOLO,
        phase: Phase::Waiting { ahead: 1 },
        line: String::new(),
    };
    let product = pr(PRODUCT, "feature", false, "SUCCESS", &[]);
    let (mutation, calls) = enqueue_under(&gate, &product);
    assert_eq!(mutation.as_deref(), Some("held_solo_entry:#7"));
    assert!(calls.is_empty(), "{calls}");
    let solo = pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[]);
    let (mutation, calls) = enqueue_under(&gate, &solo);
    assert_eq!(mutation.as_deref(), Some("solo_entry_waiting:1_ahead"));
    assert!(calls.is_empty(), "{calls}");
    // The control: with the gate off the same path goes on to read GitHub.
    let (_, calls) = enqueue_under(&Gate::Off, &product);
    assert!(!calls.is_empty());
}

#[test]
fn a_ready_solo_pr_with_an_empty_queue_is_the_only_admission() {
    let observation = observation_with(vec![
        pr(PRODUCT, "feature", false, "SUCCESS", &[]),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[]),
    ]);
    let reads = FakeReads::release(&[]);
    let mut ledger = StewardLedger::default();
    let gate = run(
        &settings_for("owner/repo"),
        &observation,
        &reads,
        &mut ledger,
        now(),
    );
    assert_eq!(gate.enqueue_hold(SOLO), None, "{gate:?}");
    assert_eq!(
        gate.enqueue_hold(PRODUCT).as_deref(),
        Some("held_solo_entry:#7")
    );
    assert!(!gate.backstop_allowed());
    assert!(gate.line().unwrap().contains("admitting it alone"));
}

#[test]
fn while_the_solo_entry_is_queued_nothing_is_admitted_behind_it() {
    let observation = observation_with(vec![
        pr(PRODUCT, "feature", false, "SUCCESS", &[]),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[(SOLO, 1)]),
    ]);
    let reads = FakeReads::release(&[(SOLO, 1)]);
    let mut ledger = StewardLedger::default();
    let gate = run(
        &settings_for("owner/repo"),
        &observation,
        &reads,
        &mut ledger,
        now(),
    );
    assert!(
        matches!(
            gate,
            Gate::Hold {
                phase: Phase::Queued,
                ..
            }
        ),
        "{gate:?}"
    );
    assert_eq!(
        gate.enqueue_hold(PRODUCT).as_deref(),
        Some("held_solo_entry:#7")
    );
    assert!(!gate.backstop_allowed());
    assert!(reads.dequeued.borrow().is_empty());
}

#[test]
fn a_solo_entry_queued_behind_another_removes_only_itself() {
    let observation = observation_with(vec![
        pr(PRODUCT, "feature", false, "SUCCESS", &[(PRODUCT, 1)]),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[(SOLO, 2)]),
    ]);
    let reads = FakeReads::release(&[(PRODUCT, 1), (SOLO, 2)]);
    let mut ledger = StewardLedger::default();
    let gate = run(
        &settings_for("owner/repo"),
        &observation,
        &reads,
        &mut ledger,
        now(),
    );
    assert_eq!(*reads.dequeued.borrow(), vec![SOLO]);
    assert_eq!(
        gate.enqueue_hold(SOLO).as_deref(),
        Some("solo_entry_waiting:1_ahead"),
        "{gate:?}"
    );
    assert!(gate.line().unwrap().contains("dequeued its own entry"));
}

#[cfg(unix)]
#[test]
fn after_its_enqueue_an_entry_ahead_dequeues_only_the_solo_entry() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let product_head = format!("{PRODUCT:0>40}");
    let solo_head = format!("{SOLO:0>40}");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"dequeuePullRequest"*) printf '%s' '{{"data":{{"dequeuePullRequest":{{"mergeQueueEntry":{{"id":"MQE"}}}}}}}}' ;;
  *"mergeQueue"*) printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[{{"position":1,"enqueuedAt":"2026-11-23T00:59:00Z","headCommit":{{"oid":"{product_head}"}},"pullRequest":{{"number":{PRODUCT}}}}},{{"position":2,"enqueuedAt":"2026-11-23T01:00:00Z","headCommit":{{"oid":"{solo_head}"}},"pullRequest":{{"number":{SOLO}}}}}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac"#,
            log = log.display(),
        ),
    );
    let solo = pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[]);
    let observation = observation_with(vec![
        pr(PRODUCT, "feature", false, "SUCCESS", &[]),
        solo.clone(),
    ]);
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let gate = Gate::Hold {
        solo: SOLO,
        phase: Phase::Admit,
        line: String::new(),
    };
    let mut context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    context.solo_entry = Some(&gate);
    let mut ledger = StewardLedger::default();

    let (mutation, error) = verify_after_enqueue(&context, &solo, &mut ledger);

    assert_eq!(mutation.as_deref(), Some("dequeued_solo_entry:1_ahead"));
    assert!(error.is_none(), "{error:?}");
    let calls = fs::read_to_string(log).expect("calls");
    let dequeues = calls
        .lines()
        .filter(|line| line.contains("dequeuePullRequest"))
        .collect::<Vec<_>>();
    assert_eq!(dequeues.len(), 1, "{calls}");
    assert!(dequeues[0].contains("id=PR_node_7"), "{calls}");
    assert!(!calls.contains("PR_node_42"), "{calls}");
    // The removal is the steward's own, so it never counts as an ejection.
    assert_eq!(ledger.steward_removals.len(), 1);
}

#[test]
fn a_solo_pr_that_cannot_merge_changes_nothing() {
    let mut ledger = StewardLedger::default();
    for (solo, approved) in [
        (pr(SOLO, SOLO_BRANCH, false, "FAILURE", &[]), true),
        (pr(SOLO, SOLO_BRANCH, true, "SUCCESS", &[]), true),
        (pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[]), false),
    ] {
        let observation =
            observation_with(vec![pr(PRODUCT, "feature", false, "SUCCESS", &[]), solo]);
        let mut reads = FakeReads::release(&[(99, 1)]);
        reads.approved = approved;
        let gate = run(
            &settings_for("owner/repo"),
            &observation,
            &reads,
            &mut ledger,
            now(),
        );
        assert!(matches!(gate, Gate::Inactive { .. }), "{gate:?}");
        assert_eq!(gate.enqueue_hold(PRODUCT), None);
        assert!(gate.backstop_allowed());
        assert!(reads.dequeued.borrow().is_empty());
    }
}

#[test]
fn past_the_expiry_normal_admission_resumes() {
    let observation = observation_with(vec![
        pr(PRODUCT, "feature", false, "SUCCESS", &[]),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[]),
    ]);
    let reads = FakeReads::release(&[(99, 1)]);
    let settings = settings_for("owner/repo");
    let mut ledger = StewardLedger::default();
    let opened = now();
    let gate = run(&settings, &observation, &reads, &mut ledger, opened);
    assert!(matches!(gate, Gate::Hold { .. }), "{gate:?}");

    let almost = opened + chrono::Duration::minutes(119);
    let gate = run(&settings, &observation, &reads, &mut ledger, almost);
    assert!(matches!(gate, Gate::Hold { .. }), "{gate:?}");

    let expired = opened + chrono::Duration::minutes(120);
    let gate = run(&settings, &observation, &reads, &mut ledger, expired);
    assert!(matches!(gate, Gate::Inactive { .. }), "{gate:?}");
    assert!(gate.line().unwrap().contains("RELEASE WINDOW EXPIRED"));
    assert_eq!(gate.enqueue_hold(PRODUCT), None);
    assert!(gate.backstop_allowed());
    assert!(!window_open(&ledger, "owner/repo", "main", expired));

    // The expiry line is logged once; the window stays shut for the version.
    let later = expired + chrono::Duration::minutes(30);
    let gate = run(&settings, &observation, &reads, &mut ledger, later);
    assert!(matches!(gate, Gate::Inactive { .. }), "{gate:?}");
    assert!(!gate.line().unwrap().contains("EXPIRED"));
}

#[test]
fn the_window_resets_for_a_new_version_and_clears_once_tagged() {
    let start = now();
    let (window, _) = advance_window(None, Some("v0.2.2"), 120, start);
    let mut window = window.expect("window");
    window.expired = true;
    let (next, status) = advance_window(Some(&window), Some("v0.2.3"), 120, start);
    assert_eq!(next.expect("window").opened_at, start);
    assert!(matches!(
        status,
        crate::app::merge_steward_cmd::solo_entry::WindowStatus::Open { .. }
    ));
    let (cleared, _) = advance_window(Some(&window), None, 120, start);
    assert!(cleared.is_none());
    assert_eq!(window_key("Owner/Repo", "main"), "owner/repo@main");
}

#[test]
fn the_release_request_is_read_from_the_version_file_and_tags() {
    let cmake =
        "cmake_minimum_required(VERSION 3.25)\nproject(Vellum\n  VERSION 0.2.2\n  LANGUAGES CXX)\n";
    assert_eq!(file_version("CMakeLists.txt", cmake), Some((0, 2, 2)));
    assert_eq!(file_version("VERSION", "1.4.0\n"), Some((1, 4, 0)));
    assert_eq!(file_version("CMakeLists.txt", "project(x)\n"), None);
    let tags = [
        "refs/tags/v0.2.1".to_owned(),
        "refs/tags/v0.1.10".to_owned(),
    ];
    assert!(release_pending((0, 2, 2), &tags, "v", false));
    assert!(
        !release_pending((0, 2, 1), &tags, "v", true),
        "tagged and released"
    );
    assert!(
        !release_pending((0, 2, 0), &tags, "v", false),
        "older than a tag"
    );
}

/// Vellum v0.2.2: the tag existed at 02:27Z, but its own provenance run judged
/// live state after signing and failed, and no release was published. The
/// window must hold until the release is published, not end at the tag.
#[test]
fn tag_exists_but_release_unpublished_keeps_the_window_open() {
    let tags = ["refs/tags/v0.2.2".to_owned(), "refs/tags/v0.2.1".to_owned()];
    assert!(
        release_pending((0, 2, 2), &tags, "v", false),
        "tagged, release missing or draft: still pending"
    );
    assert!(
        !release_pending((0, 2, 2), &tags, "v", true),
        "tagged and published: closed"
    );
    assert!(
        !release_pending((0, 2, 1), &tags, "v", false),
        "an older version is never pending"
    );
    assert!(release_pending((0, 1, 0), &[], "v", false), "no tags yet");
}

#[test]
fn a_malformed_setting_is_reported_and_admission_runs_normally() {
    let observation = observation_with(vec![pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[])]);
    let reads = FakeReads::release(&[(99, 1)]);
    let settings = Settings::from_value(Some(&toml::Value::Table(
        "[\"owner/repo\"]\nbranches = []\n".parse().expect("toml"),
    )));
    let gate = run(
        &settings,
        &observation,
        &reads,
        &mut StewardLedger::default(),
        now(),
    );
    assert!(gate.line().unwrap().contains("setting refused"), "{gate:?}");
    assert_eq!(gate.enqueue_hold(SOLO), None);
    assert!(reads.calls.borrow().is_empty());
}

#[cfg(unix)]
fn queue_gh(temp: &tempfile::TempDir, nodes: &str) -> GitHubActions {
    fake_gh(
        temp,
        &format!(
            r#"case "$*" in
  *"mergeQueue"*) printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[{nodes}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac"#
        ),
    )
}

/// Under the enqueue's guard the queue is read again: an entry that
/// appeared since the pass's read refuses the solo enqueue.
#[cfg(unix)]
#[test]
fn the_solo_enqueue_rereads_the_queue_under_the_guard() {
    let solo = pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[]);
    let gate = Gate::Hold {
        solo: SOLO,
        phase: Phase::Admit,
        line: String::new(),
    };
    let entry = format!(
        r#"{{"position":1,"enqueuedAt":"2026-11-23T00:59:00Z","headCommit":{{"oid":"{PRODUCT:0>40}"}},"pullRequest":{{"number":{PRODUCT}}}}}"#
    );
    for (nodes, expected) in [
        (
            entry.as_str(),
            Some(Ok("solo_entry_waiting:1_ahead".to_owned())),
        ),
        ("", None),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let actions = queue_gh(&temp, nodes);
        let observation = observation_with(vec![solo.clone()]);
        let control = mutation_control(&temp, "studio", "studio");
        let ledger_path = temp.path().join("ledger.json");
        let mut context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
        context.solo_entry = Some(&gate);
        assert_eq!(pre_enqueue_refusal(&context, &solo), expected, "{nodes}");
    }
}

/// The exact snippet the docs give for Vellum loads from a real
/// machine-global config file.
#[test]
fn the_documented_setting_loads_from_the_machine_global_config() {
    let temp = tempfile::tempdir().expect("temp");
    fs::write(
        temp.path().join("config.toml"),
        "[merge_steward.solo_entry.\"Generous-Corp/vellum\"]\n\
         branches = [\"automation/pulp-observatory-coalesced\"]\n\
         version_file = \"CMakeLists.txt\"\n",
    )
    .expect("config");
    let settings = Settings::from_machine_global(temp.path());
    let mut observation = observation_with(vec![pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[])]);
    "Generous-Corp/vellum".clone_into(&mut observation.repo);
    let reads = FakeReads::release(&[]);
    let gate = run(
        &settings,
        &observation,
        &reads,
        &mut StewardLedger::default(),
        now(),
    );
    assert!(
        matches!(
            gate,
            Gate::Hold {
                solo: SOLO,
                phase: Phase::Admit,
                ..
            }
        ),
        "{gate:?}"
    );
}
