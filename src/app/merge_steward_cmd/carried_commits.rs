//! Refuse to admit a head that carries commits a newer HOLD covers.
//!
//! A HOLD stops one pull request's head. It does not stop the same commits
//! reaching the base through a second pull request: a branch that merged the
//! held branch (or a branch that did) carries the held commits, a reviewer can
//! approve the second pull request on its own diff, and the steward would then
//! admit the held code. Before admitting candidate P, the steward therefore
//! compares P's commits that are not on the base with the commits of every
//! other open pull request Q in the repository. When P carries a commit of a
//! Q whose current head the exact-head approval gate does not admit, P is
//! refused with `carries_unapproved_commits:#Q`, naming the shared commits,
//! unless the HOLD-recency rule clears Q ([`recency_judgement`]).
//!
//! The rule: P's exact-head APPROVE reviewed P's whole diff, the shared
//! commits included, so a shared commit holds P back only while some counting
//! HOLD on any head of Q's, or of P's own, is at least as new as that APPROVE.
//! Times are the server's (a review's `submitted_at`, a comment's
//! `created_at`, and its `updated_at` when later), read from the same verdict
//! records the gate reads. Every HOLD on Q counts, not only those on heads
//! holding the shared commits: a superset that refuses more, never less. A
//! child waiting for review therefore never blocks its approved parent, a
//! parent with several children or a child built on several parents is
//! settled pair by pair, and a HOLD posted after P's approval stops P until P
//! is reviewed again. A P with no exact-head APPROVE of its own (an approval
//! carried from an earlier head, or a rollup) is held by any HOLD. A verdict
//! that cannot be read refuses (`carried_commits_unreadable:#Q`), and under
//! fleet-guard admission, which keeps no verdict timeline here, a refused Q
//! always refuses. Labels (`stacked:<n>`), opening order, `Supersedes #n`, the
//! clean-replacement marker and head ancestry never clear a HOLD: none of
//! them says whether anyone reviewed the shared commits after the HOLD.
//!
//! Not blocked, before any verdict is read:
//!
//! * Q is P's own base-branch pull request (same repository, Q's head branch
//!   is P's base); a properly stacked P never targets the steward's base.
//! * Q is stacked on P, directly or transitively (following Q's base branch
//!   through other open pull requests' head branches reaches P's head
//!   branch): Q builds on P and cannot land before it.
//! * Q only inherits the commit: Q "introduced" a commit only if it is in Q's
//!   own diff against Q's own base branch (`Q.base...Q.head`), not merely
//!   reachable from Q. A pull request stacked on another branch therefore
//!   never matches the commits its base branch already carries. A pull
//!   request that merges another pull request's branch still has those
//!   commits in its own diff and is still caught.
//! * A carried commit whose change is already on the base under other shas
//!   (a merge that only re-applies landed content, a cherry-pick with the
//!   base's patch-id and matching changed entries): see [`landed`]. The
//!   steward proves it against the base's pinned tip; any doubt or read error
//!   keeps the refusal.
//! * Commits already on the base: P's set comes from the compare API
//!   `base...head`, which lists only commits the base does not have.
//! * A base whose policy does not require head approval: there every head is
//!   admissible, so no commit is unapproved.
//!
//! Every steward admission path runs it through [`refusal_for`]: the managed
//! enqueue (including the hosted-setup priority re-enqueue and the one
//! not-head's-fault same-head retry) and the exact-head merge, both via
//! [`admission_refusal`] in `mutate_pr_with_recovery`, and the
//! `--arm-unqueued` native-arm backstop, which refuses the arm with an
//! [`crate::auto_arm::ArmSkip::CarriedCommits`] skip and gives the pull
//! request's report row the same `blocked_on` token.
//!
//! Cost is bounded per pass. Open pull requests and their commits are read
//! once per pass per repository (one paged GraphQL listing, cached in the
//! mutation context), Q's approval is read whenever Q overlaps a candidate,
//! and P's own set costs one compare call. At most
//! [`MAX_COMMITS_PER_PR`] commits are read for any pull request. A Q whose
//! repository-wide listing has more is not yet unbounded: its own diff
//! (`Q.base...Q.head`, paged) is read, and the cap applies to that. A Q whose
//! own diff is still over the cap is judged, and an approved one never
//! refuses; for an unapproved one each of P's commits (at most
//! [`MAX_PROBED_COMMITS`]) is probed for reachability from Q's head and
//! absence from Q's base, so an over-cap Q that carries none of P's commits
//! never blocks P. When P has more commits than that, P's commits not on the
//! base are read once with their parents and Q's head is probed for each of
//! their roots (at most [`MAX_PROBED_COMMITS`]): a head that reaches none of
//! the roots reaches none of P's commits, which clears Q whatever its size
//! (see `candidate_roots_unreached`). Only when neither settles it does P
//! fail closed (`carried_commits_over_cap:#Q`). A candidate P over the cap itself is
//! checked the other way ([`over_cap_refusal`]): each other open Q's own
//! commits not on the base are read with their parents, and P's head is probed
//! for reaching the roots of that set, so a large P (a mirror-history import)
//! carrying no unapproved Q's commits is admitted, while one that does, or an
//! unapproved Q too large to settle, still refuses. An unreadable listing,
//! compare, probe or approval also fails closed
//! (`carried_commits_unreadable`). Q's approval is judged only while its live
//! head is still the head the listing saw; when an overlap with Q survives, a
//! push after the listing fails closed (`carried_commits_unreadable:#Q`) until
//! the next pass. A Q cleared by a probe of its listed head is not held against
//! P even if Q has since moved: the held content is the listed head's, and the
//! next pass re-lists Q.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::MutationApplyContext;
use super::ObservedPr;
use crate::head_approval::RunGh;

/// Most commits read for any one pull request. GitHub's compare and pull
/// request commit listings both stop at 250.
pub(super) const MAX_COMMITS_PER_PR: usize = 250;

/// Most of P's commits probed one by one against an over-cap Q's head.
pub(super) const MAX_PROBED_COMMITS: usize = 50;

/// Most reachability probes one over-cap candidate makes across every other
/// open pull request.
pub(super) const MAX_REVERSE_PROBES: usize = 100;

/// Commits per compare page.
const COMPARE_PAGE: usize = 100;

