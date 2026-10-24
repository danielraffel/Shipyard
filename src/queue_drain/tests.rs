use super::*;
use crate::merge_steward::StewardCheckSource;

const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const HEAD_C: &str = "cccccccccccccccccccccccccccccccccccccccc";
const HEAD_D: &str = "dddddddddddddddddddddddddddddddddddddddd";
const MAC: [&str; 6] = [
    "self-hosted",
    "macOS",
    "ARM64",
    "pulp-build",
    "pulp-build-vm",
    "pulp-build-pr-head",
];

fn required() -> Vec<RequiredCheck> {
    ["macos", "drift-fast"]
        .into_iter()
        .map(|context| RequiredCheck {
            context: context.to_owned(),
            app_id: None,
        })
        .collect()
}

fn check(name: &str, status: &str, conclusion: Option<&str>, run_id: u64) -> StewardCheck {
    StewardCheck {
        name: name.to_owned(),
        source: StewardCheckSource::CheckRun,
        app_id: None,
        status: status.to_owned(),
        conclusion: conclusion.map(str::to_owned),
        run_id: Some(run_id),
        observed_at: Some("2026-10-24T10:00:00Z".to_owned()),
    }
}

fn pr(number: u64, head: &str, merge_state: &str, checks: Vec<StewardCheck>) -> DrainPullRequest {
    DrainPullRequest {
        pr: StewardPullRequest {
            number,
            head_sha: head.to_owned(),
            head_branch: format!("branch-{number}"),
            author: None,
            draft: false,
            merge_state: merge_state.to_owned(),
            auto_merge_active: false,
            queue_position: None,
            labels: Vec::new(),
            checks,
        },
        title: format!("PR {number}"),
        body: String::new(),
    }
}

fn run(id: u64, pr_number: u64, head: &str, status: &str, event: &str) -> StewardRun {
    StewardRun {
        id,
        workflow_id: 7,
        run_attempt: 1,
        workflow: "Build and Test".to_owned(),
        head_sha: head.to_owned(),
        head_branch: format!("branch-{pr_number}"),
        status: status.to_owned(),
        event: event.to_owned(),
        pull_request_number: (event == "pull_request").then_some(pr_number),
        created_at: "2026-10-24T09:00:00Z".to_owned(),
        jobs: Vec::new(),
    }
}

fn job(id: u64, status: &str, minute: u32, labels: &[&str]) -> DrainJob {
    DrainJob {
        id,
        name: format!("job-{id}"),
        status: status.to_owned(),
        labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        created_at: format!("2026-10-24T10:{minute:02}:00Z"),
    }
}

