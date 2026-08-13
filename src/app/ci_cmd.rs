use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;
use toml::{Table, Value};

use crate::app::CliFailure;
use crate::app::cli::CiCommand;
use crate::output::write_pretty_json;

#[derive(Serialize)]
struct ProfilePlan {
    profile: String,
    description: String,
    repo: String,
    source: String,
    read_only: bool,
    lanes: Vec<LanePlan>,
    changes: Vec<VariableChange>,
    warnings: Vec<String>,
    note: String,
}

#[derive(Serialize)]
struct LanePlan {
    context: String,
    lane: String,
    strategy: String,
    targets: Vec<TargetPlan>,
    selected_now: Option<String>,
    selected_runs_on_json: Option<String>,
    github_variable: Option<String>,
    issue_on_failure: bool,
    ephemeral_required: bool,
    health_lease: Option<HealthLeasePlan>,
}

#[derive(Clone, Serialize)]
struct HealthLeasePlan {
    variable: String,
    ttl_seconds: u64,
    events: Vec<String>,
    runner_name_prefix: String,
    min_idle: usize,
}

#[derive(Debug)]
pub(super) struct LocalLinuxLeaseProfile {
    pub(super) variable: String,
    pub(super) ttl_seconds: u64,
    pub(super) events: Vec<String>,
    pub(super) runner_name_prefix: String,
    pub(super) min_idle: usize,
    pub(super) required_labels: Vec<String>,
}

#[derive(Serialize)]
struct TargetPlan {
    id: String,
    provider: Option<String>,
    host: Option<String>,
    os: Option<String>,
    arch: Option<String>,
    runs_on_json: Option<Value>,
    missing: bool,
}

#[derive(Serialize)]
struct VariableChange {
    variable: String,
    value: String,
    context: String,
    lane: String,
    target: String,
}

pub(super) fn ci_command<W: Write>(
    command: CiCommand,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        CiCommand::Profile { command } => match command {
            crate::app::cli::CiProfileCommand::Show { name, profile_file } => {
                let (path, text) = read_profile_text(cwd, &name, profile_file.as_deref())?;
                if json {
                    write_pretty_json(
                        stdout,
                        &serde_json::json!({
                            "profile": name,
                            "source": path,
                            "text": text,
                        }),
                    )
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
                } else {
                    stdout
                        .write_all(text.as_bytes())
                        .map_err(|error| CliFailure::new(1, error.to_string()))?;
                }
                Ok(ExitCode::SUCCESS)
            }
            crate::app::cli::CiProfileCommand::Plan {
                name,
                repo,
                profile_file,
            } => {
                let (path, profile) = load_profile(cwd, &name, profile_file.as_deref())?;
                let plan = build_plan(&profile, &repo, path.display().to_string())?;
                if json {
                    write_pretty_json(stdout, &plan)
                        .map_err(|error| CliFailure::new(1, error.to_string()))?;
                } else {
                    print_plan(stdout, &plan)
                        .map_err(|error| CliFailure::new(1, error.to_string()))?;
                }
                Ok(ExitCode::SUCCESS)
            }
        },
    }
}

fn read_profile_text(
    cwd: &Path,
    name: &str,
    explicit: Option<&Path>,
) -> Result<(PathBuf, String), CliFailure> {
    let path = resolve_profile_path(cwd, name, explicit)?;
    let text = std::fs::read_to_string(&path).map_err(|error| {
        CliFailure::new(1, format!("failed to read {}: {error}", path.display()))
    })?;
    Ok((path, text))
}

fn load_profile(
    cwd: &Path,
    name: &str,
    explicit: Option<&Path>,
) -> Result<(PathBuf, Table), CliFailure> {
    let (path, text) = read_profile_text(cwd, name, explicit)?;
    let table = text.parse::<Table>().map_err(|error| {
        CliFailure::new(1, format!("failed to parse {}: {error}", path.display()))
    })?;
    if table.get("name").and_then(Value::as_str) != Some(name) {
        return Err(CliFailure::new(
            1,
            format!(
                "profile {} does not declare name = {:?}",
                path.display(),
                name
            ),
        ));
    }
    Ok((path, table))
}