/// Commits per pull request in the listing page; more are read through REST.
const LISTING_COMMITS: usize = 100;

/// Open pull requests per listing page.
const LISTING_PAGE: usize = 50;

/// Listing pages read per pass before failing closed.
const MAX_LISTING_PAGES: usize = 20;

/// Shared shas named in one detail line.
const SHAS_NAMED: usize = 10;

/// Every open pull request with its first commits, paged.
const OPEN_PULL_COMMITS_QUERY: &str = "query($owner:String!,$name:String!,$cursor:String){\
     repository(owner:$owner,name:$name){pullRequests(states:OPEN,first:50,after:$cursor){\
     pageInfo{hasNextPage endCursor} nodes{number headRefName headRefOid baseRefName \
     isCrossRepository author{login} labels(first:50){nodes{name}} \
     carriedCommits:commits(first:100){totalCount nodes{commit{oid}}}}}}}";

/// The commits a pull request carries, as far as the steward read them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CommitList {
    /// Every commit.
    Complete(BTreeSet<String>),
    /// More commits than [`MAX_COMMITS_PER_PR`].
    OverCap {
        /// GitHub's count.
        total: usize,
    },
}

/// One open pull request in the repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct OpenPr {
    pub number: u64,
    pub head_ref: String,
    pub base_ref: String,
    pub head_sha: String,
    pub cross_repository: bool,
    pub author: Option<String>,
    pub commits: CommitList,
    /// The parent pull request a `stacked:<n>` label names, the label
    /// `shipyard pr --stacked-on <n>` applies.
    pub stacked_on: Option<u64>,
}

/// The candidate under admission.
#[derive(Clone, Debug)]
pub(super) struct Candidate<'a> {
    pub number: u64,
    /// The steward's base: P's base branch.
    pub base_ref: &'a str,
    /// P's head branch.
    pub head_ref: &'a str,
    /// P's commits not on the base.
    pub commits: &'a BTreeSet<String>,
}

/// Another open pull request whose commits P may carry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Carried {
    /// P carries these commits of Q.
    Shares {
        number: u64,
        head_sha: String,
        shared: Vec<String>,
    },
    /// Q has too many commits to rule an overlap out.
    Unbounded { number: u64, total: usize },
}

impl Carried {
    const fn number(&self) -> u64 {
        match self {
            Self::Shares { number, .. } | Self::Unbounded { number, .. } => *number,
        }
    }
}

/// Whether Q's commits may land through P: the exact-head approval gate's
/// verdict on Q's current head, then the HOLD-recency rule
/// ([`recency_judgement`]).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Judgement {
    /// The gate admits Q's head.
    Approved,
    /// The gate does not admit Q's head, but no counting HOLD on any head of
    /// Q's or P's is as new as P's exact-head approval, which reviewed every
    /// commit P carries: Q holds nothing back from P.
    Cleared(String),
    /// The gate refuses Q's head, for this reason.
    Unapproved(String),
    /// A counting HOLD names Q's head: refused, and never excused as a stack.
    Held(String),
    /// Q's approval could not be read.
    Unreadable(String),
}

impl Judgement {
    /// Whether Q's commits may land through P.
    pub(super) const fn clears(&self) -> bool {
        matches!(self, Self::Approved | Self::Cleared(_))
    }
}

/// Why P is refused: the `blocked_on` token and its detail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Refusal {
    pub token: String,
    pub detail: String,
}

/// Which other open pull requests P carries commits of. Pure.
#[must_use]
pub(super) fn carried(candidate: &Candidate<'_>, others: &[OpenPr]) -> Vec<Carried> {
    unrelated(
        candidate.number,
        candidate.base_ref,
        candidate.head_ref,
        others,
    )
    .filter_map(|q| match &q.commits {
        CommitList::Complete(commits) => {
            let shared: Vec<String> = candidate.commits.intersection(commits).cloned().collect();
            (!shared.is_empty()).then(|| Carried::Shares {
                number: q.number,
                head_sha: q.head_sha.clone(),
                shared,
            })
        }
        CommitList::OverCap { total } => Some(Carried::Unbounded {
            number: q.number,
            total: *total,
        }),
    })
    .collect()
}

/// The open pull requests whose commits P must not carry unapproved: every
/// one but P itself, P's own base-branch pull request, and those stacked on
/// P. Pure. A labelled stack is still checked here when it is unrelated to P;
/// the label is a repair-lane marker and never excuses Q. A Q stacked on P is
/// excluded by the base-branch relation itself, regardless of labels.
fn unrelated<'a>(
    number: u64,
    base_ref: &'a str,
    head_ref: &'a str,
    others: &'a [OpenPr],
) -> impl Iterator<Item = &'a OpenPr> {
    others
        .iter()
        .filter(move |q| q.number != number)
        // Q is P's base-branch pull request.
        .filter(move |q| q.cross_repository || q.head_ref != base_ref)
        // Q is stacked on P, directly or through other open pull requests.
        .filter(move |q| !stacked_on(q, head_ref, others))
}

/// The parent number of a `stacked:<n>` label, if `name` is one. Pure.
fn stacked_label_parent(name: &str) -> Option<u64> {
    name.strip_prefix(STACKED_LABEL_PREFIX)?
        .parse()
        .ok()
        .filter(|number| *number > 0)
}

/// The label prefix `shipyard pr --stacked-on <n>` applies, as `stacked:<n>`.
pub(in crate::app) const STACKED_LABEL_PREFIX: &str = "stacked:";

/// Whether `q`'s base-branch chain, followed through other open (same
/// repository) pull requests' head branches, reaches `head_ref`. Pure; a cycle
/// ends the walk.
fn stacked_on(q: &OpenPr, head_ref: &str, others: &[OpenPr]) -> bool {
    let mut seen = BTreeSet::new();
    let mut base = q.base_ref.as_str();
    while seen.insert(base) {
        if base == head_ref {
            return true;
        }
        match others
            .iter()
            .find(|r| !r.cross_repository && r.head_ref == base)
        {
            Some(parent) => base = parent.base_ref.as_str(),
            None => return false,
        }
    }
    false
}