#[test]
fn triage_marks_superseded_conflicting_and_required_failed_runs_doomed() {
    let prs = vec![
        pr(1, HEAD_B, "BLOCKED", Vec::new()),
        pr(2, HEAD_B, "DIRTY", Vec::new()),
        pr(
            3,
            HEAD_C,
            "BLOCKED",
            vec![check("drift-fast", "completed", Some("failure"), 900)],
        ),
        pr(4, HEAD_D, "BLOCKED", Vec::new()),
    ];
    let runs = vec![
        run(10, 1, HEAD_A, "queued", "pull_request"),
        run(11, 1, HEAD_B, "queued", "pull_request"),
        run(20, 2, HEAD_B, "in_progress", "pull_request"),
        run(30, 3, HEAD_C, "queued", "pull_request"),
        run(40, 4, HEAD_D, "queued", "pull_request"),
        run(50, 4, HEAD_D, "queued", "merge_group"),
        run(60, 99, HEAD_D, "queued", "pull_request"),
    ];
    let triage = plan_triage(&runs, &prs, &required());
    let verdicts = triage
        .iter()
        .map(|entry| (entry.run_id, entry.verdict.clone()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        verdicts[&10],
        RunVerdict::Doomed {
            reason: DoomReason::SupersededHead {
                current_head: HEAD_B.to_owned()
            }
        }
    );
    assert_eq!(verdicts[&11], RunVerdict::Viable);
    assert_eq!(
        verdicts[&20],
        RunVerdict::Doomed {
            reason: DoomReason::Conflicting {
                merge_state: "DIRTY".to_owned()
            }
        }
    );
    assert_eq!(
        verdicts[&30],
        RunVerdict::Doomed {
            reason: DoomReason::RequiredCheckFailed {
                contexts: vec!["drift-fast".to_owned()]
            }
        }
    );
    assert_eq!(verdicts[&40], RunVerdict::Viable);
    assert_eq!(verdicts[&50], RunVerdict::NotPullRequest);
    assert_eq!(verdicts[&60], RunVerdict::Unattributed);
}

#[test]
fn transient_or_self_produced_failures_do_not_doom_a_run() {
    let prs = vec![pr(
        5,
        HEAD_A,
        "BLOCKED",
        vec![
            check("macos", "completed", Some("cancelled"), 100),
            check("drift-fast", "completed", Some("failure"), 70),
        ],
    )];
    // Run 70 is a queued re-run of the run that produced the failure.
    let triage = plan_triage(
        &[
            run(70, 5, HEAD_A, "queued", "pull_request"),
            run(71, 5, HEAD_A, "queued", "pull_request"),
        ],
        &prs,
        &required(),
    );
    assert_eq!(triage[0].verdict, RunVerdict::Viable);
    assert!(matches!(triage[1].verdict, RunVerdict::Doomed { .. }));
}

#[test]
fn completed_runs_are_not_triaged() {
    let prs = vec![pr(1, HEAD_B, "BLOCKED", Vec::new())];
    let triage = plan_triage(
        &[run(10, 1, HEAD_A, "completed", "pull_request")],
        &prs,
        &required(),
    );
    assert!(triage.is_empty());
}

#[test]
fn branch_fallback_requires_a_unique_open_pull_request() {
    let prs = vec![pr(1, HEAD_B, "BLOCKED", Vec::new())];
    let mut orphan = run(10, 1, HEAD_A, "queued", "pull_request");
    orphan.pull_request_number = None;
    assert_eq!(
        pull_request_for_run(&orphan, &prs).map(|pr| pr.pr.number),
        Some(1)
    );
    let mut twins = prs.clone();
    twins.push(pr(2, HEAD_C, "BLOCKED", Vec::new()));
    twins[1].pr.head_branch = "branch-1".to_owned();
    assert!(pull_request_for_run(&orphan, &twins).is_none());
}

fn drain(run: StewardRun, jobs: Vec<DrainJob>) -> DrainRun {
    DrainRun { run, jobs }
}

#[test]
fn prioritize_cancels_only_queued_runs_ahead_on_competing_labels() {
    let prs = vec![
        pr(1, HEAD_A, "BLOCKED", Vec::new()),
        pr(2, HEAD_A, "BLOCKED", Vec::new()),
        pr(3, HEAD_A, "BLOCKED", Vec::new()),
        pr(4, HEAD_A, "BLOCKED", Vec::new()),
        pr(10931, HEAD_B, "BLOCKED", Vec::new()),
    ];
    let runs = vec![
        // Ahead, same macOS labels, nothing started: yields.
        drain(
            run(100, 1, HEAD_A, "in_progress", "pull_request"),
            vec![
                job(1001, "completed", 1, &["ubuntu-latest"]),
                job(1002, "queued", 5, &MAC),
            ],
        ),
        // Ahead on the same labels but a job is running: kept.
        drain(
            run(200, 2, HEAD_A, "in_progress", "pull_request"),
            vec![
                job(2001, "in_progress", 1, &["self-hosted", "Linux"]),
                job(2002, "queued", 6, &MAC),
            ],
        ),
        // Queued after the target: not ahead.
        drain(
            run(300, 3, HEAD_A, "queued", "pull_request"),
            vec![job(3001, "queued", 30, &MAC)],
        ),
        // Ahead but on unrelated labels: no competition.
        drain(
            run(400, 4, HEAD_A, "queued", "pull_request"),
            vec![job(4001, "queued", 2, &["self-hosted", "Windows"])],
        ),
        // Merge-group run ahead on the same labels: never touched.
        drain(
            run(500, 4, HEAD_C, "queued", "merge_group"),
            vec![job(5001, "queued", 1, &MAC)],
        ),
        // The prioritized PR itself.
        drain(
            run(900, 10931, HEAD_B, "in_progress", "pull_request"),
            vec![
                job(9001, "completed", 8, &["ubuntu-latest"]),
                job(9002, "queued", 10, &MAC),
            ],
        ),
    ];
    let plan = plan_prioritize(10931, &runs, &prs, &required());
    assert_eq!(plan.refusal, None);
    assert_eq!(plan.target_jobs, vec![9002]);
    assert_eq!(
        plan.yields
            .iter()
            .map(|entry| entry.run_id)
            .collect::<Vec<_>>(),
        vec![100]
    );
    assert_eq!(plan.yields[0].competing_jobs, vec!["job-1002".to_owned()]);
    assert_eq!(
        plan.kept
            .iter()
            .map(|entry| entry.run_id)
            .collect::<Vec<_>>(),
        vec![200]
    );
}

#[test]
fn prioritize_never_selects_a_run_with_an_in_progress_job() {
    let prs = vec![
        pr(1, HEAD_A, "BLOCKED", Vec::new()),
        pr(2, HEAD_B, "BLOCKED", Vec::new()),
    ];
    for running_status in ["in_progress", "IN_PROGRESS"] {
        let runs = vec![
            drain(
                run(100, 1, HEAD_A, "in_progress", "pull_request"),
                vec![
                    job(1001, running_status, 1, &MAC),
                    job(1002, "queued", 2, &MAC),
                ],
            ),
            drain(
                run(200, 2, HEAD_B, "queued", "pull_request"),
                vec![job(2001, "queued", 10, &MAC)],
            ),
        ];
        let plan = plan_prioritize(2, &runs, &prs, &required());
        assert!(plan.yields.is_empty(), "{running_status}: {plan:?}");
        assert_eq!(plan.kept.len(), 1);
    }
}

#[test]
fn prioritize_refuses_doomed_draft_missing_or_idle_targets() {
    let mut draft = pr(2, HEAD_B, "BLOCKED", Vec::new());
    draft.pr.draft = true;
    let prs = vec![
        pr(1, HEAD_A, "DIRTY", Vec::new()),
        draft,
        pr(3, HEAD_C, "BLOCKED", Vec::new()),
    ];
    let runs = vec![drain(
        run(300, 3, HEAD_C, "in_progress", "pull_request"),
        vec![job(3001, "in_progress", 1, &MAC)],
    )];
    assert!(matches!(
        plan_prioritize(1, &runs, &prs, &required()).refusal,
        Some(PrioritizeRefusal::Doomed { .. })
    ));
    assert_eq!(
        plan_prioritize(2, &runs, &prs, &required()).refusal,
        Some(PrioritizeRefusal::Draft)
    );
    assert_eq!(
        plan_prioritize(3, &runs, &prs, &required()).refusal,
        Some(PrioritizeRefusal::NothingWaiting)
    );
    assert_eq!(
        plan_prioritize(77, &runs, &prs, &required()).refusal,
        Some(PrioritizeRefusal::NotOpen)
    );
}

#[test]
fn label_competition_is_subset_based_and_case_insensitive() {
    let mac = job(1, "queued", 1, &MAC);
    let generic = job(2, "queued", 1, &["SELF-HOSTED"]);
    let windows = job(3, "queued", 1, &["self-hosted", "Windows"]);
    assert!(labels_compete(&mac, &generic));
    assert!(labels_compete(&generic, &windows));
    assert!(!labels_compete(&mac, &windows));
    assert!(!labels_compete(&mac, &job(4, "queued", 1, &[])));
}

#[test]
fn unblocking_candidates_need_a_label_or_a_fix_word_naming_a_red_context() {
    let base_checks = vec![
        check("macos", "completed", Some("failure"), 1),
        check("drift-fast", "completed", Some("success"), 2),
    ];
    let red = red_required_contexts(&base_checks, &required());
    assert_eq!(red, vec!["macos".to_owned()]);

    let mut fix = pr(10931, HEAD_A, "BLOCKED", Vec::new());
    fix.title = "Fix macos gate on main".to_owned();
    let mut mention_only = pr(2, HEAD_A, "BLOCKED", Vec::new());
    mention_only.body = "Adds a macos-only helper".to_owned();
    let mut fixture = pr(3, HEAD_A, "BLOCKED", Vec::new());
    fixture.body = "Update fixtures for macosx builds".to_owned();
    let mut labelled = pr(4, HEAD_A, "BLOCKED", Vec::new());
    labelled.pr.labels = vec!["Shipyard:Unblock".to_owned()];
    let mut conflicting = pr(5, HEAD_A, "DIRTY", Vec::new());
    conflicting.title = "fix macos".to_owned();
    let mut self_red = pr(
        6,
        HEAD_A,
        "BLOCKED",
        vec![check("macos", "completed", Some("failure"), 9)],
    );
    self_red.title = "fix macos".to_owned();

    let candidates = unblocking_candidates(
        &[fix, mention_only, fixture, labelled, conflicting, self_red],
        &red,
        &required(),
    );
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.pr_number)
            .collect::<Vec<_>>(),
        vec![10931, 4]
    );
}

