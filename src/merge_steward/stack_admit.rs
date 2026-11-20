//! Planning for admitting an approved pull-request stack as one queue batch.
//!
//! The planner is deliberately transport-free.  The steward supplies the
//! immutable census (head, base, labels, approval and required-check state)
//! and applies the returned operations only after the whole chain has been
//! validated.  This makes a mid-pass change fail closed without partially
//! inventing a stack.

use std::collections::{BTreeMap, BTreeSet};

/// One pull request in a candidate stack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Member {
    /// Pull-request number.
    pub number: u64,
    /// Head branch name.
    pub head_branch: String,
    /// Immutable observed head.
    pub head_sha: String,
    /// Target branch name.
    pub base_branch: String,
    /// Labels observed on the pull request.
    pub labels: Vec<String>,
    /// Exact-head approval and all required checks are green.
    pub approved_green: bool,
    /// Commits present in this PR's own diff which are inherited from open
    /// ancestors.  The carried-commit guard uses the same set.
    pub carried_commits: BTreeSet<String>,
    /// Commits introduced by this pull request.
    pub own_commits: BTreeSet<String>,
}

/// A single pass's safe stack plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Plan {
    /// Root first, child last.
    /// Pull-request numbers in root-to-leaf order.
    pub members: Vec<u64>,
    /// Child retarget operations, in order.
    pub retarget: Vec<(u64, String)>,
    /// Fail-closed reason when no plan was produced.
    pub reason: Option<String>,
}

impl Plan {
    fn refused(reason: impl Into<String>) -> Self {
        Self {
            members: Vec::new(),
            retarget: Vec::new(),
            reason: Some(reason.into()),
        }
    }
}

/// Build a root-to-leaf plan.  A child declares its parent either by a
/// `stacked:<n>` label or by targeting the parent's head branch.  Every
/// member must be approved and green at the census head; an unapproved or
/// inconsistent member stops the pass after the already validated prefix.
#[must_use]
pub fn plan(base: &str, rows: &[Member]) -> Plan {
    let by_number: BTreeMap<u64, &Member> = rows.iter().map(|row| (row.number, row)).collect();
    let mut children: BTreeMap<u64, Vec<&Member>> = BTreeMap::new();
    for row in rows {
        let parent = row.labels.iter().find_map(|label| {
            label
                .strip_prefix("stacked:")
                .and_then(|n| n.parse::<u64>().ok())
        });
        if let Some(parent) = parent.or_else(|| {
            rows.iter()
                .find(|candidate| candidate.head_branch == row.base_branch)
                .map(|p| p.number)
        }) {
            children.entry(parent).or_default().push(row);
        }
    }
    let roots: Vec<&Member> = rows.iter().filter(|row| row.base_branch == base).collect();
    for root in roots {
        let mut chain = Vec::new();
        let mut current = root;
        let mut seen = BTreeSet::new();
        loop {
            if !seen.insert(current.number) {
                return Plan::refused("stack cycle");
            }
            if !current.approved_green {
                return Plan::refused(format!(
                    "stack member #{} is not approved and green",
                    current.number
                ));
            }
            if current.number != root.number && current.base_branch == base {
                return Plan::refused(format!(
                    "stack member #{} was already retargeted",
                    current.number
                ));
            }
            if let Some(parent) = current.labels.iter().find_map(|label| {
                label
                    .strip_prefix("stacked:")
                    .and_then(|n| n.parse::<u64>().ok())
            }) && !by_number.contains_key(&parent)
            {
                return Plan::refused(format!(
                    "stack member #{} names missing parent #{}",
                    current.number, parent
                ));
            }
            chain.push(current.number);
            let next = children.get(&current.number).and_then(|members| {
                if members.len() == 1 {
                    Some(members[0])
                } else {
                    None
                }
            });
            let Some(next) = next else { break };
            current = next;
        }
        if chain.len() > 1 {
            let retarget = chain
                .iter()
                .skip(1)
                .map(|number| (*number, base.to_owned()))
                .collect();
            return Plan {
                members: chain,
                retarget,
                reason: None,
            };
        }
    }
    Plan::refused("no declared stack")
}

/// A child may carry only commits introduced by the ancestors named by the
/// chain.  This is the narrow exception used by the carried-commit guard.
#[must_use]
pub fn carries_only_declared_ancestors(child: &Member, ancestors: &[&Member]) -> bool {
    let allowed: BTreeSet<&String> = ancestors
        .iter()
        .flat_map(|row| row.own_commits.iter())
        .collect();
    child
        .carried_commits
        .iter()
        .all(|commit| allowed.contains(commit))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(number: u64, head: &str, base: &str) -> Member {
        Member {
            number,
            head_branch: head.into(),
            head_sha: format!("{number:040}"),
            base_branch: base.into(),
            labels: Vec::new(),
            approved_green: true,
            carried_commits: BTreeSet::new(),
            own_commits: [format!("{number}")].into_iter().collect(),
        }
    }

    #[test]
    fn three_deep_stack_is_ordered_and_retargeted() {
        let rows = vec![
            row(1, "one", "main"),
            row(2, "two", "one"),
            row(3, "three", "two"),
        ];
        assert_eq!(
            plan("main", &rows),
            Plan {
                members: vec![1, 2, 3],
                retarget: vec![(2, "main".into()), (3, "main".into())],
                reason: None
            }
        );
    }

    #[test]
    fn unapproved_middle_member_stops_the_plan() {
        let mut middle = row(2, "two", "one");
        middle.approved_green = false;
        let result = plan(
            "main",
            &[row(1, "one", "main"), middle, row(3, "three", "two")],
        );
        assert!(result.members.is_empty());
        assert!(result.reason.unwrap().contains("#2"));
    }

    #[test]
    fn extra_carried_commit_is_refused() {
        let parent = row(1, "one", "main");
        let mut child = row(2, "two", "one");
        child.carried_commits.insert("unrelated".into());
        assert!(!carries_only_declared_ancestors(&child, &[&parent]));
    }
}
