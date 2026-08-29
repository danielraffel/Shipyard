use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use serde_json::Value;

use super::{
    CliFailure,
    branch_cmd::detect_repo_from_remote,
    ship_cmd::{ShipCommandArgs, ShipInvocation, ship_command},
};
use crate::config::LoadedConfig;
use crate::gate_scripts::{SKILL_SYNC, VERSION_BUMP, VERSIONING_CONFIG, resolve};
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::paths::RuntimePaths;

// A CLI argument bag: one bool per user-facing flag is the shape the
// command line already has, and grouping them into sub-structs would
// only move the flags further from the flags they mirror.
#[allow(clippy::struct_excessive_bools)]
pub(super) struct PrCommandArgs {
    pub(super) base: String,
    pub(super) apply_bumps: bool,
    pub(super) allow_unreachable_targets: bool,
    /// Proceed even when this host has not converged to the declared fleet epoch.
    pub(super) allow_fleet_epoch_drift: bool,
    pub(super) skip_targets: Vec<String>,
    pub(super) skip_bump: Vec<String>,
    pub(super) bump_reason: Option<String>,
    pub(super) skip_skill_update: Vec<String>,
    pub(super) skill_reason: Option<String>,
    /// Forwarded to `ship`: adopt the current head SHA on recorded-state drift
    /// (amend / force-push) instead of failing. See Shipyard #346.
    pub(super) adopt_head: bool,
    pub(super) workstream_id: Option<String>,
    pub(super) context_url: Option<String>,
    pub(super) launch_profile: Option<PathBuf>,
    pub(super) steward_handoff_preference: StewardHandoffPreference,
    pub(super) python_command: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StewardHandoffPreference {
    ProjectDefault,
    Disabled,
}

struct PrGates {
    skill_sync: PathBuf,
    version_bump: PathBuf,
    versioning_config: PathBuf,
}

/// Issue #301 (1/3): strip a leading `origin/` from `--base` so both
/// `--base main` and `--base origin/main` work. Without this, the two
/// `format!("origin/{base}")` sites below double-prefix the value into
/// `origin/origin/main`, which fails the skill-sync `git diff origin/
/// origin/main..HEAD` call with a Python traceback. Multi-remote setups
/// should still pass the bare branch name; arbitrary prefixes are NOT
/// stripped (we only special-case the `origin` remote because that's
/// what Shipyard appends internally).
fn normalize_base(input: &str) -> &str {
    input.strip_prefix("origin/").unwrap_or(input)
}

#[allow(clippy::too_many_lines)]
pub(super) fn pr_command<W: Write>(
    mut args: PrCommandArgs,
    config: &LoadedConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    // `clone_into(&mut args.base)` would alias-violate because the source
    // slice points INTO args.base; introduce a temporary to decouple.
    let normalized = normalize_base(&args.base).to_owned();
    args.base = normalized;

    if !args.skip_bump.is_empty() && args.bump_reason.is_none() {
        return Err(CliFailure::new(
            2,
            "--skip-bump requires --bump-reason \"...\".",
        ));
    }
    if !args.skip_skill_update.is_empty() && args.skill_reason.is_none() {
        return Err(CliFailure::new(
            2,
            "--skip-skill-update requires --skill-reason \"...\".",
        ));
    }
    if args.steward_handoff_preference == StewardHandoffPreference::Disabled
        && (args.workstream_id.is_some()
            || args.context_url.is_some()
            || args.launch_profile.is_some())
    {
        return Err(CliFailure::new(
            2,
            "--no-steward-handoff conflicts with --workstream-id/--context-url/--launch-profile.",
        ));
    }
    if args.launch_profile.is_some() && args.apply_bumps {
        return Err(CliFailure::new(
            2,
            "--launch-profile requires --no-apply-bumps so the private profile can bind the final exact head; apply and commit required bumps before generating the profile",
        ));
    }
    if args.launch_profile.is_some()
        && (!args.skip_bump.is_empty() || !args.skip_skill_update.is_empty())
    {
        return Err(CliFailure::new(
            2,
            "--launch-profile cannot amend shortcut trailers; commit all required trailers before generating the final exact-head profile",
        ));
    }

    let trailers = shortcut_trailers(&args);
    if !trailers.is_empty() {
        let added = append_trailers_to_tip(cwd, &trailers)
            .map_err(|error| CliFailure::new(2, error.to_string()))?;
        for trailer in added {
            writeln!(stdout, "▸ Added trailer: {trailer}")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }

    let repo_root = git_output(cwd, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .map_err(|error| CliFailure::new(2, error))?;
    let gates = resolve_pr_gates(&repo_root, config)?;
    let python = args
        .python_command
        .as_deref()
        .map_or_else(|| PathBuf::from("python3"), Path::to_path_buf);

    warn_missing_release_bot_token(stdout, cwd, config);
    run_skill_sync(stdout, &python, &gates, &repo_root, &args.base)?;
    let bumped_files = run_version_bump(stdout, &python, &gates, &repo_root, &args)?;
    if !bumped_files.is_empty() {
        writeln!(
            stdout,
            "▸ Committing version bump(s) — {} file(s)",
            bumped_files.len()
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        commit_bumped_files(&repo_root, &bumped_files)
            .map_err(|error| CliFailure::new(1, error))?;
    }

    let steward_handoff =
        resolve_steward_handoff(&args, protected_base_auto_handoff(cwd, &args.base));
    writeln!(stdout, "▸ Handing off to `shipyard ship`")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    ship_command(
        ShipCommandArgs {
            pr: None,
            base: args.base,
            auto_create_base: None,
            no_warm: false,
            resume_from: None,
            merge_command: None,
            merge_result: None,
            gh_command: None,
            pr_snapshot_file: None,
            allow_unreachable_targets: args.allow_unreachable_targets,
            allow_fleet_epoch_drift: args.allow_fleet_epoch_drift,
            skip_targets: args.skip_targets,
            adopt_head: args.adopt_head,
            steward_handoff,
            invocation: ShipInvocation::PrCommand,
            foreground: false,
        },
        config,
        cwd,
        runtime_paths,
        json_mode,
        stdout,
    )
}

fn resolve_steward_handoff(
    args: &PrCommandArgs,
    protected_default: bool,
) -> Option<super::ship_cmd::ShipStewardHandoff> {
    (args.steward_handoff_preference != StewardHandoffPreference::Disabled
        && (protected_default
            || args.workstream_id.is_some()
            || args.context_url.is_some()
            || args.launch_profile.is_some()))
    .then(|| super::ship_cmd::ShipStewardHandoff {
        workstream_id: args.workstream_id.clone(),
        context_url: args.context_url.clone(),
        launch_profile: args.launch_profile.clone(),
    })
}

fn protected_base_auto_handoff(cwd: &Path, base: &str) -> bool {
    // Security policy must come from the protected base, never the PR branch:
    // otherwise an untrusted contribution could opt its own head into mutation.
    let spec = format!("origin/{base}:.shipyard/config.toml");
    git_output(cwd, &["show", &spec])
        .ok()
        .and_then(|contents| contents.parse::<toml::Table>().ok())
        .and_then(|table| table.get("merge_steward")?.get("auto_handoff")?.as_bool())
        .unwrap_or(false)
}

fn resolve_pr_gates(repo_root: &Path, config: &LoadedConfig) -> Result<PrGates, CliFailure> {
    Ok(PrGates {
        skill_sync: resolve(SKILL_SYNC, repo_root, config)
            .map_err(|error| CliFailure::new(2, error.to_string()))?,
        version_bump: resolve(VERSION_BUMP, repo_root, config)
            .map_err(|error| CliFailure::new(2, error.to_string()))?,
        versioning_config: resolve(VERSIONING_CONFIG, repo_root, config)
            .map_err(|error| CliFailure::new(2, error.to_string()))?,
    })
}

fn run_skill_sync<W: Write>(
    stdout: &mut W,
    python: &Path,
    gates: &PrGates,
    repo_root: &Path,
    base: &str,
) -> Result<(), CliFailure> {
    writeln!(stdout, "▸ Skill-sync check")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let skill_status = Command::new(python)
        .arg(&gates.skill_sync)
        .args(["--base", &format!("origin/{base}")])
        .arg("--config")
        .arg(&gates.versioning_config)
        .arg("--mode=report")
        .current_dir(repo_root)
        .status()
        .map_err(|error| CliFailure::new(1, format!("failed to run skill-sync gate: {error}")))?;
    if !skill_status.success() {
        return Err(CliFailure::new(
            status_code(skill_status.code()),
            "skill-sync gate failed. Update the listed SKILL.md(s) or add a `Skill-Update: skip skill=<name> reason=\"...\"` trailer on the tip commit, then retry.",
        ));
    }
    Ok(())
}

fn run_version_bump<W: Write>(
    stdout: &mut W,
    python: &Path,
    gates: &PrGates,
    repo_root: &Path,
    args: &PrCommandArgs,
) -> Result<Vec<String>, CliFailure> {
    let bump_mode = if args.apply_bumps { "apply" } else { "report" };
    writeln!(stdout, "▸ Version-bump {bump_mode}")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let version_output = Command::new(python)
        .arg(&gates.version_bump)
        .args(["--base", &format!("origin/{}", args.base)])
        .arg("--config")
        .arg(&gates.versioning_config)
        .arg(format!("--mode={bump_mode}"))
        .current_dir(repo_root)
        .output()
        .map_err(|error| CliFailure::new(1, format!("failed to run version-bump gate: {error}")))?;
    if !version_output.stdout.is_empty() {
        stdout
            .write_all(&version_output.stdout)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    if !version_output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&version_output.stderr));
    }
    if !version_output.status.success() {
        return Err(CliFailure::new(
            status_code(version_output.status.code()),
            "version-bump gate failed; fix the bump and retry.",
        ));
    }
    Ok(parse_edited_files(&String::from_utf8_lossy(
        &version_output.stdout,
    )))
}

fn status_code(code: Option<i32>) -> u8 {
    code.and_then(|value| u8::try_from(value).ok())
        .filter(|value| *value != 0)
        .unwrap_or(1)
}

fn shortcut_trailers(args: &PrCommandArgs) -> Vec<String> {
    let mut trailers = Vec::new();
    if let Some(reason) = &args.bump_reason {
        for surface in &args.skip_bump {
            trailers.push(format!("Version-Bump: {surface}=skip reason=\"{reason}\""));
        }
    }
    if let Some(reason) = &args.skill_reason {
        for skill in &args.skip_skill_update {
            trailers.push(format!(
                "Skill-Update: skip skill={skill} reason=\"{reason}\""
            ));
        }
    }
    trailers
}

fn parse_edited_files(output: &str) -> Vec<String> {
    let mut in_block = false;
    let mut files = Vec::new();
    for line in output.lines() {
        if line.starts_with("Edited files:") {
            in_block = true;
            continue;
        }
        if in_block {
            if line.starts_with("  ") && !line.trim().is_empty() {
                files.push(line.trim().to_owned());
            } else {
                break;
            }
        }
    }
    files
}

#[derive(Debug)]
struct TrailerAmendError(String);

impl std::fmt::Display for TrailerAmendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for TrailerAmendError {}

fn append_trailers_to_tip(
    cwd: &Path,
    trailers: &[String],
) -> Result<Vec<String>, TrailerAmendError> {
    let index = crate::supervised::git_supervised()
        .args(["diff", "--cached", "--quiet"])
        .current_dir(cwd)
        .output()
        .map_err(|error| {
            TrailerAmendError(format!(
                "Couldn't probe git index - is this a git repo? {error}"
            ))
        })?;
    if !index.status.success() {
        return Err(TrailerAmendError(
            "Refusing to amend: staged changes would be folded into the tip commit. Commit, unstage (git reset), or stash them first, then re-run `shipyard pr` with the shortcut flags.".to_owned(),
        ));
    }

    let mut message = git_output(cwd, &["log", "-1", "--format=%B"]).map_err(|_| {
        TrailerAmendError(
            "Couldn't read tip commit message (is this a git repo with at least one commit?)."
                .to_owned(),
        )
    })?;
    let mut added = Vec::new();
    for trailer in trailers {
        if message.contains(trailer) {
            continue;
        }
        message = strip_conflicting_trailer(&message, trailer);
        message = interpret_trailer(cwd, &message, trailer)?;
        added.push(trailer.clone());
    }

    if added.is_empty() {
        return Ok(added);
    }

    let amend = crate::supervised::git_supervised()
        .args(["commit", "--amend", "--allow-empty", "-m", &message])
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| TrailerAmendError(format!("git commit --amend failed: {error}")))?;
    if !amend.status.success() {
        return Err(TrailerAmendError(
            "git commit --amend failed. Commit manually or re-run without the trailer shortcut flags."
                .to_owned(),
        ));
    }
    Ok(added)
}

fn strip_conflicting_trailer(message: &str, new_trailer: &str) -> String {
    let Some((key, payload)) = new_trailer.split_once(':') else {
        return message.to_owned();
    };
    let key = key.trim();
    let payload = payload.trim();
    let target = if key == "Version-Bump" {
        payload
            .split_once('=')
            .map(|(surface, _)| surface.trim().to_owned())
    } else if key == "Skill-Update" {
        payload.split_once("skill=").and_then(|(_, after)| {
            after
                .split_whitespace()
                .next()
                .map(|name| name.trim_end_matches(',').trim_end_matches(';').to_owned())
        })
    } else {
        None
    };
    let Some(target) = target.filter(|target| !target.is_empty()) else {
        return message.to_owned();
    };

    let mut kept = Vec::new();
    for line in message.lines() {
        if trailer_line_conflicts(line, key, &target) {
            continue;
        }
        kept.push(line);
    }
    let mut stripped = kept.join("\n");
    if message.ends_with('\n') && !stripped.ends_with('\n') {
        stripped.push('\n');
    }
    stripped
}

fn trailer_line_conflicts(line: &str, key: &str, target: &str) -> bool {
    let Some((line_key, line_payload)) = line.split_once(':') else {
        return false;
    };
    if line_key.trim() != key {
        return false;
    }
    let payload = line_payload.trim();
    if key == "Version-Bump" {
        return payload
            .split_once('=')
            .is_some_and(|(surface, _)| surface.trim() == target);
    }
    if key == "Skill-Update"
        && let Some((_, after)) = payload.split_once("skill=")
        && let Some(skill) = after.split_whitespace().next()
    {
        return skill.trim_end_matches(',').trim_end_matches(';') == target;
    }
    false
}

fn interpret_trailer(
    cwd: &Path,
    message: &str,
    trailer: &str,
) -> Result<String, TrailerAmendError> {
    let mut child = crate::supervised::git_supervised()
        .args([
            "interpret-trailers",
            "--if-exists",
            "addIfDifferent",
            "--trailer",
            trailer,
        ])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            TrailerAmendError(format!(
                "git interpret-trailers rejected trailer '{trailer}': {error}"
            ))
        })?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| TrailerAmendError("failed to open git stdin".to_owned()))?
        .write_all(message.as_bytes())
        .map_err(|error| TrailerAmendError(format!("failed to write trailer input: {error}")))?;
    let output = child.wait_with_output().map_err(|error| {
        TrailerAmendError(format!("git interpret-trailers failed to finish: {error}"))
    })?;
    if !output.status.success() {
        return Err(TrailerAmendError(format!(
            "git interpret-trailers rejected trailer '{trailer}'. Check the trailer format."
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn commit_bumped_files(repo_root: &Path, bumped_files: &[String]) -> Result<(), String> {
    let mut args = vec![
        "-c".to_owned(),
        "commit.gpgsign=false".to_owned(),
        "commit".to_owned(),
        "-m".to_owned(),
        "chore: bump versions".to_owned(),
        "--only".to_owned(),
        "--".to_owned(),
    ];
    args.extend(bumped_files.iter().cloned());
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = crate::supervised::git_supervised()
        .args(&refs)
        .current_dir(repo_root)
        .output()
        .map_err(|error| format!("failed to commit version bump(s): {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git commit for version bump(s) failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn warn_missing_release_bot_token<W: Write>(stdout: &mut W, cwd: &Path, config: &LoadedConfig) {
    let Some(repo) = detect_repo_from_remote(cwd, None) else {
        return;
    };
    let Ok(client) = GhClient::from_loaded_config(config) else {
        return;
    };
    let Ok(mut command) =
        client.prepare_command(cwd, None, GhSupervision::Supervised, GhAuthPolicy::Default)
    else {
        return;
    };
    let Ok(output) = command
        .args([
            "api",
            &format!("repos/{repo}/actions/secrets"),
            "--paginate",
        ])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let Ok(data) = serde_json::from_slice::<Value>(&output.stdout) else {
        return;
    };
    let has_token = data
        .get("secrets")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|secret| {
                secret.get("name").and_then(Value::as_str) == Some("RELEASE_BOT_TOKEN")
            })
        });
    if !has_token {
        let _ = writeln!(
            stdout,
            "▸ Heads-up: RELEASE_BOT_TOKEN secret is missing on this repo.\n         Auto-release will tag but the binary release workflow won't fire.\n         See `shipyard doctor` for the one-time setup steps."
        );
    }
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = crate::supervised::git_supervised()
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("failed to run git {}: {error}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs;

    use crate::config::{LoadedConfig, LocalOverlaySource};

    use super::*;

    #[test]
    fn normalize_base_strips_origin_prefix() {
        // Both forms a CLAUDE.md-following user would type must resolve
        // to the same internal base. See issue #301 (1/3).
        assert_eq!(normalize_base("main"), "main");
        assert_eq!(normalize_base("origin/main"), "main");
        assert_eq!(normalize_base("origin/develop/foo"), "develop/foo");
        // Other remotes are NOT special-cased — pass the bare name.
        assert_eq!(normalize_base("upstream/main"), "upstream/main");
        // Defensive: empty / weird shapes pass through unchanged.
        assert_eq!(normalize_base(""), "");
        assert_eq!(normalize_base("origin/"), "");
    }

    fn pr_args() -> PrCommandArgs {
        PrCommandArgs {
            base: String::from("main"),
            apply_bumps: true,
            allow_unreachable_targets: false,
            allow_fleet_epoch_drift: false,
            skip_targets: Vec::new(),
            skip_bump: Vec::new(),
            bump_reason: None,
            skip_skill_update: Vec::new(),
            skill_reason: None,
            adopt_head: false,
            workstream_id: None,
            context_url: None,
            launch_profile: None,
            steward_handoff_preference: StewardHandoffPreference::ProjectDefault,
            python_command: None,
        }
    }

    fn empty_config() -> LoadedConfig {
        LoadedConfig {
            data: toml::Table::new(),
            global_dir: PathBuf::from("/tmp/global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn config_from_toml(contents: &str, root: &Path) -> LoadedConfig {
        LoadedConfig {
            data: contents.parse::<toml::Table>().expect("config TOML"),
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    #[cfg(unix)]
    fn git(cwd: &Path, args: &[&str]) {
        let output = crate::supervised::git_supervised()
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git spawn");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn parses_edited_files_block_only() {
        let files = parse_edited_files(
            "checking\nEdited files:\n  pyproject.toml\n  crates/foo/Cargo.toml\n\n[next]\n",
        );

        assert_eq!(
            files,
            vec![
                String::from("pyproject.toml"),
                String::from("crates/foo/Cargo.toml")
            ]
        );
    }

    #[test]
    fn strips_conflicting_version_bump_trailer_only() {
        let message = "Title\n\nVersion-Bump: sdk=patch\nVersion-Bump: docs=skip reason=\"x\"\n";

        let stripped =
            strip_conflicting_trailer(message, "Version-Bump: sdk=skip reason=\"manual\"");

        assert!(!stripped.contains("Version-Bump: sdk=patch"));
        assert!(stripped.contains("Version-Bump: docs=skip"));
    }

    #[test]
    fn strips_conflicting_skill_trailer_on_token_boundary() {
        let message = "Title\n\nSkill-Update: skip skill=ci reason=\"x\"\nSkill-Update: skip skill=ci-tools reason=\"y\"\n";

        let stripped =
            strip_conflicting_trailer(message, "Skill-Update: skip skill=ci reason=\"new\"");

        assert!(!stripped.contains("skill=ci reason=\"x\""));
        assert!(stripped.contains("skill=ci-tools reason=\"y\""));
    }

    #[test]
    fn shortcut_trailers_require_reason_upstream_and_preserve_shape() {
        let args = PrCommandArgs {
            skip_bump: vec![String::from("sdk")],
            bump_reason: Some(String::from("manual bump later")),
            skip_skill_update: vec![String::from("ci")],
            skill_reason: Some(String::from("docs-only")),
            ..pr_args()
        };

        let trailers = shortcut_trailers(&args);

        assert_eq!(
            trailers,
            vec![
                String::from("Version-Bump: sdk=skip reason=\"manual bump later\""),
                String::from("Skill-Update: skip skill=ci reason=\"docs-only\"")
            ]
        );
    }

    #[test]
    fn steward_handoff_is_project_default_or_cli_opt_in_with_opt_out() {
        let automatic = resolve_steward_handoff(&pr_args(), true).expect("default");
        assert!(automatic.workstream_id.is_none());

        let explicit = PrCommandArgs {
            workstream_id: Some(String::from("GEN-7")),
            context_url: Some(String::from("https://linear.example/GEN-7")),
            ..pr_args()
        };
        let resolved = resolve_steward_handoff(&explicit, false).expect("explicit");
        assert_eq!(resolved.workstream_id.as_deref(), Some("GEN-7"));

        let profiled = PrCommandArgs {
            launch_profile: Some(PathBuf::from("/private/profile.json")),
            ..pr_args()
        };
        let resolved = resolve_steward_handoff(&profiled, false).expect("profile enables handoff");
        assert_eq!(
            resolved.launch_profile.as_deref(),
            Some(Path::new("/private/profile.json"))
        );

        let disabled = PrCommandArgs {
            steward_handoff_preference: StewardHandoffPreference::Disabled,
            ..pr_args()
        };
        assert!(resolve_steward_handoff(&disabled, true).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn auto_handoff_default_is_read_from_protected_origin_base() {
        let temp = tempfile::tempdir().expect("tempdir");
        let remote = temp.path().join("remote.git");
        let repo = temp.path().join("repo");
        git(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        std::fs::create_dir_all(repo.join(".shipyard")).expect("config dir");
        git(&repo, &["init", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        std::fs::write(
            repo.join(".shipyard/config.toml"),
            "[merge_steward]\nauto_handoff = true\n",
        )
        .expect("config");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&repo, &["push", "-u", "origin", "main"]);
        git(&repo, &["checkout", "-b", "feature"]);
        std::fs::write(
            repo.join(".shipyard/config.toml"),
            "[merge_steward]\nauto_handoff = false\n",
        )
        .expect("untrusted branch config");

        assert!(protected_base_auto_handoff(&repo, "main"));
    }

    #[cfg(unix)]
    #[test]
    fn untrusted_branch_cannot_enable_auto_handoff() {
        let temp = tempfile::tempdir().expect("tempdir");
        let remote = temp.path().join("remote.git");
        let repo = temp.path().join("repo");
        git(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        std::fs::create_dir_all(repo.join(".shipyard")).expect("config dir");
        git(&repo, &["init", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        std::fs::write(
            repo.join(".shipyard/config.toml"),
            "[merge_steward]\nauto_handoff = false\n",
        )
        .expect("config");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&repo, &["push", "-u", "origin", "main"]);
        git(&repo, &["checkout", "-b", "feature"]);
        std::fs::write(
            repo.join(".shipyard/config.toml"),
            "[merge_steward]\nauto_handoff = true\n",
        )
        .expect("untrusted branch config");

        assert!(!protected_base_auto_handoff(&repo, "main"));
    }

    #[cfg(unix)]
    #[test]
    fn resolves_pr_gates_from_default_tools_scripts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let scripts = temp.path().join("tools/scripts");
        fs::create_dir_all(&scripts).expect("scripts dir");
        fs::write(scripts.join("skill_sync_check.py"), "# skill\n").expect("skill script");
        fs::write(scripts.join("version_bump_check.py"), "# bump\n").expect("bump script");
        fs::write(scripts.join("versioning.json"), "{}\n").expect("versioning config");

        let gates = resolve_pr_gates(temp.path(), &empty_config()).expect("gates");

        assert_eq!(gates.skill_sync, scripts.join("skill_sync_check.py"));
        assert_eq!(gates.version_bump, scripts.join("version_bump_check.py"));
        assert_eq!(gates.versioning_config, scripts.join("versioning.json"));
    }

    #[cfg(unix)]
    #[test]
    fn run_skill_sync_invokes_gate_in_report_mode() {
        let temp = tempfile::tempdir().expect("tempdir");
        let log = temp.path().join("python.log");
        let skill_sync = temp.path().join("skill_sync_check.py");
        fs::write(
            &skill_sync,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$0 $*" > '{}'
exit 0
"#,
                log.display()
            ),
        )
        .expect("write skill-sync script");
        let gates = PrGates {
            skill_sync,
            version_bump: temp.path().join("version_bump_check.py"),
            versioning_config: temp.path().join("versioning.json"),
        };
        let mut stdout = Vec::new();

        run_skill_sync(
            &mut stdout,
            Path::new("/bin/sh"),
            &gates,
            temp.path(),
            "develop/next",
        )
        .expect("skill sync");

        let output = String::from_utf8(stdout).expect("stdout");
        assert!(output.contains("Skill-sync check"));
        let command_line = fs::read_to_string(log).expect("python log");
        assert!(command_line.contains("skill_sync_check.py --base origin/develop/next"));
        assert!(command_line.contains("--mode=report"));
    }

    #[cfg(unix)]
    #[test]
    fn run_version_bump_parses_apply_output_and_maps_failures() {
        let temp = tempfile::tempdir().expect("tempdir");
        let version_bump = temp.path().join("version_bump_check.py");
        fs::write(
            &version_bump,
            r#"#!/bin/sh
case "$*" in
  *"--mode=apply"*)
    printf '%s\n' 'Edited files:' '  Cargo.toml' '  crates/foo/Cargo.toml'
    exit 0
    ;;
  *)
    printf '%s\n' 'gate failed'
    exit 7
    ;;
esac
"#,
        )
        .expect("write version-bump script");
        let gates = PrGates {
            skill_sync: temp.path().join("skill_sync_check.py"),
            version_bump,
            versioning_config: temp.path().join("versioning.json"),
        };
        let mut stdout = Vec::new();

        let bumped = run_version_bump(
            &mut stdout,
            Path::new("/bin/sh"),
            &gates,
            temp.path(),
            &pr_args(),
        )
        .expect("bump");

        assert_eq!(
            bumped,
            vec![
                String::from("Cargo.toml"),
                String::from("crates/foo/Cargo.toml")
            ]
        );
        let mut report_args = pr_args();
        report_args.apply_bumps = false;
        let error = run_version_bump(
            &mut Vec::new(),
            Path::new("/bin/sh"),
            &gates,
            temp.path(),
            &report_args,
        )
        .expect_err("report mode failure");
        assert_eq!(error.code, 7);
        assert!(error.message.contains("version-bump gate failed"));
    }

    #[cfg(unix)]
    #[test]
    fn append_trailers_refuses_staged_changes() {
        let temp = tempfile::tempdir().expect("tempdir");
        git(temp.path(), &["init"]);
        git(temp.path(), &["config", "user.name", "test"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "commit.gpgsign", "false"]);
        fs::write(temp.path().join("file.txt"), "one\n").expect("write");
        git(temp.path(), &["add", "file.txt"]);
        git(temp.path(), &["commit", "-m", "initial"]);
        fs::write(temp.path().join("file.txt"), "two\n").expect("write");
        git(temp.path(), &["add", "file.txt"]);

        let error = append_trailers_to_tip(
            temp.path(),
            &[String::from("Version-Bump: sdk=skip reason=\"manual\"")],
        )
        .expect_err("staged change");

        assert!(error.to_string().contains("Refusing to amend"));
    }

    #[cfg(unix)]
    #[test]
    fn append_trailers_amends_tip_and_replaces_conflict() {
        let temp = tempfile::tempdir().expect("tempdir");
        git(temp.path(), &["init"]);
        git(temp.path(), &["config", "user.name", "test"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "commit.gpgsign", "false"]);
        fs::write(temp.path().join("file.txt"), "one\n").expect("write");
        git(temp.path(), &["add", "file.txt"]);
        git(
            temp.path(),
            &["commit", "-m", "initial", "-m", "Version-Bump: sdk=patch"],
        );

        let added = append_trailers_to_tip(
            temp.path(),
            &[String::from("Version-Bump: sdk=skip reason=\"manual\"")],
        )
        .expect("append");

        assert_eq!(
            added,
            vec![String::from("Version-Bump: sdk=skip reason=\"manual\"")]
        );
        let message = git_output(temp.path(), &["log", "-1", "--format=%B"]).expect("message");
        assert!(message.contains("Version-Bump: sdk=skip reason=\"manual\""));
        assert!(!message.contains("Version-Bump: sdk=patch"));
    }

    #[cfg(unix)]
    #[test]
    fn commit_bumped_files_commits_only_requested_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        git(temp.path(), &["init"]);
        git(temp.path(), &["config", "user.name", "test"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "commit.gpgsign", "false"]);
        fs::write(temp.path().join("Cargo.toml"), "one\n").expect("cargo");
        fs::write(temp.path().join("README.md"), "one\n").expect("readme");
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-m", "initial"]);
        fs::write(temp.path().join("Cargo.toml"), "two\n").expect("cargo update");
        fs::write(temp.path().join("README.md"), "two\n").expect("readme update");

        commit_bumped_files(temp.path(), &[String::from("Cargo.toml")]).expect("commit bump");

        let subject = git_output(temp.path(), &["log", "-1", "--format=%s"]).expect("subject");
        assert_eq!(subject, "chore: bump versions");
        let status = git_output(temp.path(), &["status", "--short"]).expect("status");
        assert_eq!(status, "M README.md");
    }

    #[test]
    fn missing_skip_reason_returns_exit_two() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut output = Vec::new();
        let args = PrCommandArgs {
            skip_bump: vec![String::from("sdk")],
            bump_reason: None,
            ..pr_args()
        };

        let error = pr_command(
            args,
            &empty_config(),
            temp.path(),
            &RuntimePaths::current(crate::identity::RuntimeMode::Isolated),
            false,
            &mut output,
        )
        .expect_err("missing reason");

        assert_eq!(error.code, 2);
        assert!(error.message.contains("--skip-bump requires"));
    }

    #[test]
    fn missing_skill_reason_returns_exit_two() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut output = Vec::new();
        let args = PrCommandArgs {
            skip_skill_update: vec![String::from("ci")],
            skill_reason: None,
            ..pr_args()
        };

        let error = pr_command(
            args,
            &config_from_toml("", temp.path()),
            temp.path(),
            &RuntimePaths::current(crate::identity::RuntimeMode::Isolated),
            false,
            &mut output,
        )
        .expect_err("missing skill reason");

        assert_eq!(error.code, 2);
        assert!(error.message.contains("--skip-skill-update requires"));
    }

    #[test]
    fn launch_profile_refuses_a_head_changing_bump_pass() {
        let temp = tempfile::tempdir().expect("tempdir");
        let args = PrCommandArgs {
            launch_profile: Some(temp.path().join("profile.json")),
            ..pr_args()
        };

        let error = pr_command(
            args,
            &empty_config(),
            temp.path(),
            &RuntimePaths::current(crate::identity::RuntimeMode::Isolated),
            false,
            &mut Vec::new(),
        )
        .expect_err("profile cannot precede automatic bumps");

        assert_eq!(error.code, 2);
        assert!(error.message.contains("--no-apply-bumps"));
        assert!(error.message.contains("final exact head"));

        let args = PrCommandArgs {
            apply_bumps: false,
            launch_profile: Some(temp.path().join("profile.json")),
            skip_bump: vec!["sdk".to_owned()],
            bump_reason: Some("already handled".to_owned()),
            ..pr_args()
        };
        let error = pr_command(
            args,
            &empty_config(),
            temp.path(),
            &RuntimePaths::current(crate::identity::RuntimeMode::Isolated),
            false,
            &mut Vec::new(),
        )
        .expect_err("profile cannot precede a trailer amend");
        assert!(error.message.contains("cannot amend shortcut trailers"));
    }
}
