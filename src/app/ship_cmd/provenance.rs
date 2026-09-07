use std::io::Write;
use std::path::Path;
use std::process::Command;

#[cfg(not(test))]
use super::steward_handoff_command;
use super::{
    CliFailure, LoadedConfig, ResolvedPrContext, ShipStewardHandoff, StewardHandoffArgs,
    steward_handoff_transfer_report,
};
#[cfg(test)]
use crate::app::merge_steward_cmd::steward_handoff_command_without_ambient;
use crate::cloud::GitHubActions;
use crate::paths::RuntimePaths;

#[derive(Clone, Debug)]
pub(super) struct AppliedStewardHandoff {
    pub(super) workstream_id: String,
    pub(super) context_url: Option<String>,
    pub(super) head: String,
    pub(super) monitoring_transferred: bool,
    pub(super) agent_disposition: String,
    pub(super) pause_required: bool,
    pub(super) publication_work_id: Option<String>,
    pub(super) publication_route_ref: Option<String>,
    pub(super) publication_wake_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PrProvenanceHook {
    pub(super) command: Vec<String>,
    pub(super) required: bool,
}

pub(super) fn configured_pr_provenance_hook(
    config: &LoadedConfig,
) -> Result<Option<PrProvenanceHook>, CliFailure> {
    let Some(value) = config.get("pr.provenance.command") else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err(CliFailure::new(
            2,
            "pr.provenance.command must be a non-empty TOML string array",
        ));
    };
    let command = items
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| CliFailure::new(2, "pr.provenance.command must contain only strings"))?;
    if command.is_empty() || command[0].is_empty() {
        return Err(CliFailure::new(
            2,
            "pr.provenance.command must be a non-empty TOML string array",
        ));
    }
    let required = config
        .get("pr.provenance.required")
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    Ok(Some(PrProvenanceHook { command, required }))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_pr_provenance_hook<W: Write>(
    config: &LoadedConfig,
    cwd: &Path,
    stdout: &mut W,
    repo: &str,
    branch: &str,
    base: &str,
    head: &str,
    pr: &ResolvedPrContext,
) -> Result<(), CliFailure> {
    let Some(hook) = configured_pr_provenance_hook(config)? else {
        return Ok(());
    };
    let pr_number = pr.number.to_string();
    let pr_url = pr.pr_url.as_deref().unwrap_or_default();
    let values = [
        ("{pr}", pr_number.as_str()),
        ("{repo}", repo),
        ("{head}", head),
        ("{branch}", branch),
        ("{base}", base),
        ("{url}", pr_url),
    ];
    let expand = |argument: &str| {
        values
            .iter()
            .fold(argument.to_owned(), |expanded, (key, value)| {
                expanded.replace(key, value)
            })
    };
    let program = expand(&hook.command[0]);
    let arguments = hook.command[1..]
        .iter()
        .map(|argument| expand(argument))
        .collect::<Vec<_>>();
    let output = Command::new(&program)
        .args(&arguments)
        .current_dir(cwd)
        .env("SHIPYARD_PR_NUMBER", &pr_number)
        .env("SHIPYARD_PR_REPO", repo)
        .env("SHIPYARD_PR_HEAD", head)
        .env("SHIPYARD_PR_BRANCH", branch)
        .env("SHIPYARD_PR_BASE", base)
        .env("SHIPYARD_PR_URL", pr_url)
        .output();
    match output {
        Ok(result) if result.status.success() => {
            writeln!(
                stdout,
                "▸ PR provenance hook completed for #{pr_number} at {}",
                head.chars().take(12).collect::<String>()
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            Ok(())
        }
        Ok(result) => {
            let diagnostic = if result.stderr.is_empty() {
                &result.stdout
            } else {
                &result.stderr
            };
            let detail = String::from_utf8_lossy(diagnostic).trim().to_owned();
            let message = format!(
                "PR provenance hook failed for #{pr_number} at {} (exit {}): {}",
                head.chars().take(12).collect::<String>(),
                result.status.code().unwrap_or(1),
                if detail.is_empty() {
                    "no diagnostic output"
                } else {
                    &detail
                }
            );
            if hook.required {
                Err(CliFailure::new(1, message))
            } else {
                writeln!(stdout, "⚠︎ {message}")
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
                Ok(())
            }
        }
        Err(error) => {
            let message = format!(
                "PR provenance hook failed to start for #{pr_number} at {}: {error}",
                head.chars().take(12).collect::<String>()
            );
            if hook.required {
                Err(CliFailure::new(1, message))
            } else {
                writeln!(stdout, "⚠︎ {message}")
                    .map_err(|write_error| CliFailure::new(1, write_error.to_string()))?;
                Ok(())
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_requested_steward_handoff<W: Write>(
    request: Option<&ShipStewardHandoff>,
    repo: &str,
    head: &str,
    pr: &ResolvedPrContext,
    config: &LoadedConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json_mode: bool,
    stdout: &mut W,
) -> Result<Option<AppliedStewardHandoff>, CliFailure> {
    let actions = GitHubActions::from_loaded_config(cwd, config);
    apply_requested_steward_handoff_with_actions(
        request,
        repo,
        head,
        pr,
        cwd,
        runtime_paths,
        &actions,
        json_mode,
        stdout,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_requested_steward_handoff_with_actions<W: Write>(
    request: Option<&ShipStewardHandoff>,
    repo: &str,
    head: &str,
    pr: &ResolvedPrContext,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    actions: &GitHubActions,
    json_mode: bool,
    stdout: &mut W,
) -> Result<Option<AppliedStewardHandoff>, CliFailure> {
    let Some(request) = request else {
        return Ok(None);
    };
    let workstream_id = request
        .workstream_id
        .clone()
        // Lowercase the slug: the steward's legacy-fallback hatch canonicalises
        // on a lowercase owner/name, so synthesizing with the origin's original
        // case (e.g. `Generous-Corp/pulp`) produced an id the hatch could never
        // match, and the handoff refused AFTER the branch was pushed.
        .unwrap_or_else(|| format!("{}#{}", repo.to_ascii_lowercase(), pr.number));
    let context_url = request.context_url.clone().or_else(|| pr.pr_url.clone());
    let mut sink = std::io::sink();
    let handoff_args = StewardHandoffArgs {
        repo: Some(repo.to_owned()),
        pr: pr.number,
        head: head.to_owned(),
        workstream_id: workstream_id.clone(),
        context_url: context_url.clone(),
        agent_provider: None,
        agent_session_id: None,
        agent_parent_session_id: None,
        agent_surface_id: None,
        launch_profile: request.launch_profile.clone(),
        task_graph: request.task_graph.clone(),
        goal_managed: request.launch_profile.is_some(),
        after_handoff: request.after_handoff.clone(),
        transfer_agent_owner: false,
        apply: true,
    };
    #[cfg(not(test))]
    steward_handoff_command(&handoff_args, cwd, runtime_paths, actions, false, &mut sink)?;
    #[cfg(test)]
    steward_handoff_command_without_ambient(
        &handoff_args,
        cwd,
        runtime_paths,
        actions,
        false,
        &mut sink,
    )?;
    let transfer = steward_handoff_transfer_report(runtime_paths, repo, pr.number, head)?;
    if !json_mode {
        writeln!(
            stdout,
            "▸ Durable steward receipt: PR #{} head={} workstream={workstream_id} monitoring_transferred={} agent_disposition={} pause_required={}",
            pr.number,
            head,
            transfer.wake_consumer_available,
            transfer.agent_disposition,
            transfer.pause_required
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(Some(AppliedStewardHandoff {
        workstream_id,
        context_url,
        head: head.to_owned(),
        monitoring_transferred: transfer.wake_consumer_available,
        agent_disposition: transfer.agent_disposition,
        pause_required: transfer.pause_required,
        publication_work_id: transfer.publication_work_id,
        publication_route_ref: transfer.publication_route_ref,
        publication_wake_id: transfer.publication_wake_id,
    }))
}