fn entry(run_id: u64, pr_number: u64, head: &str) -> RestoreEntry {
    RestoreEntry {
        repo: "Generous-Corp/pulp".to_owned(),
        base: "main".to_owned(),
        run_id,
        workflow: "Build and Test".to_owned(),
        pr_number,
        head_sha: head.to_owned(),
        status_at_cancel: "queued".to_owned(),
        cause: CancelCause::YieldedTo { pr_number: 10931 },
        recorded_at: "2026-10-24T10:00:00Z".to_owned(),
    }
}

#[test]
fn restore_ledger_round_trips_through_json() {
    let mut ledger = RestoreLedger::default();
    ledger.record(entry(100, 1, HEAD_A));
    let mut doomed = entry(200, 2, HEAD_B);
    doomed.cause = CancelCause::Doomed {
        reason: DoomReason::RequiredCheckFailed {
            contexts: vec!["macos".to_owned()],
        },
    };
    ledger.record(doomed);
    let encoded = serde_json::to_string_pretty(&ledger).expect("encode");
    let decoded: RestoreLedger = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded, ledger);
    assert_eq!(decoded.version, 1);
    assert_eq!(
        decoded
            .for_repo("generous-corp/PULP")
            .iter()
            .map(|entry| entry.run_id)
            .collect::<Vec<_>>(),
        vec![100, 200]
    );
    let mut cleared = decoded;
    assert!(cleared.remove("Generous-Corp/pulp", 100).is_some());
    assert!(cleared.remove("Generous-Corp/pulp", 100).is_none());
    assert_eq!(cleared.entries.len(), 1);
    // An empty file body from an older writer still decodes.
    let legacy: RestoreLedger = serde_json::from_str("{}").expect("legacy");
    assert!(legacy.entries.is_empty());
}

