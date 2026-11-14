use std::cell::RefCell;
use std::collections::BTreeMap;

use super::super::{
    CapacityPreemptionPolicy, ObservedPr, PrReport, RepoObservation, RequiredCheck,
    StewardDecision, StewardPullRequest,
};
use super::*;
use crate::head_approval::{Approval, HeadGate};

fn head(pr: u64) -> String {
    format!("{pr:040x}")
}

fn ready(pr: u64) -> Ready {
    Ready {
        pr,
        head: head(pr),
        draft: false,
        labels: Vec::new(),
    }
}

fn approved() -> HeadGate {
    HeadGate::Approved {
        approval: Approval::Review {
            login: "reviewer".to_owned(),
            performed_via_github_app_id: None,
        },
    }
}

/// Readers that log every read, approve every head, and list `files` per PR
/// (keyed by head).
struct Fake {
    files: BTreeMap<String, Vec<String>>,
    gates: BTreeMap<u64, HeadGate>,
    reads: RefCell<Vec<String>>,
}

impl Fake {
    fn new(files: &[(u64, &[&str])]) -> Self {
        Self {
            files: files
                .iter()
                .map(|(pr, names)| {
                    (
                        head(*pr),
                        names.iter().map(|name| (*name).to_owned()).collect(),
                    )
                })
                .collect(),
            gates: BTreeMap::new(),
            reads: RefCell::new(Vec::new()),
        }
    }

    fn plan(&self, policy: &Policy, ready: &[Ready]) -> Report {
        self.plan_with(policy, &rollup::Config::default(), ready)
    }

    fn plan_with(&self, policy: &Policy, train: &rollup::Config, ready: &[Ready]) -> Report {
        let gate = |pr: u64, sha: &str| {
            self.reads.borrow_mut().push(format!("gate #{pr}@{sha}"));
            Ok(self.gates.get(&pr).cloned().unwrap_or_else(approved))
        };
        let files = |sha: &str| {
            self.reads.borrow_mut().push(format!("files {sha}"));
            self.files
                .get(sha)
                .cloned()
                .ok_or_else(|| "no such head".to_owned())
        };
        plan(
            policy,
            train,
            &[".shipyard/**".to_owned()],
            ready,
            &Readers {
                gate: &gate,
                files: &files,
            },
        )
    }
}

fn members(report: &Report) -> Vec<u64> {
    report.members.iter().map(|member| member.pr).collect()
}

fn skipped(report: &Report, pr: u64) -> String {
    report
        .skipped
        .iter()
        .find(|skip| skip.pr == pr)
        .map(|skip| skip.why.clone())
        .unwrap_or_default()
}

fn policy(text: &str) -> Policy {
    Policy::from_table(&text.parse::<toml::Table>().expect("toml"))
}

#[test]
fn the_default_is_shadow_three_ready_four_members() {
    let policy = policy("");
    assert_eq!(policy.mode, Mode::Shadow);
    assert_eq!(policy.min_ready, 3);
    assert_eq!(policy.max_members, 4);
    assert!(policy.error.is_none());
    for path in DEFAULT_SENSITIVE_PATHS {
        assert!(policy.sensitive_paths.contains(&(*path).to_owned()));
    }
}

#[test]
fn bad_values_never_widen_anything() {
    let typo = policy("[auto_merge.rollup]\nmode = \"aply\"\nmax_members = 9\nmin_ready = 1\n");
    assert_eq!(
        typo.mode,
        Mode::Shadow,
        "an unknown mode is shadow, never apply"
    );
    assert_eq!((typo.min_ready, typo.max_members), (3, 4));
    assert!(
        typo.error
            .as_deref()
            .is_some_and(|error| error.contains("mode"))
    );
    let unreadable = policy("[auto_merge.rollup]\nsensitive_paths = \"src/**\"\n");
    assert!(unreadable.sensitive_paths.contains(&"**".to_owned()));
    let extra = policy("[auto_merge.rollup]\nmode = \"off\"\nsensitive_paths = [\"docs/**\"]\n");
    assert_eq!(extra.mode, Mode::Off);
    assert!(extra.sensitive_paths.contains(&"docs/**".to_owned()));
    assert!(
        extra
            .sensitive_paths
            .contains(&".github/workflows/**".to_owned()),
        "a configured list adds to the defaults, never replaces them"
    );
}

