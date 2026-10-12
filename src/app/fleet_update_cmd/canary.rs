//! Canary smoke for a fleet rollout.
//!
//! The first host a rollout updates is the canary. After it verifies, the
//! newly installed binary on that host runs the read-only form of the command
//! the fleet's CI depends on most: `runner admission-clean` (no `--apply`),
//! once per repository the host serves, with the host's own runner labels.
//! Version and daemon checks cannot see a release that installs cleanly and
//! then fails every GitHub observation it makes: v0.302.0 to v0.307.0 did
//! exactly that, and the rollout put it on every host. A smoke that cannot
//! observe stops the rollout on the canary, rolls the canary back, and names
//! the failure, before any other host is touched.

use std::fmt::Write as _;
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use super::HostUpdatePlan;
use crate::executor::ssh::shlex_quote;
use crate::paths::{home_dir, unattended_tool_path};

/// Upper bound for the whole canary smoke on one host. Each admission call
/// carries its own 120-second observation budget.
pub(super) const CANARY_SMOKE_TIMEOUT: Duration = Duration::from_mins(6);

const RESULT_PREFIX: &str = "SHIPYARD_CANARY_SMOKE=";
const STDERR_PREFIX: &str = "SHIPYARD_CANARY_SMOKE_STDERR=";
/// The first release with `runner admission-clean`. A target before it cannot
/// be smoked this way and is reported skipped; that is decided from the tag,
/// never from a probe whose failure could have any cause.
pub(super) const MIN_ADMISSION_CLEAN_TARGET: [u64; 3] = [0, 94, 0];

/// Defer reasons returned before any GitHub read: they prove nothing.
const UNOBSERVED_DEFER_REASONS: [&str; 2] = ["observation_in_progress", "stewardship_in_progress"];

/// What the canary smoke runs against.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct SmokeSpec {
    pub(super) repos: Vec<String>,
    pub(super) base: String,
    pub(super) labels: Vec<String>,
}

/// One repository's smoke result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct SmokeProbe {
    pub(super) repo: String,
    pub(super) exit_code: Option<i32>,
    pub(super) verdict: Option<String>,
    pub(super) reason: Option<String>,
    pub(super) error: Option<String>,
}

/// The canary smoke outcome for one host.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(super) enum SmokeOutcome {
    /// Every repository was observed (`admit` or `defer`).
    Passed { probes: Vec<SmokeProbe> },
    /// At least one repository could not be observed, or the smoke itself
    /// could not run. The rollout stops.
    Failed {
        probes: Vec<SmokeProbe>,
        reason: String,
    },
    /// Nothing to smoke against (no repository or no runner label). Reported,
    /// never silently treated as a pass.
    Skipped { reason: String },
}

impl SmokeOutcome {
    pub(super) fn failed(&self) -> Option<&str> {
        match self {
            Self::Failed { reason, .. } => Some(reason),
            _ => None,
        }
    }

