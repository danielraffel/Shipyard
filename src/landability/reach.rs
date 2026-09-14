//! Trigger reachability: *will the required context ever be asked for?*
//!
//! [`super`] answers "can the required contexts be **scheduled**". That
//! presupposes a run will be **requested**, and on 2026-09-14 it was not: a
//! pull request opened against a feature base sat `CLEAN` with an empty check
//! rollup for 2 h 48 m because its gate declared `on.pull_request.branches:
//! [main]` and the base was not `main`. Nothing was broken. Nothing was queued.
//! Nothing said so.
//!
//! The two modules answer links of one chain, and an operator wants one
//! answer — *which link is broken, and whose fix is it*:
//!
//! ```text
//! (1) something REQUIRES C  ->  (2) a workflow W PRODUCES C under a PR-shaped event
//! -> (3) W's `on:` ADMITS this PR  ->  (4) W's jobs are SCHEDULABLE  -> (5) a run EXISTS
//! ```
//!
//! ## Why its own exit code
//!
//! A lane fault (link 4) is fixed on the **fleet**, by an operator with SSH
//! access, and `--allow-unserved-lane` exists so a human who just restored a
//! runner can ship before the census catches up. A trigger fault (links 1-3)
//! is fixed on **the pull request or the workflow file**, by its author, right
//! now. Those remedies are disjoint, and the lane bypass must not be able to
//! wave through a pull request whose gate will never be requested — so this
//! carries [`EXIT_TRIGGER_UNREACHABLE`](super::EXIT_TRIGGER_UNREACHABLE) and
//! its own equally narrow escape.
//!
//! ## Only one verdict is helped by waiting
//!
//! That is the entire product. Of the eight verdicts below, seven are
//! permanent until somebody changes something, and the tool says which
//! somebody. The 2 h 48 m was spent on a state that was never going to change.
//!
//! ## What it never does
//!
//! It never pushes, dispatches, re-dispatches or re-targets. Decisions
//! contract row `[default] #4` is about exactly this, and an empty commit is a
//! dispatch by another name: it fires `synchronize`. Four blind re-dispatches
//! made the predecessor incident materially worse. For the retarget hole the
//! tool prints the exact command and a human runs it.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::trigger::{Admit, EventFilter, TriggerUnknown, Triggers};
use super::workflow::WorkflowJob;

/// One required context and where the requirement came from.
#[derive(Clone, Debug, Serialize)]
pub struct RequiredContext {
    /// The context name as branch protection or the producing job renders it.
    pub name: String,
    /// Branch protection (or a ruleset) on the base requires it.
    pub protected: bool,
    /// The Shipyard config table that also requires it, when one does —
    /// `[governance] required_status_checks` or `[merge] require_platforms`.
    pub shipyard_source: Option<String>,
}

/// One workflow file, as read from the checkout.
#[derive(Clone, Debug)]
pub struct WorkflowUnderTest {
    /// Repository-relative path, e.g. `.github/workflows/build.yml`.
    pub path: String,
    /// Jobs parsed by [`super::workflow::parse_workflow_jobs`].
    pub jobs: Vec<WorkflowJob>,
    /// Triggers as they read on the **head** ref — the copy GitHub uses for
    /// `pull_request`.
    pub triggers: Result<Triggers, TriggerUnknown>,
    /// Triggers as they read on the **base** ref, when the two differ.
    ///
    /// `pull_request_target` and `merge_group` run the base's copy. A pull
    /// request that *edits its own trigger* is precisely the one whose
    /// reachability is in doubt, so the difference is reported rather than
    /// resolved silently.
    pub base_triggers: Option<Result<Triggers, TriggerUnknown>>,
}

/// A workflow run observed on the pull request's head SHA.
///
/// Deliberately carries **no** `pull_requests[]` association. The one genuine
/// `pull_request` run on the incident's pull request came back with
/// `pull_requests: []`, so a detector keyed on that array reports *no run* for
/// a pull request whose run exists. Pulp's own `vellum-freeze-recovery.yml`
/// has that bug. The key here is `head_sha` + `event`, and nothing else.
#[derive(Clone, Debug, Serialize)]
pub struct HeadRun {
    /// `.github/workflows/…` path the run belongs to.
    pub workflow_path: String,
    /// The event that created it: `pull_request`, `workflow_dispatch`, …
    pub event: String,
    /// When GitHub created the run.
    pub created_at: DateTime<Utc>,
    /// Run id, for the operator to open.
    pub id: u64,
}