fn resolve_profile_path(
    cwd: &Path,
    name: &str,
    explicit: Option<&Path>,
) -> Result<PathBuf, CliFailure> {
    if let Some(path) = explicit {
        return Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        });
    }
    let candidates = [
        cwd.join(".tartci").join(format!("{name}.toml")),
        cwd.join(".shipyard")
            .join("ci-profiles")
            .join(format!("{name}.toml")),
        cwd.join("ci-profiles").join(format!("{name}.toml")),
    ];
    candidates
        .into_iter()
        .find(|path| path.exists())
        .ok_or_else(|| {
            CliFailure::new(
                1,
                format!(
                    "profile {name:?} not found; tried .tartci, .shipyard/ci-profiles, and ci-profiles"
                ),
            )
        })
}

fn build_plan(profile: &Table, repo: &str, source: String) -> Result<ProfilePlan, CliFailure> {
    let profile_name = required_str(profile, "name")?.to_owned();
    let description = optional_str(profile, "description")
        .unwrap_or("")
        .to_owned();
    let repo_table = repo_profile_table(profile, repo)?;
    let targets = profile
        .get("targets")
        .and_then(Value::as_table)
        .cloned()
        .unwrap_or_default();

    let mut lanes = Vec::new();
    let mut changes = Vec::new();
    let mut warnings = Vec::new();

    for spec in lane_specs(repo_table) {
        let outcome = plan_lane(&spec, &targets);
        warnings.extend(outcome.warnings);
        if let Some(change) = outcome.change {
            changes.push(change);
        }
        lanes.push(outcome.lane);
    }

    Ok(ProfilePlan {
        profile: profile_name,
        description,
        repo: repo.to_owned(),
        source,
        read_only: true,
        lanes,
        changes,
        warnings,
        note: "read-only plan; resolve ordered fallback with live fleet status before applying GitHub variables".to_owned(),
    })
}

struct LaneSpec<'a> {
    context: &'a str,
    lane: &'a str,
    table: &'a Table,
}

struct LaneOutcome {
    lane: LanePlan,
    change: Option<VariableChange>,
    warnings: Vec<String>,
}

fn repo_profile_table<'a>(profile: &'a Table, repo: &str) -> Result<&'a Table, CliFailure> {
    required_table(profile, "repo")?
        .get(repo)
        .and_then(Value::as_table)
        .ok_or_else(|| CliFailure::new(1, format!("profile has no repo entry for {repo}")))
}

fn lane_specs(repo_table: &Table) -> Vec<LaneSpec<'_>> {
    let mut specs = Vec::new();
    for (context, context_value) in repo_table {
        let Some(context_table) = context_value.as_table() else {
            continue;
        };
        for (lane, lane_value) in context_table {
            let Some(table) = lane_value.as_table() else {
                continue;
            };
            specs.push(LaneSpec {
                context,
                lane,
                table,
            });
        }
    }
    specs
}

fn plan_lane(spec: &LaneSpec<'_>, targets: &Table) -> LaneOutcome {
    let target_plans = target_ids(spec.table)
        .iter()
        .map(|id| target_plan(id, targets))
        .collect::<Vec<_>>();
    let selected = target_plans.iter().find(|target| !target.missing);
    let selected_id = selected.map(|target| target.id.clone());
    let selected_runs_on_json = selected
        .and_then(|target| target.runs_on_json.as_ref())
        .map(compact_json);
    let github_variable = optional_str(spec.table, "github_variable");
    let ephemeral_required = optional_bool(spec.table, "ephemeral_required");
    let lane = LanePlan {
        context: spec.context.to_owned(),
        lane: spec.lane.to_owned(),
        strategy: optional_str(spec.table, "strategy")
            .unwrap_or("ordered-fallback")
            .to_owned(),
        targets: target_plans,
        selected_now: selected_id.clone(),
        selected_runs_on_json: selected_runs_on_json.clone(),
        github_variable: github_variable.map(str::to_owned),
        issue_on_failure: optional_bool(spec.table, "issue_on_failure"),
        ephemeral_required,
        health_lease: health_lease_plan(spec.table),
    };
    let change = variable_change(
        spec,
        github_variable,
        selected_runs_on_json.as_ref(),
        selected_id.as_ref(),
    );
    let warnings = lane_warnings(spec, targets, &lane.targets, ephemeral_required);
    LaneOutcome {
        lane,
        change,
        warnings,
    }
}

