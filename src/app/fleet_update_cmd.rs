//! Governed exact-version rollout for configured Shipyard host classes.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::Value;

mod auth_support;
mod command;
mod evidence;
mod release_authority;

#[cfg(all(test, unix))]
use command::exact_asset_curl_shim;
#[cfg(all(test, target_os = "macos"))]
use command::remote_pair_probe;
use command::{local_update_command, remote_update_command, render_host_result, render_plan};

#[cfg(all(test, unix))]
use evidence::execute_plan_with_timeout;
#[cfg(test)]
use evidence::{
    AuthSupportEvidence, BinaryEvidence, BinaryPairEvidence, SourceIdentityBasis,
    SupportFileEvidence,
};
use evidence::{HostUpdateEvidence, PlanExecutionError, execute_plan, validate_evidence};
use release_authority::{
    GitHubReleaseAuthorityVerifier, ReleaseAuthority, ReleaseAuthorityVerifier,
};

#[cfg(test)]
mod tests;

use super::CliFailure;
use crate::capacity::{HostClassConfig, parse_host_classes};
use crate::config::LoadedConfig;
use crate::executor::ssh::shlex_quote;
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::paths::{RuntimePaths, home_dir, unattended_tool_path};

const REMOTE_MINIMAL_PATH: &str =
    "/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
const HOST_UPDATE_TIMEOUT: Duration = Duration::from_mins(10);
const REMOTE_UPDATE_TIMEOUT: Duration = Duration::from_mins(9);
const MIN_FLEET_UPDATE_TARGET: [u64; 3] = [0, 100, 0];
const MIN_PAIRED_BINARY_TARGET: [u64; 3] = [0, 127, 0];
const COMPANION_BINARY_NAME: &str = "shipyard-workstream-provider";
const REMOTE_BEFORE_PRIMARY_SHA256_PREFIX: &str = "SHIPYARD_FLEET_BEFORE_PRIMARY_SHA256=";
const REMOTE_BEFORE_PRIMARY_VERSION_PREFIX: &str = "SHIPYARD_FLEET_BEFORE_PRIMARY_VERSION=";
const REMOTE_BEFORE_COMPANION_SHA256_PREFIX: &str = "SHIPYARD_FLEET_BEFORE_COMPANION_SHA256=";
const REMOTE_BEFORE_COMPANION_VERSION_PREFIX: &str = "SHIPYARD_FLEET_BEFORE_COMPANION_VERSION=";
const REMOTE_AFTER_PRIMARY_SHA256_PREFIX: &str = "SHIPYARD_FLEET_AFTER_PRIMARY_SHA256=";
const REMOTE_AFTER_PRIMARY_VERSION_PREFIX: &str = "SHIPYARD_FLEET_AFTER_PRIMARY_VERSION=";
const REMOTE_AFTER_COMPANION_SHA256_PREFIX: &str = "SHIPYARD_FLEET_AFTER_COMPANION_SHA256=";
const REMOTE_AFTER_COMPANION_VERSION_PREFIX: &str = "SHIPYARD_FLEET_AFTER_COMPANION_VERSION=";
const REMOTE_BEFORE_STATUS_PREFIX: &str = "SHIPYARD_FLEET_BEFORE_STATUS=";
const REMOTE_REFRESH_PREFIX: &str = "SHIPYARD_FLEET_REFRESH=";
const REMOTE_AFTER_STATUS_PREFIX: &str = "SHIPYARD_FLEET_AFTER_STATUS=";
const REMOTE_AUTHORITY_ID_PREFIX: &str = "SHIPYARD_FLEET_AUTHORITY_ID=";
const REMOTE_RELEASE_ASSET_SHA256_PREFIX: &str = "SHIPYARD_FLEET_RELEASE_ASSET_SHA256=";
const REMOTE_SUPERVISOR: &str = r#"use strict;
use warnings;
use POSIX qw(WNOHANG setsid);
my $seconds = shift @ARGV;
my $pid = fork();
die "fork failed: $!" unless defined $pid;
if ($pid == 0) {
    setsid() >= 0 or die "setsid failed: $!";
    exec @ARGV;
    die "exec failed: $!";
}
local $SIG{ALRM} = sub {
    kill 'TERM', -$pid;
    my $leader_reaped = 0;
    for (1..50) {
        my $done = waitpid($pid, WNOHANG);
        if ($done == $pid) {
            $leader_reaped = 1;
            last;
        }
        select undef, undef, undef, 0.1;
    }
    # The leader may exit on TERM while an installer/download descendant in
    # the same session ignores it. Always close the whole group with KILL.
    kill 'KILL', -$pid;
    waitpid($pid, 0) unless $leader_reaped;
    exit 124;
};
alarm $seconds;
waitpid($pid, 0);
alarm 0;
my $status = $?;
exit(($status & 127) ? 128 + ($status & 127) : $status >> 8);
"#;

