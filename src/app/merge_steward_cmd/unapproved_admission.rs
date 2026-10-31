//! Withdraw admission GitHub granted a head nobody approved.
//!
//! GitHub drops an armed auto-merge request on a push only when the pusher
//! lacks write access. Agents and the steward's own app have write access, so
//! a request armed for one head survives every later push. Once the new head's
//! checks go green GitHub enqueues it (on a queue base) or merges it (on a base
//! without one) itself: past the steward, past the `ghapp` arm guard, and past
//! exact-head approval. On enqueue GitHub consumes the request, so a queued
//! entry carries no arm left to disarm.
//!
//! The steward is the single admission writer for its base, so on every pass
//! it sweeps every open pull request on that base, owned or not, whatever the
//! base's merge path, and withdraws what the exact-head approval gate does not
//! admit:
//!
//! * an armed request on an unadmitted head is disabled
//!   (`disarmed_stale_auto_merge:<head>`);
//! * a merge-queue entry whose head is unadmitted is removed
//!   (`dequeued_unapproved:<head>`).
//!
//! A head the gate admits (`Approved`, or `NotRequired` where the base does not
//! require approval) keeps its arm and its queue entry: the steward would
//! enqueue that head itself, and the `--arm-unqueued` backstop arms exactly
//! such heads.
//!
//! The two withdrawals fail differently, because they cost differently. A
//! disarm only removes standing authority, so it fails closed: an approval or
//! live state that cannot be read, or a live head that moved since the census,
//! disarms. A dequeue throws away a merge group's runs, so it needs positive
//! evidence from a successful read of the queued head: a HOLD at that head, an
//! approval only for another head, no approval where one is required, or a
//! base config that a successful read found absent (a 404, typed
//! [`crate::head_approval::PolicyReadError::Absent`]). An unreadable approval,
//! live state (including a GraphQL answer with `errors` or no pull request) or
//! config defers it (`dequeue_deferred_unreadable`) and counts a
//! consecutive-unreadable streak in the steward ledger, keyed
//! `repo#pr:head`. Any successful judgement clears it; it is pruned when the
//! pull request leaves the queue or its head changes; the third consecutive
//! unreadable pass dequeues. At most [`MAX_DEQUEUES_PER_PASS`] entries leave a
//! repository's queue per pass; the rest report `dequeue_deferred_cap` and wait
//! for the next pass. A dequeue reached by the bound reports
//! `dequeued_unreadable_streak` rather than `dequeued_unapproved`, and every
//! deferral returns its read error as the PR's error, so the pass is
//! unhealthy and the deferral shows in the tick summary. A dry run reports
//! `would_disarm_stale_auto_merge` and `would_dequeue_unapproved` (or
//! `would_dequeue_unreadable_streak`) and changes nothing, including the
//! streak.
//!
//! A removal the steward makes is recorded in the ledger and is not counted
//! toward [`crate::head_approval::EJECTION_CAP`] when the steward judges that
//! head again: withdrawing an entry it could not judge says nothing against
//! the head, so it must not cost an approved head its approval.
//!
//! Both mutations run under the PR mutation guard on Shipyard's internal
//! queue-mutation path, whose one-shot `internal-removal` nonce the `ghapp`
//! `queue-removal-guard` accepts as an audited Shipyard removal.
//!
//! An armed request on an owned pull request is disarmed unconditionally,
//! approved or not, by [`super::exact_head_merge::disarm_owned_auto_merge`]:
//! the steward admits owned heads itself.

use chrono::{DateTime, Utc};

use super::{
    GitHubActions, MutationApplyContext, ObservedPr, RepoObservation, StewardDecision,
    StewardLedger, Value, acquire_pr_mutation_guard,
    exact_head_merge::{disarm_under_guard, owns_for_disarm},
    pr_mutations::{evaluate_head_gate, read_pr_queue_state_strict},
    record_audit,
};
use crate::auto_arm::first_graphql_error;
use crate::head_approval::{ApprovalPolicy, HeadGate, PolicyReadError};

