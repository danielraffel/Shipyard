//! Determining the merge mechanism from the surfaces that can express it.
//!
//! Three surfaces, and they are not interchangeable:
//!
//! | surface | can express a queue | supplies |
//! |---|---|---|
//! | `GET /repos/{o}/{r}/rulesets` + `/rulesets/{id}` | yes, authoritatively | every queue parameter |
//! | GraphQL `repository.mergeQueue(branch:)` | yes, as effective state | the same parameters, no ruleset identity |
//! | `GET /repos/{o}/{r}/branches/{b}/protection` | **no** | strict up-to-date only |
//!
//! The third row is the whole point. Branch protection's REST payload has no
//! merge-queue field, so it can neither confirm nor deny one. An
//! implementation that asks only there gets a well-formed `200 OK` describing
//! a repository whose queue it cannot see, and reports no queue. That answer
//! is indistinguishable from the truth on a repository that genuinely has
//! none, which is why this module records protection's silence as
//! [`crate::landing::SurfaceOutcome::Inexpressible`] and refuses to let it
//! vote.
//!
//! ## Combination
//!
//! 1. Any voting surface found a queue -> `Present`. Disagreement between
//!    surfaces is reported alongside the finding, never resolved silently.
//! 2. Otherwise, any voting surface was unreadable -> `Unknown`. Absence is
//!    only assertable when every surface that could have contradicted it was
//!    read.
//! 3. Otherwise, at least one voting surface answered and found none ->
//!    `Absent`.
//! 4. Nothing was measured at all -> `Unknown`.

use serde::Serialize;
use serde_json::Value;

use crate::fleet_service::Boundary;
use crate::landing::{SurfaceOutcome, SurfaceRead, Verdict};

/// Ruleset enforcement levels that actually gate a merge.
///
/// `evaluate` runs the rule and reports, but does not block, so a queue in
/// that state is reported as present-but-not-enforcing rather than folded in
/// with an active one.
const ENFORCEMENT_DISABLED: &str = "disabled";

/// A merge queue's operating parameters, as GitHub reports them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MergeQueueConfig {
    /// Ruleset that carries the queue, when it came from a ruleset.
    pub ruleset_name: Option<String>,
    /// Ruleset id, for the follow-up call a reader will want to make.
    pub ruleset_id: Option<u64>,
    /// `active`, `evaluate`, or `disabled`. Absent when the queue was only
    /// observed through GraphQL, which reports effective state without an
    /// enforcement level.
    pub enforcement: Option<String>,
    /// `ALLGREEN` or `HEADGREEN`.
    pub grouping_strategy: Option<String>,
    /// Upper bound on entries merged in one batch.
    pub max_entries_to_merge: Option<u64>,
    /// Upper bound on entries built concurrently.
    pub max_entries_to_build: Option<u64>,
    /// Lower bound before a batch merges.
    pub min_entries_to_merge: Option<u64>,
    /// `MERGE`, `SQUASH`, or `REBASE`. The method the queue uses, which is
    /// not necessarily a method the repository allows on a direct merge.
    pub merge_method: Option<String>,
    /// How long the queue waits for checks, in minutes.
    pub check_response_timeout_minutes: Option<u64>,
    /// Which surfaces reported this queue.
    pub observed_by: Vec<String>,
}

impl MergeQueueConfig {
    /// Whether this queue blocks merges rather than merely reporting.
    #[must_use]
    pub fn enforcing(&self) -> bool {
        self.enforcement
            .as_deref()
            .is_none_or(|value| !value.eq_ignore_ascii_case(ENFORCEMENT_DISABLED))
    }
}

/// The merge-queue finding, with the provenance that justifies it.
#[derive(Clone, Debug, Serialize)]
pub struct QueueFinding {
    /// Present, absent, or unknown.
    pub verdict: Verdict<MergeQueueConfig>,
    /// Surfaces that reported a queue and surfaces that reported none, when
    /// both happened. Empty when every surface agreed.
    pub disagreements: Vec<String>,
}