pub(super) struct FleetUpdateArgs {
    pub(super) to: String,
    pub(super) host_classes: Vec<String>,
    pub(super) all_hosts: bool,
    pub(super) apply: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HostUpdatePlan {
    class: String,
    ssh: Option<String>,
    binary: PathBuf,
    companion_binary: PathBuf,
    auth_helper: PathBuf,
    auth_wrapper: PathBuf,
    target: String,
    source_identity: String,
    release_authority: ReleaseAuthority,
    companion_required: bool,
    command: String,
    runtime_mode: RuntimeMode,
    global_dir: PathBuf,
    state_dir: PathBuf,
}

pub(super) fn fleet_update_command<W: Write>(
    args: &FleetUpdateArgs,
    _mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if cfg!(not(unix)) {
        return Err(CliFailure::new(
            1,
            "fleet-update requires a Unix rollout controller",
        ));
    }
    let target = normalize_exact_tag(&args.to)?;
    // Fleet mutation topology is machine policy. Never let a repository's
    // tracked overlay select SSH destinations or executable paths.
    let config = LoadedConfig::load_machine_global_from_dir(runtime_paths.global_dir.clone())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let classes = parse_host_classes(&config.data).map_err(|error| CliFailure::new(2, error))?;
    if classes.is_empty() {
        return Err(CliFailure::new(
            1,
            "No [host_class.<name>] configured — fleet-update has no rollout targets.",
        ));
    }
    let selected_classes = select_host_classes(&classes, &args.host_classes, args.all_hosts)?;
    // Eligibility is established once, before the first host can mutate. The
    // verifier independently binds live GitHub tag/release objects, downloaded
    // asset bytes, the checksum manifest, and signed build provenance.
    let release_authority = GitHubReleaseAuthorityVerifier::new(&config, cwd)
        .verify(&target)
        .map_err(|error| CliFailure::new(1, format!("fleet release is ineligible: {error}")))?;
    let plans = selected_classes
        .iter()
        .map(|class| host_update_plan_with_authority(class, &target, &release_authority))
        .collect::<Result<Vec<_>, _>>()?;

    if !args.apply {
        render_plan(stdout, json, &target, &plans, args.all_hosts)?;
        return Ok(ExitCode::SUCCESS);
    }

    apply_plans(&plans, &target, json, stdout, execute_plan)
}

#[cfg(all(test, not(unix)))]
mod non_unix_tests {
    use super::*;

    #[test]
    fn fleet_update_refuses_before_loading_or_mutating_machine_state() {
        let temp = tempfile::tempdir().expect("temp");
        let global_dir = temp.path().join("global");
        let state_dir = temp.path().join("state");
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Shipyard,
            Some(global_dir.clone()),
            Some(state_dir.clone()),
        );
        let args = FleetUpdateArgs {
            to: "v0.127.4".to_owned(),
            host_classes: vec!["m1".to_owned()],
            all_hosts: false,
            apply: true,
        };
        let mut output = Vec::new();
        let error = fleet_update_command(
            &args,
            RuntimeMode::Shipyard,
            temp.path(),
            &paths,
            true,
            &mut output,
        )
        .expect_err("non-Unix fleet mutation must fail closed");
        assert!(error.message.contains("requires a Unix rollout controller"));
        assert!(output.is_empty());
        assert!(!global_dir.exists());
        assert!(!state_dir.exists());
    }
}

