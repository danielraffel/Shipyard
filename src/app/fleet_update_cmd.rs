//! Governed exact-version rollout for configured Shipyard host classes.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::Value;

mod evidence;
mod release_authority;

#[cfg(test)]
use evidence::{
    BinaryEvidence, BinaryPairEvidence, SourceIdentityBasis, execute_plan_with_timeout,
};
use evidence::{HostUpdateEvidence, PlanExecutionError, execute_plan, validate_evidence};
use release_authority::{
    GitHubReleaseAuthorityVerifier, ReleaseAuthority, ReleaseAuthorityVerifier,
};

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
    let command = if class.ssh.is_some() {
        let helper = class.github_cli.as_deref().ok_or_else(|| {
            CliFailure::new(
                2,
                format!(
                    "host_class.{}.github_cli must name the absolute governed auth helper for bootstrap rollout",
                    class.class
                ),
            )
        })?;
        let helper = PathBuf::from(helper);
        if !helper.to_string_lossy().starts_with('/') {
            return Err(CliFailure::new(
                2,
                format!(
                    "host_class.{}.github_cli must be absolute for bootstrap rollout",
                    class.class
                ),
            ));
        }
        remote_update_command(
            &binary,
            &companion_binary,
            target,
            release_authority,
            &helper,
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn remote_update_command(
    binary: &Path,
    companion_binary: &Path,
    target: &str,
    authority: &ReleaseAuthority,
    auth_helper: &Path,
    mode: &str,
    global_dir: &Path,
    state_dir: &Path,
) -> String {
    let install_dir = binary.parent().unwrap_or_else(|| Path::new("/"));
    let version = target.strip_prefix('v').unwrap_or(target);
    let installer_url = format!(
        "https://raw.githubusercontent.com/danielraffel/Shipyard/{}/install.sh",
        authority.commit_oid
    );
    let release_asset_url = format!(
        "https://api.github.com/repos/danielraffel/Shipyard/releases/assets/{}",
        authority.platform_asset.id
    );
    let before_pair = remote_pair_probe(binary, companion_binary, "before", None, false);
    let after_pair = remote_pair_probe(
        binary,
        companion_binary,
        "after",
        Some(version),
        tag_requires_companion(target),
    );
    let exact_asset_curl_shim = exact_asset_curl_shim(&authority.platform_asset.name);
    let script = format!(
        "set -euo pipefail\n\
         {}\n\
         before_status=\"$({} --mode {} --global-dir {} --state-dir {} --json daemon status | /usr/bin/tr -d '\\n')\"\n\
         token=\"$({} auth token)\"\n\
         installer=\"$(/usr/bin/mktemp)\"\n\
         staging_dir=\"$(/usr/bin/mktemp -d)\"\n\
         trap '/bin/rm -f \"$installer\"; /bin/rm -rf \"$staging_dir\"' EXIT\n\
         /usr/bin/curl -fsSL --output \"$installer\" {}\n\
         installer_sha256=\"$(/usr/bin/shasum -a 256 \"$installer\" | /usr/bin/awk '{{print $1}}')\"\n\
         test \"$installer_sha256\" = {}\n\
         release_asset=\"$staging_dir/release-asset\"\n\
         /usr/bin/printf 'Authorization: Bearer %s\\n' \"$token\" | /usr/bin/curl -fsSL -H @- -H 'Accept: application/octet-stream' --output \"$release_asset\" {}\n\
         release_asset_sha256=\"$(/usr/bin/shasum -a 256 \"$release_asset\" | /usr/bin/awk '{{print $1}}')\"\n\
         test \"$release_asset_sha256\" = {}\n\
         curl_shim=\"$staging_dir/curl-exact-asset\"\n\
         /usr/bin/printf '%s\\n' {} > \"$curl_shim\"\n\
         /bin/chmod 700 \"$curl_shim\"\n\
         SHIPYARD_FLEET_ASSET_PATH=\"$release_asset\" SHIPYARD_GITHUB_TOKEN=\"$token\" SHIPYARD_VERSION={} SHIPYARD_INSTALL_DIR=\"$staging_dir\" SHIPYARD_CURL_BIN=\"$curl_shim\" /bin/bash \"$installer\" >/dev/null\n\
         staged_binary=\"$staging_dir/shipyard\"\n\
         staged_version=\"$(\"$staged_binary\" --version)\"\n\
         test \"$staged_version\" = {}\n\
         \"$staged_binary\" --mode {} --global-dir {} --state-dir {} update --to {} --check --unattended-fleet >/dev/null\n\
         SHIPYARD_FLEET_ASSET_PATH=\"$release_asset\" SHIPYARD_GITHUB_TOKEN=\"$token\" SHIPYARD_VERSION={} SHIPYARD_INSTALL_DIR={} SHIPYARD_CURL_BIN=\"$curl_shim\" /bin/bash \"$installer\" >/dev/null\n\
         unset token\n\
         {}\n\
         {} --mode {} --global-dir {} --state-dir {} update --to {} --check --unattended-fleet >/dev/null\n\
         refresh_receipt=\"$({} --mode {} --global-dir {} --state-dir {} --json daemon refresh | /usr/bin/tr -d '\\n')\"\n\
         after_status=\"$({} --mode {} --global-dir {} --state-dir {} --json daemon status | /usr/bin/tr -d '\\n')\"\n\
         printf '%s%s\\n' {} \"$before_primary_sha256\"\n\
         printf '%s%s\\n' {} \"$before_primary_version\"\n\
         printf '%s%s\\n' {} \"$before_companion_sha256\"\n\
         printf '%s%s\\n' {} \"$before_companion_version\"\n\
         printf '%s%s\\n' {} \"$after_primary_sha256\"\n\
         printf '%s%s\\n' {} \"$after_primary_version\"\n\
         printf '%s%s\\n' {} \"$after_companion_sha256\"\n\
         printf '%s%s\\n' {} \"$after_companion_version\"\n\
         printf '%s%s\\n' {} \"$before_status\"\n\
         printf '%s%s\\n' {} \"$refresh_receipt\"\n\
         printf '%s%s\\n' {} \"$after_status\"\n\
         printf '%s%s\\n' {} {}\n\
         printf '%s%s\\n' {} \"$release_asset_sha256\"",
        before_pair,
        shlex_quote(&binary.display().to_string()),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&state_dir.display().to_string()),
        shlex_quote(&auth_helper.display().to_string()),
        shlex_quote(&installer_url),
        shlex_quote(&authority.installer.sha256),
        shlex_quote(&release_asset_url),
        shlex_quote(&authority.platform_asset.sha256),
        shlex_quote(&exact_asset_curl_shim),
        shlex_quote(version),
        shlex_quote(&format!("shipyard {version}")),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&state_dir.display().to_string()),
        shlex_quote(target),
        shlex_quote(version),
        shlex_quote(&install_dir.display().to_string()),
        after_pair,
        shlex_quote(&binary.display().to_string()),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&state_dir.display().to_string()),
        shlex_quote(target),
        shlex_quote(&binary.display().to_string()),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&state_dir.display().to_string()),
        shlex_quote(&binary.display().to_string()),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&state_dir.display().to_string()),
        shlex_quote(REMOTE_BEFORE_PRIMARY_SHA256_PREFIX),
        shlex_quote(REMOTE_BEFORE_PRIMARY_VERSION_PREFIX),
        shlex_quote(REMOTE_BEFORE_COMPANION_SHA256_PREFIX),
        shlex_quote(REMOTE_BEFORE_COMPANION_VERSION_PREFIX),
        shlex_quote(REMOTE_AFTER_PRIMARY_SHA256_PREFIX),
        shlex_quote(REMOTE_AFTER_PRIMARY_VERSION_PREFIX),
        shlex_quote(REMOTE_AFTER_COMPANION_SHA256_PREFIX),
        shlex_quote(REMOTE_AFTER_COMPANION_VERSION_PREFIX),
        shlex_quote(REMOTE_BEFORE_STATUS_PREFIX),
        shlex_quote(REMOTE_REFRESH_PREFIX),
        shlex_quote(REMOTE_AFTER_STATUS_PREFIX),
        shlex_quote(REMOTE_AUTHORITY_ID_PREFIX),
        shlex_quote(&authority.identity_sha256),
        shlex_quote(REMOTE_RELEASE_ASSET_SHA256_PREFIX),
    );
    format!(
        "/usr/bin/env -i HOME=\"$HOME\" PATH={} /usr/bin/perl -e {} {} /bin/bash -c {}",
        REMOTE_MINIMAL_PATH,
        shlex_quote(REMOTE_SUPERVISOR),
        REMOTE_UPDATE_TIMEOUT.as_secs(),
        shlex_quote(&script),
    )
}

