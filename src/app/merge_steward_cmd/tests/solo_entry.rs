//! Solo-entry admission: a release reconcile lands with nothing ahead of it.
use std::cell::RefCell;

use super::*;
use crate::app::merge_steward_cmd::pr_mutations::mutate_pr_with_recovery;
use crate::app::merge_steward_cmd::solo_entry::{
    Attempt, Gate, MAX_ATTEMPTS, MAX_OWN_DEQUEUES, MAX_READ_FAILURES, MAX_REFUSED_PASSES,
    ObserveInput, Phase, Reads, RepoSetting, Settings, TagState, Window, admission_hold,
    attempt_expired, behind_by_zero, file_version, hold_engaged, observe, pre_enqueue_refusal,
    record_native_arms, release_published, settle, tag_state, verify_after_enqueue, window_key,
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
        "author": {"login": "app/shipyard-local"},
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

/// `pr` with another head.
fn rebuilt(mut pr: ObservedPr, head: &str) -> ObservedPr {
    pr.fact.head_sha = head.repeat(40);
    pr
}

/// [`Reads`] from fixed answers, recording every call.
struct FakeReads {
    pending: Result<Option<String>, String>,
    approved: bool,
    same_repo: Result<bool, String>,
    up_to_date: Result<bool, String>,
    state: Result<String, String>,
    queue: Result<BTreeMap<u64, u64>, String>,
    calls: RefCell<Vec<String>>,
    dequeued: RefCell<Vec<u64>>,
    disarmed: RefCell<Vec<u64>>,
}

impl FakeReads {
    fn release(queue: &[(u64, u64)]) -> Self {
        Self {
            pending: Ok(Some("v0.2.2".to_owned())),
            approved: true,
            same_repo: Ok(true),
            up_to_date: Ok(true),
            state: Ok("merged".to_owned()),
            queue: Ok(queue.iter().copied().collect()),
            calls: RefCell::default(),
            dequeued: RefCell::default(),
            disarmed: RefCell::default(),
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
    fn same_repo(&self, pr: &ObservedPr) -> Result<bool, String> {
        self.calls
            .borrow_mut()
            .push(format!("same_repo:{}", pr.fact.number));
        self.same_repo.clone()
    }
    fn queue(&self) -> Result<BTreeMap<u64, u64>, String> {
        self.calls.borrow_mut().push("queue".to_owned());
        self.queue.clone()
    }
    fn up_to_date(&self, _pr: &ObservedPr) -> Result<bool, String> {
        self.calls.borrow_mut().push("compare".to_owned());
        self.up_to_date.clone()
    }
    fn pr_state(&self, number: u64) -> Result<String, String> {
        self.calls.borrow_mut().push(format!("state:{number}"));
        self.state.clone()
    }
    fn dequeue(&self, pr: &ObservedPr, _ledger: &mut StewardLedger) -> Result<(), String> {
        self.dequeued.borrow_mut().push(pr.fact.number);
        Ok(())
    }
    fn disarm(&self, pr: &ObservedPr, _ledger: &mut StewardLedger) -> Result<(), String> {
        self.disarmed.borrow_mut().push(pr.fact.number);
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

/// [`enqueue_under`] with the pass's base-poison reading.
#[cfg(unix)]
fn enqueue_under_poison(
    gate: &Gate,
    pr: &ObservedPr,
    poison: Option<&crate::app::merge_steward_cmd::base_poison::RepoPoison>,
) -> (Option<String>, String) {
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
    context.base_poison = poison;
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

fn solo_pr() -> ObservedPr {
    pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[])
}

fn product_pr() -> ObservedPr {
    pr(PRODUCT, "feature", false, "SUCCESS", &[])
}

/// The product pull request and the solo one, neither queued.
fn both() -> RepoObservation {
    observation_with(vec![product_pr(), solo_pr()])
}

fn settings() -> Settings {
    settings_for("owner/repo")
}

fn window(ledger: &StewardLedger) -> &Window {
    ledger
        .solo_entry_windows
        .get(&window_key("owner/repo", "main"))
        .expect("window")
}

fn attempt(ledger: &StewardLedger) -> &Attempt {
    window(ledger).attempt.as_ref().expect("engaged hold")
}

fn minutes(count: i64) -> chrono::Duration {
    chrono::Duration::minutes(count)
}

fn is_hold(gate: &Gate) -> bool {
    matches!(gate, Gate::Hold { .. })
}

/// A gate that holds nothing: no token for either row, backstop free.
fn assert_free(gate: &Gate) {
    assert!(matches!(gate, Gate::Inactive { .. }), "{gate:?}");
    assert_eq!(gate.enqueue_hold(PRODUCT), None, "{gate:?}");
    assert_eq!(gate.enqueue_hold(SOLO), None, "{gate:?}");
    assert!(gate.backstop_allowed(), "{gate:?}");
}

fn report(number: u64, mutation: Option<&str>, error: Option<&str>) -> PrReport {
    PrReport {
        number,
        head_sha: format!("{number:0>40}"),
        decision: StewardDecision::ArmMergeQueue,
        mutation: mutation.map(str::to_owned),
        error: error.map(str::to_owned),
        blocked: crate::app::merge_steward_cmd::blocked_on::Stall::default(),
        required_checks: Vec::new(),
    }
}

#[test]
fn without_a_setting_nothing_is_read_and_nothing_is_held() {
    let observation = both();
    let reads = FakeReads::release(&[(99, 1)]);
    let mut ledger = StewardLedger::default();
    // State left by a setting since removed (a rollback) is forgotten.
    let mut left = Window::new("v0.2.2", now());
    left.attempt = Some(Attempt {
        solo: SOLO,
        head: format!("{SOLO:0>40}"),
        engaged_at: now(),
        deadline: now() + minutes(120),
        refused: 0,
        queued: false,
        own_dequeues: 0,
    });
    let key = window_key("owner/repo", "main");
    ledger.solo_entry_windows.insert(key.clone(), left);
    ledger
        .solo_entry_native_arms
        .insert(key, [(PRODUCT, format!("{PRODUCT:0>40}"))].into());

    // Another repository's setting does not reach this one.
    for settings in [Settings::default(), settings_for("Generous-Corp/vellum")] {
        let gate = run(&settings, &observation, &reads, &mut ledger, now());
        assert_eq!(gate, Gate::Off);
        assert_eq!(gate.line(), None);
        assert!(gate.backstop_allowed());
        assert_eq!(gate.enqueue_hold(PRODUCT), None);
        assert_eq!(gate.enqueue_hold(SOLO), None);
        assert_eq!(admission_hold(Some(&gate), None, PRODUCT), None);
    }
    assert!(reads.calls.borrow().is_empty(), "{:?}", reads.calls);
    assert!(reads.disarmed.borrow().is_empty());
    assert!(ledger.solo_entry_windows.is_empty());
    assert!(ledger.solo_entry_native_arms.is_empty());
    assert!(!hold_engaged(&ledger, "owner/repo", "main", now()));
}

#[test]
fn no_release_pending_means_the_mode_is_inactive() {
    let observation = both();
    let mut reads = FakeReads::release(&[(99, 1)]);
    reads.pending = Ok(None);
    let mut ledger = StewardLedger::default();
    let gate = run(&settings(), &observation, &reads, &mut ledger, now());
    assert_free(&gate);
    assert!(gate.line().unwrap().contains("no release pending"));
    assert!(ledger.solo_entry_windows.is_empty());

    // With no window on record, an unreadable release state holds nothing.
    reads.pending = Err("HTTP 502".to_owned());
    let gate = run(&settings(), &observation, &reads, &mut ledger, now());
    assert_free(&gate);
    assert!(gate.line().unwrap().contains("release state unknown"));
    assert!(ledger.solo_entry_windows.is_empty());
}

#[test]
fn a_ready_solo_pr_with_a_non_empty_queue_holds_everything_and_waits() {
    let reads = FakeReads::release(&[(99, 1), (98, 2)]);
    let mut ledger = StewardLedger::default();
    let gate = run(&settings(), &both(), &reads, &mut ledger, now());
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
        line.contains("#7")
            && line.contains("2 queue entries ahead")
            && line.contains("HOLD ENGAGED (attempt 1 of 3"),
        "{line}"
    );
    assert!(reads.dequeued.borrow().is_empty());
    assert!(hold_engaged(&ledger, "owner/repo", "main", now()));
}

#[cfg(unix)]
#[test]
fn held_rows_reach_no_github_call_on_the_enqueue_path() {
    let gate = Gate::Hold {
        solo: SOLO,
        phase: Phase::Waiting { ahead: 1 },
        line: String::new(),
    };
    let (mutation, calls) = enqueue_under(&gate, &product_pr());
    assert_eq!(mutation.as_deref(), Some("held_solo_entry:#7"));
    assert!(calls.is_empty(), "{calls}");
    let (mutation, calls) = enqueue_under(&gate, &solo_pr());
    assert_eq!(mutation.as_deref(), Some("solo_entry_waiting:1_ahead"));
    assert!(calls.is_empty(), "{calls}");
    // The control: with the gate off the same path goes on to read GitHub.
    let (_, calls) = enqueue_under(&Gate::Off, &product_pr());
    assert!(!calls.is_empty());
}

#[test]
fn a_ready_solo_pr_with_an_empty_queue_and_a_current_head_is_the_only_admission() {
    let reads = FakeReads::release(&[]);
    let mut ledger = StewardLedger::default();
    let gate = run(&settings(), &both(), &reads, &mut ledger, now());
    assert_eq!(gate.enqueue_hold(SOLO), None, "{gate:?}");
    assert_eq!(
        gate.enqueue_hold(PRODUCT).as_deref(),
        Some("held_solo_entry:#7")
    );
    assert!(!gate.backstop_allowed());
    assert!(gate.line().unwrap().contains("admitting it alone"));
    assert!(reads.calls.borrow().contains(&"compare".to_owned()));
}

/// Review fix 1: an empty queue is not enough. A head behind the base tip
/// would merge alone and still be refused by the tag gate, so it waits.
#[test]
fn a_stale_solo_head_is_never_admitted_into_an_empty_queue() {
    let mut reads = FakeReads::release(&[]);
    reads.up_to_date = Ok(false);
    let mut ledger = StewardLedger::default();
    let gate = run(&settings(), &both(), &reads, &mut ledger, now());
    assert!(
        matches!(
            gate,
            Gate::Hold {
                phase: Phase::Stale,
                ..
            }
        ),
        "{gate:?}"
    );
    assert_eq!(
        gate.enqueue_hold(SOLO).as_deref(),
        Some("solo_entry_waiting:behind_base")
    );
    assert_eq!(
        gate.enqueue_hold(PRODUCT).as_deref(),
        Some("held_solo_entry:#7"),
        "the hold continues while its owner rebuilds it"
    );
    assert!(
        gate.line()
            .unwrap()
            .contains("solo_entry_waiting:behind_base")
    );

    // A comparison that cannot be read admits nothing either.
    reads.up_to_date = Err("solo head compare unreadable: HTTP 502".to_owned());
    let gate = run(&settings(), &both(), &reads, &mut ledger, now());
    assert_eq!(
        gate.enqueue_hold(SOLO).as_deref(),
        Some("solo_entry_base_unreadable")
    );
    assert!(gate.line().unwrap().contains("HTTP 502"));
    assert_eq!(behind_by_zero("0\n"), Ok(true));
    assert_eq!(behind_by_zero("3"), Ok(false));
    assert!(behind_by_zero("").is_err());
    assert!(behind_by_zero("null").is_err());
}

/// Mutant "unreadable queue admits": a queue that cannot be read is never
/// an empty queue.
#[test]
fn an_unreadable_queue_admits_nothing() {
    let mut reads = FakeReads::release(&[]);
    reads.queue = Err("HTTP 502".to_owned());
    let mut ledger = StewardLedger::default();
    let gate = run(&settings(), &both(), &reads, &mut ledger, now());
    assert!(
        matches!(
            gate,
            Gate::Hold {
                phase: Phase::QueueUnreadable,
                ..
            }
        ),
        "{gate:?}"
    );
    assert_eq!(
        gate.enqueue_hold(SOLO).as_deref(),
        Some("solo_entry_queue_unreadable")
    );
    assert_eq!(
        gate.enqueue_hold(PRODUCT).as_deref(),
        Some("held_solo_entry:#7")
    );
    assert!(gate.line().unwrap().contains("HTTP 502"));
    assert!(reads.dequeued.borrow().is_empty());
}

#[test]
fn while_the_solo_entry_is_queued_nothing_is_admitted_behind_it() {
    let observation = observation_with(vec![
        product_pr(),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[(SOLO, 1)]),
    ]);
    let reads = FakeReads::release(&[(SOLO, 1)]);
    let mut ledger = StewardLedger::default();
    let gate = run(&settings(), &observation, &reads, &mut ledger, now());
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
    assert!(!gate.line().unwrap().contains("shared_batch_risk"));
    assert!(reads.dequeued.borrow().is_empty());
    assert!(attempt(&ledger).queued);
}

/// Review fix 7: an entry behind the solo entry can merge in its push. It is
/// reported by number and never removed: it is not the steward's entry.
#[test]
fn an_entry_behind_the_solo_entry_is_reported_and_never_dequeued() {
    let observation = observation_with(vec![
        pr(PRODUCT, "feature", false, "SUCCESS", &[(PRODUCT, 2)]),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[(SOLO, 1)]),
    ]);
    let reads = FakeReads::release(&[(SOLO, 1), (PRODUCT, 2)]);
    let mut ledger = StewardLedger::default();
    let gate = run(&settings(), &observation, &reads, &mut ledger, now());
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
    let line = gate.line().expect("line");
    assert!(line.contains("solo_entry_shared_batch_risk: #42"), "{line}");
    assert!(reads.dequeued.borrow().is_empty(), "nothing is dequeued");
    assert!(reads.disarmed.borrow().is_empty());
}

#[test]
fn a_solo_entry_queued_behind_another_removes_only_itself() {
    let observation = observation_with(vec![
        pr(PRODUCT, "feature", false, "SUCCESS", &[(PRODUCT, 1)]),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[(SOLO, 2)]),
    ]);
    let reads = FakeReads::release(&[(PRODUCT, 1), (SOLO, 2)]);
    let mut ledger = StewardLedger::default();
    let gate = run(&settings(), &observation, &reads, &mut ledger, now());
    assert_eq!(*reads.dequeued.borrow(), vec![SOLO]);
    assert_eq!(
        gate.enqueue_hold(SOLO).as_deref(),
        Some("solo_entry_waiting:1_ahead"),
        "{gate:?}"
    );
    assert!(gate.line().unwrap().contains("dequeued its own entry"));
    // Its own removal is not an ejection: the hold stays engaged.
    assert_eq!(attempt(&ledger).own_dequeues, 1);
    assert!(!attempt(&ledger).queued);
}

/// Review item 9: every own dequeue and re-enqueue starts a merge-group
/// run, so one attempt makes at most three.
#[test]
fn own_dequeues_are_capped_per_attempt() {
    let observation = observation_with(vec![
        pr(PRODUCT, "feature", false, "SUCCESS", &[(PRODUCT, 1)]),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[(SOLO, 2)]),
    ]);
    let reads = FakeReads::release(&[(PRODUCT, 1), (SOLO, 2)]);
    let mut ledger = StewardLedger::default();
    for pass in 1..=MAX_OWN_DEQUEUES {
        let gate = run(&settings(), &observation, &reads, &mut ledger, now());
        assert!(is_hold(&gate), "pass {pass}: {gate:?}");
        assert_eq!(attempt(&ledger).own_dequeues, pass);
    }
    assert_eq!(reads.dequeued.borrow().len(), MAX_OWN_DEQUEUES as usize);
    let gate = run(&settings(), &observation, &reads, &mut ledger, now());
    assert_free(&gate);
    assert!(gate.line().unwrap().contains("HOLD RELEASED"), "{gate:?}");
    assert_eq!(
        reads.dequeued.borrow().len(),
        MAX_OWN_DEQUEUES as usize,
        "no fourth dequeue"
    );
    assert!(window(&ledger).is_released(SOLO, &format!("{SOLO:0>40}")));
}