pub(super) fn load_local_linux_lease_profile(
    cwd: &Path,
    name: &str,
    explicit: Option<&Path>,
    repo: &str,
    context: &str,
    lane: &str,
) -> Result<LocalLinuxLeaseProfile, CliFailure> {
    let (_, profile) = load_profile(cwd, name, explicit)?;
    let repo_table = repo_profile_table(&profile, repo)?;
    let lane_table = repo_table
        .get(context)
        .and_then(Value::as_table)
        .and_then(|table| table.get(lane))
        .and_then(Value::as_table)
        .ok_or_else(|| {
            CliFailure::new(
                2,
                format!("profile has no lane repo.{repo}.{context}.{lane}"),
            )
        })?;
    let lease = strict_health_lease_plan(lane_table)?;
    let first_target = target_ids(lane_table).into_iter().next().ok_or_else(|| {
        CliFailure::new(2, format!("profile lane {context}.{lane} has no targets"))
    })?;
    let target = required_table(&profile, "targets")?
        .get(&first_target)
        .and_then(Value::as_table)
        .ok_or_else(|| CliFailure::new(2, format!("profile target {first_target} is missing")))?;
    let required_labels = target
        .get("runs_on_json")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CliFailure::new(
                2,
                format!("profile target {first_target} must use an array runs_on_json selector"),
            )
        })?
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                CliFailure::new(
                    2,
                    format!("profile target {first_target} has a non-string label"),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    for required in ["self-hosted", "Linux", "X64", "pulp-auto-linux-x64"] {
        if !required_labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case(required))
        {
            return Err(CliFailure::new(
                2,
                format!("profile target {first_target} is missing required label {required}"),
            ));
        }
    }
    Ok(LocalLinuxLeaseProfile {
        variable: lease.variable,
        ttl_seconds: lease.ttl_seconds,
        events: lease.events,
        runner_name_prefix: lease.runner_name_prefix,
        min_idle: lease.min_idle,
        required_labels,
    })
}

fn health_lease_plan(table: &Table) -> Option<HealthLeasePlan> {
    let variable = optional_str(table, "health_lease_variable")?.to_owned();
    let ttl_seconds = table
        .get("health_lease_ttl_seconds")?
        .as_integer()
        .and_then(|value| u64::try_from(value).ok())?;
    let events = string_array(table, "health_lease_events")?;
    let runner_name_prefix = optional_str(table, "health_lease_runner_name_prefix")?.to_owned();
    let min_idle = health_lease_min_idle(table).ok()?;
    Some(HealthLeasePlan {
        variable,
        ttl_seconds,
        events,
        runner_name_prefix,
        min_idle,
    })
}

fn health_lease_min_idle(table: &Table) -> Result<usize, &'static str> {
    match table.get("health_lease_min_idle") {
        None => Ok(1),
        Some(value) => value
            .as_integer()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value >= 1)
            .ok_or("health_lease_min_idle must be a positive integer"),
    }
}