/// The ledger action recorded for each disarm.
const DISARM_AUDIT: &str = "disarm_stale_auto_merge";
/// The ledger action recorded for each dequeue.
const DEQUEUE_AUDIT: &str = "dequeue_unapproved_queue_entry";
/// Consecutive PR-specific unreadable passes after which a queued head is
/// dequeued.
pub(super) const UNREADABLE_PASSES_TO_DEQUEUE: u32 = 3;
/// Most queue entries removed per repository and base per pass.
pub(super) const MAX_DEQUEUES_PER_PASS: usize = 2;
/// How far a timeline removal may sit from the time the steward recorded its
/// own dequeue and still be that dequeue.
const REMOVAL_MATCH_WINDOW_SECONDS: i64 = 180;

/// GraphQL document that removes a pull request from its merge queue.
const DEQUEUE_MUTATION: &str =
    "mutation($id:ID!){dequeuePullRequest(input:{id:$id}){mergeQueueEntry{id}}}";

/// `[merge_steward] unapproved_dequeue` in the machine-global config: whether
/// the sweep removes unapproved queue entries or only reports what it would
/// remove. The disarm is not governed by it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum DequeueMode {
    /// Compute and report `would_dequeue_*`; never call `dequeuePullRequest`.
    #[default]
    Report,
    Apply,
}

impl DequeueMode {
    /// The mode named by `value`, the config's string.
    ///
    /// # Errors
    ///
    /// For any value other than `report` or `apply`.
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "report" => Ok(Self::Report),
            "apply" => Ok(Self::Apply),
            other => Err(format!(
                "merge_steward.unapproved_dequeue must be \"report\" or \"apply\", not {other:?}"
            )),
        }
    }

    /// The mode the machine-global config at `global_dir` selects;
    /// [`Self::Report`] when it is not set.
    ///
    /// # Errors
    ///
    /// When the config cannot be loaded or names an unknown mode.
    pub(super) fn from_machine_global(global_dir: &std::path::Path) -> Result<Self, String> {
        let config =
            crate::config::LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
                .map_err(|error| {
                    format!("failed to load merge_steward.unapproved_dequeue: {error}")
                })?;
        match config.get("merge_steward.unapproved_dequeue") {
            None => Ok(Self::default()),
            Some(value) => value.as_str().map_or_else(
                || Err("merge_steward.unapproved_dequeue must be a string".to_owned()),
                Self::parse,
            ),
        }
    }
}

/// The ledger key for one head of one pull request.
fn head_key(repo: &str, number: u64, head: &str) -> String {
    format!("{repo}#{number}:{}", head.to_ascii_lowercase())
}

/// `(number, head)` from a [`head_key`] of `repo`, or `None` for another repo.
fn parse_head_key<'a>(key: &'a str, repo: &str) -> Option<(u64, &'a str)> {
    let rest = key.strip_prefix(repo)?.strip_prefix('#')?;
    let (number, head) = rest.split_once(':')?;
    Some((number.parse().ok()?, head))
}

/// When the steward itself removed `head` from the queue.
pub(super) fn steward_removal_times(
    ledger: &StewardLedger,
    repo: &str,
    number: u64,
    head: &str,
) -> Vec<DateTime<Utc>> {
    ledger
        .steward_removals
        .get(&head_key(repo, number, head))
        .into_iter()
        .flatten()
        .filter_map(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
        .collect()
}

/// Whether timeline `node` is a non-merge queue removal of `head`.
fn is_removal_of(node: &Value, head: &str) -> bool {
    node.get("__typename").and_then(Value::as_str) == Some("RemovedFromMergeQueueEvent")
        && node
            .pointer("/beforeCommit/oid")
            .and_then(Value::as_str)
            .is_some_and(|oid| oid.eq_ignore_ascii_case(head))
        && !node
            .get("reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason.eq_ignore_ascii_case("merged"))
}

/// `queue` with the removals of `head` that `keep` rejects taken out of its
/// timeline.
fn filter_removals(queue: &Value, head: &str, mut keep: impl FnMut(&Value) -> bool) -> Value {
    let mut queue = queue.clone();
    if let Some(nodes) = queue
        .pointer_mut("/data/repository/pullRequest/timelineItems/nodes")
        .and_then(Value::as_array_mut)
    {
        nodes.retain(|node| !is_removal_of(node, head) || keep(node));
    }
    queue
}

/// `queue` without the removals of `head` the steward recorded making: each
/// recorded time excuses the one removal closest to it within
/// [`REMOVAL_MATCH_WINDOW_SECONDS`], so a removal anyone else made still counts
/// toward the ejection cap.
pub(super) fn without_steward_removals(
    queue: &Value,
    head: &str,
    recorded: &[DateTime<Utc>],
) -> Value {
    let mut unclaimed = recorded.to_vec();
    filter_removals(queue, head, |node| {
        let Some(at) = node
            .get("createdAt")
            .and_then(Value::as_str)
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Utc))
        else {
            return true;
        };
        let nearest = unclaimed
            .iter()
            .enumerate()
            .map(|(index, mine)| (index, (*mine - at).num_seconds().abs()))
            .filter(|(_, distance)| *distance <= REMOVAL_MATCH_WINDOW_SECONDS)
            .min_by_key(|(_, distance)| *distance);
        nearest.is_none_or(|(index, _)| {
            unclaimed.remove(index);
            false
        })
    })
}

