//! Ready-on-event admission: an event only triggers the steward's own
//! admission for one pull request; the head is read again and admitted
//! through the tick's path under the steward lock.
use super::*;
use crate::app::merge_steward_cmd::ready_event::{
    ReadyEvent, ReadyEventOutcome, ReadyReads, admit_ready_event, attempts_path, claim_attempt,
};

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MOVED: &str = "cccccccccccccccccccccccccccccccccccccccc";
const POLICY_REQUIRED: &str = r#"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = true' 'reviewer_identities = ["shipyard-review[bot]"]'"#;
/// [`POLICY_REQUIRED`], with the base's merge groups read (`merge_group_hold`).
const POLICY_WITH_HOLD: &str = r#"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = true' 'reviewer_identities = ["shipyard-review[bot]"]' '[base_health]' 'merge_group_hold = "on"'"#;
const ACTIONS_APP: u64 = 15368;

/// A fleet-authored (so managed) pull request, green on `macos`.
fn pr_json(head: &str) -> String {
    format!(
        r#"{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"{head}","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"author":{{"login":"app/shipyard-local","is_bot":true}},"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-11-13T00:00:00Z","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}"#
    )
}

fn observed(head: &str) -> RepoObservation {
    let pr = parse_pr(
        &serde_json::from_str(&pr_json(head)).expect("PR json"),
        &BTreeMap::new(),
    )
    .expect("PR");
    observation_for(pr, true)
}

/// The app-bound `review-verdict` check run a base with no queue requires.
fn review_check_run(head: &str) -> String {
    format!(
        r#"{{"id":9001,"name":"review-verdict","head_sha":"{head}","status":"completed","conclusion":"success","started_at":"2026-11-13T00:00:30Z","completed_at":"2026-11-13T00:01:00Z","details_url":"https://github.com/owner/repo/actions/runs/101/job/9001","app":{{"id":{ACTIONS_APP},"slug":"github-actions"}}}}"#
    )
}

/// A base with no merge queue that GitHub admits only through the app-bound
/// `review-verdict` check: the steward merges the exact head there.
fn observed_no_queue(head: &str) -> RepoObservation {
    let mut observation = observed(head);
    observation.merge_queue = false;
    observation.allow_auto_merge = true;
    observation.required_checks.push(RequiredCheck {
        context: "review-verdict".to_owned(),
        app_id: Some(ACTIONS_APP),
    });
    observation.prs[0].fact.checks.push(
        crate::app::merge_steward_cmd::observation::parse_rest_check(
            &serde_json::from_str(&review_check_run(head)).expect("check run"),
        )
        .expect("check run"),
    );
    observation
}

fn approved_comment(head: &str) -> String {
    format!(
        r#"{{"id":12345,"login":"shipyard-review[bot]","type":"Bot","at":"2026-11-13T00:05:00Z","body":"LGTM\nreviewed: {head}","performed_via_github_app_id":99}}"#
    )
}

struct Fixture {
    /// The base's approval policy read; `None` makes it fail.
    policy: Option<&'static str>,
    /// The head the reviewer approved.
    approved: &'static str,
    /// The head every live read reports.
    live_head: &'static str,
    /// The answer to the merge-group run list.
    merge_groups: &'static str,
    /// The base has no merge queue (an exact-head merge base).
    no_queue: bool,
}

impl Fixture {
    fn approved() -> Self {
        Self {
            policy: Some(POLICY_REQUIRED),
            approved: HEAD,
            live_head: HEAD,
            merge_groups: r#"printf '%s' '{"workflow_runs":[]}'"#,
            no_queue: false,
        }
    }