fn strict_health_lease_plan(table: &Table) -> Result<HealthLeasePlan, CliFailure> {
    health_lease_min_idle(table).map_err(|message| CliFailure::new(2, message))?;
    let lease = health_lease_plan(table).ok_or_else(|| {
        CliFailure::new(
            2,
            "profile lane needs health_lease_variable, health_lease_ttl_seconds, health_lease_events, and health_lease_runner_name_prefix",
        )
    })?;
    if lease.variable != "PULP_LOCAL_LINUX_LEASE_UNTIL" {
        return Err(CliFailure::new(
            2,
            format!(
                "local Linux lease variable must be PULP_LOCAL_LINUX_LEASE_UNTIL, got {}",
                lease.variable
            ),
        ));
    }
    if !(60..=900).contains(&lease.ttl_seconds) {
        return Err(CliFailure::new(
            2,
            "health_lease_ttl_seconds must be between 60 and 900",
        ));
    }
    if lease.events != ["merge_group"] {
        return Err(CliFailure::new(
            2,
            "health_lease_events must be exactly [\"merge_group\"]",
        ));
    }
    if lease.runner_name_prefix.trim().is_empty() || !lease.runner_name_prefix.contains("ephemeral")
    {
        return Err(CliFailure::new(
            2,
            "health_lease_runner_name_prefix must identify an ephemeral pool",
        ));
    }
    if lease.min_idle == 0 {
        return Err(CliFailure::new(
            2,
            "health_lease_min_idle must be at least 1",
        ));
    }
    Ok(lease)
}

fn string_array(table: &Table, key: &str) -> Option<Vec<String>> {
    table.get(key).and_then(Value::as_array).and_then(|values| {
        values
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect()
    })
}