/// Post-open facts about one pull request. Absent at `shipyard pr` time.
#[derive(Clone, Debug, Serialize)]
pub struct HeadEvidence {
    /// Pull request number.
    pub number: u64,
    /// Current head SHA every run is matched against.
    pub head_sha: String,
    /// Every run on that SHA, repo-wide, one API call.
    pub runs: Vec<HeadRun>,
    /// When the most recent `base_ref_changed` fired, if the timeline was read.
    pub base_ref_changed_at: Option<DateTime<Utc>>,
    /// The base the pull request was retargeted *from*.
    pub base_ref_changed_from: Option<String>,
    /// Whether the timeline was actually read. A `None` above means "not read"
    /// only when this is `false`; distinguishing the two is what keeps an
    /// unread timeline from reading as "no retarget happened".
    pub timeline_read: bool,
}

/// How a [`Reachability::Triggered`] verdict was reached.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerEvidence {
    /// Links 1-3 hold and no run evidence was read (the `shipyard pr` path,
    /// where the pull request does not exist yet). A claim about the future,
    /// and labelled as one.
    StaticallyAdmitted,
    /// Links 1-3 hold and no run exists on the head yet, but nothing prevents
    /// one. This is the only state where waiting is the right move, and link 4
    /// (the lane verdict) says whether waiting will terminate.
    AdmittedAwaitingRun,
    /// A run of the producing workflow exists on this head under a
    /// PR-shaped event.
    Run {
        /// The event that created it.
        event: String,
        /// Run id.
        id: u64,
    },
}

/// Which link of the chain is the first that cannot hold.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Reachability {
    /// R7 — nothing in links 1-3 stops this context being requested.
    Triggered {
        /// How that was decided.
        evidence: TriggerEvidence,
    },
    /// R0 — Shipyard requires this context and branch protection does not.
    ///
    /// Not a refusal: nothing is broken on the pull request. It is a statement
    /// about the *repository*, and the one that matters is the second half —
    /// with nothing required, auto-merge merges with nothing run.
    NotRequired,
    /// R1 — no workflow in the checkout renders a job to this name under a
    /// PR-shaped event.
    NoProducer,
    /// R6 — runs exist on this head, and every one of them is the wrong kind
    /// of run to satisfy a pull-request requirement.
    WrongEvidence {
        /// The events actually observed.
        events: Vec<String>,
    },
    /// R5 — the base is admitted *now*, the pull request was retargeted, and
    /// the producing workflow does not declare `edited`, so nothing re-fired.
    Retargeted {
        /// The base it was retargeted away from, when the timeline named one.
        from: Option<String>,
    },
    /// R3 — every changed file is excluded by the workflow's path filter.
    PathsExcluded {
        /// Under branch protection a skipped required check stays *Pending*
        /// forever and blocks the pull request; unprotected it is an intended
        /// skip. Same clause, opposite sign, and the operator needs to know
        /// which one they are looking at.
        protected: bool,
    },
    /// R2 — the workflow's branch filter does not admit this pull request's
    /// base.
    BaseExcluded,
    /// R4 — the producing workflow declares no event that can put a check on
    /// a pull request.
    EventExcluded,
    /// The instrument could not decide. Never a pass, never alone a refusal.
    Unknown {
        /// Where the reader stopped.
        boundary: String,
        /// What it could not read.
        detail: String,
    },
}

