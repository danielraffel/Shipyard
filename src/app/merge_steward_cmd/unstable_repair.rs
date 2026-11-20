//! Route an approved head that GitHub holds `UNSTABLE` on a red, non-required,
//! non-advisory check to the repair lane (`[auto_merge] repair_unstable_reds`,
//! default off).
//!
//! On a base with no merge queue the steward merges only `CLEAN`, or
//! `UNSTABLE` over checks the base lists as advisory
//! ([`crate::merge_steward::merge_state_wait`]). Any other red check holds the
//! head as `merge_state:UNSTABLE`, which the loop sends to the coordinator.
//! The repair lane never sees it: its queue builds `FAILED:` only from
//! required checks. On 2026-11-19 Shipyard #1529 waited 6.5 h there on a red
//! `Enforce version & skill sync`.
//!
//! With the flag on, such a head reads `unstable_failed:<checks>`: loop state
//! `required_red`, actor repair. This module adds the stop rule: once the
//! same check has gone red on two distinct runs at one head (two routes to
//! repair), a third red run reads `unstable_red_exhausted:<checks>` and goes
//! to the coordinator instead. The runs are kept in the ledger
//! ([`Routes`]), reset by a new head, and dropped when the pull request
//! closes. Nothing here admits, merges or posts anything.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::merge_steward::UnstableRed;

/// `blocked_on` kind for a head the repair lane should fix.
pub(super) const ROUTED: &str = "unstable_failed";
/// `blocked_on` kind once the route limit is spent at this head.
pub(super) const EXHAUSTED: &str = "unstable_red_exhausted";
/// Red runs of one check at one head that go to repair; the next goes to the
/// coordinator.
pub(super) const ROUTE_LIMIT: usize = 2;

/// The red runs routed for one pull request at one head.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Routes {
    /// The head the runs were seen at (lowercase).
    pub(super) head: String,
    /// `<check>\t<check id>` for each distinct red run seen, sorted.
    pub(super) runs: Vec<String>,
}

fn run_key(red: &UnstableRed) -> String {
    format!(
        "{}\t{}",
        red.name,
        red.check_id
            .map_or_else(|| "-".to_owned(), |id| id.to_string())
    )
}

fn names(reds: &[UnstableRed]) -> String {
    reds.iter()
        .map(|red| red.name.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// The runs on record for this head plus the current ones, deduplicated.
fn runs_with(prior: Option<&Routes>, head: &str, reds: &[UnstableRed]) -> BTreeSet<String> {
    let mut runs = prior
        .filter(|routes| routes.head.eq_ignore_ascii_case(head))
        .map(|routes| routes.runs.iter().cloned().collect::<BTreeSet<_>>())
        .unwrap_or_default();
    runs.extend(reds.iter().map(run_key));
    runs
}

/// The token and detail for a routed head: `unstable_failed:<checks>`, or
/// `unstable_red_exhausted:<checks>` when any current red check has more than
/// [`ROUTE_LIMIT`] distinct red runs at this head.
#[must_use]
pub(super) fn token(reds: &[UnstableRed], prior: Option<&Routes>, head: &str) -> (String, String) {
    let runs = runs_with(prior, head, reds);
    let spent = reds
        .iter()
        .filter(|red| {
            let prefix = format!("{}\t", red.name);
            runs.iter().filter(|run| run.starts_with(&prefix)).count() > ROUTE_LIMIT
        })
        .cloned()
        .collect::<Vec<_>>();
    if spent.is_empty() {
        (
            format!("{ROUTED}:{}", names(reds)),
            format!(
                "red, not required, not in unstable_advisory_checks: {}; the repair lane fixes \
                 it (route limit {ROUTE_LIMIT} red runs per check at this head)",
                names(reds)
            ),
        )
    } else {
        (
            format!("{EXHAUSTED}:{}", names(&spent)),
            format!(
                "{} went red on more than {ROUTE_LIMIT} runs at this head after repair routes; \
                 the coordinator decides",
                names(&spent)
            ),
        )
    }
}

/// Keep the runs of every routed row for the next pass. A row on a new head
/// starts over; a closed pull request is dropped. `rows` is
/// `(repo#pr key, head, reds)` for each row this pass routed or exhausted;
/// `open` is every open pull request's key in this repository.
pub(super) fn record(
    routes: &mut BTreeMap<String, Routes>,
    prefix: &str,
    rows: &[(String, String, Vec<UnstableRed>)],
    open: &BTreeSet<String>,
) {
    routes.retain(|key, _| !key.starts_with(prefix) || open.contains(key));
    for (key, head, reds) in rows {
        let runs = runs_with(routes.get(key), head, reds);
        routes.insert(
            key.clone(),
            Routes {
                head: head.to_ascii_lowercase(),
                runs: runs.into_iter().collect(),
            },
        );
    }
}

/// Apply the route limit to every routed row of one pass, against the runs
/// on record, and (apply passes only) keep this pass's runs. Reads nothing
/// from GitHub.
pub(super) fn annotate(
    observation: &super::RepoObservation,
    policy: &crate::merge_steward::StewardPolicy,
    ledger: &mut super::StewardLedger,
    reports: &mut [super::PrReport],
    apply: bool,
) {
    let prefix = format!("{}#", observation.repo.to_ascii_lowercase());
    let mut rows = Vec::new();
    for report in reports.iter_mut() {
        let kind = report
            .blocked
            .blocked_on
            .split_once(':')
            .map_or(report.blocked.blocked_on.as_str(), |(kind, _)| kind);
        if kind != ROUTED {
            continue;
        }
        let Some(pr) = observation
            .prs
            .iter()
            .find(|pr| pr.fact.number == report.number)
        else {
            continue;
        };
        let reds = crate::merge_steward::unstable_reds(&pr.fact, policy);
        if reds.is_empty() {
            continue;
        }
        let key = format!("{prefix}{}", report.number);
        let (token, detail) = token(&reds, ledger.unstable_routes.get(&key), &pr.fact.head_sha);
        if token != report.blocked.blocked_on {
            report.blocked.blocked_on = token;
            report.blocked.blocked_detail = Some(detail);
            report.blocked.next_action = Some(super::next_action::for_row(
                &super::next_action::RowContext {
                    repo: &observation.repo,
                    base: &observation.base,
                    pr: &pr.fact,
                    policy,
                    error: report.error.as_deref(),
                },
                &report.blocked,
            ));
        }
        rows.push((key, pr.fact.head_sha.clone(), reds));
    }
    if apply {
        let open = observation
            .prs
            .iter()
            .map(|pr| format!("{prefix}{}", pr.fact.number))
            .collect::<BTreeSet<_>>();
        record(&mut ledger.unstable_routes, &prefix, &rows, &open);
    }
}