/// Strict up-to-date protection, and what it means for a backlog.
#[derive(Clone, Debug, Serialize)]
pub struct StrictFinding {
    /// On, off, or unknown. `Absent` means the branch carries no protection
    /// at all, which is a finding in its own right.
    pub verdict: Verdict<bool>,
    /// Plain-language consequence for an agent planning work.
    pub implication: String,
}

/// How to put a pull request on the path to merge in this repository.
#[derive(Clone, Debug, Serialize)]
pub struct EnqueueGuidance {
    /// The action an agent should take: `enqueue`, `merge`, or `unknown`.
    pub action: String,
    /// Exact command, when one can be named.
    pub command: Option<String>,
    /// Why this is the action here.
    pub rationale: String,
}

/// What a single surface concluded about queue presence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceVote {
    /// This surface found a queue.
    Found,
    /// This surface was read and found none.
    NotFound,
    /// This surface could not be read.
    Unreadable(Boundary),
    /// This surface cannot express a queue and therefore did not vote.
    Abstained,
}

/// A surface's raw payload, or the reason it is missing.
#[derive(Clone, Debug, Default)]
pub enum Payload {
    /// Parsed JSON from the surface.
    Json(Value),
    /// The surface answered that the thing does not exist.
    NotFound,
    /// The surface could not be read.
    Unreadable(Boundary, String),
    /// The surface was not consulted.
    #[default]
    NotConsulted,
}

/// Every input the queue determination needs, gathered but not yet judged.
#[derive(Clone, Debug, Default)]
pub struct QueueInputs {
    /// Full ruleset objects, each including its `rules` array. Detail calls,
    /// not the summary list, because the list omits `rules` entirely.
    pub rulesets: Option<Vec<Value>>,
    /// Why the rulesets could not be read.
    pub rulesets_error: Option<(Boundary, String)>,
    /// `GET /repos/{o}/{r}/branches/{b}/protection`.
    pub protection: Payload,
    /// GraphQL `repository.mergeQueue(branch:)`, which is `null` when absent.
    pub graphql_queue: Payload,
}

/// Decide the queue finding and record every surface that was consulted.
///
/// `surfaces` is appended to rather than returned so the caller can keep one
/// ordered provenance list across all of the report's findings.
pub fn determine_queue(
    inputs: &QueueInputs,
    base: &str,
    default_branch: Option<&str>,
    surfaces: &mut Vec<SurfaceRead>,
) -> QueueFinding {
    let mut votes = Votes::default();

    read_rulesets(inputs, base, default_branch, surfaces, &mut votes);
    record_protection(inputs, surfaces);
    read_graphql_queue(inputs, surfaces, &mut votes);

    let disagreements = describe_disagreements(&votes);
    let verdict = combine(votes);
    QueueFinding {
        verdict,
        disagreements,
    }
}

/// What each surface concluded, accumulated across the three reads.
#[derive(Default)]
struct Votes {
    found: Vec<MergeQueueConfig>,
    not_found: Vec<String>,
    unreadable: Vec<(Boundary, String)>,
}

/// Surface 1: rulesets. The only one that names the ruleset carrying the
/// queue, and the one a protection-only implementation never reaches.
fn read_rulesets(
    inputs: &QueueInputs,
    base: &str,
    default_branch: Option<&str>,
    surfaces: &mut Vec<SurfaceRead>,
    votes: &mut Votes,
) {
    match (&inputs.rulesets, &inputs.rulesets_error) {
        (_, Some((boundary, detail))) => {
            surfaces.push(SurfaceRead {
                surface: "rulesets".to_owned(),
                query: "GET /repos/{owner}/{repo}/rulesets".to_owned(),
                outcome: SurfaceOutcome::Unreadable {
                    boundary: *boundary,
                    detail: detail.clone(),
                },
            });
            votes
                .unreadable
                .push((*boundary, format!("rulesets: {detail}")));
        }
        (Some(rulesets), None) => {
            surfaces.push(SurfaceRead {
                surface: "rulesets".to_owned(),
                query: "GET /repos/{owner}/{repo}/rulesets/{id}".to_owned(),
                outcome: SurfaceOutcome::Read,
            });
            let mut hit = false;
            for ruleset in rulesets {
                if !ruleset_targets_branch(ruleset, base, default_branch) {
                    continue;
                }
                if let Some(config) = queue_from_ruleset(ruleset) {
                    hit = true;
                    votes.found.push(config);
                }
            }
            if !hit {
                votes.not_found.push("rulesets".to_owned());
            }
        }
        (None, None) => {
            surfaces.push(SurfaceRead {
                surface: "rulesets".to_owned(),
                query: "GET /repos/{owner}/{repo}/rulesets".to_owned(),
                outcome: SurfaceOutcome::Unreadable {
                    boundary: Boundary::Transport,
                    detail: "not consulted".to_owned(),
                },
            });
            votes
                .unreadable
                .push((Boundary::Transport, "rulesets: not consulted".to_owned()));
        }
    }
}