impl Reachability {
    /// Snake-case name used in JSON and human output.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Triggered { .. } => "triggered",
            Self::NotRequired => "not_required",
            Self::NoProducer => "no_producer",
            Self::WrongEvidence { .. } => "wrong_evidence",
            Self::Retargeted { .. } => "retargeted",
            Self::PathsExcluded { .. } => "paths_excluded",
            Self::BaseExcluded => "base_excluded",
            Self::EventExcluded => "event_excluded",
            Self::Unknown { .. } => "unknown",
        }
    }

    /// Severity for the one-line summary.
    ///
    /// Ordered so that, among the verdicts that block, the **earliest broken
    /// link** ranks worst: fixing a later link changes nothing while an
    /// earlier one is broken.
    #[must_use]
    pub fn severity(&self) -> u8 {
        match self {
            Self::Triggered { .. } => 0,
            Self::NotRequired => 1,
            Self::NoProducer => 2,
            Self::Unknown { .. } => 3,
            Self::PathsExcluded { protected: false } => 4,
            // Blocking, ordered by link index: evidence (5), then filters (3),
            // then the event itself (2).
            Self::WrongEvidence { .. } => 5,
            Self::Retargeted { .. } => 6,
            Self::PathsExcluded { protected: true } => 7,
            Self::BaseExcluded => 8,
            Self::EventExcluded => 9,
        }
    }

    /// Whether this verdict refuses the submission.
    ///
    /// `NotRequired`, `NoProducer`, an intended path skip and `Unknown` all
    /// warn instead: the first two are statements about configuration the
    /// author may have made on purpose, the third is a designed skip, and the
    /// fourth is a statement about the instrument. An instrument that cannot
    /// see must not be able to stop the fleet — and must equally never fold
    /// its own blindness into a pass, which is why it prints either way.
    #[must_use]
    pub fn blocks(&self) -> bool {
        self.severity() >= 5
    }

    /// Whether waiting can change this verdict.
    #[must_use]
    pub fn waiting_helps(&self) -> bool {
        matches!(
            self,
            Self::Triggered {
                evidence: TriggerEvidence::AdmittedAwaitingRun
            }
        )
    }
}

/// One required context's reachability, with the clause that decided it.
#[derive(Clone, Debug, Serialize)]
pub struct ContextReachability {
    /// Required context name.
    pub context: String,
    /// Producing workflow path, when one was found.
    pub workflow: Option<String>,
    /// The verdict.
    pub verdict: Reachability,
    /// The clause as written in the workflow file, e.g.
    /// `on.pull_request.branches: [main]`. Empty when no clause decided it.
    pub clause: Option<String>,
    /// Sentences the operator should act on. The tool performs none of them.
    pub remedies: Vec<String>,
    /// Statements worth printing that are not the verdict — the unprotected
    /// base, a missing `edited`, a head/base trigger divergence.
    pub notes: Vec<String>,
    /// The operator waived this workflow with `--allow-unreachable-trigger`.
    pub waived: bool,
}

/// Everything [`assess_reachability`] needs. All of it is local except
/// `evidence`, which is `None` until a pull request exists.
#[derive(Clone, Copy, Debug)]
pub struct ReachInput<'a> {
    /// Required contexts and where each requirement came from.
    pub contexts: &'a [RequiredContext],
    /// Workflows named by `[landability] workflows`.
    pub workflows: &'a [WorkflowUnderTest],
    /// The pull request's base branch.
    pub base: &'a str,
    /// Three-dot diff against the base, as GitHub evaluates path filters.
    pub changed_paths: &'a [String],
    /// Whether the protection read succeeded. An *unreadable* protection and
    /// an *absent* one are different facts and only one of them is R0.
    pub protection_readable: bool,
    /// Post-open evidence, when a pull request exists.
    pub evidence: Option<&'a HeadEvidence>,
    /// Workflow paths (or basenames) the operator waived for this run.
    pub allow_unreachable: &'a [String],
}

/// Classify every required context.
#[must_use]
pub fn assess_reachability(input: &ReachInput<'_>) -> Vec<ContextReachability> {
    input
        .contexts
        .iter()
        .map(|context| assess_one(context, input))
        .collect()
}

