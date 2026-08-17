use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;
use toml::{Table, Value};

use crate::app::CliFailure;
use crate::app::cli::CiCommand;
use crate::ci_profile::{CiProfile, HealthLease, HealthLeaseFields};
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
    health_lease: Option<HealthLease>,
}

#[derive(Debug)]
pub(super) struct LocalLinuxLeaseProfile {
    pub(super) variable: String,
    pub(super) ttl_seconds: u64,
    pub(super) events: Vec<String>,
    pub(super) runner_name_prefix: String,
    pub(super) merge_queue_branch: String,
    pub(super) admission_burst: usize,
    pub(super) min_idle: usize,
    /// Capability label that disqualifies a runner from this lease, so a
    /// sibling pool cannot silently satisfy it.
    pub(super) forbidden_capability: String,
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
    mode: crate::identity::RuntimeMode,
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
            crate::app::cli::CiProfileCommand::Apply {
                name,
                repo,
                context,
                apply,
                max_evidence_age_days,
                topology_check,
                profile_file,
            } => {
                let config = crate::config::LoadedConfig::load_from_cwd(mode, cwd)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
                let actions = crate::cloud::GitHubActions::from_loaded_config(cwd, &config);
                super::profile_apply_cmd::profile_apply_command(
                    super::profile_apply_cmd::ProfileApplyArgs {
                        name,
                        repo,
                        context,
                        apply,
                        max_evidence_age_days,
                        topology_check,
                        profile_file,
                    },
                    cwd,
                    &actions,
                    json,
                    stdout,
                )
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

/// Load a profile through the typed schema, so an unknown key is a load-time
/// error rather than a setting that is silently dropped.
pub(super) fn load_typed_profile(
    cwd: &Path,
    name: &str,
    explicit: Option<&Path>,
) -> Result<(PathBuf, CiProfile), CliFailure> {
    let (path, text) = read_profile_text(cwd, name, explicit)?;
    let profile = CiProfile::parse_named(&text, name)
        .map_err(|error| CliFailure::new(1, format!("{}: {error}", path.display())))?;
    Ok((path, profile))
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
        health_lease: lane_health_lease(spec.table),
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

/// Load and validate one lane's health-lease declaration.
///
/// Works for any repository, context, and lane: the capability namespace, the
/// runner-name prefix, and the published variable all come from the profile
/// rather than from a hardcoded table of one repository's Linux lanes. The
/// safety invariants that keep a trusted pool and a PR-safe pool from bleeding
/// into each other are preserved, but they are now expressed generically.
pub(super) fn load_local_linux_lease_profile(
    cwd: &Path,
    name: &str,
    explicit: Option<&Path>,
    repo: &str,
    context: &str,
    lane: &str,
) -> Result<LocalLinuxLeaseProfile, CliFailure> {
    let (_, profile) = load_typed_profile(cwd, name, explicit)?;
    let lane_body = profile
        .lane(repo, context, lane)
        .map_err(|error| CliFailure::new(2, error.to_string()))?;
    let lease = strict_health_lease(&lane_body.health_lease, context)?;
    let first_target_id = lane_body.targets.first().ok_or_else(|| {
        CliFailure::new(2, format!("profile lane {context}.{lane} has no targets"))
    })?;
    let target = profile.target(first_target_id).ok_or_else(|| {
        CliFailure::new(2, format!("profile target {first_target_id} is missing"))
    })?;
    let required_labels = target.required_labels().ok_or_else(|| {
        CliFailure::new(
            2,
            format!("profile target {first_target_id} must use an array runs_on_json selector"),
        )
    })?;

    let capability = lease.required_capability.clone().ok_or_else(|| {
        CliFailure::new(
            2,
            format!(
                "lane {context}.{lane} declares a health lease and must also declare \
                 health_lease_required_capability"
            ),
        )
    })?;
    let forbidden_capability = lease.forbidden_capability.clone().ok_or_else(|| {
        CliFailure::new(
            2,
            format!(
                "lane {context}.{lane} declares a health lease and must also declare \
                 health_lease_forbidden_capability so a foreign pool cannot satisfy it"
            ),
        )
    })?;

    // A self-hosted lease is only meaningful over self-hosted runners; the
    // capability label is what separates one pool from its sibling.
    for required in ["self-hosted", capability.as_str()] {
        if !required_labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case(required))
        {
            return Err(CliFailure::new(
                2,
                format!("profile target {first_target_id} is missing required label {required}"),
            ));
        }
    }
    if required_labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(&forbidden_capability))
    {
        return Err(CliFailure::new(
            2,
            format!(
                "profile target {first_target_id} mixes the {capability} and {forbidden_capability} capability namespaces"
            ),
        ));
    }

    Ok(LocalLinuxLeaseProfile {
        variable: lease.variable,
        ttl_seconds: lease.ttl_seconds,
        events: lease.events,
        runner_name_prefix: lease.runner_name_prefix,
        merge_queue_branch: lease.merge_queue_branch,
        admission_burst: lease.admission_burst,
        min_idle: lease.min_idle,
        forbidden_capability,
        required_labels,
    })
}

/// Validate a lane's health-lease group, applying the generic namespace rules.
fn strict_health_lease(
    fields: &HealthLeaseFields,
    context: &str,
) -> Result<HealthLease, CliFailure> {
    let lease = HealthLease::from_fields(fields)
        .map_err(|error| CliFailure::new(2, error.to_string()))?
        .ok_or_else(|| {
            CliFailure::new(
                2,
                format!(
                    "profile lane for context {context} declares no health lease; it needs \
                     health_lease_variable, health_lease_ttl_seconds, health_lease_events, \
                     health_lease_runner_name_prefix, health_lease_merge_queue_branch, and \
                     health_lease_admission_burst"
                ),
            )
        })?;

    // A prefix without a trailing delimiter prefix-matches every longer
    // sibling namespace, so `pulp-ci-ephemeral` would silently admit
    // `pulp-ci-ephemeral-prod-*` runners into a lease meant for one pool.
    let prefix = &lease.runner_name_prefix;
    if prefix.len() < 4 || !prefix.ends_with('-') && !prefix.ends_with('_') {
        return Err(CliFailure::new(
            2,
            format!(
                "health lease namespace is too broad: health_lease_runner_name_prefix {prefix:?} \
                 must be at least 4 characters and end with '-' or '_' so it cannot prefix-match \
                 a sibling runner pool"
            ),
        ));
    }

    // Trusted merge-queue capacity and untrusted pull-request capacity are
    // different trust domains. One lease may not authorize both.
    let admits_untrusted = lease
        .events
        .iter()
        .any(|event| event.eq_ignore_ascii_case("pull_request"));
    let admits_trusted = lease
        .events
        .iter()
        .any(|event| event.eq_ignore_ascii_case("merge_group"));
    if admits_untrusted && admits_trusted {
        return Err(CliFailure::new(
            2,
            "health lease namespace must not mix trust domains: health_lease_events may name \
             pull_request or merge_group, never both",
        ));
    }

    if let (Some(required), Some(forbidden)) = (
        lease.required_capability.as_deref(),
        lease.forbidden_capability.as_deref(),
    ) && required.eq_ignore_ascii_case(forbidden)
    {
        return Err(CliFailure::new(
            2,
            "health lease namespace is degenerate: health_lease_required_capability and \
             health_lease_forbidden_capability must name different labels",
        ));
    }

    if lease.merge_queue_branch.trim().is_empty() {
        return Err(CliFailure::new(
            2,
            "health_lease_merge_queue_branch must name a branch",
        ));
    }

    Ok(lease)
}

/// Read a lane table's `health_lease_*` group as a validated lease.
///
/// Used by the read-only plan view, which reports what a lane declares. An
/// invalid or absent group renders as no lease rather than failing the whole
/// plan; `strict_health_lease` is the enforcing path.
fn lane_health_lease(table: &Table) -> Option<HealthLease> {
    let fields: HealthLeaseFields = table.clone().try_into().ok()?;
    HealthLease::from_fields(&fields).ok().flatten()
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
        if target.is_self_managed_x64() && !target_bool(targets, &target.id, "proven") {
            warnings.push(format!(
                "{}.{}: self-managed x64 target {} is not marked proven",
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

    fn is_self_managed_x64(&self) -> bool {
        if matches!(self.provider.as_deref(), Some("github" | "namespace")) {
            return false;
        }
        let mentions_x64 = |value: &str| {
            value
                .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                .any(|part| part.eq_ignore_ascii_case("x64") || part.eq_ignore_ascii_case("x86_64"))
        };
        self.arch.as_deref().is_some_and(mentions_x64)
            || mentions_x64(&self.id)
            || self
                .runs_on_json
                .as_ref()
                .is_some_and(|selector| match selector {
                    Value::String(value) => mentions_x64(value),
                    Value::Array(values) => {
                        values.iter().filter_map(Value::as_str).any(mentions_x64)
                    }
                    _ => false,
                })
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
                "  health lease: {} ttl={}s events={} prefix={} branch={} admission_burst={}",
                lease.variable,
                lease.ttl_seconds,
                lease.events.join(","),
                lease.runner_name_prefix,
                lease.merge_queue_branch,
                lease.admission_burst
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

    use toml::Table;

    use super::{build_plan, load_local_linux_lease_profile, strict_health_lease};
    use crate::ci_profile::HealthLeaseFields;

    /// Build a lease-bearing lane body with an overridable field set.
    ///
    /// The generalized contract has no repo-specific allowlist, so every test
    /// states the namespace it means rather than inheriting one.
    fn lease_fields(overrides: &str) -> HealthLeaseFields {
        let mut table: Table = r#"
health_lease_variable = "TRUSTED_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["merge_group"]
health_lease_runner_name_prefix = "acme-ci-ephemeral-"
health_lease_merge_queue_branch = "main"
health_lease_admission_burst = 5
health_lease_required_capability = "acme-auto-linux-x64"
health_lease_forbidden_capability = "acme-pr-safe-linux-x64"
"#
        .parse()
        .expect("lease lane");
        let overrides: Table = overrides.parse().expect("lease overrides");
        for (key, value) in overrides {
            table.insert(key, value);
        }
        table.try_into().expect("lease fields")
    }

    #[test]
    fn accepts_a_well_formed_namespace_for_any_repo() {
        let lease = strict_health_lease(&lease_fields(""), "merge_group").expect("valid namespace");

        assert_eq!(lease.runner_name_prefix, "acme-ci-ephemeral-");
        assert_eq!(lease.variable, "TRUSTED_LINUX_LEASE_UNTIL");
        assert_eq!(
            lease.forbidden_capability.as_deref(),
            Some("acme-pr-safe-linux-x64")
        );
    }

    #[test]
    fn rejects_a_runner_prefix_that_can_match_a_sibling_pool() {
        // A prefix with no trailing delimiter prefix-matches every longer
        // sibling namespace, which is how a foreign pool silently satisfies a
        // lease meant for one pool.
        for prefix in ["x-", "ephemeral", "acme-ci-ephemeral"] {
            let fields = lease_fields(&format!(
                "\nhealth_lease_runner_name_prefix = \"{prefix}\"\n"
            ));
            let Err(error) = strict_health_lease(&fields, "merge_group") else {
                panic!("unsafe prefix must fail closed: {prefix}");
            };
            assert!(
                error.message.contains("too broad"),
                "prefix={prefix} error={}",
                error.message
            );
        }
    }

    #[test]
    fn control_a_delimited_prefix_of_the_same_shape_is_accepted() {
        // Negative control for the prefix test: proves the rejection is the
        // missing delimiter, not the name.
        for prefix in ["acme-ci-ephemeral-", "acme_ci_ephemeral_"] {
            let fields = lease_fields(&format!(
                "\nhealth_lease_runner_name_prefix = \"{prefix}\"\n"
            ));
            let lease = strict_health_lease(&fields, "merge_group")
                .unwrap_or_else(|error| panic!("prefix {prefix} should pass: {}", error.message));
            assert_eq!(lease.runner_name_prefix, prefix);
        }
    }

    #[test]
    fn one_lease_may_not_authorize_both_trust_domains() {
        let fields = lease_fields("\nhealth_lease_events = [\"merge_group\", \"pull_request\"]\n");

        let Err(error) = strict_health_lease(&fields, "merge_group") else {
            panic!("mixed trust domains must fail closed");
        };
        assert!(
            error.message.contains("must not mix trust domains"),
            "error={}",
            error.message
        );
    }

    #[test]
    fn a_degenerate_capability_pair_is_rejected() {
        let fields =
            lease_fields("\nhealth_lease_forbidden_capability = \"acme-auto-linux-x64\"\n");

        let Err(error) = strict_health_lease(&fields, "merge_group") else {
            panic!("required == forbidden must fail closed");
        };
        assert!(
            error.message.contains("degenerate"),
            "error={}",
            error.message
        );
    }

    #[test]
    fn ttl_stays_bounded() {
        let fields = lease_fields("\nhealth_lease_ttl_seconds = 901\n");

        let Err(error) = strict_health_lease(&fields, "merge_group") else {
            panic!("oversized lease must fail");
        };
        assert!(error.message.contains("between 60 and 900"));
    }

    fn write_profile(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("profile.toml");
        fs::write(&path, body).expect("write profile");
        (dir, path)
    }

    #[test]
    fn loads_a_health_lease_for_a_non_pulp_repo_and_a_macos_lane() {
        // The whole point of the generalization: nothing about the repo slug,
        // the platform, or the variable name is baked into Shipyard.
        let (dir, path) = write_profile(
            r#"
name = "vellum-local"

[repo."Generous-Corp/vellum".pr.macos]
strategy = "ordered-fallback"
targets = ["m5.macos-arm64-vm", "github.macos-arm64"]
github_variable = "VELLUM_LOCAL_MACOS_RUNS_ON_JSON"
health_lease_variable = "VELLUM_LOCAL_MACOS_LEASE_UNTIL"
health_lease_ttl_seconds = 600
health_lease_events = ["pull_request"]
health_lease_runner_name_prefix = "vellum-pr-safe-ephemeral-"
health_lease_merge_queue_branch = "main"
health_lease_admission_burst = 3
health_lease_min_idle = 4
health_lease_required_capability = "vellum-pr-safe-macos-arm64"
health_lease_forbidden_capability = "vellum-auto-macos-arm64"

[targets."m5.macos-arm64-vm"]
runs_on_json = ["self-hosted", "macOS", "ARM64", "vellum-pr-safe-macos-arm64"]
proven = true
ephemeral = true

[targets."github.macos-arm64"]
runs_on_json = "macos-15"
"#,
        );

        let lease = load_local_linux_lease_profile(
            dir.path(),
            "vellum-local",
            Some(&path),
            "Generous-Corp/vellum",
            "pr",
            "macos",
        )
        .expect("lease profile");

        assert_eq!(lease.variable, "VELLUM_LOCAL_MACOS_LEASE_UNTIL");
        assert_eq!(lease.ttl_seconds, 600);
        assert_eq!(lease.events, ["pull_request"]);
        assert_eq!(lease.runner_name_prefix, "vellum-pr-safe-ephemeral-");
        assert_eq!(lease.admission_burst, 3);
        assert_eq!(lease.min_idle, 4);
        assert_eq!(lease.forbidden_capability, "vellum-auto-macos-arm64");
        assert!(
            lease
                .required_labels
                .iter()
                .any(|label| label == "vellum-pr-safe-macos-arm64")
        );
    }

    #[test]
    fn rejects_a_target_that_carries_the_forbidden_sibling_capability() {
        let (dir, path) = write_profile(
            r#"
name = "mixed"

[repo."owner/repo".merge_group.linux]
targets = ["host.linux-x64-vm"]
health_lease_variable = "TRUSTED_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["merge_group"]
health_lease_runner_name_prefix = "acme-ci-ephemeral-"
health_lease_merge_queue_branch = "main"
health_lease_admission_burst = 2
health_lease_required_capability = "acme-auto-linux-x64"
health_lease_forbidden_capability = "acme-pr-safe-linux-x64"

[targets."host.linux-x64-vm"]
runs_on_json = ["self-hosted", "Linux", "X64", "acme-auto-linux-x64", "acme-pr-safe-linux-x64"]
"#,
        );

        let error = load_local_linux_lease_profile(
            dir.path(),
            "mixed",
            Some(&path),
            "owner/repo",
            "merge_group",
            "linux",
        )
        .expect_err("mixed capability target must fail closed");

        assert!(
            error.message.contains("mixes")
                && error.message.contains("acme-auto-linux-x64")
                && error.message.contains("acme-pr-safe-linux-x64"),
            "error={}",
            error.message
        );
    }

    #[test]
    fn rejects_a_target_missing_the_required_capability() {
        let (dir, path) = write_profile(
            r#"
name = "unprotected"

[repo."owner/repo".merge_group.linux]
targets = ["host.linux-x64-vm"]
health_lease_variable = "TRUSTED_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["merge_group"]
health_lease_runner_name_prefix = "acme-ci-ephemeral-"
health_lease_merge_queue_branch = "main"
health_lease_admission_burst = 5
health_lease_required_capability = "acme-auto-linux-x64"
health_lease_forbidden_capability = "acme-pr-safe-linux-x64"

[targets."host.linux-x64-vm"]
runs_on_json = ["self-hosted", "Linux", "X64"]
"#,
        );

        let error = load_local_linux_lease_profile(
            dir.path(),
            "unprotected",
            Some(&path),
            "owner/repo",
            "merge_group",
            "linux",
        )
        .expect_err("unprotected target must fail");

        assert!(error.message.contains("acme-auto-linux-x64"));
    }

    #[test]
    fn a_lease_lane_must_declare_its_capability_namespace() {
        // Without a forbidden capability there is nothing separating this pool
        // from its sibling, so the lane must not load.
        let (dir, path) = write_profile(
            r#"
name = "no-namespace"

[repo."owner/repo".merge_group.linux]
targets = ["host.linux-x64-vm"]
health_lease_variable = "TRUSTED_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["merge_group"]
health_lease_runner_name_prefix = "acme-ci-ephemeral-"
health_lease_merge_queue_branch = "main"
health_lease_admission_burst = 5

[targets."host.linux-x64-vm"]
runs_on_json = ["self-hosted", "Linux", "X64"]
"#,
        );

        let error = load_local_linux_lease_profile(
            dir.path(),
            "no-namespace",
            Some(&path),
            "owner/repo",
            "merge_group",
            "linux",
        )
        .expect_err("missing capability namespace must fail");

        assert!(
            error.message.contains("health_lease_required_capability"),
            "error={}",
            error.message
        );
    }

    #[test]
    fn rejects_malformed_health_lease_admission_burst_instead_of_defaulting() {
        let (dir, path) = write_profile(
            r#"
name = "malformed"

[repo."owner/repo".merge_group.linux]
targets = ["host.linux-x64-vm"]
health_lease_variable = "TRUSTED_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["merge_group"]
health_lease_runner_name_prefix = "acme-ci-ephemeral-"
health_lease_merge_queue_branch = "main"
health_lease_admission_burst = "5"

[targets."host.linux-x64-vm"]
runs_on_json = ["self-hosted", "Linux", "X64"]
"#,
        );

        let error = load_local_linux_lease_profile(
            dir.path(),
            "malformed",
            Some(&path),
            "owner/repo",
            "merge_group",
            "linux",
        )
        .expect_err("malformed burst must fail");

        // The typed schema rejects the string before any range check runs.
        assert!(
            error.message.contains("health_lease_admission_burst")
                || error.message.contains("invalid type"),
            "error={}",
            error.message
        );
    }

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
    fn warns_for_unproven_self_managed_x64_without_arch_metadata() {
        for (target_id, selector) in [
            (
                "macpro.linux-vm",
                r#"["self-hosted", "Linux", "X64", "pulp-host-macpro"]"#,
            ),
            ("local.linux-x64", r#"["self-hosted", "Linux"]"#),
        ] {
            let profile: Table = format!(
                r#"
name = "normal"

[repo."owner/repo".pr.linux]
targets = ["{target_id}"]
github_variable = "LOCAL_LINUX_RUNS_ON_JSON"

[targets."{target_id}"]
runs_on_json = {selector}
"#
            )
            .parse()
            .expect("profile toml");

            let plan = build_plan(&profile, "owner/repo", "test.toml".to_owned()).expect("plan");
            assert_eq!(plan.warnings.len(), 1, "target={target_id}");
            assert!(plan.warnings[0].contains("self-managed x64 target"));
        }
    }

    #[test]
    fn proven_or_cloud_x64_targets_do_not_warn() {
        let profile = r#"
name = "normal"

[repo."owner/repo".pr.linux]
targets = ["local.linux-x64", "namespace.linux-x64"]
github_variable = "LOCAL_LINUX_RUNS_ON_JSON"

[targets."local.linux-x64"]
runs_on_json = ["self-hosted", "Linux", "X64"]
proven = true

[targets."namespace.linux-x64"]
provider = "namespace"
runs_on_json = ["self-hosted", "Linux", "X64"]
"#
        .parse()
        .expect("profile toml");

        let plan = build_plan(&profile, "owner/repo", "test.toml".to_owned()).expect("plan");
        assert!(plan.warnings.is_empty());
    }
}