/// `queue` without its latest removal of `head`: the excuse the arming paths
/// give a classified interruption ([`crate::head_approval::decide_excusing`]).
fn without_latest_removal(queue: &Value, head: &str) -> Value {
    let latest = queue
        .pointer("/data/repository/pullRequest/timelineItems/nodes")
        .and_then(Value::as_array)
        .and_then(|nodes| nodes.iter().rposition(|node| is_removal_of(node, head)));
    let mut queue = queue.clone();
    if let (Some(latest), Some(nodes)) = (
        latest,
        queue
            .pointer_mut("/data/repository/pullRequest/timelineItems/nodes")
            .and_then(Value::as_array_mut),
    ) {
        nodes.remove(latest);
    }
    queue
}

/// Drop streaks for a pull request that is no longer queued at that head, and
/// steward-removal records for a pull request that is gone or moved on.
pub(super) fn prune(ledger: &mut StewardLedger, observation: &RepoObservation) {
    let census = |number: u64| observation.prs.iter().find(|pr| pr.fact.number == number);
    ledger.dequeue_unreadable.retain(|key, _| {
        parse_head_key(key, &observation.repo).is_none_or(|(number, head)| {
            census(number).is_some_and(|pr| {
                pr.fact.queue_position.is_some() && pr.fact.head_sha.eq_ignore_ascii_case(head)
            })
        })
    });
    let open_at_head = |key: &String| {
        parse_head_key(key, &observation.repo).is_none_or(|(number, head)| {
            census(number).is_some_and(|pr| pr.fact.head_sha.eq_ignore_ascii_case(head))
        })
    };
    ledger.steward_removals.retain(|key, _| open_at_head(key));
    // Superseded by `steward_removals`, which records when; dropped as read.
    ledger
        .steward_dequeues
        .retain(|key, _| parse_head_key(key, &observation.repo).is_none());
}

