//! The `update_branch` class through the carrier's plan-and-apply path.
//!
//! Each scenario builds a real origin repository (a bare repository at
//! `<temp>/owner/repo.git` with `refs/pull/42/head`) and a clone of it, and a
//! fake `gh` that answers GitHub's reads and performs `update-branch` by
//! writing a real merge commit into the origin. The carrier then plans from
//! those facts and applies an intent naming the carry.
use super::*;
use crate::merge_carrier::CarrierHold;
use std::process::Command;

const REVIEWER: &str = "shipyard-review[bot]";
const ARMED: &str = r#"{"data":{"enablePullRequestAutoMerge":{"pullRequest":{"number":42}}}}"#;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("git");
    assert!(output.status.success(), "git {args:?} failed: {output:?}");
    String::from_utf8(output.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn numbered(edits: &[(usize, &str)]) -> String {
    (1..=40)
        .map(|line| {
            edits
                .iter()
                .find(|(at, _)| *at == line)
                .map_or_else(|| format!("line {line}\n"), |(_, text)| format!("{text}\n"))
        })
        .collect()
}

/// What the base holds when the carry runs.
#[derive(Clone, Copy)]
enum BaseChange {
    /// An unrelated line inserted in the same file.
    Elsewhere,
    /// The pull request's own change, already landed on the base.
    SameAsPullRequest,
}

/// How the fake GitHub performs the update.
#[derive(Clone, Copy, PartialEq)]
enum Update {
    Merge,
    /// A merge commit whose tree drops the pull request's change.
    Tampered,
    /// The pull request's own lines on the base, but placed elsewhere in the
    /// file: the `-U0` own lines match while the tree is not the automatic
    /// merge.
    RelocatedLines,
    /// The base lands the pull request's change between the prediction and
    /// the update, and GitHub automatically merges that base: the tree is
    /// the automatic merge while the own lines are now empty.
    BaseLandsTheChange,
    /// The automatic merge of the approved head with a commit that is not on
    /// the base: tree and own lines both match when measured from it.
    OffBaseParent,
    /// A correct merge tree whose first parent is a rewrite of the approved
    /// head (same tree, different commit), not the approved head itself.
    RewrittenHead,
    /// GitHub refuses the update.
    Rejected,
}

/// A GitHub answer that disagrees with what the carrier expects.
#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    /// GitHub's live base is the base's parent, not what the clone fetches.
    StaleLiveBase,
    /// GitHub answers the disarm with the auto-merge request still on it.
    DisarmIgnored,
}

struct Scenario {
    merge_state: &'static str,
    armed: bool,
    review: Option<(&'static str, bool)>,
    prior_carries: u32,
    live_run: bool,
    base: BaseChange,
    update: Update,
    fault: Fault,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            merge_state: "BEHIND",
            armed: true,
            review: Some((REVIEWER, true)),
            prior_carries: 0,
            live_run: false,
            base: BaseChange::Elsewhere,
            update: Update::Merge,
            fault: Fault::None,
        }
    }
}

struct Fixture {
    temp: tempfile::TempDir,
    actions: GitHubActions,
    clone: PathBuf,
    head: String,
    observation: RepoObservation,
}

