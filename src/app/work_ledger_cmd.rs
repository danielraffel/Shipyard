use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use crate::output::write_pretty_json;
use crate::work_ledger::{
    RepoPolicy, WorkLedger, absent_status, apply_legacy_snapshot, plan_legacy_snapshot,
    validate_repo_policy,
};

use super::CliFailure;
use super::cli::{WorkLedgerCommand, WorkLedgerPolicyCommand};

pub(super) fn work_ledger_command<W: Write>(
    command: &WorkLedgerCommand,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        WorkLedgerCommand::Status => {
            let status = WorkLedger::open_existing(state_dir)
                .map_err(failure)?
                .map_or_else(|| Ok(absent_status()), |ledger| ledger.status())
                .map_err(failure)?;
            if json {
                write_pretty_json(stdout, &status).map_err(failure)?;
            } else {
                writeln!(
                    stdout,
                    "Work ledger: {}",
                    if status.exists { "present" } else { "absent" }
                )
                .map_err(failure)?;
                writeln!(stdout, "Schema: {}", status.schema_version).map_err(failure)?;
                writeln!(stdout, "Integrity: {}", status.integrity).map_err(failure)?;
                writeln!(stdout, "Journal: {}", status.journal_mode).map_err(failure)?;
                writeln!(stdout, "Synchronous: {}", status.synchronous).map_err(failure)?;
                writeln!(stdout, "Foreign keys: {}", status.foreign_keys).map_err(failure)?;
                writeln!(stdout, "Work items: {}", status.work_items).map_err(failure)?;
                writeln!(stdout, "Pending wakes: {}", status.pending_wakes).map_err(failure)?;
                writeln!(stdout, "Uncertain wakes: {}", status.uncertain_wakes).map_err(failure)?;
                writeln!(stdout, "Activation: disabled (shadow only)").map_err(failure)?;
                writeln!(stdout, "Dispatch: disabled (shadow only)").map_err(failure)?;
            }
        }
        WorkLedgerCommand::Import { apply } => {
            let report = if *apply {
                apply_legacy_snapshot(state_dir).map_err(failure)?
            } else {
                plan_legacy_snapshot(state_dir).map_err(failure)?
            };
            if json {
                write_pretty_json(stdout, &report).map_err(failure)?;
            } else {
                writeln!(
                    stdout,
                    "Legacy import: {} (shadow only)",
                    if report.applied { "applied" } else { "dry-run" }
                )
                .map_err(failure)?;
                writeln!(stdout, "Candidates: {}", report.candidates).map_err(failure)?;
                writeln!(stdout, "Inserted: {}", report.inserted).map_err(failure)?;
                writeln!(stdout, "Updated: {}", report.updated).map_err(failure)?;
                writeln!(stdout, "Unchanged: {}", report.unchanged).map_err(failure)?;
                for (kind, count) in &report.by_kind {
                    writeln!(stdout, "  {kind}: {count}").map_err(failure)?;
                }
                writeln!(stdout, "Plan digest: {}", report.plan_digest).map_err(failure)?;
                writeln!(stdout, "Activation: disabled").map_err(failure)?;
                writeln!(stdout, "Dispatch: disabled").map_err(failure)?;
            }
        }
        WorkLedgerCommand::Policy { command } => {
            policy_command(command, state_dir, json, stdout)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn policy_command<W: Write>(
    command: &WorkLedgerPolicyCommand,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    match command {
        WorkLedgerPolicyCommand::List => policy_list(state_dir, json, stdout),
        WorkLedgerPolicyCommand::Set { .. } => policy_set(command, state_dir, json, stdout),
    }
}

fn policy_list<W: Write>(state_dir: &Path, json: bool, stdout: &mut W) -> Result<(), CliFailure> {
    let policies = WorkLedger::open_existing(state_dir)
        .map_err(failure)?
        .map_or_else(
            || Ok(crate::work_ledger::default_repo_policies()),
            |ledger| ledger.repo_policies(),
        )
        .map_err(failure)?;
    if json {
        write_pretty_json(
            stdout,
            &serde_json::json!({
                "mode": "shadow",
                "activation_enabled": false,
                "dispatch_enabled": false,
                "policies": policies,
            }),
        )
        .map_err(failure)?;
    } else {
        if policies.is_empty() {
            writeln!(stdout, "No repository policies in the shadow ledger.").map_err(failure)?;
        }
        for policy in policies {
            writeln!(
                stdout,
                "{}: primary={} compatibility={} lanes={:?} blocking={} dependencies={:?} revision={}",
                policy.repo,
                policy.primary_platform,
                policy.compatibility_mode,
                policy.compatibility_lanes,
                policy.blocking_rule,
                policy.declared_dependency_lanes,
                policy.revision
            )
            .map_err(failure)?;
        }
        writeln!(stdout, "Scheduler activation: disabled (shadow only)").map_err(failure)?;
        writeln!(stdout, "Dispatch: disabled (shadow only)").map_err(failure)?;
    }
    Ok(())
}

fn policy_set<W: Write>(
    command: &WorkLedgerPolicyCommand,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let WorkLedgerPolicyCommand::Set {
        repo,
        primary_platform,
        compatibility_mode,
        compatibility_lanes,
        blocking_rule,
        declared_dependency_lanes,
        expected_revision,
        apply,
    } = command
    else {
        unreachable!("policy_set only receives set commands")
    };
    let planned = RepoPolicy {
        repo: repo.clone(),
        primary_platform: primary_platform.clone(),
        compatibility_mode: compatibility_mode.clone(),
        compatibility_lanes: {
            let mut lanes = compatibility_lanes.clone();
            lanes.sort();
            lanes
        },
        blocking_rule: blocking_rule.clone(),
        declared_dependency_lanes: {
            let mut lanes = declared_dependency_lanes.clone();
            lanes.sort();
            lanes
        },
        revision: *expected_revision,
    };
    validate_repo_policy(&planned, *expected_revision).map_err(failure)?;
    let policy = if *apply {
        WorkLedger::open(state_dir)
            .and_then(|ledger| ledger.set_repo_policy(&planned, *expected_revision))
            .map_err(failure)?
    } else {
        WorkLedger::open_existing(state_dir)
            .map_err(failure)?
            .map_or_else(
                || {
                    if *expected_revision
                        != crate::work_ledger::absent_policy_revision(&planned.repo)
                    {
                        return Err(failure("repository policy revision no longer matches"));
                    }
                    let mut next = planned.clone();
                    next.revision = expected_revision + 1;
                    Ok(next)
                },
                |ledger| {
                    ledger
                        .plan_repo_policy(&planned, *expected_revision)
                        .map_err(failure)
                },
            )?
    };
    if json {
        write_pretty_json(
            stdout,
            &serde_json::json!({
                "mode": "shadow",
                "activation_enabled": false,
                "dispatch_enabled": false,
                "policy": policy,
            }),
        )
        .map_err(failure)?;
    } else {
        writeln!(
            stdout,
            "{} policy {}: primary={} compatibility={} lanes={:?} blocking={} dependencies={:?} revision={}",
            policy.repo,
            if *apply { "applied" } else { "planned" },
            policy.primary_platform,
            policy.compatibility_mode,
            policy.compatibility_lanes,
            policy.blocking_rule,
            policy.declared_dependency_lanes,
            policy.revision
        )
        .map_err(failure)?;
        writeln!(stdout, "Scheduler activation: disabled (shadow only)").map_err(failure)?;
        writeln!(stdout, "Dispatch: disabled (shadow only)").map_err(failure)?;
    }
    Ok(())
}

fn failure(error: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(1, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tempfile::TempDir;

    #[test]
    fn policy_json_always_reports_shadow_activation_and_dispatch() {
        let temp = TempDir::new().expect("temp");
        for command in [
            WorkLedgerPolicyCommand::List,
            WorkLedgerPolicyCommand::Set {
                repo: "generous-corp/forge".to_owned(),
                primary_platform: "macos".to_owned(),
                compatibility_mode: "independent".to_owned(),
                compatibility_lanes: vec!["linux".to_owned(), "windows".to_owned()],
                blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                declared_dependency_lanes: Vec::new(),
                expected_revision: 1,
                apply: false,
            },
        ] {
            let mut output = Vec::new();
            policy_command(&command, temp.path(), true, &mut output).expect("policy json");
            let value: Value = serde_json::from_slice(&output).expect("valid json");
            assert_eq!(value["mode"], "shadow");
            assert_eq!(value["activation_enabled"], false);
            assert_eq!(value["dispatch_enabled"], false);
        }
    }

    #[test]
    fn policy_json_exposes_builtins_and_plans_first_override_as_revision_two() {
        let temp = TempDir::new().expect("temp");
        let mut output = Vec::new();
        policy_list(temp.path(), true, &mut output).expect("policy list");
        let listed: Value = serde_json::from_slice(&output).expect("valid list json");
        assert_eq!(listed["policies"].as_array().map(Vec::len), Some(3));

        output.clear();
        policy_set(
            &WorkLedgerPolicyCommand::Set {
                repo: "generous-corp/forge".to_owned(),
                primary_platform: "macos".to_owned(),
                compatibility_mode: "independent".to_owned(),
                compatibility_lanes: vec!["linux".to_owned(), "windows".to_owned()],
                blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                declared_dependency_lanes: Vec::new(),
                expected_revision: 1,
                apply: false,
            },
            temp.path(),
            true,
            &mut output,
        )
        .expect("policy plan");
        let planned: Value = serde_json::from_slice(&output).expect("valid plan json");
        assert_eq!(planned["policy"]["revision"], 2);
    }
}
