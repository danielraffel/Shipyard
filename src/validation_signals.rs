//! Structured validation signals a repository's CI publishes as check-run
//! annotations, and how Shipyard reads them.
//!
//! A green required check says the job succeeded. It does not say how much the
//! job tested. A repository that runs a narrowed test tier on pull-request
//! heads and the full suite only in the merge queue, or that lets a merge
//! group reuse an earlier run's receipt instead of re-running the suite, has
//! two greens that mean different things. Without a structured signal, the
//! only record of which one happened is log text.
//!
//! This module defines Shipyard's side of a small annotation contract. A job
//! emits a GitHub Actions workflow-command notice whose **title** names the
//! signal and whose **message** is one compact JSON object:
//!
//! ```text
//! ::notice title=shipyard-test-tier::{"schema":"shipyard-test-tier/v1","tier":"fast","selector":"pr-fast","full_suite_runs_in":"merge_group"}
//! ::notice title=shipyard-receipt-decision::{"schema":"shipyard-receipt-decision/v1","target":"macos","verdict":"reuse","reason":"...","source_run_id":"123","selected":10,"passed":10,"skipped":0,"inventory_count":10}
//! ```
//!
//! GitHub turns each notice into a check-run annotation, readable through
//! `GET repos/{o}/{r}/check-runs/{id}/annotations`. Nothing here depends on a
//! particular repository's job names: the titles are the contract.
//!
//! The reading rules are deliberately fail-honest:
//!
//! - no `shipyard-test-tier` annotation means the tier is **unknown**, never
//!   "full";
//! - an unrecognised tier value is displayed verbatim, never rejected;
//! - a malformed message or a wrong schema is reported as unparseable, never
//!   dropped and never a crash;
//! - an unreadable annotation endpoint (403, 404, transport error) is reported
//!   as unknown with the reason.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Annotation title carrying the test tier a job ran.
pub const TEST_TIER_TITLE: &str = "shipyard-test-tier";
/// Schema identifier inside a [`TEST_TIER_TITLE`] message.
pub const TEST_TIER_SCHEMA: &str = "shipyard-test-tier/v1";
/// Annotation title carrying one receipt-reuse decision.
pub const RECEIPT_DECISION_TITLE: &str = "shipyard-receipt-decision";
/// Schema identifier inside a [`RECEIPT_DECISION_TITLE`] message.
pub const RECEIPT_DECISION_SCHEMA: &str = "shipyard-receipt-decision/v1";

/// Tier value that means the full suite ran.
pub const FULL_TIER: &str = "full";
/// Tier reported when no check published one.
pub const UNKNOWN_TIER: &str = "unknown";

/// Upper bound on annotation reads for the required checks of one head.
pub const MAX_HEAD_ANNOTATION_READS: usize = 20;
/// Upper bound on annotation reads for the check runs of one merge group.
pub const MAX_MERGE_GROUP_ANNOTATION_READS: usize = 30;
/// Upper bound on check-run pages read for one merge-group commit.
const MAX_CHECK_RUN_PAGES: u32 = 3;

/// A `gh`-shaped reader: takes the argument vector after `gh`, returns stdout
/// or an error description. Kept as a closure so callers pick the transport
/// and tests need no subprocess.
pub type GhReader<'a> = dyn Fn(&[String]) -> Result<String, String> + 'a;

/// One annotation, reduced to the two fields the contract uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Annotation {
    /// Annotation title.
    pub title: String,
    /// Annotation message.
    pub message: String,
}

/// Parse annotations from either the REST array shape or a GraphQL
/// `annotations { nodes { title message } }` connection.
#[must_use]
pub fn parse_annotations(value: &Value) -> Vec<Annotation> {
    let items = value
        .as_array()
        .or_else(|| value.get("nodes").and_then(Value::as_array));
    items
        .into_iter()
        .flatten()
        .map(|item| Annotation {
            title: item
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            message: item
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        })
        .collect()
}

