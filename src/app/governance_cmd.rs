use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::Utc;
use serde_json::{Value as JsonValue, json};
use toml::{Table, Value};

use super::{CliFailure, branch_cmd::detect_repo_from_remote, cli::GovernanceCommand};
use crate::config::LoadedConfig;
use crate::governance::{
    ApplyResult, BranchProtectionRules, GovernanceGh, build_apply_plan, build_status,
    compute_drift, execute_apply_plan, get_branch_protection, resolve_branch_rules,
    rules_from_toml_table, rules_to_toml_table,
};
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;

pub(super) fn governance_command<W: Write>(
    command: GovernanceCommand,
    mode: RuntimeMode,
    cwd: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    governance_command_with(command, &config, cwd, json_mode, stdout, None, None)
}

fn governance_command_with<W: Write>(
    command: GovernanceCommand,
    config: &LoadedConfig,
    cwd: &Path,
    json_mode: bool,
    stdout: &mut W,
    git_command: Option<&Path>,
    gh_command: Option<&Path>,
) -> Result<ExitCode, CliFailure> {
    let repo = detect_repo_from_remote(cwd, git_command)
        .ok_or_else(|| CliFailure::new(1, "Could not detect repo from git remote."))?;
    let gh = GovernanceGh::from_loaded_config(cwd, config, gh_command)
        .map_err(|error| CliFailure::new(1, error))?;
    match command {
        GovernanceCommand::Status { branches } => {
            let branches = branches_or_main(branches);
            governance_status(&repo, config, &branches, json_mode, stdout, &gh)
        }
        GovernanceCommand::Apply {
            branches,
            dry_run,
            from_path,
        } => governance_apply(
            &repo,
            config,
            &branches_or_main(branches),
            dry_run,
            from_path.as_deref(),
            json_mode,
            stdout,
            &gh,
        ),
        GovernanceCommand::Diff { branches } => {
            let branches = branches_or_main(branches);
            governance_diff(&repo, config, &branches, stdout, &gh)
        }
        GovernanceCommand::Export { branches, output } => {
            let branches = branches_or_main(branches);
            governance_export(&repo, &branches, output.as_deref(), stdout, &gh)
        }
        GovernanceCommand::Use {
            profile_name,
            yes,
            dry_run,
        } => governance_use(&repo, config, &profile_name, yes, dry_run, stdout, &gh),
    }
}