fn exact_asset_curl_shim(asset_name: &str) -> String {
    format!(
        "#!/bin/bash\nset -euo pipefail\ncase \"$*\" in\n  *\"/releases/tags/\"*) /usr/bin/printf '{{\"assets\":[{{\"name\":\"{asset_name}\",\"url\":\"file://%s\",\"browser_download_url\":\"file://%s\"}}]}}\\n200\\n' \"$SHIPYARD_FLEET_ASSET_PATH\" \"$SHIPYARD_FLEET_ASSET_PATH\" ;;\n  *) exec /usr/bin/curl \"$@\" ;;\nesac"
    )
}

fn remote_pair_probe(
    binary: &Path,
    companion: &Path,
    prefix: &str,
    expected_version: Option<&str>,
    companion_required: bool,
) -> String {
    let binary = shlex_quote(&binary.display().to_string());
    let companion = shlex_quote(&companion.display().to_string());
    let [minimum_major, minimum_minor, minimum_patch] = MIN_PAIRED_BINARY_TARGET;
    let expected = expected_version.map_or_else(String::new, |version| {
        let primary = shlex_quote(&format!("shipyard {version}"));
        let provider = shlex_quote(&format!("{COMPANION_BINARY_NAME} {version}"));
        if companion_required {
            format!(
                "test \"${prefix}_primary_version\" = {primary}\n\
                 test \"${prefix}_companion_version\" = {provider}"
            )
        } else {
            format!(
                "test \"${prefix}_primary_version\" = {primary}\n\
                 test \"${prefix}_companion_version\" = absent"
            )
        }
    });
    let inferred = expected_version.map_or_else(
        || format!(
            "{prefix}_semver=\"${{{prefix}_primary_version#shipyard }}\"\n\
             test \"${prefix}_primary_version\" = \"shipyard ${{{prefix}_semver}}\"\n\
             case \"${{{prefix}_semver}}\" in *.*.*) ;; *) exit 1 ;; esac\n\
             case \"${{{prefix}_semver}}\" in *.*.*.*) exit 1 ;; esac\n\
             IFS=. read -r {prefix}_major {prefix}_minor {prefix}_patch <<EOF\n\
             ${{{prefix}_semver}}\n\
             EOF\n\
             for {prefix}_component in \"${{{prefix}_major}}\" \"${{{prefix}_minor}}\" \"${{{prefix}_patch}}\"; do\n\
               case \"${{{prefix}_component}}\" in *[!0-9]*|'') exit 1 ;; esac\n\
               case \"${{{prefix}_component}}\" in 0|[1-9]*) ;; *) exit 1 ;; esac\n\
               if [ \"${{#{prefix}_component}}\" -gt 20 ] || {{ [ \"${{#{prefix}_component}}\" -eq 20 ] && [ \"${{{prefix}_component}}\" \\> 18446744073709551615 ]; }}; then exit 1; fi\n\
             done\n\
             {prefix}_decimal_gt() {{\n\
               [ \"${{#1}}\" -gt \"${{#2}}\" ] || {{ [ \"${{#1}}\" -eq \"${{#2}}\" ] && [ \"$1\" \\> \"$2\" ]; }}\n\
             }}\n\
             {prefix}_requires=0\n\
             if {prefix}_decimal_gt \"${{{prefix}_major}}\" {minimum_major} || {{ [ \"${{{prefix}_major}}\" = {minimum_major} ] && {{ {prefix}_decimal_gt \"${{{prefix}_minor}}\" {minimum_minor} || {{ [ \"${{{prefix}_minor}}\" = {minimum_minor} ] && {{ [ \"${{{prefix}_patch}}\" = {minimum_patch} ] || {prefix}_decimal_gt \"${{{prefix}_patch}}\" {minimum_patch}; }}; }}; }}; }}; then {prefix}_requires=1; fi\n\
             if [ \"${{{prefix}_requires}}\" -eq 1 ]; then\n\
               test \"${prefix}_companion_version\" = \"{COMPANION_BINARY_NAME} ${{{prefix}_semver}}\"\n\
             else\n\
               test \"${prefix}_companion_version\" = absent\n\
             fi"
        ),
        |_| expected,
    );
    format!(
        "{prefix}_primary_sha256_before=\"$(/usr/bin/shasum -a 256 {binary} | /usr/bin/awk '{{print $1}}')\"\n\
         {prefix}_primary_version=\"$({binary} --version)\"\n\
         {prefix}_primary_sha256=\"$(/usr/bin/shasum -a 256 {binary} | /usr/bin/awk '{{print $1}}')\"\n\
         test \"${prefix}_primary_sha256_before\" = \"${prefix}_primary_sha256\"\n\
         if [ -e {companion} ] || [ -L {companion} ]; then\n\
           test -x {companion}\n\
           {prefix}_companion_sha256_before=\"$(/usr/bin/shasum -a 256 {companion} | /usr/bin/awk '{{print $1}}')\"\n\
           {prefix}_companion_version=\"$({companion} --version)\"\n\
           {prefix}_companion_sha256=\"$(/usr/bin/shasum -a 256 {companion} | /usr/bin/awk '{{print $1}}')\"\n\
           test \"${prefix}_companion_sha256_before\" = \"${prefix}_companion_sha256\"\n\
         else\n\
           {prefix}_companion_version=absent\n\
           {prefix}_companion_sha256=absent\n\
         fi\n\
         {inferred}"
    )
}

