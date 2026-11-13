//! The base-poison hold and the fix fast-track on the steward's enqueue.
//!
//! Each case runs the real enqueue path against a fake `gh` (the bundled
//! queue-arm guard included), with the pass's merge-group reading preset.
use std::collections::BTreeSet;

use super::*;
use crate::app::merge_steward_cmd::base_poison::RepoPoison;
use crate::app::merge_steward_cmd::pr_mutations::enqueue_pull_request;
use crate::base_health::merge_group::{GroupRef, MergeGroupHealth, PoisonedContext};
use crate::base_health::{AutoJump, BasePoisonPolicy, PoisonHold};
use crate::queue_arm_gate::tests_support::with_real_guard;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CONTEXT: &str = "Versioning & Skill-Sync";

/// `jump_answer` is what GitHub says to an enqueue that asks for the jump.
fn steward_gh(temp: &tempfile::TempDir, jump_answer: &str, comments: &str) -> GitHubActions {
    let log = temp.path().join("calls");
    fake_gh(
        temp,
        &format!(
            r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"contents/.shipyard/config.toml"*) printf '%s\n' '[auto_merge]' 'reviewer_signers = ["pulp-review"]' ;;
  *"--slurp"*"pulls/42/reviews"*) printf '%s' '[[]]' ;;
  *"--slurp"*"issues/42/comments"*) printf '%s' '[[{comments}]]' ;;
  *"node(id:"*) printf '%s' '{{"data":{{"node":{{"number":42,"state":"OPEN","headRefOid":"{head}","baseRefName":"main","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"repository":{{"nameWithOwner":"Generous-Corp/pulp"}},"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}' ;;
  "api repos/Generous-Corp/pulp/pulls/42") printf '%s' '{{"number":42,"base":{{"ref":"main"}},"head":{{"sha":"{head}"}},"user":{{"login":"shipyard-local[bot]","type":"Bot"}}}}' ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{head}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"{head}","headRefName":"fix/cadence","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"stackConfig"*) printf '%s' '{{"data":{{"repository":{{"stackConfig":{{"text":"stacked_pr_mode = \"observe\"\n"}},"pullRequest":{{"headRefOid":"{head}","stack":null,"stackEntry":null}}}}}}}}' ;;
  *"stackEntry"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"stack":null,"stackEntry":null}}}}}}}}' ;;
  *"enqueuePullRequest"*"jump:true"*) printf '%s' '{jump_answer}' ;;
  *"enqueuePullRequest"*) printf '%s' '{{"data":{{"enqueuePullRequest":{{"mergeQueueEntry":{{"position":3}}}}}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
            head = HEAD,
            comments = comments,
            jump_answer = jump_answer,
        ),
    )
}

fn approval() -> String {
    format!(
        r#"{{"id":6390564321,"user":{{"login":"shipyard-local[bot]","type":"Bot"}},"author_association":"CONTRIBUTOR","created_at":"2026-11-11T01:53:36Z","body":"reviewer: pulp-review\n<!-- session: Daniels-Mac-Studio-m3/08fb81d8 -->\nverdict: APPROVE head:{HEAD}"}}"#
    )
}

const JUMP_ACCEPTED: &str = r#"{"data":{"enqueuePullRequest":{"mergeQueueEntry":{"position":1}}}}"#;
const JUMP_FORBIDDEN: &str = r#"{"data":{"enqueuePullRequest":null},"errors":[{"type":"FORBIDDEN","message":"Viewer is not allowed to jump the queue"}]}"#;

fn poisoned(hold: PoisonHold, auto_jump: AutoJump, fixes: &[u64]) -> RepoPoison {
    let mut poison = streak_only(hold, auto_jump, fixes);
    poison.tip = crate::app::merge_steward_cmd::base_poison::red_tip(&poison.health);
    poison
}

