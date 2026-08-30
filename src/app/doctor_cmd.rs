use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use serde_json::Value;

use crate::config::LoadedConfig;
use crate::doctor::{
    DoctorReport, SystemCommandProbe, check_github_auth_for_repo, check_release_chain_with_mode,
    collect_report_for_repo, collect_runner_checks, runner_config_error_checks,
};
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;

#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub(super) fn doctor<W: Write>(
    json: bool,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    repo: Option<&str>,
    release_chain: bool,
    runners: bool,
    rate_limit: bool,
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(repo) = repo {
        crate::gh::validate_repo_slug(repo)?;
    }
    let mut report = collect_report_for_repo(&SystemCommandProbe, mode, cwd, state_dir, repo);
    if release_chain && let Some(entry) = check_release_chain_with_mode(mode, cwd) {
        report
            .checks
            .entry("Release pipeline".to_owned())
            .or_default()
            .insert("release_chain".to_owned(), entry);
    }
    if runners {
        let runner_section = match LoadedConfig::load_from_cwd(mode, cwd) {
            Ok(config) => collect_runner_checks(&config),
            Err(error) => Some(runner_config_error_checks(error.to_string())),
        };
        if let Some(section) = runner_section {
            report.checks.insert("Runners".to_owned(), section);
        }
    }
    if rate_limit {
        let entries = collect_rate_limit_section(mode, cwd, repo);
        report
            .checks
            .insert("GitHub rate limits".to_owned(), entries);
    }
    if json {
        let mut data = BTreeMap::new();
        let value = serde_json::to_value(report)?;
        let Value::Object(map) = value else {
            return Err("doctor report must serialize as an object".into());
        };
        for (key, value) in map {
            data.insert(key, value);
        }
        write_json_envelope(stdout, "doctor", data)?;
        return Ok(());
    }

    write_human_report(stdout, report)?;
    Ok(())
}

fn write_human_report<W: Write>(stdout: &mut W, report: DoctorReport) -> std::io::Result<()> {
    writeln!(stdout, "shipyard doctor")?;
    writeln!(
        stdout,
        "{}",
        if report.ready {
            "ready: yes"
        } else {
            "ready: no"
        }
    )?;
    for (section_name, entries) in report.checks {
        writeln!(stdout, "{section_name}")?;
        for (name, entry) in entries {
            let status = if entry.ok { "ok" } else { "fail" };
            let summary = entry
                .version
                .as_deref()
                .or(entry.error.as_deref())
                .unwrap_or("");
            writeln!(stdout, "  {name}: {status} {summary}")?;
            if !entry.ok
                && let Some(detail) = entry.detail.as_deref()
                && detail != summary
            {
                for line in detail.lines().filter(|line| !line.trim().is_empty()) {
                    writeln!(stdout, "    {line}")?;
                }
            }
        }
    }
    Ok(())
}