/// Whether a read failure says nothing about this one pull request: a
/// base-wide input, or a transport, server, rate-limit or credential failure
/// that would fail every PR alike. Such a failure defers without advancing
/// any PR's streak, so an outage can never drain the queue.
pub(super) fn is_systemic_failure(error: &str) -> bool {
    let status = crate::head_approval::gh_error_http_status(error);
    let lowered = error.to_ascii_lowercase();
    status.is_some_and(|code| code >= 500 || matches!(code, 401 | 403 | 429))
        || [
            "rate limit",
            "timed out",
            "timeout",
            "deadline",
            "failed with status signal",
            "connection",
            "could not resolve",
            "bad credentials",
            "installation",
            "token",
        ]
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// The base's reviewer policy, as one read found it.
enum PolicyRead {
    Policy(ApprovalPolicy),
    /// A successful, confirmed read found no config on the base.
    Absent(String),
    Unreadable(String),
}

/// State shared by every pull request in one repository's pass.
#[derive(Default)]
pub(super) struct SweepPass {
    mode: DequeueMode,
    policy: Option<PolicyRead>,
    dequeues: usize,
}

impl SweepPass {
    pub(super) fn new(mode: DequeueMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// The reviewer policy, read at most once per pass.
    fn policy(&mut self, actions: &GitHubActions, observation: &RepoObservation) -> &PolicyRead {
        self.policy.get_or_insert_with(|| {
            let run_gh = |argv: &[String]| actions.run_gh(argv).map_err(|error| error.to_string());
            match crate::head_approval::read_policy_required(
                &run_gh,
                &observation.repo,
                &observation.base,
            ) {
                Ok(policy) => PolicyRead::Policy(policy),
                Err(PolicyReadError::Absent(detail)) => PolicyRead::Absent(detail),
                Err(PolicyReadError::Unreadable(detail)) => PolicyRead::Unreadable(detail),
            }
        })
    }
}

/// Whether the sweep judges `pr` on this pass, from census facts.
pub(super) fn applies(pr: &ObservedPr, decision: &StewardDecision) -> bool {
    pr.fact.queue_position.is_some() || (pr.fact.auto_merge_active && !owns_for_disarm(decision))
}

/// The gate's verdict on one head.
enum Verdict {
    Admitted,
    /// Positive evidence from successful reads that the head is not admitted.
    Refused(String),
    /// The head was approved but has reached the ejection cap since. The arm
    /// is withdrawn (fail closed); the entry is not, because the cap alone
    /// says nothing against a head somebody approved.
    EjectionCapped(String),
    /// `systemic` when the failure is base-wide or transport-wide.
    Unreadable {
        why: String,
        systemic: bool,
    },
}

impl Verdict {
    fn unreadable(why: String) -> Self {
        let systemic = is_systemic_failure(&why);
        Self::Unreadable { why, systemic }
    }
}

/// The classified-interruption excuse for `head`'s latest ejection, exactly
/// as the `--arm-unqueued` backstop and `ship` apply it: only while the queue
/// state is that ejection, and only when the repository opted in.
fn interruption_excused(
    actions: &GitHubActions,
    observation: &RepoObservation,
    queue: &Value,
) -> bool {
    let report = crate::pr_queue_state::explain_pr_queue_state(queue);
    if !crate::environment_requeue::applies(&report) {
        return false;
    }
    let run_gh = |argv: &[String]| actions.run_gh(argv).map_err(|error| error.to_string());
    crate::environment_requeue::read_opt_in(&run_gh, &observation.repo, &observation.base)
        .ok()
        .and_then(|opted_in| {
            crate::environment_requeue::assess(&run_gh, &observation.repo, &report, opted_in)
        })
        .is_some_and(|verdict| verdict.is_allowed_interruption())
}

fn verdict(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr: &ObservedPr,
    head: &str,
    queue: &Value,
    pass: &mut SweepPass,
    ledger: &StewardLedger,
) -> Verdict {
    let policy = match pass.policy(actions, observation) {
        PolicyRead::Unreadable(error) => {
            // Base-wide: it would fail every PR alike.
            return Verdict::Unreadable {
                why: format!("head approval policy unreadable: {error}"),
                systemic: true,
            };
        }
        PolicyRead::Absent(detail) => return Verdict::Refused(detail.clone()),
        PolicyRead::Policy(policy) => policy.clone(),
    };
    let judge =
        |queue: &Value| evaluate_head_gate(actions, observation, pr, head, &policy, queue, ledger);
    match judge(queue) {
        Ok((gate, _events)) if gate.allows() => Verdict::Admitted,
        Ok((HeadGate::EjectionCap { .. }, _events)) => {
            if interruption_excused(actions, observation, queue)
                && judge(&without_latest_removal(queue, head))
                    .is_ok_and(|(gate, _events)| gate.allows())
            {
                return Verdict::Admitted;
            }
            // Was the head approved at all, ejections aside?
            match judge(&filter_removals(queue, head, |_| false)) {
                Ok((gate, _events)) if gate.allows() => Verdict::EjectionCapped(format!(
                    "head {} is approved but reached the ejection cap",
                    head.get(..12).unwrap_or(head)
                )),
                Ok((gate, _events)) => Verdict::Refused(gate.explain()),
                Err(error) => Verdict::unreadable(error),
            }
        }
        Ok((gate, _events)) => Verdict::Refused(gate.explain()),
        Err(error) => Verdict::unreadable(error),
    }
}

/// What to do about the queue entry.
#[derive(Debug, Eq, PartialEq)]
enum EntryPlan {
    /// Not queued, or admitted: nothing.
    Keep,
    Dequeue {
        head: String,
        reason: String,
        /// Reached by the consecutive-unreadable bound rather than by
        /// positive evidence.
        streak: bool,
    },
    /// Unreadable, below the bound or systemic; `why` is the read error.
    Deferred { head: String, why: String },
}

/// What the sweep plans for `pr`.
struct Plan {
    /// `Some(reason)` when the arm is withdrawn.
    disarm: Option<String>,
    entry: EntryPlan,
    /// The head a PR-specific unreadable judgement counts against.
    unreadable_head: Option<String>,
    /// Whether a judgement of the queued head succeeded (clears its streak).
    judged: bool,
}

#[allow(clippy::too_many_lines)] // The arm and the entry are judged from one live read.
fn plan(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr: &ObservedPr,
    owned: bool,
    pass: &mut SweepPass,
    ledger: &StewardLedger,
) -> Plan {
    let census_head = pr.fact.head_sha.as_str();
    let census_armed = pr.fact.auto_merge_active && !owned;
    let census_queued = pr.fact.queue_position.is_some();
    let streak = |head: &str| {
        ledger
            .dequeue_unreadable
            .get(&head_key(&observation.repo, pr.fact.number, head))
            .copied()
            .unwrap_or(0)
            .saturating_add(1)
    };
    let unreadable = |head: &str, why: String, systemic: bool, disarm: Option<String>| {
        if systemic {
            // An outage says nothing about this entry: defer, and leave its
            // streak exactly where it was.
            return Plan {
                disarm,
                entry: EntryPlan::Deferred {
                    head: head.to_owned(),
                    why,
                },
                unreadable_head: None,
                judged: false,
            };
        }
        let passes = streak(head);
        Plan {
            disarm,
            entry: if passes >= UNREADABLE_PASSES_TO_DEQUEUE {
                EntryPlan::Dequeue {
                    head: head.to_owned(),
                    reason: format!("unreadable on {passes} consecutive passes: {why}"),
                    streak: true,
                }
            } else {
                EntryPlan::Deferred {
                    head: head.to_owned(),
                    why,
                }
            },
            unreadable_head: Some(head.to_owned()),
            judged: false,
        }
    };
    let queue = match read_pr_queue_state_strict(actions, observation, pr.fact.number) {
        Ok(queue) => queue,
        Err(error) => {
            // Fail closed on the arm; defer the entry.
            let disarm = census_armed.then(|| error.clone());
            if !census_queued {
                return Plan {
                    disarm,
                    entry: EntryPlan::Keep,
                    unreadable_head: None,
                    judged: false,
                };
            }
            let systemic = is_systemic_failure(&error);
            return unreadable(census_head, error, systemic, disarm);
        }
    };
    let live = queue.pointer("/data/repository/pullRequest");
    let field = |name: &str| live.and_then(|pull| pull.get(name));
    let armed = !owned && field("autoMergeRequest").is_some_and(|request| !request.is_null());
    let queued = field("isInMergeQueue").and_then(Value::as_bool);
    let live_head = field("headRefOid").and_then(Value::as_str);

    // The arm: judged on the census head, failing closed.
    let disarm = if armed {
        if live_head.is_some_and(|live| live.eq_ignore_ascii_case(census_head)) {
            match verdict(actions, observation, pr, census_head, &queue, pass, ledger) {
                Verdict::Admitted => None,
                Verdict::Refused(reason)
                | Verdict::EjectionCapped(reason)
                | Verdict::Unreadable { why: reason, .. } => Some(reason),
            }
        } else {
            Some("head moved since the census".to_owned())
        }
    } else {
        None
    };

    // The entry: judged on the head actually queued, only on positive evidence.
    let (Some(queued), Some(entry_head)) = (queued, live_head) else {
        if !census_queued {
            return Plan {
                disarm,
                entry: EntryPlan::Keep,
                unreadable_head: None,
                judged: false,
            };
        }
        return unreadable(
            census_head,
            "the queue state named no head or queue membership".to_owned(),
            false,
            disarm,
        );
    };
    if !queued {
        return Plan {
            disarm,
            entry: EntryPlan::Keep,
            unreadable_head: None,
            judged: true,
        };
    }
    match verdict(actions, observation, pr, entry_head, &queue, pass, ledger) {
        Verdict::Admitted | Verdict::EjectionCapped(_) => Plan {
            disarm,
            entry: EntryPlan::Keep,
            unreadable_head: None,
            judged: true,
        },
        Verdict::Refused(reason) => Plan {
            disarm,
            entry: EntryPlan::Dequeue {
                head: entry_head.to_owned(),
                reason,
                streak: false,
            },
            unreadable_head: None,
            judged: true,
        },
        Verdict::Unreadable { why, systemic } => unreadable(entry_head, why, systemic, disarm),
    }
}

/// Remove `pr` from its merge queue under an already-held guard.
fn dequeue_under_guard(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    head: &str,
    ledger: &mut StewardLedger,
) -> Result<(), String> {
    if pr.node_id.is_empty() {
        return Err(format!("PR #{} node ID unavailable", pr.fact.number));
    }
    let args = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={DEQUEUE_MUTATION}"),
        "-F".to_owned(),
        format!("id={}", pr.node_id),
    ];
    let raw = context
        .actions
        .run_gh_internal_queue_mutation(&args)
        .map_err(|error| format!("PR #{} could not be dequeued: {error}", pr.fact.number))?;
    let accepted = serde_json::from_str::<Value>(&raw).is_ok_and(|value| {
        value.get("errors").is_none()
            && value
                .pointer("/data/dequeuePullRequest")
                .is_some_and(|result| !result.is_null())
    });
    if !accepted {
        return Err(format!(
            "PR #{} dequeue was not accepted: {}",
            pr.fact.number,
            first_graphql_error(&raw).unwrap_or_else(|| "no errors reported".to_owned())
        ));
    }
    record_audit(
        ledger,
        &context.observation.repo,
        &format!("pr:{}:{head}", pr.fact.number),
        DEQUEUE_AUDIT,
    );
    ledger
        .steward_removals
        .entry(head_key(&context.observation.repo, pr.fact.number, head))
        .or_default()
        .push(Utc::now().to_rfc3339());
    Ok(())
}

/// Advance or clear `pr`'s unreadable streak from `plan`. Only an applying
/// pass changes steward state.
fn record_streak(ledger: &mut StewardLedger, repo: &str, pr: &ObservedPr, plan: &Plan) {
    let prefix = format!("{repo}#{}:", pr.fact.number);
    match &plan.unreadable_head {
        Some(head) => {
            let key = head_key(repo, pr.fact.number, head);
            // A new head starts over: drop any other head's streak.
            ledger
                .dequeue_unreadable
                .retain(|other, _| !other.starts_with(&prefix) || *other == key);
            *ledger.dequeue_unreadable.entry(key).or_insert(0) += 1;
        }
        None if plan.judged => {
            ledger
                .dequeue_unreadable
                .retain(|other, _| !other.starts_with(&prefix));
        }
        None => {}
    }
}

/// Judge `pr` and, under `--apply`, withdraw its arm and queue entry when its
/// head is not admitted.
///
/// Mutations, comma-joined as the per-PR mutation string:
/// * `disarmed_stale_auto_merge:<head>` / `would_disarm_stale_auto_merge:<head>`
/// * `dequeued_unapproved:<head>` / `would_dequeue_unapproved:<head>`: on
///   positive evidence
/// * `dequeued_unreadable_streak:<head>` /
///   `would_dequeue_unreadable_streak:<head>`: on the consecutive-unreadable
///   bound
///
/// A deferral (`dequeue_deferred_unreadable`) also returns the underlying read
/// error as this PR's error, so the pass is reported unhealthy.
/// * `dequeue_deferred_unreadable:<head>`: unreadable, streak below the bound
/// * `dequeue_deferred_cap:<head>`: this pass's dequeue cap is spent
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // One pass's inputs and one guarded transaction.
pub(super) fn sweep(
    actions: &GitHubActions,
    observation: &RepoObservation,
    context: Option<&MutationApplyContext<'_>>,
    apply: bool,
    pr: &ObservedPr,
    decision: &StewardDecision,
    pass: &mut SweepPass,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let census_head = &pr.fact.head_sha;
    let plan = plan(
        actions,
        observation,
        pr,
        owns_for_disarm(decision),
        pass,
        ledger,
    );
    if apply {
        record_streak(ledger, &observation.repo, pr, &plan);
    }
    let mut reports = Vec::new();
    // A deferral is reported as this PR's error, so the pass is unhealthy and
    // an unreadable queue entry is visible rather than silently waiting.
    let mut deferred = None;
    let dequeue = match &plan.entry {
        EntryPlan::Keep => None,
        EntryPlan::Deferred { head, why } => {
            reports.push(format!("dequeue_deferred_unreadable:{head}"));
            deferred = Some(format!(
                "PR #{} queue entry at {head} not judged, dequeue deferred: {why}",
                pr.fact.number
            ));
            None
        }
        EntryPlan::Dequeue { head, .. } if pass.dequeues >= MAX_DEQUEUES_PER_PASS => {
            reports.push(format!("dequeue_deferred_cap:{head}"));
            None
        }
        EntryPlan::Dequeue {
            head,
            reason,
            streak,
        } => {
            pass.dequeues += 1;
            let label = if *streak {
                (
                    "dequeued_unreadable_streak",
                    "would_dequeue_unreadable_streak",
                )
            } else {
                ("dequeued_unapproved", "would_dequeue_unapproved")
            };
            Some((head.clone(), reason.clone(), label))
        }
    };
    // Report mode (the default): say what would be removed, remove nothing.
    let dequeue = match dequeue {
        Some((head, _, (_, planned))) if pass.mode == DequeueMode::Report => {
            reports.push(format!("{planned}:{head}"));
            None
        }
        dequeue => dequeue,
    };
    if plan.disarm.is_none() && dequeue.is_none() {
        return ((!reports.is_empty()).then(|| reports.join(",")), deferred);
    }
    if !apply {
        let planned = [
            plan.disarm
                .as_ref()
                .map(|_| format!("would_disarm_stale_auto_merge:{census_head}")),
            dequeue
                .as_ref()
                .map(|(head, _, (_, planned))| format!("{planned}:{head}")),
        ];
        reports.extend(planned.into_iter().flatten());
        return (Some(reports.join(",")), deferred);
    }
    let Some(context) = context else {
        return (
            None,
            Some("unapproved admission withdrawal requires mutation control".to_owned()),
        );
    };
    let guard = match acquire_pr_mutation_guard(
        context.mutation_control,
        context.observation,
        pr,
        "runner steward withdraw admission of an unapproved head",
    ) {
        Ok(guard) => guard,
        Err(error) => return (None, Some(error)),
    };
    let mut done = Vec::new();
    let mut error = None;
    // Disarm first, so a request left armed cannot re-enqueue the head the
    // dequeue removes.
    if let Some(reason) = &plan.disarm {
        if let Err(failure) = disarm_under_guard(context, pr, ledger, DISARM_AUDIT) {
            error = Some(format!("{failure} (disarming because {reason})"));
        } else {
            done.push(format!("disarmed_stale_auto_merge:{census_head}"));
        }
    }
    if let Some((head, reason, (label, _))) = &dequeue
        && error.is_none()
    {
        if let Err(failure) = dequeue_under_guard(context, pr, head, ledger) {
            error = Some(format!("{failure} (dequeuing because {reason})"));
        } else {
            ledger
                .dequeue_unreadable
                .remove(&head_key(&observation.repo, pr.fact.number, head));
            done.push(format!("{label}:{head}"));
        }
    }
    let outcome = if error.is_some() {
        "withdrawal_failed"
    } else {
        "unapproved_admission_withdrawn"
    };
    let error = match (error, deferred) {
        (Some(error), Some(deferred)) => Some(format!("{error}; {deferred}")),
        (error, deferred) => error.or(deferred),
    };
    done.extend(reports);
    let mutation = (!done.is_empty()).then(|| done.join(","));
    match (guard.finish(outcome), error) {
        (Ok(()), error) => (mutation, error),
        (Err(audit), None) => (mutation, Some(format!("mutation audit failed: {audit}"))),
        (Err(audit), Some(error)) => (
            mutation,
            Some(format!("{error}; mutation audit also failed: {audit}")),
        ),
    }
}
