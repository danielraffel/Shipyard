//! `shipyard verdicts` — report finished validations nobody has consumed.
//!
//! A validation that ran to completion and failed is not orphaned, so it never
//! appears in `ship-state list`, and the agent that dispatched it has usually
//! finished its turn by the time the verdict lands. This command reads the
//! records Shipyard already wrote and says which of them still need someone.
//!
//! Resolution is **batched per repository**: one `pr list` invocation covers
//! every candidate in that repository, so lookup cost scales with the chosen
//! window rather than with how many records the store holds. The per-record
//! alternative is what `ship-state list` does, and its own source notes why
//! that is a hazard — it caps lookups at 25 and leaves the remainder
//! unresolved. Batching per repository removes both the cap and the burst.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::output::write_json_envelope;
use crate::ship_state::ShipStateStore;
use crate::verdicts::{
    PrDisposition, TerminalVerdict, VerdictReport, VerdictRow, build_report, terminal_verdict,
};

/// Exit code returned when at least one finished validation still needs action.
pub(super) const VERDICTS_EXIT_ACTIONABLE: u8 = 1;
/// Most unresolved rows listed before the remainder is summarised.
///
/// The count always prints in full; only the per-row listing is bounded. An
/// unbounded list of rows nobody can act on is how a useful signal acquires the
/// habit of being scrolled past.
const MAX_UNRESOLVED_ROWS_SHOWN: usize = 10;

/// Exit code returned when the scan could not resolve part of what it found.
///
/// Deliberately distinct from both success and "work to do": a scan that was
/// partly blind must not be readable as either.
pub(super) const VERDICTS_EXIT_UNRESOLVED: u8 = 5;

/// Resolves every candidate pull request in one repository with one call.
///
/// Production passes a `gh`-backed reader; tests inject a fixture so no test
/// touches the network. A repository the reader cannot answer for yields an
/// empty map, which leaves its rows [`PrDisposition::Unknown`] and therefore
/// counted as unresolved rather than quietly dropped.
pub(super) type RepoDispositionReader<'a> =
    &'a mut dyn FnMut(&str, &BTreeSet<u64>) -> BTreeMap<u64, PrDisposition>;

/// Scan durable ship-state and report unconsumed terminal verdicts.
pub(super) fn verdicts<W: Write>(
    store: &ShipStateStore,
    resolve: RepoDispositionReader<'_>,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let states = store.list_active();

    // Pre-pass: only records with a non-passing terminal verdict ever cost a
    // lookup. An in-flight ship has no verdict to consume, and a pass needs
    // nobody, so neither reaches GitHub.
    let mut wanted: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    for state in &states {
        if terminal_verdict(state).is_some_and(TerminalVerdict::needs_attention) {
            wanted
                .entry(state.repo.clone())
                .or_default()
                .insert(state.pr);
        }
    }

    let mut resolved: BTreeMap<String, BTreeMap<u64, PrDisposition>> = BTreeMap::new();
    for (repo, prs) in &wanted {
        resolved.insert(repo.clone(), resolve(repo, prs));
    }

    let report = build_report(&states, |repo, pr| {
        resolved
            .get(repo)
            .and_then(|found| found.get(&pr).copied())
            .unwrap_or(PrDisposition::Unknown)
    });

    if json_output {
        emit_json(&report, wanted.len(), stdout)?;
    } else {
        render(&report, wanted.len(), stdout)?;
    }

    Ok(exit_code(&report))
}

/// Map a report onto a process exit code.
///
/// Actionable work outranks partial blindness, because a known failure is more
/// urgent than an unknown one; blindness alone still refuses to return success.
fn exit_code(report: &VerdictReport) -> ExitCode {
    if report.census.actionable > 0 {
        ExitCode::from(VERDICTS_EXIT_ACTIONABLE)
    } else if report.census.unresolved > 0 {
        ExitCode::from(VERDICTS_EXIT_UNRESOLVED)
    } else {
        ExitCode::SUCCESS
    }
}

/// One rendered row.
fn render_row(row: &VerdictRow) -> String {
    let title = if row.title.is_empty() {
        String::new()
    } else {
        format!("  {}", row.title)
    };
    format!(
        "  {} PR #{} [{}] pr={}{}",
        row.repo,
        row.pr,
        row.verdict.label(),
        row.disposition.label(),
        title
    )
}

