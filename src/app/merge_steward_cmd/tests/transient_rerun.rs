//! Bounded transient reruns through the steward's apply entry point.
//!
//! Each scenario serves a fake GitHub with one PR whose required `macos`
//! check concluded `CANCELLED`, then runs [`apply_pr_plans`] with `--apply`.
//! Only a run whose jobs match the interruption signature is rerun, and only
//! once per head and check.
use super::*;
use crate::app::merge_steward_cmd::cancellation::apply_pr_plans;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// One job row of the run's latest attempt.
struct Job<'a> {
    conclusion: &'a str,
    runner: &'a str,
    created: &'a str,
    completed: &'a str,
}

const STARVED: Job<'static> = Job {
    conclusion: "cancelled",
    runner: "",
    created: "2026-10-11T08:02:00Z",
    completed: "2026-10-11T08:19:00Z",
};

struct Harness {
    temp: tempfile::TempDir,
    actions: GitHubActions,
    control: MutationControl,
    ledger: StewardLedger,
}

impl Harness {
    /// `run_id` is the run the live check names; `attempt` is GitHub's
    /// `run_attempt` for it.
    fn new(run_id: u64, attempt: u64, job: &Job<'_>, annotation: &str) -> Self {
        Self::with_total(run_id, attempt, job, annotation, 1)
    }

    /// `total` is the jobs listing's `total_count`; the listing always holds
    /// one job.
    fn with_total(run_id: u64, attempt: u64, job: &Job<'_>, annotation: &str, total: u64) -> Self {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls");
        let actions = fake_gh(
            &temp,
            &format!(
                r#"
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"{HEAD}","headRefName":"feature","mergeStateStatus":"BLOCKED","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"CANCELLED","completedAt":"2026-10-11T08:19:00Z","detailsUrl":"https://github.com/owner/repo/actions/runs/{run_id}"}}]}}' ;;
  *"actions/runs/{run_id}/attempts/{attempt}/jobs"*)
    printf '%s' '{{"total_count":{total},"jobs":[{{"id":9001,"name":"macos","status":"completed","conclusion":"{conclusion}","runner_name":"{runner}","created_at":"{created}","completed_at":"{completed}"}}]}}' ;;
  "api repos/owner/repo/actions/runs/{run_id}")
    printf '%s' '{{"id":{run_id},"workflow_id":77,"run_attempt":{attempt},"name":"Build","head_sha":"{HEAD}","head_branch":"feature","status":"completed","conclusion":"cancelled","event":"pull_request","pull_requests":[{{"number":42}}],"created_at":"2026-10-11T08:02:00Z"}}' ;;
  *"check-runs/9001/annotations"*) printf '%s' '[{annotation}]' ;;
  "run rerun "*) printf '%s\n' "$*" >> '{reruns}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
                log = log.display(),
                reruns = temp.path().join("reruns").display(),
                conclusion = job.conclusion,
                runner = job.runner,
                created = job.created,
                completed = job.completed,
            ),
        );
        let control = mutation_control(&temp, "m1", "m1");
        Self {
            temp,
            actions,
            control,
            ledger: StewardLedger::default(),
        }
    }

    fn observed(run_id: u64) -> ObservedPr {
        let mut pr = ready_pr();
        pr.fact.merge_state = "BLOCKED".to_owned();
        pr.fact.checks[0].conclusion = Some("CANCELLED".to_owned());
        pr.fact.checks[0].run_id = Some(run_id);
        pr
    }

    fn tick(&mut self, run_id: u64) -> PrReport {
        let observation = observation_for(Self::observed(run_id), true);
        let args = StewardCommandArgs {
            repos: vec!["owner/repo".to_owned()],
            base: "main".to_owned(),
            opt_out_label: "steward:skip".to_owned(),
            provenance_blocking_labels: default_provenance_blocking_labels(),
            managed_label: MANAGED_LABEL.to_owned(),
            handoff_context: HANDOFF_CONTEXT.to_owned(),
            max_transient_reruns: 1,
            recover_hosted_setup_eviction_priority: false,
            coalesce: true,
            preempt_capacity: true,
            max_preemptions_per_head: 1,
            arm_unqueued: false,
            approval_context: crate::merge_steward::DEFAULT_APPROVAL_CONTEXT.to_owned(),
            wake_owner: false,
            unapproved_dequeue: crate::app::merge_steward_cmd::DequeueMode::Report,
            apply: true,
            ledger: None,
        };
        let (mut reports, _) = apply_pr_plans(
            &self.actions,
            &args,
            &observation,
            &queue_policy(),
            &self.temp.path().join("ledger.json"),
            &mut self.ledger,
            Some(&self.control),
        );
        reports.remove(0)
    }

    fn reruns(&self) -> usize {
        fs::read_to_string(self.temp.path().join("reruns")).map_or(0, |text| text.lines().count())
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.temp.path().join("calls")).unwrap_or_default()
    }
}