impl Fixture {
    fn new(scenario: &Scenario) -> Self {
        let temp = tempfile::tempdir().expect("temp");
        let work = temp.path().join("work");
        fs::create_dir_all(&work).expect("work");
        git(&work, &["-c", "init.defaultBranch=main", "init", "-q"]);
        fs::write(work.join("a.txt"), numbered(&[])).expect("base");
        git(&work, &["add", "."]);
        git(&work, &["commit", "-qm", "base"]);
        git(&work, &["checkout", "-qb", "feature"]);
        fs::write(work.join("a.txt"), numbered(&[(30, "feature")])).expect("feature");
        git(&work, &["commit", "-qam", "feature"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["checkout", "-q", "main"]);
        // The base inserts a line above the pull request's change, so every
        // later line number, the hunk headers, and the file's blob ids move.
        let base_edits: &[(usize, &str)] = match scenario.base {
            BaseChange::Elsewhere => &[(2, "main\nline 2")],
            BaseChange::SameAsPullRequest => &[(2, "main\nline 2"), (30, "feature")],
        };
        fs::write(work.join("a.txt"), numbered(base_edits)).expect("main");
        git(&work, &["commit", "-qam", "main moves"]);
        // A commit off the base that never lands on it.
        git(&work, &["checkout", "-qb", "off-base"]);
        fs::write(
            work.join("a.txt"),
            numbered(&[(2, "main\nline 2"), (10, "off base")]),
        )
        .expect("off base");
        git(&work, &["commit", "-qam", "off base"]);
        git(&work, &["checkout", "-q", "main"]);
        let owner = temp.path().join("owner");
        fs::create_dir_all(&owner).expect("owner");
        git(
            temp.path(),
            &["clone", "-q", "--bare", "work", "owner/repo.git"],
        );
        let bare = owner.join("repo.git");
        git(&bare, &["update-ref", "refs/pull/42/head", &head]);
        git(temp.path(), &["clone", "-q", "owner/repo.git", "clone"]);
        let clone = temp.path().join("clone");

        if scenario.armed {
            fs::write(temp.path().join("armed-head"), &head).expect("armed");
        }
        let actions = fake_gh(&temp, &gh_body(scenario, &bare, &head, temp.path()));
        let observation = observation(scenario.merge_state, &head);
        Self {
            temp,
            actions,
            clone,
            head,
            observation,
        }
    }

    fn planned(&self) -> CarrierRepoReport {
        planned_report(&self.actions, &self.observation)
    }

    fn apply(&self, report: &mut CarrierRepoReport, clones: &BTreeMap<String, PathBuf>) {
        let (store, global_dir) = authority(&self.temp, "m5s");
        let intent = CarrierIntent {
            schema_version: 1,
            actions: vec![IntentAction {
                repo: "owner/repo".to_owned(),
                number: 42,
                head_sha: self.head.clone(),
                action: CarrierAction::UpdateBranch {
                    head: self.head.clone(),
                },
            }],
        };
        apply_intent(
            &self.actions,
            &self.observation,
            self.temp.path(),
            clones,
            &intent,
            &BTreeSet::from([CarrierClass::UpdateBranch]),
            &store,
            RuntimeMode::Shipyard,
            &global_dir,
            report,
        );
    }

    fn clones(&self) -> BTreeMap<String, PathBuf> {
        BTreeMap::from([("owner/repo".to_owned(), self.clone.clone())])
    }

    fn calls(&self) -> String {
        gh_calls(&self.temp)
    }

    fn pr_head(&self) -> String {
        git(
            &self.temp.path().join("owner/repo.git"),
            &["rev-parse", "refs/pull/42/head"],
        )
    }

    /// The head native auto-merge is armed on, as the fake GitHub keeps it:
    /// set by an arm, cleared by a disarm, untouched by `update-branch`.
    fn armed_head(&self) -> Option<String> {
        fs::read_to_string(self.temp.path().join("armed-head"))
            .ok()
            .map(|head| head.trim().to_owned())
    }

    fn audit(&self) -> String {
        fs::read_to_string(self.temp.path().join("state/merge_queue/mutations.jsonl"))
            .unwrap_or_default()
    }
}

fn observation(merge_state: &str, head: &str) -> RepoObservation {
    RepoObservation {
        repo: "owner/repo".to_owned(),
        base: "main".to_owned(),
        allow_auto_merge: true,
        merge_queue: true,
        required_checks: vec![RequiredCheck {
            context: "macos".to_owned(),
            app_id: None,
        }],
        prs: vec![ObservedPr {
            node_id: "PR_kw".to_owned(),
            owner: Some("team-lead".to_owned()),
            fact: StewardPullRequest {
                number: 42,
                head_sha: head.to_owned(),
                author: Some("app/shipyard-local".to_owned()),
                head_branch: "feature".to_owned(),
                draft: false,
                merge_state: merge_state.to_owned(),
                auto_merge_active: true,
                queue_position: None,
                labels: Vec::new(),
                checks: vec![StewardCheck {
                    name: "macos".to_owned(),
                    source: StewardCheckSource::CheckRun,
                    app_id: None,
                    status: "COMPLETED".to_owned(),
                    conclusion: Some("SUCCESS".to_owned()),
                    run_id: Some(300),
                    observed_at: Some("2026-10-09T09:00:00Z".to_owned()),
                }],
            },
            check_rollup_maybe_truncated: false,
        }],
        runs: Vec::new(),
        merge_group_heads: BTreeMap::new(),
        merge_group_enqueued_at: BTreeMap::new(),
        capacity_preemption_policy: crate::merge_steward::CapacityPreemptionPolicy::for_repository(
            "owner/repo",
        ),
        preemption_error: None,
    }
}

fn gh_body(scenario: &Scenario, bare: &Path, head: &str, state: &Path) -> String {
    let bare = bare.display();
    let state = state.display();
    let (auto_merge, arm_event) = if scenario.armed {
        (
            r#"{"enabledAt":"2026-10-09T08:00:00Z"}"#,
            r#",{"__typename":"AutoMergeEnabledEvent","createdAt":"2026-10-09T08:00:00Z","actor":{"login":"shipyard-local"}}"#,
        )
    } else {
        ("null", "")
    };
    let timeline = format!(
        r#"{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{head}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":{auto_merge},"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[{{"__typename":"PullRequestCommit","commit":{{"oid":"{head}","checkSuites":{{"nodes":[{{"createdAt":"2026-10-09T07:00:00Z"}}]}}}}}}{arm_event}]}}}}}}}}}}"#
    );
    let reviews = scenario.review.map_or_else(String::new, |(login, on_head)| {
        let commit = if on_head {
            head.to_owned()
        } else {
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned()
        };
        format!(
            r#"{{"login":"{login}","type":"Bot","at":"2026-10-09T09:00:00Z","state":"APPROVED","commit_id":"{commit}","performed_via_github_app_id":999}}"#
        )
    });
    let recent = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let commits = (0..scenario.prior_carries)
        .map(|_| {
            format!(
                // The quotes around `main` are escaped for the fake's single-quoted printf.
                r#"{{"parents":2,"message":"Merge branch '"'"'main'"'"' into feature","email":"noreply@github.com","date":"{recent}"}}"#
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let (status, conclusion) = if scenario.live_run {
        ("in_progress", "null")
    } else {
        ("completed", r#""success""#)
    };
    let runs = format!(
        r#"{{"total_count":1,"workflow_runs":[{{"id":300,"path":".github/workflows/build.yml","name":"Build","event":"pull_request","head_sha":"{head}","status":"{status}","conclusion":{conclusion},"run_attempt":1,"run_started_at":"2026-10-09T08:00:00Z"}}]}}"#
    );
    let update = update_script(scenario.update, &bare, head);
    let live_base = if scenario.fault == Fault::StaleLiveBase {
        "refs/heads/main^"
    } else {
        "refs/heads/main"
    };
    // GitHub's answer to the disarm; an ignored disarm leaves the arm on.
    let disarm = if scenario.fault == Fault::DisarmIgnored {
        format!(
            "printf '%s' '{}'",
            r#"{"data":{"disablePullRequestAutoMerge":{"pullRequest":{"number":42,"autoMergeRequest":{"enabledAt":"2026-10-09T08:00:00Z"}}}}}"#
        )
    } else {
        format!(
            "rm -f '{state}/armed-head'\n    printf '%s' '{}'",
            r#"{"data":{"disablePullRequestAutoMerge":{"pullRequest":{"number":42,"autoMergeRequest":null}}}}"#
        )
    };
    format!(
        r#"
export GIT_AUTHOR_NAME=GitHub GIT_AUTHOR_EMAIL=noreply@github.com GIT_COMMITTER_NAME=GitHub GIT_COMMITTER_EMAIL=noreply@github.com
case "$*" in
  *"contents/.shipyard/config.toml"*) printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = true' 'reviewer_identities = ["{REVIEWER}"]' ;;
  *disablePullRequestAutoMerge*)
    {disarm} ;;
  *enablePullRequestAutoMerge*)
    for arg in "$@"; do case "$arg" in head=*) printf '%s' "${{arg#head=}}" > '{state}/armed-head' ;; esac; done
    printf '%s' '{ARMED}' ;;
  *timelineItems*) printf '%s' '{timeline}' ;;
  *"repos/owner/repo/pulls/42/reviews"*) printf '%s\n' '{reviews}' ;;
  *"--paginate repos/owner/repo/issues/42/comments"*) : ;;
  *"repos/owner/repo/issues/42/comments?per_page=100"*) printf '%s' '[]' ;;
  *"repos/owner/repo/pulls/42/commits"*) printf '%s\n' '{commits}' ;;
  *"actions/runs?head_sha={head}"*) printf '%s' '{runs}' ;;
  *"repos/owner/repo/branches/main"*) printf '{{"commit":{{"sha":"%s"}}}}' "$(git -C '{bare}' rev-parse {live_base})" ;;
  *"--method PUT repos/owner/repo/pulls/42/update-branch"*)
    {update}
    git -C '{bare}' update-ref refs/pull/42/head "$new"
    printf '%s' '{{"message":"Updating pull request branch."}}' ;;
  "api repos/owner/repo/pulls/42") printf '{{"head":{{"sha":"%s"}}}}' "$(git -C '{bare}' rev-parse refs/pull/42/head)" ;;
  *) printf '%s' '{{}}' ;;