fn collect_rate_limit_section(
    mode: RuntimeMode,
    cwd: &Path,
    repo: Option<&str>,
) -> BTreeMap<String, crate::doctor::DoctorEntry> {
    let mut entries: BTreeMap<String, crate::doctor::DoctorEntry> = BTreeMap::new();
    entries.insert(
        "auth".to_owned(),
        check_github_auth_for_repo(mode, cwd, repo),
    );

    let raw = (|| {
        let client = GhClient::from_cwd(mode, cwd).map_err(|error| error.to_string())?;
        let client = match repo {
            Some(repo) => client
                .with_repo_override(repo)
                .map_err(|error| error.to_string())?,
            None => client,
        };
        client
            .prepare_command(
                cwd,
                None,
                GhSupervision::Unsupervised,
                GhAuthPolicy::Default,
            )
            .map_err(|error| error.to_string())?
            .args(["api", "rate_limit"])
            .output()
            .map_err(|error| error.to_string())
    })();
    match raw {
        Ok(output) if output.status.success() => {
            let parsed: Result<Value, _> = serde_json::from_slice(&output.stdout);
            match parsed {
                Ok(value) => {
                    for bucket in ["core", "graphql"] {
                        let entry = rate_limit_entry(&value, bucket);
                        entries.insert(rate_limit_label(bucket), entry);
                    }
                }
                Err(error) => {
                    entries.insert(
                        "rate_limit".to_owned(),
                        crate::doctor::DoctorEntry {
                            ok: false,
                            version: None,
                            detail: None,
                            error: Some(format!("failed to parse `gh api rate_limit`: {error}")),
                        },
                    );
                }
            }
        }
        Ok(output) => {
            entries.insert(
                "rate_limit".to_owned(),
                crate::doctor::DoctorEntry {
                    ok: false,
                    version: None,
                    detail: None,
                    error: Some(format!(
                        "`gh api rate_limit` exited non-zero: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )),
                },
            );
        }
        Err(error) => {
            entries.insert(
                "rate_limit".to_owned(),
                crate::doctor::DoctorEntry {
                    ok: false,
                    version: None,
                    detail: None,
                    error: Some(format!("failed to invoke gh: {error}")),
                },
            );
        }
    }
    entries
}

fn rate_limit_label(bucket: &str) -> String {
    match bucket {
        "core" => "REST (core)".to_owned(),
        "graphql" => "GraphQL".to_owned(),
        other => other.to_owned(),
    }
}

fn rate_limit_entry(value: &Value, bucket: &str) -> crate::doctor::DoctorEntry {
    let Some(node) = value
        .get("resources")
        .and_then(|res| res.get(bucket))
        .and_then(Value::as_object)
    else {
        return crate::doctor::DoctorEntry {
            ok: false,
            version: None,
            detail: None,
            error: Some(format!("rate_limit response missing resources.{bucket}")),
        };
    };
    let remaining = node.get("remaining").and_then(Value::as_u64).unwrap_or(0);
    let limit = node.get("limit").and_then(Value::as_u64).unwrap_or(0);
    let reset_epoch = node.get("reset").and_then(Value::as_u64).unwrap_or(0);
    let reset_relative = relative_reset(reset_epoch);
    let summary = format!("{remaining}/{limit} remaining (resets in {reset_relative})");
    let degraded = limit > 0 && remaining < limit / 10;
    crate::doctor::DoctorEntry {
        ok: !degraded,
        version: Some(summary),
        detail: degraded.then(|| {
            "Bucket is < 10% of quota. Use the OTHER bucket if you have an option: `shipyard auto-merge` and `shipyard wait pr` accept REST fallbacks; `shipyard rescue` and `gh api` are REST-only.".to_owned()
        }),
        error: None,
    }
}

fn relative_reset(reset_epoch: u64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if reset_epoch <= now {
        return "now".to_owned();
    }
    let secs = reset_epoch - now;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::doctor::{DoctorEntry, DoctorReport};

    use super::{rate_limit_entry, relative_reset, write_human_report};
    use serde_json::json;

    #[test]
    fn human_doctor_renders_detail_for_failing_rows() {
        let mut checks = BTreeMap::new();
        let mut section = BTreeMap::new();
        section.insert(
            "rich-bundle".to_owned(),
            DoctorEntry {
                ok: false,
                version: None,
                detail: Some("Reinstall using install.sh.\nRun: curl ... | bash".to_owned()),
                error: Some("broken bundle".to_owned()),
            },
        );
        section.insert(
            "healthy".to_owned(),
            DoctorEntry {
                ok: true,
                version: Some("ok".to_owned()),
                detail: Some("hidden detail".to_owned()),
                error: None,
            },
        );
        checks.insert("Core".to_owned(), section);
        let mut output = Vec::new();

        write_human_report(
            &mut output,
            DoctorReport {
                ready: false,
                checks,
            },
        )
        .expect("render");

        let text = String::from_utf8(output).expect("utf8");
        assert!(text.contains("rich-bundle: fail broken bundle"));
        assert!(text.contains("    Reinstall using install.sh."));
        assert!(text.contains("    Run: curl ... | bash"));
        assert!(!text.contains("hidden detail"));
    }

    #[test]
    fn rate_limit_entry_renders_remaining_versus_limit() {
        let value = json!({
            "resources": {
                "core": {"remaining": 4826, "limit": 5000, "reset": 0},
                "graphql": {"remaining": 0, "limit": 5000, "reset": 0},
            }
        });
        let core = rate_limit_entry(&value, "core");
        let graphql = rate_limit_entry(&value, "graphql");
        assert!(core.ok);
        assert!(core.version.as_deref().unwrap().contains("4826/5000"));
        assert!(!graphql.ok);
        assert!(graphql.version.as_deref().unwrap().contains("0/5000"));
        // Detail should explain the asymmetry on the degraded bucket.
        assert!(graphql.detail.as_deref().unwrap().contains("OTHER bucket"));
    }

    #[test]
    fn rate_limit_entry_handles_missing_bucket() {
        let value = json!({"resources": {}});
        let entry = rate_limit_entry(&value, "core");
        assert!(!entry.ok);
        assert!(entry.error.as_deref().unwrap().contains("resources.core"));
    }

    #[test]
    fn relative_reset_returns_now_for_past_epoch() {
        assert_eq!(relative_reset(0), "now");
    }

    #[test]
    fn relative_reset_formats_seconds_minutes_hours() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        // future reset 45s out
        let s = relative_reset(now + 45);
        assert!(s.ends_with('s') && !s.contains('m'));
        // future reset 90s out → minutes+seconds
        let ms = relative_reset(now + 90);
        assert!(ms.contains('m'));
        // future reset 1h+ out → hours+minutes
        let hm = relative_reset(now + 3700);
        assert!(hm.contains('h'));
    }
}
