//! External health-lease operator for Pulp's disposable Mac Pro Linux pool.
//!
//! The consumer workflow reads an RFC3339 repository variable before choosing
//! its `runs-on` value. This command is the trusted producer: it observes the
//! registered runner fleet using Shipyard's configured GitHub auth and renews
//! the short lease only while matching idle capacity is visible. Any unhealthy
//! or unreadable observation clears the variable, so new jobs fall back to the
//! hosted selector. A crashed operator is bounded by the lease expiry.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::Value;

use super::CliFailure;
use crate::cloud::GitHubActions;
use crate::output::{SCHEMA_VERSION, write_json_envelope};
use crate::runner_provision::ApiLabel;

#[derive(Debug, Deserialize)]
struct LeaseRunner {
    name: String,
    status: String,
    busy: bool,
    labels: Vec<ApiLabel>,
}

#[derive(Debug, Deserialize)]
struct LeaseJob {
    status: String,
    labels: Vec<String>,
}

#[derive(Debug)]
struct FleetObservation {
    runners: Vec<LeaseRunner>,
    queued_matching_jobs: usize,
}

impl LeaseRunner {
    fn label_names(&self) -> BTreeSet<String> {
        self.labels
            .iter()
            .map(|label| label.name.to_ascii_lowercase())
            .collect()
    }
}

pub(super) struct LocalLinuxLeaseArgs {
    pub(super) repo: Option<String>,
    pub(super) profile: String,
    pub(super) profile_file: Option<PathBuf>,
    pub(super) context: String,
    pub(super) lane: String,
    pub(super) apply: bool,
    pub(super) watch: bool,
    pub(super) interval_secs: u64,
    pub(super) max_ticks: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LeaseAction {
    Renew,
    Clear,
}

impl LeaseAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Renew => "renew",
            Self::Clear => "clear",
        }
    }
}

#[derive(Debug)]
struct LeaseDecision {
    action: LeaseAction,
    observed_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    matching: usize,
    online: usize,
    idle: usize,
    queued: usize,
    available: usize,
    reason: String,
}

pub(super) fn local_linux_lease_command<W: Write>(
    args: LocalLinuxLeaseArgs,
    cwd: &Path,
    actions: &GitHubActions,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let repo = super::runner_cmd::resolve_repo_slug(args.repo, cwd)?;
    let actions = actions.clone().with_repo_override(&repo);
    let profile = super::ci_cmd::load_local_linux_lease_profile(
        cwd,
        &args.profile,
        args.profile_file.as_deref(),
        &repo,
        &args.context,
        &args.lane,
    )?;
    if args.watch && args.interval_secs >= profile.ttl_seconds {
        return Err(CliFailure::new(
            2,
            format!(
                "--interval-secs must be shorter than the {} second health lease",
                profile.ttl_seconds
            ),
        ));
    }
    if args.watch && args.interval_secs < 15 {
        return Err(CliFailure::new(
            2,
            "--interval-secs must be at least 15 to avoid API hot-looping",
        ));
    }

    let mut tick = 0u32;
    loop {
        tick += 1;
        let observed_at = Utc::now();
        let decision = match observe_fleet(&actions, &repo, &profile.required_labels) {
            Ok(observation) => decide_lease(
                &observation.runners,
                &profile.runner_name_prefix,
                &profile.required_labels,
                observation.queued_matching_jobs,
                profile.min_idle,
                profile.ttl_seconds,
                observed_at,
            ),
            Err(error) => LeaseDecision {
                action: LeaseAction::Clear,
                observed_at,
                expires_at: None,
                matching: 0,
                online: 0,
                idle: 0,
                queued: 0,
                available: 0,
                reason: format!("fleet_unreadable: {error}"),
            },
        };
        let (mutation, mutation_failed) = if args.apply {
            mutation_for_tick(
                apply_decision(&actions, &repo, &profile.variable, &decision),
                args.watch,
            )?
        } else {
            ("dry_run".to_owned(), false)
        };
        emit_decision(
            stdout,
            json,
            tick,
            &repo,
            &profile.variable,
            &profile.events,
            &decision,
            &mutation,
            args.watch,
        )?;
        let tick_exit = if decision.action == LeaseAction::Renew && !mutation_failed {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        };

        if !args.watch || args.max_ticks.is_some_and(|max| tick >= max) {
            return Ok(tick_exit);
        }
        sleep(StdDuration::from_secs(args.interval_secs));
    }
}