/// What an overlap with `q` narrows to once `q`'s own diff is read.
#[derive(Debug)]
enum Narrowed {
    /// `q` introduced none of P's commits.
    Clear,
    /// `q` introduced these commits of P's.
    Known(Carried),
    /// `q`'s own diff is over the cap; `probe` holds the commits of P's that
    /// may still be `q`'s, `reachable` whether they are already known to be
    /// reachable from `q`'s head.
    OverCap {
        total: usize,
        probe: BTreeSet<String>,
        reachable: bool,
    },
}

/// Narrow an overlap with `q` to the commits `q` introduced: those in `q`'s
/// own diff against `q`'s own base branch (`q.base...q.head`, paged). A `q` on
/// the steward's base whose listing was complete is already listed against
/// it. A `q` whose listing was over the cap is not yet unbounded: the cap
/// applies to its own diff, not to that listing.
fn introduced_overlap(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    q: &OpenPr,
    own: &BTreeSet<String>,
    overlap: Carried,
    graphs: &mut GraphCache,
) -> Result<Narrowed, String> {
    let (candidates, reachable) = match overlap {
        Carried::Shares {
            number,
            head_sha,
            shared,
        } => {
            if q.base_ref == base {
                return Ok(Narrowed::Known(Carried::Shares {
                    number,
                    head_sha,
                    shared,
                }));
            }
            (shared.into_iter().collect::<BTreeSet<_>>(), true)
        }
        Carried::Unbounded { .. } => (own.clone(), false),
    };
    // One cached read per `(base, head)` for the pass.
    match cached_graph(run_gh, repo, graphs, &q.base_ref, &q.head_sha)? {
        CommitGraph::Complete(introduced) => {
            let shared: Vec<String> = candidates
                .into_iter()
                .filter(|sha| introduced.contains_key(sha))
                .collect();
            Ok(if shared.is_empty() {
                Narrowed::Clear
            } else {
                Narrowed::Known(Carried::Shares {
                    number: q.number,
                    head_sha: q.head_sha.clone(),
                    shared,
                })
            })
        }
        CommitGraph::OverCap { total } => Ok(Narrowed::OverCap {
            total,
            probe: candidates,
            reachable,
        }),
    }
}

/// Which of `probe` (P's commits) `q` introduced, settled commit by commit: a
/// commit is `q`'s when `q`'s head reaches it and `q`'s base does not. `None`
/// when there are too many commits to probe. `reachable` skips the head
/// probe for commits already known to be in `q`'s listing.
fn probe_introduced(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    q: &OpenPr,
    probe: &BTreeSet<String>,
    reachable: bool,
) -> Result<Option<Vec<String>>, String> {
    if probe.len() > MAX_PROBED_COMMITS {
        return Ok(None);
    }
    let mut shared = Vec::new();
    for sha in probe {
        if !reachable && !is_ancestor(run_gh, repo, sha, &q.head_sha)? {
            continue;
        }
        // P's commits are not on the steward's base, so a `q` on that base
        // introduced every commit its head reaches.
        if q.base_ref != base && is_ancestor(run_gh, repo, sha, &q.base_ref)? {
            continue;
        }
        shared.push(sha.clone());
    }
    Ok(Some(shared))
}

/// Whether `of` reaches `ancestor`, from the compare API `of...ancestor`:
/// `behind` or `identical` means `ancestor` is in `of`'s history.
fn is_ancestor(run_gh: RunGh<'_>, repo: &str, ancestor: &str, of: &str) -> Result<bool, String> {
    let raw = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/compare/{of}...{ancestor}?per_page=1"),
        "--jq".to_owned(),
        ".status".to_owned(),
    ])
    .map_err(|error| format!("compare {of}...{ancestor} unreadable: {error}"))?;
    match raw.trim() {
        "behind" | "identical" => Ok(true),
        "ahead" | "diverged" => Ok(false),
        other => Err(format!(
            "compare {of}...{ancestor} has an unknown status {other:?}"
        )),
    }
}

/// Settle one overlap with `q`: narrow it to `q`'s own diff, judge `q`, and
/// for an unapproved `q` whose own diff is over the cap probe P's commits
/// one by one, or, when P has too many for that, the roots of P's commits
/// ([`candidate_roots_unreached`]). `None` when `q` carries none of P's
/// commits. An unapproved, over-cap `q` refuses only when neither probe can
/// settle the overlap.
#[allow(clippy::too_many_arguments)]
fn resolve_overlap(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    p_head: &str,
    q: &OpenPr,
    own: &BTreeSet<String>,
    overlap: Carried,
    graphs: &mut GraphCache,
    judge: &mut dyn FnMut(&OpenPr) -> Judgement,
) -> Option<(Carried, Judgement)> {
    let unreadable = |error: String| {
        Some((
            Carried::Unbounded {
                number: q.number,
                total: 0,
            },
            Judgement::Unreadable(error),
        ))
    };
    let narrowed = match introduced_overlap(run_gh, repo, base, q, own, overlap, graphs) {
        Ok(narrowed) => narrowed,
        Err(error) => return unreadable(error),
    };
    // Re-read the carried head's approval for every candidate. The live head
    // check is part of `judge`; caching an approval would let a force-push
    // after the first candidate reuse a stale verdict later in the same pass.
    match narrowed {
        Narrowed::Clear => None,
        Narrowed::Known(carried) => Some((carried, judge(q))),
        Narrowed::OverCap {
            total,
            probe,
            reachable,
        } => {
            let judgement = judge(q);
            let unbounded = Carried::Unbounded {
                number: q.number,
                total,
            };
            if judgement.clears() {
                return Some((unbounded, judgement));
            }
            match probe_introduced(run_gh, repo, base, q, &probe, reachable) {
                Ok(Some(shared)) if shared.is_empty() => None,
                Ok(Some(shared)) => Some((
                    Carried::Shares {
                        number: q.number,
                        head_sha: q.head_sha.clone(),
                        shared,
                    },
                    judgement,
                )),
                Ok(None) => {
                    match candidate_roots_unreached(run_gh, repo, base, p_head, q, graphs) {
                        Ok(true) => None,
                        Ok(false) => Some((unbounded, judgement)),
                        Err(error) => unreadable(error),
                    }
                }
                Err(error) => unreadable(error),
            }
        }
    }
}