fn local_update_command(plan: &HostUpdatePlan) -> String {
    let installer_url = format!(
        "https://raw.githubusercontent.com/danielraffel/Shipyard/{}/install.sh",
        plan.release_authority.commit_oid
    );
    let release_asset_url = format!(
        "https://api.github.com/repos/danielraffel/Shipyard/releases/assets/{}",
        plan.release_authority.platform_asset.id
    );
    let curl_shim = exact_asset_curl_shim(&plan.release_authority.platform_asset.name);
    format!(
        "set -euo pipefail; staging_dir=\"$(/usr/bin/mktemp -d)\"; installer=\"$staging_dir/install.sh\"; release_asset=\"$staging_dir/release-asset\"; curl_shim=\"$staging_dir/curl-exact-asset\"; trap '/bin/rm -rf \"$staging_dir\"' EXIT; /usr/bin/curl -fsSL --output \"$installer\" {}; installer_sha256=\"$(/usr/bin/shasum -a 256 \"$installer\" | /usr/bin/awk '{{print $1}}')\"; test \"$installer_sha256\" = {}; /usr/bin/curl -fsSL -H 'Accept: application/octet-stream' --output \"$release_asset\" {}; release_asset_sha256=\"$(/usr/bin/shasum -a 256 \"$release_asset\" | /usr/bin/awk '{{print $1}}')\"; test \"$release_asset_sha256\" = {}; /usr/bin/printf '%s\\n' {} > \"$curl_shim\"; /bin/chmod 700 \"$curl_shim\"; SHIPYARD_FLEET_ASSET_PATH=\"$release_asset\" /usr/bin/env -i HOME={} PATH={} SHIPYARD_FLEET_ASSET_PATH=\"$release_asset\" {} --mode {} --global-dir {} --state-dir {} --json update --to {} --install-script-url \"file://$installer\" --curl-bin \"$curl_shim\" --refresh-daemon --unattended-fleet",
        shlex_quote(&installer_url),
        shlex_quote(&plan.release_authority.installer.sha256),
        shlex_quote(&release_asset_url),
        shlex_quote(&plan.release_authority.platform_asset.sha256),
        shlex_quote(&curl_shim),
        shlex_quote(&home_dir().display().to_string()),
        shlex_quote(&unattended_tool_path().to_string_lossy()),
        shlex_quote(&plan.binary.display().to_string()),
        plan.runtime_mode.as_str(),
        shlex_quote(&plan.global_dir.display().to_string()),
        shlex_quote(&plan.state_dir.display().to_string()),
        shlex_quote(&plan.target),
    )
}