    fn actions(&self, temp: &tempfile::TempDir) -> (GitHubActions, PathBuf) {
        let log = temp.path().join("calls");
        let policy = self
            .policy
            .unwrap_or("echo 'HTTP 502: bad gateway' >&2; exit 1");
        let live_head = self.live_head;
        let comments = approved_comment(self.approved);
        let live_pr = pr_json(live_head);
        let merge_groups = self.merge_groups;
        let (check_runs, merge_queue) = if self.no_queue {
            (
                format!(r#"{{"check_runs":[{}]}}"#, review_check_run(live_head)),
                r#"{"data":{"repository":{"mergeQueue":null}}}"#,
            )
        } else {
            (
                r#"{"check_runs":[]}"#.to_owned(),
                r#"{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}}"#,
            )
        };
        let script = format!(
            r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"contents/.shipyard/config.toml"*) {policy} ;;
  *"pulls/42/reviews"*) printf '' ;;
  *"issues/42/comments"*) printf '%s\n' '{comments}' ;;
  *"/check-runs?"*) printf '%s' '{check_runs}' ;;
  *"/statuses?"*) printf '%s' '[]' ;;
  *"commits/{live_head}"*) printf '%s' '2026-11-13T00:00:00Z' ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{live_head}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *"actions/runs?event=merge_group"*) {merge_groups} ;;
  *"query=query("*"mergeQueue"*) printf '%s' '{merge_queue}' ;;
  *"mergePullRequest("*) printf '%s' '{{"data":{{"mergePullRequest":{{"pullRequest":{{"number":42,"merged":true}}}}}}}}' ;;
  "pr view "*) printf '%s' '{live_pr}' ;;
  *"stackConfig"*) printf '%s' '{{"data":{{"repository":{{"stackConfig":{{"text":"stacked_pr_mode = \"observe\"\n"}},"pullRequest":{{"headRefOid":"{live_head}","stack":null,"stackEntry":null}}}}}}}}' ;;
  *"stackEntry"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"stack":null,"stackEntry":null}}}}}}}}' ;;
  *"enqueuePullRequest"*) printf '%s' '{{"data":{{"enqueuePullRequest":{{"mergeQueueEntry":{{"position":1}}}}}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
        );
        (fake_gh(temp, &script), log)
    }
}

struct Harness {
    temp: tempfile::TempDir,
    actions: GitHubActions,
    log: PathBuf,
    control: MutationControl,
    ledger_path: PathBuf,
    /// The steward's fleet list.
    fleet: Vec<String>,
    /// The REST budget read.
    rest: Result<u64, String>,
}

impl Harness {
    fn new(fixture: &Fixture) -> Self {
        let temp = tempfile::tempdir().expect("temp");
        let (actions, log) = fixture.actions(&temp);
        let control = mutation_control(&temp, "studio", "studio");
        let ledger_path = control.state_dir.join("merge-steward.json");
        Self {
            temp,
            actions,
            log,
            control,
            ledger_path,
            fleet: vec!["Owner/Repo".to_owned()],
            rest: Ok(5_000),
        }
    }

    fn defect_fix_ledger(&self) -> PathBuf {
        self.temp.path().join("queue-removals.jsonl")
    }

    fn run(
        &self,
        head: &str,
        observe: &dyn Fn() -> Result<RepoObservation, String>,
        apply: bool,
        now: DateTime<Utc>,
    ) -> ReadyEventOutcome {
        let fleet = || Ok(self.fleet.clone());
        let rest = || self.rest.clone();
        admit_ready_event(
            &self.actions,
            &self.control,
            &self.ledger_path,
            &Self::event(head),
            apply,
            Duration::from_secs(60),
            &ReadyReads {
                observe,
                fleet_repos: &fleet,
                rest_remaining: &rest,
                defect_fix_ledger: self.defect_fix_ledger(),
            },
            now,
        )
    }

    fn save(&self, ledger: &StewardLedger) {
        save_ledger(&self.ledger_path, ledger).expect("save ledger");
    }

    fn event(head: &str) -> ReadyEvent {
        ReadyEvent {
            repo: "owner/repo".to_owned(),
            base: "main".to_owned(),
            number: 42,
            head: head.to_owned(),
        }
    }

    fn admit_at(
        &self,
        head: &str,
        observation: &RepoObservation,
        apply: bool,
        now: DateTime<Utc>,
    ) -> ReadyEventOutcome {
        let observe = || Ok(observation.clone());
        self.run(head, &observe, apply, now)
    }

