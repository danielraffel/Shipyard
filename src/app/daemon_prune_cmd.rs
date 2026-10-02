//! `shipyard daemon prune-webhooks`: list, then optionally delete, Shipyard
//! webhooks that no live advertising daemon owns, and drop the matching stale
//! records from this host's `registrations.json`.
//!
//! A dry run is the default; nothing changes without `--apply`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::{Value, json};

use super::CliFailure;
use crate::daemon_ipc::read_daemon_status;
use crate::daemon_runtime::normalize_repos;
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;
use crate::registrar::{Registrar, SUBSCRIBED_EVENTS};
use crate::webhook_prune::{
    HookFacts, HostContext, Verdict, classify_hook, daemon_callback_host, delivery_facts,
    has_daemon_events, stale_registrations,
};
use crate::webhook_reconcile::HostIdentity;

/// Deliveries per page read for a peer hook.
const PRUNE_DELIVERY_PAGE: usize = 100;
/// Pages read at most per peer hook. GitHub meters delivery reads separately
/// (500 an hour), and a busy repository can take several pages to span a day.
const PRUNE_DELIVERY_MAX_PAGES: usize = 5;

/// Read a peer hook's deliveries, newest first, until the window holds a
/// delivered or answered attempt (the hook is live; stop), spans the
/// dead-target threshold, or runs out of pages. `None` when the first page
/// could not be read.
fn read_failure_window(
    registrar: &Registrar,
    repo: &str,
    hook_id: u64,
) -> Option<Vec<crate::webhook_prune::DeliveryFact>> {
    let mut facts = Vec::new();
    let mut cursor: Option<String> = None;
    for page in 0..PRUNE_DELIVERY_MAX_PAGES {
        let Ok((records, next)) = registrar.list_hook_deliveries_page(
            repo,
            hook_id,
            PRUNE_DELIVERY_PAGE,
            cursor.as_deref(),
        ) else {
            return (page > 0).then_some(facts);
        };
        facts.extend(delivery_facts(&records));
        if crate::webhook_prune::window_is_conclusive(&facts) {
            break;
        }
        match next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Some(facts)
}

#[derive(Debug)]
struct HookRow {
    repo: String,
    hook_id: u64,
    url: String,
    verdict: Verdict,
    applied: Option<Result<(), String>>,
}

#[derive(Debug)]
struct RegistrationRow {
    repo: String,
    hook_id: u64,
    reason: &'static str,
    applied: Option<Result<(), String>>,
}

/// Everything one run found and, with `--apply`, did.
struct PruneReport {
    identity: HostIdentity,
    advertised: Option<BTreeSet<String>>,
    advertised_untrusted: Option<String>,
    apply: bool,
    hooks: Vec<HookRow>,
    registrations: Vec<RegistrationRow>,
    unreadable: Vec<(String, String)>,
}

impl PruneReport {
    fn failed(&self) -> bool {
        !self.unreadable.is_empty()
            || self
                .hooks
                .iter()
                .map(|row| &row.applied)
                .chain(self.registrations.iter().map(|row| &row.applied))
                .any(|applied| matches!(applied, Some(Err(_))))
    }
}

pub(super) fn daemon_prune_webhooks<W: Write>(
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
    requested_repos: &[String],
    apply: bool,
) -> Result<ExitCode, CliFailure> {
    let identity = crate::tunnel::probe_tailscale().host_identity();
    let this_host = match &identity {
        HostIdentity::Known(name) => Some(name.clone()),
        HostIdentity::Unreadable { .. } => None,
    };
    let (advertised, advertised_untrusted) = match advertised_repos(&runtime_paths.state_dir) {
        Ok(repos) => (Some(repos), None),
        Err(reason) => (None, Some(reason)),
    };
    let tailnet_nodes = crate::tunnel::probe_tailnet_node_names();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut registrar = Registrar::new_with_context(mode, &runtime_paths.state_dir, &cwd);
    let registrations = registrar.all();
    let repos = scanned_repos(requested_repos, advertised.as_ref(), &registrations);
    let context = HostContext {
        this_host: this_host.as_deref(),
        advertised: advertised.as_ref(),
        tailnet_nodes: tailnet_nodes.as_ref(),
    };

    let mut unreadable = Vec::new();
    let mut live_hook_ids = BTreeMap::new();
    let mut hooks = Vec::new();
    for repo in &repos {
        match registrar.list_repo_hooks(repo) {
            Ok(repo_hooks) => {
                live_hook_ids.insert(
                    repo.clone(),
                    repo_hooks
                        .iter()
                        .filter_map(|hook| hook.get("id").and_then(Value::as_u64))
                        .collect::<BTreeSet<_>>(),
                );
                hooks.extend(classify_repo_hooks(&registrar, repo, &repo_hooks, &context));
            }
            Err(error) => unreadable.push((repo.clone(), error.to_string())),
        }
    }
    let registrations = stale_registrations(&registrations, advertised.as_ref(), &live_hook_ids)
        .into_iter()
        .map(|(repo, hook_id, reason)| RegistrationRow {
            repo,
            hook_id,
            reason: reason.describe(),
            applied: None,
        })
        .collect();

    let mut report = PruneReport {
        identity,
        advertised,
        advertised_untrusted,
        apply,
        hooks,
        registrations,
        unreadable,
    };
    if apply {
        apply_prune(&mut registrar, &mut report);
    }
    if json {
        render_json(stdout, &report)
    } else {
        render_text(stdout, &report)
    }
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(if report.failed() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Repositories the running daemon advertises, or why that set is not
/// trusted: no daemon answered, or it runs a different Shipyard version than
/// this command, so its status may describe a configuration since changed.
fn advertised_repos(state_dir: &std::path::Path) -> Result<BTreeSet<String>, String> {
    let status =
        read_daemon_status(state_dir).ok_or_else(|| "no running daemon answered".to_owned())?;
    let running = status
        .get("shipyard_version")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if running != env!("CARGO_PKG_VERSION") {
        return Err(format!(
            "running daemon is {running}, this command is {}; refresh the daemon first",
            env!("CARGO_PKG_VERSION")
        ));
    }
    let repos = status
        .get("configured_repos")
        .and_then(Value::as_array)
        .ok_or_else(|| "daemon status carried no configured_repos".to_owned())?;
    Ok(normalize_repos(
        repos
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
    )
    .into_iter()
    .collect())
}

fn classify_repo_hooks(
    registrar: &Registrar,
    repo: &str,
    hooks: &[Value],
    context: &HostContext<'_>,
) -> Vec<HookRow> {
    hooks
        .iter()
        .map(hook_facts)
        .filter(is_daemon_hook)
        .map(|mut facts| {
            if context.this_host != daemon_callback_host(&facts.url) {
                facts.deliveries = read_failure_window(registrar, repo, facts.id);
            }
            HookRow {
                repo: repo.to_owned(),
                hook_id: facts.id,
                verdict: classify_hook(repo, &facts, context, &SUBSCRIBED_EVENTS),
                url: facts.url,
                applied: None,
            }
        })
        .collect()
}

fn apply_prune(registrar: &mut Registrar, report: &mut PruneReport) {
    for row in &mut report.hooks {
        if matches!(row.verdict, Verdict::Prune(_)) {
            row.applied = Some(
                registrar
                    .delete_repo_hook(&row.repo, row.hook_id)
                    .map_err(|error| error.to_string()),
            );
        }
    }
    for row in &mut report.registrations {
        row.applied = Some(
            registrar
                .forget(&row.repo)
                .map_err(|error| error.to_string()),
        );
    }
}

/// Explicit repositories, else everything this host advertises or has ever
/// recorded a hook for.
fn scanned_repos(
    requested: &[String],
    advertised: Option<&BTreeSet<String>>,
    registrations: &BTreeMap<String, u64>,
) -> Vec<String> {
    if !requested.is_empty() {
        return normalize_repos(requested.to_vec());
    }
    normalize_repos(
        advertised
            .into_iter()
            .flatten()
            .cloned()
            .chain(registrations.keys().cloned())
            .collect(),
    )
}

fn hook_facts(hook: &Value) -> HookFacts {
    HookFacts {
        id: hook.get("id").and_then(Value::as_u64).unwrap_or_default(),
        url: hook
            .get("config")
            .and_then(|config| config.get("url"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        events: hook
            .get("events")
            .and_then(Value::as_array)
            .map(|events| {
                events
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        deliveries: None,
    }
}

fn is_daemon_hook(facts: &HookFacts) -> bool {
    daemon_callback_host(&facts.url).is_some()
        && has_daemon_events(&facts.events, &SUBSCRIBED_EVENTS)
}

fn verdict_parts(verdict: &Verdict) -> (&'static str, String) {
    match verdict {
        Verdict::Foreign => ("foreign", String::new()),
        Verdict::Keep(reason) => ("keep", reason.clone()),
        Verdict::Prune(reason) => ("prune", reason.describe()),
        Verdict::Undecided(reason) => ("undecided", reason.clone()),
    }
}

fn applied_text(applied: Option<&Result<(), String>>) -> Value {
    match applied {
        None => Value::Null,
        Some(Ok(())) => json!("done"),
        Some(Err(error)) => json!(format!("failed: {error}")),
    }
}

fn identity_text(identity: &HostIdentity) -> String {
    match identity {
        HostIdentity::Known(name) => name.clone(),
        HostIdentity::Unreadable { detail } => format!("UNREADABLE ({detail})"),
    }
}

fn render_json<W: Write>(
    stdout: &mut W,
    report: &PruneReport,
) -> Result<(), Box<dyn std::error::Error>> {
    let hooks = report
        .hooks
        .iter()
        .map(|row| {
            let (verdict, reason) = verdict_parts(&row.verdict);
            json!({
                "repo": row.repo,
                "hook_id": row.hook_id,
                "url": row.url,
                "verdict": verdict,
                "reason": reason,
                "applied": applied_text(row.applied.as_ref()),
            })
        })
        .collect();
    let registrations = report
        .registrations
        .iter()
        .map(|row| {
            json!({
                "repo": row.repo,
                "hook_id": row.hook_id,
                "reason": row.reason,
                "applied": applied_text(row.applied.as_ref()),
            })
        })
        .collect();
    let unreadable = report
        .unreadable
        .iter()
        .map(|(repo, error)| json!({"repo": repo, "error": error}))
        .collect();
    let data = BTreeMap::from([
        (
            "identity".to_owned(),
            json!(identity_text(&report.identity)),
        ),
        ("apply".to_owned(), json!(report.apply)),
        (
            "advertised".to_owned(),
            report
                .advertised
                .as_ref()
                .map_or(Value::Null, |repos| json!(repos)),
        ),
        (
            "advertised_untrusted".to_owned(),
            json!(report.advertised_untrusted),
        ),
        ("hooks".to_owned(), Value::Array(hooks)),
        ("registrations".to_owned(), Value::Array(registrations)),
        ("unreadable".to_owned(), Value::Array(unreadable)),
    ]);
    write_json_envelope(stdout, "daemon:prune-webhooks", data)
}

fn applied_suffix(applied: Option<&Result<(), String>>, done: &str) -> String {
    match applied {
        None => String::new(),
        Some(Ok(())) => format!(" -> {done}"),
        Some(Err(error)) => format!(" -> FAILED: {error}"),
    }
}

fn render_text<W: Write>(
    stdout: &mut W,
    report: &PruneReport,
) -> Result<(), Box<dyn std::error::Error>> {
    writeln!(
        stdout,
        "host identity: {}\nadvertised: {}\nmode: {}",
        identity_text(&report.identity),
        report.advertised.as_ref().map_or_else(
            || {
                format!(
                    "not trusted ({}); this host's own hooks are left undecided",
                    report.advertised_untrusted.as_deref().unwrap_or("unknown")
                )
            },
            |repos| repos.iter().cloned().collect::<Vec<_>>().join(", ")
        ),
        if report.apply {
            "apply"
        } else {
            "dry run (pass --apply to delete)"
        }
    )?;
    for row in &report.hooks {
        let (verdict, reason) = verdict_parts(&row.verdict);
        writeln!(
            stdout,
            "[{verdict}] {} hook {} {}: {reason}{}",
            row.repo,
            row.hook_id,
            row.url,
            applied_suffix(row.applied.as_ref(), "deleted")
        )?;
    }
    for row in &report.registrations {
        writeln!(
            stdout,
            "[stale-registration] {} hook {}: {}{}",
            row.repo,
            row.hook_id,
            row.reason,
            applied_suffix(row.applied.as_ref(), "dropped")
        )?;
    }
    for (repo, error) in &report.unreadable {
        writeln!(stdout, "[unreadable] {repo}: {error}")?;
    }
    let prunable = report
        .hooks
        .iter()
        .filter(|row| matches!(row.verdict, Verdict::Prune(_)))
        .count();
    writeln!(
        stdout,
        "summary: {} daemon hooks read, {prunable} prunable, {} stale registrations, {} repositories unreadable",
        report.hooks.len(),
        report.registrations.len(),
        report.unreadable.len()
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanned_repos_cover_advertised_and_recorded_repositories() {
        let advertised = BTreeSet::from(["o/a".to_owned()]);
        let registrations = BTreeMap::from([("o/b".to_owned(), 2), ("o/a".to_owned(), 1)]);
        assert_eq!(
            scanned_repos(&[], Some(&advertised), &registrations),
            vec!["o/a".to_owned(), "o/b".to_owned()]
        );
        assert_eq!(
            scanned_repos(&["O/C".to_owned()], Some(&advertised), &registrations),
            vec!["o/c".to_owned()]
        );
    }

    #[test]
    fn dry_run_render_lists_every_verdict_and_changes_nothing() {
        let hooks = vec![HookRow {
            repo: "o/r".to_owned(),
            hook_id: 9,
            url: "https://gone.ts.net/webhook".to_owned(),
            verdict: Verdict::Prune(crate::webhook_prune::PruneReason::DeadTarget {
                failures: 30,
                span_hours: 58,
            }),
            applied: None,
        }];
        let registrations = vec![RegistrationRow {
            repo: "o/old".to_owned(),
            hook_id: 4,
            reason: "GitHub no longer holds the recorded hook",
            applied: None,
        }];
        let report = PruneReport {
            identity: HostIdentity::Known("me.ts.net".to_owned()),
            advertised: None,
            advertised_untrusted: Some("no running daemon answered".to_owned()),
            apply: false,
            hooks,
            registrations,
            unreadable: Vec::new(),
        };
        let mut out = Vec::new();
        render_text(&mut out, &report).expect("render");
        assert!(!report.failed());
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("dry run (pass --apply to delete)"), "{text}");
        assert!(
            text.contains("[prune] o/r hook 9 https://gone.ts.net/webhook"),
            "{text}"
        );
        assert!(text.contains("[stale-registration] o/old hook 4"), "{text}");
        assert!(
            !text.contains("deleted") && !text.contains("dropped"),
            "{text}"
        );
    }
}