/// Surface 2: branch protection. It abstains, always, and the report says so
/// in the surface list rather than leaving a reader to infer it.
fn record_protection(inputs: &QueueInputs, surfaces: &mut Vec<SurfaceRead>) {
    let outcome = match &inputs.protection {
        Payload::Json(_) | Payload::NotFound => SurfaceOutcome::Inexpressible {
            detail: "this payload has no merge-queue field; its silence about a queue is not \
                     evidence of absence"
                .to_owned(),
        },
        Payload::Unreadable(boundary, detail) => SurfaceOutcome::Unreadable {
            boundary: *boundary,
            detail: detail.clone(),
        },
        Payload::NotConsulted => return,
    };
    surfaces.push(SurfaceRead {
        surface: "branch_protection".to_owned(),
        query: "GET /repos/{owner}/{repo}/branches/{branch}/protection".to_owned(),
        outcome,
    });
}

/// Surface 3: GraphQL effective state. Corroborates the ruleset read and
/// catches a queue configured somewhere the rulesets call did not cover.
fn read_graphql_queue(inputs: &QueueInputs, surfaces: &mut Vec<SurfaceRead>, votes: &mut Votes) {
    const QUERY: &str = "query { repository { mergeQueue(branch:) } }";
    let outcome = match &inputs.graphql_queue {
        Payload::Json(value) => {
            if value.is_null() {
                votes.not_found.push("graphql_merge_queue".to_owned());
            } else {
                votes.found.push(queue_from_graphql(value));
            }
            SurfaceOutcome::Read
        }
        Payload::NotFound => {
            votes.not_found.push("graphql_merge_queue".to_owned());
            SurfaceOutcome::Read
        }
        Payload::Unreadable(boundary, detail) => {
            votes
                .unreadable
                .push((*boundary, format!("graphql merge queue: {detail}")));
            SurfaceOutcome::Unreadable {
                boundary: *boundary,
                detail: detail.clone(),
            }
        }
        Payload::NotConsulted => return,
    };
    surfaces.push(SurfaceRead {
        surface: "graphql_merge_queue".to_owned(),
        query: QUERY.to_owned(),
        outcome,
    });
}