    fn admit(&self, head: &str, observation: &RepoObservation) -> ReadyEventOutcome {
        self.admit_at(head, observation, true, Utc::now())
    }

    fn calls(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn enqueues(&self) -> usize {
        self.calls().matches("enqueuePullRequest").count()
    }
}

#[test]
fn an_approval_event_enqueues_an_approved_green_head_once_through_the_tick_path() {
    let harness = Harness::new(&Fixture::approved());
    let observation = observed(HEAD);

    let first = harness.admit(HEAD, &observation);
    assert_eq!(first.outcome, "admitted", "{first:?}");
    assert_eq!(first.mutation.as_deref(), Some("enqueued"));
    let calls = harness.calls();
    // The live exact-head approval read, then the head-pinned enqueue.
    assert!(calls.contains("issues/42/comments"), "{calls}");
    assert!(calls.contains("expectedHeadOid"), "{calls}");
    assert!(calls.contains(&format!("head={HEAD}")), "{calls}");
    assert_eq!(harness.enqueues(), 1, "{calls}");
    // The tick's own ledger records the admission.
    let ledger = load_ledger(&harness.ledger_path).expect("ledger");
    assert!(
        ledger
            .terminal_handoffs
            .values()
            .any(|handoff| handoff.pr_number == 42 && handoff.head_sha == HEAD),
        "the admission's success continuation is in the steward ledger"
    );

    let duplicate = harness.admit(HEAD, &observation);
    assert_eq!(duplicate.outcome, "skipped");
    assert_eq!(duplicate.reason, "duplicate_event");
    assert_eq!(
        harness.enqueues(),
        1,
        "a duplicate event never enqueues again"
    );
}

#[test]
fn a_duplicate_event_reads_nothing_and_a_later_one_finds_the_head_queued() {
    let harness = Harness::new(&Fixture::approved());
    let now = Utc::now();
    assert_eq!(
        harness.admit_at(HEAD, &observed(HEAD), true, now).outcome,
        "admitted"
    );
    let reads = harness.calls().lines().count();
    let again = harness.admit_at(
        HEAD,
        &observed(HEAD),
        true,
        now + chrono::Duration::seconds(30),
    );
    assert_eq!(again.reason, "duplicate_event");
    assert_eq!(harness.calls().lines().count(), reads, "no GitHub read");

    // Past the window the head is read again; GitHub now has it queued.
    let mut queued = observed(HEAD);
    queued.prs[0].fact.queue_position = Some(1);
    let later = harness.admit_at(HEAD, &queued, true, now + chrono::Duration::seconds(90));
    assert_eq!(later.outcome, "skipped");
    assert_eq!(later.reason, "already_queued_or_armed");
    assert_eq!(harness.enqueues(), 1);
}

#[test]
fn an_event_for_a_head_that_moved_is_refused() {
    let harness = Harness::new(&Fixture::approved());
    // The event names a head the pull request no longer has.
    let stale = harness.admit(MOVED, &observed(HEAD));
    assert_eq!(stale.outcome, "skipped");
    assert_eq!(stale.reason, format!("head_moved:{HEAD}"));
    assert_eq!(harness.enqueues(), 0);
    assert!(
        !harness.calls().contains("issues/42/comments"),
        "a stale event reads no approval"
    );
}

#[test]
fn a_head_that_moves_after_the_event_is_refused_by_the_live_revalidation() {
    // The event and the first read agree, but the head moved before the
    // admission's own live read: the tick's revalidation refuses it.
    let harness = Harness::new(&Fixture {
        live_head: MOVED,
        ..Fixture::approved()
    });
    let outcome = harness.admit(HEAD, &observed(HEAD));
    // The live gate reads the moved head, which nobody approved.
    assert_eq!(outcome.outcome, "refused", "{outcome:?}");
    assert!(
        matches!(
            outcome.mutation.as_deref(),
            Some("waiting_head_approval" | "skipped_after_live_revalidation")
        ),
        "{outcome:?}"
    );
    assert_eq!(harness.enqueues(), 0, "{}", harness.calls());
}

#[test]
fn an_approval_of_another_head_is_not_admitted() {
    let harness = Harness::new(&Fixture {
        approved: MOVED,
        ..Fixture::approved()
    });
    let outcome = harness.admit(HEAD, &observed(HEAD));
    assert_eq!(outcome.outcome, "refused", "{outcome:?}");
    assert_eq!(outcome.mutation.as_deref(), Some("waiting_head_approval"));
    assert_eq!(harness.enqueues(), 0);
}

#[test]
fn an_event_during_a_tick_is_serialized_by_the_steward_lock() {
    let harness = Harness::new(&Fixture::approved());
    // A tick holds the ledger lock for its whole pass.
    let tick = try_acquire_ledger_lock(&harness.ledger_path)
        .expect("lock")
        .expect("uncontended");
    let busy = harness.admit(HEAD, &observed(HEAD));
    assert_eq!(busy.outcome, "skipped");
    assert_eq!(busy.reason, "steward_busy");
    assert_eq!(harness.enqueues(), 0);
    assert!(
        !harness.calls().contains("issues/42/comments"),
        "no admission read runs outside the lock"
    );
    drop(tick);

    // The tick waits out an event's admission instead of skipping its pass.
    let event_lock = try_acquire_ledger_lock(&harness.ledger_path)
        .expect("lock")
        .expect("uncontended");
    let path = harness.ledger_path.clone();
    let waiter = std::thread::spawn(move || {
        acquire_ledger_lock_within(&path, Duration::from_secs(10)).map(|_| ())
    });
    std::thread::sleep(Duration::from_millis(400));
    drop(event_lock);
    assert!(waiter.join().expect("join").is_ok());
    // And still gives up when the lock is never released.
    let held = try_acquire_ledger_lock(&harness.ledger_path)
        .expect("lock")
        .expect("uncontended");
    let refused = acquire_ledger_lock_within(&harness.ledger_path, Duration::from_millis(300));
    assert!(
        refused
            .err()
            .is_some_and(|error| error.message().contains("already running"))
    );
    drop(held);
}

#[test]
fn an_unreadable_gate_does_nothing() {
    let harness = Harness::new(&Fixture {
        policy: None,
        ..Fixture::approved()
    });
    let outcome = harness.admit(HEAD, &observed(HEAD));
    // Fail closed: the admission refuses on the unread policy.
    assert_ne!(outcome.outcome, "admitted", "{outcome:?}");
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("head approval policy unreadable")),
        "{outcome:?}"
    );
    assert_eq!(harness.enqueues(), 0, "{}", harness.calls());
}