fn assess_one(context: &RequiredContext, input: &ReachInput<'_>) -> ContextReachability {
    let producers: Vec<&WorkflowUnderTest> = input
        .workflows
        .iter()
        .filter(|workflow| workflow.jobs.iter().any(|job| job.produces(&context.name)))
        .collect();

    let mut notes = Vec::new();
    if !context.protected
        && input.protection_readable
        && let Some(source) = &context.shipyard_source
    {
        notes.push(format!(
            "branch protection on `{}` does not require this context; only Shipyard's {source} \
             does. Auto-merge on this repository would merge with nothing run.",
            input.base
        ));
    }

    if producers.is_empty() {
        let mut assessment = ContextReachability {
            context: context.name.clone(),
            workflow: None,
            verdict: Reachability::NoProducer,
            clause: None,
            remedies: vec![
                "list the producing workflow in `[landability] workflows`, or".to_owned(),
                "accept that this context comes from outside the checkout (an external App or a \
                 workflow in another repository) and was NOT checked"
                    .to_owned(),
            ],
            notes,
            waived: false,
        };
        // R0 outranks R1 only when there is genuinely nothing to produce and
        // nothing to require: an unprotected context with no producer is a
        // configuration statement, not a broken trigger.
        if !context.protected && input.protection_readable && context.shipyard_source.is_some() {
            assessment.verdict = Reachability::NotRequired;
            assessment.remedies = not_required_remedies(input.base);
        }
        return assessment;
    }

    // A context may legitimately be produced by more than one workflow; it is
    // reachable if ANY of them admits this pull request, so the best verdict
    // wins and the worst is kept as a note.
    let mut assessments: Vec<ContextReachability> = producers
        .iter()
        .map(|workflow| assess_producer(context, workflow, input))
        .collect();
    assessments.sort_by_key(|assessment| assessment.verdict.severity());
    let mut best = assessments.remove(0);
    for other in assessments {
        best.notes.push(format!(
            "also produced by {} ({})",
            other.workflow.as_deref().unwrap_or("?"),
            other.verdict.as_str()
        ));
    }
    best.notes.splice(0..0, notes);

    if !context.protected
        && input.protection_readable
        && context.shipyard_source.is_some()
        && matches!(best.verdict, Reachability::Triggered { .. })
    {
        // Promote to R0 only when the trigger link has nothing to report. A
        // specific clause always outranks it: "your path filter excluded every
        // changed file" tells the author what to look at, while "nothing
        // requires this" does not — and the unprotected fact survives as a
        // note and a warning on the same block either way.
        best.verdict = Reachability::NotRequired;
        best.remedies = not_required_remedies(input.base);
    }
    best
}

fn not_required_remedies(base: &str) -> Vec<String> {
    vec![
        format!("protect `{base}` (or add a ruleset) requiring this context, and"),
        "add a companion workflow posting the same check name with the NEGATED path filter, so a \
         docs-only pull request is green rather than Pending forever (GitHub: do not path-filter \
         a required workflow)"
            .to_owned(),
        "until then, treat a CLEAN mergeability with an empty check rollup as literally true: \
         nothing requires anything on this branch"
            .to_owned(),
    ]
}

/// Events that can put a check on a pull request's merge ref.
fn is_pull_request_shaped(event: &str) -> bool {
    matches!(
        event,
        "pull_request" | "pull_request_target" | "merge_group"
    )
}

