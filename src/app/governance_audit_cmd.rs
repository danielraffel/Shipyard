use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value as JsonValue, json};

use super::{CliFailure, branch_cmd::detect_repo_from_remote};
use crate::config::LoadedConfig;
use crate::governance::GovernanceGh;
use crate::governance_audit::{
    AuditPolicy, AuditReport, AuditStatus, UpdatesBot, audit_snapshot, fetch_snapshot,
};
use crate::output::write_json_envelope;

/// Flags for `shipyard governance audit`.
pub(super) struct AuditRequest {
    pub(super) repos: Vec<String>,
    pub(super) gh_binary: Option<PathBuf>,
    pub(super) profile: Option<String>,
    pub(super) updates_bot: Option<String>,
}

pub(super) fn governance_audit<W: Write>(
    request: &AuditRequest,
    config: Option<&LoadedConfig>,
    cwd: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let policy = resolve_policy(request, config).map_err(|error| CliFailure::new(1, error))?;
    let repos = if request.repos.is_empty() {
        vec![detect_repo_from_remote(cwd, None).ok_or_else(|| {
            CliFailure::new(
                1,
                "No --repo given and no repo detected from the git remote.",
            )
        })?]
    } else {
        request.repos.clone()
    };
    let gh = match (&request.gh_binary, config) {
        (Some(binary), _) => GovernanceGh::ambient(cwd, Some(binary)),
        (None, Some(config)) => GovernanceGh::from_loaded_config(cwd, config, None)
            .map_err(|error| CliFailure::new(1, error))?,
        (None, None) => GovernanceGh::ambient(cwd, None),
    };
    let reports: Vec<AuditReport> = repos
        .iter()
        .map(|repo| audit_snapshot(&fetch_snapshot(repo, &gh), &policy))
        .collect();
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("policy".to_owned(), policy_json(&policy));
        data.insert(
            "repos".to_owned(),
            JsonValue::Array(reports.iter().map(report_json).collect()),
        );
        write_json_envelope(stdout, "governance.audit", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        render_text(stdout, &policy, &reports)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(if reports.iter().any(AuditReport::has_drift) {
        ExitCode::from(1)
    } else if reports.iter().any(AuditReport::has_unknown) {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}

fn resolve_policy(
    request: &AuditRequest,
    config: Option<&LoadedConfig>,
) -> Result<AuditPolicy, String> {
    let mut policy = match config {
        Some(config) => AuditPolicy::from_config(&config.data)?,
        None => AuditPolicy::default(),
    };
    if let Some(bot) = &request.updates_bot {
        policy.updates_bot = UpdatesBot::parse(bot)?;
    }
    if let Some(profile) = &request.profile {
        if !matches!(profile.as_str(), "solo" | "multi" | "custom") {
            return Err(format!(
                "Unknown governance profile '{profile}'. Expected one of: solo, multi, custom"
            ));
        }
        profile.clone_into(&mut policy.profile);
    }
    Ok(policy)
}

fn policy_json(policy: &AuditPolicy) -> JsonValue {
    json!({
        "profile": policy.profile,
        "updates_bot": policy.updates_bot.as_str(),
        "cooldown_days": policy.cooldown_days,
        "open_pull_requests_limit": policy.open_pull_requests_limit,
        "lockfile": policy.lockfile_mode.as_str(),
        "lockfile_context": policy.lockfile_context,
    })
}

fn report_json(report: &AuditReport) -> JsonValue {
    json!({
        "repo": report.repo,
        "has_drift": report.has_drift(),
        "has_unknown": report.has_unknown(),
        "rows": report.rows.iter().map(|row| json!({
            "row": row.setting.key(),
            "status": row.status.as_str(),
            "detail": row.detail,
        })).collect::<Vec<_>>(),
    })
}

fn count(report: &AuditReport, status: AuditStatus) -> usize {
    report
        .rows
        .iter()
        .filter(|row| row.status == status)
        .count()
}

fn render_text<W: Write>(
    stdout: &mut W,
    policy: &AuditPolicy,
    reports: &[AuditReport],
) -> std::io::Result<()> {
    writeln!(
        stdout,
        "governance audit (read-only): profile {}, updates {} weekly limit {} cooldown {}d, lockfile {}",
        policy.profile,
        policy.updates_bot.as_str(),
        policy.open_pull_requests_limit,
        policy.cooldown_days,
        policy.lockfile_mode.as_str(),
    )?;
    for report in reports {
        writeln!(
            stdout,
            "\n{}: {} PASS, {} DRIFT, {} UNKNOWN",
            report.repo,
            count(report, AuditStatus::Pass),
            count(report, AuditStatus::Drift),
            count(report, AuditStatus::Unknown),
        )?;
        for row in &report.rows {
            writeln!(
                stdout,
                "  {:<7} {:<20} {}",
                row.status.as_str(),
                row.setting.key(),
                row.detail
            )?;
        }
    }
    let drift = reports.iter().filter(|report| report.has_drift()).count();
    writeln!(
        stdout,
        "\n{drift} of {} repo(s) drift. A row is a target until it prints PASS. Nothing was written.",
        reports.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governance_audit::{AuditRow, Setting};

    fn request(bot: Option<&str>, profile: Option<&str>) -> AuditRequest {
        AuditRequest {
            repos: Vec::new(),
            gh_binary: None,
            profile: profile.map(str::to_owned),
            updates_bot: bot.map(str::to_owned),
        }
    }

    #[test]
    fn flags_override_policy() {
        let policy =
            resolve_policy(&request(Some("renovate"), Some("multi")), None).expect("policy");
        assert_eq!(policy.updates_bot, UpdatesBot::Renovate);
        assert_eq!(policy.cooldown_days, 7);
        assert_eq!(policy.profile, "multi");
        assert!(resolve_policy(&request(Some("snyk"), None), None).is_err());
        assert!(resolve_policy(&request(None, Some("team")), None).is_err());
        assert_eq!(
            resolve_policy(&request(None, None), None).expect("default"),
            AuditPolicy::default()
        );
    }

    #[test]
    fn text_and_json_name_each_row_and_status() {
        let report = AuditReport {
            repo: "o/r".to_owned(),
            rows: Setting::ALL
                .iter()
                .map(|&setting| AuditRow {
                    setting,
                    status: if setting == Setting::ZizmorReport {
                        AuditStatus::Drift
                    } else {
                        AuditStatus::Pass
                    },
                    detail: "evidence".to_owned(),
                })
                .collect(),
        };
        let mut out = Vec::new();
        render_text(
            &mut out,
            &AuditPolicy::default(),
            std::slice::from_ref(&report),
        )
        .expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("updates dependabot weekly limit 1 cooldown 7d, lockfile advisory"));
        assert!(text.contains("o/r: 10 PASS, 1 DRIFT, 0 UNKNOWN"));
        assert!(text.contains("  DRIFT   zizmor-report        evidence"));
        assert!(text.contains("1 of 1 repo(s) drift"));
        let json = report_json(&report);
        assert_eq!(json["has_drift"], true);
        assert_eq!(json["rows"][3]["row"], "zizmor-report");
        assert_eq!(json["rows"][3]["status"], "DRIFT");
    }
}