fn describe_disagreements(votes: &Votes) -> Vec<String> {
    let mut disagreements = Vec::new();
    if !votes.found.is_empty() && !votes.not_found.is_empty() {
        disagreements.push(format!(
            "surfaces disagree: {} reported a merge queue, {} reported none; the queue is \
             reported because a surface that can express one found one, and only a surface that \
             cannot see a queue can be silent about a live one",
            votes
                .found
                .iter()
                .flat_map(|config| config.observed_by.clone())
                .collect::<Vec<_>>()
                .join(", "),
            votes.not_found.join(", ")
        ));
    }
    if !votes.found.is_empty() && !votes.unreadable.is_empty() {
        disagreements.push(format!(
            "reported from a partial read: {} could not be consulted",
            votes
                .unreadable
                .iter()
                .map(|(_, detail)| detail.clone())
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    disagreements
}

/// Fail closed: absence is only assertable when every surface that could have
/// contradicted it was actually read.
fn combine(votes: Votes) -> Verdict<MergeQueueConfig> {
    if let Some(config) = merge_configs(votes.found) {
        return Verdict::Present(config);
    }
    if let Some((boundary, detail)) = strongest_boundary(&votes.unreadable) {
        return Verdict::Unknown {
            boundary,
            detail: format!(
                "no surface reported a merge queue, but {detail} - absence cannot be asserted \
                 from a surface that was not read"
            ),
        };
    }
    if votes.not_found.is_empty() {
        return Verdict::Unknown {
            boundary: Boundary::Transport,
            detail: "no surface capable of expressing a merge queue was consulted".to_owned(),
        };
    }
    Verdict::Absent
}

/// Fold several surfaces' readings of the same queue into one, preferring the
/// richer value field by field.
fn merge_configs(found: Vec<MergeQueueConfig>) -> Option<MergeQueueConfig> {
    let mut iter = found.into_iter();
    let mut merged = iter.next()?;
    for other in iter {
        merged.ruleset_name = merged.ruleset_name.or(other.ruleset_name);
        merged.ruleset_id = merged.ruleset_id.or(other.ruleset_id);
        merged.enforcement = merged.enforcement.or(other.enforcement);
        merged.grouping_strategy = merged.grouping_strategy.or(other.grouping_strategy);
        merged.max_entries_to_merge = merged.max_entries_to_merge.or(other.max_entries_to_merge);
        merged.max_entries_to_build = merged.max_entries_to_build.or(other.max_entries_to_build);
        merged.min_entries_to_merge = merged.min_entries_to_merge.or(other.min_entries_to_merge);
        merged.merge_method = merged.merge_method.or(other.merge_method);
        merged.check_response_timeout_minutes = merged
            .check_response_timeout_minutes
            .or(other.check_response_timeout_minutes);
        for surface in other.observed_by {
            if !merged.observed_by.contains(&surface) {
                merged.observed_by.push(surface);
            }
        }
    }
    Some(merged)
}

/// The boundary worth reporting when several reads failed.
///
/// A permission fact outranks a transport blip: "this token cannot see
/// rulesets" sends the reader somewhere useful, while "the call timed out"
/// sends them to retry.
fn strongest_boundary(unreadable: &[(Boundary, String)]) -> Option<(Boundary, String)> {
    unreadable
        .iter()
        .max_by_key(|(boundary, _)| boundary_rank(*boundary))
        .map(|(boundary, detail)| (*boundary, detail.clone()))
}

/// How useful a boundary is to the reader, highest first.
const fn boundary_rank(boundary: Boundary) -> u8 {
    match boundary {
        Boundary::Permission => 5,
        Boundary::Identity => 4,
        Boundary::Scope => 3,
        Boundary::Grammar => 2,
        Boundary::Parse => 1,
        Boundary::Transport => 0,
    }
}

/// Whether a ruleset's `ref_name` conditions cover the branch being modelled.
///
/// `~DEFAULT_BRANCH` and `~ALL` are GitHub's own aliases; resolving the first
/// needs the repository's default branch, and when that is unknown the alias
/// is treated as covering rather than excluded. An over-inclusive match here
/// reports a queue that may not apply to an unusual base; an under-inclusive
/// one hides a live queue, which is the failure that costs a session.
#[must_use]
pub fn ruleset_targets_branch(ruleset: &Value, base: &str, default_branch: Option<&str>) -> bool {
    if ruleset.get("target").and_then(Value::as_str) == Some("tag") {
        return false;
    }
    let Some(include) = ruleset
        .pointer("/conditions/ref_name/include")
        .and_then(Value::as_array)
    else {
        // No conditions block at all: the ruleset detail call omits it on some
        // org-level rulesets. Treat as covering rather than silently dropping
        // a queue.
        return true;
    };
    let excluded = ruleset
        .pointer("/conditions/ref_name/exclude")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .any(|pattern| ref_pattern_matches(pattern, base, default_branch))
        });
    if excluded {
        return false;
    }
    include
        .iter()
        .filter_map(Value::as_str)
        .any(|pattern| ref_pattern_matches(pattern, base, default_branch))
}

fn ref_pattern_matches(pattern: &str, base: &str, default_branch: Option<&str>) -> bool {
    match pattern {
        "~ALL" => true,
        "~DEFAULT_BRANCH" => default_branch.is_none_or(|branch| branch == base),
        other => {
            let candidate = other.strip_prefix("refs/heads/").unwrap_or(other);
            candidate == base
                || candidate
                    .strip_suffix("**")
                    .or_else(|| candidate.strip_suffix('*'))
                    .is_some_and(|prefix| base.starts_with(prefix))
        }
    }
}

/// Extract a queue config from one full ruleset object, when it carries a
/// `merge_queue` rule.
#[must_use]
pub fn queue_from_ruleset(ruleset: &Value) -> Option<MergeQueueConfig> {
    let rules = ruleset.get("rules").and_then(Value::as_array)?;
    let rule = rules
        .iter()
        .find(|rule| rule.get("type").and_then(Value::as_str) == Some("merge_queue"))?;
    let parameters = rule.get("parameters");
    Some(MergeQueueConfig {
        ruleset_name: ruleset
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        ruleset_id: ruleset.get("id").and_then(Value::as_u64),
        enforcement: ruleset
            .get("enforcement")
            .and_then(Value::as_str)
            .map(str::to_owned),
        grouping_strategy: parameter_str(parameters, "grouping_strategy"),
        max_entries_to_merge: parameter_u64(parameters, "max_entries_to_merge"),
        max_entries_to_build: parameter_u64(parameters, "max_entries_to_build"),
        min_entries_to_merge: parameter_u64(parameters, "min_entries_to_merge"),
        merge_method: parameter_str(parameters, "merge_method"),
        check_response_timeout_minutes: parameter_u64(parameters, "check_response_timeout_minutes"),
        observed_by: vec!["rulesets".to_owned()],
    })
}

/// Extract a queue config from GraphQL `repository.mergeQueue`.
#[must_use]
pub fn queue_from_graphql(value: &Value) -> MergeQueueConfig {
    let configuration = value.get("configuration");
    MergeQueueConfig {
        ruleset_name: None,
        ruleset_id: None,
        enforcement: None,
        grouping_strategy: parameter_str(configuration, "mergingStrategy"),
        max_entries_to_merge: parameter_u64(configuration, "maximumEntriesToMerge"),
        max_entries_to_build: parameter_u64(configuration, "maximumEntriesToBuild"),
        min_entries_to_merge: parameter_u64(configuration, "minimumEntriesToMerge"),
        merge_method: parameter_str(configuration, "mergeMethod"),
        // GraphQL reports this timeout in seconds; every other surface and the
        // web UI use minutes, so it is normalized here rather than printed in
        // a unit the reader has to convert.
        check_response_timeout_minutes: parameter_u64(configuration, "checkResponseTimeout")
            .map(|seconds| seconds / 60),
        observed_by: vec!["graphql_merge_queue".to_owned()],
    }
}

fn parameter_str(parameters: Option<&Value>, key: &str) -> Option<String> {
    parameters?.get(key)?.as_str().map(str::to_owned)
}

fn parameter_u64(parameters: Option<&Value>, key: &str) -> Option<u64> {
    parameters?.get(key)?.as_u64()
}

/// Read strict up-to-date protection, and state its consequence.
///
/// The consequence depends on the queue: with strict on and no queue, every
/// merge invalidates every other open pull request and somebody has to update
/// them one at a time. With a queue, that is the queue's job.
pub fn determine_strict(protection: &Payload, queue: &QueueFinding) -> StrictFinding {
    let verdict = match protection {
        Payload::Json(value) => match value
            .pointer("/required_status_checks/strict")
            .and_then(Value::as_bool)
        {
            Some(strict) => Verdict::Present(strict),
            None => Verdict::Absent,
        },
        Payload::NotFound => Verdict::Absent,
        Payload::Unreadable(boundary, detail) => Verdict::Unknown {
            boundary: *boundary,
            detail: detail.clone(),
        },
        Payload::NotConsulted => Verdict::Unknown {
            boundary: Boundary::Transport,
            detail: "branch protection was not consulted".to_owned(),
        },
    };

    let implication = match (&verdict, &queue.verdict) {
        (Verdict::Present(true), Verdict::Present(_)) => {
            "Strict is ON and a merge queue is live. Every merge makes every other open pull \
             request `behind`, so merging one at a time is a treadmill: each landing forces a \
             full-gate revalidation of the rest. Enqueue instead and let the queue batch them."
                .to_owned()
        }
        (Verdict::Present(true), Verdict::Absent) => {
            "Strict is ON with no merge queue. Every merge makes every other open pull request \
             `behind` and each one must be updated and revalidated individually. Land in \
             dependency order and expect serialized revalidation."
                .to_owned()
        }
        (Verdict::Present(true), Verdict::Unknown { .. }) => {
            "Strict is ON. Whether a merge queue absorbs the resulting revalidation could not be \
             determined; do not assume it does not."
                .to_owned()
        }
        (Verdict::Present(false), _) => {
            "Strict is OFF. A pull request does not have to be up to date with the base to merge, \
             so landing one does not invalidate the others."
                .to_owned()
        }
        (Verdict::Absent, _) => {
            "This branch reports no required-status-check protection, so nothing on GitHub's side \
             requires a check to pass before a merge."
                .to_owned()
        }
        (Verdict::Unknown { .. }, _) => {
            "Up-to-date protection could not be read, so the cost of landing one pull request on \
             the others is unknown."
                .to_owned()
        }
    };

    StrictFinding {
        verdict,
        implication,
    }
}

/// Name the action and the exact command for putting a pull request on the
/// path to merge here.
///
/// The merge method is taken from the queue's own configuration rather than
/// from a convention, because picking the wrong one has consequences beyond
/// the merge: a squash folds a repository's release-marker commit into the
/// squash subject and the release automation that keys on that subject then
/// fires, or fails to, on the wrong commit.
#[must_use]
pub fn enqueue_guidance(queue: &QueueFinding) -> EnqueueGuidance {
    match &queue.verdict {
        Verdict::Present(config) => {
            let method = config.merge_method.as_deref().unwrap_or("MERGE");
            let flag = match method.to_ascii_uppercase().as_str() {
                "SQUASH" => "--squash",
                "REBASE" => "--rebase",
                _ => "--merge",
            };
            let enforcing = config.enforcing();
            EnqueueGuidance {
                action: if enforcing { "enqueue" } else { "merge" }.to_owned(),
                command: Some(format!("gh pr merge <number> --auto {flag}")),
                rationale: if enforcing {
                    format!(
                        "A merge queue is live on this branch and uses {method}. Enabling \
                         auto-merge adds the pull request to the queue; the queue builds and \
                         merges batches of up to {} entries. Do not merge directly, and do not \
                         pass a method other than {flag}: the queue's method is the one the \
                         repository's downstream automation was built around.",
                        config
                            .max_entries_to_merge
                            .map_or_else(|| "?".to_owned(), |value| value.to_string())
                    )
                } else {
                    format!(
                        "A merge queue is configured with enforcement `{}`, so it reports but \
                         does not gate. Merge directly with {flag}.",
                        config.enforcement.as_deref().unwrap_or("unknown")
                    )
                },
            }
        }
        Verdict::Absent => EnqueueGuidance {
            action: "merge".to_owned(),
            command: Some("gh pr merge <number> --auto".to_owned()),
            rationale: "No merge queue is configured on this branch, so a pull request merges \
                        directly once its required checks pass."
                .to_owned(),
        },
        Verdict::Unknown { detail, .. } => EnqueueGuidance {
            action: "unknown".to_owned(),
            command: None,
            rationale: format!(
                "The merge mechanism could not be determined ({detail}). Determine it before \
                 doing bulk work: hand-rebasing a backlog against a live queue is wasted effort, \
                 and enqueuing against a repository with no queue silently does nothing."
            ),
        },
    }
}