#[test]
fn below_the_threshold_there_is_no_rollup_and_no_read() {
    let fake = Fake::new(&[(1, &["a.rs"]), (2, &["b.rs"])]);
    let report = fake.plan(&Policy::default(), &[ready(1), ready(2)]);
    assert_eq!(report.action, "below_threshold");
    assert!(report.members.is_empty());
    assert!(fake.reads.borrow().is_empty(), "{:?}", fake.reads.borrow());
}

#[test]
fn a_backlog_of_disjoint_approved_heads_forms_one_rollup_oldest_first() {
    let fake = Fake::new(&[
        (14, &["d.rs"]),
        (11, &["a.rs"]),
        (12, &["b.rs"]),
        (13, &["c.rs"]),
        (15, &["e.rs"]),
    ]);
    let ready: Vec<Ready> = [11, 12, 13, 14, 15].into_iter().map(ready).collect();
    let report = fake.plan(&Policy::default(), &ready);
    assert_eq!(report.action, "shadow");
    assert_eq!(members(&report), vec![11, 12, 13, 14]);
    assert_eq!(report.members[0].head, head(11), "members name exact heads");
    assert!(skipped(&report, 15).contains("full"));
}

#[test]
fn overlapping_files_keep_the_newer_pull_request_out() {
    let fake = Fake::new(&[
        (1, &["src/a.rs", "src/shared.rs"]),
        (2, &["src/shared.rs"]),
        (3, &["src/c.rs"]),
    ]);
    let report = fake.plan(&Policy::default(), &[ready(1), ready(2), ready(3)]);
    assert_eq!(members(&report), vec![1, 3]);
    assert!(skipped(&report, 2).contains("#1 also changes src/shared.rs"));
}

#[test]
fn a_sensitive_path_keeps_a_pull_request_out() {
    for path in [
        ".github/workflows/ci.yml",
        "scripts/ghapp_queue_arm_guard.py",
        "scripts/pr_close_guard.sh",
        "src/head_approval.rs",
        "src/head_approval/carry.rs",
        ".shipyard/config.toml",
    ] {
        let fake = Fake::new(&[(1, &["a.rs"]), (2, &[path]), (3, &["c.rs"])]);
        let report = fake.plan(&Policy::default(), &[ready(1), ready(2), ready(3)]);
        assert_eq!(members(&report), vec![1, 3], "{path}");
        assert!(skipped(&report, 2).contains(path), "{path}: {report:?}");
    }
    let fake = Fake::new(&[(1, &["a.rs"]), (2, &["docs/x.md"]), (3, &["c.rs"])]);
    let report = fake.plan(
        &policy("[auto_merge.rollup]\nsensitive_paths = [\"docs/**\"]\n"),
        &[ready(1), ready(2), ready(3)],
    );
    assert_eq!(members(&report), vec![1, 3]);
}

#[test]
fn train_exclusions_and_hot_files_apply_too() {
    let fake = Fake::new(&[
        (1, &["Cargo.lock"]),
        (2, &["vendor/x.rs"]),
        (3, &["Cargo.toml"]),
        (4, &["d.rs"]),
    ]);
    let train = rollup::Config {
        exclude_paths: vec!["vendor/**".to_owned()],
        hot_files: vec!["Cargo.*".to_owned()],
        ..rollup::Config::default()
    };
    let report = fake.plan_with(
        &Policy::default(),
        &train,
        &[ready(1), ready(2), ready(3), ready(4)],
    );
    assert_eq!(members(&report), vec![1, 4]);
    assert!(skipped(&report, 2).contains("vendor/x.rs"));
    assert!(skipped(&report, 3).contains("hot file"));
}