esac
"#
    )
}

/// The fake GitHub's `update-branch`: shell that sets `new` to the commit the
/// update creates in the origin.
fn update_script(update: Update, bare: &dyn std::fmt::Display, head: &str) -> String {
    let merged =
        format!("$(git -C '{bare}' merge-tree --write-tree refs/heads/main {head} | head -n 1)");
    match update {
        Update::Merge => format!(
            "new=$(git -C '{bare}' commit-tree {merged} -p {head} -p refs/heads/main -m \"Merge branch 'main' into feature\")"
        ),
        Update::Tampered => format!(
            "new=$(git -C '{bare}' commit-tree $(git -C '{bare}' rev-parse 'refs/heads/main^{{tree}}') -p {head} -p refs/heads/main -m \"Merge branch 'main' into feature\")"
        ),
        Update::RelocatedLines => format!(
            "blob=$( (git -C '{bare}' show refs/heads/main:a.txt | grep -vx 'line 30'; echo feature) | git -C '{bare}' hash-object -w --stdin)\n    \
             tree=$(printf '100644 blob %s\\ta.txt\\n' \"$blob\" | git -C '{bare}' mktree)\n    \
             new=$(git -C '{bare}' commit-tree \"$tree\" -p {head} -p refs/heads/main -m \"Merge branch 'main' into feature\")"
        ),
        Update::BaseLandsTheChange => format!(
            "blob=$(git -C '{bare}' show refs/heads/main:a.txt | sed 's/^line 30$/feature/' | git -C '{bare}' hash-object -w --stdin)\n    \
             tree=$(printf '100644 blob %s\\ta.txt\\n' \"$blob\" | git -C '{bare}' mktree)\n    \
             landed=$(git -C '{bare}' commit-tree \"$tree\" -p refs/heads/main -m 'main lands the change')\n    \
             git -C '{bare}' update-ref refs/heads/main \"$landed\"\n    \
             new=$(git -C '{bare}' commit-tree {merged} -p {head} -p refs/heads/main -m \"Merge branch 'main' into feature\")"
        ),
        Update::OffBaseParent => format!(
            "new=$(git -C '{bare}' commit-tree $(git -C '{bare}' merge-tree --write-tree refs/heads/off-base {head} | head -n 1) -p {head} -p refs/heads/off-base -m \"Merge branch 'main' into feature\")"
        ),
        Update::RewrittenHead => format!(
            "rewritten=$(git -C '{bare}' commit-tree '{head}^{{tree}}' -p '{head}^' -m rewritten)\n    new=$(git -C '{bare}' commit-tree {merged} -p \"$rewritten\" -p refs/heads/main -m \"Merge branch 'main' into feature\")"
        ),
        Update::Rejected => "echo 'HTTP 422: merge conflict' >&2; exit 1".to_owned(),
    }
}