#[test]
fn resume_reruns_only_open_same_head_runs_that_are_no_longer_doomed() {
    let prs = vec![
        pr(1, HEAD_A, "BLOCKED", Vec::new()),
        pr(2, HEAD_C, "BLOCKED", Vec::new()),
        pr(3, HEAD_A, "DIRTY", Vec::new()),
    ];
    assert_eq!(
        resume_decision(&entry(100, 1, HEAD_A), &prs, &required()),
        ResumeDecision::Rerun
    );
    assert!(matches!(
        resume_decision(&entry(200, 2, HEAD_B), &prs, &required()),
        ResumeDecision::Drop { .. }
    ));
    assert!(matches!(
        resume_decision(&entry(300, 3, HEAD_A), &prs, &required()),
        ResumeDecision::Keep {
            reason: DoomReason::Conflicting { .. }
        }
    ));
    assert!(matches!(
        resume_decision(&entry(400, 4, HEAD_A), &prs, &required()),
        ResumeDecision::Drop { .. }
    ));
}

#[test]
fn merge_queue_runs_pushes_and_enqueued_pull_requests_are_never_doomed() {
    let mut enqueued = pr(1, HEAD_B, "DIRTY", Vec::new());
    enqueued.pr.queue_position = Some(1);
    let prs = vec![enqueued, pr(2, HEAD_C, "DIRTY", Vec::new())];
    let mut readonly = run(30, 2, HEAD_C, "queued", "pull_request");
    readonly.head_branch = "gh-readonly-queue/main/pr-2-abc".to_owned();
    let mut push = run(40, 0, HEAD_D, "queued", "push");
    push.head_branch = "main".to_owned();
    let triage = plan_triage(
        &[
            run(10, 1, HEAD_A, "queued", "pull_request"),
            run(20, 2, HEAD_C, "queued", "merge_group"),
            readonly,
            push,
        ],
        &prs,
        &required(),
    );
    let verdicts = triage
        .iter()
        .map(|entry| (entry.run_id, entry.verdict.clone()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(verdicts[&10], RunVerdict::InMergeQueue);
    assert_eq!(verdicts[&20], RunVerdict::NotPullRequest);
    assert_eq!(verdicts[&30], RunVerdict::NotPullRequest);
    assert_eq!(verdicts[&40], RunVerdict::NotPullRequest);
}

#[test]
fn prioritize_never_yields_merge_queue_work_and_refuses_an_enqueued_target() {
    let mut enqueued = pr(1, HEAD_A, "BLOCKED", Vec::new());
    enqueued.pr.queue_position = Some(1);
    let prs = vec![enqueued, pr(2, HEAD_B, "BLOCKED", Vec::new())];
    let mut readonly = run(300, 1, HEAD_C, "queued", "pull_request");
    readonly.head_branch = "gh-readonly-queue/main/pr-1-abc".to_owned();
    let runs = vec![
        drain(
            run(100, 1, HEAD_A, "queued", "pull_request"),
            vec![job(1001, "queued", 1, &MAC)],
        ),
        drain(
            run(150, 1, HEAD_C, "queued", "merge_group"),
            vec![job(1501, "queued", 1, &MAC)],
        ),
        drain(readonly, vec![job(3001, "queued", 1, &MAC)]),
        drain(
            run(200, 2, HEAD_B, "queued", "pull_request"),
            vec![job(2001, "queued", 10, &MAC)],
        ),
    ];
    let plan = plan_prioritize(2, &runs, &prs, &required());
    assert!(plan.yields.is_empty(), "{plan:?}");
    assert_eq!(
        plan.kept.iter().map(|kept| kept.run_id).collect::<Vec<_>>(),
        vec![100]
    );
    assert_eq!(
        plan_prioritize(1, &runs, &prs, &required()).refusal,
        Some(PrioritizeRefusal::InMergeQueue)
    );
}

#[test]
fn resume_holds_an_enqueued_pull_request() {
    let mut enqueued = pr(1, HEAD_A, "BLOCKED", Vec::new());
    enqueued.pr.queue_position = Some(3);
    assert_eq!(
        resume_decision(&entry(100, 1, HEAD_A), &[enqueued], &required()),
        ResumeDecision::HoldForMergeQueue
    );
}