/// Human-readable output.
///
/// The census prints on every run, including the quiet one. An absence claim
/// that shows no counts cannot be distinguished from an instrument that read
/// nothing, which is the failure this command exists to stop repeating.
fn render<W: Write>(
    report: &VerdictReport,
    repos_queried: usize,
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let actionable = report.actionable();
    if actionable.is_empty() {
        // Only a scan that resolved everything it found may say so. A blind
        // scan that printed this line first would read as an all-clear to
        // anyone who stopped at the first line, which is the exact misreading
        // this command exists to prevent.
        if report.census.supports_all_clear() {
            writeln!(stdout, "No unconsumed terminal verdicts need action.")?;
        } else {
            writeln!(
                stdout,
                "No ACTIONABLE verdict found — but this scan was incomplete; see below."
            )?;
        }
    } else {
        writeln!(
            stdout,
            "Unconsumed terminal verdicts (validation finished, pull request still open)"
        )?;
        for row in actionable {
            writeln!(stdout, "{}", render_row(row))?;
        }
    }

    let unresolved = report.unresolved();
    if !unresolved.is_empty() {
        writeln!(
            stdout,
            "Unresolved ({}) — a verdict was found but its pull request state could not be read.\n  These are NOT clean; they are unknown. Raise --limit if they predate the lookup window.",
            unresolved.len()
        )?;
        for row in unresolved.iter().take(MAX_UNRESOLVED_ROWS_SHOWN) {
            writeln!(stdout, "{}", render_row(row))?;
        }
        if let Some(extra) = unresolved.len().checked_sub(MAX_UNRESOLVED_ROWS_SHOWN)
            && extra > 0
        {
            writeln!(
                stdout,
                "  … and {extra} more unresolved (see --json for all)"
            )?;
        }
    }

    let census = report.census;
    writeln!(
        stdout,
        "Census  scanned={} terminal={} passed={} failed={} cancelled={} resolved={} unresolved={} actionable={} repos_queried={}",
        census.scanned,
        census.terminal,
        census.passed,
        census.failed,
        census.cancelled,
        census.resolved,
        census.unresolved,
        census.actionable,
        repos_queried
    )?;

    if !census.is_self_consistent() {
        writeln!(
            stdout,
            "Census does not reconcile: passed+failed+cancelled != terminal. Treat this scan as unreliable."
        )?;
    } else if !census.supports_all_clear() && census.actionable == 0 {
        writeln!(
            stdout,
            "This scan cannot support an all-clear: {} verdict(s) went unresolved.",
            census.unresolved
        )?;
    }
    Ok(())
}