fn hold(report: &CarrierRepoReport) -> CarrierHold {
    match &report.prs[0].plan.decision {
        crate::merge_carrier::CarrierDecision::Hold { hold } => hold.clone(),
        crate::merge_carrier::CarrierDecision::Propose { action } => {
            panic!("expected a hold, got {action:?}")
        }
    }
}

/// Apply the carry intent and assert nothing was written to GitHub.
fn assert_not_carried(fixture: &Fixture, mut report: CarrierRepoReport) {
    fixture.apply(&mut report, &fixture.clones());
    assert!(report.prs[0].mutation.is_none(), "{:?}", report.prs[0]);
    let calls = fixture.calls();
    assert!(!calls.contains("update-branch"), "{calls}");
    assert!(!calls.contains("enablePullRequestAutoMerge"), "{calls}");
    assert!(!calls.contains("disablePullRequestAutoMerge"), "{calls}");
    assert_eq!(fixture.pr_head(), fixture.head);
}

#[test]
fn an_approved_armed_behind_head_is_carried_verified_and_rearmed() {
    let fixture = Fixture::new(&Scenario {
        prior_carries: 1,
        ..Scenario::default()
    });
    let mut report = fixture.planned();
    assert_eq!(
        report.prs[0].plan.action(),
        Some(&CarrierAction::UpdateBranch {
            head: fixture.head.clone()
        }),
        "{:?}",
        report.prs[0]
    );
    assert!(
        report.prs[0]
            .facts
            .carry_approval
            .as_deref()
            .is_some_and(|evidence| evidence.contains(REVIEWER) && evidence.contains("app=999")),
        "{:?}",
        report.prs[0].facts
    );
    fixture.apply(&mut report, &fixture.clones());
    let new_head = fixture.pr_head();
    assert_ne!(new_head, fixture.head);
    let mutation = report.prs[0].mutation.clone().unwrap_or_default();
    assert!(
        mutation.contains(&format!("carried {}", fixture.head))
            && mutation.contains(&format!("armed {new_head}")),
        "{:?}",
        report.prs[0]
    );
    let calls = fixture.calls();
    assert!(calls.contains("update_method=merge"), "{calls}");
    assert!(
        calls.contains(&format!("expected_head_sha={}", fixture.head)),
        "{calls}"
    );
    assert!(calls.contains(&format!("head={new_head}")), "{calls}");
    // Disarmed before the update, armed again only on the verified head.
    let disarm = calls.find("disablePullRequestAutoMerge").expect("disarm");
    let update = calls.find("update-branch").expect("update");
    let arm = calls.find("enablePullRequestAutoMerge").expect("arm");
    assert!(disarm < update && update < arm, "{calls}");
    assert_eq!(fixture.armed_head().as_deref(), Some(new_head.as_str()));
    let audit = fixture.audit();
    assert!(audit.contains("merge_tree:"), "{audit}");
    assert!(audit.contains("own_lines_sha256:"), "{audit}");
    assert!(
        audit.contains(&format!("approval:review:{REVIEWER}")),
        "{audit}"
    );
}

