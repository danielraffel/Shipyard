//! `shipyard reuse switch|trip|rederive|rederive-sweep`: read the live-reuse
//! kill switch, turn it off and file the tracking issue, or re-derive keyed
//! shadow runs on this host. The switch policy is in
//! [`crate::changed_surface::live_switch`]; this is the `gh` half.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use chrono::Utc;
use serde_json::Value;

use super::CliFailure;
use super::cli::{
    ReuseCommand, ReuseRecordsArgs, ReuseRederiveArgs, ReuseRederiveSweepArgs, ReuseSwitchArgs,
    ReuseTripArgs,
};
use super::reuse_rederive::{ProducedBy, rederive_trial, sweep};
use crate::changed_surface::live_switch::{self, SwitchMode};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::landability::gate;
use crate::output::write_json_envelope;

pub(super) fn reuse_command<W: Write>(
    command: ReuseCommand,
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let actions = GitHubActions::from_loaded_config(cwd, config);
    let gh = |args: &[String]| actions.run_gh(args).map_err(|error| error.to_string());
    match command {
        ReuseCommand::Switch(args) => switch(&gh, &args, config, cwd, json, stdout),
        ReuseCommand::Trip(args) => trip(&gh, &args, config, cwd, json, stdout),
        ReuseCommand::Rederive(args) => rederive(
            &|_: &Path, args: &[String]| gh(args),
            &args,
            config,
            cwd,
            state_dir,
            json,
            stdout,
        ),
        ReuseCommand::Records(args) => records(&args, config, cwd, state_dir, json, stdout),
        ReuseCommand::RederiveSweep(args) => rederive_sweep(
            &|_: &Path, args: &[String]| gh(args),
            &args,
            state_dir,
            json,
            stdout,
        ),
    }
}

fn records<W: Write>(
    args: &ReuseRecordsArgs,
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mode = super::ship_cmd::changed_surface_execution::machine_mode(config)?;
    let mut found = super::ship_cmd::executable_reuse_plan::bindable_records(
        cwd,
        state_dir,
        &args.base,
        &args.target,
    )
    .map_err(|error| CliFailure::new(1, error))?;
    narrow(&mut found, args.repo.as_deref(), args.sha.as_deref())?;
    let human = records_line(&found, mode);
    let Value::Object(fields) =
        serde_json::to_value(&found).map_err(|error| CliFailure::new(1, error.to_string()))?
    else {
        return Err(CliFailure::new(
            1,
            "the records did not serialize as an object",
        ));
    };
    let mut data: BTreeMap<String, Value> = fields.into_iter().collect();
    data.insert(
        "changed_surface_execution_mode".to_owned(),
        Value::from(mode),
    );
    emit(stdout, json, "reuse.records", data, &human)?;
    Ok(ExitCode::SUCCESS)
}

/// Refuse a caller expecting another repository; keep only `sha`'s records.
fn narrow(
    found: &mut super::ship_cmd::executable_reuse_plan::BindableRecords,
    repo: Option<&str>,
    sha: Option<&str>,
) -> Result<(), CliFailure> {
    if let Some(expected) = repo
        && !expected.eq_ignore_ascii_case(&found.repository)
    {
        return Err(CliFailure::new(
            2,
            format!(
                "the base names repository {}, not {expected}",
                found.repository
            ),
        ));
    }
    if let Some(sha) = sha {
        found.records.retain(|record| record.sha == sha);
    }
    Ok(())
}

fn records_line(
    found: &super::ship_cmd::executable_reuse_plan::BindableRecords,
    mode: &str,
) -> String {
    let head = format!(
        "{} {} at {} on {} (mode {mode})",
        found.repository, found.target, found.base_sha, found.platform
    );
    let mut lines = vec![match &found.no_base {
        Some(why) => format!("{head}: 0 bindable records ({why})"),
        None => format!("{head}: {} bindable record(s)", found.bindable),
    }];
    for record in &found.records {
        lines.push(format!(
            "  {} {} {}{}",
            record.sha,
            record.run_id,
            if record.candidate {
                "candidate"
            } else if record.bindable {
                "bindable"
            } else {
                "refused"
            },
            record
                .reason
                .as_deref()
                .map(|why| format!(": {why}"))
                .unwrap_or_default()
        ));
    }
    lines.join("\n")
}

