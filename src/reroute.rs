//! Cloud→local macOS reroute decision logic (#316 Part C).
//!
//! Ports the behavior of Pulp's `tools/scripts/macos_reroute_watcher.py`
//! (task #22), generalized from single-host busy/idle to multi-host VM-slot
//! accounting. The watcher's job: when local macOS capacity frees up, claw a
//! still-queued cloud-bound macOS job back to a local runner — macOS builds run
//! far faster on a warm-cache local Mac than a cold GitHub-hosted `macos-15`,
//! and once a PR is dispatched to cloud it otherwise stays there even if local
//! frees before the cloud pool picks it up.
//!
//! This module is the **pure decision core**; the polling loop, capacity probe,
//! and the actual reroute are the impure edge in `src/app/reroute_cmd.rs`. The
//! four safety properties from the Pulp spec live here or are enforced by the
//! caller:
//!
//! 1. **Slot-safe** — only reroute when `free > 0` (free slots come from
//!    [`crate::capacity`], where an unreadable host already counts as 0, so this
//!    is also **fail-closed**: an all-unreadable fleet yields `free == 0` and we
//!    do nothing).
//! 2. **Flap-guard** — a PR rerouted within the flap window is skipped, even if
//!    it still matches, to avoid thrashing cloud↔local.
//! 3. **One reroute per tick** — natural pacing; the next tick reassesses.
//! 4. **Deterministic choice** — candidates are considered in caller order
//!    (oldest-queued first), so the decision is reproducible.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

/// Labels that mark a macOS job as bound to a **cloud** runner pool.
const CLOUD_MARKERS: [&str; 3] = ["macos-15", "nscloud-", "namespace-profile-"];

/// A cloud-queued macOS job that could be drained to a local runner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RerouteCandidate {
    /// PR number (needed to drive `cloud retarget --pr`).
    pub pr: u64,
    /// The workflow run whose macOS leg is queued on cloud.
    pub run_id: u64,
    /// Head branch of the run (for logging / dispatch ref resolution).
    pub head_branch: String,
}

/// Remembers when each PR was last rerouted and suppresses re-action within the
/// flap window. Mirrors the Pulp watcher's `FlapGuard`.
#[derive(Debug, Default)]
pub struct FlapGuard {
    window_secs: i64,
    last: HashMap<u64, DateTime<Utc>>,
}

impl FlapGuard {
    /// New guard with the given window in seconds.
    #[must_use]
    pub fn new(window_secs: i64) -> Self {
        Self {
            window_secs: window_secs.max(0),
            last: HashMap::new(),
        }
    }

    /// Whether `pr` may be rerouted now (never rerouted, or last reroute was
    /// more than the window ago).
    #[must_use]
    pub fn can_reroute(&self, pr: u64, now: DateTime<Utc>) -> bool {
        match self.last.get(&pr) {
            None => true,
            Some(last) => (now - *last).num_seconds() >= self.window_secs,
        }
    }

    /// Record that `pr` was just rerouted, and trim entries older than twice the
    /// window to bound memory.
    pub fn record(&mut self, pr: u64, now: DateTime<Utc>) {
        self.last.insert(pr, now);
        let cutoff = self.window_secs.saturating_mul(2);
        self.last
            .retain(|_, when| (now - *when).num_seconds() < cutoff);
    }
}

/// The outcome of one decision tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RerouteDecision {
    /// Reroute this candidate (and only this one) this tick.
    Reroute(RerouteCandidate),
    /// No free local slots — `free == 0` (also the all-unreadable, fail-closed case).
    NoFreeSlots,
    /// Free slots exist but nothing is queued on cloud.
    NoCandidates,
    /// Candidates exist but all are inside their flap window.
    AllFlapGuarded,
}

/// Decide whether to reroute exactly one candidate this tick.
///
/// `free` is the fleet's free macOS slots from [`crate::capacity::total_free`]
/// (unreadable hosts already count as 0 → fail-closed). `candidates` are the
/// cloud-queued macOS PRs in priority order (oldest first). Returns at most one
/// reroute (one-per-tick pacing); the loop reassesses next tick.
#[must_use]
pub fn decide_reroute(
    free: u32,
    candidates: &[RerouteCandidate],
    guard: &FlapGuard,
    now: DateTime<Utc>,
) -> RerouteDecision {
    if free == 0 {
        return RerouteDecision::NoFreeSlots;
    }
    if candidates.is_empty() {
        return RerouteDecision::NoCandidates;
    }
    match candidates.iter().find(|c| guard.can_reroute(c.pr, now)) {
        Some(candidate) => RerouteDecision::Reroute(candidate.clone()),
        None => RerouteDecision::AllFlapGuarded,
    }
}