#[test]
fn only_a_behind_head_is_carried() {
    let fixture = Fixture::new(&Scenario {
        merge_state: "BLOCKED",
        ..Scenario::default()
    });
    let report = fixture.planned();
    assert_eq!(hold(&report), CarrierHold::AwaitingQueue);
    assert_not_carried(&fixture, report);
}

#[test]
fn an_unarmed_head_is_not_carried() {
    let fixture = Fixture::new(&Scenario {
        armed: false,
        ..Scenario::default()
    });
    let report = fixture.planned();
    assert_eq!(hold(&report), CarrierHold::Unarmed);
    assert_not_carried(&fixture, report);
}

#[test]
fn an_armed_head_without_a_reviewer_approval_is_not_carried() {
    // Arming is not approval, and the fleet author App cannot approve.
    for review in [None, Some(("shipyard-local[bot]", true))] {
        let fixture = Fixture::new(&Scenario {
            review,
            ..Scenario::default()
        });
        let report = fixture.planned();
        assert!(
            matches!(hold(&report), CarrierHold::CarryNotApproved { .. }),
            "{review:?}: {:?}",
            report.prs[0]
        );
        assert_not_carried(&fixture, report);
    }
}

#[test]
fn an_approval_of_an_earlier_head_is_not_carried() {
    let fixture = Fixture::new(&Scenario {
        review: Some((REVIEWER, false)),
        ..Scenario::default()
    });
    let report = fixture.planned();
    assert!(matches!(
        hold(&report),
        CarrierHold::CarryNotApproved { .. }
    ));
    assert_not_carried(&fixture, report);
}