/// What one check said about its test tier.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TierReading {
    /// A well-formed `shipyard-test-tier/v1` annotation was found.
    Reported {
        /// Tier value, verbatim (`fast`, `full`, or anything a future
        /// producer emits).
        tier: String,
        /// Label selector the tier ran, when given.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        /// Where the full suite runs instead, when this tier is narrowed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        full_suite_runs_in: Option<String>,
    },
    /// No tier could be read. Never to be read as "full".
    Unknown {
        /// Why the tier is unknown.
        reason: String,
    },
    /// A tier annotation was present but could not be parsed.
    Unparseable {
        /// The annotation message as published.
        raw: String,
        /// What was wrong with it.
        error: String,
    },
}

impl TierReading {
    /// The tier value when one was reported.
    #[must_use]
    pub fn tier(&self) -> Option<&str> {
        match self {
            Self::Reported { tier, .. } => Some(tier),
            _ => None,
        }
    }

    /// One-line human rendering.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Reported {
                tier,
                selector,
                full_suite_runs_in,
            } => {
                let mut text = format!("tier {tier}");
                if let Some(selector) = selector {
                    let _ = write!(text, " (selector {selector})");
                }
                if let Some(runs_in) = full_suite_runs_in {
                    let _ = write!(text, "; full suite runs in {runs_in}");
                }
                text
            }
            Self::Unknown { reason } => format!("tier unknown ({reason})"),
            Self::Unparseable { raw, error } => {
                format!("tier annotation UNPARSEABLE ({error}): {}", clip(raw))
            }
        }
    }
}

/// Parse one `shipyard-test-tier` message.
#[must_use]
pub fn parse_test_tier(message: &str) -> TierReading {
    let object = match parse_schema_object(message, TEST_TIER_SCHEMA) {
        Ok(object) => object,
        Err(error) => {
            return TierReading::Unparseable {
                raw: message.to_owned(),
                error,
            };
        }
    };
    let Some(tier) = object
        .get("tier")
        .and_then(Value::as_str)
        .filter(|tier| !tier.trim().is_empty())
    else {
        return TierReading::Unparseable {
            raw: message.to_owned(),
            error: "missing string field `tier`".to_owned(),
        };
    };
    TierReading::Reported {
        tier: tier.to_owned(),
        selector: optional_text(&object, "selector"),
        full_suite_runs_in: optional_text(&object, "full_suite_runs_in"),
    }
}

/// The tier a set of annotations reports. The last tier annotation wins, so a
/// step that re-emits after widening its tier is read as the wider tier.
#[must_use]
pub fn tier_from_annotations(annotations: &[Annotation]) -> TierReading {
    annotations
        .iter()
        .rev()
        .find(|annotation| annotation.title == TEST_TIER_TITLE)
        .map_or_else(
            || TierReading::Unknown {
                reason: format!("no {TEST_TIER_TITLE} annotation"),
            },
            |annotation| parse_test_tier(&annotation.message),
        )
}

/// One receipt-reuse decision.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReceiptDecision {
    /// A well-formed `shipyard-receipt-decision/v1` annotation.
    Parsed {
        /// Target the decision is about.
        target: String,
        /// `reuse`, `refuse`, or a future value kept verbatim.
        verdict: String,
        /// Why.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Run whose receipt was reused, when one was.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_run_id: Option<String>,
        /// Tests the receipt's run selected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected: Option<u64>,
        /// Tests that passed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        passed: Option<u64>,
        /// Tests that were skipped.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skipped: Option<u64>,
        /// Size of the full test inventory, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inventory_count: Option<u64>,
        /// Check run that published the decision.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        check: Option<String>,
    },
    /// A decision annotation was present but could not be parsed.
    Unparseable {
        /// The annotation message as published.
        raw: String,
        /// What was wrong with it.
        error: String,
        /// Check run that published it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        check: Option<String>,
    },
}

impl ReceiptDecision {
    /// One-line human rendering.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Parsed {
                target,
                verdict,
                reason,
                source_run_id,
                selected,
                passed,
                skipped,
                ..
            } => match verdict.as_str() {
                "reuse" => format!(
                    "{target}: reused receipt from run {}: {} selected / {} passed ({} skipped)",
                    source_run_id.as_deref().unwrap_or("UNKNOWN"),
                    count(*selected),
                    count(*passed),
                    count(*skipped),
                ),
                "refuse" => format!(
                    "{target}: validated in full: receipt refused because {}",
                    reason.as_deref().unwrap_or("no reason given")
                ),
                other => format!(
                    "{target}: verdict {other}{}",
                    reason
                        .as_deref()
                        .map_or_else(String::new, |reason| format!(": {reason}"))
                ),
            },
            Self::Unparseable { raw, error, check } => format!(
                "receipt decision UNPARSEABLE{} ({error}): {}",
                check
                    .as_deref()
                    .map_or_else(String::new, |check| format!(" on {check}")),
                clip(raw)
            ),
        }
    }
}

