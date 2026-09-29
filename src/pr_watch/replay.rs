//! Offline simulation: run [`evaluate`] at every tick of a past window, fold
//! each tick into a throwaway [`Ledger`] exactly as a live scan would, and
//! report every flag episode, whether it would have reached a digest, and
//! whether caller expectations and the clean-landing control hold.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use super::digest::{self, DigestPolicy};
use super::flags::{FlagKind, Thresholds, evaluate};
use super::ledger::{Ledger, PrNow, reconcile};
use super::{QueueEventKind, RepoHistory, head_at, open_at, outcomes_for};

/// Report schema.
pub const REPLAY_SCHEMA: &str = "shipyard.pr-watch.replay/v1";

/// `--expect 8933=1,3,4,5`: these kinds must be raised on this pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expectation {
    /// Pull request.
    pub pr: u64,
    /// Kinds that must be raised at some tick.
    pub kinds: BTreeSet<FlagKind>,
}

impl std::str::FromStr for Expectation {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (pr, kinds) = value
            .split_once('=')
            .ok_or_else(|| format!("expectation {value:?} must look like 8933=1,3,4,5"))?;
        let pr: u64 = pr
            .trim()
            .trim_start_matches('#')
            .parse()
            .map_err(|_| format!("expectation {value:?}: bad pull request number"))?;
        let mut set = BTreeSet::new();
        for part in kinds.split(',').filter(|part| !part.trim().is_empty()) {
            let number: u8 = part
                .trim()
                .parse()
                .map_err(|_| format!("expectation {value:?}: bad flag {part:?}"))?;
            set.insert(
                FlagKind::from_number(number)
                    .ok_or_else(|| format!("expectation {value:?}: no flag {number}"))?,
            );
        }
        if set.is_empty() {
            return Err(format!("expectation {value:?} names no flags"));
        }
        Ok(Self { pr, kinds: set })
    }
}

/// Replay settings.
#[derive(Clone, Debug)]
pub struct ReplayOptions {
    /// Evaluation step.
    pub tick: Duration,
    /// Rule thresholds.
    pub thresholds: Thresholds,
    /// Digest timing.
    pub digest: DigestPolicy,
    /// Expectations to check.
    pub expectations: Vec<Expectation>,
    /// Check that no cleanly merged pull request was flagged.
    pub control_merged_clean: bool,
}

/// One continuous flag episode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Episode {
    /// Pull request.
    pub pr: u64,
    /// Spec flag number.
    pub flag: u8,
    /// Kind.
    pub kind: String,
    /// Discriminator.
    pub key: String,
    /// Verdict at the last tick it held.
    pub verdict: String,
    /// Evidence when first seen.
    pub first_evidence: String,
    /// Evidence at the last tick it held.
    pub last_evidence: String,
    /// Head.
    pub head_sha: String,
    /// First tick.
    pub first_seen_at: DateTime<Utc>,
    /// Last tick.
    pub last_seen_at: DateTime<Utc>,
    /// Ticks it held.
    pub ticks: u64,
    /// When a digest would have carried it.
    pub digested_at: Option<DateTime<Utc>>,
    /// Why it ended, if it did.
    pub addressed_reason: Option<String>,
}

/// One expectation's result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExpectationResult {
    /// Pull request.
    pub pr: u64,
    /// Expected flag numbers.
    pub expected: Vec<u8>,
    /// Raised flag numbers.
    pub raised: Vec<u8>,
    /// Expected but not raised.
    pub missing: Vec<u8>,
    /// Passed.
    pub pass: bool,
}

/// Clean-landing control result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ControlResult {
    /// Pull requests that merged in the window with no failed required job
    /// and no `failed_checks` removal.
    pub clean_merged: Vec<u64>,
    /// Episodes raised on them.
    pub violations: Vec<Episode>,
    /// Passed.
    pub pass: bool,
}

/// The replay report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReplayReport {
    /// [`REPLAY_SCHEMA`].
    pub schema: String,
    /// `OWNER/REPO`.
    pub repo: String,
    /// Window start.
    pub from: DateTime<Utc>,
    /// Window end.
    pub to: DateTime<Utc>,
    /// Step, minutes.
    pub tick_minutes: i64,
    /// Ticks evaluated.
    pub ticks: u64,
    /// Pull requests in the history.
    pub prs: usize,
    /// Required checks used.
    pub required_checks: Vec<String>,
    /// Every episode.
    pub episodes: Vec<Episode>,
    /// Expectation results.
    pub expectations: Vec<ExpectationResult>,
    /// Control result.
    pub control: Option<ControlResult>,
    /// Digests that would have been sent.
    pub digests: u64,
    /// Read gaps.
    pub gaps: Vec<String>,
    /// Everything passed.
    pub pass: bool,
}

fn pr_now(history: &RepoHistory, at: DateTime<Utc>) -> BTreeMap<u64, PrNow> {
    history
        .prs
        .values()
        .map(|pr| {
            let open = open_at(pr, at);
            (
                pr.number,
                PrNow {
                    open,
                    merged: pr.merged_at.is_some_and(|merged| merged <= at),
                    head_sha: head_at(pr, at).map_or_else(String::new, |head| head.sha.clone()),
                    acknowledged: false,
                    title: pr.title.clone(),
                    url: pr.url.clone(),
                },
            )
        })
        .collect()
}