#[test]
fn a_third_carry_in_a_day_is_refused() {
    let fixture = Fixture::new(&Scenario {
        prior_carries: 2,
        ..Scenario::default()
    });
    let report = fixture.planned();
    assert_eq!(hold(&report), CarrierHold::CarryBudgetSpent { carries: 2 });
    assert_not_carried(&fixture, report);
}

#[test]
fn a_live_run_on_the_head_holds_the_carry() {
    let fixture = Fixture::new(&Scenario {
        live_run: true,
        ..Scenario::default()
    });
    let report = fixture.planned();
    assert_eq!(
        hold(&report),
        CarrierHold::RunStillLive { run_ids: vec![300] }
    );
    assert_not_carried(&fixture, report);
}

#[test]
fn a_dirty_head_is_never_carried() {
    let fixture = Fixture::new(&Scenario {
        merge_state: "DIRTY",
        ..Scenario::default()
    });
    let report = fixture.planned();
    assert!(matches!(hold(&report), CarrierHold::Conflicting { .. }));
    assert_not_carried(&fixture, report);
}

#[test]
fn a_carry_that_would_change_the_own_lines_is_refused_before_the_update() {
    let fixture = Fixture::new(&Scenario {
        base: BaseChange::SameAsPullRequest,
        ..Scenario::default()
    });
    let mut report = fixture.planned();
    assert!(report.prs[0].plan.action().is_some(), "{:?}", report.prs[0]);
    fixture.apply(&mut report, &fixture.clones());
    let error = report.prs[0].error.clone().unwrap_or_default();
    assert!(error.contains("own lines"), "{:?}", report.prs[0]);
    let calls = fixture.calls();
    assert!(!calls.contains("update-branch"), "{calls}");
    assert!(!calls.contains("enablePullRequestAutoMerge"), "{calls}");
    // Refused before anything moved: the approved head stays armed.
    assert_eq!(fixture.armed_head().as_deref(), Some(fixture.head.as_str()));
}

/// Apply the carry and return its error; GitHub's arm, which survives an
/// `update-branch` by a writer, must be gone.
fn carry_left_disarmed(fixture: &Fixture) -> String {
    let mut report = fixture.planned();
    fixture.apply(&mut report, &fixture.clones());
    let error = report.prs[0].error.clone().unwrap_or_default();
    assert!(error.contains("left disarmed"), "{:?}", report.prs[0]);
    let calls = fixture.calls();
    assert!(calls.contains("disablePullRequestAutoMerge"), "{calls}");
    assert!(calls.contains("update-branch"), "{calls}");
    assert!(!calls.contains("enablePullRequestAutoMerge"), "{calls}");
    assert_eq!(fixture.armed_head(), None, "{calls}");
    assert!(
        fixture.audit().contains("carry_verification_failed"),
        "{}",
        fixture.audit()
    );
    error
}

#[test]
fn a_carried_tree_that_does_not_match_is_left_disarmed() {
    let fixture = Fixture::new(&Scenario {
        update: Update::Tampered,
        ..Scenario::default()
    });
    carry_left_disarmed(&fixture);
}

#[test]
fn a_carry_whose_first_parent_is_not_the_approved_head_is_left_disarmed() {
    // Same tree and own lines as a correct carry: only the first parent
    // shows that the approved commit is not what was carried.
    let fixture = Fixture::new(&Scenario {
        update: Update::RewrittenHead,
        ..Scenario::default()
    });
    let error = carry_left_disarmed(&fixture);
    assert!(
        error.contains("does not descend from the approved head"),
        "{error}"
    );
}

#[test]
fn a_carry_merging_a_commit_that_is_not_on_the_base_is_left_disarmed() {
    // First parent, tree and own lines all match when measured from the
    // second parent: only its absence from the base shows that the carry
    // brings in changes nobody reviewed.
    let fixture = Fixture::new(&Scenario {
        update: Update::OffBaseParent,
        ..Scenario::default()
    });
    let error = carry_left_disarmed(&fixture);
    assert!(error.contains("is not on main"), "{error}");
}