#[test]
fn an_unreadable_pull_request_does_nothing_and_takes_no_lock() {
    let harness = Harness::new(&Fixture::approved());
    let held = try_acquire_ledger_lock(&harness.ledger_path)
        .expect("lock")
        .expect("uncontended");
    let observe = || Err::<RepoObservation, String>("HTTP 502".to_owned());
    let outcome = harness.run(HEAD, &observe, true, Utc::now());
    assert_eq!(outcome.outcome, "skipped");
    assert_eq!(outcome.reason, "unread:HTTP 502");
    assert_eq!(harness.enqueues(), 0);
    drop(held);
}

#[test]
fn a_head_the_policy_does_not_classify_ready_is_left_to_the_tick() {
    let harness = Harness::new(&Fixture::approved());
    let mut pending = observed(HEAD);
    pending.prs[0].fact.checks[0].status = "IN_PROGRESS".to_owned();
    pending.prs[0].fact.checks[0].conclusion = None;
    let outcome = harness.admit(HEAD, &pending);
    assert_eq!(outcome.outcome, "skipped");
    assert!(outcome.reason.starts_with("not_ready:"), "{outcome:?}");
    assert_eq!(harness.enqueues(), 0);
}

#[test]
fn a_dry_run_records_nothing_and_mutates_nothing() {
    let harness = Harness::new(&Fixture::approved());
    let outcome = harness.admit_at(HEAD, &observed(HEAD), false, Utc::now());
    assert_eq!(outcome.outcome, "would_admit", "{outcome:?}");
    assert_eq!(harness.enqueues(), 0);
    assert!(!attempts_path(&harness.ledger_path).exists());
    assert!(!harness.ledger_path.exists());
    let _ = &harness.temp;
}