/// Whether `q`'s head provably reaches none of P's commits not on `base`,
/// settled from P's side when P has too many commits to probe one by one.
///
/// Let S be P's commits not on `base` (`base...p_head`, read with parents).
/// S is closed downward inside itself: a commit of S has every parent either
/// in S or on `base`. So if `q`'s head reaches any commit of S, following
/// parents that stay in S from it ends at a root of S (a commit of S none of
/// whose parents is in S), which `q`'s head then reaches too. Probing every
/// root therefore settles the whole set: when `q`'s head reaches no root, it
/// reaches no commit of S, and `q`'s own commits (all reachable from its
/// head) share none with P. This is the merge-base argument made exact: it
/// says every merge base of P's and `q`'s heads is on `base`, without
/// trusting the single merge base the compare API names, which a criss-cross
/// history can have several of.
///
/// `false` (not settled, so the caller fails closed) when S is over the cap,
/// empty (the base absorbed P between reads; the next pass re-reads it), or
/// has more than [`MAX_PROBED_COMMITS`] roots, or when any root is reachable.
/// A root with no parents at all (an orphan history) is a root like any
/// other. The graph must be complete: a missing commit could hide a root.
/// A failed read is an error.
fn candidate_roots_unreached(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    p_head: &str,
    q: &OpenPr,
    graphs: &mut GraphCache,
) -> Result<bool, String> {
    let CommitGraph::Complete(graph) = cached_graph(run_gh, repo, graphs, base, p_head)? else {
        return Ok(false);
    };
    let roots: Vec<&String> = graph
        .iter()
        .filter(|(_, parents)| !parents.iter().any(|parent| graph.contains_key(parent)))
        .map(|(sha, _)| sha)
        .collect();
    if roots.is_empty() || roots.len() > MAX_PROBED_COMMITS {
        return Ok(false);
    }
    for root in roots {
        if is_ancestor(run_gh, repo, root, &q.head_sha)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether P may be admitted given what it carries and each carried pull
/// request's judgement. Pure. An approved Q never refuses; an unapproved Q
/// whose commits P carries refuses before an unbounded one, and either before
/// an unreadable one.
#[must_use]
pub(super) fn decide(base: &str, judged: &[(Carried, Judgement)]) -> Option<Refusal> {
    let mut carries = Vec::new();
    let mut unbounded = Vec::new();
    let mut unreadable = Vec::new();
    for (carried, judgement) in judged {
        match (carried, judgement) {
            (_, Judgement::Approved | Judgement::Cleared(_)) => {}
            (Carried::Shares { .. }, Judgement::Unapproved(reason) | Judgement::Held(reason)) => {
                carries.push((carried, reason));
            }
            (
                Carried::Unbounded { .. },
                Judgement::Unapproved(reason) | Judgement::Held(reason),
            ) => {
                unbounded.push((carried, reason));
            }
            (_, Judgement::Unreadable(error)) => unreadable.push((carried, error)),
        }
    }
    if let Some((first, _)) = carries.first() {
        let detail = carries
            .iter()
            .map(|(carried, reason)| match carried {
                Carried::Shares {
                    number,
                    head_sha,
                    shared,
                } => format!(
                    "carries {} commit(s) of #{number} not on {base} ({}); #{number}'s head \
                     {head_sha} is not approved: {reason}",
                    shared.len(),
                    name_shas(shared)
                ),
                Carried::Unbounded { .. } => String::new(),
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Some(Refusal {
            token: format!("carries_unapproved_commits:#{}", first.number()),
            detail: format!(
                "{detail}. Have this head reviewed again after that HOLD, land or approve that \
                 pull request first, or drop its commits from this branch; a branch built on \
                 another pull request should target that pull request's branch"
            ),
        });
    }
    if let Some((first, reason)) = unbounded.first() {
        let Carried::Unbounded { number, total } = first else {
            return None;
        };
        return Some(Refusal {
            token: format!("carried_commits_over_cap:#{number}"),
            detail: format!(
                "whether this head carries #{number}'s commits could not be settled within the \
                 read limits ({MAX_COMMITS_PER_PR} commits listed, {MAX_PROBED_COMMITS} probed \
                 per pull request, {MAX_REVERSE_PROBES} per over-cap head; #{number} counted \
                 {total}), and #{number}'s head is not approved: {reason}"
            ),
        });
    }
    unreadable.first().map(|(first, error)| Refusal {
        token: format!("carried_commits_unreadable:#{}", first.number()),
        detail: format!("approval of #{} unreadable: {error}", first.number()),
    })
}

fn name_shas(shas: &[String]) -> String {
    let named = shas
        .iter()
        .take(SHAS_NAMED)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    match shas.len().checked_sub(SHAS_NAMED) {
        Some(more) if more > 0 => format!("{named} and {more} more"),
        _ => named,
    }
}

/// Reads this pass already made, per repository.
#[derive(Debug, Default)]
pub(super) struct PassCache {
    open: Option<Result<Vec<OpenPr>, String>>,
    /// Each open pull request's own roots, by number and listed head, for
    /// every over-cap candidate and admission path in the pass.
    roots: BTreeMap<(u64, String), Result<Roots, String>>,
    /// Commit graphs (`base...head` with parents), by `(base, head)`, read
    /// once per pass.
    graphs: GraphCache,
}

impl PassCache {
    /// The open-pull-request listing this pass read, when it read one: the
    /// dwell step reuses it to see stacked pull requests at no extra cost.
    pub(super) fn take_open(&mut self) -> Option<Vec<OpenPr>> {
        match self.open.take() {
            Some(Ok(open)) => Some(open),
            _ => None,
        }
    }
}

/// Commit graphs read this pass, by `(base, head)`.
type GraphCache = BTreeMap<(String, String), Result<CommitGraph, String>>;

/// `base...head` with parents, read once per pass.
fn cached_graph(
    run_gh: RunGh<'_>,
    repo: &str,
    graphs: &mut GraphCache,
    base: &str,
    head: &str,
) -> Result<CommitGraph, String> {
    graphs
        .entry((base.to_owned(), head.to_owned()))
        .or_insert_with(|| compare_graph(run_gh, repo, base, head))
        .clone()
}

/// The guard on the steward's own admission path (enqueue and exact-head
/// merge): `Some` refuses `pr` (already live revalidated) with
/// `(mutation token, detail)`.
pub(super) fn admission_refusal(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    ledger: &super::StewardLedger,
) -> Option<(String, String)> {
    let run_gh = |argv: &[String]| {
        context
            .actions
            .run_gh(argv)
            .map_err(|error| error.to_string())
    };
    let policy = match crate::head_approval::read_policy_required(
        &run_gh,
        &context.observation.repo,
        &context.observation.base,
    ) {
        Ok(policy) => policy,
        Err(error) => {
            return Some((
                "carried_commits_unreadable".to_owned(),
                format!("head approval policy unreadable: {error}"),
            ));
        }
    };
    let mut cache = context.carried_commits.borrow_mut();
    refusal_for(
        context.actions,
        &context.observation.repo,
        &context.observation.base,
        &policy,
        &pr.fact,
        Some(ledger),
        &mut cache,
    )
    .map(|refusal| (refusal.token, refusal.detail))
}

/// The guard every steward admission path runs once `policy` (the base's
/// strictly read approval policy) is in hand: the managed enqueue, the
/// exact-head merge, and the `--arm-unqueued` native-arm backstop. `ledger`
/// excuses queue removals the steward made itself, as admission does.
pub(super) fn refusal_for(
    actions: &super::GitHubActions,
    repo: &str,
    base: &str,
    policy: &crate::head_approval::ApprovalPolicy,
    pr: &crate::merge_steward::StewardPullRequest,
    ledger: Option<&super::StewardLedger>,
    cache: &mut PassCache,
) -> Option<Refusal> {
    if !policy.required {
        return None;
    }
    let run_gh = |argv: &[String]| actions.run_gh(argv).map_err(|error| error.to_string());
    let comment_source = matches!(
        policy.source,
        crate::head_approval::guard::ApprovalSource::Comments
    );
    let mut report_policy = policy.clone();
    report_policy.dry_run = true;
    let histories = std::cell::RefCell::new(BTreeMap::new());
    let p_dated = std::cell::RefCell::new(None);
    let mut judge = |q: &OpenPr| -> Judgement {
        let queue = match super::native_arm::read_queue_state(actions, repo, q.number) {
            Ok(queue) => queue,
            Err(error) => return Judgement::Unreadable(error),
        };
        if let Err(error) = live_head_matches(&queue, q) {
            return Judgement::Unreadable(error);
        }
        let queue = match ledger {
            Some(ledger) => {
                let recorded = super::unapproved_admission::steward_removal_times(
                    ledger,
                    repo,
                    q.number,
                    &q.head_sha,
                );
                super::unapproved_admission::without_steward_removals(
                    &queue,
                    &q.head_sha,
                    &recorded,
                )
            }
            None => queue,
        };
        let gate = match crate::head_approval::evaluate_with_policy_for_author_with_diagnosis(
            &run_gh,
            repo,
            q.number,
            &q.head_sha,
            &report_policy,
            &queue,
            q.author.as_deref(),
        ) {
            Ok((gate, _, blocker)) => judgement_of(&gate, blocker.as_ref(), comment_source),
            Err(error) => return Judgement::Unreadable(error),
        };
        // Under fleet-guard admission there is no verdict timeline to date
        // P's approval by, so a refused Q keeps refusing: fail closed.
        if !comment_source || gate.clears() {
            return gate;
        }
        let history = |number: u64, author: Option<&str>| {
            histories
                .borrow_mut()
                .entry(number)
                .or_insert_with(|| {
                    crate::head_approval::verdict_history(
                        &run_gh,
                        repo,
                        number,
                        &report_policy,
                        author,
                    )
                })
                .clone()
        };
        // P is dated only by an approval the gate would count: never an
        // edited one, by GraphQL's edit history, read once per candidate.
        let p_history = p_dated
            .borrow_mut()
            .get_or_insert_with(|| {
                history(pr.number, pr.author.as_deref())
                    .and_then(|history| history.without_edited_approvals(&run_gh, &pr.head_sha))
            })
            .clone();
        match (p_history, history(q.number, q.author.as_deref())) {
            (Ok(p_history), Ok(q_history)) => recency_judgement(
                gate,
                pr.number,
                &pr.head_sha,
                &p_history,
                q.number,
                &q_history,
            ),
            (Err(error), _) | (_, Err(error)) => Judgement::Unreadable(error),
        }
    };
    refusal_with(
        &run_gh,
        repo,
        base,
        pr.number,
        &pr.head_branch,
        &pr.head_sha,
        cache,
        &mut judge,
    )
}

/// Whether a Q the gate refuses still holds P back, by HOLD recency. P's
/// exact-head approval reviewed every commit P carries, Q's included, so a
/// shared commit stops P only while some counting HOLD is at least as new as
/// that approval: a HOLD on any head of Q's, or of P's own. Every HOLD on Q
/// counts, not only those on heads that contain the shared commits: a
/// deliberate superset that refuses more, never less, and needs no reads of
/// heads Q has since dropped. With no such HOLD Q is
/// [`Judgement::Cleared`]; with one Q is [`Judgement::Held`], naming the
/// HOLD and the remedy (a review of P's head after it). A P with no counting,
/// unedited exact-head APPROVE of its own (one carried from an earlier head,
/// or a rollup) has nothing to date, so any HOLD holds it. Only server-set
/// times are compared, never labels, numbers or commit dates. An approved,
/// cleared or unreadable `gate` is returned as it is. Pure.
pub(super) fn recency_judgement(
    gate: Judgement,
    p_number: u64,
    p_head: &str,
    p_history: &crate::head_approval::VerdictHistory,
    q_number: u64,
    q_history: &crate::head_approval::VerdictHistory,
) -> Judgement {
    let reason = match &gate {
        Judgement::Unapproved(reason) | Judgement::Held(reason) => reason.clone(),
        Judgement::Approved | Judgement::Cleared(_) | Judgement::Unreadable(_) => return gate,
    };
    let approved_at = p_history.approved_at(p_head);
    let newest = q_history
        .holds
        .iter()
        .map(|hold| (q_number, hold))
        .chain(p_history.holds.iter().map(|hold| (p_number, hold)))
        .filter(|(_, hold)| approved_at.is_none_or(|approved| hold.at >= approved))
        .max_by_key(|(_, hold)| hold.at);
    let approval = approved_at.map_or_else(
        || format!("#{p_number} has no exact-head APPROVE of its own at {p_head}"),
        |at| format!("#{p_number}'s exact-head APPROVE at {}", at.to_rfc3339()),
    );
    match newest {
        None => Judgement::Cleared(format!(
            "{reason}; no HOLD on #{q_number} or #{p_number} is as new as {approval}"
        )),
        Some((number, hold)) => Judgement::Held(format!(
            "{reason}; a HOLD on #{number}'s head {} ({} {}, {}) is not older than {approval}: \
             a review of #{p_number}'s head posted after that HOLD clears it",
            hold.head,
            hold.record,
            hold.id
                .map_or_else(|| "unrecorded".to_owned(), |id| id.to_string()),
            hold.at.to_rfc3339()
        )),
    }
}

/// The judgement of Q's head from the approval gate and its blocker. A HOLD
/// at the head is [`Judgement::Held`]. Under fleet-guard admission
/// (`comment_source` false) a HOLD is not observable here, so any refusal is
/// treated as held and a refused stack is never excused: fail closed. Pure.
fn judgement_of(
    gate: &crate::head_approval::HeadGate,
    blocker: Option<&crate::head_approval::ApprovalBlocker>,
    comment_source: bool,
) -> Judgement {
    if gate.allows() {
        Judgement::Approved
    } else if !comment_source
        || matches!(
            blocker,
            Some(crate::head_approval::ApprovalBlocker::Held { .. })
        )
    {
        Judgement::Held(gate.explain())
    } else {
        Judgement::Unapproved(gate.explain())
    }
}

/// [`admission_refusal`] over any `gh` runner and approval judge, after the
/// policy says approval is required.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn refusal_with(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    number: u64,
    head_ref: &str,
    head_sha: &str,
    cache: &mut PassCache,
    judge: &mut dyn FnMut(&OpenPr) -> Judgement,
) -> Option<Refusal> {
    let own = match compare_commits(run_gh, repo, base, head_sha) {
        Ok(CommitList::Complete(commits)) => commits,
        Ok(CommitList::OverCap { .. }) => {
            return over_cap_refusal(run_gh, repo, base, number, head_ref, head_sha, cache, judge);
        }
        Err(error) => return Some(unreadable(error)),
    };
    if own.is_empty() {
        return None;
    }
    let open = cache
        .open
        .get_or_insert_with(|| open_pull_requests(run_gh, repo));
    let others = match open {
        Ok(others) => others,
        Err(error) => return Some(unreadable(error.clone())),
    };
    let candidate = Candidate {
        number,
        base_ref: base,
        head_ref,
        commits: &own,
    };
    let overlaps = carried(&candidate, others);
    let mut judged = Vec::with_capacity(overlaps.len());
    for overlap in overlaps {
        let q = others
            .iter()
            .find(|q| q.number == overlap.number())
            .expect("an overlap names a listed pull request");
        let Some(settled) = resolve_overlap(
            run_gh,
            repo,
            base,
            head_sha,
            q,
            &own,
            overlap,
            &mut cache.graphs,
            judge,
        ) else {
            continue;
        };
        if settled.1.clears() {
            continue;
        }
        if let Some(settled) = landed::without_landed_commits(run_gh, repo, base, settled) {
            judged.push(settled);
        }
    }
    decide(base, &judged)
}

/// Q's approval is judged only at the head the listing saw, so the live
/// queue-state read must still show that head. A push after the listing
/// (or an answer with no head) fails closed: judging the listed head could
/// call a since-replaced, unapproved head approved. The next pass re-reads
/// the listing. Pure.
pub(super) fn live_head_matches(queue: &Value, q: &OpenPr) -> Result<(), String> {
    match queue
        .pointer("/data/repository/pullRequest/headRefOid")
        .and_then(Value::as_str)
    {
        Some(live) if live == q.head_sha => Ok(()),
        Some(live) => Err(format!(
            "#{} moved from {} to {live} after this pass listed it; its commits and \
             approval are re-read next pass",
            q.number, q.head_sha
        )),
        None => Err(format!(
            "#{}'s live head is unreadable, so its approval at {} cannot be bound to it",
            q.number, q.head_sha
        )),
    }
}

fn unreadable(error: String) -> Refusal {
    Refusal {
        token: "carried_commits_unreadable".to_owned(),
        detail: error,
    }
}

/// `head`'s commits not on `base`, from the compare API, paged up to
/// [`MAX_COMMITS_PER_PR`].
fn compare_commits(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    head: &str,
) -> Result<CommitList, String> {
    let mut shas = BTreeSet::new();
    let mut total = 0;
    let mut basis = None;
    for page in 1..=MAX_COMMITS_PER_PR.div_ceil(COMPARE_PAGE) {
        let raw = run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/compare/{base}...{head}?per_page={COMPARE_PAGE}&page={page}"),
            "--jq".to_owned(),
            "{total: .total_commits, merge_base: .merge_base_commit.sha, shas: [.commits[].sha]}"
                .to_owned(),
        ])
        .map_err(|error| format!("compare {base}...{head} unreadable: {error}"))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("compare {base}...{head} malformed: {error}"))?;
        let page_total = value
            .get("total")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("compare {base}...{head} has no total_commits"))?;
        let page_shas = sha_array(value.get("shas"))
            .ok_or_else(|| format!("compare {base}...{head} has no commit list"))?;
        same_basis(&mut basis, page_total, &value, base, head)?;
        total = usize::try_from(page_total).unwrap_or(usize::MAX);
        if total > MAX_COMMITS_PER_PR {
            return Ok(CommitList::OverCap { total });
        }
        let read = page_shas.len();
        shas.extend(page_shas);
        if shas.len() >= total || read < COMPARE_PAGE {
            break;
        }
    }
    Ok(bounded(total, shas))
}