#[test]
fn only_a_live_approval_naming_the_exact_head_counts() {
    let mut fake = Fake::new(&[
        (1, &["a.rs"]),
        (2, &["b.rs"]),
        (3, &["c.rs"]),
        (4, &["d.rs"]),
        (5, &["e.rs"]),
        (6, &["f.rs"]),
    ]);
    fake.gates.insert(
        2,
        HeadGate::Approved {
            approval: Approval::Carried {
                from_head: head(99),
                approval: Box::new(Approval::Review {
                    login: "reviewer".to_owned(),
                    performed_via_github_app_id: None,
                }),
                rebase: false,
            },
        },
    );
    fake.gates
        .insert(3, HeadGate::NotApproved { head: head(3) });
    fake.gates.insert(
        4,
        HeadGate::EjectionCap {
            head: head(4),
            ejections: Vec::new(),
        },
    );
    fake.gates.insert(5, HeadGate::NotRequired);
    let ready: Vec<Ready> = (1..=6).map(ready).collect();
    let report = fake.plan(&Policy::default(), &ready);
    assert_eq!(members(&report), vec![1, 6]);
    assert!(skipped(&report, 2).contains("carried"));
    assert!(!skipped(&report, 3).is_empty());
    assert!(!skipped(&report, 4).is_empty(), "the ejection cap holds");
    assert!(skipped(&report, 5).contains("not required"));
    // A head refused at the gate never has its files read.
    let log = fake.reads.borrow();
    for pr in [2, 3, 4, 5] {
        assert!(!log.contains(&format!("files {}", head(pr))), "#{pr}");
    }
}

#[test]
fn drafts_and_fleet_hold_labels_stay_out_without_a_read() {
    let fake = Fake::new(&[
        (1, &["a.rs"]),
        (2, &["b.rs"]),
        (3, &["c.rs"]),
        (4, &["d.rs"]),
    ]);
    let mut draft = ready(2);
    draft.draft = true;
    let mut parked = ready(3);
    parked.labels = vec!["PARKED/windows".to_owned()];
    let mut owner = ready(4);
    owner.labels = vec!["owner-decision".to_owned()];
    let report = fake.plan(&Policy::default(), &[ready(1), draft, parked, owner]);
    assert_eq!(
        report.action, "no_batch",
        "one eligible member is no rollup"
    );
    assert!(report.members.is_empty());
    assert!(skipped(&report, 2).contains("draft"));
    assert!(skipped(&report, 3).contains("PARKED/windows"));
    assert!(skipped(&report, 4).contains("owner-decision"));
    let reads = fake.reads.borrow();
    assert!(reads.iter().all(|read| !read.contains(&head(2))));
}

#[test]
fn unreadable_files_keep_a_pull_request_out() {
    let fake = Fake::new(&[(1, &["a.rs"]), (3, &["c.rs"])]);
    let report = fake.plan(&Policy::default(), &[ready(1), ready(2), ready(3)]);
    assert_eq!(members(&report), vec![1, 3]);
    assert!(skipped(&report, 2).contains("cannot be read"));
}

#[test]
fn apply_fails_closed_and_merges_nothing_as_a_batch() {
    let fake = Fake::new(&[(1, &["a.rs"]), (2, &["b.rs"]), (3, &["c.rs"])]);
    let report = fake.plan(
        &policy("[auto_merge.rollup]\nmode = \"apply\"\n"),
        &[ready(1), ready(2), ready(3)],
    );
    assert_eq!(report.action, "apply_unavailable");
    assert_eq!(members(&report), vec![1, 2, 3]);
    let detail = report.detail.unwrap_or_default();
    assert!(detail.contains("merges on its own"), "{detail}");
    assert!(detail.contains("[train]"), "{detail}");
}

fn observed(pr: u64, head_sha: &str) -> ObservedPr {
    ObservedPr {
        node_id: format!("PR_{pr}"),
        fact: StewardPullRequest {
            number: pr,
            head_sha: head_sha.to_owned(),
            head_branch: format!("feature-{pr}"),
            author: None,
            draft: false,
            merge_state: "CLEAN".to_owned(),
            auto_merge_active: false,
            queue_position: None,
            labels: Vec::new(),
            checks: Vec::new(),
        },
        owner: None,
        check_rollup_maybe_truncated: false,
    }
}