/// Before any hold has engaged, a solo pull request that is not
/// admission-ready, or is not the fleet App's own branch in this repository
/// (earlier review F4), changes nothing.
#[test]
fn a_solo_pr_that_cannot_engage_a_hold_changes_nothing() {
    let other_author = {
        let mut pr = solo_pr();
        pr.fact.author = Some("someone".to_owned());
        pr
    };
    let no_author = {
        let mut pr = solo_pr();
        pr.fact.author = None;
        pr
    };
    for (name, solo, approved, same_repo) in [
        (
            "red",
            pr(SOLO, SOLO_BRANCH, false, "FAILURE", &[]),
            true,
            Ok(true),
        ),
        (
            "draft",
            pr(SOLO, SOLO_BRANCH, true, "SUCCESS", &[]),
            true,
            Ok(true),
        ),
        ("unapproved", solo_pr(), false, Ok(true)),
        ("another author", other_author, true, Ok(true)),
        ("no author", no_author, true, Ok(true)),
        ("a fork's branch", solo_pr(), true, Ok(false)),
        (
            "head repository unreadable",
            solo_pr(),
            true,
            Err("HTTP 502".to_owned()),
        ),
    ] {
        let mut ledger = StewardLedger::default();
        let observation = observation_with(vec![product_pr(), solo]);
        let mut reads = FakeReads::release(&[(99, 1)]);
        reads.approved = approved;
        reads.same_repo = same_repo;
        let gate = run(&settings(), &observation, &reads, &mut ledger, now());
        assert_free(&gate);
        assert!(reads.dequeued.borrow().is_empty(), "{name}");
        assert!(
            !hold_engaged(&ledger, "owner/repo", "main", now()),
            "{name}"
        );
        assert_eq!(window(&ledger).attempts, 0, "{name}");
    }
}

/// Review fix 3, earlier review F2: the clock starts when the hold engages.
/// A window that has been open for hours with no solo pull request has spent
/// nothing.
#[test]
fn the_hold_clock_starts_when_the_hold_engages_not_when_the_window_opens() {
    let reads = FakeReads::release(&[(99, 1)]);
    let mut ledger = StewardLedger::default();
    let opened = now();
    let gate = run(
        &settings(),
        &observation_with(vec![product_pr()]),
        &reads,
        &mut ledger,
        opened,
    );
    assert_free(&gate);
    assert!(gate.line().unwrap().contains("no admission-ready"));
    assert_eq!(window(&ledger).opened_at, opened);
    assert!(!hold_engaged(&ledger, "owner/repo", "main", opened));

    // Five hours later, far past `window_minutes`, the reconcile is ready.
    let engaged = opened + minutes(300);
    let gate = run(&settings(), &both(), &reads, &mut ledger, engaged);
    assert!(is_hold(&gate), "{gate:?}");
    assert_eq!(attempt(&ledger).engaged_at, engaged);
    assert_eq!(attempt(&ledger).deadline, engaged + minutes(120));

    let gate = run(
        &settings(),
        &both(),
        &reads,
        &mut ledger,
        engaged + minutes(119),
    );
    assert!(is_hold(&gate), "{gate:?}");
    assert!(hold_engaged(
        &ledger,
        "owner/repo",
        "main",
        engaged + minutes(119)
    ));

    let expired = engaged + minutes(120);
    let gate = run(&settings(), &both(), &reads, &mut ledger, expired);
    assert_free(&gate);
    let line = gate.line().expect("line");
    assert!(line.contains("RELEASE WINDOW EXPIRED"), "{line}");
    // The minutes it held, counted from the engage (the window opened 5 h
    // earlier, and the setting may have changed since).
    assert!(line.contains("ended after 120 min"), "{line}");
    assert!(!hold_engaged(&ledger, "owner/repo", "main", expired));

    // The expiry is logged once, and the same head never holds again.
    let gate = run(
        &settings(),
        &both(),
        &reads,
        &mut ledger,
        expired + minutes(30),
    );
    assert_free(&gate);
    assert!(!gate.line().unwrap().contains("EXPIRED"));
    assert_eq!(window(&ledger).attempts, 1);
}

/// Review fix 3: after an expiry only a new head engages again, and a
/// version gets at most three holds.
#[test]
fn a_new_head_starts_the_next_attempt_and_a_version_gets_three() {
    let reads = FakeReads::release(&[(99, 1)]);
    let mut ledger = StewardLedger::default();
    let mut at = now();
    for (number, head) in ["a", "b", "c"].into_iter().enumerate() {
        let observation = observation_with(vec![product_pr(), rebuilt(solo_pr(), head)]);
        let gate = run(&settings(), &observation, &reads, &mut ledger, at);
        assert!(is_hold(&gate), "attempt {}: {gate:?}", number + 1);
        assert!(
            gate.line()
                .unwrap()
                .contains(&format!("attempt {} of 3", number + 1)),
            "{gate:?}"
        );
        at += minutes(120);
        let gate = run(&settings(), &observation, &reads, &mut ledger, at);
        assert_free(&gate);
        assert!(gate.line().unwrap().contains("EXPIRED"), "{gate:?}");
        at += minutes(1);
    }
    assert_eq!(window(&ledger).attempts, MAX_ATTEMPTS);
    let observation = observation_with(vec![product_pr(), rebuilt(solo_pr(), "d")]);
    let gate = run(&settings(), &observation, &reads, &mut ledger, at);
    assert_free(&gate);
    assert!(gate.line().unwrap().contains("attempts"), "{gate:?}");
    assert!(!hold_engaged(&ledger, "owner/repo", "main", at));
}

/// Mutant "backwards clock does not expire": a clock that moved back must
/// end the hold, never extend it.
#[test]
fn a_clock_that_moves_back_ends_the_hold() {
    let reads = FakeReads::release(&[(99, 1)]);
    let mut ledger = StewardLedger::default();
    let engaged = now();
    assert!(is_hold(&run(
        &settings(),
        &both(),
        &reads,
        &mut ledger,
        engaged
    )));
    let earlier = engaged - minutes(1);
    assert!(attempt_expired(attempt(&ledger), earlier));
    assert!(!hold_engaged(&ledger, "owner/repo", "main", earlier));
    let gate = run(&settings(), &both(), &reads, &mut ledger, earlier);
    assert_free(&gate);
    assert!(gate.line().unwrap().contains("EXPIRED"), "{gate:?}");
}