fn mutation_for_tick(
    result: Result<String, CliFailure>,
    watch: bool,
) -> Result<(String, bool), CliFailure> {
    match result {
        Ok(mutation) => Ok((mutation, false)),
        Err(error) if watch => Ok((format!("failed: {}", error.message), true)),
        Err(error) => Err(error),
    }
}

fn observe_fleet(
    actions: &GitHubActions,
    repo: &str,
    required_labels: &[String],
) -> Result<FleetObservation, String> {
    let queued_matching_jobs = fetch_queued_matching_jobs(actions, repo, required_labels)?;
    Ok(FleetObservation {
        runners: fetch_runners(actions, repo)?,
        queued_matching_jobs,
    })
}

fn fetch_runners(actions: &GitHubActions, repo: &str) -> Result<Vec<LeaseRunner>, String> {
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            "--paginate".to_owned(),
            format!("repos/{repo}/actions/runners?per_page=100"),
            "--jq".to_owned(),
            ".runners[]".to_owned(),
        ])
        .map_err(|error| format!("failed to list runners: {error}"))?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<LeaseRunner>(line)
                .map_err(|error| format!("runner JSON parse failed: {error}"))
        })
        .collect()
}

fn fetch_queued_matching_jobs(
    actions: &GitHubActions,
    repo: &str,
    required_labels: &[String],
) -> Result<usize, String> {
    let mut run_ids = BTreeSet::new();
    for status in ["queued", "in_progress", "requested", "waiting", "pending"] {
        let raw = actions
            .run_gh(&[
                "api".to_owned(),
                "--paginate".to_owned(),
                format!("repos/{repo}/actions/runs?status={status}&per_page=100"),
                "--jq".to_owned(),
                ".workflow_runs[].id".to_owned(),
            ])
            .map_err(|error| format!("failed to list {status} workflow runs: {error}"))?;
        for line in raw.lines().filter(|line| !line.trim().is_empty()) {
            run_ids.insert(
                line.trim()
                    .parse::<u64>()
                    .map_err(|error| format!("workflow run id parse failed: {error}"))?,
            );
        }
    }

    let required = required_labels
        .iter()
        .map(|label| label.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let mut queued = 0usize;
    for run_id in run_ids {
        let raw = actions
            .run_gh(&[
                "api".to_owned(),
                "--paginate".to_owned(),
                format!("repos/{repo}/actions/runs/{run_id}/jobs?filter=all&per_page=100"),
                "--jq".to_owned(),
                ".jobs[]".to_owned(),
            ])
            .map_err(|error| format!("failed to list jobs for workflow run {run_id}: {error}"))?;
        for line in raw.lines().filter(|line| !line.trim().is_empty()) {
            let job = serde_json::from_str::<LeaseJob>(line)
                .map_err(|error| format!("workflow job JSON parse failed: {error}"))?;
            let labels = job
                .labels
                .iter()
                .map(|label| label.to_ascii_lowercase())
                .collect::<BTreeSet<_>>();
            if job.status.eq_ignore_ascii_case("queued") && required.is_subset(&labels) {
                queued += 1;
            }
        }
    }
    Ok(queued)
}

fn decide_lease(
    runners: &[LeaseRunner],
    name_prefix: &str,
    required_labels: &[String],
    queued: usize,
    min_idle: usize,
    ttl_seconds: u64,
    observed_at: DateTime<Utc>,
) -> LeaseDecision {
    let required = required_labels
        .iter()
        .map(|label| label.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let matching = runners
        .iter()
        .filter(|runner| {
            // GitHub's repository-runner API does not expose registration
            // ephemerality. The profile prefix is therefore a dedicated,
            // controller-owned namespace in addition to the exact pool labels.
            runner.name.starts_with(name_prefix) && required.is_subset(&runner.label_names())
        })
        .collect::<Vec<_>>();
    let online = matching
        .iter()
        .filter(|runner| runner.status.eq_ignore_ascii_case("online"))
        .count();
    let idle = matching
        .iter()
        .filter(|runner| runner.status.eq_ignore_ascii_case("online") && !runner.busy)
        .count();
    let available = idle.saturating_sub(queued);
    if available >= min_idle {
        LeaseDecision {
            action: LeaseAction::Renew,
            observed_at,
            expires_at: Some(
                observed_at
                    + Duration::seconds(
                        i64::try_from(ttl_seconds).expect("profile TTL is bounded"),
                    ),
            ),
            matching: matching.len(),
            online,
            idle,
            queued,
            available,
            reason: "healthy_unreserved_idle_capacity".to_owned(),
        }
    } else {
        LeaseDecision {
            action: LeaseAction::Clear,
            observed_at,
            expires_at: None,
            matching: matching.len(),
            online,
            idle,
            queued,
            available,
            reason: format!(
                "insufficient_unreserved_idle_capacity: required={min_idle} idle={idle} queued={queued} available={available}"
            ),
        }
    }
}

fn apply_decision(
    actions: &GitHubActions,
    repo: &str,
    variable: &str,
    decision: &LeaseDecision,
) -> Result<String, CliFailure> {
    let path = format!("repos/{repo}/actions/variables/{variable}");
    match decision.action {
        LeaseAction::Renew => {
            let expires_at = decision
                .expires_at
                .expect("renew decisions always carry an expiry")
                .to_rfc3339_opts(SecondsFormat::Secs, true);
            let patch_args = variable_write_args("PATCH", &path, variable, &expires_at);
            match actions.run_gh(&patch_args) {
                Ok(_) => Ok("renewed".to_owned()),
                Err(error) if is_not_found(&error.to_string()) => {
                    let create_path = format!("repos/{repo}/actions/variables");
                    let create_args =
                        variable_write_args("POST", &create_path, variable, &expires_at);
                    actions.run_gh(&create_args).map_err(|create_error| {
                        CliFailure::new(1, format!("failed to create health lease: {create_error}"))
                    })?;
                    Ok("created".to_owned())
                }
                Err(error) => Err(CliFailure::new(
                    1,
                    format!("failed to renew health lease: {error}"),
                )),
            }
        }
        LeaseAction::Clear => {
            let args = vec![
                "api".to_owned(),
                "--method".to_owned(),
                "DELETE".to_owned(),
                path,
            ];
            actions.run_gh(&args).map_err(|error| {
                CliFailure::new(
                    1,
                    format!("fleet is unhealthy and health lease clear failed: {error}"),
                )
            })?;
            Ok("cleared".to_owned())
        }
    }
}

fn variable_write_args(method: &str, path: &str, variable: &str, value: &str) -> Vec<String> {
    vec![
        "api".to_owned(),
        "--method".to_owned(),
        method.to_owned(),
        path.to_owned(),
        "--raw-field".to_owned(),
        format!("name={variable}"),
        "--raw-field".to_owned(),
        format!("value={value}"),
    ]
}

fn is_not_found(error: &str) -> bool {
    error.contains("HTTP 404") || error.contains("Not Found (HTTP 404)")
}

#[allow(clippy::too_many_arguments)]
fn emit_decision<W: Write>(
    stdout: &mut W,
    json: bool,
    tick: u32,
    repo: &str,
    variable: &str,
    events: &[String],
    decision: &LeaseDecision,
    mutation: &str,
    watch: bool,
) -> Result<(), CliFailure> {
    let expiry = decision
        .expires_at
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Secs, true));
    if json {
        let mut data = BTreeMap::new();
        data.insert("tick".to_owned(), Value::from(tick));
        data.insert("repo".to_owned(), Value::from(repo));
        data.insert("variable".to_owned(), Value::from(variable));
        data.insert("events".to_owned(), Value::from(events));
        data.insert("action".to_owned(), Value::from(decision.action.as_str()));
        data.insert("mutation".to_owned(), Value::from(mutation));
        data.insert("reason".to_owned(), Value::from(decision.reason.clone()));
        data.insert(
            "matching_runners".to_owned(),
            Value::from(decision.matching),
        );
        data.insert("online_runners".to_owned(), Value::from(decision.online));
        data.insert("idle_runners".to_owned(), Value::from(decision.idle));
        data.insert(
            "queued_matching_jobs".to_owned(),
            Value::from(decision.queued),
        );
        data.insert(
            "available_unreserved_runners".to_owned(),
            Value::from(decision.available),
        );
        data.insert(
            "observed_at".to_owned(),
            Value::from(
                decision
                    .observed_at
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
            ),
        );
        data.insert(
            "expires_at".to_owned(),
            expiry.map_or(Value::Null, Value::from),
        );
        if watch {
            let mut root = serde_json::Map::new();
            root.insert("schema_version".to_owned(), Value::from(SCHEMA_VERSION));
            root.insert(
                "command".to_owned(),
                Value::from("runner.local-linux-lease"),
            );
            root.extend(data);
            serde_json::to_writer(&mut *stdout, &Value::Object(root))
                .map_err(|error| CliFailure::new(1, format!("failed to write JSON: {error}")))?;
            stdout
                .write_all(b"\n")
                .map_err(|error| CliFailure::new(1, format!("failed to write JSON: {error}")))?;
        } else {
            write_json_envelope(stdout, "runner.local-linux-lease", data)
                .map_err(|error| CliFailure::new(1, format!("failed to write JSON: {error}")))?;
        }
    } else {
        writeln!(
            stdout,
            "local-linux-lease tick={tick} repo={repo} action={} mutation={mutation} matching={} online={} idle={} queued={} available={} expires_at={} reason={}",
            decision.action.as_str(),
            decision.matching,
            decision.online,
            decision.idle,
            decision.queued,
            decision.available,
            expiry.as_deref().unwrap_or("-"),
            decision.reason
        )
        .map_err(|error| CliFailure::new(1, format!("failed to write output: {error}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use chrono::TimeZone;

    use super::*;
    use crate::runner_provision::ApiLabel;

    fn runner(name: &str, status: &str, busy: bool, labels: &[&str]) -> LeaseRunner {
        LeaseRunner {
            name: name.to_owned(),
            status: status.to_owned(),
            busy,
            labels: labels
                .iter()
                .map(|name| ApiLabel {
                    name: (*name).to_owned(),
                })
                .collect(),
        }
    }

    #[test]
    fn missing_busy_state_is_unreadable_not_idle() {
        let malformed = r#"{"name":"pulp-ci-ephemeral-201","status":"online","labels":[]}"#;
        assert!(serde_json::from_str::<LeaseRunner>(malformed).is_err());
    }

    #[test]
    fn watch_mode_keeps_a_failed_mutation_as_a_failed_tick() {
        let failure = CliFailure::new(1, "transient GitHub failure");
        let (mutation, failed) =
            mutation_for_tick(Err(failure), true).expect("watch tick must continue");
        assert!(failed);
        assert!(mutation.contains("transient GitHub failure"));

        let one_shot =
            mutation_for_tick(Err(CliFailure::new(1, "transient GitHub failure")), false);
        assert!(one_shot.is_err());
    }

    fn labels() -> Vec<String> {
        [
            "self-hosted",
            "Linux",
            "X64",
            "pulp-build-linux-x64",
            "pulp-host-macpro",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    #[test]
    fn renews_only_for_online_idle_exact_pool_capacity() {
        let now = Utc.with_ymd_and_hms(2026, 8, 13, 18, 45, 0).unwrap();
        let runners = vec![runner(
            "pulp-ci-ephemeral-201",
            "online",
            false,
            &[
                "self-hosted",
                "Linux",
                "X64",
                "pulp-build-linux-x64",
                "pulp-host-macpro",
            ],
        )];
        let decision = decide_lease(&runners, "pulp-ci-ephemeral-", &labels(), 0, 1, 300, now);
        assert_eq!(decision.action, LeaseAction::Renew);
        assert_eq!(
            decision.expires_at,
            Some(Utc.with_ymd_and_hms(2026, 8, 13, 18, 50, 0).unwrap())
        );
    }

    #[test]
    fn negative_control_busy_capacity_clears_instead_of_renewing() {
        let now = Utc.with_ymd_and_hms(2026, 8, 13, 18, 45, 0).unwrap();
        let runners = vec![runner(
            "pulp-ci-ephemeral-201",
            "online",
            true,
            &[
                "self-hosted",
                "Linux",
                "X64",
                "pulp-build-linux-x64",
                "pulp-host-macpro",
            ],
        )];
        let decision = decide_lease(&runners, "pulp-ci-ephemeral-", &labels(), 0, 1, 300, now);
        assert_eq!(decision.action, LeaseAction::Clear);
        assert!(decision.expires_at.is_none());
    }

    #[test]
    fn queued_matching_jobs_reserve_observed_idle_capacity() {
        let now = Utc.with_ymd_and_hms(2026, 8, 13, 18, 45, 0).unwrap();
        let runners = vec![
            runner(
                "pulp-ci-ephemeral-201",
                "online",
                false,
                &[
                    "self-hosted",
                    "Linux",
                    "X64",
                    "pulp-build-linux-x64",
                    "pulp-host-macpro",
                ],
            ),
            runner(
                "pulp-ci-ephemeral-202",
                "online",
                false,
                &[
                    "self-hosted",
                    "Linux",
                    "X64",
                    "pulp-build-linux-x64",
                    "pulp-host-macpro",
                ],
            ),
        ];
        let decision = decide_lease(&runners, "pulp-ci-ephemeral-", &labels(), 2, 1, 300, now);
        assert_eq!(decision.action, LeaseAction::Clear);
        assert_eq!(decision.available, 0);
        assert_eq!(decision.queued, 2);
    }

    #[test]
    fn wrong_name_or_incomplete_labels_cannot_authorize_lease() {
        let now = Utc.with_ymd_and_hms(2026, 8, 13, 18, 45, 0).unwrap();
        let runners = vec![
            runner(
                "persistent-macpro",
                "online",
                false,
                &["self-hosted", "Linux", "X64", "pulp-host-macpro"],
            ),
            runner(
                "pulp-ci-ephemeral-201",
                "online",
                false,
                &["self-hosted", "Linux", "X64"],
            ),
        ];
        let decision = decide_lease(&runners, "pulp-ci-ephemeral-", &labels(), 0, 1, 300, now);
        assert_eq!(decision.action, LeaseAction::Clear);
        assert_eq!(decision.matching, 0);
    }

    #[test]
    fn mutation_arguments_preserve_rfc3339_as_an_opaque_string() {
        let args = variable_write_args(
            "PATCH",
            "repos/owner/repo/actions/variables/PULP_LOCAL_LINUX_LEASE_UNTIL",
            "PULP_LOCAL_LINUX_LEASE_UNTIL",
            "2026-08-13T18:50:00Z",
        );
        assert_eq!(
            args[1..4],
            [
                "--method",
                "PATCH",
                "repos/owner/repo/actions/variables/PULP_LOCAL_LINUX_LEASE_UNTIL"
            ]
        );
        assert!(args.contains(&"value=2026-08-13T18:50:00Z".to_owned()));
    }

    #[test]
    fn watch_json_is_one_parseable_envelope_per_line() {
        let decision = LeaseDecision {
            action: LeaseAction::Renew,
            observed_at: Utc.with_ymd_and_hms(2026, 8, 13, 18, 45, 0).unwrap(),
            expires_at: Some(Utc.with_ymd_and_hms(2026, 8, 13, 18, 50, 0).unwrap()),
            matching: 2,
            online: 2,
            idle: 2,
            queued: 0,
            available: 2,
            reason: "healthy_unreserved_idle_capacity".to_owned(),
        };
        let mut output = Vec::new();
        for tick in 1..=2 {
            emit_decision(
                &mut output,
                true,
                tick,
                "owner/repo",
                "PULP_LOCAL_LINUX_LEASE_UNTIL",
                &["merge_group".to_owned()],
                &decision,
                "renewed",
                true,
            )
            .expect("emit watch tick");
        }
        let stream = String::from_utf8(output).expect("UTF-8 output");
        let lines = stream.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .all(|line| serde_json::from_str::<Value>(line).is_ok())
        );
    }

    #[cfg(unix)]
    #[test]
    fn clear_decision_does_not_mistake_ambiguous_404_for_absence() {
        let temp = tempfile::tempdir().expect("tempdir");
        let calls = temp.path().join("calls");
        let gh = temp.path().join("gh");
        fs::write(
            &gh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\necho 'HTTP 404 Not Found' >&2\nexit 1\n",
                calls.display()
            ),
        )
        .expect("write fake gh");
        let mut permissions = fs::metadata(&gh).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&gh, permissions).expect("chmod fake gh");
        let actions = GitHubActions::new(temp.path()).with_gh_binary_for_tests(&gh);
        let decision = LeaseDecision {
            action: LeaseAction::Clear,
            observed_at: Utc.with_ymd_and_hms(2026, 8, 13, 18, 45, 0).unwrap(),
            expires_at: None,
            matching: 0,
            online: 0,
            idle: 0,
            queued: 0,
            available: 0,
            reason: "fleet_unreadable".to_owned(),
        };

        let error = apply_decision(
            &actions,
            "owner/repo",
            "PULP_LOCAL_LINUX_LEASE_UNTIL",
            &decision,
        )
        .expect_err("404 may mean inaccessible repository, not absent variable");

        assert!(error.message.contains("clear failed"));
        let call = fs::read_to_string(calls).expect("recorded call");
        assert!(call.contains("--method DELETE"));
        assert!(call.contains("repos/owner/repo/actions/variables/PULP_LOCAL_LINUX_LEASE_UNTIL"));
    }
}
