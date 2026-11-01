//! Refuse to admit a head that carries another open pull request's
//! unapproved commits.
//!
//! A HOLD stops one pull request's head. It does not stop the same commits
//! reaching the base through a second pull request: a branch that merged the
//! held branch (or a branch that did) carries the held commits, a reviewer can
//! approve the second pull request on its own diff, and the steward would then
//! admit the held code. Before admitting candidate P, the steward therefore
//! compares P's commits that are not on the base with the commits of every
//! other open pull request Q in the repository. When P carries any commit of a
//! Q whose current head the exact-head approval gate does not admit (a counted
//! HOLD, or no counted APPROVE), P is refused with
//! `carries_unapproved_commits:#Q`, and the shared commits are named.
//!
//! Not blocked:
//!
//! * Q is P's own base-branch pull request (same repository, Q's head branch
//!   is P's base); a properly stacked P never targets the steward's base.
//! * Q is stacked on P (Q's base branch is P's head branch): Q builds on P.
//! * Q is a Shipyard rollup that merged P's head: it shares only P's own
//!   commits, and its other members are open pull requests judged directly.
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
//! mutation context), Q's approval is read only when Q overlaps a candidate
//! and is cached per head, and P's own set costs one compare call. At most
//! [`MAX_COMMITS_PER_PR`] commits are read for any pull request; a pull
//! request with more fails closed (`carried_commits_over_cap:#N`) unless it is
//! an approved Q. An unreadable listing, compare or approval also fails closed
//! (`carried_commits_unreadable`).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::MutationApplyContext;
use super::ObservedPr;
use crate::head_approval::RunGh;

/// Most commits read for any one pull request. GitHub's compare and pull
/// request commit listings both stop at 250.
pub(super) const MAX_COMMITS_PER_PR: usize = 250;

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
     isCrossRepository author{login} carriedCommits:commits(first:100){totalCount \
     nodes{commit{oid}}}}}}}";

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
}

/// The candidate under admission.
#[derive(Clone, Debug)]
pub(super) struct Candidate<'a> {
    pub number: u64,
    /// The steward's base: P's base branch.
    pub base_ref: &'a str,
    /// P's head branch.
    pub head_ref: &'a str,
    /// P's head commit.
    pub head_sha: &'a str,
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

/// The exact-head approval gate's verdict on Q's current head.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Judgement {
    /// The gate admits Q's head.
    Approved,
    /// The gate refuses Q's head, for this reason.
    Unapproved(String),
    /// Q's approval could not be read.
    Unreadable(String),
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
    others
        .iter()
        .filter(|q| q.number != candidate.number)
        // Q is P's base-branch pull request.
        .filter(|q| q.cross_repository || q.head_ref != candidate.base_ref)
        // Q is stacked on P.
        .filter(|q| q.base_ref != candidate.head_ref)
        // Q is a rollup with P as a member: it merged P's head, so what they
        // share is P's own, and the rollup's other members are judged as
        // pull requests of their own.
        .filter(|q| !(is_rollup(q) && contains(q, candidate.head_sha)))
        .filter_map(|q| match &q.commits {
            CommitList::Complete(commits) => {
                let shared: Vec<String> =
                    candidate.commits.intersection(commits).cloned().collect();
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

fn is_rollup(q: &OpenPr) -> bool {
    !q.cross_repository && q.head_ref.starts_with(crate::rollup::BRANCH_PREFIX)
}

fn contains(q: &OpenPr, sha: &str) -> bool {
    matches!(&q.commits, CommitList::Complete(commits) if commits.contains(sha))
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
            (_, Judgement::Approved) => {}
            (Carried::Shares { .. }, Judgement::Unapproved(reason)) => {
                carries.push((carried, reason));
            }
            (Carried::Unbounded { .. }, Judgement::Unapproved(reason)) => {
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
                "{detail}. Land or approve that pull request first, or drop its commits from \
                 this branch; a branch built on another pull request should target that pull \
                 request's branch"
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
                "#{number} has {total} commits, more than the {MAX_COMMITS_PER_PR} read, and \
                 its head is not approved ({reason}), so whether this head carries them is \
                 unproven"
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
    judged: BTreeMap<(u64, String), Judgement>,
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
    let mut report_policy = policy.clone();
    report_policy.dry_run = true;
    let mut judge = |q: &OpenPr| -> Judgement {
        let queue = match super::native_arm::read_queue_state(actions, repo, q.number) {
            Ok(queue) => queue,
            Err(error) => return Judgement::Unreadable(error),
        };
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
        match crate::head_approval::evaluate_with_policy_for_author_with_diagnosis(
            &run_gh,
            repo,
            q.number,
            &q.head_sha,
            &report_policy,
            &queue,
            q.author.as_deref(),
        ) {
            Ok((gate, _, _)) if gate.allows() => Judgement::Approved,
            Ok((gate, _, _)) => Judgement::Unapproved(gate.explain()),
            Err(error) => Judgement::Unreadable(error),
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

/// [`admission_refusal`] over any `gh` runner and approval judge, after the
/// policy says approval is required.
#[allow(clippy::too_many_arguments)]
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
        Ok(CommitList::OverCap { total }) => {
            return Some(Refusal {
                token: format!("carried_commits_over_cap:#{number}"),
                detail: format!(
                    "this head has {total} commits not on {base}, more than the \
                     {MAX_COMMITS_PER_PR} read, so whether it carries another pull request's \
                     unapproved commits is unproven"
                ),
            });
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
        head_sha,
        commits: &own,
    };
    let overlaps = carried(&candidate, others);
    let mut judged = Vec::with_capacity(overlaps.len());
    for overlap in overlaps {
        let q = others
            .iter()
            .find(|q| q.number == overlap.number())
            .expect("an overlap names a listed pull request");
        let judgement = cache
            .judged
            .entry((q.number, q.head_sha.clone()))
            .or_insert_with(|| judge(q))
            .clone();
        judged.push((overlap, judgement));
    }
    decide(base, &judged)
}

fn unreadable(error: String) -> Refusal {
    Refusal {
        token: "carried_commits_unreadable".to_owned(),
        detail: error,
    }
}

/// `head`'s commits not on `base`, from the compare API.
fn compare_commits(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    head: &str,
) -> Result<CommitList, String> {
    let raw = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/compare/{base}...{head}"),
        "--jq".to_owned(),
        "{total: .total_commits, shas: [.commits[].sha]}".to_owned(),
    ])
    .map_err(|error| format!("compare {base}...{head} unreadable: {error}"))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("compare {base}...{head} malformed: {error}"))?;
    let total = value
        .get("total")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("compare {base}...{head} has no total_commits"))?;
    let shas = sha_array(value.get("shas"))
        .ok_or_else(|| format!("compare {base}...{head} has no commit list"))?;
    let total = usize::try_from(total).unwrap_or(usize::MAX);
    Ok(bounded(total, shas))
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

#[cfg(test)]
mod tests;