#[test]
fn a_runnerless_cancel_in_the_timeout_window_is_rerun_once() {
    let mut harness = Harness::new(100, 1, &STARVED, "");
    let report = harness.tick(100);
    assert_eq!(
        report.decision,
        StewardDecision::RerunTransient { run_ids: vec![100] }
    );
    assert!(report.error.is_none(), "{report:?}");
    assert_eq!(
        report.mutation.as_deref(),
        Some("reran [100]"),
        "{report:?}"
    );
    assert_eq!(harness.reruns(), 1, "{}", harness.calls());
}

#[test]
fn a_second_interruption_on_the_same_head_is_not_rerun() {
    // GitHub already accepted one rerun of run 100 (attempt 2), and the
    // rerun was interrupted the same way.
    let mut harness = Harness::new(100, 2, &STARVED, "");
    harness
        .ledger
        .transient_attempts
        .insert(attempt_key("owner/repo", 42, HEAD, 100), 1);
    let report = harness.tick(100);
    assert!(
        matches!(report.decision, StewardDecision::RequiredFailed { .. }),
        "{report:?}"
    );
    assert_eq!(harness.reruns(), 0, "{}", harness.calls());

    // A lost ledger cannot buy a second rerun: GitHub's attempt fences it.
    let mut harness = Harness::new(100, 2, &STARVED, "");
    let report = harness.tick(100);
    assert!(
        report
            .mutation
            .as_deref()
            .is_some_and(|mutation| mutation.contains("budget exhausted")),
        "{report:?}"
    );
    assert_eq!(harness.reruns(), 0, "{}", harness.calls());
}

#[test]
fn a_fresh_run_of_an_already_rerun_check_is_not_rerun() {
    // Run 100 of `macos` was rerun once on this head; a new run 200 of the
    // same check on the same head is interrupted again.
    let mut harness = Harness::new(200, 1, &STARVED, "");
    harness
        .ledger
        .transient_attempts
        .insert(check_attempt_key("owner/repo", 42, HEAD, "macos"), 1);
    let report = harness.tick(200);
    assert!(
        matches!(report.decision, StewardDecision::RequiredFailed { .. }),
        "{report:?}"
    );
    assert_eq!(harness.reruns(), 0, "{}", harness.calls());
}

#[test]
fn a_genuine_cancel_or_failure_is_not_rerun() {
    let cases = [
        // A person cancelled the run after a runner took it.
        (
            Job {
                runner: "m3-vm-2",
                ..STARVED
            },
            "",
        ),
        // A supersede or a person ended it within seconds, with no runner.
        (
            Job {
                completed: "2026-10-11T08:02:40Z",
                ..STARVED
            },
            "",
        ),
        // Cancelled long after the runner-assignment timeout.
        (
            Job {
                completed: "2026-10-11T08:47:00Z",
                ..STARVED
            },
            "",
        ),
        // A concurrency supersede inside the window.
        (
            STARVED,
            r#"{"message":"Canceling since a higher priority waiting request for build exists"}"#,
        ),
        // The run's job ran and failed; the run conclusion still reads cancelled.
        (
            Job {
                conclusion: "failure",
                runner: "m3-vm-2",
                ..STARVED
            },
            "",
        ),
    ];
    for (job, annotation) in cases {
        let mut harness = Harness::new(100, 1, &job, annotation);
        let report = harness.tick(100);
        assert!(report.error.is_none(), "{report:?}");
        assert!(
            report
                .mutation
                .as_deref()
                .is_some_and(|mutation| mutation.starts_with("not an interruption: run 100")),
            "{report:?}"
        );
        assert_eq!(harness.reruns(), 0, "{}", harness.calls());
        assert!(
            harness
                .ledger
                .transient_attempts
                .values()
                .all(|count| *count == 0),
            "a refused rerun must not spend the budget: {:?}",
            harness.ledger.transient_attempts
        );
    }
}

#[test]
fn a_run_an_account_cancelled_is_not_rerun() {
    // The real annotation GitHub writes when an account cancels a run
    // (danielraffel/tartci job 85516736821, also runner-less).
    let annotations = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/transient-interruption/user-cancel-annotations.json"),
    )
    .expect("annotations");
    let inner = annotations
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .expect("annotation array");
    let mut harness = Harness::new(100, 1, &STARVED, inner);
    let report = harness.tick(100);
    assert!(
        report
            .mutation
            .as_deref()
            .is_some_and(|mutation| mutation.contains("cancelled by a person or tool")),
        "{report:?}"
    );
    assert_eq!(harness.reruns(), 0, "{}", harness.calls());
}

#[test]
fn a_partial_jobs_listing_is_refused_without_a_rerun() {
    let mut harness = Harness::with_total(100, 1, &STARVED, "", 2);
    let report = harness.tick(100);
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("could not be read completely")),
        "{report:?}"
    );
    assert_eq!(harness.reruns(), 0, "{}", harness.calls());
    assert!(
        harness.ledger.transient_attempts.is_empty(),
        "a refused read must not spend the budget: {:?}",
        harness.ledger.transient_attempts
    );
}