fn governance_status<W: Write>(
    repo: &str,
    config: &LoadedConfig,
    branches: &[String],
    json_mode: bool,
    stdout: &mut W,
    gh: &GovernanceGh,
) -> Result<ExitCode, CliFailure> {
    let status = build_status(repo, &config.data, branches, gh);
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), JsonValue::from(status.repo.clone()));
        data.insert(
            "profile".to_owned(),
            JsonValue::from(status.profile_name.clone()),
        );
        data.insert("has_drift".to_owned(), JsonValue::from(status.has_drift()));
        data.insert(
            "reports".to_owned(),
            JsonValue::Array(
                status
                    .reports
                    .iter()
                    .map(|report| {
                        json!({
                            "branch": report.branch,
                            "live_unprotected": report.live_unprotected,
                            "drifted_fields": report.drifted_entries().iter().map(|entry| entry.field_name).collect::<Vec<_>>(),
                            "deviated_fields": report.deviated_entries().iter().map(|entry| entry.field_name).collect::<Vec<_>>(),
                        })
                    })
                    .collect(),
            ),
        );
        data.insert(
            "errors".to_owned(),
            JsonValue::Array(status.errors.iter().cloned().map(JsonValue::from).collect()),
        );
        write_json_envelope(stdout, "governance.status", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        write_status_text(stdout, &status)?;
    }

    Ok(if status.has_drift() || status.has_errors() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

#[allow(clippy::too_many_arguments)]
fn governance_apply<W: Write>(
    repo: &str,
    config: &LoadedConfig,
    branches: &[String],
    dry_run: bool,
    from_path: Option<&Path>,
    json_mode: bool,
    stdout: &mut W,
    gh: &GovernanceGh,
) -> Result<ExitCode, CliFailure> {
    let (results, errors, from_snapshot) = if let Some(path) = from_path {
        apply_from_snapshot(repo, path, branches, dry_run, gh)?
    } else {
        apply_from_config(repo, config, branches, dry_run, gh)?
    };
    render_apply_results(stdout, &results, &errors, dry_run, from_snapshot, json_mode)?;
    let failed = !errors.is_empty() || results.iter().any(|result| result.error_message.is_some());
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn governance_diff<W: Write>(
    repo: &str,
    config: &LoadedConfig,
    branches: &[String],
    stdout: &mut W,
    gh: &GovernanceGh,
) -> Result<ExitCode, CliFailure> {
    let status = build_status(repo, &config.data, branches, gh);
    let mut any_drift = false;
    for report in &status.reports {
        if !report.has_drift() {
            writeln!(stdout, "  OK {}: no changes", report.branch)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            continue;
        }
        any_drift = true;
        if report.live_unprotected {
            writeln!(
                stdout,
                "  CREATE {}: create protection ({} fields)",
                report.branch,
                report.entries.len()
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        } else {
            let drifted = report.drifted_entries();
            writeln!(
                stdout,
                "  UPDATE {}: update {} field(s)",
                report.branch,
                drifted.len()
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            for entry in drifted {
                writeln!(
                    stdout,
                    "      {}: {} -> {}",
                    entry.field_name, entry.live_value, entry.declared_value
                )
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            }
        }
    }
    if any_drift {
        writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout, "Run: shipyard governance apply")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    if status.has_errors() {
        writeln!(
            stdout,
            "\ngovernance diff: live state could not be read for one or more branches"
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for error in status.errors {
            writeln!(stdout, "  ! {error}")
                .map_err(|io_error| CliFailure::new(1, io_error.to_string()))?;
        }
        return Ok(ExitCode::from(1));
    }
    Ok(ExitCode::SUCCESS)
}

fn governance_export<W: Write>(
    repo: &str,
    branches: &[String],
    output: Option<&Path>,
    stdout: &mut W,
    gh: &GovernanceGh,
) -> Result<ExitCode, CliFailure> {
    let mut live_branches = BTreeMap::new();
    let mut errors = Vec::new();
    for branch in branches {
        match get_branch_protection(repo, branch, gh) {
            Ok(Some(rules)) => {
                live_branches.insert(branch.clone(), rules);
            }
            Ok(None) => errors.push(format!("{branch}: no protection set")),
            Err(error) => errors.push(format!("{branch}: {error}")),
        }
    }
    if !errors.is_empty() {
        return Err(CliFailure::new(1, errors.join("\n")));
    }

    let toml_text = snapshot_to_toml(repo, &live_branches)?;
    if let Some(path) = output {
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        fs::write(path, toml_text).map_err(|error| {
            CliFailure::new(1, format!("failed to write {}: {error}", path.display()))
        })?;
        writeln!(stdout, "Wrote snapshot for {repo} to {}", path.display())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        write!(stdout, "{toml_text}").map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

fn governance_use<W: Write>(
    repo: &str,
    config: &LoadedConfig,
    profile_name: &str,
    yes: bool,
    dry_run: bool,
    stdout: &mut W,
    gh: &GovernanceGh,
) -> Result<ExitCode, CliFailure> {
    if !matches!(profile_name, "solo" | "multi" | "custom") {
        return Err(CliFailure::new(
            1,
            "Unknown governance profile. Expected one of: solo, multi, custom",
        ));
    }
    let config_path = config_path(config)?;
    let current_profile = config.get_str("project.profile").unwrap_or("solo");
    let mut hypothetical = config.data.clone();
    set_profile(&mut hypothetical, profile_name)?;
    let status = build_status(repo, &hypothetical, &[String::from("main")], gh);

    writeln!(
        stdout,
        "Switching profile: {current_profile} -> {profile_name}"
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    for report in &status.reports {
        if report.has_drift() {
            let drifted = report.drifted_entries();
            writeln!(
                stdout,
                "  {}: would update ({} field(s): {})",
                report.branch,
                drifted.len(),
                drifted
                    .iter()
                    .map(|entry| entry.field_name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        } else {
            writeln!(stdout, "  {}: no changes", report.branch)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    if dry_run {
        return Ok(ExitCode::SUCCESS);
    }
    if status.has_drift() && !yes {
        return Err(CliFailure::new(
            1,
            "Profile switch has live changes. Re-run with --yes to apply.",
        ));
    }

    rewrite_profile_in_config(&config_path, profile_name)?;
    writeln!(
        stdout,
        "Updated {} -> profile = \"{profile_name}\"",
        config_path.display()
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;

    let updated = LoadedConfig {
        data: hypothetical,
        global_dir: config.global_dir.clone(),
        project_dir: config.project_dir.clone(),
        local_dir: config.local_dir.clone(),
        local_overlay_source: config.local_overlay_source,
    };
    let (results, errors, _) =
        apply_from_config(repo, &updated, &[String::from("main")], false, gh)?;
    render_apply_results(stdout, &results, &errors, false, false, false)?;
    Ok(
        if errors.is_empty() && results.iter().all(|result| result.error_message.is_none()) {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        },
    )
}

fn apply_from_config(
    repo: &str,
    config: &LoadedConfig,
    branches: &[String],
    dry_run: bool,
    gh: &GovernanceGh,
) -> Result<(Vec<ApplyResult>, Vec<String>, bool), CliFailure> {
    let status = build_status(repo, &config.data, branches, gh);
    let mut results = Vec::new();
    for report in status.reports {
        let declared = resolve_branch_rules(&config.data, &report.branch)
            .map_err(|error| CliFailure::new(1, error))?;
        let branch = report.branch.clone();
        let plan = build_apply_plan(repo, &branch, declared, report);
        results.push(execute_apply_plan(plan, dry_run, gh));
    }
    Ok((results, status.errors, false))
}

fn apply_from_snapshot(
    repo: &str,
    path: &Path,
    branches: &[String],
    dry_run: bool,
    gh: &GovernanceGh,
) -> Result<(Vec<ApplyResult>, Vec<String>, bool), CliFailure> {
    let snapshot = parse_snapshot(path)?;
    if snapshot.repo != repo {
        return Err(CliFailure::new(
            1,
            format!(
                "Snapshot is for '{}' but current repo is '{repo}'. Refusing to apply.",
                snapshot.repo
            ),
        ));
    }
    let mut results = Vec::new();
    let mut errors = Vec::new();
    for branch in branches {
        let Some(declared) = snapshot.branches.get(branch) else {
            errors.push(format!("{branch}: not in snapshot"));
            continue;
        };
        let live = match get_branch_protection(repo, branch, gh) {
            Ok(rules) => rules,
            Err(error) => {
                errors.push(format!("{branch}: {error}"));
                continue;
            }
        };
        let report = compute_drift(branch, declared, declared, live.as_ref());
        let plan = build_apply_plan(repo, branch, declared.clone(), report);
        results.push(execute_apply_plan(plan, dry_run, gh));
    }
    Ok((results, errors, true))
}

fn render_apply_results<W: Write>(
    stdout: &mut W,
    results: &[ApplyResult],
    errors: &[String],
    dry_run: bool,
    from_snapshot: bool,
    json_mode: bool,
) -> Result<(), CliFailure> {
    let changed = results
        .iter()
        .any(|result| result.executed || (dry_run && !result.plan.is_noop()));
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("dry_run".to_owned(), JsonValue::from(dry_run));
        data.insert("from_snapshot".to_owned(), JsonValue::from(from_snapshot));
        data.insert("changed".to_owned(), JsonValue::from(changed));
        data.insert(
            "results".to_owned(),
            JsonValue::Array(
                results
                    .iter()
                    .map(|result| {
                        json!({
                            "branch": result.plan.branch,
                            "action": result.plan.action.as_str(),
                            "executed": result.executed,
                            "error": result.error_message,
                        })
                    })
                    .collect(),
            ),
        );
        data.insert(
            "errors".to_owned(),
            JsonValue::Array(errors.iter().cloned().map(JsonValue::from).collect()),
        );
        write_json_envelope(stdout, "governance.apply", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }

    if results.is_empty() && errors.is_empty() {
        writeln!(stdout, "No branches to apply to.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    for result in results {
        let branch = &result.plan.branch;
        let action = result.plan.action.as_str();
        if let Some(error) = &result.error_message {
            writeln!(stdout, "  ERROR {branch}: {action} failed - {error}")
                .map_err(|io_error| CliFailure::new(1, io_error.to_string()))?;
        } else if result.plan.is_noop() {
            writeln!(stdout, "  OK {branch}: already aligned (no changes)")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        } else if dry_run {
            let fields = result
                .plan
                .drift_report
                .drifted_entries()
                .iter()
                .map(|entry| entry.field_name)
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(
                stdout,
                "  WOULD {branch}: would {action} (fields: {fields})"
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        } else {
            writeln!(stdout, "  OK {branch}: {action} applied")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    for error in errors {
        writeln!(stdout, "  ! {error}")
            .map_err(|io_error| CliFailure::new(1, io_error.to_string()))?;
    }
    if let Some(result) = results.first() {
        writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(
            stdout,
            "Manual followups (Shipyard cannot apply these via API):"
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for followup in &result.plan.manual_followups {
            writeln!(stdout, "  ! {followup}")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(())
}

fn write_status_text<W: Write>(
    stdout: &mut W,
    status: &crate::governance::GovernanceStatus,
) -> Result<(), CliFailure> {
    writeln!(stdout, "Project: {}", status.repo)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "Profile: {}", status.profile_name)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if !status.errors.is_empty() {
        writeln!(stdout, "Errors: {}", status.errors.len())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for error in &status.errors {
            writeln!(stdout, "  ! {error}")
                .map_err(|io_error| CliFailure::new(1, io_error.to_string()))?;
        }
    }
    writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    for report in &status.reports {
        if report.live_unprotected {
            writeln!(
                stdout,
                "  ERROR {}: UNPROTECTED (run: shipyard governance apply)",
                report.branch
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            continue;
        }
        let drifted = report.drifted_entries();
        let deviated = report.deviated_entries();
        if drifted.is_empty() && deviated.is_empty() {
            writeln!(stdout, "  OK {}: aligned with profile", report.branch)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            continue;
        }
        if !drifted.is_empty() {
            writeln!(
                stdout,
                "  ERROR {}: {} field(s) drifted from config",
                report.branch,
                drifted.len()
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            for entry in &drifted {
                writeln!(
                    stdout,
                    "      {}: config={}, live={}",
                    entry.field_name, entry.declared_value, entry.live_value
                )
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            }
            writeln!(stdout, "      fix: shipyard governance apply")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        if !deviated.is_empty() && drifted.is_empty() {
            writeln!(
                stdout,
                "  INFO {}: {} field(s) deviated from profile",
                report.branch,
                deviated.len()
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            for entry in deviated {
                writeln!(
                    stdout,
                    "      {}: profile={}, config={}",
                    entry.field_name, entry.profile_value, entry.declared_value
                )
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            }
        }
    }
    Ok(())
}

fn branches_or_main(branches: Vec<String>) -> Vec<String> {
    if branches.is_empty() {
        vec![String::from("main")]
    } else {
        branches
    }
}

fn snapshot_to_toml(
    repo: &str,
    live_branches: &BTreeMap<String, BranchProtectionRules>,
) -> Result<String, CliFailure> {
    let mut root = Table::new();
    let mut header = Table::new();
    header.insert("schema_version".to_owned(), Value::Integer(1));
    header.insert("repo".to_owned(), Value::String(repo.to_owned()));
    header.insert(
        "exported_at".to_owned(),
        Value::String(Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
    );
    root.insert(
        "shipyard_governance_snapshot".to_owned(),
        Value::Table(header),
    );
    let mut branch_table = Table::new();
    for (branch, rules) in live_branches {
        branch_table.insert(branch.clone(), Value::Table(rules_to_toml_table(rules)));
    }
    if !branch_table.is_empty() {
        root.insert("branch_protection".to_owned(), Value::Table(branch_table));
    }
    toml::to_string_pretty(&root).map_err(|error| CliFailure::new(1, error.to_string()))
}

struct ParsedSnapshot {
    repo: String,
    branches: BTreeMap<String, BranchProtectionRules>,
}

fn parse_snapshot(path: &Path) -> Result<ParsedSnapshot, CliFailure> {
    let text = fs::read_to_string(path).map_err(|error| {
        CliFailure::new(1, format!("failed to read {}: {error}", path.display()))
    })?;
    let data = text
        .parse::<Table>()
        .map_err(|error| CliFailure::new(1, format!("Could not parse snapshot: {error}")))?;
    let header = data
        .get("shipyard_governance_snapshot")
        .and_then(Value::as_table)
        .ok_or_else(|| {
            CliFailure::new(
                1,
                "Could not parse snapshot: missing [shipyard_governance_snapshot] header",
            )
        })?;
    let schema_version = header
        .get("schema_version")
        .and_then(Value::as_integer)
        .unwrap_or(0);
    if schema_version != 1 {
        return Err(CliFailure::new(
            1,
            format!("Could not parse snapshot: unsupported schema version {schema_version}"),
        ));
    }
    let repo = header
        .get("repo")
        .and_then(Value::as_str)
        .filter(|repo| !repo.is_empty())
        .ok_or_else(|| CliFailure::new(1, "Could not parse snapshot: missing repo"))?
        .to_owned();
    let mut branches = BTreeMap::new();
    if let Some(branch_table) = data.get("branch_protection").and_then(Value::as_table) {
        for (branch, value) in branch_table {
            if let Some(table) = value.as_table() {
                branches.insert(branch.clone(), rules_from_toml_table(table));
            }
        }
    }
    Ok(ParsedSnapshot { repo, branches })
}

fn config_path(config: &LoadedConfig) -> Result<PathBuf, CliFailure> {
    let Some(project_dir) = &config.project_dir else {
        return Err(CliFailure::new(
            1,
            "No .shipyard/config.toml found. Run `shipyard init` first.",
        ));
    };
    let path = project_dir.join("config.toml");
    if path.exists() {
        Ok(path)
    } else {
        Err(CliFailure::new(
            1,
            format!("Config file not found: {}", path.display()),
        ))
    }
}

fn set_profile(data: &mut Table, profile_name: &str) -> Result<(), CliFailure> {
    if !data.contains_key("project") {
        data.insert("project".to_owned(), Value::Table(Table::new()));
    }
    let Some(project) = data.get_mut("project").and_then(Value::as_table_mut) else {
        return Err(CliFailure::new(1, "[project] config is not a table"));
    };
    project.insert("profile".to_owned(), Value::String(profile_name.to_owned()));
    Ok(())
}

fn rewrite_profile_in_config(path: &Path, profile_name: &str) -> Result<(), CliFailure> {
    let text = fs::read_to_string(path).map_err(|error| {
        CliFailure::new(1, format!("failed to read {}: {error}", path.display()))
    })?;
    let mut output = Vec::new();
    let mut in_project = false;
    let mut replaced = false;
    for line in text.split_inclusive('\n') {
        let stripped = line.trim();
        if stripped.starts_with('[') && stripped.ends_with(']') {
            in_project = stripped == "[project]";
            output.push(line.to_owned());
            continue;
        }
        if in_project && stripped.starts_with("profile") {
            let leading = &line[..line.len() - line.trim_start().len()];
            output.push(format!("{leading}profile   = \"{profile_name}\"\n"));
            replaced = true;
            continue;
        }
        output.push(line.to_owned());
    }
    if !replaced {
        let mut with_profile = Vec::new();
        for line in output {
            let found_project_header = line.trim() == "[project]";
            with_profile.push(line);
            if found_project_header {
                with_profile.push(format!("profile   = \"{profile_name}\"\n"));
                replaced = true;
            }
        }
        output = with_profile;
    }
    if !replaced {
        output.push(format!("\n[project]\nprofile   = \"{profile_name}\"\n"));
    }
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    fs::write(path, output.concat())
        .map_err(|error| CliFailure::new(1, format!("failed to write {}: {error}", path.display())))
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;
    use toml::Table;

    use super::{governance_command_with, rewrite_profile_in_config};
    use crate::app::cli::GovernanceCommand;
    use crate::config::{LoadedConfig, LocalOverlaySource};

    #[test]
    fn governance_status_json_reports_live_drift() {
        let temp = TempDir::new().expect("tempdir");
        let git = fake_git(temp.path());
        let gh = fake_gh_read(temp.path());
        let mut stdout = Vec::new();

        let code = governance_command_with(
            GovernanceCommand::Status {
                branches: vec![String::from("main")],
            },
            &loaded_config(temp.path()),
            temp.path(),
            true,
            &mut stdout,
            Some(&git),
            Some(&gh),
        )
        .expect("status");

        assert_eq!(code, std::process::ExitCode::from(1));
        let value: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "governance.status");
        assert_eq!(value["repo"], "owner/repo");
        assert_eq!(value["has_drift"], true);
        assert!(
            value["reports"][0]["drifted_fields"]
                .as_array()
                .expect("drifted fields")
                .iter()
                .any(|field| field == "require_strict_status")
        );
    }

    #[test]
    fn governance_apply_dry_run_plans_without_put() {
        let temp = TempDir::new().expect("tempdir");
        let git = fake_git(temp.path());
        let put_marker = temp.path().join("put-called");
        let gh = fake_gh_read_and_put(temp.path(), &put_marker);
        let mut stdout = Vec::new();

        let code = governance_command_with(
            GovernanceCommand::Apply {
                branches: vec![String::from("main")],
                dry_run: true,
                from_path: None,
            },
            &loaded_config(temp.path()),
            temp.path(),
            true,
            &mut stdout,
            Some(&git),
            Some(&gh),
        )
        .expect("apply");

        assert_eq!(code, std::process::ExitCode::SUCCESS);
        assert!(!put_marker.exists());
        let value: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "governance.apply");
        assert_eq!(value["changed"], true);
        assert_eq!(value["results"][0]["action"], "update");
        assert_eq!(value["results"][0]["executed"], false);
    }

    #[test]
    fn governance_export_writes_snapshot_file() {
        let temp = TempDir::new().expect("tempdir");
        let git = fake_git(temp.path());
        let gh = fake_gh_read(temp.path());
        let output = temp.path().join("snapshot.toml");
        let mut stdout = Vec::new();

        let code = governance_command_with(
            GovernanceCommand::Export {
                branches: vec![String::from("main")],
                output: Some(output.clone()),
            },
            &loaded_config(temp.path()),
            temp.path(),
            false,
            &mut stdout,
            Some(&git),
            Some(&gh),
        )
        .expect("export");

        assert_eq!(code, std::process::ExitCode::SUCCESS);
        let snapshot = fs::read_to_string(output).expect("snapshot");
        assert!(snapshot.contains("[shipyard_governance_snapshot]"));
        assert!(snapshot.contains("repo = \"owner/repo\""));
        assert!(snapshot.contains("[branch_protection.main]"));
    }

    #[test]
    fn rewrite_profile_preserves_project_section_shape() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("config.toml");
        fs::write(
            &path,
            "[project]\nname = \"demo\"\nprofile   = \"solo\"\n\n[targets.local]\nbackend = \"local\"\n",
        )
        .expect("write config");

        rewrite_profile_in_config(&path, "multi").expect("rewrite");

        let text = fs::read_to_string(path).expect("config");
        assert!(text.contains("profile   = \"multi\""));
        assert!(text.contains("[targets.local]"));
    }

    fn loaded_config(root: &std::path::Path) -> LoadedConfig {
        let data = r#"
            [project]
            profile = "multi"

            [governance]
            required_status_checks = ["ci"]
        "#
        .parse::<Table>()
        .expect("toml");
        LoadedConfig {
            data,
            global_dir: root.join("global"),
            project_dir: Some(root.join(".shipyard")),
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn fake_git(dir: &std::path::Path) -> std::path::PathBuf {
        write_script(
            dir,
            "git",
            r#"#!/bin/sh
if [ "$1" = "remote" ]; then
  printf 'git@github.com:owner/repo.git\n'
  exit 0
fi
exit 99
"#,
        )
    }

    fn fake_gh_read(dir: &std::path::Path) -> std::path::PathBuf {
        let payload = write_live_payload(dir);
        write_script(
            dir,
            "gh-read",
            &format!(
                r#"#!/bin/sh
if [ "$1" = "api" ]; then
  cat '{}'
  exit 0
fi
exit 99
"#,
                payload.display()
            ),
        )
    }

    fn fake_gh_read_and_put(
        dir: &std::path::Path,
        put_marker: &std::path::Path,
    ) -> std::path::PathBuf {
        let payload = write_live_payload(dir);
        write_script(
            dir,
            "gh-read-put",
            &format!(
                r#"#!/bin/sh
if [ "$1" = "api" ] && [ "$2" = "-X" ]; then
  cat > '{}'
  exit 0
fi
if [ "$1" = "api" ]; then
  cat '{}'
  exit 0
fi
exit 99
"#,
                put_marker.display(),
                payload.display()
            ),
        )
    }

    fn write_live_payload(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("live.json");
        fs::write(
            &path,
            r#"{
  "required_status_checks": {"strict": false, "contexts": ["ci"]},
  "required_pull_request_reviews": {
    "required_approving_review_count": 0,
    "dismiss_stale_reviews": false,
    "require_code_owner_reviews": false
  },
  "enforce_admins": {"enabled": false},
  "allow_force_pushes": {"enabled": false},
  "allow_deletions": {"enabled": false},
  "required_linear_history": {"enabled": false},
  "required_conversation_resolution": {"enabled": false}
}"#,
        )
        .expect("write payload");
        path
    }

    fn write_script(dir: &std::path::Path, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).expect("write script");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod");
        path
    }
}
