//! `shipyard ci profile apply` -- proof-gate a profile and write its routes.
//!
//! This is the point where a routing profile becomes live: the GitHub
//! repository variables that decide where jobs land. Every lane is proved
//! before its variable is written, and the command is dry-run by default, so
//! the destructive act is always explicit.
//!
//! Observation lives here; the decision lives in [`crate::profile_apply`],
//! which is pure and tested without a network.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use super::CliFailure;
use crate::ci_profile::{CiProfile, HealthLease, Lane};
use crate::cloud::GitHubActions;
use crate::output::write_pretty_json;
use crate::profile_apply::{LaneObservation, LaneVerdict, evaluate_lane};

/// Arguments for one apply run.
pub(super) struct ProfileApplyArgs {
    pub(super) name: String,
    pub(super) repo: String,
    pub(super) context: String,
    pub(super) apply: bool,
    pub(super) max_evidence_age_days: u32,
    pub(super) topology_check: Option<PathBuf>,
    pub(super) profile_file: Option<PathBuf>,
}

/// What the command did, in a shape agents can consume.
#[derive(Serialize)]
struct ApplyReport {
    profile: String,
    repo: String,
    context: String,
    source: String,
    /// False on a dry run. The field is named for what happened, not what was
    /// requested, so a report can never imply a write that did not occur.
    applied: bool,
    lanes: Vec<LaneReport>,
    written: Vec<String>,
    blocked: Vec<String>,
    /// Lanes that passed every gate but declare no variable to write.
    skipped: Vec<String>,
}

#[derive(Serialize)]
struct LaneReport {
    #[serde(flatten)]
    verdict: LaneVerdict,
    observation: LaneObservation,
    writable: bool,
    mutation: String,
}