/// Parse one `shipyard-receipt-decision` message.
#[must_use]
pub fn parse_receipt_decision(message: &str, check: Option<&str>) -> ReceiptDecision {
    let unparseable = |error: String| ReceiptDecision::Unparseable {
        raw: message.to_owned(),
        error,
        check: check.map(str::to_owned),
    };
    let object = match parse_schema_object(message, RECEIPT_DECISION_SCHEMA) {
        Ok(object) => object,
        Err(error) => return unparseable(error),
    };
    let Some(target) = optional_text(&object, "target") else {
        return unparseable("missing string field `target`".to_owned());
    };
    let Some(verdict) = optional_text(&object, "verdict") else {
        return unparseable("missing string field `verdict`".to_owned());
    };
    let mut counts = [None; 4];
    for (slot, field) in counts
        .iter_mut()
        .zip(["selected", "passed", "skipped", "inventory_count"])
    {
        match object.get(field) {
            None | Some(Value::Null) => {}
            Some(value) => match value.as_u64() {
                Some(number) => *slot = Some(number),
                None => {
                    return unparseable(format!(
                        "field `{field}` must be a non-negative integer or null"
                    ));
                }
            },
        }
    }
    let source_run_id = match object.get("source_run_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => Some(number.to_string()),
        Some(_) => {
            return unparseable("field `source_run_id` must be a string, number, or null".into());
        }
    };
    let [selected, passed, skipped, inventory_count] = counts;
    ReceiptDecision::Parsed {
        target,
        verdict,
        reason: optional_text(&object, "reason"),
        source_run_id,
        selected,
        passed,
        skipped,
        inventory_count,
        check: check.map(str::to_owned),
    }
}

/// Every receipt decision in a set of annotations, in publication order.
#[must_use]
pub fn receipt_decisions_from_annotations(
    annotations: &[Annotation],
    check: Option<&str>,
) -> Vec<ReceiptDecision> {
    annotations
        .iter()
        .filter(|annotation| annotation.title == RECEIPT_DECISION_TITLE)
        .map(|annotation| parse_receipt_decision(&annotation.message, check))
        .collect()
}

/// The tier one check reported.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CheckTier {
    /// Check-run name (or status-context name).
    pub check: String,
    /// Check-run database id, when the context is a check run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_run_id: Option<u64>,
    /// Lower-case lifecycle status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Lower-case conclusion when completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    /// What the check said about its tier.
    pub reading: TierReading,
}

/// The tier verdict for a set of checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TierVerdict {
    /// `full`, `unknown`, or the narrowed tier verbatim (for example `fast`).
    pub tier: String,
    /// Selector of the narrowed tier, when one reported it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    /// Where the full suite runs instead, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_suite_runs_in: Option<String>,
    /// Checks that reported a tier.
    pub reported: usize,
    /// Checks that reported none, or whose tier was unreadable.
    pub unreported: usize,
    /// Checks whose tier annotation could not be parsed.
    pub unparseable: usize,
    /// One sentence for humans.
    pub summary: String,
}

