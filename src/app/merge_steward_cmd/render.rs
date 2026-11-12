use super::{BTreeMap, CliFailure, Path, RepoReport, Value, Write, write_json_envelope};

#[allow(clippy::too_many_lines)] // One flat text layout per field; splitting it hides the order.
pub(super) fn render_report<W: Write>(
    stdout: &mut W,
    json_output: bool,
    apply: bool,
    ledger_path: &Path,
    reports: &[RepoReport],
) -> Result<(), CliFailure> {
    if json_output {
        let mut data = BTreeMap::new();
        data.insert("apply".to_owned(), Value::from(apply));
        data.insert(
            "handoff_ledger".to_owned(),
            Value::from(ledger_path.display().to_string()),
        );
        data.insert("api_cost".to_owned(), api_cost(reports));
        data.insert(
            "repos".to_owned(),
            serde_json::to_value(reports).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        return write_json_envelope(stdout, "runner.steward", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(
        stdout,
        "merge steward: mode={} handoff_ledger={}",
        if apply { "apply" } else { "dry-run" },
        ledger_path.display()
    )
    .map_err(|error| io_failure(&error))?;
    for repo in reports {
        writeln!(
            stdout,
            "{} base={} path={} queue={} native_auto_merge={} required={}",
            repo.repo,
            repo.base,
            repo.merge_path,
            repo.merge_queue,
            repo.allow_auto_merge,
            if repo.required_contexts.is_empty() {
                "not-authoritative".to_owned()
            } else {
                repo.required_contexts.join(",")
            }
        )
        .map_err(|error| io_failure(&error))?;
        if let Some(api) = &repo.api_calls {
            writeln!(
                stdout,
                "  github: rest={} graphql={} cache_hits={}",
                api.rest, api.graphql, api.cache_hits
            )
            .map_err(|error| io_failure(&error))?;
        }
        if !repo.invariants.is_empty() {
            writeln!(
                stdout,
                "  invariants={}",
                super::invariants::summary_token(&repo.invariants)
            )
            .map_err(|error| io_failure(&error))?;
            for invariant in repo.invariants.iter().filter(|row| row.incident()) {
                writeln!(
                    stdout,
                    "  INCIDENT {}: {} fix: {}",
                    invariant.id,
                    invariant.detail,
                    invariant.fix.as_deref().unwrap_or_default()
                )
                .map_err(|error| io_failure(&error))?;
            }
        }
        if let Some(dwell) = &repo.dwell {
            writeln!(
                stdout,
                "  dwell: tracked={} overdue={} escalations_new={} act_mode={:?} next={}",
                dwell.tracked,
                dwell.overdue.len(),
                dwell.escalations_new,
                dwell.act_mode,
                dwell
                    .next_to_admit
                    .iter()
                    .map(|row| format!("#{}", row.number))
                    .collect::<Vec<_>>()
                    .join(",")
            )
            .map_err(|error| io_failure(&error))?;
        }
        writeln!(
            stdout,
            "  stale-pr-run-wedge policy={} candidates={} receipts={}",
            repo.stale_pr_run_wedge.policy,
            repo.stale_pr_run_wedge.candidates.len(),
            repo.stale_pr_run_wedge.receipts.len(),
        )
        .map_err(|error| io_failure(&error))?;
        for pr in &repo.prs {
            writeln!(
                stdout,
                "  #{} {} {:?}{}{}",
                pr.number,
                pr.head_sha.chars().take(12).collect::<String>(),
                pr.decision,
                pr.mutation
                    .as_ref()
                    .map_or_else(String::new, |value| format!(" mutation={value}")),
                pr.error
                    .as_ref()
                    .map_or_else(String::new, |value| format!(" ERROR={value}"))
            )
            .map_err(|error| io_failure(&error))?;
            if let Some(next) = &pr.blocked.next_action {
                writeln!(
                    stdout,
                    "    blocked_on={} next({:?}): {}",
                    pr.blocked.blocked_on, next.actor, next.action
                )
                .map_err(|error| io_failure(&error))?;
            }
        }
        for cancellation in &repo.cancellations {
            writeln!(
                stdout,
                "  run {} cancel={}{}{}",
                cancellation.run_id,
                cancellation.reason,
                cancellation
                    .mutation
                    .as_ref()
                    .map_or_else(String::new, |value| format!(" mutation={value}")),
                cancellation
                    .error
                    .as_ref()
                    .map_or_else(String::new, |value| format!(" ERROR={value}"))
            )
            .map_err(|error| io_failure(&error))?;
        }
        for error in &repo.errors {
            writeln!(stdout, "  ERROR: {error}").map_err(|error| io_failure(&error))?;
        }
    }
    Ok(())
}

/// The whole pass's GitHub cost: REST and GraphQL children and cache hits,
/// summed over every repository, plus the per-repository split.
fn api_cost(reports: &[RepoReport]) -> Value {
    let (mut rest, mut graphql, mut cache_hits, mut millis) = (0_u64, 0_u64, 0_u64, 0_u128);
    let mut by_repo = serde_json::Map::new();
    for report in reports {
        let Some(api) = &report.api_calls else {
            continue;
        };
        rest += api.rest;
        graphql += api.graphql;
        cache_hits += api.cache_hits;
        millis += api.millis;
        by_repo.insert(
            report.repo.clone(),
            serde_json::json!({"rest": api.rest, "graphql": api.graphql, "cache_hits": api.cache_hits}),
        );
    }
    serde_json::json!({
        "rest": rest,
        "graphql": graphql,
        "cache_hits": cache_hits,
        "millis": u64::try_from(millis).unwrap_or(u64::MAX),
        "by_repo": by_repo,
    })
}

pub(super) fn io_failure(error: &std::io::Error) -> CliFailure {
    CliFailure::new(1, error.to_string())
}

pub(super) fn is_private_free_entitlement(message: &str) -> bool {
    message.contains("Upgrade to GitHub Pro or make this repository public")
}

pub(super) fn is_admin_protection_denied(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("http 403")
        && (lower.contains("must have admin rights")
            || lower.contains("administration permission")
            || lower.contains("resource not accessible by integration"))
}

pub(super) fn enqueue_requirements_pending(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    if [
        "http 401",
        "http 403",
        "http 429",
        "bad credentials",
        "resource not accessible by integration",
        "rate limit",
        "too many requests",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return false;
    }
    lower.contains("required status check")
        || lower.contains("required check")
        || lower.contains("required approving review")
        || lower.contains("required review")
        || lower.contains("requirements are not met")
}