/// [`refusal_with`] for a candidate P whose own commits are over the cap and
/// so cannot be listed: the check runs the other way. For each other open
/// pull request Q, Q's own commits not on the base (`base...Q.head`, and for a
/// Q on another base branch also its own diff `Q.base...Q.head`) are read with
/// their parents; that set's roots are the commits none of whose parents is in
/// it. P's head carries some commit of the set exactly when it reaches one of
/// the roots (from any carried commit, following parents inside the set ends at
/// a root P also reaches), and reachability is probed exactly, root by root.
/// The commit lists may name more commits than the exact set, never fewer, so
/// they can only add roots and refuse more, never admit a carrier. A Q that
/// shares a root with P is judged like any carried pull request. An
/// unapproved Q whose set or roots are over the cap, and any failed read, fail
/// closed.
#[allow(clippy::too_many_arguments)]
fn over_cap_refusal(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    number: u64,
    head_ref: &str,
    head_sha: &str,
    cache: &mut PassCache,
    judge: &mut dyn FnMut(&OpenPr) -> Judgement,
) -> Option<Refusal> {
    let open = cache
        .open
        .get_or_insert_with(|| open_pull_requests(run_gh, repo));
    let others = match open {
        Ok(others) => others,
        Err(error) => return Some(unreadable(error.clone())),
    };
    let mut judged = Vec::new();
    let mut probes_left = MAX_REVERSE_PROBES;
    for q in unrelated(number, base, head_ref, others) {
        let roots = cache
            .roots
            .entry((q.number, q.head_sha.clone()))
            .or_insert_with(|| own_roots(run_gh, repo, base, q))
            .clone();
        let roots = match roots {
            Ok(Roots::Clear) => continue,
            Ok(Roots::Probe(roots)) if roots.len() <= probes_left => roots,
            Ok(Roots::Probe(roots)) => {
                // The probe budget is spent: only an approved Q is settled.
                judge_unbounded(q, roots.len(), judge, &mut judged);
                continue;
            }
            Ok(Roots::OverCap { total }) => {
                judge_unbounded(q, total, judge, &mut judged);
                continue;
            }
            Err(error) => {
                judged.push(unreadable_overlap(q, error));
                continue;
            }
        };
        probes_left -= roots.len();
        let mut shared = Vec::new();
        let mut failed = None;
        for root in roots {
            match is_ancestor(run_gh, repo, &root, head_sha) {
                Ok(true) => shared.push(root),
                Ok(false) => {}
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        if let Some(error) = failed {
            judged.push(unreadable_overlap(q, error));
            continue;
        }
        if shared.is_empty() {
            continue;
        }
        let judgement = judge(q);
        if !judgement.clears() {
            judged.push((
                Carried::Shares {
                    number: q.number,
                    head_sha: q.head_sha.clone(),
                    shared,
                },
                judgement,
            ));
        }
    }
    decide(base, &judged)
}

/// Record `q` as unbounded unless the gate admits its head.
fn judge_unbounded(
    q: &OpenPr,
    total: usize,
    judge: &mut dyn FnMut(&OpenPr) -> Judgement,
    judged: &mut Vec<(Carried, Judgement)>,
) {
    let judgement = judge(q);
    if !judgement.clears() {
        judged.push((
            Carried::Unbounded {
                number: q.number,
                total,
            },
            judgement,
        ));
    }
}

fn unreadable_overlap(q: &OpenPr, error: String) -> (Carried, Judgement) {
    (
        Carried::Unbounded {
            number: q.number,
            total: 0,
        },
        Judgement::Unreadable(error),
    )
}

/// The roots of an open pull request's own commits not on the base.
#[derive(Clone, Debug)]
enum Roots {
    /// It has no such commits.
    Clear,
    /// These roots, at most [`MAX_PROBED_COMMITS`].
    Probe(Vec<String>),
    /// Too many commits, or roots, to settle.
    OverCap { total: usize },
}

/// The roots of `q`'s own commits not on `base`: `base...q.head`, narrowed to
/// `q.base...q.head` when `q` targets another branch, keeping each commit's
/// parents.
fn own_roots(run_gh: RunGh<'_>, repo: &str, base: &str, q: &OpenPr) -> Result<Roots, String> {
    let mut narrowed_to_own_base = false;
    let mut graph = match compare_graph(run_gh, repo, base, &q.head_sha)? {
        CommitGraph::Complete(graph) => graph,
        CommitGraph::OverCap { .. } if q.base_ref != base => {
            // The repository-base comparison can be over the cap solely
            // because Q inherits a large stacked history. Read Q's own
            // base-to-head graph before deciding that Q is unbounded.
            narrowed_to_own_base = true;
            match compare_graph(run_gh, repo, &q.base_ref, &q.head_sha)? {
                CommitGraph::Complete(graph) => graph,
                CommitGraph::OverCap { total } => return Ok(Roots::OverCap { total }),
            }
        }
        CommitGraph::OverCap { total } => return Ok(Roots::OverCap { total }),
    };
    if q.base_ref != base && !narrowed_to_own_base {
        match compare_commits(run_gh, repo, &q.base_ref, &q.head_sha)? {
            CommitList::Complete(introduced) => {
                graph.retain(|sha, _| introduced.contains(sha));
            }
            CommitList::OverCap { total } => return Ok(Roots::OverCap { total }),
        }
    }
    if graph.is_empty() {
        return Ok(Roots::Clear);
    }
    let roots: Vec<String> = graph
        .iter()
        .filter(|(_, parents)| !parents.iter().any(|parent| graph.contains_key(parent)))
        .map(|(sha, _)| sha.clone())
        .collect();
    if roots.len() > MAX_PROBED_COMMITS {
        return Ok(Roots::OverCap { total: graph.len() });
    }
    Ok(Roots::Probe(roots))
}

/// Commits with their parents, as far as the steward read them.
#[derive(Clone, Debug)]
enum CommitGraph {
    /// Every commit, each with its parents.
    Complete(BTreeMap<String, Vec<String>>),
    /// More commits than [`MAX_COMMITS_PER_PR`].
    OverCap { total: usize },
}

/// `head`'s commits not on `base` with each one's parents, from the compare
/// API, paged up to [`MAX_COMMITS_PER_PR`].
fn compare_graph(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    head: &str,
) -> Result<CommitGraph, String> {
    let mut graph = BTreeMap::new();
    let mut total = 0;
    let mut basis = None;
    for page in 1..=MAX_COMMITS_PER_PR.div_ceil(COMPARE_PAGE) {
        let raw = run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/compare/{base}...{head}?per_page={COMPARE_PAGE}&page={page}"),
            "--jq".to_owned(),
            "{total: .total_commits, merge_base: .merge_base_commit.sha, commits: [.commits[] | \
             {sha, parents: [.parents[].sha]}]}"
                .to_owned(),
        ])
        .map_err(|error| format!("compare {base}...{head} unreadable: {error}"))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("compare {base}...{head} malformed: {error}"))?;
        let page_total = value
            .get("total")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("compare {base}...{head} has no total_commits"))?;
        same_basis(&mut basis, page_total, &value, base, head)?;
        total = usize::try_from(page_total).unwrap_or(usize::MAX);
        if total > MAX_COMMITS_PER_PR {
            return Ok(CommitGraph::OverCap { total });
        }
        let commits = value
            .get("commits")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("compare {base}...{head} has no commit list"))?;
        for commit in commits {
            let sha = commit
                .get("sha")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("compare {base}...{head} has a commit without sha"))?;
            let parents = commit
                .get("parents")
                .and_then(Value::as_array)
                .and_then(|parents| {
                    parents
                        .iter()
                        .map(|parent| parent.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| format!("compare {base}...{head}: {sha} has no parents"))?;
            graph.insert(sha.to_owned(), parents);
        }
        if graph.len() >= total || commits.len() < COMPARE_PAGE {
            break;
        }
    }
    Ok(if graph.len() < total {
        CommitGraph::OverCap { total }
    } else {
        CommitGraph::Complete(graph)
    })
}