/// Collapse per-check tiers into one verdict.
///
/// A narrowed tier anywhere wins over `full`: the weakest reported evidence is
/// the honest headline. With no reported tier at all the verdict is
/// [`UNKNOWN_TIER`], never `full`.
#[must_use]
pub fn summarize_tiers(checks: &[CheckTier]) -> TierVerdict {
    let reported: Vec<&CheckTier> = checks
        .iter()
        .filter(|check| check.reading.tier().is_some())
        .collect();
    let unparseable = checks
        .iter()
        .filter(|check| matches!(check.reading, TierReading::Unparseable { .. }))
        .count();
    let unreported = checks.len() - reported.len();
    let narrowed = reported
        .iter()
        .find(|check| check.reading.tier() != Some(FULL_TIER));
    let (tier, selector, full_suite_runs_in, summary) = match (reported.is_empty(), narrowed) {
        (true, _) => (
            UNKNOWN_TIER.to_owned(),
            None,
            None,
            format!(
                "test tier unknown: no required check published a {TEST_TIER_TITLE} annotation; \
                 do not assume the full suite ran"
            ),
        ),
        (false, Some(check)) => {
            let TierReading::Reported {
                tier,
                selector,
                full_suite_runs_in,
            } = &check.reading
            else {
                unreachable!("filtered to reported tiers")
            };
            let mut summary = format!("validated on the {tier} tier");
            if let Some(selector) = selector {
                let _ = write!(summary, " ({selector})");
            }
            let _ = write!(summary, " by {}", check.check);
            match full_suite_runs_in {
                Some(runs_in) => {
                    let _ = write!(
                        summary,
                        "; the full suite runs in {runs_in}, not on this head"
                    );
                }
                None => summary.push_str("; this is not full validation"),
            }
            (
                tier.clone(),
                selector.clone(),
                full_suite_runs_in.clone(),
                summary,
            )
        }
        (false, None) => {
            let names: BTreeSet<&str> = reported.iter().map(|check| check.check.as_str()).collect();
            let mut summary = format!(
                "fully tested (tier full, reported by {})",
                names.into_iter().collect::<Vec<_>>().join(", ")
            );
            if unreported > 0 {
                let _ = write!(
                    summary,
                    "; {unreported} other required check(s) reported no tier"
                );
            }
            (FULL_TIER.to_owned(), None, None, summary)
        }
    };
    TierVerdict {
        tier,
        selector,
        full_suite_runs_in,
        reported: reported.len(),
        unreported,
        unparseable,
        summary,
    }
}

/// Where a merge-group SHA came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeGroupSource {
    /// The pull request's live merge-queue entry.
    MergeQueueEntry,
    /// The merge commit of a merged pull request (a merge-queue landing puts
    /// the group's head commit on the base branch).
    MergeCommit,
}

/// Receipt decisions and tiers observed on one merge group.
#[derive(Clone, Debug, Serialize)]
pub struct MergeGroupSignals {
    /// Merge-group head SHA.
    pub sha: String,
    /// Where the SHA came from.
    pub source: MergeGroupSource,
    /// `read`, `no_merge_group_runs`, or `unreadable`.
    pub status: &'static str,
    /// Detail for a non-`read` status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// `merge_group` workflow runs found on the SHA.
    pub merge_group_runs: Vec<u64>,
    /// Every receipt decision published by those runs' check runs.
    pub receipt_decisions: Vec<ReceiptDecision>,
    /// Tiers those check runs published.
    pub test_tier: Vec<CheckTier>,
    /// Whether the read hit a bound, so the lists may be incomplete.
    pub truncated: bool,
}

impl MergeGroupSignals {
    /// The one-line answer to "did this merge group run tests?".
    #[must_use]
    pub fn headline(&self) -> String {
        if self.status != "read" {
            return format!(
                "{}: {}",
                self.status.replace('_', " "),
                self.detail.as_deref().unwrap_or("no detail")
            );
        }
        if self.receipt_decisions.is_empty() && self.test_tier.is_empty() {
            return format!(
                "no {RECEIPT_DECISION_TITLE} or {TEST_TIER_TITLE} annotations: whether this \
                 merge group ran tests is UNKNOWN from structured evidence"
            );
        }
        let reused = self.receipt_decisions.iter().any(|decision| {
            matches!(decision, ReceiptDecision::Parsed { verdict, .. } if verdict == "reuse")
        });
        let any_refused = self.receipt_decisions.iter().any(|decision| {
            matches!(decision, ReceiptDecision::Parsed { verdict, .. } if verdict == "refuse")
        });
        let ran_full = any_refused
            || self
                .test_tier
                .iter()
                .any(|check| check.reading.tier() == Some(FULL_TIER));
        match (reused, ran_full) {
            (true, true) => "mixed: some targets reused a receipt, some ran the full suite".into(),
            (true, false) => "receipt reused: this merge group did not re-run the suite".into(),
            (false, true) => "full suite ran in this merge group".into(),
            (false, false) => "no target reported reuse or a full-tier run; see decisions".into(),
        }
    }
}