#[test]
fn attempts_are_one_per_head_per_window_and_fail_closed_when_unreadable() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("attempts.json");
    let now = Utc::now();
    let window = Duration::from_secs(60);
    assert_eq!(claim_attempt(&path, "k@a", now, window), Ok(true));
    assert_eq!(
        claim_attempt(&path, "k@a", now + chrono::Duration::seconds(59), window),
        Ok(false)
    );
    assert_eq!(
        claim_attempt(&path, "k@b", now, window),
        Ok(true),
        "a new head is a new attempt"
    );
    assert_eq!(
        claim_attempt(&path, "k@a", now + chrono::Duration::seconds(61), window),
        Ok(true)
    );
    fs::write(&path, "{not json").expect("corrupt");
    assert!(claim_attempt(&path, "k@c", now, window).is_err());
}

#[test]
fn a_repository_outside_the_fleet_list_is_refused_before_any_read() {
    let mut harness = Harness::new(&Fixture::approved());
    harness.fleet = vec!["owner/other".to_owned()];
    let outcome = harness.admit(HEAD, &observed(HEAD));
    assert_eq!(outcome.reason, "not_fleet_repo");
    assert!(harness.calls().is_empty(), "{}", harness.calls());
    assert!(!attempts_path(&harness.ledger_path).exists());
}

#[test]
fn a_rate_limited_repository_is_not_read() {
    let mut harness = Harness::new(&Fixture::approved());
    harness.rest = Ok(299);
    let low = harness.admit(HEAD, &observed(HEAD));
    assert_eq!(low.reason, "rate_limited:rest_remaining=299");
    harness.rest = Err("HTTP 502".to_owned());
    let unread = harness.admit_at(
        HEAD,
        &observed(HEAD),
        true,
        Utc::now() + chrono::Duration::seconds(120),
    );
    assert_eq!(unread.reason, "rate_limited:unread:HTTP 502");
    assert!(harness.calls().is_empty(), "{}", harness.calls());

    // A cooldown the tick recorded for this credential and repository.
    harness.rest = Ok(5_000);
    let key = crate::app::merge_steward_cmd::pass_budget::rate_limit_cooldown_key(
        &harness.actions.rate_limit_scope_key("owner/repo"),
        "owner/repo",
    );
    let now = Utc::now();
    let mut ledger = StewardLedger::default();
    ledger.rate_limit_cooldowns.insert(
        key,
        crate::app::merge_steward_cmd::pass_budget::RateLimitCooldown {
            repo: "owner/repo".to_owned(),
            scope_key: "scope".to_owned(),
            until_epoch: now.timestamp() + 600,
            observed_at_epoch: now.timestamp(),
            detail: "secondary rate limit".to_owned(),
        },
    );
    harness.save(&ledger);
    let cooled = harness.admit_at(
        HEAD,
        &observed(HEAD),
        true,
        now + chrono::Duration::seconds(240),
    );
    assert_eq!(cooled.reason, "rate_limited:cooldown");
    assert!(harness.calls().is_empty(), "{}", harness.calls());
}

#[test]
fn a_dry_run_reads_the_gate_and_never_claims_an_unapproved_head() {
    let harness = Harness::new(&Fixture {
        approved: MOVED,
        ..Fixture::approved()
    });
    let outcome = harness.admit_at(HEAD, &observed(HEAD), false, Utc::now());
    assert_eq!(outcome.outcome, "skipped", "{outcome:?}");
    assert_eq!(outcome.reason, "waiting_head_approval");
    assert!(harness.calls().contains("issues/42/comments"));
    assert_eq!(harness.enqueues(), 0);

    let unread = Harness::new(&Fixture {
        policy: None,
        ..Fixture::approved()
    });
    let outcome = unread.admit_at(HEAD, &observed(HEAD), false, Utc::now());
    assert!(outcome.reason.starts_with("gate_unread:"), "{outcome:?}");
}

