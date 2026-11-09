//! Rank the open pull requests by how close each is to mergeable, so the
//! steward admits in that order and agents read one prioritised list.
//!
//! Tiers, closest first:
//!
//! 0. **stack base**: a pull request others are stacked on or carry, that is
//!    itself approved (tiers 1 and 2): landing it unblocks them. A stack base
//!    still waiting on a review stays in tier 3, first within it, so it never
//!    spends admission work ahead of an admittable head;
//! 1. **approved green**: every required check green and the exact head
//!    approved;
//! 2. **approved pending**: approved, the other required checks still
//!    running, none red;
//! 3. **green, needs review**: every required check but the approval green;
//! 4. everything else.
//!
//! Within a tier, more unblocked pull requests first, then the longest in
//! its state, then the lowest number. Pure and `O(n log n)` in open pull
//! requests: no reads.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

/// How many "next to admit" rows a report lists.
pub const NEXT_LISTED: usize = 5;

/// The non-approval required checks at a head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OthersState {
    /// Every one passed.
    Green,
    /// Some still run or have not started; none failed.
    Pending,
    /// At least one failed.
    Red,
}

/// One pull request, as the ranking reads it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RankInput {
    /// Pull request number.
    pub number: u64,
    /// Its `blocked_on` token.
    pub token: String,
    /// The required checks other than the approval context.
    pub others: OthersState,
    /// The exact head is approved: the gate admits it this pass, or the
    /// approval context is green at the head.
    pub approved: bool,
    /// GitHub reports a merge conflict, or a HOLD names the head: no tier
    /// above 4 however green.
    pub blocked_hard: bool,
    /// Already queued, admitted, a draft or excluded: not ranked.
    pub excluded: bool,
    /// Open pull requests that wait for this one (stacked on it, directly or
    /// transitively, or carrying its commits).
    pub unblocks: usize,
    /// Minutes in its current state, when dated.
    pub age_minutes: Option<i64>,
    /// Who acts next (`next_action.actor`).
    pub actor: String,
    /// What they do (`next_action.action`).
    pub action: String,
}

/// A ranking tier.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Unblocks other pull requests and is approved.
    StackBase,
    /// Approved and green.
    ApprovedGreen,
    /// Approved, checks running.
    ApprovedPending,
    /// Green, needs an exact-head review.
    GreenNeedsReview,
    /// Everything else.
    Rest,
}

/// One ranked pull request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Ranked {
    /// 1-based position.
    pub rank: usize,
    /// Pull request number.
    pub number: u64,
    /// Its tier.
    pub tier: Tier,
    /// Pull requests it unblocks.
    pub unblocks: usize,
    /// Its `blocked_on` token.
    pub blocked_on: String,
    /// The single thing it needs, in a few words.
    pub needs: String,
    /// Who acts.
    pub actor: String,
    /// What they do.
    pub action: String,
}

/// The tier of one input, before the stack-base promotion.
#[must_use]
pub fn base_tier(input: &RankInput) -> Tier {
    if input.blocked_hard {
        return Tier::Rest;
    }
    match (input.approved, input.others) {
        (true, OthersState::Green) => Tier::ApprovedGreen,
        (true, OthersState::Pending) => Tier::ApprovedPending,
        (false, OthersState::Green) => Tier::GreenNeedsReview,
        _ => Tier::Rest,
    }
}

/// The ranking tier, with the stack-base promotion of approved heads.
#[must_use]
pub fn tier(input: &RankInput) -> Tier {
    let tier = base_tier(input);
    if input.unblocks > 0 && matches!(tier, Tier::ApprovedGreen | Tier::ApprovedPending) {
        Tier::StackBase
    } else {
        tier
    }
}