/// Validation signals for one pull request.
#[derive(Clone, Debug, Serialize)]
pub struct PrValidationSignals {
    /// Head SHA read.
    pub head_sha: Option<String>,
    /// Aggregate state of the head's required checks: `green`, `pending`,
    /// `failing`, `no_required_checks`, or `unknown`.
    pub required_state: &'static str,
    /// Per-required-check tiers.
    pub test_tier: Vec<CheckTier>,
    /// Collapsed tier verdict.
    pub test_tier_verdict: TierVerdict,
    /// Merge groups related to the pull request.
    pub merge_groups: Vec<MergeGroupSignals>,
    /// Read problems worth printing.
    pub warnings: Vec<String>,
    /// API calls this read cost.
    pub api_calls: u32,
}

impl PrValidationSignals {
    /// The headline distinguishing fast-tier green from full validation.
    #[must_use]
    pub fn headline(&self) -> String {
        let head = self.head_sha.as_deref().map_or_else(
            || "PR head".to_owned(),
            |sha| format!("PR head {}", short(sha)),
        );
        let verdict = &self.test_tier_verdict;
        match self.required_state {
            "green" if verdict.tier == FULL_TIER => format!("{head} GREEN and {}", verdict.summary),
            "green" if verdict.tier == UNKNOWN_TIER => {
                format!("{head} GREEN; {}", verdict.summary)
            }
            "green" => format!(
                "{head} GREEN on the {} tier, NOT full validation: {}",
                verdict.tier, verdict.summary
            ),
            other => format!(
                "{head} required checks {}; {}",
                other.to_uppercase().replace('_', " "),
                verdict.summary
            ),
        }
    }
}

/// GraphQL read of one pull request's head, required contexts and merge refs.
pub const PR_VALIDATION_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){headRefOid mergeCommit{oid} mergeQueueEntry{headCommit{oid}} commits(last:1){nodes{commit{oid statusCheckRollup{contexts(first:100){pageInfo{hasNextPage} nodes{__typename ... on CheckRun{databaseId name status conclusion isRequired(pullRequestNumber:$number)} ... on StatusContext{context state isRequired(pullRequestNumber:$number)}}}}}}}}}}";

/// Read the validation signals for one pull request.
#[must_use]
pub fn gather_pr(gh: &GhReader<'_>, repo: &str, pr: u64) -> PrValidationSignals {
    let mut signals = PrValidationSignals {
        head_sha: None,
        required_state: "unknown",
        test_tier: Vec::new(),
        test_tier_verdict: summarize_tiers(&[]),
        merge_groups: Vec::new(),
        warnings: Vec::new(),
        api_calls: 0,
    };
    let pull = match read_pull_request(gh, repo, pr, &mut signals.api_calls) {
        Ok(pull) => pull,
        Err(warning) => {
            signals.warnings.push(warning);
            return signals;
        }
    };
    signals.head_sha = pull
        .get("headRefOid")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let commit = pull.pointer("/commits/nodes/0/commit");
    if commit
        .and_then(|commit| commit.get("oid"))
        .and_then(Value::as_str)
        != signals.head_sha.as_deref()
    {
        signals
            .warnings
            .push("last commit does not match headRefOid; tier read may be stale".to_owned());
    }
    let contexts = commit.and_then(|commit| commit.pointer("/statusCheckRollup/contexts"));
    if contexts
        .and_then(|contexts| contexts.pointer("/pageInfo/hasNextPage"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        signals.warnings.push(
            "head has more than 100 check contexts; required checks past the first 100 were \
             not read"
                .to_owned(),
        );
    }
    let required = latest_required_contexts(contexts);
    signals.required_state = required_state(&required);
    signals.test_tier = read_required_tiers(gh, repo, required, &mut signals.api_calls);
    signals.test_tier_verdict = summarize_tiers(&signals.test_tier);

    let queue_sha = pull
        .pointer("/mergeQueueEntry/headCommit/oid")
        .and_then(Value::as_str);
    let merge_sha = pull.pointer("/mergeCommit/oid").and_then(Value::as_str);
    for (sha, source) in [
        (queue_sha, MergeGroupSource::MergeQueueEntry),
        (merge_sha, MergeGroupSource::MergeCommit),
    ] {
        if let Some(sha) = sha {
            let (group, calls) = gather_merge_group(gh, repo, sha, source);
            signals.api_calls += calls;
            signals.merge_groups.push(group);
        }
    }
    signals
}

fn read_pull_request(
    gh: &GhReader<'_>,
    repo: &str,
    pr: u64,
    calls: &mut u32,
) -> Result<Value, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("repo `{repo}` is not OWNER/REPO"))?;
    *calls += 1;
    let body = gh(&[
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={PR_VALIDATION_QUERY}"),
        "-F".to_owned(),
        format!("owner={owner}"),
        "-F".to_owned(),
        format!("name={name}"),
        "-F".to_owned(),
        format!("number={pr}"),
    ])
    .and_then(|raw| serde_json::from_str::<Value>(&raw).map_err(|error| error.to_string()))
    .map_err(|error| format!("pull request read failed: {error}"))?;
    body.pointer("/data/repository/pullRequest")
        .filter(|pull| !pull.is_null())
        .cloned()
        .ok_or_else(|| {
            format!(
                "pull request read returned no data: {}",
                clip(
                    &body
                        .get("errors")
                        .map_or_else(String::new, Value::to_string)
                )
            )
        })
}

