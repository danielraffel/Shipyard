use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use serde_json::{Value, json};

use super::CliFailure;
use crate::config::LoadedConfig;
use crate::governance::GovernanceGh;
use crate::governance_onboard::{
    ALLOWED_PATHS, BoundedWaits, OnboardingReceipt, SweepRow, is_owned_repo, render_config,
    render_review_verdict_template, verdict_unproducible,
};
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;

/// Run `governance onboard`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn governance_onboard<W: Write>(
    repo: &str,
    profile: &str,
    require_checks: &[String],
    through: Option<&str>,
    no_enroll: bool,
    apply: bool,
    mode: RuntimeMode,
    cwd: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    validate_repo(repo)?;
    if !matches!(profile, "fleet" | "builder") {
        return Err(CliFailure::new(1, "profile must be fleet or builder"));
    }
    if no_enroll && through.is_some() {
        return Err(CliFailure::new(
            1,
            "--no-enroll cannot be combined with --through",
        ));
    }
    let through = through.unwrap_or(if no_enroll { "audited" } else { "enrolled" });
    let config = LoadedConfig::load_from_cwd(mode, cwd).ok();
    let gh = match config.as_ref() {
        Some(config) => GovernanceGh::from_loaded_config(cwd, config, None)
            .map_err(|error| CliFailure::new(9, error))?,
        None => GovernanceGh::ambient(cwd, None),
    };

    let repo_json = api_json(&gh, &format!("repos/{repo}"))?;
    let owned = is_owned_repo(repo);
    if !owned {
        return Err(CliFailure::new(
            1,
            format!("refusing non-owned repository {repo}"),
        ));
    }
    let default_branch = repo_json
        .get("default_branch")
        .and_then(Value::as_str)
        .unwrap_or("main");
    let pinned_ref = repo_json
        .get("default_branch")
        .and_then(Value::as_str)
        .unwrap_or(default_branch);
    let template = include_str!("../../.github/workflows/review-verdict.yml");
    let trigger = include_str!("../../.github/workflows/review-verdict-trigger.yml");
    let rendered_review = render_review_verdict_template(template, pinned_ref);
    let rendered_config = render_config(
        &["pulp-review".to_owned(), "infra-review2".to_owned()],
        &["fleet-pass-reviewer-*".to_owned()],
    );
    let hashes = rendered_hashes(&rendered_review, trigger, &rendered_config);

    let rulesets = api_json(&gh, &format!("repos/{repo}/rulesets?includes_parents=true"))
        .unwrap_or_else(|_| json!([]));
    let pulls = api_json(&gh, &format!("repos/{repo}/pulls?state=open&per_page=100"))
        .unwrap_or_else(|_| json!([]));
    let sweep = sweep_rows(pulls.as_array().map_or(&[], |value| value), &gh, repo);
    let workflow_present = true;
    let blocked_by_policy = false;
    let own_errors = false;
    let unproducible = verdict_unproducible(workflow_present, own_errors, blocked_by_policy);

    let receipt = OnboardingReceipt {
        repo: repo.to_owned(),
        rendered_hashes: hashes.clone(),
        sweep: sweep.clone(),
        enrolled: through == "enrolled",
        ..OnboardingReceipt::default()
    };
    let plan = json!({
        "command": "governance onboard",
        "repo": repo,
        "profile": profile,
        "require_checks": require_checks,
        "default_branch": default_branch,
        "through": through,
        "apply": apply,
        "owned": owned,
        "steps": [
            {"step": 0, "name": "preflight", "action": "read-only audit"},
            {"step": 1, "name": "pr", "action": "render and open unarmed onboarding PR", "paths": ALLOWED_PATHS, "hashes": hashes},
            {"step": 2, "name": "approved", "action": "bounded exact-head approval wait", "timeout_minutes": BoundedWaits::default().approval_minutes},
            {"step": 3, "name": "green", "action": "verify current required checks"},
            {"step": 4, "name": "merged", "action": "mergePullRequest(expectedHeadOid)", "unproducible_on_main": unproducible},
            {"step": 5, "name": "tightened", "action": "backup and add-only ruleset update", "rulesets_observed": rulesets},
            {"step": 6, "name": "evaluated", "action": "first review-verdict evaluation sweep", "wait_minutes": BoundedWaits::default().evaluation_minutes, "prs": sweep},
            {"step": 7, "name": "verified", "action": "bounded wait for a terminal review-verdict check run"},
            {"step": 8, "name": "handed_over", "action": "report UNSTABLE heads"},
            {"step": 9, "name": "audited", "action": "re-audit"},
            {"step": 10, "name": "enrolled", "action": "enroll in fleet-repos.txt", "enabled": !no_enroll},
        ],
        "receipt": receipt,
        "writes": if apply { "enabled" } else { "disabled (plan-only)" },
    });
    if apply {
        return Err(CliFailure::new(
            1,
            "governance onboard --apply requires the steward writer; no live write was attempted",
        ));
    }
    if json_mode {
        let data = plan
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        write_json_envelope(stdout, "governance.onboard", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        serde_json::to_writer_pretty(&mut *stdout, &plan)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout, "plan-only: no GitHub write was attempted")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

fn validate_repo(repo: &str) -> Result<(), CliFailure> {
    let valid = repo
        .split_once('/')
        .is_some_and(|(owner, name)| !owner.is_empty() && !name.is_empty() && !name.contains('/'));
    if valid {
        Ok(())
    } else {
        Err(CliFailure::new(1, "--repo must be OWNER/NAME"))
    }
}

fn api_json(gh: &GovernanceGh, path: &str) -> Result<Value, CliFailure> {
    let raw = gh
        .api_get(path, None)
        .map_err(|error| CliFailure::new(9, error))?
        .ok_or_else(|| CliFailure::new(9, format!("GitHub returned 404 for {path}")))?;
    serde_json::from_str(&raw).map_err(|error| CliFailure::new(9, format!("{path}: {error}")))
}

fn rendered_hashes(review: &str, trigger: &str, config: &str) -> BTreeMap<String, String> {
    use sha2::{Digest, Sha256};
    let digest = |bytes: &[u8]| -> String { format!("{:x}", Sha256::digest(bytes)) };
    BTreeMap::from([
        (ALLOWED_PATHS[0].to_owned(), digest(review.as_bytes())),
        (ALLOWED_PATHS[1].to_owned(), digest(trigger.as_bytes())),
        (ALLOWED_PATHS[2].to_owned(), digest(config.as_bytes())),
    ])
}

fn sweep_rows(pulls: &[Value], gh: &GovernanceGh, repo: &str) -> Vec<SweepRow> {
    pulls
        .iter()
        .filter(|pull| pull.get("draft").and_then(Value::as_bool) != Some(true))
        .filter_map(|pull| {
            let number = pull.get("number")?.as_u64()?;
            let head = pull.get("head")?.get("sha")?.as_str()?.to_owned();
            let path = format!("repos/{repo}/commits/{head}/check-runs?check_name=review-verdict");
            let existing = api_json(gh, &path).ok();
            let run = existing
                .as_ref()
                .and_then(|value| value.get("check_runs"))
                .and_then(Value::as_array)
                .and_then(|runs| runs.first());
            Some(SweepRow {
                number,
                head,
                trigger: if run.is_some() {
                    "skip-existing"
                } else {
                    "workflow_dispatch"
                }
                .to_owned(),
                check_run_id: run.and_then(|run| run.get("id")).and_then(Value::as_u64),
                conclusion: run
                    .and_then(|run| run.get("conclusion"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}