/// Rank every input that is not excluded.
#[must_use]
pub fn rank(inputs: &[RankInput]) -> Vec<Ranked> {
    let mut rows: Vec<(&RankInput, Tier)> = inputs
        .iter()
        .filter(|input| !input.excluded)
        .map(|input| (input, tier(input)))
        .collect();
    rows.sort_by(|(left, left_tier), (right, right_tier)| {
        left_tier
            .cmp(right_tier)
            .then(right.unblocks.cmp(&left.unblocks))
            .then(
                right
                    .age_minutes
                    .unwrap_or(i64::MIN)
                    .cmp(&left.age_minutes.unwrap_or(i64::MIN)),
            )
            .then(left.number.cmp(&right.number))
    });
    rows.into_iter()
        .enumerate()
        .map(|(index, (input, tier))| Ranked {
            rank: index + 1,
            number: input.number,
            tier,
            unblocks: input.unblocks,
            blocked_on: input.token.clone(),
            needs: needs(input, tier),
            actor: input.actor.clone(),
            action: input.action.clone(),
        })
        .collect()
}

/// The one thing a ranked pull request needs, in a few words.
fn needs(input: &RankInput, tier: Tier) -> String {
    let token = input.token.as_str();
    let kind = token.split_once(':').map_or(token, |(kind, _)| kind);
    let base = match base_tier(input) {
        Tier::ApprovedGreen => match kind {
            "ready" | "admission_pending" => "admission by the steward (next pass)",
            "generated_path_batch" => "the queued entry sharing its generated path to land",
            "waiting_code_owner" => "a code-owner review",
            _ => "admission by the steward",
        },
        Tier::ApprovedPending => "required checks to finish",
        Tier::GreenNeedsReview => "an exact-head review verdict",
        Tier::StackBase | Tier::Rest => match kind {
            "merge_state" if token.ends_with("DIRTY") => "a conflict repair (merge the base in)",
            "held" => "a new head that answers the HOLD",
            "required_failed" => "a fix for the red required check",
            "required_never_ran" => "the required check to be dispatched",
            "carries_unapproved_commits" => "the carried pull request to be approved or land",
            "required_pending" => "required checks to finish",
            "no_verdict" => "an exact-head review verdict",
            "ejected_same_head" => "a new head",
            _ => "see next_action",
        },
    };
    if tier == Tier::StackBase {
        format!("{base}; unblocks {}", input.unblocks)
    } else {
        base.to_owned()
    }
}