fn rederive<F, W>(
    gh: &F,
    args: &ReuseRederiveArgs,
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure>
where
    F: Fn(&Path, &[String]) -> Result<String, String>,
    W: Write,
{
    let identity = crate::changed_surface::trial::TrialIdentity {
        repository: repo(args.repo.as_deref(), config, cwd)?,
        pull_request: args.pr,
        target: args.target.clone(),
        head_sha: args.head.clone(),
    };
    let outcomes = rederive_trial(state_dir, &identity, ProducedBy::Operator, gh);
    let errors: Vec<&str> = outcomes
        .iter()
        .filter_map(|payload| payload.error.as_deref())
        .collect();
    if !errors.is_empty() {
        return Err(CliFailure::new(1, errors.join("; ")));
    }
    let mut data = BTreeMap::new();
    data.insert(
        "outcomes".to_owned(),
        serde_json::to_value(&outcomes).map_err(|error| CliFailure::new(1, error.to_string()))?,
    );
    let human = serde_json::to_string(&outcomes).unwrap_or_default();
    emit(stdout, json, "reuse.rederive", data, &human)?;
    Ok(ExitCode::SUCCESS)
}

fn rederive_sweep<F, W>(
    gh: &F,
    args: &ReuseRederiveSweepArgs,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure>
where
    F: Fn(&Path, &[String]) -> Result<String, String>,
    W: Write,
{
    let swept = sweep(state_dir, args.cap, gh);
    let mut failed = false;
    let rows = swept
        .iter()
        .map(|(identity, outcome)| {
            failed |= outcome.is_err();
            serde_json::json!({
                "repository": identity.repository,
                "pull_request": identity.pull_request,
                "target": identity.target,
                "head_sha": identity.head_sha,
                "outcome": match outcome {
                    Ok(outcome) => serde_json::to_value(outcome).unwrap_or(Value::Null),
                    Err(error) => serde_json::json!({"outcome": "error", "error": error}),
                },
            })
        })
        .collect::<Vec<_>>();
    let human = format!("re-derived {} keyed runs", rows.len());
    let mut data = BTreeMap::new();
    data.insert("runs".to_owned(), Value::Array(rows));
    emit(stdout, json, "reuse.rederive_sweep", data, &human)?;
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn repo(explicit: Option<&str>, config: &LoadedConfig, cwd: &Path) -> Result<String, CliFailure> {
    explicit
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| gate::resolve_repo(config, cwd))
        .ok_or_else(|| CliFailure::new(1, "No repo detected. Pass --repo OWNER/REPO."))
}

fn emit<W: Write>(
    stdout: &mut W,
    json: bool,
    command: &str,
    data: BTreeMap<String, Value>,
    human: &str,
) -> Result<(), CliFailure> {
    if json {
        write_json_envelope(stdout, command, data)
            .map_err(|error| CliFailure::new(1, error.to_string()))
    } else {
        writeln!(stdout, "{human}").map_err(|error| CliFailure::new(1, error.to_string()))
    }
}

fn switch<F, W>(
    gh: &F,
    args: &ReuseSwitchArgs,
    config: &LoadedConfig,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure>
where
    F: Fn(&[String]) -> Result<String, String>,
    W: Write,
{
    let repo = repo(args.repo.as_deref(), config, cwd)?;
    let reading = live_switch::read_switch(gh, &repo, &args.variable, Utc::now());
    let mode = match reading.mode {
        SwitchMode::Live => "live",
        SwitchMode::Shadow => "shadow",
    };
    let mut data = BTreeMap::new();
    data.insert("repo".to_owned(), Value::from(repo.clone()));
    data.insert(
        "reading".to_owned(),
        serde_json::to_value(&reading).map_err(|error| CliFailure::new(1, error.to_string()))?,
    );
    emit(
        stdout,
        json,
        "reuse.switch",
        data,
        &format!("{repo} {}: {mode} ({})", reading.variable, reading.reason),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn trip<F, W>(
    gh: &F,
    args: &ReuseTripArgs,
    config: &LoadedConfig,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure>
where
    F: Fn(&[String]) -> Result<String, String>,
    W: Write,
{
    if args.reason.trim().is_empty() {
        return Err(CliFailure::new(
            2,
            "--reason must say why the switch is being turned off",
        ));
    }
    let repo = repo(args.repo.as_deref(), config, cwd)?;
    let outcome = live_switch::trip(
        gh,
        &repo,
        &args.variable,
        &args.reason,
        Utc::now(),
        args.apply,
    )
    .map_err(|error| CliFailure::new(1, error))?;
    let mut data = BTreeMap::new();
    data.insert("repo".to_owned(), Value::from(repo.clone()));
    data.insert(
        "outcome".to_owned(),
        serde_json::to_value(&outcome).map_err(|error| CliFailure::new(1, error.to_string()))?,
    );
    let dry = if outcome.applied {
        ""
    } else {
        " (dry run; pass --apply to send)"
    };
    emit(
        stdout,
        json,
        "reuse.trip",
        data,
        &format!("{repo}: {}; {}{dry}", outcome.variable, outcome.issue),
    )?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ship_cmd::executable_reuse_plan::{BindableRecords, RecordJudgment};

    fn found() -> BindableRecords {
        let record = |sha: &str, bindable: bool| RecordJudgment {
            sha: sha.to_owned(),
            target: "mac".to_owned(),
            run_id: format!("{sha}-run"),
            path: format!("/store/{sha}/{sha}-run"),
            filed_at: "2026-10-04T00:00:00Z".to_owned(),
            bindable,
            candidate: bindable,
            reason: (!bindable).then(|| "unusable: dirty".to_owned()),
        };
        BindableRecords {
            repository: "Owner/Repo".to_owned(),
            target: "mac".to_owned(),
            base_sha: "b".to_owned(),
            platform: "darwin-arm64".to_owned(),
            bindable: 1,
            candidates: Vec::new(),
            no_base: None,
            records: vec![record("a", true), record("c", false)],
        }
    }

    #[test]
    fn records_narrow_to_one_commit_and_refuse_another_repository() {
        let mut all = found();
        narrow(&mut all, Some("owner/repo"), None).expect("same repository, any case");
        assert_eq!(all.records.len(), 2);

        let mut one = found();
        narrow(&mut one, None, Some("c")).expect("narrow");
        assert_eq!(one.records.len(), 1);
        assert_eq!(one.records[0].sha, "c");
        assert_eq!(one.bindable, 1, "the plan's own count is not narrowed");

        let mut none = found();
        narrow(&mut none, None, Some("d")).expect("narrow");
        assert!(none.records.is_empty(), "an absent commit lists nothing");

        let refused = narrow(&mut found(), Some("Other/Repo"), None).expect_err("other repo");
        assert_eq!(refused.code, 2);
    }

    #[test]
    fn the_listing_names_each_record_and_why_it_is_refused() {
        let line = records_line(&found(), "shadow_compare");
        assert!(
            line.contains("(mode shadow_compare): 1 bindable record(s)"),
            "{line}"
        );
        assert!(line.contains("a a-run candidate"), "{line}");
        assert!(line.contains("c c-run refused: unusable: dirty"), "{line}");
    }
}