fn select_host_classes<'a>(
    classes: &'a [HostClassConfig],
    requested: &[String],
    all_hosts: bool,
) -> Result<Vec<&'a HostClassConfig>, CliFailure> {
    if all_hosts && !requested.is_empty() {
        return Err(CliFailure::new(
            2,
            "fleet-update accepts either --host-class or --all-hosts, not both",
        ));
    }
    if !all_hosts && requested.is_empty() {
        return Err(CliFailure::new(
            2,
            "fleet-update requires at least one --host-class or explicit --all-hosts",
        ));
    }
    if all_hosts {
        return Ok(classes.iter().collect());
    }

    let mut seen = BTreeSet::new();
    for class in requested {
        if !seen.insert(class.as_str()) {
            return Err(CliFailure::new(
                2,
                format!("fleet-update host class {class:?} was selected more than once"),
            ));
        }
    }
    let by_name = classes
        .iter()
        .map(|class| (class.class.as_str(), class))
        .collect::<BTreeMap<_, _>>();
    requested
        .iter()
        .map(|name| {
            by_name.get(name.as_str()).copied().ok_or_else(|| {
                let available = by_name.keys().copied().collect::<Vec<_>>().join(", ");
                CliFailure::new(
                    2,
                    format!(
                        "unknown fleet-update host class {name:?}; configured classes: {available}"
                    ),
                )
            })
        })
        .collect()
}