/// The merge-group streak with the base tip not read red.
fn streak_only(hold: PoisonHold, auto_jump: AutoJump, fixes: &[u64]) -> RepoPoison {
    RepoPoison {
        policy: BasePoisonPolicy {
            hold,
            control_workflows: std::collections::BTreeMap::new(),
            pause: hold,
            auto_jump,
            read_error: None,
        },
        health: MergeGroupHealth::Poisoned {
            contexts: vec![PoisonedContext {
                context: CONTEXT.to_owned(),
                first_error: Some("source-contract recheck overdue".to_owned()),
                groups: vec![
                    GroupRef {
                        head_sha: "b".repeat(40),
                        prs: vec![12016],
                    },
                    GroupRef {
                        head_sha: "c".repeat(40),
                        prs: vec![12001],
                    },
                ],
                latest_failure_at: None,
                check_id: Some(1),
                same_first_error: None,
            }],
        },
        fixes: fixes
            .iter()
            .map(|pr| (*pr, vec!["label `shipyard:unblock`".to_owned()]))
            .collect(),
        fix_read_error: None,
        fix_ambiguity: None,
        candidates: std::collections::BTreeSet::default(),
        tip: std::collections::BTreeMap::new(),
    }
}

struct Outcome {
    mutation: Option<String>,
    error: Option<String>,
    calls: String,
}

fn enqueue(poison: Option<&RepoPoison>, jump_answer: &str) -> Outcome {
    enqueue_with(poison, jump_answer, &approval())
}

fn enqueue_with(poison: Option<&RepoPoison>, jump_answer: &str, comments: &str) -> Outcome {
    let temp = tempfile::tempdir().expect("temp");
    let actions = steward_gh(&temp, jump_answer, comments);
    let pr = ready_pr();
    let mut observation = observation_for(pr.clone(), true);
    observation.repo = "Generous-Corp/pulp".to_owned();
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let mut context =
        mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);
    context.base_poison = poison;
    let arms = temp
        .path()
        .join("state")
        .join(crate::queue_arm_gate::ARMS_LEDGER);
    let guards = temp.path().join("guards");
    fs::create_dir_all(&guards).expect("guards");
    let (mutation, error) = with_real_guard(&arms, &guards, || {
        crate::head_approval::with_host_review_default(false, || {
            enqueue_pull_request(&context, &pr, &queue_policy(), &mut ledger)
        })
    });
    Outcome {
        mutation,
        error,
        calls: fs::read_to_string(temp.path().join("calls")).unwrap_or_default(),
    }
}

fn enqueues(calls: &str) -> Vec<&str> {
    calls
        .lines()
        .filter(|line| line.contains("enqueuePullRequest"))
        .collect()
}

#[test]
fn a_poisoned_base_holds_an_approved_green_pull_request_that_is_not_the_fix() {
    let poison = poisoned(PoisonHold::On, AutoJump::On, &[12025]);
    let outcome = enqueue(Some(&poison), JUMP_ACCEPTED);
    assert_eq!(
        outcome.mutation.as_deref(),
        Some("held_base_poisoned:Versioning & Skill-Sync"),
        "{:?}",
        outcome.error
    );
    assert_eq!(outcome.error, None);
    assert!(enqueues(&outcome.calls).is_empty(), "{}", outcome.calls);
}

/// The merge-group streak alone, with the base tip not read red, holds
/// nothing and jumps nothing: the enqueue goes through plainly.
#[test]
fn a_streak_the_base_tip_does_not_confirm_neither_holds_nor_jumps() {
    let other = streak_only(PoisonHold::On, AutoJump::On, &[12025]);
    let outcome = enqueue(Some(&other), JUMP_ACCEPTED);
    assert_eq!(
        outcome.mutation.as_deref(),
        Some("enqueued"),
        "{:?}",
        outcome.error
    );
    let fix = streak_only(PoisonHold::On, AutoJump::On, &[42]);
    let outcome = enqueue(Some(&fix), JUMP_ACCEPTED);
    assert_eq!(
        outcome.mutation.as_deref(),
        Some("fast_track_fix,enqueued"),
        "{:?}",
        outcome.error
    );
    let enqueues = enqueues(&outcome.calls);
    assert_eq!(enqueues.len(), 1);
    assert!(!enqueues[0].contains("jump:true"), "{}", enqueues[0]);
}