/// Review fix 2: once a hold has engaged it continues while the reconcile
/// is rebuilt on the new base tip and waits for review, and ends when the
/// pull request merges.
#[test]
fn once_engaged_the_hold_continues_while_the_solo_pr_is_rebuilt() {
    let mut reads = FakeReads::release(&[]);
    let mut ledger = StewardLedger::default();
    assert!(is_hold(&run(
        &settings(),
        &both(),
        &reads,
        &mut ledger,
        now()
    )));

    // Rebuilt: a new head, not approved yet.
    reads.approved = false;
    reads.calls.borrow_mut().clear();
    let observation = observation_with(vec![product_pr(), rebuilt(solo_pr(), "b")]);
    let later = now() + minutes(10);
    let gate = run(&settings(), &observation, &reads, &mut ledger, later);
    assert!(
        matches!(
            gate,
            Gate::Hold {
                solo: SOLO,
                phase: Phase::NotReady,
                ..
            }
        ),
        "{gate:?}"
    );
    assert_eq!(
        gate.enqueue_hold(PRODUCT).as_deref(),
        Some("held_for_solo_attempt:#7")
    );
    assert_eq!(
        gate.enqueue_hold(SOLO).as_deref(),
        Some("solo_entry_waiting:not_ready"),
        "a head that is not ready is never enqueued by the ordinary path"
    );
    assert!(!gate.backstop_allowed());
    assert!(gate.line().unwrap().contains("held_for_solo_attempt"));
    assert!(!reads.calls.borrow().contains(&"queue".to_owned()));
    // The hold follows the new head on the same clock.
    assert_eq!(attempt(&ledger).head, "b".repeat(40));
    assert_eq!(attempt(&ledger).deadline, now() + minutes(120));
    assert_eq!(window(&ledger).attempts, 1);

    // The control: the same unapproved head with no hold engaged holds nothing.
    let gate = run(
        &settings(),
        &observation,
        &reads,
        &mut StewardLedger::default(),
        later,
    );
    assert_free(&gate);

    // Review fix 8: once the solo pull request has merged nothing is held
    // for it, though the release is still unpublished.
    let merged = observation_with(vec![product_pr()]);
    let gate = run(
        &settings(),
        &merged,
        &reads,
        &mut ledger,
        later + minutes(5),
    );
    assert_free(&gate);
    assert!(gate.line().unwrap().contains("HOLD RELEASED"), "{gate:?}");
    assert!(window(&ledger).attempt.is_none());
    assert!(!hold_engaged(
        &ledger,
        "owner/repo",
        "main",
        later + minutes(5)
    ));
}

/// Re-review D: the hold ends when its solo pull request merges, even when
/// the next reconcile is already open. That one starts its own attempt, with
/// its own head-repository read, under the attempt cap.
#[test]
fn the_hold_ends_on_the_merge_and_the_next_reconcile_is_a_new_attempt() {
    let mut reads = FakeReads::release(&[]);
    let mut ledger = StewardLedger::default();
    assert!(is_hold(&run(
        &settings(),
        &both(),
        &reads,
        &mut ledger,
        now()
    )));
    // #7 merged, and the next reconcile #8 opened before this pass.
    let next = observation_with(vec![
        product_pr(),
        pr(8, SOLO_BRANCH, false, "SUCCESS", &[]),
    ]);
    reads.approved = false;
    reads.calls.borrow_mut().clear();
    let gate = run(&settings(), &next, &reads, &mut ledger, now() + minutes(7));
    assert_free(&gate);
    let line = gate.line().expect("line");
    assert!(
        line.contains("HOLD RELEASED") && line.contains("#7 merged"),
        "{line}"
    );
    assert!(reads.calls.borrow().contains(&"state:7".to_owned()));
    assert!(window(&ledger).attempt.is_none());

    // #8 engages its own attempt once ready, and only from this repository.
    reads.approved = true;
    reads.same_repo = Ok(false);
    assert_free(&run(
        &settings(),
        &next,
        &reads,
        &mut ledger,
        now() + minutes(12),
    ));
    reads.same_repo = Ok(true);
    let gate = run(&settings(), &next, &reads, &mut ledger, now() + minutes(17));
    assert!(matches!(gate, Gate::Hold { solo: 8, .. }), "{gate:?}");
    assert_eq!(window(&ledger).attempts, 2);
    assert_eq!(attempt(&ledger).engaged_at, now() + minutes(17));
}

/// A census that misses the solo pull request for one pass does not end the
/// hold: GitHub still says it is open. Neither does an unreadable state.
#[test]
fn a_census_that_misses_the_solo_pr_keeps_the_hold() {
    let mut reads = FakeReads::release(&[]);
    let mut ledger = StewardLedger::default();
    assert!(is_hold(&run(
        &settings(),
        &both(),
        &reads,
        &mut ledger,
        now()
    )));
    let missing = observation_with(vec![
        product_pr(),
        // Another pull request on the solo branch does not take the hold over.
        pr(8, SOLO_BRANCH, false, "SUCCESS", &[]),
    ]);
    for state in [Ok("open".to_owned()), Err("HTTP 502".to_owned())] {
        reads.state = state;
        let gate = run(
            &settings(),
            &missing,
            &reads,
            &mut ledger,
            now() + minutes(5),
        );
        assert!(
            matches!(
                gate,
                Gate::Hold {
                    solo: SOLO,
                    phase: Phase::NotReady,
                    ..
                }
            ),
            "{gate:?}"
        );
        assert_eq!(
            gate.enqueue_hold(PRODUCT).as_deref(),
            Some("held_for_solo_attempt:#7")
        );
        assert_eq!(
            gate.enqueue_hold(8).as_deref(),
            Some("held_for_solo_attempt:#7")
        );
        assert!(gate.line().unwrap().contains("hold kept"), "{gate:?}");
    }
    assert_eq!(window(&ledger).attempts, 1);
}

/// Re-review C: an entry the tick's own admission did not create (a ready
/// event, a person, a native arm) may be stale. A queued solo entry is
/// compared with the base, and a stale one is the steward's own to remove.
#[test]
fn a_queued_solo_entry_behind_the_base_removes_itself() {
    let observation = observation_with(vec![
        product_pr(),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[(SOLO, 1)]),
    ]);
    let mut reads = FakeReads::release(&[(SOLO, 1)]);
    reads.up_to_date = Ok(false);
    let mut ledger = StewardLedger::default();
    let gate = run(&settings(), &observation, &reads, &mut ledger, now());
    assert_eq!(*reads.dequeued.borrow(), vec![SOLO]);
    assert!(
        matches!(
            gate,
            Gate::Hold {
                phase: Phase::Stale,
                ..
            }
        ),
        "{gate:?}"
    );
    assert_eq!(
        gate.enqueue_hold(SOLO).as_deref(),
        Some("solo_entry_waiting:behind_base")
    );
    assert_eq!(
        gate.enqueue_hold(PRODUCT).as_deref(),
        Some("held_solo_entry:#7")
    );
    let line = gate.line().expect("line");
    assert!(
        line.contains("dequeued its own entry (behind the base tip)"),
        "{line}"
    );
    assert_eq!(attempt(&ledger).own_dequeues, 1);
    assert!(
        !attempt(&ledger).queued,
        "its own removal is not an ejection"
    );

    // A comparison that cannot be read removes nothing.
    let mut unread = FakeReads::release(&[(SOLO, 1)]);
    unread.up_to_date = Err("HTTP 502".to_owned());
    let gate = run(
        &settings(),
        &observation,
        &unread,
        &mut StewardLedger::default(),
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
    assert!(unread.dequeued.borrow().is_empty());
}

/// Re-review F: past its window with the solo entry still in the merge
/// queue, the hold stays (letting go would admit the backlog into its push)
/// until the queue decides, for at most one more `window_minutes`.
#[test]
fn a_hold_past_its_window_stays_while_the_solo_entry_is_queued() {
    let queued = observation_with(vec![
        product_pr(),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[(SOLO, 1)]),
    ]);
    let reads = FakeReads::release(&[(SOLO, 1)]);
    let mut ledger = StewardLedger::default();
    assert!(is_hold(&run(
        &settings(),
        &queued,
        &reads,
        &mut ledger,
        now()
    )));
    let past = now() + minutes(125);
    let gate = run(&settings(), &queued, &reads, &mut ledger, past);
    assert_eq!(
        gate.enqueue_hold(PRODUCT).as_deref(),
        Some("held_solo_entry:#7"),
        "{gate:?}"
    );
    assert!(gate.line().unwrap().contains("HOLD EXTENDED"), "{gate:?}");
    // The ledger says so too, so `steward-admit-ready` keeps skipping.
    assert!(hold_engaged(&ledger, "owner/repo", "main", past));
    assert!(hold_engaged(
        &ledger,
        "owner/repo",
        "main",
        past + minutes(5)
    ));

    // The same moment with the solo not queued: the hold expires.
    let mut idle = StewardLedger::default();
    let waiting = FakeReads::release(&[(99, 1)]);
    assert!(is_hold(&run(
        &settings(),
        &both(),
        &waiting,
        &mut idle,
        now()
    )));
    let gate = run(&settings(), &both(), &waiting, &mut idle, past);
    assert_free(&gate);
    assert!(gate.line().unwrap().contains("EXPIRED"), "{gate:?}");

    // The extension is bounded: at twice the window it expires even queued.
    let gate = run(
        &settings(),
        &queued,
        &reads,
        &mut ledger,
        now() + minutes(240),
    );
    assert_free(&gate);
    assert!(gate.line().unwrap().contains("EXPIRED"), "{gate:?}");
    // A clock that moved back still ends it, queued or not.
    let mut back = StewardLedger::default();
    assert!(is_hold(&run(
        &settings(),
        &queued,
        &reads,
        &mut back,
        now()
    )));
    assert_free(&run(
        &settings(),
        &queued,
        &reads,
        &mut back,
        now() - minutes(1),
    ));
}

/// A head pushed while the solo entry is queued takes the entry out of the
/// queue. That is a rebuild, not "the queue removed it": the hold stays on
/// the new head, on that pass and on the next.
#[test]
fn a_head_pushed_while_the_solo_entry_is_queued_keeps_the_hold() {
    let queued = observation_with(vec![
        product_pr(),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[(SOLO, 1)]),
    ]);
    let mut ledger = StewardLedger::default();
    assert!(is_hold(&run(
        &settings(),
        &queued,
        &FakeReads::release(&[(SOLO, 1)]),
        &mut ledger,
        now(),
    )));
    assert!(attempt(&ledger).queued);

    let pushed = observation_with(vec![product_pr(), rebuilt(solo_pr(), "b")]);
    let mut reads = FakeReads::release(&[]);
    reads.approved = false;
    for pass in 1..=2 {
        let gate = run(
            &settings(),
            &pushed,
            &reads,
            &mut ledger,
            now() + minutes(5 * pass),
        );
        assert_eq!(
            gate.enqueue_hold(PRODUCT).as_deref(),
            Some("held_for_solo_attempt:#7"),
            "pass {pass}: {gate:?}"
        );
        assert_eq!(attempt(&ledger).head, "b".repeat(40));
        assert!(!attempt(&ledger).queued, "the new head was never queued");
    }
    assert!(window(&ledger).released.is_empty());
}

/// Mutant "the real disarm sends nothing", and re-review E's live re-read:
/// the disarm against GitHub reads the pull request again under the guard
/// and disables only a request still armed on the same head, not queued.
#[cfg(unix)]
#[test]
fn the_real_disarm_rereads_the_pull_request_and_disables_only_its_own_arm() {
    use crate::app::merge_steward_cmd::solo_entry::GitHubReads;
    let product = product_pr();
    let head = product.fact.head_sha.clone();
    let observation = observation_with(vec![product.clone(), solo_pr()]);
    let disarm = |live_head: &str, request: &str, in_queue: bool| {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls");
        let actions = fake_gh(
            &temp,
            &format!(
                r#"printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"disablePullRequestAutoMerge"*) printf '%s' '{{"data":{{"disablePullRequestAutoMerge":{{"pullRequest":{{"number":{PRODUCT},"autoMergeRequest":null}}}}}}}}' ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":{PRODUCT},"state":"OPEN","headRefOid":"{live_head}","isInMergeQueue":{in_queue},"mergeQueueEntry":null,"autoMergeRequest":{request},"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *) printf '%s' '{{}}' ;;
esac"#,
                log = log.display(),
            ),
        );
        let control = mutation_control(&temp, "studio", "studio");
        let ledger_path = temp.path().join("ledger.json");
        let reads = GitHubReads {
            actions: &actions,
            observation: &observation,
            control: Some(&control),
            ledger_path: &ledger_path,
        };
        let outcome = reads.disarm(&product, &mut StewardLedger::default());
        (outcome, fs::read_to_string(log).unwrap_or_default())
    };
    let armed = r#"{"enabledAt":"2026-11-23T00:00:00Z"}"#;

    let (outcome, calls) = disarm(&head, armed, false);
    assert_eq!(outcome, Ok(()), "{calls}");
    let sent = calls
        .lines()
        .filter(|line| line.contains("disablePullRequestAutoMerge"))
        .collect::<Vec<_>>();
    assert_eq!(sent.len(), 1, "{calls}");
    assert!(sent[0].contains("id=PR_node_42"), "{calls}");

    for (live_head, request, in_queue, why) in [
        ("e".repeat(40), armed, false, "head moved"),
        (head.clone(), "null", false, "no longer armed"),
        (head.clone(), armed, true, "in the merge queue"),
    ] {
        let (outcome, calls) = disarm(&live_head, request, in_queue);
        assert!(
            outcome.as_ref().is_err_and(|error| error.contains(why)),
            "{outcome:?}"
        );
        assert_eq!(count(&calls, "disablePullRequestAutoMerge"), 0, "{calls}");
    }
}