#[test]
fn a_base_whose_merge_groups_are_not_clear_is_left_to_the_tick() {
    let harness = Harness::new(&Fixture {
        policy: Some(POLICY_WITH_HOLD),
        merge_groups: "echo 'HTTP 502: bad gateway' >&2; exit 1",
        ..Fixture::approved()
    });
    let outcome = harness.admit(HEAD, &observed(HEAD));
    assert_eq!(outcome.reason, "base_poison_not_clear", "{outcome:?}");
    assert_eq!(harness.enqueues(), 0);
    assert!(!harness.calls().contains("issues/42/comments"));
}

#[test]
fn a_poison_paused_entry_is_left_to_the_tick() {
    let harness = Harness::new(&Fixture::approved());
    let mut ledger = StewardLedger::default();
    ledger.poison_paused.insert(
        "owner/repo@main".to_owned(),
        vec![
            serde_json::from_value(serde_json::json!({
                "pr": 42,
                "head": HEAD,
                "position": 1,
                "contexts": "macos",
                "paused_at": "2026-11-13T00:00:00Z",
            }))
            .expect("paused entry"),
        ],
    );
    harness.save(&ledger);
    let outcome = harness.admit(HEAD, &observed(HEAD));
    assert_eq!(outcome.reason, "poison_paused", "{outcome:?}");
    assert_eq!(harness.enqueues(), 0);
}

#[test]
fn pending_cancellations_leave_the_head_to_the_tick() {
    let harness = Harness::new(&Fixture::approved());
    let mut ledger = StewardLedger::default();
    ledger
        .pending_cancellations
        .insert("owner/repo:100".to_owned(), pending_cancellation_record());
    harness.save(&ledger);
    let outcome = harness.admit(HEAD, &observed(HEAD));
    assert_eq!(outcome.reason, "pending_cancellations", "{outcome:?}");
    assert_eq!(harness.enqueues(), 0);
}

#[test]
fn a_head_removed_for_a_defect_is_refused_by_the_admission() {
    let harness = Harness::new(&Fixture::approved());
    fs::write(
        harness.defect_fix_ledger(),
        format!(
            "{}\n",
            serde_json::json!({
                "at": "2026-11-13T00:10:00Z",
                "reason": "defect-fix",
                "note": "doomed group",
                "host": "m3",
                "session": "s",
                "targets": [{"repo": "owner/repo", "pr": 42, "head": HEAD}],
            })
        ),
    )
    .expect("removal");
    let outcome = harness.admit(HEAD, &observed(HEAD));
    assert_eq!(outcome.outcome, "refused", "{outcome:?}");
    assert!(
        outcome
            .mutation
            .as_deref()
            .is_some_and(|mutation| mutation.starts_with("defect_fix_removed")),
        "{outcome:?}"
    );
    assert_eq!(harness.enqueues(), 0);
}

#[test]
fn an_armed_pull_request_is_left_to_the_tick() {
    let harness = Harness::new(&Fixture::approved());
    let mut armed = observed(HEAD);
    armed.prs[0].fact.auto_merge_active = true;
    let outcome = harness.admit(HEAD, &armed);
    assert_eq!(outcome.reason, "already_queued_or_armed");
    assert!(harness.calls().is_empty(), "{}", harness.calls());
}

#[test]
fn a_base_with_no_queue_merges_the_exact_head() {
    let harness = Harness::new(&Fixture {
        no_queue: true,
        ..Fixture::approved()
    });
    let outcome = harness.admit(HEAD, &observed_no_queue(HEAD));
    assert_eq!(
        outcome.outcome,
        "admitted",
        "{outcome:?}\n{}",
        harness.calls()
    );
    assert!(
        outcome
            .mutation
            .as_deref()
            .is_some_and(|mutation| mutation.starts_with("merged_exact_head")),
        "{outcome:?}"
    );
    let calls = harness.calls();
    assert!(calls.contains("mergePullRequest("), "{calls}");
    assert!(calls.contains("expectedHeadOid"), "{calls}");
    assert!(!calls.contains("enqueuePullRequest"), "{calls}");
}