#[test]
fn without_poison_the_same_pull_request_is_enqueued_plainly() {
    // Negative control for the hold: nothing read, nothing held.
    let outcome = enqueue(None, JUMP_ACCEPTED);
    assert_eq!(
        outcome.mutation.as_deref(),
        Some("enqueued"),
        "{:?}",
        outcome.error
    );
    let enqueues = enqueues(&outcome.calls);
    assert_eq!(enqueues.len(), 1);
    assert!(!enqueues[0].contains("jump:true"), "{}", enqueues[0]);
}

#[test]
fn a_dry_run_hold_reports_and_still_enqueues() {
    let poison = poisoned(PoisonHold::DryRun, AutoJump::Off, &[12025]);
    let outcome = enqueue(Some(&poison), JUMP_ACCEPTED);
    assert_eq!(
        outcome.mutation.as_deref(),
        Some("would_hold_base_poisoned:Versioning & Skill-Sync,enqueued"),
        "{:?}",
        outcome.error
    );
    assert_eq!(enqueues(&outcome.calls).len(), 1);
}

#[test]
fn the_named_fix_is_enqueued_at_the_front_when_auto_jump_is_on() {
    let poison = poisoned(PoisonHold::On, AutoJump::On, &[42]);
    let outcome = enqueue(Some(&poison), JUMP_ACCEPTED);
    assert_eq!(
        outcome.mutation.as_deref(),
        Some("fast_track_fix_jump,enqueued"),
        "{:?}",
        outcome.error
    );
    let enqueues = enqueues(&outcome.calls);
    assert_eq!(enqueues.len(), 1, "{enqueues:?}");
    assert!(enqueues[0].contains("jump:true"), "{}", enqueues[0]);
}

#[test]
fn the_named_fix_is_enqueued_without_a_jump_when_auto_jump_is_off() {
    // Negative control for the jump: same fix, auto_jump off.
    let poison = poisoned(PoisonHold::On, AutoJump::DryRun, &[42]);
    let outcome = enqueue(Some(&poison), JUMP_ACCEPTED);
    assert_eq!(outcome.mutation.as_deref(), Some("fast_track_fix,enqueued"));
    let enqueues = enqueues(&outcome.calls);
    assert_eq!(enqueues.len(), 1);
    assert!(!enqueues[0].contains("jump:true"), "{}", enqueues[0]);
}

#[test]
fn a_dry_run_hold_never_jumps_even_with_auto_jump_on() {
    let poison = poisoned(PoisonHold::DryRun, AutoJump::On, &[42]);
    let outcome = enqueue(Some(&poison), JUMP_ACCEPTED);
    assert_eq!(outcome.mutation.as_deref(), Some("fast_track_fix,enqueued"));
    let enqueues = enqueues(&outcome.calls);
    assert_eq!(enqueues.len(), 1);
    assert!(!enqueues[0].contains("jump:true"), "{}", enqueues[0]);
}

#[test]
fn a_refused_jump_still_enqueues_the_fix_at_the_back() {
    let poison = poisoned(PoisonHold::On, AutoJump::On, &[42]);
    let outcome = enqueue(Some(&poison), JUMP_FORBIDDEN);
    assert_eq!(
        outcome.mutation.as_deref(),
        Some("fast_track_fix_jump,enqueued"),
        "{:?}",
        outcome.error
    );
    let enqueues = enqueues(&outcome.calls);
    assert_eq!(enqueues.len(), 2, "{enqueues:?}");
    assert!(enqueues[0].contains("jump:true"));
    assert!(!enqueues[1].contains("jump:true"));
}