/// Mutant "the tick never counts refused passes" (`cancellation.rs`): an
/// applying tick whose gate admits the solo head and whose enqueue path
/// refuses it with a named token counts the pass in the ledger and says so.
#[cfg(unix)]
#[test]
fn a_tick_counts_a_pass_that_admitted_the_solo_head_and_did_not_enqueue_it() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
case "$*" in
  *"contents/CMakeLists.txt?ref=main"*) printf '%s\n' 'project(Vellum VERSION 0.2.2)' ;;
  *"git/matching-refs/tags/v"*) printf '%s\n' 'refs/tags/v0.2.1' ;;
  *"pulls/7 --jq .head.repo.full_name"*) printf '%s\n' 'owner/repo' ;;
  *"/compare/main..."*"behind_by"*) echo 0 ;;
  *"mergeQueue"*) {queue} ;;
  *enqueuePullRequest*|*enablePullRequestAutoMerge*|*dequeuePullRequest*|*disablePullRequestAutoMerge*) echo "no mutation is allowed: $*" >&2; exit 90 ;;
  *) printf '%s' '{{}}' ;;
esac
"#,
            queue = queue_answer(""),
        ),
    );
    let observation = observation_with(vec![solo_pr()]);
    let control = mutation_control(&temp, "studio", "studio");
    let mut ledger = StewardLedger::default();

    let (report, _, _) = apply_repo_plan(
        &actions,
        &tick_args(false),
        &observation,
        &temp.path().join("ledger.json"),
        &mut ledger,
        0,
        Some(&control),
        None,
    );

    let row = &report.prs[0];
    let token = row.mutation.clone().expect("a named refusal token");
    assert!(
        !token.contains("enqueued") && !token.starts_with("solo_entry_"),
        "{row:?}"
    );
    assert_eq!(attempt(&ledger).refused, 1, "{report:?}");
    let line = report.solo_entry.expect("solo_entry line");
    assert!(
        line.contains("admitted and not enqueued") && line.contains("pass 1 of 3"),
        "{line}"
    );
}

/// Mutant "the tick never records backstop arms" (`cancellation.rs`): an
/// applying tick whose backstop arms a pull request, in a repository with
/// the setting, leaves that arm in the ledger for a later hold to disable.
#[cfg(unix)]
#[test]
fn a_tick_records_the_auto_merge_its_backstop_armed() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let head = "a".repeat(40);
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"contents/CMakeLists.txt?ref=main"*) printf '%s\n' 'project(Vellum VERSION 0.2.2)' ;;
  *"git/matching-refs/tags/v"*) printf '%s\n' 'refs/tags/v0.2.1' ;;
  *enablePullRequestAutoMerge*) printf '%s' '{{"data":{{"enablePullRequestAutoMerge":{{"pullRequest":{{"number":42}}}}}}}}' ;;
  *isInMergeQueue*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{head}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *"mergeQueue"*) {queue} ;;
  *) printf '%s' '{{}}' ;;
esac
"#,
            log = log.display(),
            queue = queue_answer(""),
        ),
    );
    // Not the fleet App's and not handed off: the steward declines to own
    // it, so it is the backstop's.
    let observation = observation_for(ready_pr(), true);
    let control = mutation_control(&temp, "studio", "studio");
    let mut ledger = StewardLedger::default();

    let (report, _, _) = apply_repo_plan(
        &actions,
        &tick_args(true),
        &observation,
        &temp.path().join("ledger.json"),
        &mut ledger,
        0,
        Some(&control),
        None,
    );

    let calls = fs::read_to_string(&log).unwrap_or_default();
    let results = &report.native_auto_merge_backstop.results;
    assert_eq!(results.len(), 1, "{report:?}\n{calls}");
    assert_eq!(results[0].outcome, "armed", "{report:?}\n{calls}");
    assert_eq!(
        ledger.solo_entry_native_arms[&window_key("owner/repo", "main")],
        [(42, head)].into(),
    );
}

/// Review fix 5, earlier review F3: a solo head the queue removed is not
/// enqueued again, so the hold lets go of that head at once. A new head
/// engages a new attempt.
#[test]
fn a_solo_head_the_queue_removed_releases_the_hold() {
    let queued = observation_with(vec![
        product_pr(),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[(SOLO, 1)]),
    ]);
    let mut ledger = StewardLedger::default();
    let gate = run(
        &settings(),
        &queued,
        &FakeReads::release(&[(SOLO, 1)]),
        &mut ledger,
        now(),
    );
    assert!(is_hold(&gate), "{gate:?}");

    // Its merge group failed: open, same head, no queue entry.
    let reads = FakeReads::release(&[]);
    let gate = run(
        &settings(),
        &both(),
        &reads,
        &mut ledger,
        now() + minutes(30),
    );
    assert_free(&gate);
    let line = gate.line().expect("line");
    assert!(
        line.contains("HOLD RELEASED") && line.contains("left the merge queue unmerged"),
        "{line}"
    );
    // The same head does not hold again on the next pass.
    let gate = run(
        &settings(),
        &both(),
        &reads,
        &mut ledger,
        now() + minutes(35),
    );
    assert_free(&gate);
    // A new head does, as the second attempt.
    let observation = observation_with(vec![product_pr(), rebuilt(solo_pr(), "b")]);
    let gate = run(
        &settings(),
        &observation,
        &reads,
        &mut ledger,
        now() + minutes(40),
    );
    assert!(is_hold(&gate), "{gate:?}");
    assert_eq!(window(&ledger).attempts, 2);
}

/// Review fix 5: the gate admitted the solo head and the enqueue path
/// refused it (an unchanged ejected head, carried commits, a code-owner
/// wait). After three such passes the hold lets go of that head.
#[test]
fn a_solo_head_the_enqueue_path_refuses_releases_the_hold_after_three_passes() {
    let reads = FakeReads::release(&[]);
    let mut ledger = StewardLedger::default();
    let observation = both();
    let gate = run(&settings(), &observation, &reads, &mut ledger, now());
    assert_eq!(gate.enqueue_hold(SOLO), None, "{gate:?}");

    // Not refusals: a deferral, the guarded re-read's own wait, a failed
    // read, and an enqueue.
    for (mutation, error) in [
        (
            Some(crate::app::merge_steward_cmd::pass_budget::DEFERRED),
            None,
        ),
        (Some("solo_entry_waiting:1_ahead"), None),
        (Some("solo_entry_waiting:behind_base"), None),
        (
            None,
            Some("solo entry: merge queue unreadable before enqueue: HTTP 502"),
        ),
        // The operator's HOLD, and a host that is not the mutation machine:
        // nothing judged the head, so neither can end the hold for it.
        (
            None,
            Some("merge-queue mutations are centrally held by /state/merge-queue.HOLD"),
        ),
        (
            None,
            Some("this machine is not the merge-queue mutation machine"),
        ),
        (None, None),
        (Some("enqueued"), None),
    ] {
        let line = settle(
            &gate,
            &observation,
            &[report(SOLO, mutation, error)],
            None,
            true,
            &mut ledger,
        )
        .expect("line");
        assert_eq!(attempt(&ledger).refused, 0, "{mutation:?} {error:?}");
        assert!(!line.contains("not enqueued"), "{line}");
    }
    // A dry run counts nothing.
    settle(
        &gate,
        &observation,
        &[report(SOLO, Some("skipped_same_head_after_ejection"), None)],
        None,
        false,
        &mut ledger,
    );
    assert_eq!(attempt(&ledger).refused, 0);

    for pass in 1..MAX_REFUSED_PASSES {
        let line = settle(
            &gate,
            &observation,
            &[report(SOLO, Some("skipped_same_head_after_ejection"), None)],
            None,
            true,
            &mut ledger,
        )
        .expect("line");
        assert_eq!(attempt(&ledger).refused, pass);
        assert!(line.contains(&format!("pass {pass} of 3")), "{line}");
    }
    let line = settle(
        &gate,
        &observation,
        &[report(SOLO, Some("waiting_code_owner_review"), None)],
        None,
        true,
        &mut ledger,
    )
    .expect("line");
    assert!(
        line.contains("HOLD RELEASED for this head") && line.contains("waiting_code_owner_review"),
        "{line}"
    );
    assert!(window(&ledger).attempt.is_none());
    assert!(!hold_engaged(&ledger, "owner/repo", "main", now()));

    // The next pass holds nothing for that head.
    let gate = run(
        &settings(),
        &observation,
        &reads,
        &mut ledger,
        now() + minutes(5),
    );
    assert_free(&gate);
}

/// Review fix 6: one failed release read never drops a live hold. The last
/// known window is kept for three failed reads, then the mode fails open.
#[test]
fn a_failed_release_read_keeps_the_hold_for_three_passes_then_fails_open() {
    let mut reads = FakeReads::release(&[(SOLO, 1)]);
    let observation = observation_with(vec![
        product_pr(),
        pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[(SOLO, 1)]),
    ]);
    let mut ledger = StewardLedger::default();
    assert!(is_hold(&run(
        &settings(),
        &observation,
        &reads,
        &mut ledger,
        now()
    )));

    reads.pending = Err("HTTP 502".to_owned());
    for pass in 1..=MAX_READ_FAILURES {
        let gate = run(&settings(), &observation, &reads, &mut ledger, now());
        assert_eq!(
            gate.enqueue_hold(PRODUCT).as_deref(),
            Some("held_solo_entry:#7"),
            "failed read {pass}: {gate:?}"
        );
        assert!(
            gate.line().unwrap().contains("last known window kept"),
            "{gate:?}"
        );
        assert_eq!(window(&ledger).read_failures, pass);
    }
    // A read that works again clears the count and keeps the hold.
    let mut recovered = StewardLedger {
        solo_entry_windows: ledger.solo_entry_windows.clone(),
        ..StewardLedger::default()
    };
    reads.pending = Ok(Some("v0.2.2".to_owned()));
    let gate = run(&settings(), &observation, &reads, &mut recovered, now());
    assert!(is_hold(&gate), "{gate:?}");
    assert_eq!(window(&recovered).read_failures, 0);

    // The fourth failed read in a row fails open.
    reads.pending = Err("HTTP 502".to_owned());
    let gate = run(&settings(), &observation, &reads, &mut ledger, now());
    assert_free(&gate);
    assert!(
        gate.line().unwrap().contains("HOLD RELEASED (fail open)"),
        "{gate:?}"
    );
    assert!(!hold_engaged(&ledger, "owner/repo", "main", now()));
}