/// Every pull request's unblock count from dependency edges
/// `(dependent, depends_on)`: how many distinct pull requests wait on it,
/// directly or through a chain. A cycle counts each member once.
#[must_use]
pub fn unblock_counts(edges: &[(u64, u64)]) -> BTreeMap<u64, usize> {
    let mut dependents: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
    for (dependent, base) in edges {
        if dependent != base {
            dependents.entry(*base).or_default().insert(*dependent);
        }
    }
    let mut counts = BTreeMap::new();
    for base in dependents.keys() {
        let mut seen = BTreeSet::new();
        let mut stack = vec![*base];
        while let Some(node) = stack.pop() {
            for next in dependents.get(&node).into_iter().flatten() {
                if *next != *base && seen.insert(*next) {
                    stack.push(*next);
                }
            }
        }
        counts.insert(*base, seen.len());
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(number: u64, approved: bool, others: OthersState) -> RankInput {
        RankInput {
            number,
            token: "ready".to_owned(),
            others,
            approved,
            blocked_hard: false,
            excluded: false,
            unblocks: 0,
            age_minutes: Some(10),
            actor: "steward".to_owned(),
            action: "admit".to_owned(),
        }
    }

    fn order(ranked: &[Ranked]) -> Vec<u64> {
        ranked.iter().map(|row| row.number).collect()
    }

    #[test]
    fn tiers_rank_stack_base_then_approved_green_pending_review_rest() {
        let mut rest = input(1, false, OthersState::Red);
        rest.token = "required_failed:build".to_owned();
        let mut review = input(2, false, OthersState::Green);
        review.token = "no_verdict".to_owned();
        let pending = input(3, true, OthersState::Pending);
        let green = input(4, true, OthersState::Green);
        let mut base = input(5, true, OthersState::Pending);
        base.unblocks = 1;
        let ranked = rank(&[rest, review, pending, green, base]);
        assert_eq!(order(&ranked), vec![5, 4, 3, 2, 1]);
        let tiers: Vec<Tier> = ranked.iter().map(|row| row.tier).collect();
        assert_eq!(
            tiers,
            vec![
                Tier::StackBase,
                Tier::ApprovedGreen,
                Tier::ApprovedPending,
                Tier::GreenNeedsReview,
                Tier::Rest
            ]
        );
        assert_eq!(ranked[0].rank, 1);
        assert!(ranked[0].needs.contains("unblocks 1"), "{:?}", ranked[0]);
    }

    #[test]
    fn a_stack_base_still_needing_review_ranks_first_in_its_tier_not_above_approved_heads() {
        let mut base = input(30, false, OthersState::Green);
        base.token = "no_verdict".to_owned();
        base.unblocks = 2;
        let mut review = input(31, false, OthersState::Green);
        review.token = "no_verdict".to_owned();
        review.age_minutes = Some(500);
        let green = input(32, true, OthersState::Green);
        let ranked = rank(&[review, base, green]);
        assert_eq!(order(&ranked), vec![32, 30, 31]);
        assert_eq!(ranked[1].tier, Tier::GreenNeedsReview);
    }

    #[test]
    fn within_a_tier_more_unblocks_then_older_then_lower_number() {
        let mut a = input(10, true, OthersState::Green);
        a.unblocks = 1;
        let mut b = input(11, true, OthersState::Green);
        b.unblocks = 3;
        let mut c = input(12, true, OthersState::Green);
        c.age_minutes = Some(90);
        let mut d = input(13, true, OthersState::Green);
        d.age_minutes = Some(90);
        let ranked = rank(&[a, b, c, d]);
        assert_eq!(order(&ranked), vec![11, 10, 12, 13]);
    }

    #[test]
    fn a_red_or_dirty_stack_base_is_not_promoted_and_excluded_rows_are_not_ranked() {
        let mut dirty = input(20, true, OthersState::Green);
        dirty.blocked_hard = true;
        dirty.unblocks = 4;
        let mut queued = input(21, true, OthersState::Green);
        queued.excluded = true;
        let green = input(22, true, OthersState::Green);
        let ranked = rank(&[dirty, queued, green]);
        assert_eq!(order(&ranked), vec![22, 20]);
        assert_eq!(ranked[1].tier, Tier::Rest);
    }

    #[test]
    fn unblock_counts_follow_chains_and_survive_cycles() {
        // 3 on 2 on 1; 4 on 1; 5 and 6 form a cycle.
        let counts = unblock_counts(&[(2, 1), (3, 2), (4, 1), (5, 6), (6, 5), (7, 7)]);
        assert_eq!(counts.get(&1), Some(&3));
        assert_eq!(counts.get(&2), Some(&1));
        assert_eq!(counts.get(&5), Some(&1));
        assert_eq!(counts.get(&6), Some(&1));
        assert_eq!(counts.get(&7), None, "a self edge is no dependency");
    }

    #[test]
    fn ranking_a_large_backlog_is_cheap_and_total() {
        let inputs: Vec<RankInput> = (0..3_000)
            .map(|n| {
                let others = match n % 3 {
                    0 => OthersState::Green,
                    1 => OthersState::Pending,
                    _ => OthersState::Red,
                };
                input(n, n % 2 == 0, others)
            })
            .collect();
        let ranked = rank(&inputs);
        assert_eq!(ranked.len(), 3_000);
        assert!(
            ranked.windows(2).all(|pair| pair[0].tier <= pair[1].tier),
            "tiers are monotone"
        );
    }
}