/// Machine-readable envelope.
fn emit_json<W: Write>(
    report: &VerdictReport,
    repos_queried: usize,
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let rows = report
        .rows
        .iter()
        .map(|row| {
            json!({
                "repo": row.repo,
                "pr": row.pr,
                "verdict": row.verdict.label(),
                "disposition": row.disposition.label(),
                "actionable": row.is_actionable(),
                "unresolved": row.is_unresolved(),
                "title": row.title,
                "url": row.url,
            })
        })
        .collect::<Vec<_>>();
    let census = report.census;
    let mut data = BTreeMap::new();
    data.insert("verdicts".to_owned(), Value::Array(rows));
    data.insert(
        "census".to_owned(),
        json!({
            "scanned": census.scanned,
            "terminal": census.terminal,
            "passed": census.passed,
            "failed": census.failed,
            "cancelled": census.cancelled,
            "resolved": census.resolved,
            "unresolved": census.unresolved,
            "actionable": census.actionable,
            "repos_queried": repos_queried,
            "self_consistent": census.is_self_consistent(),
            "supports_all_clear": census.supports_all_clear(),
        }),
    );
    write_json_envelope(stdout, "verdicts", data)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::process::ExitCode;

    use chrono::Utc;

    use super::{VERDICTS_EXIT_ACTIONABLE, VERDICTS_EXIT_UNRESOLVED, verdicts};
    use crate::ship_state::{DispatchedRun, ShipState, ShipStateStore};
    use crate::verdicts::PrDisposition;

    fn store_with(states: &[ShipState]) -> (tempfile::TempDir, ShipStateStore) {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        for state in states {
            store.save(state).expect("save state");
        }
        (temp, store)
    }

    fn finished_state(repo: &str, pr: u64, run_status: &str, evidence: &str) -> ShipState {
        let now = Utc::now();
        let mut state = ShipState::new(
            pr,
            repo,
            format!("feature/{pr}"),
            "main",
            format!("{pr:0>40}"),
            "sig",
        );
        state.pr_title = format!("change {pr}");
        state.pr_url = format!("https://github.com/{repo}/pull/{pr}");
        state.dispatched_runs.push(DispatchedRun {
            target: "mac".to_owned(),
            provider: "local".to_owned(),
            run_id: format!("sy-{pr}"),
            status: run_status.to_owned(),
            started_at: now,
            updated_at: now,
            attempt: 1,
            last_heartbeat_at: None,
            phase: None,
            required: true,
        });
        state
            .evidence_snapshot
            .insert("mac".to_owned(), evidence.to_owned());
        state
    }

    fn run(
        store: &ShipStateStore,
        resolve: super::RepoDispositionReader<'_>,
    ) -> (ExitCode, String) {
        let mut out = Vec::new();
        let code = verdicts(store, resolve, false, &mut out).expect("verdicts");
        (code, String::from_utf8(out).expect("utf8"))
    }

    /// The reported instance: a gate failed, the lane moved on, the pull
    /// request is still open. It must be named, and the command must exit
    /// non-zero so a caller that only reads status still notices.
    #[test]
    fn a_failed_gate_on_an_open_pull_request_is_reported_and_exits_nonzero() {
        let (_temp, store) = store_with(&[finished_state("acme/widget", 140, "failed", "fail")]);
        let (code, text) = run(&store, &mut |_, prs| {
            prs.iter().map(|pr| (*pr, PrDisposition::Open)).collect()
        });

        assert_eq!(code, ExitCode::from(VERDICTS_EXIT_ACTIONABLE));
        assert!(text.contains("acme/widget PR #140"), "{text}");
        assert!(text.contains("[failed]"), "{text}");
        assert!(text.contains("actionable=1"), "{text}");

        // CONTROL: the same record on a merged pull request must fall out of
        // the report and return success. Without this, the assertions above
        // would also pass for a command that reports every record it sees.
        let (merged_code, merged_text) = run(&store, &mut |_, prs| {
            prs.iter().map(|pr| (*pr, PrDisposition::Merged)).collect()
        });
        assert_eq!(merged_code, ExitCode::SUCCESS);
        assert!(merged_text.contains("actionable=0"), "{merged_text}");
        assert!(
            merged_text.contains("No unconsumed terminal verdicts need action."),
            "{merged_text}"
        );
    }

    /// A resolver that has been switched off must produce a visibly incomplete
    /// scan — distinct text and a distinct exit code — never a clean bill of
    /// health. This is the property that stops the consumer from reproducing
    /// the bug it closes.
    #[test]
    fn a_disabled_resolver_is_visibly_absent_rather_than_silently_clean() {
        let (_temp, store) = store_with(&[finished_state("acme/widget", 141, "failed", "fail")]);

        // The resolver answers nothing at all, as a broken or disabled one would.
        let (code, text) = run(&store, &mut |_, _| BTreeMap::new());
        assert_eq!(
            code,
            ExitCode::from(VERDICTS_EXIT_UNRESOLVED),
            "a blind scan must not exit 0"
        );
        assert!(text.contains("Unresolved"), "{text}");
        assert!(text.contains("unresolved=1"), "{text}");
        assert!(
            text.contains("cannot support an all-clear"),
            "the output must refuse the clean claim in words: {text}"
        );
        assert!(
            !text.contains("No unconsumed terminal verdicts need action."),
            "a blind scan must never print the all-clear line: {text}"
        );

        // CONTROL: a working resolver over the same store returns success and
        // prints the all-clear, proving the assertions above are detecting the
        // blindness and not merely restating what the command always prints.
        let (ok_code, ok_text) = run(&store, &mut |_, prs| {
            prs.iter().map(|pr| (*pr, PrDisposition::Merged)).collect()
        });
        assert_eq!(ok_code, ExitCode::SUCCESS);
        assert!(ok_text.contains("unresolved=0"), "{ok_text}");
        assert!(
            ok_text.contains("No unconsumed terminal verdicts need action."),
            "{ok_text}"
        );
    }

    /// Resolution must cost one call per repository, not one per record. The
    /// per-record shape is the documented rate-limit hazard this avoids.
    #[test]
    fn resolution_costs_one_call_per_repository_regardless_of_record_count() {
        let mut states = Vec::new();
        for pr in 500_u64..510 {
            states.push(finished_state("acme/widget", pr, "failed", "fail"));
        }
        states.push(finished_state("acme/gadget", 900, "failed", "fail"));
        let (_temp, store) = store_with(&states);

        let mut calls: Vec<(String, usize)> = Vec::new();
        let mut out = Vec::new();
        verdicts(
            &store,
            &mut |repo, prs| {
                calls.push((repo.to_owned(), prs.len()));
                prs.iter().map(|pr| (*pr, PrDisposition::Open)).collect()
            },
            false,
            &mut out,
        )
        .expect("verdicts");

        assert_eq!(calls.len(), 2, "two repositories, two calls: {calls:?}");
        let widget = calls.iter().find(|(repo, _)| repo == "acme/widget");
        assert_eq!(
            widget.map(|(_, count)| *count),
            Some(10),
            "all ten candidates must be batched into the single widget call"
        );
    }

    /// A pass and an in-flight ship must cost no lookup at all — the first
    /// needs nobody, the second has no verdict yet.
    #[test]
    fn passing_and_in_flight_records_never_reach_the_resolver() {
        let passing = finished_state("acme/widget", 600, "completed", "pass");
        let in_flight = ShipState::new(
            601,
            "acme/widget",
            "feature/601",
            "main",
            "b".repeat(40),
            "sig",
        );
        let (_temp, store) = store_with(&[passing, in_flight]);

        let mut calls = 0_usize;
        let mut out = Vec::new();
        let code = verdicts(
            &store,
            &mut |_, _| {
                calls += 1;
                BTreeMap::new()
            },
            false,
            &mut out,
        )
        .expect("verdicts");
        let text = String::from_utf8(out).expect("utf8");

        assert_eq!(calls, 0, "no candidate means no GitHub call");
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(text.contains("terminal=1"), "the pass is terminal: {text}");
        assert!(text.contains("passed=1"), "{text}");
        assert!(text.contains("scanned=2"), "{text}");

        // CONTROL: a failing record in the same store does reach the resolver,
        // proving the zero above is a real skip and not a dead code path.
        store
            .save(&finished_state("acme/widget", 602, "failed", "fail"))
            .expect("save");
        let mut calls_after = 0_usize;
        let mut out_after = Vec::new();
        verdicts(
            &store,
            &mut |_, _| {
                calls_after += 1;
                BTreeMap::new()
            },
            false,
            &mut out_after,
        )
        .expect("verdicts");
        assert_eq!(calls_after, 1);
    }

    /// A long unresolved tail must be summarised, not dumped — but the count
    /// must stay exact, because the count is what forbids an all-clear.
    #[test]
    fn a_long_unresolved_tail_is_summarised_while_its_count_stays_exact() {
        let mut states = Vec::new();
        for pr in 800_u64..825 {
            states.push(finished_state("acme/widget", pr, "failed", "fail"));
        }
        let (_temp, store) = store_with(&states);
        let (code, text) = run(&store, &mut |_, _| BTreeMap::new());

        assert_eq!(code, ExitCode::from(VERDICTS_EXIT_UNRESOLVED));
        assert!(text.contains("Unresolved (25)"), "{text}");
        assert!(text.contains("and 15 more unresolved"), "{text}");
        assert!(text.contains("unresolved=25"), "count stays exact: {text}");
        let listed = text.matches("pr=unknown").count();
        assert_eq!(listed, 10, "only the bounded sample is listed: {listed}");

        // CONTROL: a short tail is listed in full with no summary line, proving
        // the truncation above is the cap working and not a fixed ceiling.
        let (_temp2, small) = store_with(&[finished_state("acme/widget", 900, "failed", "fail")]);
        let (_, small_text) = run(&small, &mut |_, _| BTreeMap::new());
        assert!(small_text.contains("Unresolved (1)"), "{small_text}");
        assert!(!small_text.contains("more unresolved"), "{small_text}");
        assert_eq!(small_text.matches("pr=unknown").count(), 1);
    }

    /// The JSON envelope must carry the census, so an automated consumer can
    /// apply the same all-clear test a human reads off the text output.
    #[test]
    fn the_json_envelope_carries_the_census_and_its_all_clear_judgement() {
        let (_temp, store) = store_with(&[finished_state("acme/widget", 700, "cancelled", "fail")]);
        let mut out = Vec::new();
        verdicts(&store, &mut |_, _| BTreeMap::new(), true, &mut out).expect("verdicts");
        let value: serde_json::Value =
            serde_json::from_slice(&out).expect("envelope must be valid json");

        assert_eq!(value["command"], "verdicts");
        assert_eq!(value["census"]["unresolved"], 1);
        assert_eq!(value["census"]["cancelled"], 1);
        assert_eq!(value["census"]["supports_all_clear"], false);
        assert_eq!(value["verdicts"][0]["verdict"], "cancelled");
        assert_eq!(value["verdicts"][0]["unresolved"], true);
    }
}
