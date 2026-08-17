//! External health-lease operator for a self-managed runner pool.
//!
//! The consumer workflow reads an RFC3339 repository variable before choosing
//! its `runs-on` value. This command is the trusted producer: it observes the
//! registered runner fleet using Shipyard's configured GitHub auth and renews
//! the short lease only while matching idle capacity is visible. Any unhealthy
//! or unreadable observation clears the variable, so new jobs fall back to the
//! hosted selector. A crashed operator is bounded by the lease expiry.
//!
//! Which repository, context, and lane it operates on, which variable it
//! publishes, and which capability labels separate this pool from a sibling
//! pool all come from the routing profile — nothing here is specific to one
//! repository or one platform.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::Value;

use super::CliFailure;
use crate::cloud::GitHubActions;
use crate::output::{SCHEMA_VERSION, write_json_envelope};
use crate::runner_provision::ApiLabel;

const FLEET_OBSERVATION_TIMEOUT: StdDuration = StdDuration::from_secs(20);
const LEASE_MUTATION_TIMEOUT: StdDuration = StdDuration::from_secs(10);

struct ObservationBudget {
    deadline: Instant,
}

impl ObservationBudget {
    fn new(timeout: StdDuration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
        }
    }

    fn run_gh(&self, actions: &GitHubActions, args: &[String]) -> Result<String, String> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("fleet observation exhausted its total time budget".to_owned());
        }
        actions
            .run_gh_with_timeout(args, remaining)
            .map_err(|error| error.to_string())
    }
}

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
    observed_admission_burst: usize,
}