fn read_required_tiers(
    gh: &GhReader<'_>,
    repo: &str,
    required: Vec<RequiredContext>,
    calls: &mut u32,
) -> Vec<CheckTier> {
    let mut reads = 0usize;
    required
        .into_iter()
        .map(|context| {
            let reading = match context.check_run_id {
                None => TierReading::Unknown {
                    reason: "commit status context, which carries no annotations".to_owned(),
                },
                Some(_) if reads >= MAX_HEAD_ANNOTATION_READS => TierReading::Unknown {
                    reason: format!("annotation read bound ({MAX_HEAD_ANNOTATION_READS}) reached"),
                },
                Some(id) => {
                    reads += 1;
                    *calls += 1;
                    match read_annotations(gh, repo, id) {
                        Ok(annotations) => tier_from_annotations(&annotations),
                        Err(error) => TierReading::Unknown {
                            reason: format!("annotations unreadable: {}", clip(&error)),
                        },
                    }
                }
            };
            CheckTier {
                check: context.name,
                check_run_id: context.check_run_id,
                status: Some(context.status),
                conclusion: context.conclusion,
                reading,
            }
        })
        .collect()
}

/// Read the receipt decisions and tiers published on one merge-group SHA.
///
/// Only check runs belonging to `merge_group` workflow runs are read, and only
/// those GitHub reports as carrying annotations, so a merge commit that later
/// also ran push workflows on the base branch does not pollute the answer.
/// Returns the signals and the API calls spent.
#[must_use]
pub fn gather_merge_group(
    gh: &GhReader<'_>,
    repo: &str,
    sha: &str,
    source: MergeGroupSource,
) -> (MergeGroupSignals, u32) {
    let mut calls = 0u32;
    let mut group = MergeGroupSignals {
        sha: sha.to_owned(),
        source,
        status: "read",
        detail: None,
        merge_group_runs: Vec::new(),
        receipt_decisions: Vec::new(),
        test_tier: Vec::new(),
        truncated: false,
    };
    calls += 1;
    let suites = match read_merge_group_suites(gh, repo, sha, &mut group.merge_group_runs) {
        Ok(suites) => suites,
        Err(error) => {
            group.status = "unreadable";
            group.detail = Some(format!("workflow runs unreadable: {}", clip(&error)));
            return (group, calls);
        }
    };
    if group.merge_group_runs.is_empty() {
        group.status = "no_merge_group_runs";
        group.detail = Some(format!(
            "no merge_group workflow runs on {} (not landed through a merge queue, batched \
             into a later group, or runs expired)",
            short(sha)
        ));
        return (group, calls);
    }
    let candidates = match annotated_group_check_runs(gh, repo, sha, &suites, &mut calls) {
        Ok((candidates, truncated)) => {
            group.truncated = truncated;
            candidates
        }
        Err(error) => {
            group.status = "unreadable";
            group.detail = Some(format!("check runs unreadable: {}", clip(&error)));
            return (group, calls);
        }
    };
    if candidates.len() > MAX_MERGE_GROUP_ANNOTATION_READS {
        group.truncated = true;
    }
    for run in candidates.iter().take(MAX_MERGE_GROUP_ANNOTATION_READS) {
        let Some(id) = run.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let name = run.get("name").and_then(Value::as_str).unwrap_or("");
        calls += 1;
        let annotations = match read_annotations(gh, repo, id) {
            Ok(annotations) => annotations,
            Err(error) => {
                group.truncated = true;
                group.detail = Some(format!(
                    "annotations for check run {id} ({name}) unreadable: {}",
                    clip(&error)
                ));
                continue;
            }
        };
        group
            .receipt_decisions
            .extend(receipt_decisions_from_annotations(&annotations, Some(name)));
        if annotations
            .iter()
            .any(|annotation| annotation.title == TEST_TIER_TITLE)
        {
            group.test_tier.push(CheckTier {
                check: name.to_owned(),
                check_run_id: Some(id),
                status: lower(run, "status"),
                conclusion: lower(run, "conclusion"),
                reading: tier_from_annotations(&annotations),
            });
        }
    }
    (group, calls)
}