pub(super) fn profile_apply_command<W: Write>(
    args: ProfileApplyArgs,
    cwd: &Path,
    actions: &GitHubActions,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let (source, profile) =
        super::ci_cmd::load_typed_profile(cwd, &args.name, args.profile_file.as_deref())?;
    let lanes = profile
        .context(&args.repo, &args.context)
        .map_err(|error| CliFailure::new(2, error.to_string()))?
        .clone();
    let actions = actions.clone().with_repo_override(&args.repo);

    // The topology check is repo-wide, so it runs once rather than per lane.
    let topology_check_passed = run_topology_check(cwd, args.topology_check.as_deref());

    let mut reports = Vec::new();
    let mut written = Vec::new();
    let mut blocked = Vec::new();
    let mut skipped = Vec::new();

    for (lane_name, lane) in &lanes {
        let observation = observe_lane(&actions, &profile, &args.repo, lane, topology_check_passed);
        let verdict = evaluate_lane(
            &profile,
            &args.context,
            lane_name,
            lane,
            &observation,
            args.max_evidence_age_days,
        );
        let writable = verdict.writable();
        let gates_blocked = !verdict.blocking().is_empty();

        let mutation = if gates_blocked {
            blocked.push(format!("{}.{lane_name}", args.context));
            "blocked".to_owned()
        } else if !writable {
            // Every gate passed but the lane publishes no variable. That is a
            // lane with nothing to write, not a failure -- reporting it as
            // blocked would make the exit code claim a problem that is not one.
            skipped.push(format!("{}.{lane_name}", args.context));
            "nothing_to_write".to_owned()
        } else if args.apply {
            let variable = verdict
                .variable
                .clone()
                .expect("writable lane has variable");
            let value = verdict.value.clone().expect("writable lane has value");
            match write_variable(&actions, &args.repo, &variable, &value) {
                Ok(mutation) => {
                    written.push(variable);
                    mutation
                }
                Err(error) => {
                    blocked.push(format!("{}.{lane_name}", args.context));
                    format!("write_failed: {}", error.message)
                }
            }
        } else {
            "dry_run".to_owned()
        };

        reports.push(LaneReport {
            verdict,
            observation,
            writable,
            mutation,
        });
    }

    let report = ApplyReport {
        profile: args.name,
        repo: args.repo,
        context: args.context,
        source: source.display().to_string(),
        applied: args.apply,
        lanes: reports,
        written,
        blocked,
        skipped,
    };

    if json {
        write_pretty_json(stdout, &report)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        print_report(stdout, &report).map_err(|error| CliFailure::new(1, error.to_string()))?;
    }

    // A blocked lane is a non-zero exit even on a dry run: an agent scripting
    // this needs "nothing is applyable yet" to be a failure it can branch on.
    Ok(if report.blocked.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Gather live GitHub state for one lane.
///
/// Every read is best-effort: an unreadable observation stays `false`/`None`,
/// which fails its gate closed rather than passing on missing evidence.
fn observe_lane(
    actions: &GitHubActions,
    profile: &CiProfile,
    repo: &str,
    lane: &Lane,
    topology_check_passed: bool,
) -> LaneObservation {
    let target = lane.targets.first().and_then(|id| profile.target(id));
    let (runner_group_found, runner_group_allows_repo) = target
        .and_then(|target| target.runner_group.as_deref())
        .map_or((false, false), |group| {
            observe_runner_group(actions, repo, group)
        });
    let evidence_age_days = target
        .and_then(|target| target.evidence_job_pattern.as_deref())
        .and_then(|pattern| observe_evidence_age_days(actions, repo, pattern));
    let lease_age_seconds = HealthLease::from_fields(&lane.health_lease)
        .ok()
        .flatten()
        .and_then(|lease| observe_lease_age_seconds(actions, repo, &lease.variable));

    LaneObservation {
        runner_group_found,
        runner_group_allows_repo,
        evidence_age_days,
        topology_check_passed,
        lease_age_seconds,
    }
}

/// Whether a runner group exists and grants this repository workflow access.
fn observe_runner_group(actions: &GitHubActions, repo: &str, group: &str) -> (bool, bool) {
    let owner = repo.split('/').next().unwrap_or_default();
    let Ok(raw) = actions.run_gh(&[
        "api".to_owned(),
        "--paginate".to_owned(),
        format!("orgs/{owner}/actions/runner-groups"),
    ]) else {
        return (false, false);
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return (false, false);
    };
    let Some(found) = value
        .get("runner_groups")
        .and_then(Value::as_array)
        .and_then(|groups| {
            groups
                .iter()
                .find(|entry| entry.get("name").and_then(Value::as_str) == Some(group))
        })
    else {
        return (false, false);
    };

    // A group visible to every repository in the org trivially allows this
    // one; otherwise the repository must appear on the group's allowlist.
    let visibility = found.get("visibility").and_then(Value::as_str);
    if matches!(visibility, Some("all")) {
        return (true, true);
    }
    let Some(url) = found
        .get("selected_repositories_url")
        .and_then(Value::as_str)
    else {
        return (true, false);
    };
    let Ok(raw) = actions.run_gh(&[
        "api".to_owned(),
        "--paginate".to_owned(),
        url.trim_start_matches("https://api.github.com/").to_owned(),
    ]) else {
        return (true, false);
    };
    let allows = serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|value| {
            value
                .get("repositories")
                .and_then(Value::as_array)
                .map(|repos| {
                    repos
                        .iter()
                        .any(|entry| entry.get("full_name").and_then(Value::as_str) == Some(repo))
                })
        })
        .unwrap_or(false);
    (true, allows)
}

/// Age in days of the most recent completed run whose name matches `pattern`.
fn observe_evidence_age_days(actions: &GitHubActions, repo: &str, pattern: &str) -> Option<u32> {
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/actions/runs?per_page=100"),
        ])
        .ok()?;
    let value = serde_json::from_str::<Value>(&raw).ok()?;
    let runs = value.get("workflow_runs").and_then(Value::as_array)?;
    let needle = pattern.to_ascii_lowercase();
    let newest = runs
        .iter()
        .filter(|run| {
            [run.get("name"), run.get("display_title")]
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|field| field.to_ascii_lowercase().contains(&needle))
        })
        .filter_map(|run| run.get("created_at").and_then(Value::as_str))
        .filter_map(|created| DateTime::parse_from_rfc3339(created).ok())
        .max()?;
    let age = Utc::now().signed_duration_since(newest.with_timezone(&Utc));
    u32::try_from(age.num_days().max(0)).ok()
}