#[test]
fn the_fix_still_needs_its_exact_head_approval() {
    // The fast-track never excuses the approval: an unverdicted fix is refused
    // before any enqueue, jump or not.
    let poison = poisoned(PoisonHold::On, AutoJump::On, &[42]);
    let outcome = enqueue_with(Some(&poison), JUMP_ACCEPTED, "");
    assert_eq!(
        outcome.mutation.as_deref(),
        Some("fast_track_fix_jump,waiting_head_approval"),
        "{:?}",
        outcome.error
    );
    assert!(enqueues(&outcome.calls).is_empty(), "{}", outcome.calls);
}

#[test]
fn every_named_fix_is_admitted_and_every_other_pull_request_held() {
    use crate::app::merge_steward_cmd::base_poison::{Admission, admission};
    let poison = poisoned(PoisonHold::On, AutoJump::Off, &[7, 9]);
    let held = (1..12)
        .filter(|pr| matches!(admission(Some(&poison), *pr), Admission::Hold { .. }))
        .collect::<BTreeSet<_>>();
    assert_eq!(held, (1..12).filter(|pr| *pr != 7 && *pr != 9).collect());
    // The hold lifts with the poison: a clear reading admits everyone.
    let clear = RepoPoison {
        health: MergeGroupHealth::Clear {
            detail: "newest merge group passed".to_owned(),
            passed: Vec::new(),
            passed_by: std::collections::BTreeMap::new(),
        },
        ..poison.clone()
    };
    assert!((1..12).all(|pr| admission(Some(&clear), pr) == Admission::Proceed));
    // So does an unread one: an unreadable history never holds anything.
    let unread = RepoPoison {
        health: MergeGroupHealth::Unknown {
            detail: "HTTP 502".to_owned(),
        },
        ..poison
    };
    assert!((1..12).all(|pr| admission(Some(&unread), pr) == Admission::Proceed));
}

/// Red everywhere under `enforce`: a head still in a base-level episode is not
/// enqueued; under `shadow` (the default) the same head is enqueued as before.
fn enqueue_in_episode(mode: crate::merge_steward::ejection_guards::RedEverywhereMode) -> Outcome {
    let temp = tempfile::tempdir().expect("temp");
    let actions = steward_gh(&temp, JUMP_ACCEPTED, &approval());
    let pr = ready_pr();
    let mut observation = observation_for(pr.clone(), true);
    observation.repo = "Generous-Corp/pulp".to_owned();
    let mut ledger = StewardLedger::default();
    ledger.red_everywhere.insert(
        "generous-corp/pulp|Enforce version & skill sync|due_soon_fixture".to_owned(),
        crate::app::merge_steward_cmd::red_everywhere::Episode {
            first_epoch: 1,
            last_seen_epoch: 1,
            heads: [(pr.fact.number, pr.fact.head_sha.clone())]
                .into_iter()
                .collect(),
        },
    );
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let mut context =
        mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);
    context.ejection_guards.red_everywhere = mode;
    let arms = temp
        .path()
        .join("state")
        .join(crate::queue_arm_gate::ARMS_LEDGER);
    let guards = temp.path().join("guards");
    fs::create_dir_all(&guards).expect("guards");
    let (mutation, error) = with_real_guard(&arms, &guards, || {
        crate::head_approval::with_host_review_default(false, || {
            enqueue_pull_request(&context, &pr, &queue_policy(), &mut ledger)
        })
    });
    Outcome {
        mutation,
        error,
        calls: fs::read_to_string(temp.path().join("calls")).unwrap_or_default(),
    }
}

#[test]
fn red_everywhere_enforce_holds_an_unchanged_member_head_from_the_queue() {
    use crate::merge_steward::ejection_guards::RedEverywhereMode;
    let held = enqueue_in_episode(RedEverywhereMode::Enforce);
    assert_eq!(
        held.mutation.as_deref(),
        Some("held_red_everywhere:Enforce version & skill sync"),
        "{:?}",
        held.error
    );
    assert!(enqueues(&held.calls).is_empty(), "{}", held.calls);
    let shadow = enqueue_in_episode(RedEverywhereMode::Shadow);
    assert_eq!(enqueues(&shadow.calls).len(), 1, "{}", shadow.calls);
}