/// Review fix 7: while the hold is engaged the steward disables the
/// auto-merge requests its own backstop armed, and only those.
#[test]
fn only_auto_merge_the_steward_armed_is_disabled_for_the_hold() {
    let armed = |number: u64, queue: &[(u64, u64)]| {
        let mut pr = pr(number, "feature", false, "SUCCESS", queue);
        pr.fact.auto_merge_active = true;
        pr
    };
    let observation = observation_with(vec![
        armed(PRODUCT, &[]),
        // Armed by the steward and already queued: a queue entry is never touched.
        armed(50, &[(50, 1)]),
        // Armed by the steward at an earlier head.
        armed(51, &[]),
        // Armed by someone else.
        armed(52, &[]),
        // Recorded, no longer armed.
        pr(53, "feature", false, "SUCCESS", &[]),
        // The solo pull request's own request is never disabled.
        {
            let mut solo = solo_pr();
            solo.fact.auto_merge_active = true;
            solo
        },
    ]);
    let key = window_key("owner/repo", "main");
    let mut ledger = StewardLedger::default();
    ledger.solo_entry_native_arms.insert(
        key,
        [
            (PRODUCT, format!("{PRODUCT:0>40}")),
            (50, format!("{:0>40}", 50)),
            (51, "e".repeat(40)),
            (53, format!("{:0>40}", 53)),
            (SOLO, format!("{SOLO:0>40}")),
        ]
        .into(),
    );
    let reads = FakeReads::release(&[(50, 1)]);
    let gate = run(&settings(), &observation, &reads, &mut ledger, now());
    assert!(is_hold(&gate), "{gate:?}");
    assert_eq!(*reads.disarmed.borrow(), vec![PRODUCT]);
    assert!(
        gate.line()
            .unwrap()
            .contains("disabled the auto-merge the steward armed on #42"),
        "{gate:?}"
    );
    assert!(reads.dequeued.borrow().is_empty());
    // Re-review E: once disabled it is no longer the steward's arm, so a
    // request a person arms on that head later is never disabled.
    let arms = &ledger.solo_entry_native_arms[&window_key("owner/repo", "main")];
    assert!(!arms.contains_key(&PRODUCT), "{arms:?}");
    assert!(arms.contains_key(&50) && arms.contains_key(&51), "{arms:?}");
    let again = FakeReads::release(&[(50, 1)]);
    assert!(is_hold(&run(
        &settings(),
        &observation,
        &again,
        &mut ledger,
        now()
    )));
    assert!(
        again.disarmed.borrow().is_empty(),
        "armed again by a person"
    );

    // No hold, nothing disabled.
    let mut idle = FakeReads::release(&[]);
    idle.approved = false;
    let mut fresh = StewardLedger {
        solo_entry_native_arms: ledger.solo_entry_native_arms.clone(),
        ..StewardLedger::default()
    };
    assert_free(&run(&settings(), &observation, &idle, &mut fresh, now()));
    assert!(idle.disarmed.borrow().is_empty());
}

/// The record behind the disarm: what the backstop armed, kept only for a
/// repository with a setting and only while the head it armed is open.
#[test]
fn the_backstops_arms_are_recorded_only_for_a_configured_repository() {
    use crate::app::merge_steward_cmd::native_arm::NativeArmResult;
    let result = |number: u64, outcome: &str| NativeArmResult {
        number,
        head_sha: format!("{number:0>40}"),
        outcome: outcome.to_owned(),
        skip: None,
        error: None,
    };
    let observation = observation_with(vec![
        product_pr(),
        pr(43, "feature", false, "SUCCESS", &[]),
        pr(44, "feature", false, "SUCCESS", &[]),
        pr(45, "feature", false, "SUCCESS", &[]),
    ]);
    let results = [
        result(PRODUCT, "armed"),
        result(43, "rearmed_same_head"),
        result(44, "skipped"),
        result(45, "would_arm"),
    ];
    let key = window_key("owner/repo", "main");

    // No setting, or a dry run: the ledger is untouched.
    let mut ledger = StewardLedger::default();
    record_native_arms(
        &Settings::default(),
        &observation,
        &results,
        true,
        &mut ledger,
    );
    record_native_arms(&settings(), &observation, &results, false, &mut ledger);
    assert!(ledger.solo_entry_native_arms.is_empty());

    record_native_arms(&settings(), &observation, &results, true, &mut ledger);
    assert_eq!(
        ledger.solo_entry_native_arms[&key]
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![PRODUCT, 43]
    );
    // #42 moved to a new head and #43 closed: both records are dropped.
    let moved = observation_with(vec![rebuilt(product_pr(), "f")]);
    record_native_arms(&settings(), &moved, &[], true, &mut ledger);
    assert!(ledger.solo_entry_native_arms.is_empty());
}

#[test]
fn a_new_version_opens_a_fresh_window() {
    let mut reads = FakeReads::release(&[(99, 1)]);
    let mut ledger = StewardLedger::default();
    assert!(is_hold(&run(
        &settings(),
        &both(),
        &reads,
        &mut ledger,
        now()
    )));
    let expired = now() + minutes(120);
    assert_free(&run(&settings(), &both(), &reads, &mut ledger, expired));
    assert_eq!(window(&ledger).attempts, 1);
    assert!(!window(&ledger).released.is_empty());

    reads.pending = Ok(Some("v0.2.3".to_owned()));
    let later = expired + minutes(1);
    let gate = run(&settings(), &both(), &reads, &mut ledger, later);
    assert!(is_hold(&gate), "{gate:?}");
    assert_eq!(window(&ledger).version, "v0.2.3");
    assert_eq!(window(&ledger).opened_at, later);
    assert_eq!(window(&ledger).attempts, 1);
    assert_eq!(window_key("Owner/Repo", "main"), "owner/repo@main");

    // Published: the window is forgotten.
    reads.pending = Ok(None);
    let gate = run(&settings(), &both(), &reads, &mut ledger, later);
    assert_free(&gate);
    assert!(gate.line().unwrap().contains("hold released"), "{gate:?}");
    assert!(ledger.solo_entry_windows.is_empty());
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
        "refs/tags/dry-run/v9.9.9".to_owned(),
    ];
    assert_eq!(tag_state((0, 2, 2), &tags, "v"), TagState::Untagged);
    assert_eq!(tag_state((0, 2, 1), &tags, "v"), TagState::Tagged);
    assert_eq!(
        tag_state((0, 2, 0), &tags, "v"),
        TagState::NotPending,
        "older than a tag"
    );
    assert_eq!(
        tag_state((0, 1, 10), &tags, "v"),
        TagState::NotPending,
        "tagged, with a newer tag"
    );
    assert_eq!(tag_state((0, 1, 0), &[], "v"), TagState::Untagged);
}

/// Review fix 8: a release is published only when GitHub says it is not a
/// draft. A 404 is "not yet"; any other failure is a failed read.
#[test]
fn a_release_is_published_only_when_it_exists_and_is_not_a_draft() {
    assert_eq!(release_published("v0.2.2", Ok("false\n")), Ok(true));
    assert_eq!(release_published("v0.2.2", Ok("true")), Ok(false));
    assert_eq!(
        release_published("v0.2.2", Err("gh: Not Found (HTTP 404)")),
        Ok(false)
    );
    for failed in [
        Err("gh: Bad Gateway (HTTP 502)"),
        Err("release notes Not Found in cache"),
        Ok("null"),
        Ok(""),
    ] {
        let read = release_published("v0.2.2", failed);
        assert!(
            read.as_ref()
                .is_err_and(|error| error.contains("unreadable")),
            "{failed:?}: {read:?}"
        );
    }
}

/// The reads against GitHub: the version file, the tags and, for a tagged
/// version, its release; the base comparison; the head repository.
#[cfg(unix)]
#[test]
fn the_release_and_head_reads_ask_github_the_documented_questions() {
    use crate::app::merge_steward_cmd::solo_entry::GitHubReads;
    let setting = RepoSetting {
        branches: vec![SOLO_BRANCH.to_owned()],
        version_file: "CMakeLists.txt".to_owned(),
        tag_prefix: "v".to_owned(),
        window_minutes: 120,
    };
    let observation = observation_with(vec![solo_pr()]);
    let read = |tags: &str, release: &str, compare: &str, head_repo: &str| {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(
            &temp,
            &format!(
                r#"case "$*" in
  *"contents/CMakeLists.txt?ref=main"*) printf '%s\n' 'project(Vellum VERSION 0.2.2)' ;;
  *"git/matching-refs/tags/v"*) printf '{tags}' ;;
  *"repos/owner/repo/releases/tags/v0.2.2 --jq .draft"*) {release} ;;
  *"repos/owner/repo/compare/main...{head} --jq .behind_by"*) {compare} ;;
  *"repos/owner/repo/pulls/7 --jq .head.repo.full_name"*) printf '%s\n' '{head_repo}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac"#,
                head = "0".repeat(39) + "7",
            ),
        );
        let ledger_path = temp.path().join("ledger.json");
        let reads = GitHubReads {
            actions: &actions,
            observation: &observation,
            control: None,
            ledger_path: &ledger_path,
        };
        (
            reads.pending_release(&setting),
            reads.up_to_date(&observation.prs[0]),
            reads.same_repo(&observation.prs[0]),
        )
    };
    let untagged = r"refs/tags/v0.2.1\n";
    let tagged = r"refs/tags/v0.2.1\nrefs/tags/v0.2.2\n";
    let not_found = "echo 'gh: Not Found (HTTP 404)' >&2; exit 1";

    // Untagged: pending, and the release is not read at all.
    let (pending, up_to_date, same_repo) = read(untagged, "exit 2", "echo 0", "owner/repo");
    assert_eq!(pending, Ok(Some("v0.2.2".to_owned())));
    assert_eq!(up_to_date, Ok(true));
    assert_eq!(same_repo, Ok(true));
    // Tagged, release missing or a draft: still pending.
    assert_eq!(
        read(tagged, not_found, "echo 2", "Owner/Repo"),
        (Ok(Some("v0.2.2".to_owned())), Ok(false), Ok(true))
    );
    assert_eq!(
        read(tagged, "echo true", "echo 0", "fork/repo").0,
        Ok(Some("v0.2.2".to_owned()))
    );
    assert_eq!(
        read(tagged, "echo true", "echo 0", "fork/repo").2,
        Ok(false)
    );
    // Published: the window closes.
    assert_eq!(
        read(tagged, "echo false", "echo 0", "owner/repo").0,
        Ok(None)
    );
    // Any other failure is a failed read, not "unpublished".
    let (pending, up_to_date, _) = read(
        tagged,
        "echo 'gh: Bad Gateway (HTTP 502)' >&2; exit 1",
        "echo 'gh: Bad Gateway (HTTP 502)' >&2; exit 1",
        "owner/repo",
    );
    assert!(pending.is_err(), "{pending:?}");
    assert!(up_to_date.is_err(), "{up_to_date:?}");
}

/// Review item 9: an enqueue that restored the entry's priority is an
/// enqueue too, so the queue is read again after it.
#[test]
fn every_enqueue_token_counts_as_an_enqueue() {
    use crate::app::merge_steward_cmd::solo_entry::was_enqueued;
    assert!(was_enqueued("enqueued"));
    assert!(was_enqueued("fast_track_fix_jump,enqueued"));
    assert!(was_enqueued("enqueued_priority_restored"));
    assert!(!was_enqueued("solo_entry_waiting:1_ahead"));
    assert!(!was_enqueued("skipped_after_live_revalidation"));
}