/// Every page of one paged compare must describe the same comparison: `base`
/// is a branch name, so if it moves between pages the pages shift and a
/// commit can fall between them while the count still adds up. A changed
/// total or merge base fails the read closed; the next pass re-reads it.
fn same_basis(
    basis: &mut Option<(u64, Option<String>)>,
    total: u64,
    page: &Value,
    base: &str,
    head: &str,
) -> Result<(), String> {
    let merge_base = page
        .get("merge_base")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match basis {
        None => {
            *basis = Some((total, merge_base));
            Ok(())
        }
        Some(first) if *first == (total, merge_base) => Ok(()),
        Some(_) => Err(format!(
            "compare {base}...{head} moved between pages; its commits are re-read next pass"
        )),
    }
}

fn bounded(total: usize, shas: BTreeSet<String>) -> CommitList {
    if total > MAX_COMMITS_PER_PR || shas.len() < total {
        CommitList::OverCap { total }
    } else {
        CommitList::Complete(shas)
    }
}

fn sha_array(value: Option<&Value>) -> Option<BTreeSet<String>> {
    value?
        .as_array()?
        .iter()
        .map(|sha| sha.as_str().map(str::to_owned))
        .collect()
}

/// Every open pull request in `repo` with its commits.
fn open_pull_requests(run_gh: RunGh<'_>, repo: &str) -> Result<Vec<OpenPr>, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| "invalid repository slug".to_owned())?;
    let mut open = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_LISTING_PAGES {
        let mut args = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={OPEN_PULL_COMMITS_QUERY}"),
            "-F".to_owned(),
            format!("owner={owner}"),
            "-F".to_owned(),
            format!("name={name}"),
        ];
        if let Some(cursor) = &cursor {
            args.push("-f".to_owned());
            args.push(format!("cursor={cursor}"));
        }
        let raw =
            run_gh(&args).map_err(|error| format!("open pull requests unreadable: {error}"))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("open pull requests malformed: {error}"))?;
        if let Some(errors) = value.get("errors").filter(|errors| !errors.is_null()) {
            return Err(format!("open pull requests unreadable: {errors}"));
        }
        let page = value
            .pointer("/data/repository/pullRequests")
            .ok_or_else(|| "open pull requests unreadable: no pullRequests".to_owned())?;
        let nodes = page
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| "open pull requests unreadable: no nodes".to_owned())?;
        for node in nodes {
            open.push(parse_open_pr(run_gh, repo, node)?);
        }
        let next = page
            .pointer("/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !next {
            return Ok(open);
        }
        cursor = page
            .pointer("/pageInfo/endCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            return Err("open pull requests unreadable: next page has no cursor".to_owned());
        }
    }
    Err(format!(
        "more than {} open pull requests; the carried-commit check reads at most that many",
        MAX_LISTING_PAGES * LISTING_PAGE
    ))
}