#[allow(clippy::too_many_lines)]
fn assess_producer(
    context: &RequiredContext,
    workflow: &WorkflowUnderTest,
    input: &ReachInput<'_>,
) -> ContextReachability {
    let waived = input.allow_unreachable.iter().any(|entry| {
        workflow.path == *entry
            || workflow
                .path
                .rsplit('/')
                .next()
                .is_some_and(|base| base == entry)
    });
    let mut out = ContextReachability {
        context: context.name.clone(),
        workflow: Some(workflow.path.clone()),
        verdict: Reachability::Triggered {
            evidence: TriggerEvidence::StaticallyAdmitted,
        },
        clause: None,
        remedies: Vec::new(),
        notes: Vec::new(),
        waived,
    };

    let triggers = match &workflow.triggers {
        Ok(triggers) => triggers,
        Err(unknown) => {
            out.verdict = Reachability::Unknown {
                boundary: unknown.boundary.clone(),
                detail: format!("{}: {unknown}", workflow.path),
            };
            out.remedies.push(
                "the `on:` block was refused rather than partially read; fix the form it names, \
                 or accept that this context's trigger was NOT checked"
                    .to_owned(),
            );
            return out;
        }
    };

    if let Some(Ok(base_triggers)) = &workflow.base_triggers
        && base_triggers != triggers
    {
        out.notes.push(format!(
            "{} declares different triggers on `origin/{}` than on this head; `pull_request` uses \
             the head's copy, `pull_request_target` and `merge_group` use the base's",
            workflow.path, input.base
        ));
    }

    // A run that exists is a FACT; every clause below is a prediction about
    // whether one would be created. So the evidence is consulted first: a
    // `pull_request` run on this head settles the question no matter what the
    // local checkout's diff looks like — and when `--pr N` is run from a
    // checkout sitting on some other branch, that diff is not this pull
    // request's diff at all.
    if let Some(run) = input.evidence.and_then(|evidence| {
        evidence
            .runs
            .iter()
            .find(|run| run.workflow_path == workflow.path && is_pull_request_shaped(&run.event))
    }) {
        out.verdict = Reachability::Triggered {
            evidence: TriggerEvidence::Run {
                event: run.event.clone(),
                id: run.id,
            },
        };
        out.remedies.push(format!(
            "the run exists ({} {}); whether it can be SCHEDULED is the lane verdict, and whether \
             it is green is its outcome",
            run.event, run.id
        ));
        return out;
    }

    if !triggers.has_pull_request_shaped_event() {
        out.verdict = Reachability::EventExcluded;
        out.clause = Some(if triggers.events.is_empty() {
            "no `on:` block found".to_owned()
        } else {
            format!("on: [{}]", triggers.events.join(", "))
        });
        out.remedies.push(format!(
            "add a `pull_request` trigger to {}, or the context is produced by a different \
             workflow than the one configured",
            workflow.path
        ));
        return out;
    }

    // `pull_request` is the event that puts a check on the pull request's
    // merge ref. `pull_request_target` is the fallback for a workflow that
    // only declares that (it runs the base's copy). A `merge_group`-only
    // workflow never reports on the pull request itself.
    let Some((event_name, filter)) = triggers
        .pull_request
        .as_ref()
        .map(|filter| ("pull_request", filter))
        .or_else(|| {
            triggers
                .pull_request_target
                .as_ref()
                .map(|filter| ("pull_request_target", filter))
        })
    else {
        out.verdict = Reachability::EventExcluded;
        out.clause = Some("on: merge_group (only)".to_owned());
        out.remedies.push(format!(
            "{} reports only on queued merge groups, so this context never appears on the pull \
             request itself; add a `pull_request` trigger",
            workflow.path
        ));
        return out;
    };

    if !filter.has_type("edited") {
        out.notes.push(format!(
            "{} does not declare `edited` in `on.{event_name}.types`, so retargeting this pull \
             request will NOT re-fire this gate",
            workflow.path
        ));
    }

    match filter.admits_base(input.base) {
        Admit::No { clause, detail } => {
            out.verdict = Reachability::BaseExcluded;
            out.clause = Some(format!("on.{event_name}.{clause}  {detail}"));
            out.remedies = base_excluded_remedies(workflow, event_name, input);
            return finish(out, input, workflow, filter, event_name);
        }
        Admit::Unknown(unknown) => {
            out.verdict = Reachability::Unknown {
                boundary: unknown.boundary.clone(),
                detail: format!("{}: {unknown}", workflow.path),
            };
            return out;
        }
        Admit::Yes => {}
    }

    match filter.admits_paths(input.changed_paths) {
        Admit::No { clause, detail } => {
            out.verdict = Reachability::PathsExcluded {
                protected: context.protected,
            };
            out.clause = Some(format!("on.{event_name}.{clause}  {detail}"));
            out.remedies = if context.protected {
                vec![
                    format!(
                        "{} is a REQUIRED check with a path filter. GitHub leaves a skipped \
                         required check Pending forever, so this pull request can never merge.",
                        workflow.path
                    ),
                    "add a companion workflow posting the same check name with the NEGATED filter, \
                     or"
                        .to_owned(),
                    "move the filter into a job-level `if:` — a job skipped by `if:` reports \
                     Success, a workflow skipped by a path filter does not"
                        .to_owned(),
                ]
            } else {
                vec![format!(
                    "this is an intended skip: {} declares the filter and nothing requires the \
                     result, so no check will appear and none is expected",
                    workflow.path
                )]
            };
            return out;
        }
        Admit::Unknown(unknown) => {
            out.verdict = Reachability::Unknown {
                boundary: unknown.boundary.clone(),
                detail: format!("{}: {unknown}", workflow.path),
            };
            return out;
        }
        Admit::Yes => {}
    }

    finish(out, input, workflow, filter, event_name)
}