/// Pull requests merged in the window with no failed required job (heads or
/// named merge groups) and no `failed_checks` removal.
#[must_use]
pub fn clean_merged(history: &RepoHistory) -> Vec<u64> {
    history
        .prs
        .values()
        .filter(|pr| {
            pr.merged_at
                .is_some_and(|merged| merged >= history.from && merged < history.to)
        })
        .filter(|pr| {
            !outcomes_for(history, pr.number)
                .iter()
                .any(|record| record.check.failed())
        })
        .filter(|pr| {
            !pr.events.iter().any(|event| {
                matches!(&event.kind, QueueEventKind::Removed { reason } if reason == "failed_checks")
            })
        })
        .map(|pr| pr.number)
        .collect()
}

/// Run the simulation.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn replay(history: &RepoHistory, options: &ReplayOptions) -> ReplayReport {
    let mut ledger = Ledger::new(&history.repo, &history.base);
    let mut episodes: BTreeMap<(String, DateTime<Utc>), Episode> = BTreeMap::new();
    let mut ticks = 0_u64;
    let mut digests = 0_u64;
    let mut at = history.from + options.tick;
    while at <= history.to {
        ticks += 1;
        let flags = evaluate(history, at, &options.thresholds);
        let prs = pr_now(history, at);
        let before: Vec<(String, DateTime<Utc>)> = ledger
            .entries
            .iter()
            .filter(|(_, entry)| entry.addressed_at.is_none())
            .map(|(id, entry)| (id.clone(), entry.first_seen_at))
            .collect();
        reconcile(&mut ledger, &flags, &prs, at);
        // An entry replaced by a fresh episode ended because of a new head.
        for (id, first_seen) in before {
            let restarted = ledger
                .entries
                .get(&id)
                .is_some_and(|entry| entry.first_seen_at != first_seen);
            if restarted && let Some(episode) = episodes.get_mut(&(id, first_seen)) {
                episode.addressed_reason = Some("new_head".to_owned());
            }
        }
        for (id, entry) in &ledger.entries {
            let episode_key = (id.clone(), entry.first_seen_at);
            let episode = episodes.entry(episode_key).or_insert_with(|| Episode {
                pr: entry.pr,
                flag: entry.kind.number(),
                kind: entry.kind.as_str().to_owned(),
                key: entry.key.clone(),
                verdict: entry.verdict.clone(),
                first_evidence: entry.evidence.clone(),
                last_evidence: entry.evidence.clone(),
                head_sha: entry.head_sha.clone(),
                first_seen_at: entry.first_seen_at,
                last_seen_at: entry.last_seen_at,
                ticks: 0,
                digested_at: None,
                addressed_reason: None,
            });
            if entry.addressed_at.is_none() && entry.last_seen_at == at {
                episode.ticks += 1;
                episode.last_seen_at = at;
                episode.verdict.clone_from(&entry.verdict);
                episode.last_evidence.clone_from(&entry.evidence);
            }
            episode.addressed_reason.clone_from(&entry.addressed_reason);
        }
        // Simulate a digest that always delivers.
        let mut persist = |_: &Ledger| Ok(());
        let mut send = |_: &str| Ok(());
        if let Ok((digest::DigestOutcome::Sent { .. }, _)) = digest::run(
            &mut ledger,
            at,
            options.digest,
            true,
            &mut persist,
            &mut send,
        ) {
            digests += 1;
            for (id, entry) in &ledger.entries {
                if entry.digested_at == Some(at)
                    && let Some(episode) = episodes.get_mut(&(id.clone(), entry.first_seen_at))
                {
                    episode.digested_at = Some(at);
                }
            }
        }
        at += options.tick;
    }
    let mut episodes: Vec<Episode> = episodes.into_values().collect();
    episodes.sort_by(|a, b| {
        a.pr.cmp(&b.pr)
            .then_with(|| a.flag.cmp(&b.flag))
            .then_with(|| a.first_seen_at.cmp(&b.first_seen_at))
    });
    let raised = |pr: u64| -> BTreeSet<u8> {
        episodes
            .iter()
            .filter(|episode| episode.pr == pr)
            .map(|episode| episode.flag)
            .collect()
    };
    let expectations: Vec<ExpectationResult> = options
        .expectations
        .iter()
        .map(|expectation| {
            let expected: Vec<u8> = expectation.kinds.iter().map(|kind| kind.number()).collect();
            let got = raised(expectation.pr);
            let missing: Vec<u8> = expected
                .iter()
                .copied()
                .filter(|n| !got.contains(n))
                .collect();
            ExpectationResult {
                pr: expectation.pr,
                expected,
                raised: got.into_iter().collect(),
                pass: missing.is_empty(),
                missing,
            }
        })
        .collect();
    let control = options.control_merged_clean.then(|| {
        let clean = clean_merged(history);
        let violations: Vec<Episode> = episodes
            .iter()
            .filter(|episode| clean.contains(&episode.pr))
            .cloned()
            .collect();
        ControlResult {
            pass: violations.is_empty(),
            clean_merged: clean,
            violations,
        }
    });
    let pass = expectations.iter().all(|result| result.pass)
        && control.as_ref().is_none_or(|control| control.pass);
    ReplayReport {
        schema: REPLAY_SCHEMA.to_owned(),
        repo: history.repo.clone(),
        from: history.from,
        to: history.to,
        tick_minutes: options.tick.num_minutes(),
        ticks,
        prs: history.prs.len(),
        required_checks: history.required_checks.clone(),
        episodes,
        expectations,
        control,
        digests,
        gaps: history.gaps.clone(),
        pass,
    }
}