/// The check-suite ids of the `merge_group` workflow runs on `sha`, recording
/// each run id into `runs`.
fn read_merge_group_suites(
    gh: &GhReader<'_>,
    repo: &str,
    sha: &str,
    runs: &mut Vec<u64>,
) -> Result<BTreeSet<u64>, String> {
    let body = read_json(
        gh,
        &format!("repos/{repo}/actions/runs?head_sha={sha}&event=merge_group&per_page=100"),
    )?;
    let mut suites = BTreeSet::new();
    for run in body
        .get("workflow_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = run.get("id").and_then(Value::as_u64) {
            runs.push(id);
        }
        if let Some(suite) = run.get("check_suite_id").and_then(Value::as_u64) {
            suites.insert(suite);
        }
    }
    Ok(suites)
}

/// Check runs on `sha` that belong to one of `suites` and carry annotations.
/// The flag is true when the page bound stopped the listing.
fn annotated_group_check_runs(
    gh: &GhReader<'_>,
    repo: &str,
    sha: &str,
    suites: &BTreeSet<u64>,
    calls: &mut u32,
) -> Result<(Vec<Value>, bool), String> {
    let mut candidates = Vec::new();
    for page in 1..=MAX_CHECK_RUN_PAGES {
        *calls += 1;
        let body = read_json(
            gh,
            &format!("repos/{repo}/commits/{sha}/check-runs?per_page=100&page={page}"),
        )?;
        let runs = body
            .get("check_runs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let listed = runs.len();
        candidates.extend(runs.into_iter().filter(|run| {
            let in_group = run
                .pointer("/check_suite/id")
                .and_then(Value::as_u64)
                .is_some_and(|suite| suites.contains(&suite));
            let annotated = run
                .pointer("/output/annotations_count")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0;
            in_group && annotated
        }));
        let total = body.get("total_count").and_then(Value::as_u64).unwrap_or(0);
        if listed < 100 || u64::from(page) * 100 >= total {
            return Ok((candidates, false));
        }
    }
    Ok((candidates, true))
}

fn lower(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
}

/// Collect the receipt decisions and tiers carried by GraphQL check-run nodes
/// that requested `annotations { nodes { title message } }`.
#[must_use]
pub fn signals_from_graphql_contexts(
    contexts: Option<&Value>,
) -> (Vec<ReceiptDecision>, Vec<CheckTier>) {
    let mut decisions = Vec::new();
    let mut tiers = Vec::new();
    for node in contexts
        .and_then(|contexts| contexts.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(annotations) = node.get("annotations") else {
            continue;
        };
        let annotations = parse_annotations(annotations);
        let name = node.get("name").and_then(Value::as_str).unwrap_or("");
        decisions.extend(receipt_decisions_from_annotations(&annotations, Some(name)));
        if annotations
            .iter()
            .any(|annotation| annotation.title == TEST_TIER_TITLE)
        {
            tiers.push(CheckTier {
                check: name.to_owned(),
                check_run_id: node.get("databaseId").and_then(Value::as_u64),
                status: node
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_ascii_lowercase),
                conclusion: node
                    .get("conclusion")
                    .and_then(Value::as_str)
                    .map(str::to_ascii_lowercase),
                reading: tier_from_annotations(&annotations),
            });
        }
    }
    (decisions, tiers)
}