#[test]
fn a_malformed_setting_is_reported_and_admission_runs_normally() {
    let observation = observation_with(vec![solo_pr()]);
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

fn holding_gate() -> Gate {
    Gate::Hold {
        solo: SOLO,
        phase: Phase::Waiting { ahead: 1 },
        line: "solo_entry: #7 waiting".to_owned(),
    }
}

/// Review fix 4, earlier review F1: a confirmed red base holds the solo pull
/// request, so the solo hold must not hold the fix. Only the pull request
/// the base-poison path fast-tracks passes; nothing else does.
#[cfg(unix)]
#[test]
fn the_fix_for_a_confirmed_red_base_passes_the_solo_hold() {
    use crate::base_health::{AutoJump, PoisonHold};
    let gate = holding_gate();
    let confirmed = super::base_poison::poisoned(PoisonHold::On, AutoJump::On, &[PRODUCT]);
    assert_eq!(admission_hold(Some(&gate), Some(&confirmed), PRODUCT), None);
    // Any other pull request is still held.
    assert_eq!(
        admission_hold(Some(&gate), Some(&confirmed), 43).as_deref(),
        Some("held_solo_entry:#7")
    );
    // A streak the base tip does not confirm exempts nothing: the solo pull
    // request is not held by the base then, so there is no deadlock to break.
    let streak = super::base_poison::streak_only(PoisonHold::On, AutoJump::On, &[PRODUCT]);
    assert_eq!(
        admission_hold(Some(&gate), Some(&streak), PRODUCT).as_deref(),
        Some("held_solo_entry:#7")
    );
    assert_eq!(
        admission_hold(Some(&gate), None, PRODUCT).as_deref(),
        Some("held_solo_entry:#7")
    );
    // The solo pull request's own wait is never exempt, even named as a fix.
    let solo_named = super::base_poison::poisoned(PoisonHold::On, AutoJump::On, &[SOLO]);
    assert_eq!(
        admission_hold(Some(&gate), Some(&solo_named), SOLO).as_deref(),
        Some("solo_entry_waiting:1_ahead")
    );

    // Through the enqueue path: the fix goes on to GitHub, a product row
    // under the same reading does not.
    let (mutation, calls) = enqueue_under_poison(&gate, &product_pr(), Some(&confirmed));
    assert_ne!(mutation.as_deref(), Some("held_solo_entry:#7"));
    assert!(!calls.is_empty(), "the fix reached the enqueue path");
    let other = pr(43, "feature", false, "SUCCESS", &[]);
    let (mutation, calls) = enqueue_under_poison(&gate, &other, Some(&confirmed));
    assert_eq!(mutation.as_deref(), Some("held_solo_entry:#7"));
    assert!(calls.is_empty(), "{calls}");

    // The exemption is logged on the repository's `solo_entry` line.
    let observation = both();
    let line = settle(
        &gate,
        &observation,
        &[],
        Some(&confirmed),
        true,
        &mut StewardLedger::default(),
    )
    .expect("line");
    assert!(line.contains("solo_entry_exempt_base_fix: #42"), "{line}");
    let line = settle(
        &gate,
        &observation,
        &[],
        Some(&streak),
        true,
        &mut StewardLedger::default(),
    )
    .expect("line");
    assert!(!line.contains("exempt"), "{line}");
}

/// The fix reorder jump-enqueues a fix, so a held repository must not run
/// it: the step reports `fix_reorder: held` and reads nothing from GitHub.
/// The exemption is admission's: the fast-tracked fix for a confirmed red
/// base is still moved.
#[cfg(unix)]
#[test]
fn the_fix_reorder_is_held_while_a_solo_pr_holds_the_repository() {
    use crate::app::merge_steward_cmd::cancellation::fix_reorder_step;
    use crate::base_health::{AutoJump, PoisonHold};

    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!("printf '%s\\n' \"$*\" >> '{}'\nexit 2", log.display()),
    );
    let observation = observation_with(vec![pr(
        PRODUCT,
        "feature",
        false,
        "SUCCESS",
        &[(PRODUCT, 2)],
    )]);
    let mut streak = super::base_poison::streak_only(PoisonHold::On, AutoJump::On, &[PRODUCT]);
    streak.policy.acts.reorder_fix = PoisonHold::DryRun;

    let held = fix_reorder_step(
        &holding_gate(),
        &actions,
        None,
        true,
        &observation,
        Some(&streak),
    );
    assert_eq!(
        held,
        vec!["fix_reorder: held: solo_entry: #7 waiting".to_owned()]
    );
    assert!(fs::read_to_string(&log).unwrap_or_default().is_empty());

    // Controls: no poison runs nothing; an inactive gate does not hold.
    assert!(fix_reorder_step(&holding_gate(), &actions, None, true, &observation, None).is_empty());
    let open = fix_reorder_step(
        &Gate::Off,
        &actions,
        None,
        true,
        &observation,
        Some(&streak),
    );
    assert!(open.iter().all(|line| !line.contains("held")), "{open:?}");

    // Confirmed red with the named fix queued second: the reorder runs
    // under the hold (here it only reports, the pass has no control).
    let mut confirmed = super::base_poison::poisoned(PoisonHold::On, AutoJump::On, &[PRODUCT]);
    confirmed.policy.acts.reorder_fix = PoisonHold::DryRun;
    let moved = fix_reorder_step(
        &holding_gate(),
        &actions,
        None,
        true,
        &observation,
        Some(&confirmed),
    );
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert!(
        moved[0].contains("#42 would reorder") && !moved[0].contains("held"),
        "{moved:?}"
    );
}

/// The native-arm backstop arms unqueued pull requests, so a held repository
/// arms nothing; with the gate off or the backstop disabled it is not held.
#[test]
fn the_native_arm_backstop_is_held_while_a_solo_pr_holds_the_repository() {
    use crate::app::merge_steward_cmd::cancellation::native_arm_solo_hold;

    let held = native_arm_solo_hold(&holding_gate(), true).expect("held");
    assert_eq!(
        held.policy,
        "held: solo_entry: #7 waiting; nothing armed this pass"
    );
    assert!(held.results.is_empty());
    assert!(native_arm_solo_hold(&holding_gate(), false).is_none());
    assert!(native_arm_solo_hold(&Gate::Off, true).is_none());
}

#[cfg(unix)]
fn queue_gh(temp: &tempfile::TempDir, nodes: &str, behind_by: &str) -> GitHubActions {
    fake_gh(
        temp,
        &format!(
            r#"case "$*" in
  *"mergeQueue"*) printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[{nodes}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  *"/compare/main..."*"behind_by"*) {behind_by} ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac"#
        ),
    )
}

/// Under the enqueue's guard the queue and the base comparison are read
/// again: an entry that appeared, or a base that moved, since the pass's
/// read refuses the solo enqueue.
#[cfg(unix)]
#[test]
fn the_solo_enqueue_rereads_the_queue_and_the_base_under_the_guard() {
    let solo = solo_pr();
    let gate = Gate::Hold {
        solo: SOLO,
        phase: Phase::Admit,
        line: String::new(),
    };
    let entry = format!(
        r#"{{"position":1,"enqueuedAt":"2026-11-23T00:59:00Z","headCommit":{{"oid":"{PRODUCT:0>40}"}},"pullRequest":{{"number":{PRODUCT}}}}}"#
    );
    for (nodes, behind_by, expected) in [
        (
            entry.as_str(),
            "echo 0",
            Some(Ok("solo_entry_waiting:1_ahead".to_owned())),
        ),
        ("", "echo 0", None),
        (
            "",
            "echo 1",
            Some(Ok("solo_entry_waiting:behind_base".to_owned())),
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let actions = queue_gh(&temp, nodes, behind_by);
        let observation = observation_with(vec![solo.clone()]);
        let control = mutation_control(&temp, "studio", "studio");
        let ledger_path = temp.path().join("ledger.json");
        let mut context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
        context.solo_entry = Some(&gate);
        assert_eq!(
            pre_enqueue_refusal(&context, &solo),
            expected,
            "{nodes} {behind_by}"
        );
    }
    // A comparison that cannot be read refuses the enqueue with an error.
    let temp = tempfile::tempdir().expect("temp");
    let actions = queue_gh(&temp, "", "exit 1");
    let observation = observation_with(vec![solo.clone()]);
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let mut context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    context.solo_entry = Some(&gate);
    assert!(matches!(pre_enqueue_refusal(&context, &solo), Some(Err(_))));
    // Only the pull request the gate admits is checked.
    assert_eq!(pre_enqueue_refusal(&context, &product_pr()), None);
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
    let solo = solo_pr();
    let observation = observation_with(vec![product_pr(), solo.clone()]);
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let gate = Gate::Hold {
        solo: SOLO,
        phase: Phase::Admit,
        line: String::new(),
    };
    let mut context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    context.solo_entry = Some(&gate);
    // The hold the pass engaged, as the ledger holds it.
    let mut ledger = StewardLedger::default();
    run(
        &settings(),
        &observation,
        &FakeReads::release(&[]),
        &mut ledger,
        now(),
    );

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
    // The removal is the steward's own, so it never counts as an ejection,
    // and it counts against the attempt's cap.
    assert_eq!(ledger.steward_removals.len(), 1);
    assert_eq!(attempt(&ledger).own_dequeues, 1);
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
    let mut observation = observation_with(vec![solo_pr()]);
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

// ---- Wiring: the gate through the real call sites -------------------------

const WIRE_HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// A `gh` that answers the steward's whole enqueue path for pull request 42
/// (the fixture `tests/base_poison.rs` uses), with the merge queue and the
/// base comparison answered by `queue` and `behind_by` (shell).
#[cfg(unix)]
fn enqueue_gh(temp: &tempfile::TempDir, queue: &str, behind_by: &str) -> GitHubActions {
    let log = temp.path().join("calls");
    let marker = temp.path().join("enqueued");
    let approval = format!(
        r#"{{"id":6390564321,"user":{{"login":"shipyard-local[bot]","type":"Bot"}},"author_association":"CONTRIBUTOR","created_at":"2026-11-11T01:53:36Z","updated_at":"2026-11-11T01:53:36Z","node_id":"IC_verdict","body":"reviewer: pulp-review\n<!-- session: Daniels-Mac-Studio-m3/08fb81d8 -->\nverdict: APPROVE head:{WIRE_HEAD}"}}"#
    );
    fake_gh(
        temp,
        &format!(
            r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '{log}'
ENQUEUED='{marker}'
case "$*" in
  *"contents/.shipyard/config.toml"*) printf '%s\n' '[auto_merge]' 'reviewer_signers = ["pulp-review"]' ;;
  *"--slurp"*"pulls/42/reviews"*) printf '%s' '[[]]' ;;
  *"--slurp"*"issues/42/comments"*) printf '%s' '[[{approval}]]' ;;
  *"lastEditedAt"*) printf '%s' '{{"data":{{"node":{{"lastEditedAt":null}}}}}}' ;;
  *"node(id:"*) printf '%s' '{{"data":{{"node":{{"number":42,"state":"OPEN","headRefOid":"{head}","baseRefName":"main","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"repository":{{"nameWithOwner":"Generous-Corp/pulp"}},"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}' ;;
  "api repos/Generous-Corp/pulp/pulls/42") printf '%s' '{{"number":42,"base":{{"ref":"main","repo":{{"default_branch":"main"}}}},"head":{{"sha":"{head}"}},"user":{{"login":"shipyard-local[bot]","type":"Bot"}}}}' ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{head}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *"dequeuePullRequest"*) printf '%s' '{{"data":{{"dequeuePullRequest":{{"mergeQueueEntry":{{"id":"MQE"}}}}}}}}' ;;
  *"query=query("*"mergeQueue"*)
    {queue} ;;
  *"/compare/main..."*"behind_by"*) {behind_by} ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"{head}","headRefName":"{SOLO_BRANCH}","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"stackConfig"*) printf '%s' '{{"data":{{"repository":{{"stackConfig":{{"text":"stacked_pr_mode = \"observe\"\n"}},"pullRequest":{{"headRefOid":"{head}","stack":null,"stackEntry":null}}}}}}}}' ;;
  *"stackEntry"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"stack":null,"stackEntry":null}}}}}}}}' ;;
  *"enqueuePullRequest"*) : > "$ENQUEUED"; printf '%s' '{{"data":{{"enqueuePullRequest":{{"mergeQueueEntry":{{"position":1}}}}}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
            marker = marker.display(),
            head = WIRE_HEAD,
        ),
    )
}