/// Fold post-open run evidence into a statically-decided verdict.
fn finish(
    mut out: ContextReachability,
    input: &ReachInput<'_>,
    workflow: &WorkflowUnderTest,
    filter: &EventFilter,
    event_name: &str,
) -> ContextReachability {
    let Some(evidence) = input.evidence else {
        return out;
    };

    let admitted_now = !out.verdict.blocks();
    let runs: Vec<&HeadRun> = evidence
        .runs
        .iter()
        .filter(|run| run.workflow_path == workflow.path)
        .collect();
    debug_assert!(
        !runs.iter().any(|run| is_pull_request_shaped(&run.event)),
        "a pull-request-shaped run is decided before the static clauses"
    );

    if !runs.is_empty() {
        // The `workflow_dispatch` trap. A dispatch checks out the branch tip,
        // not `refs/pull/N/merge`, so under a strict protection policy it is
        // the wrong proof even where GitHub accepts the check — and Shipyard's
        // own cloud backend is itself a dispatch source on some repositories,
        // so its runs must never be read as the gate.
        out.verdict = Reachability::WrongEvidence {
            events: {
                let mut events: Vec<String> = runs.iter().map(|run| run.event.clone()).collect();
                events.sort_unstable();
                events.dedup();
                events
            },
        };
        out.clause = Some(format!(
            "runs on {} exist, none created by a pull-request event",
            &evidence.head_sha[..evidence.head_sha.len().min(8)]
        ));
        out.remedies = vec![
            "a workflow_dispatch run checks out the branch tip, not the pull request's merge ref; \
             it is not the run the requirement was written for and must not be counted"
                .to_owned(),
            "to obtain a real run, push a commit (fires `synchronize`) — never dispatch".to_owned(),
        ];
        return out;
    }

    // No runs at all on this head.
    if admitted_now
        && evidence.timeline_read
        && evidence.base_ref_changed_at.is_some()
        && !filter.has_type("edited")
    {
        out.verdict = Reachability::Retargeted {
            from: evidence.base_ref_changed_from.clone(),
        };
        out.clause = Some(format!(
            "base admitted now; `base_ref_changed`{} and `edited` is not in on.{event_name}.types",
            evidence
                .base_ref_changed_at
                .map(|at| format!(" at {}", at.to_rfc3339()))
                .unwrap_or_default()
        ));
        out.remedies = retarget_remedies(workflow, event_name);
        return out;
    }

    if admitted_now {
        out.verdict = Reachability::Triggered {
            evidence: TriggerEvidence::AdmittedAwaitingRun,
        };
        out.remedies.push(format!(
            "nothing in the trigger stops this; no run exists on {} yet. This is the one verdict \
             waiting can resolve — the lane verdict says whether it terminates.",
            &evidence.head_sha[..evidence.head_sha.len().min(8)]
        ));
    }
    out
}

fn base_excluded_remedies(
    workflow: &WorkflowUnderTest,
    event_name: &str,
    input: &ReachInput<'_>,
) -> Vec<String> {
    let mut remedies = vec![format!(
        "open the pull request against a base that {} admits, or",
        workflow.path
    )];
    if input.evidence.is_some() {
        remedies.push(
            "if it has already been retargeted, push a commit to fire `synchronize` (run this \
             yourself, under your own identity — an App push may not fire `pull_request`):"
                .to_owned(),
        );
        remedies.push(
            "  git commit --allow-empty -m \"ci: re-fire pull_request after retarget\" && git push"
                .to_owned(),
        );
    }
    remedies.push(format!(
        "permanent fix in {}: add `edited` to on.{event_name}.types with the documented guard \
         `if: github.event.action != 'edited' || github.event.changes.base.ref.from != null`",
        workflow.path
    ));
    remedies
}