/// Age in seconds of a published health lease, or `None` when it is absent,
/// unparseable, or already expired.
fn observe_lease_age_seconds(actions: &GitHubActions, repo: &str, variable: &str) -> Option<i64> {
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/actions/variables/{variable}"),
        ])
        .ok()?;
    let value = serde_json::from_str::<Value>(&raw).ok()?;
    let expires_at = value.get("value").and_then(Value::as_str)?;
    let expires_at = DateTime::parse_from_rfc3339(expires_at).ok()?;
    let now = Utc::now();
    if expires_at.with_timezone(&Utc) < now {
        // An expired lease is not a stale lease; it is no lease at all.
        return None;
    }
    // Report how long ago the publisher last wrote, derived from the expiry.
    Some(
        now.signed_duration_since(expires_at.with_timezone(&Utc))
            .num_seconds()
            .abs(),
    )
}

/// Run the repository's runner-topology checker.
///
/// A missing or unrunnable checker reports `false`, so the gate fails closed
/// rather than passing because nothing ran.
fn run_topology_check(cwd: &Path, explicit: Option<&Path>) -> bool {
    let script = explicit.map_or_else(
        || cwd.join("tools/scripts/runner_topology_check.py"),
        Path::to_path_buf,
    );
    if !script.exists() {
        return false;
    }
    Command::new("python3")
        .arg(&script)
        .current_dir(cwd)
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Create or update one repository variable.
fn write_variable(
    actions: &GitHubActions,
    repo: &str,
    variable: &str,
    value: &str,
) -> Result<String, CliFailure> {
    let patch = vec![
        "api".to_owned(),
        "--method".to_owned(),
        "PATCH".to_owned(),
        format!("repos/{repo}/actions/variables/{variable}"),
        "-f".to_owned(),
        format!("name={variable}"),
        "-f".to_owned(),
        format!("value={value}"),
    ];
    match actions.run_gh(&patch) {
        Ok(_) => Ok("updated".to_owned()),
        Err(error) if error.to_string().contains("404") => {
            let create = vec![
                "api".to_owned(),
                "--method".to_owned(),
                "POST".to_owned(),
                format!("repos/{repo}/actions/variables"),
                "-f".to_owned(),
                format!("name={variable}"),
                "-f".to_owned(),
                format!("value={value}"),
            ];
            actions.run_gh(&create).map_err(|create_error| {
                CliFailure::new(1, format!("failed to create {variable}: {create_error}"))
            })?;
            Ok("created".to_owned())
        }
        Err(error) => Err(CliFailure::new(
            1,
            format!("failed to update {variable}: {error}"),
        )),
    }
}

fn print_report<W: Write>(stdout: &mut W, report: &ApplyReport) -> std::io::Result<()> {
    writeln!(
        stdout,
        "{} profile {} for {} context {} ({})",
        if report.applied {
            "Applying"
        } else {
            "Dry run of"
        },
        report.profile,
        report.repo,
        report.context,
        report.source
    )?;
    for lane in &report.lanes {
        writeln!(stdout)?;
        writeln!(
            stdout,
            "{}.{} -> {}",
            lane.verdict.context,
            lane.verdict.lane,
            lane.verdict.target.as_deref().unwrap_or("(unresolved)")
        )?;
        for gate in &lane.verdict.gates {
            writeln!(stdout, "    {gate}")?;
        }
        match (&lane.verdict.variable, &lane.verdict.value) {
            (Some(variable), Some(value)) if lane.writable => {
                writeln!(stdout, "  would set {variable}={value} [{}]", lane.mutation)?;
            }
            (Some(variable), _) => {
                writeln!(stdout, "  {variable} NOT written [{}]", lane.mutation)?;
            }
            _ => writeln!(
                stdout,
                "  no github_variable declared, nothing to write [{}]",
                lane.mutation
            )?,
        }
    }
    writeln!(stdout)?;
    if report.applied {
        writeln!(stdout, "written: {}", display_list(&report.written))?;
    } else {
        writeln!(
            stdout,
            "dry run -- nothing was written. Re-run with --apply to write {} lane(s).",
            report.lanes.iter().filter(|lane| lane.writable).count()
        )?;
    }
    if !report.skipped.is_empty() {
        writeln!(stdout, "skipped: {}", display_list(&report.skipped))?;
    }
    if !report.blocked.is_empty() {
        writeln!(stdout, "blocked: {}", display_list(&report.blocked))?;
    }
    Ok(())
}

fn display_list(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_owned()
    } else {
        items.join(", ")
    }
}
