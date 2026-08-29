//! Governed exact-version rollout for configured Shipyard host classes.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::Value;

mod auth_support;
mod evidence;
mod release_authority;

#[cfg(test)]
use evidence::{
    AuthSupportEvidence, BinaryEvidence, BinaryPairEvidence, SourceIdentityBasis,
    SupportFileEvidence, execute_plan_with_timeout,
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
    if !auth_wrapper.is_absolute() || !auth_helper.is_absolute() || auth_wrapper == auth_helper {
        return Err(CliFailure::new(
            2,
            format!(
                "host_class.{} auth helper and wrapper paths must be distinct absolute paths",
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
    let managed_targets = [
        auth_helper.clone(),
        auth_wrapper.clone(),
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
                "host_class.{} auth helper and wrapper paths must not overlap managed binaries or transaction state",
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn remote_update_command(
    binary: &Path,
    companion_binary: &Path,
    target: &str,
    authority: &ReleaseAuthority,
    auth_wrapper: &Path,
    auth_helper: &Path,
    mode: &str,
    global_dir: &Path,
    state_dir: &Path,
) -> String {
    let install_dir = binary.parent().unwrap_or_else(|| Path::new("/"));
    let version = target.strip_prefix('v').unwrap_or(target);
    let installer_url = format!(
        "https://raw.githubusercontent.com/{}/{}/{}",
        authority.repository, authority.commit_oid, authority.installer.path
    );
    let release_asset_url = format!(
        "https://api.github.com/repos/{}/releases/assets/{}",
        authority.repository, authority.platform_asset.id
    );
    let (auth_helper_url, auth_wrapper_url) = auth_support::source_urls(authority);
    let before_auth = auth_support::probe(auth_helper, auth_wrapper, "before");
    let after_auth = auth_support::probe(auth_helper, auth_wrapper, "after");
    let auth_contract = auth_support::wrapper_helper_contract_probe(auth_helper);
    let binary_install_command = format!(
        "SHIPYARD_FLEET_ASSET_PATH=\"$release_asset\" SHIPYARD_GITHUB_TOKEN=\"$token\" SHIPYARD_VERSION={} SHIPYARD_INSTALL_DIR={} SHIPYARD_CURL_BIN=\"$curl_shim\" /bin/bash \"$installer\" >/dev/null",
        shlex_quote(version),
        shlex_quote(&install_dir.display().to_string())
    );
    let auth_transaction = auth_support::install_transaction(
        auth_helper,
        auth_wrapper,
        binary,
        companion_binary,
        tag_requires_companion(target),
        "\"$auth_helper_source\"",
        "\"$auth_wrapper_source\"",
        &binary_install_command,
        state_dir,
        authority,
        false,
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
        "set -euo pipefail\n{}\n{}\n{}\n\
         before_status=\"$({} --mode {} --global-dir {} --state-dir {} --json daemon status | /usr/bin/tr -d '\\n')\"\n\
         token=\"$({} auth token)\"\n\
         installer=\"$(/usr/bin/mktemp)\"; staging_dir=\"$(/usr/bin/mktemp -d)\"\n\
         trap '/bin/rm -f \"$installer\"; /bin/rm -rf \"$staging_dir\"' EXIT\n\
         /usr/bin/curl -fsSL --output \"$installer\" {}\n\
         test \"$(/usr/bin/shasum -a 256 \"$installer\" | /usr/bin/awk '{{print $1}}')\" = {}\n\
         release_asset=\"$staging_dir/release-asset\"\n\
         /usr/bin/printf 'Authorization: Bearer %s\\n' \"$token\" | /usr/bin/curl -fsSL -H @- -H 'Accept: application/octet-stream' --output \"$release_asset\" {}\n\
         release_asset_sha256=\"$(/usr/bin/shasum -a 256 \"$release_asset\" | /usr/bin/awk '{{print $1}}')\"; test \"$release_asset_sha256\" = {}\n\
         auth_helper_source=\"$staging_dir/shipyard-github-app-token\"; auth_wrapper_source=\"$staging_dir/ghapp\"\n\
         /usr/bin/printf 'Authorization: Bearer %s\\n' \"$token\" | /usr/bin/curl -fsSL -H @- --output \"$auth_helper_source\" {}\n\
         /usr/bin/printf 'Authorization: Bearer %s\\n' \"$token\" | /usr/bin/curl -fsSL -H @- --output \"$auth_wrapper_source\" {}\n\
         test \"$(/usr/bin/shasum -a 256 \"$auth_helper_source\" | /usr/bin/awk '{{print $1}}')\" = {}\n\
         test \"$(/usr/bin/shasum -a 256 \"$auth_wrapper_source\" | /usr/bin/awk '{{print $1}}')\" = {}\n\
         curl_shim=\"$staging_dir/curl-exact-asset\"; /usr/bin/printf '%s\\n' {} > \"$curl_shim\"; /bin/chmod 700 \"$curl_shim\"\n\
         SHIPYARD_FLEET_ASSET_PATH=\"$release_asset\" SHIPYARD_GITHUB_TOKEN=\"$token\" SHIPYARD_VERSION={} SHIPYARD_INSTALL_DIR=\"$staging_dir\" SHIPYARD_CURL_BIN=\"$curl_shim\" /bin/bash \"$installer\" >/dev/null\n\
         staged_binary=\"$staging_dir/shipyard\"; test \"$(\"$staged_binary\" --version)\" = {}\n\
         \"$staged_binary\" --mode {} --global-dir {} --state-dir {} update --to {} --check --unattended-fleet >/dev/null\n\
         {}\nunset token\n{}\n{}\n\
         {} --mode {} --global-dir {} --state-dir {} update --to {} --check --unattended-fleet >/dev/null\n\
         refresh_receipt=\"$({} --mode {} --global-dir {} --state-dir {} --json daemon refresh | /usr/bin/tr -d '\\n')\"\n\
         after_status=\"$({} --mode {} --global-dir {} --state-dir {} --json daemon status | /usr/bin/tr -d '\\n')\"\n\
         printf '%s%s\\n' {} \"$before_primary_sha256\"; printf '%s%s\\n' {} \"$before_primary_version\"\n\
         printf '%s%s\\n' {} \"$before_companion_sha256\"; printf '%s%s\\n' {} \"$before_companion_version\"\n\
         printf '%s%s\\n' {} \"$after_primary_sha256\"; printf '%s%s\\n' {} \"$after_primary_version\"\n\
         printf '%s%s\\n' {} \"$after_companion_sha256\"; printf '%s%s\\n' {} \"$after_companion_version\"\n\
         printf '%s%s\\n' {} \"$before_auth_helper_sha256\"; printf '%s%s\\n' {} \"$before_auth_helper_mode\"\n\
         printf '%s%s\\n' {} \"$before_auth_wrapper_sha256\"; printf '%s%s\\n' {} \"$before_auth_wrapper_mode\"\n\
         printf '%s%s\\n' {} \"$after_auth_helper_sha256\"; printf '%s%s\\n' {} \"$after_auth_helper_mode\"\n\
         printf '%s%s\\n' {} \"$after_auth_wrapper_sha256\"; printf '%s%s\\n' {} \"$after_auth_wrapper_mode\"\n\
         printf '%s%s\\n' {} \"$before_status\"; printf '%s%s\\n' {} \"$refresh_receipt\"; printf '%s%s\\n' {} \"$after_status\"\n\
         printf '%s%s\\n' {} {}; printf '%s%s\\n' {} \"$release_asset_sha256\"",
        before_pair,
        before_auth,
        auth_contract,
        shlex_quote(&binary.display().to_string()),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&state_dir.display().to_string()),
        shlex_quote(&auth_wrapper.display().to_string()),
        shlex_quote(&installer_url),
        shlex_quote(&authority.installer.sha256),
        shlex_quote(&release_asset_url),
        shlex_quote(&authority.platform_asset.sha256),
        shlex_quote(&auth_helper_url),
        shlex_quote(&auth_wrapper_url),
        shlex_quote(&authority.auth_helper.sha256),
        shlex_quote(&authority.auth_wrapper.sha256),
        shlex_quote(&exact_asset_curl_shim),
        shlex_quote(version),
        shlex_quote(&format!("shipyard {version}")),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&state_dir.display().to_string()),
        shlex_quote(target),
        auth_transaction,
        after_pair,
        after_auth,
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
        shlex_quote(auth_support::BEFORE_HELPER_SHA_PREFIX),
        shlex_quote(auth_support::BEFORE_HELPER_MODE_PREFIX),
        shlex_quote(auth_support::BEFORE_WRAPPER_SHA_PREFIX),
        shlex_quote(auth_support::BEFORE_WRAPPER_MODE_PREFIX),
        shlex_quote(auth_support::AFTER_HELPER_SHA_PREFIX),
        shlex_quote(auth_support::AFTER_HELPER_MODE_PREFIX),
        shlex_quote(auth_support::AFTER_WRAPPER_SHA_PREFIX),
        shlex_quote(auth_support::AFTER_WRAPPER_MODE_PREFIX),
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
        "https://raw.githubusercontent.com/{}/{}/{}",
        plan.release_authority.repository,
        plan.release_authority.commit_oid,
        plan.release_authority.installer.path
    );
    let release_asset_url = format!(
        "https://api.github.com/repos/{}/releases/assets/{}",
        plan.release_authority.repository, plan.release_authority.platform_asset.id
    );
    let (auth_helper_url, auth_wrapper_url) = auth_support::source_urls(&plan.release_authority);
    let auth_contract = auth_support::wrapper_helper_contract_probe(&plan.auth_helper);
    let curl_shim = exact_asset_curl_shim(&plan.release_authority.platform_asset.name);
    let binary_install_command = format!(
        "SHIPYARD_FLEET_ASSET_PATH=\"$release_asset\" /usr/bin/env -i HOME={} PATH={} SHIPYARD_FLEET_ASSET_PATH=\"$release_asset\" {} --mode {} --global-dir {} --state-dir {} --json update --to {} --install-script-url \"file://$installer\" --curl-bin \"$curl_shim\" --refresh-daemon --unattended-fleet",
        shlex_quote(&home_dir().display().to_string()),
        shlex_quote(&unattended_tool_path().to_string_lossy()),
        shlex_quote(&plan.binary.display().to_string()),
        plan.runtime_mode.as_str(),
        shlex_quote(&plan.global_dir.display().to_string()),
        shlex_quote(&plan.state_dir.display().to_string()),
        shlex_quote(&plan.target),
    );
    let auth_transaction = auth_support::install_transaction(
        &plan.auth_helper,
        &plan.auth_wrapper,
        &plan.binary,
        &plan.companion_binary,
        plan.companion_required,
        "\"$auth_helper_source\"",
        "\"$auth_wrapper_source\"",
        &binary_install_command,
        &plan.state_dir,
        &plan.release_authority,
        false,
    );
    format!(
        "set -euo pipefail; {}; staging_dir=\"$(/usr/bin/mktemp -d)\"; installer=\"$staging_dir/install.sh\"; release_asset=\"$staging_dir/release-asset\"; auth_helper_source=\"$staging_dir/shipyard-github-app-token\"; auth_wrapper_source=\"$staging_dir/ghapp\"; curl_shim=\"$staging_dir/curl-exact-asset\"; trap '/bin/rm -rf \"$staging_dir\"' EXIT; /usr/bin/curl -fsSL --output \"$installer\" {}; test \"$(/usr/bin/shasum -a 256 \"$installer\" | /usr/bin/awk '{{print $1}}')\" = {}; /usr/bin/curl -fsSL -H 'Accept: application/octet-stream' --output \"$release_asset\" {}; test \"$(/usr/bin/shasum -a 256 \"$release_asset\" | /usr/bin/awk '{{print $1}}')\" = {}; /usr/bin/curl -fsSL --output \"$auth_helper_source\" {}; /usr/bin/curl -fsSL --output \"$auth_wrapper_source\" {}; test \"$(/usr/bin/shasum -a 256 \"$auth_helper_source\" | /usr/bin/awk '{{print $1}}')\" = {}; test \"$(/usr/bin/shasum -a 256 \"$auth_wrapper_source\" | /usr/bin/awk '{{print $1}}')\" = {}; /usr/bin/printf '%s\\n' {} > \"$curl_shim\"; /bin/chmod 700 \"$curl_shim\"; {}",
        auth_contract,
        shlex_quote(&installer_url),
        shlex_quote(&plan.release_authority.installer.sha256),
        shlex_quote(&release_asset_url),
        shlex_quote(&plan.release_authority.platform_asset.sha256),
        shlex_quote(&auth_helper_url),
        shlex_quote(&auth_wrapper_url),
        shlex_quote(&plan.release_authority.auth_helper.sha256),
        shlex_quote(&plan.release_authority.auth_wrapper.sha256),
        shlex_quote(&curl_shim),
        auth_transaction,
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
                            "auth_helper": plan.auth_helper,
                            "auth_wrapper": plan.auth_wrapper,
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

#[allow(clippy::too_many_lines)] // Human and JSON receipts intentionally share one field-complete boundary.
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
            "auth_helper".to_owned(),
            Value::from(plan.auth_helper.display().to_string()),
        );
        data.insert(
            "auth_wrapper".to_owned(),
            Value::from(plan.auth_wrapper.display().to_string()),
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
                "auth_support_before".to_owned(),
                serde_json::to_value(&evidence.auth_support_before)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?,
            );
            data.insert(
                "auth_support_after".to_owned(),
                serde_json::to_value(&evidence.auth_support_after)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?,
            );
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
mod tests;