#[cfg(unix)]
fn queue_answer(nodes: &str) -> String {
    format!(
        r#"printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[{nodes}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}'"#
    )
}

#[cfg(unix)]
fn queue_node(number: u64, position: u64, head: &str) -> String {
    format!(
        r#"{{"position":{position},"enqueuedAt":"2026-11-23T00:59:00Z","headCommit":{{"oid":"{head}"}},"pullRequest":{{"number":{number}}}}}"#
    )
}

/// The steward's real enqueue for solo pull request 42 under a gate that
/// admits it: the mutation, the error and every `gh` call.
#[cfg(unix)]
fn enqueue_solo(queue: &str, behind_by: &str) -> (Option<String>, Option<String>, String) {
    let pr = ready_pr();
    let mut observation = observation_for(pr.clone(), true);
    "Generous-Corp/pulp".clone_into(&mut observation.repo);
    let gate = Gate::Hold {
        solo: pr.fact.number,
        phase: Phase::Admit,
        line: String::new(),
    };
    enqueue_solo_in(
        queue,
        behind_by,
        &observation,
        &gate,
        &mut StewardLedger::default(),
    )
}

/// [`enqueue_solo`] for pull request 42 of `observation`, under `gate` and
/// with the pass's `ledger`.
#[cfg(unix)]
fn enqueue_solo_in(
    queue: &str,
    behind_by: &str,
    observation: &RepoObservation,
    gate: &Gate,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>, String) {
    use crate::app::merge_steward_cmd::pr_mutations::enqueue_pull_request;
    use crate::queue_arm_gate::tests_support::with_real_guard;

    let temp = tempfile::tempdir().expect("temp");
    let actions = enqueue_gh(&temp, queue, behind_by);
    let pr = observation
        .prs
        .iter()
        .find(|pr| pr.fact.number == 42)
        .expect("pull request 42")
        .clone();
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let mut context = mutation_apply_context(&actions, observation, &ledger_path, &control);
    context.solo_entry = Some(gate);
    let arms = temp
        .path()
        .join("state")
        .join(crate::queue_arm_gate::ARMS_LEDGER);
    let guards = temp.path().join("guards");
    fs::create_dir_all(&guards).expect("guards");
    let (mutation, error) = with_real_guard(&arms, &guards, || {
        crate::head_approval::with_host_review_default(false, || {
            enqueue_pull_request(&context, &pr, &queue_policy(), ledger)
        })
    });
    (
        mutation,
        error,
        fs::read_to_string(temp.path().join("calls")).unwrap_or_default(),
    )
}

#[cfg(unix)]
fn count(calls: &str, needle: &str) -> usize {
    calls.lines().filter(|line| line.contains(needle)).count()
}

/// Mutant "guarded re-read never called" (`pr_mutations.rs`): the real
/// enqueue path refuses the solo pull request when an entry appeared since
/// the pass's read, or its head fell behind the base, and enqueues it alone
/// otherwise.
#[cfg(unix)]
#[test]
fn the_real_enqueue_path_refuses_a_solo_pr_the_guarded_read_refuses() {
    // The control: an empty queue and a current head are enqueued.
    let (mutation, error, calls) = enqueue_solo(&queue_answer(""), "echo 0");
    assert_eq!(mutation.as_deref(), Some("enqueued"), "{error:?}\n{calls}");
    assert_eq!(count(&calls, "enqueuePullRequest"), 1, "{calls}");
    assert_eq!(count(&calls, "behind_by"), 1, "{calls}");

    let entry = queue_node(99, 1, &"9".repeat(40));
    let (mutation, error, calls) = enqueue_solo(&queue_answer(&entry), "echo 0");
    assert_eq!(
        mutation.as_deref(),
        Some("solo_entry_waiting:1_ahead"),
        "{error:?}"
    );
    assert_eq!(count(&calls, "enqueuePullRequest"), 0, "{calls}");

    let (mutation, error, calls) = enqueue_solo(&queue_answer(""), "echo 4");
    assert_eq!(
        mutation.as_deref(),
        Some("solo_entry_waiting:behind_base"),
        "{error:?}"
    );
    assert_eq!(count(&calls, "enqueuePullRequest"), 0, "{calls}");
}

/// Mutant "recheck after enqueue never called" (`pr_mutations.rs`): an entry
/// that lands ahead of the solo entry during its enqueue is seen by the read
/// right after, in the same pass, and the steward removes only its own entry.
#[cfg(unix)]
#[test]
fn the_real_enqueue_path_rechecks_the_queue_and_removes_its_own_entry() {
    let ahead = format!(
        "{},{}",
        queue_node(99, 1, &"9".repeat(40)),
        queue_node(42, 2, WIRE_HEAD)
    );
    // Empty until the enqueue, then an entry is ahead of the solo entry.
    let queue = format!(
        "if [ -f \"$ENQUEUED\" ]; then {}; else {}; fi",
        queue_answer(&ahead),
        queue_answer("")
    );
    let (mutation, error, calls) = enqueue_solo(&queue, "echo 0");
    assert_eq!(
        mutation.as_deref(),
        Some("enqueued,dequeued_solo_entry:1_ahead"),
        "{error:?}\n{calls}"
    );
    assert_eq!(count(&calls, "enqueuePullRequest"), 1, "{calls}");
    let dequeues = calls
        .lines()
        .filter(|line| line.contains("dequeuePullRequest"))
        .collect::<Vec<_>>();
    assert_eq!(dequeues.len(), 1, "{calls}");
    assert!(dequeues[0].contains("id=PR_kw"), "{calls}");
}

#[cfg(unix)]
fn tick_args(arm_unqueued: bool) -> StewardCommandArgs {
    StewardCommandArgs {
        repos: vec!["owner/repo".to_owned()],
        base: "main".to_owned(),
        opt_out_label: "steward:skip".to_owned(),
        provenance_blocking_labels: default_provenance_blocking_labels(),
        managed_label: MANAGED_LABEL.to_owned(),
        handoff_context: HANDOFF_CONTEXT.to_owned(),
        max_transient_reruns: 1,
        recover_hosted_setup_eviction_priority: false,
        coalesce: false,
        preempt_capacity: false,
        max_preemptions_per_head: 1,
        arm_unqueued,
        approval_context: crate::merge_steward::DEFAULT_APPROVAL_CONTEXT.to_owned(),
        wake_owner: false,
        unapproved_dequeue: crate::app::merge_steward_cmd::DequeueMode::Report,
        solo_entry: settings(),
        infra_rerun: crate::app::merge_steward_cmd::InfraRerunMode::Off,
        pass_budget: None,
        pass_deadline: None,
        invariants: None,
        apply: true,
        ledger: None,
    }
}

/// Mutants "gate not passed to admission", "native-arm backstop given
/// `Gate::Off`" and "fix reorder given `Gate::Off`" (`cancellation.rs`): one
/// applying tick over a ready solo pull request and an approved green
/// product pull request, with an entry already in the queue. The product row
/// is held, nothing is armed, the fix reorder is held, and the pass sends no
/// mutation at all.
#[cfg(unix)]
#[test]
fn a_tick_holds_every_other_admission_for_a_ready_solo_pr() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let entry = queue_node(99, 1, &"9".repeat(40));
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"contents/.shipyard/config.toml"*) printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = false' '[base_health]' 'merge_group_hold = "on"' ;;
  *"contents/CMakeLists.txt?ref=main"*) printf '%s\n' 'project(Vellum VERSION 0.2.2)' ;;
  *"git/matching-refs/tags/v"*) printf '%s\n' 'refs/tags/v0.2.1' ;;
  *"pulls/7 --jq .head.repo.full_name"*) printf '%s\n' 'owner/repo' ;;
  *"actions/runs"*) printf '%s' '{{"workflow_runs":[]}}' ;;
  *"mergeQueue"*) {queue} ;;
  *enqueuePullRequest*|*enablePullRequestAutoMerge*|*dequeuePullRequest*|*disablePullRequestAutoMerge*) echo "no mutation is allowed: $*" >&2; exit 90 ;;
  *) printf '%s' '{{}}' ;;
esac
"#,
            log = log.display(),
            queue = queue_answer(&entry),
        ),
    );
    let observation = both();
    let control = mutation_control(&temp, "studio", "studio");
    let mut ledger = StewardLedger::default();

    let (report, _, _) = apply_repo_plan(
        &actions,
        &tick_args(true),
        &observation,
        &temp.path().join("ledger.json"),
        &mut ledger,
        0,
        Some(&control),
        None,
    );

    let calls = fs::read_to_string(&log).unwrap_or_default();
    let row = |number: u64| {
        report
            .prs
            .iter()
            .find(|pr| pr.number == number)
            .unwrap_or_else(|| panic!("row #{number}: {report:?}"))
    };
    let line = report.solo_entry.clone().expect("solo_entry line");
    assert!(
        line.contains("#7") && line.contains("holds admission") && line.contains("HOLD ENGAGED"),
        "{line}\n{calls}"
    );
    assert_eq!(
        row(PRODUCT).mutation.as_deref(),
        Some("held_solo_entry:#7"),
        "{report:?}\n{calls}"
    );
    assert_eq!(
        row(SOLO).mutation.as_deref(),
        Some("solo_entry_waiting:1_ahead"),
        "{report:?}"
    );
    assert!(
        report
            .native_auto_merge_backstop
            .policy
            .starts_with("held: solo entry: #7"),
        "{:?}",
        report.native_auto_merge_backstop
    );
    let poison = serde_json::to_string(&report.base_poison).expect("base poison report");
    assert!(
        poison.contains("fix_reorder: held: solo entry: #7"),
        "{poison}"
    );
    for mutation in [
        "enqueuePullRequest",
        "enablePullRequestAutoMerge",
        "dequeuePullRequest",
        "disablePullRequestAutoMerge",
    ] {
        assert_eq!(count(&calls, mutation), 0, "{calls}");
    }
    assert!(hold_engaged(&ledger, "owner/repo", "main", Utc::now()));
}

/// Rehearsal case 1 (inert): the same tick with no setting holds nothing
/// and reads none of the solo-entry endpoints.
#[cfg(unix)]
#[test]
fn a_tick_without_the_setting_reads_none_of_the_solo_entry_endpoints() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"mergeQueue"*) {queue} ;;
  *) printf '%s' '{{}}' ;;
esac
"#,
            log = log.display(),
            queue = queue_answer(""),
        ),
    );
    let mut args = tick_args(false);
    args.solo_entry = Settings::default();
    args.apply = false;
    let mut ledger = StewardLedger::default();
    let (report, _, _) = apply_repo_plan(
        &actions,
        &args,
        &both(),
        &temp.path().join("ledger.json"),
        &mut ledger,
        0,
        None,
        None,
    );
    assert_eq!(report.solo_entry, None);
    assert!(
        report.prs.iter().all(|pr| pr
            .mutation
            .as_deref()
            .is_none_or(|mutation| !mutation.contains("solo"))),
        "{report:?}"
    );
    let calls = fs::read_to_string(&log).unwrap_or_default();
    for read in [
        "CMakeLists.txt",
        "matching-refs",
        "releases/tags",
        "behind_by",
        "full_name",
    ] {
        assert_eq!(count(&calls, read), 0, "{read}: {calls}");
    }
    assert!(ledger.solo_entry_windows.is_empty());
    assert!(ledger.solo_entry_native_arms.is_empty());
}