fn row(pr: u64, decision: StewardDecision) -> PrReport {
    PrReport {
        number: pr,
        head_sha: head(pr),
        decision,
        mutation: None,
        error: None,
        blocked: super::super::blocked_on::Stall::default(),
    }
}

fn observation(prs: Vec<ObservedPr>, merge_queue: bool) -> RepoObservation {
    RepoObservation {
        repo: "owner/repo".to_owned(),
        base: "main".to_owned(),
        allow_auto_merge: false,
        merge_queue,
        required_checks: vec![RequiredCheck {
            context: "review-verdict".to_owned(),
            app_id: Some(15368),
        }],
        prs,
        runs: Vec::new(),
        merge_group_heads: BTreeMap::new(),
        merge_group_enqueued_at: BTreeMap::new(),
        capacity_preemption_policy: CapacityPreemptionPolicy::pulp(),
        preemption_error: None,
    }
}

#[test]
fn only_ready_rows_at_the_observed_head_count() {
    let observation = observation(
        vec![
            observed(1, &head(1)),
            observed(2, &head(77)),
            observed(3, &head(3)),
            observed(4, &head(4)),
        ],
        false,
    );
    let rows = vec![
        row(4, StewardDecision::ArmMergeQueue),
        row(1, StewardDecision::ArmMergeQueue),
        // The head moved after the decision: not ready.
        row(2, StewardDecision::ArmMergeQueue),
        row(3, StewardDecision::Draft),
    ];
    let ready = ready_rows(&observation, &rows);
    assert_eq!(
        ready.iter().map(|ready| ready.pr).collect::<Vec<_>>(),
        vec![1, 4]
    );
}

/// `run` through a recording `gh` that answers only the base config read.
fn run_with(config: &str, merge_queue: bool, merge_path: &str) -> (Option<Report>, Vec<String>) {
    let calls = RefCell::new(Vec::new());
    let run_gh = |argv: &[String]| {
        calls.borrow_mut().push(argv.join(" "));
        if argv
            .iter()
            .any(|arg| arg.contains("contents/.shipyard/config.toml"))
        {
            Ok(config.to_owned())
        } else {
            Err("unexpected read".to_owned())
        }
    };
    let observation = observation(
        (1..=3).map(|pr| observed(pr, &head(pr))).collect(),
        merge_queue,
    );
    let rows: Vec<PrReport> = (1..=3)
        .map(|pr| row(pr, StewardDecision::ArmMergeQueue))
        .collect();
    let report = run(&run_gh, &observation, merge_path, &rows);
    (report, calls.into_inner())
}

#[test]
fn a_merge_queue_base_or_off_mode_does_nothing() {
    let (report, calls) = run_with("", true, "merge_queue");
    assert!(report.is_none());
    assert!(
        calls.is_empty(),
        "a merge-queue base reads nothing: {calls:?}"
    );
    let (report, calls) = run_with(
        "[auto_merge.rollup]\nmode = \"off\"\n",
        false,
        "server_enforced_exact_head_merge",
    );
    assert!(report.is_none());
    assert_eq!(calls.len(), 1, "off reads only the base config: {calls:?}");
}

#[test]
fn below_threshold_reads_only_the_base_config_and_writes_nothing() {
    let (report, calls) = run_with(
        "[auto_merge.rollup]\nmin_ready = 4\n",
        false,
        "server_enforced_exact_head_merge",
    );
    let report = report.expect("report");
    assert_eq!(report.action, "below_threshold");
    assert_eq!(report.ready, vec![1, 2, 3]);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        calls
            .iter()
            .all(|call| !call.contains("-X") && !call.contains("--method")),
        "{calls:?}"
    );
}

#[test]
fn unreadable_approval_reads_form_nothing() {
    // The config reads, nothing else does: no batch, no write.
    let (report, calls) = run_with("", false, "server_enforced_exact_head_merge");
    let report = report.expect("report");
    assert!(
        ["unreadable", "no_batch"].contains(&report.action),
        "{report:?}"
    );
    assert!(report.members.is_empty());
    assert!(
        calls
            .iter()
            .all(|call| !call.contains("-X") && !call.contains("mutation")),
        "{calls:?}"
    );
}