fn parse_open_pr(run_gh: RunGh<'_>, repo: &str, node: &Value) -> Result<OpenPr, String> {
    let text = |field: &str| {
        node.get(field)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("open pull request without {field}"))
    };
    let number = node
        .get("number")
        .and_then(Value::as_u64)
        .ok_or_else(|| "open pull request without number".to_owned())?;
    let total = node
        .pointer("/carriedCommits/totalCount")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("#{number}: no commit count"))?;
    let total = usize::try_from(total).unwrap_or(usize::MAX);
    let commits = if total <= LISTING_COMMITS {
        let shas = node
            .pointer("/carriedCommits/nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("#{number}: no commits"))?
            .iter()
            .map(|commit| {
                commit
                    .pointer("/commit/oid")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<Option<BTreeSet<_>>>()
            .ok_or_else(|| format!("#{number}: a commit without oid"))?;
        bounded(total, shas)
    } else if total <= MAX_COMMITS_PER_PR {
        bounded(total, rest_commits(run_gh, repo, number)?)
    } else {
        CommitList::OverCap { total }
    };
    Ok(OpenPr {
        number,
        head_ref: text("headRefName")?,
        base_ref: text("baseRefName")?,
        head_sha: text("headRefOid")?,
        cross_repository: node
            .get("isCrossRepository")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        author: node
            .pointer("/author/login")
            .and_then(Value::as_str)
            .map(str::to_owned),
        commits,
        stacked_on: node
            .pointer("/labels/nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|label| label.get("name").and_then(Value::as_str))
            .find_map(stacked_label_parent),
    })
}

/// Up to [`MAX_COMMITS_PER_PR`] commits of pull request `number`, over REST.
fn rest_commits(run_gh: RunGh<'_>, repo: &str, number: u64) -> Result<BTreeSet<String>, String> {
    let raw = run_gh(&[
        "api".to_owned(),
        "--paginate".to_owned(),
        format!("repos/{repo}/pulls/{number}/commits?per_page=100"),
        "--jq".to_owned(),
        ".[].sha".to_owned(),
    ])
    .map_err(|error| format!("#{number} commits unreadable: {error}"))?;
    Ok(raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

mod landed;

#[cfg(test)]
mod stacked_verdict_tests;

#[cfg(test)]
mod parent_child_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod stacked_label_tests;