// ---- A solo entry the queue removes within one tick, and a crash around the
// ---- steward's own dequeue (rehearsal case 8, review finding J) ------------

/// The window the pulp fixtures engage: repository `Generous-Corp/pulp`.
#[cfg(unix)]
fn pulp_attempt(ledger: &StewardLedger) -> &Attempt {
    ledger
        .solo_entry_windows
        .get(&window_key("Generous-Corp/pulp", "main"))
        .and_then(|window| window.attempt.as_ref())
        .expect("engaged hold")
}

/// Rehearsal pass p082 then p084, state for state. The tick admits the solo
/// head and its own enqueue succeeds; the merge group fails within the tick
/// and the queue removes the entry before any pass has seen it queued. The
/// next pass finds the head open, unchanged and not queued: the hold is
/// released at once and the products are admitted. Before the fix that pass
/// kept the hold and reported `admitted and not enqueued
/// (skipped_ejected_same_head:failed_checks:needs_new_head): pass 1 of 3`.
#[cfg(unix)]
#[test]
fn a_solo_entry_the_queue_ejects_within_one_tick_releases_the_hold_on_the_next_pass() {
    let mut solo = ready_pr();
    SOLO_BRANCH.clone_into(&mut solo.fact.head_branch);
    solo.fact.author = Some("app/shipyard-local".to_owned());
    let mut observation = observation_for(solo.clone(), true);
    "Generous-Corp/pulp".clone_into(&mut observation.repo);
    observation
        .prs
        .insert(0, pr(43, "feature", false, "SUCCESS", &[]));
    let settings = settings_for("Generous-Corp/pulp");
    let reads = FakeReads::release(&[]);
    let mut ledger = StewardLedger::default();

    // p082: the hold engages and the gate admits the solo head alone.
    let gate = run(&settings, &observation, &reads, &mut ledger, now());
    assert_eq!(gate.enqueue_hold(42), None, "{gate:?}");
    assert_eq!(
        gate.enqueue_hold(43).as_deref(),
        Some("held_solo_entry:#42")
    );
    assert!(!pulp_attempt(&ledger).queued);
    // Empty until the enqueue, then the solo entry alone.
    let queue = format!(
        "if [ -f \"$ENQUEUED\" ]; then {}; else {}; fi",
        queue_answer(&queue_node(42, 1, WIRE_HEAD)),
        queue_answer("")
    );
    let (mutation, error, calls) =
        enqueue_solo_in(&queue, "echo 0", &observation, &gate, &mut ledger);
    assert_eq!(mutation.as_deref(), Some("enqueued"), "{error:?}\n{calls}");
    assert!(
        pulp_attempt(&ledger).queued,
        "its own enqueue put the entry in the queue"
    );

    // p084: ejected, so open at the same head with no queue entry.
    let gate = run(
        &settings,
        &observation,
        &reads,
        &mut ledger,
        now() + minutes(5),
    );
    assert!(matches!(gate, Gate::Inactive { .. }), "{gate:?}");
    assert_eq!(gate.enqueue_hold(43), None, "the product is admitted");
    assert_eq!(gate.enqueue_hold(42), None);
    let line = settle(
        &gate,
        &observation,
        &[PrReport {
            number: 42,
            head_sha: WIRE_HEAD.to_owned(),
            decision: StewardDecision::ArmMergeQueue,
            mutation: Some("skipped_ejected_same_head:failed_checks:needs_new_head".to_owned()),
            error: None,
            blocked: crate::app::merge_steward_cmd::blocked_on::Stall::default(),
            required_checks: Vec::new(),
        }],
        None,
        true,
        &mut ledger,
    )
    .expect("line");
    assert!(
        line.contains("HOLD RELEASED") && line.contains("#42 left the merge queue unmerged"),
        "{line}"
    );
    assert!(!line.contains("admitted and not enqueued"), "{line}");
    let window = &ledger.solo_entry_windows[&window_key("Generous-Corp/pulp", "main")];
    assert!(window.attempt.is_none());
    assert!(window.is_released(42, WIRE_HEAD));
}

/// The record behind that release: right after its own enqueue the entry
/// counts as seen queued, whether or not the queue could be read again, and
/// only for the head the hold is for.
#[cfg(unix)]
#[test]
fn the_solo_entrys_own_enqueue_counts_as_seen_queued() {
    let solo = solo_pr();
    let observation = both();
    let gate = Gate::Hold {
        solo: SOLO,
        phase: Phase::Admit,
        line: String::new(),
    };
    let alone = queue_node(SOLO, 1, &solo.fact.head_sha);
    for (queue, token) in [
        (queue_answer(&alone), None),
        ("exit 1".to_owned(), Some("solo_entry_recheck_unread")),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(
            &temp,
            &format!("case \"$*\" in\n  *\"mergeQueue\"*) {queue} ;;\n  *) exit 2 ;;\nesac"),
        );
        let control = mutation_control(&temp, "studio", "studio");
        let ledger_path = temp.path().join("ledger.json");
        let mut context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
        context.solo_entry = Some(&gate);
        let mut ledger = StewardLedger::default();
        run(
            &settings(),
            &observation,
            &FakeReads::release(&[]),
            &mut ledger,
            now(),
        );
        assert!(!attempt(&ledger).queued);

        // Another head than the one the hold is for records nothing.
        let other = rebuilt(solo.clone(), "b");
        verify_after_enqueue(&context, &other, &mut ledger);
        assert!(!attempt(&ledger).queued);

        let (mutation, error) = verify_after_enqueue(&context, &solo, &mut ledger);
        assert_eq!(mutation.as_deref(), token, "{error:?}");
        assert!(attempt(&ledger).queued, "{queue}");

        // Ejected before the next pass: released at once.
        let gate = run(
            &settings(),
            &observation,
            &FakeReads::release(&[]),
            &mut ledger,
            now() + minutes(5),
        );
        assert_free(&gate);
        assert!(
            gate.line()
                .unwrap()
                .contains("left the merge queue unmerged"),
            "{gate:?}"
        );
    }
}

/// Finding J: the ledger says "not queued" on disk before the steward's own
/// dequeue is sent. A crash right after the dequeue (the pass's save never
/// happens) leaves that record, so the next pass keeps the hold instead of
/// reading the missing entry as "the queue removed it". Both own-dequeue
/// call sites: the pass's, and the recheck after the enqueue.
#[cfg(unix)]
#[test]
fn a_crash_after_the_own_dequeue_is_not_an_unmerged_exit() {
    use crate::app::merge_steward_cmd::ledger::load_ledger;
    use crate::app::merge_steward_cmd::solo_entry::GitHubReads;

    let solo = solo_pr();
    let observation = both();
    let ahead = format!(
        "{},{}",
        queue_node(PRODUCT, 1, &format!("{PRODUCT:0>40}")),
        queue_node(SOLO, 2, &solo.fact.head_sha)
    );
    // What an earlier pass saved: the hold engaged, the entry seen queued.
    let seen_queued = || {
        let mut ledger = StewardLedger::default();
        let queued = observation_with(vec![
            product_pr(),
            pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[(SOLO, 1)]),
        ]);
        run(
            &settings(),
            &queued,
            &FakeReads::release(&[(SOLO, 1)]),
            &mut ledger,
            now(),
        );
        assert!(attempt(&ledger).queued);
        ledger
    };
    for site in ["pass", "recheck"] {
        let temp = tempfile::tempdir().expect("temp");
        let ledger_path = temp.path().join("ledger.json");
        let on_disk = temp.path().join("at-dequeue.json");
        // The dequeue copies the ledger file as it is on disk at that moment.
        let actions = fake_gh(
            &temp,
            &format!(
                r#"case "$*" in
  *"dequeuePullRequest"*) cp '{ledger}' '{snapshot}'; printf '%s' '{{"data":{{"dequeuePullRequest":{{"mergeQueueEntry":{{"id":"MQE"}}}}}}}}' ;;
  *"mergeQueue"*) {queue} ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac"#,
                ledger = ledger_path.display(),
                snapshot = on_disk.display(),
                queue = queue_answer(&ahead),
            ),
        );
        let control = mutation_control(&temp, "studio", "studio");
        let mut ledger = seen_queued();
        if site == "pass" {
            let reads = GitHubReads {
                actions: &actions,
                observation: &observation,
                control: Some(&control),
                ledger_path: &ledger_path,
            };
            assert_eq!(reads.dequeue(&solo, &mut ledger), Ok(()), "{site}");
        } else {
            let gate = Gate::Hold {
                solo: SOLO,
                phase: Phase::Admit,
                line: String::new(),
            };
            let mut context =
                mutation_apply_context(&actions, &observation, &ledger_path, &control);
            context.solo_entry = Some(&gate);
            let (mutation, error) = verify_after_enqueue(&context, &solo, &mut ledger);
            assert_eq!(
                mutation.as_deref(),
                Some("dequeued_solo_entry:1_ahead"),
                "{error:?}"
            );
        }
        assert!(!attempt(&ledger).queued, "{site}");
        assert_eq!(attempt(&ledger).own_dequeues, 1, "{site}");

        // The crash: only what was on disk when the dequeue went out survives.
        let mut survived = load_ledger(&on_disk).expect("ledger saved before the dequeue");
        assert!(!attempt(&survived).queued, "{site}");
        assert_eq!(attempt(&survived).own_dequeues, 1, "{site}");
        let gate = run(
            &settings(),
            &observation,
            &FakeReads::release(&[(PRODUCT, 1)]),
            &mut survived,
            now() + minutes(5),
        );
        assert_eq!(
            gate.enqueue_hold(PRODUCT).as_deref(),
            Some("held_solo_entry:#7"),
            "{site}: {gate:?}"
        );
        assert!(window(&survived).released.is_empty(), "{site}");
    }
}

/// The own dequeue is sent only once its record is on disk, and a dequeue
/// that fails puts the record back: the entry is still in the queue.
#[cfg(unix)]
#[test]
fn an_own_dequeue_that_is_not_sent_or_fails_leaves_the_entry_on_record() {
    use crate::app::merge_steward_cmd::solo_entry::GitHubReads;

    let solo = solo_pr();
    let observation = both();
    for (unsavable, why) in [(true, "ledger not saved"), (false, "dequeue")] {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls");
        let actions = fake_gh(
            &temp,
            &format!("printf '%s\\n' \"$*\" >> '{}'\nexit 1", log.display()),
        );
        let control = mutation_control(&temp, "studio", "studio");
        // A directory cannot be written as the ledger file.
        let ledger_path = if unsavable {
            temp.path().to_path_buf()
        } else {
            temp.path().join("ledger.json")
        };
        let mut ledger = StewardLedger::default();
        let queued = observation_with(vec![
            product_pr(),
            pr(SOLO, SOLO_BRANCH, false, "SUCCESS", &[(SOLO, 1)]),
        ]);
        run(
            &settings(),
            &queued,
            &FakeReads::release(&[(SOLO, 1)]),
            &mut ledger,
            now(),
        );
        let reads = GitHubReads {
            actions: &actions,
            observation: &observation,
            control: Some(&control),
            ledger_path: &ledger_path,
        };
        let outcome = reads.dequeue(&solo, &mut ledger);
        assert!(
            outcome.as_ref().is_err_and(|error| error.contains(why)),
            "{outcome:?}"
        );
        assert!(attempt(&ledger).queued, "{why}");
        assert_eq!(attempt(&ledger).own_dequeues, 0, "{why}");
        let calls = fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(
            count(&calls, "dequeuePullRequest"),
            usize::from(!unsavable),
            "{calls}"
        );
    }
}