fn apply_plans<W, F>(
    plans: &[HostUpdatePlan],
    target: &str,
    json: bool,
    stdout: &mut W,
    execute: F,
) -> Result<ExitCode, CliFailure>
where
    W: Write,
    F: FnMut(&HostUpdatePlan) -> Result<HostUpdateEvidence, PlanExecutionError>,
{
    let mut execute = execute;
    let mut installed_pair_sha256: Option<(String, Option<String>)> = None;
    for plan in plans {
        match execute(plan) {
            Ok(evidence) => {
                if let Err(error) = validate_evidence(plan, &evidence) {
                    render_host_result(
                        stdout,
                        json,
                        target,
                        plan,
                        false,
                        Some(&evidence),
                        Some(&error),
                    )?;
                    return Err(CliFailure::new(
                        1,
                        format!(
                            "fleet update stopped after {} evidence failed: {error}",
                            plan.class
                        ),
                    ));
                }
                let observed_pair = (
                    evidence.after_pair.primary.sha256.clone(),
                    evidence
                        .after_pair
                        .companion
                        .as_ref()
                        .map(|companion| companion.sha256.clone()),
                );
                if let Some(expected_pair) = &installed_pair_sha256
                    && expected_pair != &observed_pair
                {
                    let detail = format!(
                        "installed binary pair hashes disagreed with the first successful host: expected {expected_pair:?}, observed {observed_pair:?}"
                    );
                    render_host_result(
                        stdout,
                        json,
                        target,
                        plan,
                        false,
                        Some(&evidence),
                        Some(&detail),
                    )?;
                    return Err(CliFailure::new(
                        1,
                        format!(
                            "fleet update stopped after {} evidence failed: {detail}",
                            plan.class
                        ),
                    ));
                }
                installed_pair_sha256.get_or_insert(observed_pair);
                render_host_result(stdout, json, target, plan, true, Some(&evidence), None)?;
            }
            Err(PlanExecutionError::TimedOut(error)) => {
                render_host_result(stdout, json, target, plan, false, None, Some(&error))?;
                return Err(CliFailure::new(
                    1,
                    format!(
                        "fleet update stopped after {} timed out: {error}",
                        plan.class
                    ),
                ));
            }
            Err(PlanExecutionError::Failed(error)) => {
                render_host_result(stdout, json, target, plan, false, None, Some(&error))?;
                return Err(CliFailure::new(
                    1,
                    format!("fleet update stopped after {} failed: {error}", plan.class),
                ));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn normalize_exact_tag(raw: &str) -> Result<String, CliFailure> {
    let raw = raw.trim();
    let version = raw.strip_prefix('v').unwrap_or(raw);
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.chars().all(|ch| ch.is_ascii_digit()))
    {
        return Err(CliFailure::new(
            2,
            "fleet-update --to requires an exact stable vMAJOR.MINOR.PATCH tag",
        ));
    }
    let parsed = parts
        .iter()
        .map(|part| part.parse::<u64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| {
            CliFailure::new(
                2,
                "fleet-update --to version components must fit in unsigned 64-bit integers",
            )
        })?;
    if parsed.as_slice() < MIN_FLEET_UPDATE_TARGET.as_slice() {
        return Err(CliFailure::new(
            2,
            "fleet-update cannot safely bootstrap or validate targets older than v0.100.0; use that release's documented manual rollback procedure",
        ));
    }
    Ok(format!("v{version}"))
}

fn tag_requires_companion(tag: &str) -> bool {
    let parsed = tag
        .trim_start_matches('v')
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>();
    parsed.is_ok_and(|parts| parts.as_slice() >= MIN_PAIRED_BINARY_TARGET.as_slice())
}

#[allow(clippy::too_many_lines)] // One fail-closed validation boundary for the complete host profile.
fn host_update_plan_with_authority(
    class: &HostClassConfig,
    target: &str,
    release_authority: &ReleaseAuthority,
) -> Result<HostUpdatePlan, CliFailure> {
    if let Some(host) = &class.ssh
        && (host.starts_with('-') || host.chars().any(char::is_control))
    {
        return Err(CliFailure::new(
            2,
            format!(
                "host_class.{}.ssh is not a valid SSH destination",
                class.class
            ),
        ));
    }
    let binary = match (&class.ssh, &class.shipyard_bin) {
        (_, Some(binary)) => PathBuf::from(binary),
        (None, None) => std::env::current_exe().map_err(|error| {
            CliFailure::new(
                1,
                format!("failed to resolve local Shipyard binary: {error}"),
            )
        })?,
        (Some(_), None) => {
            return Err(CliFailure::new(
                2,
                format!(
                    "host_class.{}.shipyard_bin must name the absolute remote binary; relative lookup is launch-environment drift and cannot establish binary identity",
                    class.class
                ),
            ));
        }
    };
    let is_remote = class.ssh.is_some();
    let binary_is_absolute = if is_remote {
        class
            .shipyard_bin
            .as_deref()
            .is_some_and(|path| path.starts_with('/'))
    } else {
        binary.is_absolute()
    };
    if !binary_is_absolute {
        return Err(CliFailure::new(
            2,
            format!(
                "host_class.{}.shipyard_bin must be absolute; relative lookup is launch-environment drift, not proof the tool is absent",
                class.class
            ),
        ));
    }
    let expected_binary_name = if class.ssh.is_some() {
        "shipyard".to_owned()
    } else {
        format!("shipyard{}", std::env::consts::EXE_SUFFIX)
    };
    let binary_name = if is_remote {
        class
            .shipyard_bin
            .as_deref()
            .and_then(|path| path.rsplit('/').next())
    } else {
        binary.file_name().and_then(|name| name.to_str())
    };
    if binary_name != Some(expected_binary_name.as_str()) {
        return Err(CliFailure::new(
            2,
            format!(
                "host_class.{}.shipyard_bin must end in /{} because the verified installer replaces that exact filename",
                class.class, expected_binary_name
            ),
        ));
    }
    let expected_companion_name = format!(
        "{COMPANION_BINARY_NAME}{}",
        if is_remote {
            ""
        } else {
            std::env::consts::EXE_SUFFIX
        }
    );
    // The verified installer owns the pair transaction and always places the
    // companion adjacent to the primary under this canonical name.
    let companion_binary = binary
        .parent()
        .unwrap_or_else(|| Path::new("/"))
        .join(expected_companion_name);
    let mode = class.shipyard_mode.as_deref().ok_or_else(|| {
        CliFailure::new(
            2,
            format!(
                "host_class.{}.shipyard_mode is required to identify the daemon context",
                class.class
            ),
        )
    })?;
    let runtime_mode = match mode {
        "shipyard" => RuntimeMode::Shipyard,
        "isolated" => RuntimeMode::Isolated,
        _ => {
            return Err(CliFailure::new(
                2,
                format!("host_class.{}.shipyard_mode is invalid", class.class),
            ));
        }
    };
    let global_dir = PathBuf::from(class.shipyard_global_dir.as_deref().ok_or_else(|| {
        CliFailure::new(
            2,
            format!(
                "host_class.{}.shipyard_global_dir is required to identify the daemon context",
                class.class
            ),
        )
    })?);
    let state_dir = PathBuf::from(class.shipyard_state_dir.as_deref().ok_or_else(|| {
        CliFailure::new(
            2,
            format!(
                "host_class.{}.shipyard_state_dir is required to identify the daemon context",
                class.class
            ),
        )
    })?);
    let daemon_paths_are_absolute = if is_remote {
        class
            .shipyard_global_dir
            .as_deref()
            .is_some_and(|path| path.starts_with('/'))
            && class
                .shipyard_state_dir
                .as_deref()
                .is_some_and(|path| path.starts_with('/'))
    } else {
        global_dir.is_absolute() && state_dir.is_absolute()
    };
    if !daemon_paths_are_absolute {
        return Err(CliFailure::new(
            2,
            format!(
                "host_class.{} daemon global/state directories must be absolute",
                class.class
            ),
        ));
    }
    if !is_lexically_normal_absolute(&global_dir) || !is_lexically_normal_absolute(&state_dir) {
        return Err(CliFailure::new(
            2,
            format!(
                "host_class.{} daemon global/state directories must be normalized absolute paths",
                class.class
            ),
        ));
    }
    let auth_wrapper = class.github_cli.as_deref().map(PathBuf::from).ok_or_else(|| {
        CliFailure::new(
            2,
            format!(
                "host_class.{}.github_cli must name the absolute governed wrapper for fleet rollout",
                class.class
            ),
        )
    })?;
    let auth_helper = class
        .github_token_helper
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| {
            CliFailure::new(
                2,
                format!(
                    "host_class.{}.github_token_helper must name the absolute owner-private helper path for fleet rollout",
                    class.class
                ),
            )
        })?;
    let auth_paths_are_absolute = if is_remote {
        class
            .github_cli
            .as_deref()
            .is_some_and(|path| path.starts_with('/'))
            && class
                .github_token_helper
                .as_deref()
                .is_some_and(|path| path.starts_with('/'))
    } else {
        auth_wrapper.is_absolute() && auth_helper.is_absolute()
    };
    if !auth_paths_are_absolute || auth_wrapper == auth_helper {
        return Err(CliFailure::new(
            2,
            format!(
                "host_class.{} auth helper and wrapper paths must be distinct absolute paths",
                class.class
            ),
        ));
    }
    if auth_wrapper.parent() != binary.parent()
        || auth_wrapper.file_name().and_then(|name| name.to_str()) != Some("ghapp")
    {
        return Err(CliFailure::new(
            2,
            format!(
                "host_class.{} github_cli must be the ghapp sibling of shipyard_bin",
                class.class
            ),
        ));
    }
    if !is_lexically_normal_absolute(&auth_wrapper) || !is_lexically_normal_absolute(&auth_helper) {
        return Err(CliFailure::new(
            2,
            format!(
                "host_class.{} auth helper and wrapper paths must not contain dot or parent components",
                class.class
            ),
        ));
    }
    let auth_journal = state_dir.join("fleet-auth-support.transaction");
    let auth_lock = state_dir.join("fleet-auth-support.lock");
    let auth_context = PathBuf::from(format!("{}.shipyard-context.json", auth_wrapper.display()));
    let managed_targets = [
        auth_helper.clone(),
        auth_wrapper.clone(),
        auth_context,
        binary.clone(),
        companion_binary.clone(),
    ];
    let mut transaction_paths = Vec::with_capacity(managed_targets.len() * 3);
    for target in managed_targets {
        transaction_paths.push(target.clone());
        transaction_paths.extend(transaction_marker_paths(&target));
    }
    let mut unique_paths = std::collections::HashSet::new();
    if transaction_paths
        .iter()
        .any(|path| !unique_paths.insert(path.clone()))
        || transaction_paths
            .iter()
            .any(|path| path == &auth_journal || path.starts_with(&auth_lock))
    {
        return Err(CliFailure::new(
            2,
            format!(
                "host_class.{} auth support paths must not overlap managed binaries or transaction state",
                class.class
            ),
        ));
    }
    let command = if class.ssh.is_some() {
        remote_update_command(
            &binary,
            &companion_binary,
            target,
            release_authority,
            &auth_wrapper,
            &auth_helper,
            runtime_mode.as_str(),
            &global_dir,
            &state_dir,
        )
    } else {
        String::new()
    };
    let mut plan = HostUpdatePlan {
        class: class.class.clone(),
        ssh: class.ssh.clone(),
        binary,
        companion_binary,
        auth_helper,
        auth_wrapper,
        target: target.to_owned(),
        source_identity: release_authority.identity_sha256.clone(),
        release_authority: release_authority.clone(),
        companion_required: tag_requires_companion(target),
        command,
        runtime_mode,
        global_dir,
        state_dir,
    };
    if plan.ssh.is_none() {
        plan.command = local_update_command(&plan);
    }
    Ok(plan)
}

fn is_lexically_normal_absolute(path: &Path) -> bool {
    path.to_str().is_some_and(|raw| {
        raw.starts_with('/')
            && raw.len() > 1
            && !raw.chars().any(char::is_control)
            && raw
                .split('/')
                .skip(1)
                .all(|component| !matches!(component, "" | "." | ".."))
    })
}

fn transaction_marker_paths(path: &Path) -> [PathBuf; 2] {
    let marker = |suffix: &str| {
        let mut value = path.as_os_str().to_os_string();
        value.push(suffix);
        PathBuf::from(value)
    };
    [marker(".shipyard-rollback"), marker(".shipyard-was-absent")]
}

#[cfg(test)]
fn host_update_plan(class: &HostClassConfig, target: &str) -> Result<HostUpdatePlan, CliFailure> {
    host_update_plan_with_authority(class, target, &test_release_authority(target))
}

#[cfg(test)]
fn test_release_authority(tag: &str) -> ReleaseAuthority {
    use release_authority::ReleaseAssetAuthority;

    ReleaseAuthority {
        repository: "danielraffel/Shipyard".to_owned(),
        tag: tag.to_owned(),
        tag_object_oid: "1".repeat(40),
        commit_oid: "2".repeat(40),
        tree_oid: "3".repeat(40),
        release_id: 42,
        installer: release_authority::InstallerAuthority {
            path: "install.sh".to_owned(),
            blob_oid: "9".repeat(40),
            sha256: "a".repeat(64),
        },
        auth_helper: release_authority::SourceFileAuthority {
            path: "scripts/shipyard-github-app-token".to_owned(),
            blob_oid: "b".repeat(40),
            sha256: "c".repeat(64),
        },
        auth_wrapper: release_authority::SourceFileAuthority {
            path: "scripts/ghapp".to_owned(),
            blob_oid: "d".repeat(40),
            sha256: "e".repeat(64),
        },
        checksum_manifest: ReleaseAssetAuthority {
            id: 10,
            name: "checksums.sha256".to_owned(),
            sha256: "4".repeat(64),
            attestation_statement_sha256: None,
        },
        platform_asset: ReleaseAssetAuthority {
            id: 11,
            name: "shipyard-macos-arm64.dmg".to_owned(),
            sha256: "6".repeat(64),
            attestation_statement_sha256: Some("7".repeat(64)),
        },
        identity_sha256: "8".repeat(64),
    }
}