#[test]
fn a_carried_tree_that_is_not_the_automatic_merge_is_left_disarmed() {
    // The own lines match (the change is moved, which `-U0` cannot see): only
    // the tree shows that GitHub's merge was not the automatic one.
    let fixture = Fixture::new(&Scenario {
        update: Update::RelocatedLines,
        ..Scenario::default()
    });
    let error = carry_left_disarmed(&fixture);
    assert!(error.contains("is not the automatic merge"), "{error}");
}

#[test]
fn a_carry_whose_own_lines_changed_after_the_prediction_is_left_disarmed() {
    // The tree is the automatic merge of a base that landed the change after
    // the prediction: only the own lines show the approval no longer applies.
    let fixture = Fixture::new(&Scenario {
        update: Update::BaseLandsTheChange,
        ..Scenario::default()
    });
    let error = carry_left_disarmed(&fixture);
    assert!(error.contains("differ from the approved"), "{error}");
}

#[test]
fn a_disarm_that_leaves_auto_merge_on_stops_the_carry() {
    let fixture = Fixture::new(&Scenario {
        fault: Fault::DisarmIgnored,
        ..Scenario::default()
    });
    let mut report = fixture.planned();
    fixture.apply(&mut report, &fixture.clones());
    let error = report.prs[0].error.clone().unwrap_or_default();
    assert!(error.contains("could not disarm"), "{:?}", report.prs[0]);
    let calls = fixture.calls();
    assert!(calls.contains("disablePullRequestAutoMerge"), "{calls}");
    assert!(!calls.contains("update-branch"), "{calls}");
    assert_eq!(fixture.pr_head(), fixture.head);
    assert!(
        fixture.audit().contains("carry_disarm_failed"),
        "{}",
        fixture.audit()
    );
}

#[test]
fn a_fetched_base_that_is_not_the_live_base_is_refused_before_the_update() {
    let fixture = Fixture::new(&Scenario {
        fault: Fault::StaleLiveBase,
        ..Scenario::default()
    });
    let mut report = fixture.planned();
    fixture.apply(&mut report, &fixture.clones());
    let error = report.prs[0].error.clone().unwrap_or_default();
    assert!(error.contains("live main"), "{:?}", report.prs[0]);
    let calls = fixture.calls();
    assert!(!calls.contains("update-branch"), "{calls}");
    assert!(!calls.contains("disablePullRequestAutoMerge"), "{calls}");
    assert_eq!(fixture.armed_head().as_deref(), Some(fixture.head.as_str()));
}

#[test]
fn a_refused_update_re_arms_the_unchanged_approved_head() {
    let fixture = Fixture::new(&Scenario {
        update: Update::Rejected,
        ..Scenario::default()
    });
    let mut report = fixture.planned();
    fixture.apply(&mut report, &fixture.clones());
    let error = report.prs[0].error.clone().unwrap_or_default();
    assert!(
        error.contains("update branch failed"),
        "{:?}",
        report.prs[0]
    );
    assert_eq!(fixture.pr_head(), fixture.head);
    assert_eq!(fixture.armed_head().as_deref(), Some(fixture.head.as_str()));
}

#[test]
fn git_evidence_comes_from_the_target_repository_clone() {
    // The current directory is not a clone, and a clone of another repository
    // is refused: no update is attempted without the right evidence.
    let fixture = Fixture::new(&Scenario::default());
    let other = fixture.temp.path().join("other");
    git(
        fixture.temp.path(),
        &["clone", "-q", "owner/repo.git", "other"],
    );
    git(
        &other,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/someone/else.git",
        ],
    );
    for clones in [
        BTreeMap::new(),
        BTreeMap::from([("owner/repo".to_owned(), other.clone())]),
    ] {
        let mut report = fixture.planned();
        fixture.apply(&mut report, &clones);
        let error = report.prs[0].error.clone().unwrap_or_default();
        assert!(
            error.contains("no clone of owner/repo"),
            "{:?}",
            report.prs[0]
        );
        assert!(!fixture.calls().contains("update-branch"));
    }
}