fn target_ids(lane_table: &Table) -> Vec<String> {
    lane_table
        .get("targets")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn variable_change(
    spec: &LaneSpec<'_>,
    variable: Option<&str>,
    value: Option<&String>,
    target: Option<&String>,
) -> Option<VariableChange> {
    Some(VariableChange {
        variable: variable?.to_owned(),
        value: value?.clone(),
        context: spec.context.to_owned(),
        lane: spec.lane.to_owned(),
        target: target?.clone(),
    })
}

fn lane_warnings(
    spec: &LaneSpec<'_>,
    targets: &Table,
    target_plans: &[TargetPlan],
    ephemeral_required: bool,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for target in target_plans {
        if target.missing {
            warnings.push(format!(
                "{}.{}: target {} is not defined",
                spec.context, spec.lane, target.id
            ));
        }
        if ephemeral_required
            && !target.is_github()
            && !target_bool(targets, &target.id, "ephemeral")
        {
            warnings.push(format!(
                "{}.{}: target {} is not marked ephemeral",
                spec.context, spec.lane, target.id
            ));
        }
    }
    warnings
}

fn target_plan(id: &str, targets: &Table) -> TargetPlan {
    let target = targets.get(id).and_then(Value::as_table);
    let inferred_provider = if id.starts_with("github.") {
        Some("github".to_owned())
    } else {
        None
    };
    TargetPlan {
        id: id.to_owned(),
        provider: target
            .and_then(|t| t.get("provider"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(inferred_provider),
        host: target
            .and_then(|t| t.get("host"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        os: target
            .and_then(|t| t.get("os"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        arch: target
            .and_then(|t| t.get("arch"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        runs_on_json: target.and_then(|t| t.get("runs_on_json")).cloned(),
        missing: target.is_none(),
    }
}

impl TargetPlan {
    fn is_github(&self) -> bool {
        self.provider.as_deref() == Some("github") || self.id.starts_with("github.")
    }
}

fn target_bool(targets: &Table, id: &str, key: &str) -> bool {
    targets
        .get(id)
        .and_then(Value::as_table)
        .and_then(|target| target.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn optional_str<'a>(table: &'a Table, key: &str) -> Option<&'a str> {
    table.get(key).and_then(Value::as_str)
}

fn optional_bool(table: &Table, key: &str) -> bool {
    table.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn compact_json(value: &Value) -> String {
    let json_value = serde_json::to_value(value).expect("toml value serializes");
    serde_json::to_string(&json_value).expect("json value serializes")
}

fn required_str<'a>(table: &'a Table, key: &str) -> Result<&'a str, CliFailure> {
    table
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| CliFailure::new(1, format!("profile missing string key {key:?}")))
}

fn required_table<'a>(table: &'a Table, key: &str) -> Result<&'a Table, CliFailure> {
    table
        .get(key)
        .and_then(Value::as_table)
        .ok_or_else(|| CliFailure::new(1, format!("profile missing table {key:?}")))
}

fn print_plan<W: Write>(stdout: &mut W, plan: &ProfilePlan) -> std::io::Result<()> {
    writeln!(
        stdout,
        "Read-only CI profile plan for {} using {} ({})",
        plan.repo, plan.profile, plan.source
    )?;
    for lane in &plan.lanes {
        let chain = lane
            .targets
            .iter()
            .map(|target| target.id.as_str())
            .collect::<Vec<_>>()
            .join(" -> ");
        writeln!(stdout, "{}.{}: {}", lane.context, lane.lane, chain)?;
        if let (Some(variable), Some(value)) = (&lane.github_variable, &lane.selected_runs_on_json)
        {
            writeln!(stdout, "  {variable}={value}")?;
        }
        if let Some(lease) = &lane.health_lease {
            writeln!(
                stdout,
                "  health lease: {} ttl={}s events={} prefix={} min_idle={}",
                lease.variable,
                lease.ttl_seconds,
                lease.events.join(","),
                lease.runner_name_prefix,
                lease.min_idle
            )?;
        }
    }
    if !plan.warnings.is_empty() {
        writeln!(stdout, "warnings:")?;
        for warning in &plan.warnings {
            writeln!(stdout, "  - {warning}")?;
        }
    }
    writeln!(stdout, "{}", plan.note)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{build_plan, load_local_linux_lease_profile};

    #[test]
    fn builds_provider_neutral_profile_plan() {
        let profile = r#"
name = "normal"
description = "local first"

[repo."owner/repo".pr.linux]
strategy = "ordered-fallback"
targets = ["local.linux-arm64", "github.linux-x64"]
github_variable = "LOCAL_LINUX_RUNS_ON_JSON"

[targets."local.linux-arm64"]
runs_on_json = ["self-hosted", "Linux", "ARM64", "build"]

[targets."github.linux-x64"]
runs_on_json = "ubuntu-latest"
"#
        .parse()
        .expect("profile toml");

        let plan = build_plan(&profile, "owner/repo", "test.toml".to_owned()).expect("plan");

        assert_eq!(plan.profile, "normal");
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].variable, "LOCAL_LINUX_RUNS_ON_JSON");
        assert_eq!(
            plan.changes[0].value,
            "[\"self-hosted\",\"Linux\",\"ARM64\",\"build\"]"
        );
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn github_target_id_satisfies_coverage_ephemeral_requirement() {
        let profile = r#"
name = "coverage"

[repo."owner/repo".coverage.windows]
strategy = "github-only"
targets = ["github.windows-x64"]
github_variable = "COVERAGE_WINDOWS_RUNS_ON_JSON"
ephemeral_required = true

[targets."github.windows-x64"]
runs_on_json = "windows-latest"
"#
        .parse()
        .expect("profile toml");

        let plan = build_plan(&profile, "owner/repo", "test.toml".to_owned()).expect("plan");

        assert!(plan.warnings.is_empty());
        assert_eq!(plan.changes[0].value, "\"windows-latest\"");
    }

    #[test]
    fn loads_bounded_local_linux_health_lease_from_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("profile.toml");
        fs::write(
            &path,
            r#"
name = "normal-local-fast"

[repo."owner/repo".pr.linux]
strategy = "ordered-fallback"
targets = ["macpro.linux-x64-vm", "github.linux-x64"]
github_variable = "PULP_LOCAL_LINUX_RUNS_ON_JSON"
health_lease_variable = "PULP_LOCAL_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["merge_group"]
health_lease_runner_name_prefix = "pulp-ci-ephemeral-"
health_lease_min_idle = 1

[targets."macpro.linux-x64-vm"]
runs_on_json = ["self-hosted", "Linux", "X64", "pulp-build-linux-x64", "pulp-host-macpro", "pulp-auto-linux-x64"]

[targets."github.linux-x64"]
runs_on_json = "ubuntu-latest"
"#,
        )
        .expect("write profile");

        let lease = load_local_linux_lease_profile(
            dir.path(),
            "normal-local-fast",
            Some(&path),
            "owner/repo",
            "pr",
            "linux",
        )
        .expect("lease profile");

        assert_eq!(lease.variable, "PULP_LOCAL_LINUX_LEASE_UNTIL");
        assert_eq!(lease.ttl_seconds, 300);
        assert_eq!(lease.events, ["merge_group"]);
        assert_eq!(lease.runner_name_prefix, "pulp-ci-ephemeral-");
        assert_eq!(lease.min_idle, 1);
        assert!(lease.required_labels.iter().any(|label| label == "X64"));
        assert!(
            lease
                .required_labels
                .iter()
                .any(|label| label == "pulp-auto-linux-x64")
        );
    }

    #[test]
    fn rejects_pull_request_health_lease_scope() {
        let profile = r#"
name = "normal-local-fast"

[repo."owner/repo".pr.linux]
targets = ["macpro.linux-x64-vm"]
health_lease_variable = "PULP_LOCAL_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["pull_request", "merge_group"]
health_lease_runner_name_prefix = "pulp-ci-ephemeral-"

[targets."macpro.linux-x64-vm"]
runs_on_json = ["self-hosted", "Linux", "X64", "pulp-host-macpro", "pulp-auto-linux-x64"]
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("profile.toml");
        fs::write(&path, profile).expect("write profile");

        let error = load_local_linux_lease_profile(
            dir.path(),
            "normal-local-fast",
            Some(&path),
            "owner/repo",
            "pr",
            "linux",
        )
        .expect_err("pull-request scope must fail");
        assert!(error.message.contains("exactly [\"merge_group\"]"));
    }

    #[test]
    fn rejects_target_without_protected_automatic_label() {
        let profile = r#"
name = "normal-local-fast"

[repo."owner/repo".pr.linux]
targets = ["macpro.linux-x64-vm"]
health_lease_variable = "PULP_LOCAL_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["merge_group"]
health_lease_runner_name_prefix = "pulp-ci-ephemeral-"

[targets."macpro.linux-x64-vm"]
runs_on_json = ["self-hosted", "Linux", "X64", "pulp-host-macpro"]
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("profile.toml");
        fs::write(&path, profile).expect("write profile");

        let error = load_local_linux_lease_profile(
            dir.path(),
            "normal-local-fast",
            Some(&path),
            "owner/repo",
            "pr",
            "linux",
        )
        .expect_err("unprotected automatic target must fail");
        assert!(error.message.contains("pulp-auto-linux-x64"));
    }

    #[test]
    fn rejects_health_lease_longer_than_fifteen_minutes() {
        let profile = r#"
name = "normal-local-fast"

[repo."owner/repo".pr.linux]
targets = ["macpro.linux-x64-vm"]
health_lease_variable = "PULP_LOCAL_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 901
health_lease_events = ["merge_group"]
health_lease_runner_name_prefix = "pulp-ci-ephemeral-"

[targets."macpro.linux-x64-vm"]
runs_on_json = ["self-hosted", "Linux", "X64", "pulp-host-macpro"]
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("profile.toml");
        fs::write(&path, profile).expect("write profile");

        let error = load_local_linux_lease_profile(
            dir.path(),
            "normal-local-fast",
            Some(&path),
            "owner/repo",
            "pr",
            "linux",
        )
        .expect_err("oversized lease must fail");
        assert!(error.message.contains("between 60 and 900"));
    }

    #[test]
    fn rejects_malformed_health_lease_min_idle_instead_of_defaulting() {
        let profile = r#"
name = "normal-local-fast"

[repo."owner/repo".pr.linux]
targets = ["macpro.linux-x64-vm"]
health_lease_variable = "PULP_LOCAL_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["merge_group"]
health_lease_runner_name_prefix = "pulp-ci-ephemeral-"
health_lease_min_idle = "2"

[targets."macpro.linux-x64-vm"]
runs_on_json = ["self-hosted", "Linux", "X64", "pulp-host-macpro"]
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("profile.toml");
        fs::write(&path, profile).expect("write profile");

        let error = load_local_linux_lease_profile(
            dir.path(),
            "normal-local-fast",
            Some(&path),
            "owner/repo",
            "pr",
            "linux",
        )
        .expect_err("malformed minimum must fail");
        assert!(error.message.contains("positive integer"));
    }
}