/// Whether a flattened, comma-joined macOS-job label string targets a **cloud**
/// runner (GitHub-hosted `macos-15` or a Namespace selector) and not a
/// self-hosted local runner. Mirrors Pulp's `_macos_job_targets_cloud`:
/// `self-hosted` anywhere means it's already local (skip); otherwise a cloud
/// marker means it's reroutable. An empty string (macOS job not yet dispatched —
/// resolver still running) is not reroutable: we don't know its target yet.
#[must_use]
pub fn macos_job_targets_cloud(labels_csv: &str) -> bool {
    let lower = labels_csv.to_lowercase();
    if lower.trim().is_empty() {
        return false;
    }
    if lower.split(',').any(|l| l.trim() == "self-hosted") {
        return false;
    }
    CLOUD_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Whether `--apply` is safe to act: `cloud retarget` has no `--repo` flag and
/// resolves its repo from the current checkout, so the monitored repo must match
/// the checkout — otherwise an `--apply` reroute would target the same-numbered
/// PR in the *wrong* repo. Observe mode (no apply) may freely monitor any repo.
#[must_use]
pub fn apply_repo_is_safe(apply: bool, monitored_repo: &str, checkout_repo: &str) -> bool {
    !apply || monitored_repo.eq_ignore_ascii_case(checkout_repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("timestamp")
    }

    fn cand(pr: u64) -> RerouteCandidate {
        RerouteCandidate {
            pr,
            run_id: pr * 1000,
            head_branch: format!("feature/{pr}"),
        }
    }

    #[test]
    fn no_free_slots_means_no_reroute() {
        let guard = FlapGuard::new(300);
        assert_eq!(
            decide_reroute(0, &[cand(1)], &guard, ts(0)),
            RerouteDecision::NoFreeSlots
        );
    }

    #[test]
    fn free_slots_but_no_candidates() {
        let guard = FlapGuard::new(300);
        assert_eq!(
            decide_reroute(2, &[], &guard, ts(0)),
            RerouteDecision::NoCandidates
        );
    }

    #[test]
    fn reroutes_first_eligible_candidate_only() {
        let guard = FlapGuard::new(300);
        let decision = decide_reroute(2, &[cand(1), cand(2)], &guard, ts(0));
        // One per tick — the first (oldest) eligible.
        assert_eq!(decision, RerouteDecision::Reroute(cand(1)));
    }

    #[test]
    fn flap_guard_skips_recently_rerouted_pr() {
        let mut guard = FlapGuard::new(300);
        guard.record(1, ts(0));
        // 100s later: still inside the 300s window → PR 1 skipped, PR 2 chosen.
        assert_eq!(
            decide_reroute(2, &[cand(1), cand(2)], &guard, ts(100)),
            RerouteDecision::Reroute(cand(2))
        );
        // Only PR 1 queued and still guarded → no reroute.
        assert_eq!(
            decide_reroute(2, &[cand(1)], &guard, ts(100)),
            RerouteDecision::AllFlapGuarded
        );
    }

    #[test]
    fn flap_guard_allows_after_window() {
        let mut guard = FlapGuard::new(300);
        guard.record(1, ts(0));
        assert!(!guard.can_reroute(1, ts(299)));
        assert!(guard.can_reroute(1, ts(300)));
        assert_eq!(
            decide_reroute(1, &[cand(1)], &guard, ts(300)),
            RerouteDecision::Reroute(cand(1))
        );
    }

    #[test]
    fn flap_guard_record_trims_old_entries() {
        let mut guard = FlapGuard::new(300);
        guard.record(1, ts(0));
        // Much later: recording PR 2 evicts PR 1 (older than 2× window).
        guard.record(2, ts(1000));
        assert!(guard.can_reroute(1, ts(1000)), "stale PR 1 entry trimmed");
    }

    #[test]
    fn macos_job_targets_cloud_detects_cloud_markers() {
        assert!(macos_job_targets_cloud("macos-15"));
        assert!(macos_job_targets_cloud(
            "macOS (ARM64) [github-hosted],macos-15"
        ));
        assert!(macos_job_targets_cloud(
            "namespace-profile-generouscorp-macos"
        ));
        assert!(macos_job_targets_cloud("nscloud-macos"));
    }

    #[test]
    fn apply_repo_guard_blocks_cross_repo_apply_only() {
        // Observe mode: any monitored repo is fine.
        assert!(apply_repo_is_safe(false, "owner/other", "owner/here"));
        // Apply + matching repo (case-insensitive): safe.
        assert!(apply_repo_is_safe(
            true,
            "danielraffel/Shipyard",
            "danielraffel/shipyard"
        ));
        // Apply + different repo: unsafe (would retarget the wrong repo's PR).
        assert!(!apply_repo_is_safe(true, "owner/other", "owner/here"));
    }

    #[test]
    fn macos_job_already_local_or_unknown_is_not_reroutable() {
        // self-hosted anywhere → already local.
        assert!(!macos_job_targets_cloud(
            "self-hosted,macos,arm64,local-mac"
        ));
        assert!(!macos_job_targets_cloud("self-hosted,macos-15"));
        // empty (resolver still running, target unknown) → not yet reroutable.
        assert!(!macos_job_targets_cloud(""));
        assert!(!macos_job_targets_cloud("   "));
        // a non-cloud, non-self-hosted label set → not a known cloud target.
        assert!(!macos_job_targets_cloud("ubuntu-latest"));
    }
}