fn render_plan<W: Write>(
    stdout: &mut W,
    json: bool,
    target: &str,
    plans: &[HostUpdatePlan],
    all_hosts: bool,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("plan"));
        data.insert("target".to_owned(), Value::from(target));
        data.insert("apply".to_owned(), Value::Bool(false));
        data.insert("all_hosts".to_owned(), Value::Bool(all_hosts));
        data.insert(
            "selected_host_classes".to_owned(),
            serde_json::to_value(plans.iter().map(|plan| &plan.class).collect::<Vec<_>>())
                .map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        data.insert(
            "hosts".to_owned(),
            Value::Array(
                plans
                    .iter()
                    .map(|plan| {
                        serde_json::json!({
                            "class": plan.class,
                            "ssh": plan.ssh,
                            "binary": plan.binary,
                            "companion_binary": plan.companion_binary,
                            "source_identity": plan.source_identity,
                            "release_authority": plan.release_authority,
                            "companion_required": plan.companion_required,
                            "command": plan.command,
                        })
                    })
                    .collect(),
            ),
        );
        write_json_envelope(stdout, "runner.fleet-update", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "Fleet update plan for {target} ({}):",
            if all_hosts {
                "explicit all-host selection"
            } else {
                "explicit host-class selection"
            }
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for plan in plans {
            let route = plan.ssh.as_deref().unwrap_or("local");
            writeln!(stdout, "  {} ({route}): {}", plan.class, plan.command)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        writeln!(stdout, "Re-run with --apply to execute.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn render_host_result<W: Write>(
    stdout: &mut W,
    json: bool,
    target: &str,
    plan: &HostUpdatePlan,
    ok: bool,
    evidence: Option<&HostUpdateEvidence>,
    error: Option<&str>,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("host_result"));
        data.insert("target".to_owned(), Value::from(target));
        data.insert("host_class".to_owned(), Value::from(plan.class.clone()));
        data.insert("ok".to_owned(), Value::Bool(ok));
        data.insert(
            "binary".to_owned(),
            Value::from(plan.binary.display().to_string()),
        );
        data.insert(
            "companion_binary".to_owned(),
            Value::from(plan.companion_binary.display().to_string()),
        );
        data.insert(
            "source_identity".to_owned(),
            Value::from(plan.source_identity.clone()),
        );
        data.insert(
            "release_authority".to_owned(),
            serde_json::to_value(&plan.release_authority)
                .map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        data.insert(
            "daemon_mode".to_owned(),
            Value::from(plan.runtime_mode.as_str()),
        );
        data.insert(
            "daemon_global_dir".to_owned(),
            Value::from(plan.global_dir.display().to_string()),
        );
        data.insert(
            "daemon_state_dir".to_owned(),
            Value::from(plan.state_dir.display().to_string()),
        );
        if let Some(evidence) = evidence {
            insert_binary_pair_evidence(&mut data, evidence)?;
            data.insert(
                "executable_sha256".to_owned(),
                Value::from(evidence.executable_sha256.clone()),
            );
            data.insert(
                "cli_version".to_owned(),
                Value::from(evidence.cli_version.clone()),
            );
            data.insert(
                "daemon_version".to_owned(),
                Value::from(evidence.daemon_version.clone()),
            );
            data.insert("daemon_pid".to_owned(), Value::from(evidence.daemon_pid));
            data.insert(
                "configured_repos_before".to_owned(),
                serde_json::to_value(&evidence.configured_repos_before)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?,
            );
            data.insert(
                "configured_repos_after".to_owned(),
                serde_json::to_value(&evidence.configured_repos_after)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?,
            );
            data.insert(
                "configured_repos_preserved".to_owned(),
                serde_json::to_value(evidence.configured_repos_preserved)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?,
            );
        }
        if let Some(error) = error {
            data.insert("error".to_owned(), Value::from(error));
        }
        write_json_envelope(stdout, "runner.fleet-update", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if ok {
        writeln!(
            stdout,
            "{}: updated to {target}; primary sha256={}; companion sha256={}; source={}; daemon pid={} version={}; configured repos preserved={}",
            plan.class,
            evidence.map_or("unavailable", |value| value.executable_sha256.as_str()),
            evidence
                .and_then(|value| value.after_pair.companion.as_ref())
                .map_or("absent", |value| value.sha256.as_str()),
            evidence
                .and_then(|value| value.after_pair.primary.source_identity.as_deref())
                .unwrap_or("unavailable"),
            evidence.map_or(0, |value| value.daemon_pid),
            evidence.map_or("unavailable", |value| value.daemon_version.as_str()),
            evidence
                .and_then(|value| value.configured_repos_preserved)
                .map_or("not-applicable", |preserved| if preserved { "true" } else { "false" }),
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "{}: FAILED ({})",
            plan.class,
            error.unwrap_or("unknown error")
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn insert_binary_pair_evidence(
    data: &mut BTreeMap<String, Value>,
    evidence: &HostUpdateEvidence,
) -> Result<(), CliFailure> {
    data.insert(
        "release_authority_identity".to_owned(),
        Value::from(evidence.release_authority_identity.clone()),
    );
    data.insert(
        "release_asset_sha256".to_owned(),
        Value::from(evidence.release_asset_sha256.clone()),
    );
    data.insert(
        "binary_pair_before".to_owned(),
        serde_json::to_value(&evidence.before_pair)
            .map_err(|error| CliFailure::new(1, error.to_string()))?,
    );
    data.insert(
        "binary_pair_after".to_owned(),
        serde_json::to_value(&evidence.after_pair)
            .map_err(|error| CliFailure::new(1, error.to_string()))?,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn host(ssh: Option<&str>, shipyard_bin: Option<&str>) -> HostClassConfig {
        HostClassConfig {
            class: "m5".to_owned(),
            ssh: ssh.map(str::to_owned),
            cap: 2,
            tart_bin: "/opt/homebrew/bin/tart".to_owned(),
            tartci_bin: "/Users/ci/.local/bin/tartci".to_owned(),
            shipyard_bin: shipyard_bin.map(str::to_owned),
            shipyard_mode: Some("shipyard".to_owned()),
            shipyard_global_dir: Some("/Users/ci/Library/Application Support/shipyard".to_owned()),
            shipyard_state_dir: Some("/Users/ci/Library/Application Support/shipyard".to_owned()),
            github_cli: Some("/Users/ci/.local/bin/ghapp".to_owned()),
            tart_home: Some("/Users/ci/VMs".to_owned()),
            labels: Vec::new(),
        }
    }

    #[test]
    fn remote_plan_uses_absolute_binary_and_minimal_canonical_path() {
        let plan = host_update_plan(
            &host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard")),
            "v0.127.0",
        )
        .expect("plan");
        assert!(plan.command.starts_with("/usr/bin/env -i HOME=\"$HOME\""));
        assert!(plan.command.contains("/opt/homebrew/bin"));
        assert!(plan.command.contains("/usr/bin/perl -e"));
        assert!(plan.command.contains("TERM"));
        assert!(plan.command.contains("KILL"));
        assert!(plan.command.contains("waitpid"));
        assert!(plan.command.contains(&format!(
            " {} /bin/bash -c",
            REMOTE_UPDATE_TIMEOUT.as_secs()
        )));
        assert!(plan.command.contains("/Users/ci/.local/bin/shipyard"));
        assert!(plan.command.contains("/Users/ci/.local/bin/ghapp"));
        assert!(
            plan.command
                .contains(&format!("Shipyard/{}/install.sh", "2".repeat(40)))
        );
        assert!(plan.command.contains("releases/assets/11"));
        assert!(plan.command.contains(&"a".repeat(64)));
        assert!(plan.command.contains(&"6".repeat(64)));
        assert!(plan.command.contains("--mode shipyard"));
        assert!(
            plan.command
                .contains("/Users/ci/Library/Application Support/shipyard")
        );
        assert!(
            plan.command
                .contains("update --to v0.127.0 --check --unattended-fleet")
        );
        assert_eq!(
            plan.companion_binary,
            PathBuf::from("/Users/ci/.local/bin/shipyard-workstream-provider")
        );
        assert!(plan.companion_required);
        assert_eq!(plan.source_identity, "8".repeat(64));
        assert!(plan.command.contains("/usr/bin/shasum -a 256"));
        assert!(plan.command.contains(REMOTE_BEFORE_STATUS_PREFIX));
        assert!(plan.command.contains(REMOTE_REFRESH_PREFIX));
        assert!(plan.command.contains(REMOTE_AFTER_STATUS_PREFIX));
        assert!(plan.command.contains(REMOTE_AUTHORITY_ID_PREFIX));
        assert!(plan.command.contains(REMOTE_RELEASE_ASSET_SHA256_PREFIX));
        let status_probe = plan.command.find("before_status=").expect("status probe");
        let auth = plan.command.find("token=").expect("auth boundary");
        assert!(
            status_probe < auth,
            "status must fail before auth and install"
        );
        assert!(!plan.command.contains("observed_before="));
        let preflight = plan
            .command
            .find("staged_binary")
            .expect("staged authenticated preflight");
        let replacement = plan
            .command
            .find("SHIPYARD_INSTALL_DIR=/Users/ci/.local/bin")
            .expect("real install destination");
        assert!(
            preflight < replacement,
            "governed config and helper must pass before binary replacement"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn remote_pair_probe_rejects_mixed_or_malformed_preinstall_state() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let primary = temp.path().join("shipyard");
        let companion = temp.path().join(COMPANION_BINARY_NAME);
        let write_binary = |path: &Path, label: &str, version: &str| {
            std::fs::write(
                path,
                format!("#!/bin/sh\nprintf '%s\\n' '{label} {version}'\n"),
            )
            .expect("fixture");
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
                .expect("executable");
        };
        write_binary(&primary, "shipyard", "0.126.2");
        let legacy_probe = remote_pair_probe(&primary, &companion, "before", None, false);
        assert!(
            Command::new("/bin/bash")
                .args(["-c", &legacy_probe])
                .status()
                .expect("legacy probe")
                .success()
        );

        write_binary(&companion, COMPANION_BINARY_NAME, "0.127.0");
        assert!(
            !Command::new("/bin/bash")
                .args(["-c", &legacy_probe])
                .status()
                .expect("mixed probe")
                .success()
        );

        write_binary(&primary, "shipyard", "0.127.0");
        let paired_probe = remote_pair_probe(&primary, &companion, "before", None, false);
        assert!(
            Command::new("/bin/bash")
                .args(["-c", &paired_probe])
                .status()
                .expect("paired probe")
                .success()
        );

        for malformed in [
            "0.126.",
            "0.0126.3",
            "0.127.0.1",
            "18446744073709551616.0.0",
        ] {
            write_binary(&primary, "shipyard", malformed);
            write_binary(&companion, COMPANION_BINARY_NAME, malformed);
            let malformed_probe = remote_pair_probe(&primary, &companion, "before", None, false);
            assert!(
                !Command::new("/bin/bash")
                    .args(["-c", &malformed_probe])
                    .status()
                    .expect("malformed probe")
                    .success(),
                "malformed preinstall version {malformed:?} must fail before rollout"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_plan_preserves_host_class_daemon_context() {
        let mut class = host(None, Some("/Users/ci/.local/bin/shipyard"));
        class.shipyard_mode = Some("isolated".to_owned());
        class.shipyard_global_dir = Some("/tmp/governed config".to_owned());
        class.shipyard_state_dir = Some("/tmp/governed state".to_owned());
        let plan = host_update_plan(&class, "v0.98.1").expect("plan");
        let command = local_update_command(&plan);

        assert!(command.contains("--mode isolated"));
        assert!(command.contains("--global-dir '/tmp/governed config'"));
        assert!(command.contains("--state-dir '/tmp/governed state'"));
        assert!(command.ends_with("--refresh-daemon --unattended-fleet"));
    }

    #[cfg(unix)]
    #[test]
    fn exact_asset_shim_serves_authenticated_and_unauthenticated_installer_urls() {
        let temp = tempfile::tempdir().expect("temp");
        let shim = temp.path().join("curl-shim");
        std::fs::write(&shim, exact_asset_curl_shim("shipyard-macos-arm64")).expect("shim");
        let asset = temp.path().join("verified-asset");
        let output = Command::new("/bin/bash")
            .arg(&shim)
            .arg("https://api.github.com/repos/example/shipyard/releases/tags/v1.2.3")
            .env("SHIPYARD_FLEET_ASSET_PATH", &asset)
            .output()
            .expect("run shim");
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 response");
        let payload: Value =
            serde_json::from_str(stdout.lines().next().expect("release response JSON line"))
                .expect("release response JSON");
        let expected = format!("file://{}", asset.display());
        assert_eq!(
            payload.pointer("/assets/0/url").and_then(Value::as_str),
            Some(expected.as_str())
        );
        assert_eq!(
            payload
                .pointer("/assets/0/browser_download_url")
                .and_then(Value::as_str),
            Some(expected.as_str())
        );
    }

    #[test]
    fn stripped_path_is_launch_environment_drift_not_absence() {
        let error = host_update_plan(&host(Some("m5"), None), "v0.98.1")
            .expect_err("remote relative lookup must fail closed");
        assert!(error.message.contains("launch-environment drift"));
        assert!(!error.message.contains("not installed"));
        assert!(!error.message.contains("absent"));
    }

    #[test]
    fn option_like_ssh_destination_is_rejected_before_spawn() {
        let error = host_update_plan(
            &host(
                Some("-oProxyCommand=/tmp/untrusted"),
                Some("/Users/ci/.local/bin/shipyard"),
            ),
            "v0.98.1",
        )
        .expect_err("SSH option injection");
        assert!(error.message.contains("not a valid SSH destination"));
    }

    #[test]
    fn remote_bootstrap_requires_absolute_governed_auth_helper() {
        let mut config = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
        config.github_cli = Some("ghapp".to_owned());
        let error = host_update_plan(&config, "v0.98.1").expect_err("relative helper");
        assert!(
            error
                .message
                .contains("github_cli must be absolute for bootstrap rollout")
        );
    }

    #[test]
    fn remote_rollout_requires_an_explicit_daemon_context() {
        let mut config = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
        config.shipyard_state_dir = None;
        let error = host_update_plan(&config, "v0.100.0").expect_err("missing context");
        assert!(
            error
                .message
                .contains("shipyard_state_dir is required to identify the daemon context")
        );
    }

    #[test]
    fn remote_bootstrap_rejects_a_filename_the_installer_cannot_replace() {
        let error = host_update_plan(
            &host(Some("m5-lan"), Some("/Users/ci/.local/bin/current")),
            "v0.98.1",
        )
        .expect_err("renamed binary");
        assert!(error.message.contains("must end in /shipyard"));
    }

    #[cfg(unix)]
    #[test]
    fn local_rollout_rejects_a_filename_the_installer_cannot_replace() {
        let error = host_update_plan(&host(None, Some("/Users/ci/.local/bin/current")), "v0.98.1")
            .expect_err("renamed local binary");
        assert!(error.message.contains("must end in /shipyard"));
    }

    #[cfg(unix)]
    #[test]
    fn one_stalled_host_is_terminated_at_its_bound() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let binary = temp.path().join("shipyard");
        std::fs::write(
            &binary,
            "#!/bin/sh\ncase \"$*\" in *\"daemon status\"*) printf '%s\\n' '{\"command\":\"daemon:status\",\"running\":false}' ;; *) sleep 60 ;; esac\n",
        )
        .expect("fixture");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("executable");
        let plan = host_update_plan(&host(None, binary.to_str()), "v0.98.1").expect("plan");
        assert!(matches!(
            execute_plan_with_timeout(&plan, Duration::from_millis(50)),
            Err(PlanExecutionError::TimedOut(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn configured_absolute_tool_survives_a_stripped_non_login_path() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let homebrew_bin = temp.path().join("opt/homebrew/bin");
        std::fs::create_dir_all(&homebrew_bin).expect("homebrew bin");
        let tart = homebrew_bin.join("tart");
        std::fs::write(&tart, "#!/bin/sh\nexit 0\n").expect("tool fixture");
        std::fs::set_permissions(&tart, std::fs::Permissions::from_mode(0o755))
            .expect("executable fixture");
        let shipyard = homebrew_bin.join("shipyard");
        std::fs::write(&shipyard, "#!/bin/sh\nexit 0\n").expect("Shipyard fixture");
        std::fs::set_permissions(&shipyard, std::fs::Permissions::from_mode(0o755))
            .expect("executable Shipyard fixture");

        let hidden = Command::new("/usr/bin/env")
            .args([
                "-i",
                "PATH=/usr/bin:/bin",
                "/bin/sh",
                "-c",
                "command -v tart",
            ])
            .output()
            .expect("stripped-path probe");
        assert!(
            !hidden.status.success(),
            "ambient lookup must miss the fixture"
        );

        let plan = host_update_plan(&host(Some("m5-lan"), shipyard.to_str()), "v0.98.1")
            .expect("an absolute profile path remains authoritative");
        assert_eq!(plan.binary, shipyard);
        assert!(
            plan.command
                .contains(&shlex_quote(&shipyard.display().to_string()))
        );
    }

    #[test]
    fn exact_release_tag_is_required() {
        assert_eq!(normalize_exact_tag("0.100.0").expect("tag"), "v0.100.0");
        assert!(normalize_exact_tag("v0.99.0").is_err());
        assert!(normalize_exact_tag("v0.98.1").is_err());
        assert!(normalize_exact_tag("latest").is_err());
        assert!(normalize_exact_tag("v0.98").is_err());
        assert!(normalize_exact_tag("v0.98.1-rc1").is_err());
        assert!(normalize_exact_tag("v18446744073709551616.0.0").is_err());
        assert!(!tag_requires_companion("v0.126.2"));
        assert!(tag_requires_companion("v0.127.0"));
    }

    fn named_host(name: &str) -> HostClassConfig {
        let mut class = host(None, Some("/Users/ci/.local/bin/shipyard"));
        class.class = name.to_owned();
        class
    }

    fn pair(version: &str, verified: bool) -> BinaryPairEvidence {
        let source_identity = verified.then(|| "8".repeat(64));
        let source_identity_basis = if verified {
            SourceIdentityBasis::VerifiedReleaseAuthority
        } else {
            SourceIdentityBasis::UnverifiedPreinstall
        };
        let primary = BinaryEvidence {
            path: PathBuf::from("/Users/ci/.local/bin/shipyard"),
            semantic_version: version.to_owned(),
            sha256: "a".repeat(64),
            source_identity: source_identity.clone(),
            source_identity_basis,
        };
        let companion = tag_requires_companion(&format!("v{version}")).then(|| BinaryEvidence {
            path: PathBuf::from("/Users/ci/.local/bin/shipyard-workstream-provider"),
            semantic_version: version.to_owned(),
            sha256: "b".repeat(64),
            source_identity,
            source_identity_basis,
        });
        BinaryPairEvidence { primary, companion }
    }

    fn evidence(version: &str) -> HostUpdateEvidence {
        HostUpdateEvidence {
            release_authority_identity: "8".repeat(64),
            release_asset_sha256: "6".repeat(64),
            executable_sha256: "a".repeat(64),
            cli_version: format!("shipyard {version}"),
            before_pair: pair(version, false),
            after_pair: pair(version, true),
            daemon_version: version.to_owned(),
            daemon_pid: 42,
            configured_repos_before: Some(vec!["owner/repo".to_owned()]),
            configured_repos_after: vec!["owner/repo".to_owned()],
            configured_repos_preserved: Some(true),
        }
    }

    #[test]
    fn host_selection_is_explicit_ordered_and_fail_closed() {
        let classes = vec![named_host("m1"), named_host("m3"), named_host("m5")];
        let error = select_host_classes(&classes, &[], false).expect_err("implicit fleet");
        assert!(error.message.contains("explicit --all-hosts"));

        let selected = select_host_classes(&classes, &["m5".to_owned(), "m1".to_owned()], false)
            .expect("selected subset");
        assert_eq!(
            selected
                .iter()
                .map(|class| class.class.as_str())
                .collect::<Vec<_>>(),
            ["m5", "m1"]
        );

        let duplicate = select_host_classes(&classes, &["m1".to_owned(), "m1".to_owned()], false)
            .expect_err("duplicate");
        assert!(duplicate.message.contains("more than once"));
        let unknown =
            select_host_classes(&classes, &["studio".to_owned()], false).expect_err("unknown");
        assert!(unknown.message.contains("configured classes: m1, m3, m5"));
        assert!(select_host_classes(&classes, &["m1".to_owned()], true).is_err());
        assert_eq!(
            select_host_classes(&classes, &[], true)
                .expect("explicit all")
                .len(),
            3
        );
    }

    #[test]
    fn apply_stops_before_every_later_host_after_first_failure() {
        let plans = ["m1", "m3", "m5"]
            .iter()
            .map(|name| host_update_plan(&named_host(name), "v0.100.0").expect("plan"))
            .collect::<Vec<_>>();
        let mut attempted = Vec::new();
        let mut output = Vec::new();
        let error = apply_plans(&plans, "v0.100.0", true, &mut output, |plan| {
            attempted.push(plan.class.clone());
            if plan.class == "m3" {
                Err(PlanExecutionError::Failed("controlled failure".to_owned()))
            } else {
                Ok(evidence("0.100.0"))
            }
        })
        .expect_err("apply stops");
        assert_eq!(attempted, ["m1", "m3"]);
        assert!(error.message.contains("stopped after m3"));
        let rendered = String::from_utf8(output).expect("UTF-8");
        let receipts = serde_json::Deserializer::from_str(&rendered)
            .into_iter::<Value>()
            .collect::<Result<Vec<_>, _>>()
            .expect("typed receipts");
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0]["host_class"], "m1");
        assert_eq!(receipts[0]["target"], "v0.100.0");
        assert_eq!(receipts[0]["executable_sha256"], "a".repeat(64));
        assert_eq!(
            receipts[0]["binary_pair_before"]["primary"]["source_identity"],
            Value::Null
        );
        assert_eq!(
            receipts[0]["binary_pair_before"]["primary"]["source_identity_basis"],
            "unverified_preinstall"
        );
        assert_eq!(receipts[0]["binary_pair_after"]["companion"], Value::Null);
        assert_eq!(receipts[0]["daemon_pid"], 42);
        assert_eq!(receipts[0]["configured_repos_preserved"], true);
        assert_eq!(receipts[1]["host_class"], "m3");
        assert_eq!(receipts[1]["ok"], false);
        assert!(!rendered.contains("\"host_class\": \"m5\""));
    }

    #[test]
    fn authority_receipt_mismatch_stops_before_the_next_host() {
        let plans = ["m1", "m3"]
            .iter()
            .map(|name| host_update_plan(&named_host(name), "v0.127.0").expect("plan"))
            .collect::<Vec<_>>();
        let mut attempted = Vec::new();
        let mut output = Vec::new();
        let error = apply_plans(&plans, "v0.127.0", true, &mut output, |plan| {
            attempted.push(plan.class.clone());
            let mut observed = evidence("0.127.0");
            if plan.class == "m3" {
                observed.release_authority_identity = "f".repeat(64);
            }
            Ok(observed)
        })
        .expect_err("drift must stop rollout");
        assert_eq!(attempted, ["m1", "m3"]);
        assert!(error.message.contains("stopped after m3 evidence failed"));
        assert!(error.message.contains("frozen release authority"));
    }

    #[test]
    fn cross_host_binary_pair_hash_drift_stops_rollout() {
        let plans = ["m1", "m3", "m5"]
            .iter()
            .map(|name| host_update_plan(&named_host(name), "v0.127.0").expect("plan"))
            .collect::<Vec<_>>();
        let mut attempted = Vec::new();
        let mut output = Vec::new();
        let error = apply_plans(&plans, "v0.127.0", true, &mut output, |plan| {
            attempted.push(plan.class.clone());
            let mut observed = evidence("0.127.0");
            if plan.class == "m3" {
                observed.after_pair.primary.sha256 = "d".repeat(64);
                observed.executable_sha256 = "d".repeat(64);
            }
            Ok(observed)
        })
        .expect_err("cross-host drift must stop rollout");
        assert_eq!(attempted, ["m1", "m3"]);
        assert!(error.message.contains("hashes disagreed"));
    }

    #[test]
    fn paired_host_receipt_exposes_reconcilable_before_and_after_identities() {
        let plan = host_update_plan(&named_host("m1"), "v0.127.0").expect("plan");
        let evidence = evidence("0.127.0");
        let mut output = Vec::new();
        render_host_result(
            &mut output,
            true,
            "v0.127.0",
            &plan,
            true,
            Some(&evidence),
            None,
        )
        .expect("receipt");
        let receipt: Value = serde_json::from_slice(&output).expect("json");
        for phase in ["binary_pair_before", "binary_pair_after"] {
            assert_eq!(receipt[phase]["primary"]["semantic_version"], "0.127.0");
            assert_eq!(receipt[phase]["companion"]["semantic_version"], "0.127.0");
            assert_eq!(
                receipt[phase]["primary"]["source_identity"],
                receipt[phase]["companion"]["source_identity"]
            );
        }
        assert_eq!(
            receipt["binary_pair_before"]["primary"]["source_identity_basis"],
            "unverified_preinstall"
        );
        assert_eq!(
            receipt["binary_pair_after"]["primary"]["source_identity_basis"],
            "verified_release_authority"
        );
        assert_eq!(receipt["release_authority"]["commit_oid"], "2".repeat(40));
        assert_eq!(receipt["release_authority"]["tree_oid"], "3".repeat(40));
        assert_eq!(
            receipt["release_authority"]["checksum_manifest"]["sha256"],
            "4".repeat(64)
        );
        assert_eq!(
            receipt["release_authority"]["platform_asset"]["attestation_statement_sha256"],
            "7".repeat(64)
        );
    }
}