    pub(super) fn summary(&self) -> String {
        match self {
            Self::Passed { probes } => format!(
                "canary smoke passed: {}",
                probes
                    .iter()
                    .map(|probe| format!(
                        "{} {}",
                        probe.repo,
                        probe.verdict.as_deref().unwrap_or("?")
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Failed { reason, .. } => format!("canary smoke FAILED: {reason}"),
            Self::Skipped { reason } => format!("canary smoke SKIPPED: {reason}"),
        }
    }
}

/// The repositories and labels to smoke on one host.
///
/// `[fleet_update] smoke_repos` (and `smoke_base`, default `main`) in the
/// machine-global config override the repositories; otherwise the host's own
/// daemon-configured repositories, read from the update evidence, are used.
pub(super) fn smoke_spec(
    config: &toml::Table,
    host_labels: &[String],
    daemon_repos: &[String],
) -> Result<SmokeSpec, String> {
    let section = config.get("fleet_update").and_then(toml::Value::as_table);
    let configured = match section.and_then(|table| table.get("smoke_repos")) {
        None => None,
        Some(value) => Some(
            value
                .as_array()
                .ok_or_else(|| "fleet_update.smoke_repos must be an array of strings".to_owned())?
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_owned).ok_or_else(|| {
                        "fleet_update.smoke_repos must be an array of strings".to_owned()
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
    };
    let base = match section.and_then(|table| table.get("smoke_base")) {
        None => "main".to_owned(),
        Some(value) => value
            .as_str()
            .filter(|base| !base.is_empty())
            .ok_or_else(|| "fleet_update.smoke_base must be a non-empty string".to_owned())?
            .to_owned(),
    };
    let mut repos = configured.unwrap_or_else(|| daemon_repos.to_vec());
    repos.sort();
    repos.dedup();
    Ok(SmokeSpec {
        repos,
        base,
        labels: host_labels.to_vec(),
    })
}

/// The script the canary host runs: one read-only admission call per repo.
pub(super) fn smoke_script(plan: &HostUpdatePlan, spec: &SmokeSpec) -> String {
    let binary = shlex_quote(&plan.binary.display().to_string());
    // The probe uses a throwaway state directory. The production one holds the
    // per-(repo, base, labels) observation lock every TartCI admission takes,
    // and a contended lock defers before any GitHub read -- which would let a
    // binary that cannot observe GitHub pass on a busy host. The global dir is
    // kept so the probe authenticates exactly as the host does.
    let mut script = String::from(
        "set -u\nsmoke_state=\"$(/usr/bin/mktemp -d)\"\ntrap '/bin/rm -rf \"$smoke_state\"' EXIT\n",
    );
    for repo in &spec.repos {
        let _ = write!(
            script,
            "errf=\"$(/usr/bin/mktemp)\"\n\
             out=\"$({binary} --mode {mode} --global-dir {global} --state-dir \"$smoke_state\" --json runner admission-clean --repo {repo} --base {base} --labels {labels} 2>\"$errf\")\"; code=$?\n\
             err=\"$(/usr/bin/head -c 2000 \"$errf\" | /usr/bin/tr '\\n' ' ')\"; /bin/rm -f \"$errf\"\n\
             printf '%s%s|%s|%s\\n' {prefix} {repo} \"$code\" \"$(printf '%s' \"$out\" | /usr/bin/tr -d '\\n')\"\n\
             printf '%s%s|%s\\n' {stderr_prefix} {repo} \"$err\"\n",
            mode = shlex_quote(plan.runtime_mode.as_str()),
            global = shlex_quote(&plan.global_dir.display().to_string()),
            repo = shlex_quote(repo),
            base = shlex_quote(&spec.base),
            labels = shlex_quote(&spec.labels.join(",")),
            prefix = shlex_quote(RESULT_PREFIX),
            stderr_prefix = shlex_quote(STDERR_PREFIX),
        );
    }
    script
}

/// Judge a smoke transcript. Pure, so every branch is testable.
///
/// `admit` (exit 0) proves the binary observed GitHub, and so does `defer`
/// (exit 3) unless its reason is one returned before any read (a contended
/// lock). The verdict must also name the requested repository. Anything else,
/// including a missing line, is a failure.
pub(super) fn judge(spec: &SmokeSpec, stdout: &str) -> SmokeOutcome {
    let mut probes = Vec::new();
    let mut failures = Vec::new();
    for repo in &spec.repos {
        let lines = stdout
            .lines()
            .filter_map(|line| line.strip_prefix(RESULT_PREFIX))
            .filter_map(|rest| {
                let (name, rest) = rest.split_once('|')?;
                (name == repo).then_some(rest)
            })
            .collect::<Vec<_>>();
        let [line] = lines.as_slice() else {
            failures.push(format!("{repo}: no single smoke result"));
            probes.push(SmokeProbe {
                repo: repo.clone(),
                exit_code: None,
                verdict: None,
                reason: None,
                error: Some("no single smoke result".to_owned()),
            });
            continue;
        };
        let (code, output) = line.split_once('|').unwrap_or((line, ""));
        let exit_code = code.trim().parse::<i32>().ok();
        let document = serde_json::from_str::<Value>(output.trim()).ok();
        let stderr = stdout
            .lines()
            .filter_map(|line| line.strip_prefix(STDERR_PREFIX))
            .filter_map(|rest| rest.split_once('|'))
            .find(|(name, _)| name == repo)
            .map(|(_, text)| text.trim().to_owned())
            .filter(|text| !text.is_empty());
        let field = |name: &str| {
            document
                .as_ref()
                .and_then(|value| value.get(name))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let verdict = field("verdict");
        let probe = SmokeProbe {
            repo: repo.clone(),
            exit_code,
            verdict: verdict.clone(),
            reason: field("reason"),
            error: field("error").or(stderr),
        };
        let named_repo = field("repo").is_some_and(|named| named.eq_ignore_ascii_case(repo));
        let observed = named_repo
            && match (exit_code, verdict.as_deref()) {
                (Some(0), Some("admit")) => true,
                (Some(3), Some("defer")) => probe
                    .reason
                    .as_deref()
                    .is_some_and(|reason| !UNOBSERVED_DEFER_REASONS.contains(&reason)),
                _ => false,
            };
        if !observed {
            failures.push(format!(
                "{repo}: exit {} verdict {} reason {} repo {}{}",
                exit_code.map_or_else(|| "unknown".to_owned(), |code| code.to_string()),
                probe.verdict.as_deref().unwrap_or("none"),
                probe.reason.as_deref().unwrap_or("none"),
                field("repo").as_deref().unwrap_or("none"),
                probe
                    .error
                    .as_deref()
                    .map(|error| format!(": {}", error.chars().take(400).collect::<String>()))
                    .unwrap_or_default(),
            ));
        }
        probes.push(probe);
    }
    if failures.is_empty() {
        SmokeOutcome::Passed { probes }
    } else {
        SmokeOutcome::Failed {
            probes,
            reason: failures.join("; "),
        }
    }
}

/// Run the canary smoke on the plan's host.
pub(super) fn run_smoke(plan: &HostUpdatePlan, spec: &SmokeSpec) -> SmokeOutcome {
    if !super::tag_at_least(&plan.target, MIN_ADMISSION_CLEAN_TARGET) {
        return SmokeOutcome::Skipped {
            reason: format!("{} predates `runner admission-clean`", plan.target),
        };
    }
    if spec.repos.is_empty() {
        return SmokeOutcome::Skipped {
            reason: format!(
                "{} serves no repository and [fleet_update] smoke_repos is unset",
                plan.class
            ),
        };
    }
    if spec.labels.is_empty() {
        return SmokeOutcome::Skipped {
            reason: format!("host_class.{} declares no runner labels", plan.class),
        };
    }
    let deadline = Instant::now() + CANARY_SMOKE_TIMEOUT;
    let script = smoke_script(plan, spec);
    let mut command = if let Some(host) = &plan.ssh {
        let mut command = Command::new(super::evidence::ssh_binary_path());
        command.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=8",
            "-o",
            "StrictHostKeyChecking=yes",
        ]);
        command.arg(host).arg(&script);
        command
    } else {
        let mut command = Command::new("/bin/bash");
        command
            .args(["-c", &script])
            .env_clear()
            .env("HOME", home_dir())
            .env("PATH", unattended_tool_path());
        command
    };
    let label = format!("canary smoke for host class {}", plan.class);
    match crate::process::run_output_until(&mut command, deadline, &label) {
        Ok(output) if output.status.success() => {
            judge(spec, &String::from_utf8_lossy(&output.stdout))
        }
        Ok(output) => SmokeOutcome::Failed {
            probes: Vec::new(),
            reason: format!(
                "smoke process exited {}",
                output.status.code().unwrap_or(-1)
            ),
        },
        Err(error) => SmokeOutcome::Failed {
            probes: Vec::new(),
            reason: error.to_string(),
        },
    }
}