#[derive(Clone, Copy, Debug)]
struct AdmissionPolicy {
    declared_burst: usize,
    live_burst: usize,
    /// Idle floor a renewal must clear. Defaults to the declared burst, so an
    /// undeclared floor cannot arm a lease over a fully busy fleet.
    min_idle: usize,
    ttl_seconds: u64,
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
        let decision = match observe_fleet(
            &actions,
            &repo,
            &profile.required_labels,
            &profile.merge_queue_branch,
            &profile.events,
            profile.admission_burst,
        ) {
            Ok(observation) => {
                // Lease time begins only after every fleet and ruleset read is
                // complete. A slow observation can delay or expire the previous
                // lease, but it can never publish an already-aged renewal.
                let observed_at = Utc::now();
                decide_lease(
                    &observation.runners,
                    &profile.runner_name_prefix,
                    &profile.required_labels,
                    &profile.forbidden_capability,
                    observation.queued_matching_jobs,
                    AdmissionPolicy {
                        declared_burst: profile.admission_burst,
                        live_burst: observation.observed_admission_burst,
                        min_idle: profile.min_idle,
                        ttl_seconds: profile.ttl_seconds,
                    },
                    observed_at,
                )
            }
            Err(error) => unreadable_decision(&error),
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

fn unreadable_decision(error: &str) -> LeaseDecision {
    LeaseDecision {
        action: LeaseAction::Clear,
        observed_at: Utc::now(),
        expires_at: None,
        matching: 0,
        online: 0,
        idle: 0,
        queued: 0,
        available: 0,
        reason: format!("fleet_unreadable: {error}"),
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
    merge_queue_branch: &str,
    events: &[String],
    declared_burst: usize,
) -> Result<FleetObservation, String> {
    observe_fleet_with_timeout(
        actions,
        repo,
        required_labels,
        merge_queue_branch,
        events,
        declared_burst,
        FLEET_OBSERVATION_TIMEOUT,
    )
}

fn observe_fleet_with_timeout(
    actions: &GitHubActions,
    repo: &str,
    required_labels: &[String],
    merge_queue_branch: &str,
    events: &[String],
    declared_burst: usize,
    timeout: StdDuration,
) -> Result<FleetObservation, String> {
    let budget = ObservationBudget::new(timeout);
    let queued_matching_jobs = fetch_queued_matching_jobs(actions, repo, required_labels, &budget)?;
    let live_burst = if events == ["merge_group"] {
        fetch_merge_queue_build_concurrency(actions, repo, merge_queue_branch, &budget)?
    } else {
        declared_burst
    };
    Ok(FleetObservation {
        runners: fetch_runners(actions, repo, &budget)?,
        queued_matching_jobs,
        observed_admission_burst: live_burst,
    })
}

fn fetch_merge_queue_build_concurrency(
    actions: &GitHubActions,
    repo: &str,
    branch: &str,
    budget: &ObservationBudget,
) -> Result<usize, String> {
    let raw = budget
        .run_gh(actions, &merge_queue_rules_args(repo, branch))
        .map_err(|error| format!("failed to read rules for branch {branch}: {error}"))?;
    parse_merge_queue_build_concurrency(&raw, branch)
}

fn merge_queue_rules_args(repo: &str, branch: &str) -> Vec<String> {
    vec![
        "api".to_owned(),
        "--paginate".to_owned(),
        "--slurp".to_owned(),
        format!("repos/{repo}/rules/branches/{branch}?per_page=100"),
    ]
}

fn parse_merge_queue_build_concurrency(raw: &str, branch: &str) -> Result<usize, String> {
    let pages = serde_json::from_str::<Vec<Value>>(raw)
        .map_err(|error| format!("branch rules JSON parse failed: {error}"))?;
    let mut rules = Vec::new();
    for (index, page) in pages.into_iter().enumerate() {
        let Value::Array(page_rules) = page else {
            return Err(format!(
                "branch rules page {} is not a JSON array",
                index + 1
            ));
        };
        rules.extend(page_rules);
    }
    let bursts = rules
        .into_iter()
        .filter(|rule| rule.get("type").and_then(Value::as_str) == Some("merge_queue"))
        .filter_map(|rule| {
            rule.get("parameters")?
                .get("max_entries_to_build")?
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
        })
        .filter(|value| *value > 0)
        .collect::<Vec<_>>();
    bursts.into_iter().max().ok_or_else(|| {
        format!("no positive merge_queue max_entries_to_build applies to branch {branch}")
    })
}

fn fetch_runners(
    actions: &GitHubActions,
    repo: &str,
    budget: &ObservationBudget,
) -> Result<Vec<LeaseRunner>, String> {
    let raw = budget
        .run_gh(
            actions,
            &[
                "api".to_owned(),
                "--paginate".to_owned(),
                format!("repos/{repo}/actions/runners?per_page=100"),
                "--jq".to_owned(),
                ".runners[]".to_owned(),
            ],
        )
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
    budget: &ObservationBudget,
) -> Result<usize, String> {
    let mut run_ids = BTreeSet::new();
    for status in ["queued", "in_progress", "requested", "waiting", "pending"] {
        let raw = budget
            .run_gh(
                actions,
                &[
                    "api".to_owned(),
                    "--paginate".to_owned(),
                    format!("repos/{repo}/actions/runs?status={status}&per_page=100"),
                    "--jq".to_owned(),
                    ".workflow_runs[].id".to_owned(),
                ],
            )
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
        let raw = budget
            .run_gh(
                actions,
                &[
                    "api".to_owned(),
                    "--paginate".to_owned(),
                    format!("repos/{repo}/actions/runs/{run_id}/jobs?filter=all&per_page=100"),
                    "--jq".to_owned(),
                    ".jobs[]".to_owned(),
                ],
            )
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
    forbidden_capability: &str,
    queued: usize,
    policy: AdmissionPolicy,
    observed_at: DateTime<Utc>,
) -> LeaseDecision {
    let required = required_labels
        .iter()
        .map(|label| label.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let scheduler_eligible = runners
        .iter()
        .filter(|runner| required.is_subset(&runner.label_names()))
        .collect::<Vec<_>>();
    let forbidden_capability = forbidden_capability.to_ascii_lowercase();
    let contaminated = scheduler_eligible
        .iter()
        .filter(|runner| {
            let labels = runner.label_names();
            !runner.name.starts_with(name_prefix) || labels.contains(&forbidden_capability)
        })
        .count();
    let matching = scheduler_eligible
        .iter()
        .copied()
        .filter(|runner| runner.name.starts_with(name_prefix))
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
    if contaminated > 0 {
        return LeaseDecision {
            action: LeaseAction::Clear,
            observed_at,
            expires_at: None,
            matching: matching.len(),
            online,
            idle,
            queued,
            available,
            reason: format!(
                "scheduler_eligible_runner_outside_approved_namespace: contaminated={contaminated} prefix={name_prefix} forbidden_capability={forbidden_capability}"
            ),
        };
    }
    if policy.declared_burst < policy.live_burst {
        return LeaseDecision {
            action: LeaseAction::Clear,
            observed_at,
            expires_at: None,
            matching: matching.len(),
            online,
            idle,
            queued,
            available,
            reason: format!(
                "profile_admission_burst_below_live_merge_queue: declared={} live={}",
                policy.declared_burst, policy.live_burst
            ),
        };
    }
    let required_available = policy.declared_burst.max(policy.min_idle);
    if available >= required_available {
        LeaseDecision {
            action: LeaseAction::Renew,
            observed_at,
            expires_at: Some(
                observed_at
                    + Duration::seconds(
                        i64::try_from(policy.ttl_seconds).expect("profile TTL is bounded"),
                    ),
            ),
            matching: matching.len(),
            online,
            idle,
            queued,
            available,
            reason: format!(
                "healthy_for_admission_burst: declared={} live={}",
                policy.declared_burst, policy.live_burst
            ),
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
                "insufficient_unreserved_idle_capacity_for_admission_burst: required={required_available} live={} idle={idle} queued={queued} available={available}",
                policy.live_burst
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
    apply_decision_with_timeout(actions, repo, variable, decision, LEASE_MUTATION_TIMEOUT)
}

fn apply_decision_with_timeout(
    actions: &GitHubActions,
    repo: &str,
    variable: &str,
    decision: &LeaseDecision,
    timeout: StdDuration,
) -> Result<String, CliFailure> {
    let budget = ObservationBudget::new(timeout);
    let path = format!("repos/{repo}/actions/variables/{variable}");
    match decision.action {
        LeaseAction::Renew => {
            let expires_at = decision
                .expires_at
                .expect("renew decisions always carry an expiry")
                .to_rfc3339_opts(SecondsFormat::Secs, true);
            let patch_args = variable_write_args("PATCH", &path, variable, &expires_at);
            match budget.run_gh(actions, &patch_args) {
                Ok(_) => Ok("renewed".to_owned()),
                Err(error) if is_not_found(&error) => {
                    let create_path = format!("repos/{repo}/actions/variables");
                    let create_args =
                        variable_write_args("POST", &create_path, variable, &expires_at);
                    budget
                        .run_gh(actions, &create_args)
                        .map_err(|create_error| {
                            CliFailure::new(
                                1,
                                format!("failed to create health lease: {create_error}"),
                            )
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
            budget.run_gh(actions, &args).map_err(|error| {
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

    #[cfg(unix)]
    fn write_executable(path: &Path, body: &str) {
        fs::write(path, body).expect("write executable");
        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod executable");
    }

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

    #[cfg(unix)]
    #[test]
    fn fleet_observation_timeout_is_bounded_and_renderable_as_fail_closed_json() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = temp.path().join("gh");
        write_executable(&gh, "#!/bin/sh\nsleep 30\n");
        let actions = GitHubActions::new(temp.path()).with_gh_binary_for_tests(&gh);
        let started = Instant::now();
        let error = observe_fleet_with_timeout(
            &actions,
            "owner/repo",
            &["self-hosted".to_owned()],
            "main",
            &["merge_group".to_owned()],
            5,
            StdDuration::from_millis(150),
        )
        .expect_err("slow GitHub read must fail closed");
        assert!(started.elapsed() < StdDuration::from_secs(3));
        assert!(error.contains("timed out"), "unexpected error: {error}");

        let decision = unreadable_decision(&error);
        let mut output = Vec::new();
        emit_decision(
            &mut output,
            true,
            1,
            "owner/repo",
            "PULP_LOCAL_LINUX_LEASE_UNTIL",
            &["merge_group".to_owned()],
            &decision,
            "dry_run",
            false,
        )
        .expect("emit fail-closed JSON");
        let envelope: Value = serde_json::from_slice(&output).expect("valid JSON envelope");
        assert_eq!(envelope["action"], "clear");
        assert_eq!(envelope["mutation"], "dry_run");
        assert!(envelope["reason"].as_str().is_some_and(
            |reason| reason.contains("fleet_unreadable") && reason.contains("timed out")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn applied_clear_timeout_is_bounded_and_caller_survives() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = temp.path().join("gh");
        write_executable(&gh, "#!/bin/sh\nsleep 30\n");
        let actions = GitHubActions::new(temp.path()).with_gh_binary_for_tests(&gh);
        let decision = unreadable_decision("GitHub observation timed out");
        let started = Instant::now();
        let error = apply_decision_with_timeout(
            &actions,
            "owner/repo",
            "PULP_LOCAL_LINUX_LEASE_UNTIL",
            &decision,
            StdDuration::from_millis(150),
        )
        .expect_err("slow clear mutation must fail closed");
        assert!(started.elapsed() < StdDuration::from_secs(3));
        assert!(error.message.contains("clear failed"));
        assert!(error.message.contains("timed out"));
    }

    fn labels() -> Vec<String> {
        [
            "self-hosted",
            "Linux",
            "X64",
            "pulp-build-linux-x64",
            "pulp-host-macpro",
            "pulp-auto-linux-x64",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    fn policy(declared_burst: usize, live_burst: usize) -> AdmissionPolicy {
        AdmissionPolicy {
            declared_burst,
            live_burst,
            min_idle: declared_burst,
            ttl_seconds: 300,
        }
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
                "pulp-auto-linux-x64",
            ],
        )];
        let decision = decide_lease(
            &runners,
            "pulp-ci-ephemeral-",
            &labels(),
            "pulp-pr-safe-linux-x64",
            0,
            policy(1, 1),
            now,
        );
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
                "pulp-auto-linux-x64",
            ],
        )];
        let decision = decide_lease(
            &runners,
            "pulp-ci-ephemeral-",
            &labels(),
            "pulp-pr-safe-linux-x64",
            0,
            policy(1, 1),
            now,
        );
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
                    "pulp-auto-linux-x64",
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
                    "pulp-auto-linux-x64",
                ],
            ),
        ];
        let decision = decide_lease(
            &runners,
            "pulp-ci-ephemeral-",
            &labels(),
            "pulp-pr-safe-linux-x64",
            2,
            policy(1, 1),
            now,
        );
        assert_eq!(decision.action, LeaseAction::Clear);
        assert_eq!(decision.available, 0);
        assert_eq!(decision.queued, 2);
    }

    #[test]
    fn live_merge_queue_burst_larger_than_profile_fails_closed() {
        let now = Utc.with_ymd_and_hms(2026, 8, 13, 18, 45, 0).unwrap();
        let runners = (1..=5)
            .map(|index| {
                runner(
                    &format!("pulp-ci-ephemeral-{index}"),
                    "online",
                    false,
                    &[
                        "self-hosted",
                        "Linux",
                        "X64",
                        "pulp-build-linux-x64",
                        "pulp-host-macpro",
                        "pulp-auto-linux-x64",
                    ],
                )
            })
            .collect::<Vec<_>>();
        let decision = decide_lease(
            &runners,
            "pulp-ci-ephemeral-",
            &labels(),
            "pulp-pr-safe-linux-x64",
            0,
            policy(2, 5),
            now,
        );
        assert_eq!(decision.action, LeaseAction::Clear);
        assert!(decision.reason.contains("declared=2 live=5"));
    }

    #[test]
    fn full_declared_admission_burst_must_be_idle_before_renewal() {
        let now = Utc.with_ymd_and_hms(2026, 8, 13, 18, 45, 0).unwrap();
        let runners = (1..=5)
            .map(|index| {
                runner(
                    &format!("pulp-ci-ephemeral-{index}"),
                    "online",
                    false,
                    &[
                        "self-hosted",
                        "Linux",
                        "X64",
                        "pulp-build-linux-x64",
                        "pulp-host-macpro",
                        "pulp-auto-linux-x64",
                    ],
                )
            })
            .collect::<Vec<_>>();
        let decision = decide_lease(
            &runners,
            "pulp-ci-ephemeral-",
            &labels(),
            "pulp-pr-safe-linux-x64",
            0,
            policy(5, 5),
            now,
        );
        assert_eq!(decision.action, LeaseAction::Renew);
        assert_eq!(decision.available, 5);
    }

    #[test]
    fn two_runner_fleet_cannot_arm_a_five_entry_merge_queue() {
        let now = Utc.with_ymd_and_hms(2026, 8, 13, 18, 45, 0).unwrap();
        let runners = (1..=2)
            .map(|index| {
                runner(
                    &format!("pulp-ci-ephemeral-{index}"),
                    "online",
                    false,
                    &[
                        "self-hosted",
                        "Linux",
                        "X64",
                        "pulp-build-linux-x64",
                        "pulp-host-macpro",
                        "pulp-auto-linux-x64",
                    ],
                )
            })
            .collect::<Vec<_>>();
        let decision = decide_lease(
            &runners,
            "pulp-ci-ephemeral-",
            &labels(),
            "pulp-pr-safe-linux-x64",
            0,
            policy(5, 5),
            now,
        );
        assert_eq!(decision.action, LeaseAction::Clear);
        assert_eq!(decision.available, 2);
        assert!(decision.reason.contains("required=5 live=5"));
    }

    #[test]
    fn branch_rules_require_a_positive_merge_queue_concurrency() {
        let valid = r#"[[{"type":"required_status_checks","parameters":{}},{"type":"merge_queue","parameters":{"max_entries_to_build":5}}]]"#;
        assert_eq!(
            parse_merge_queue_build_concurrency(valid, "main").expect("merge queue rule"),
            5
        );
        let missing = r#"[[{"type":"required_status_checks","parameters":{}}]]"#;
        assert!(parse_merge_queue_build_concurrency(missing, "main").is_err());
        let malformed = r#"[[{"type":"merge_queue","parameters":{"max_entries_to_build":0}}]]"#;
        assert!(parse_merge_queue_build_concurrency(malformed, "main").is_err());
    }

    #[test]
    fn branch_rules_query_requests_and_slurps_all_full_pages() {
        assert_eq!(
            merge_queue_rules_args("owner/repo", "main"),
            [
                "api",
                "--paginate",
                "--slurp",
                "repos/owner/repo/rules/branches/main?per_page=100",
            ]
        );
    }

    #[test]
    fn branch_rules_find_merge_queue_on_a_later_page() {
        let pages = r#"[[{"type":"required_status_checks","parameters":{}}],[{"type":"merge_queue","parameters":{"max_entries_to_build":5}}]]"#;
        assert_eq!(
            parse_merge_queue_build_concurrency(pages, "main").expect("later page"),
            5
        );
    }

    #[test]
    fn malformed_branch_rules_page_fails_closed() {
        let malformed_page = r#"[[{"type":"required_status_checks","parameters":{}}],{"type":"merge_queue","parameters":{"max_entries_to_build":5}}]"#;
        let error = parse_merge_queue_build_concurrency(malformed_page, "main")
            .expect_err("non-array page must not be skipped");
        assert!(error.contains("page 2 is not a JSON array"));

        assert!(parse_merge_queue_build_concurrency("[[]", "main").is_err());
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
        let decision = decide_lease(
            &runners,
            "pulp-ci-ephemeral-",
            &labels(),
            "pulp-pr-safe-linux-x64",
            0,
            policy(1, 1),
            now,
        );
        assert_eq!(decision.action, LeaseAction::Clear);
        assert_eq!(decision.matching, 0);
    }

    #[test]
    fn scheduler_eligible_runner_outside_namespace_clears_lease() {
        let now = Utc.with_ymd_and_hms(2026, 8, 13, 18, 45, 0).unwrap();
        let required = [
            "self-hosted",
            "Linux",
            "X64",
            "pulp-host-macpro",
            "pulp-pr-safe-linux-x64",
        ];
        let mut dual_capability = required.to_vec();
        dual_capability.push("pulp-auto-linux-x64");
        for contaminated in [
            runner("persistent-macpro", "offline", false, &required),
            runner(
                "pulp-pr-safe-ephemeral-202",
                "online",
                false,
                &dual_capability,
            ),
        ] {
            let runners = vec![
                runner("pulp-pr-safe-ephemeral-201", "online", false, &required),
                contaminated,
            ];
            let required_labels = required
                .iter()
                .map(|label| (*label).to_owned())
                .collect::<Vec<_>>();
            let decision = decide_lease(
                &runners,
                "pulp-pr-safe-ephemeral-",
                &required_labels,
                "pulp-auto-linux-x64",
                0,
                policy(1, 1),
                now,
            );
            assert_eq!(decision.action, LeaseAction::Clear);
            assert!(decision.reason.contains("outside_approved_namespace"));
        }
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