struct RequiredContext {
    name: String,
    check_run_id: Option<u64>,
    status: String,
    conclusion: Option<String>,
}

/// Required contexts, collapsed to the newest instance of each name.
fn latest_required_contexts(contexts: Option<&Value>) -> Vec<RequiredContext> {
    let mut latest: std::collections::BTreeMap<String, RequiredContext> =
        std::collections::BTreeMap::new();
    for node in contexts
        .and_then(|contexts| contexts.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if node.get("isRequired").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let context = if node.get("__typename").and_then(Value::as_str) == Some("StatusContext") {
            let state = node
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let terminal = matches!(state.as_str(), "success" | "failure" | "error");
            RequiredContext {
                name: node
                    .get("context")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                check_run_id: None,
                status: if terminal {
                    "completed".into()
                } else {
                    state.clone()
                },
                conclusion: terminal.then_some(state),
            }
        } else {
            RequiredContext {
                name: node
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                check_run_id: node.get("databaseId").and_then(Value::as_u64),
                status: node
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_ascii_lowercase(),
                conclusion: node
                    .get("conclusion")
                    .and_then(Value::as_str)
                    .map(str::to_ascii_lowercase),
            }
        };
        let newer = latest
            .get(&context.name)
            .is_none_or(|existing| context.check_run_id > existing.check_run_id);
        if newer {
            latest.insert(context.name.clone(), context);
        }
    }
    latest.into_values().collect()
}

fn required_state(contexts: &[RequiredContext]) -> &'static str {
    if contexts.is_empty() {
        return "no_required_checks";
    }
    let mut pending = false;
    for context in contexts {
        match context.conclusion.as_deref() {
            Some("success" | "skipped" | "neutral") => {}
            Some(_) => return "failing",
            None => pending = true,
        }
    }
    if pending { "pending" } else { "green" }
}

fn read_annotations(
    gh: &GhReader<'_>,
    repo: &str,
    check_run_id: u64,
) -> Result<Vec<Annotation>, String> {
    read_json(
        gh,
        &format!("repos/{repo}/check-runs/{check_run_id}/annotations?per_page=100"),
    )
    .map(|value| parse_annotations(&value))
}

fn read_json(gh: &GhReader<'_>, path: &str) -> Result<Value, String> {
    let raw = gh(&["api".to_owned(), path.to_owned()])?;
    serde_json::from_str(&raw).map_err(|error| format!("malformed JSON from {path}: {error}"))
}

fn parse_schema_object(
    message: &str,
    schema: &str,
) -> Result<serde_json::Map<String, Value>, String> {
    let value: Value = serde_json::from_str(message.trim())
        .map_err(|error| format!("message is not JSON: {error}"))?;
    let Value::Object(object) = value else {
        return Err("message is not a JSON object".to_owned());
    };
    match object.get("schema").and_then(Value::as_str) {
        Some(found) if found == schema => Ok(object),
        Some(found) => Err(format!("schema `{found}` is not `{schema}`")),
        None => Err(format!("missing `schema` (expected `{schema}`)")),
    }
}

fn optional_text(object: &serde_json::Map<String, Value>, field: &str) -> Option<String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

fn count(value: Option<u64>) -> String {
    value.map_or_else(|| "?".to_owned(), |value| value.to_string())
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn clip(text: &str) -> String {
    const LIMIT: usize = 200;
    if text.chars().count() <= LIMIT {
        text.to_owned()
    } else {
        format!("{}...", text.chars().take(LIMIT).collect::<String>())
    }
}

#[cfg(test)]
mod tests;