fn retarget_remedies(workflow: &WorkflowUnderTest, event_name: &str) -> Vec<String> {
    vec![
        "to fire it now (run this yourself, under your own identity — an App push may not fire \
         `pull_request`):"
            .to_owned(),
        "  git commit --allow-empty -m \"ci: re-fire pull_request after retarget\" && git push"
            .to_owned(),
        format!(
            "permanent fix in {}: on.{event_name}.types: [opened, synchronize, reopened, edited] \
             with `if: github.event.action != 'edited' || github.event.changes.base.ref.from != \
             null`",
            workflow.path
        ),
        "under `pull_request` the job must read the new base from \
         `github.event.pull_request.base.ref`, NOT `github.base_ref`, which is fixed at run \
         creation"
            .to_owned(),
    ]
}

/// Render the operator-facing refusal for every blocking, unwaived context.
///
/// This text is the product. Each block names the context, the workflow, the
/// clause **as written in the file**, and the remedies — and states that the
/// tool performed none of them.
#[must_use]
pub fn render_refusal(assessments: &[ContextReachability]) -> String {
    let mut out = String::new();
    for assessment in assessments
        .iter()
        .filter(|assessment| assessment.verdict.blocks() && !assessment.waived)
    {
        let _ = writeln!(
            out,
            "landability: required context `{}` will not be requested ({})",
            assessment.context,
            assessment.verdict.as_str()
        );
        if let Some(workflow) = &assessment.workflow {
            let _ = writeln!(out, "  workflow {workflow}");
        }
        if let Some(clause) = &assessment.clause {
            let _ = writeln!(out, "    {clause}");
        }
        for note in &assessment.notes {
            let _ = writeln!(out, "    note: {note}");
        }
        out.push_str("  remedies (this tool performs none):\n");
        for remedy in &assessment.remedies {
            let _ = writeln!(out, "    - {remedy}");
        }
    }
    if !out.is_empty() {
        out.push_str("  contract [default] #4: nothing re-dispatched.\n");
        out.push_str(
            "  override: --allow-unreachable-trigger <workflow> proceeds and prints this as a \
             warning. --allow-unserved-lane does NOT cover this: a lane fault is fixed on the \
             fleet, a trigger fault on the pull request or the workflow file.\n",
        );
    }
    out
}

/// Non-blocking statements worth printing on every ship.
#[must_use]
pub fn warnings(assessments: &[ContextReachability]) -> Vec<String> {
    let mut warnings = Vec::new();
    for assessment in assessments {
        if assessment.waived && assessment.verdict.blocks() {
            warnings.push(format!(
                "landability: trigger fault for `{}` WAIVED by --allow-unreachable-trigger ({}); \
                 the gate will not be requested",
                assessment.context,
                assessment.verdict.as_str()
            ));
        }
        match &assessment.verdict {
            Reachability::NotRequired => warnings.push(format!(
                "landability: `{}` is required by Shipyard and NOT by branch protection - \
                 auto-merge would merge with nothing run",
                assessment.context
            )),
            Reachability::Unknown { boundary, detail } => warnings.push(format!(
                "landability: trigger for `{}` is UNKNOWN [{boundary}] - {detail}",
                assessment.context
            )),
            Reachability::PathsExcluded { protected: false } => warnings.push(format!(
                "landability: `{}` is path-filtered out of this pull request and nothing requires \
                 it - an intended skip, not a missing check",
                assessment.context
            )),
            Reachability::NoProducer => warnings.push(format!(
                "landability: `{}` has no producing workflow among those configured - its trigger \
                 was NOT checked",
                assessment.context
            )),
            _ => {}
        }
        for note in &assessment.notes {
            if note.contains("does not declare `edited`") || note.contains("does not require") {
                warnings.push(format!("landability: `{}` - {note}", assessment.context));
            }
        }
    }
    warnings
}

/// Worst verdict across every context, for the one-line summary.
#[must_use]
pub fn worst(assessments: &[ContextReachability]) -> Option<&ContextReachability> {
    assessments
        .iter()
        .max_by_key(|assessment| assessment.verdict.severity())
}

#[cfg(test)]
mod tests;
