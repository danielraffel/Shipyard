//! Governed exact-version rollout for configured Shipyard host classes.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::Value;

mod auth_support;
mod command;
mod controller_lock;
mod evidence;
mod reconcile;
mod reconcile_cmd;
mod release_authority;
mod rollback;
mod verify;

#[cfg(all(test, unix))]
use command::exact_asset_curl_shim;
#[cfg(all(test, target_os = "macos"))]
use command::remote_pair_probe;
use command::{local_update_command, remote_update_command, render_host_result, render_plan};
pub(super) use reconcile_cmd::{
    FleetReconcileArgs, fleet_reconcile_command, fleet_version_doctor_section,
};

#[cfg(all(test, unix))]
use evidence::execute_plan_with_timeout;
#[cfg(test)]
use evidence::{
    AuthSupportEvidence, BinaryEvidence, BinaryPairEvidence, DaemonRuntimeEvidence,
    GenerationEvidence, GenerationMemberEvidence, SourceIdentityBasis, SupportFileEvidence,
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
use crate::paths::RuntimePaths;

const REMOTE_MINIMAL_PATH: &str =
    "/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
const HOST_UPDATE_TIMEOUT: Duration = Duration::from_mins(10);
const REMOTE_UPDATE_TIMEOUT: Duration = Duration::from_mins(9);
const MIN_FLEET_UPDATE_TARGET: [u64; 3] = [0, 137, 0];
const MIN_PAIRED_BINARY_TARGET: [u64; 3] = [0, 127, 0];
/// First target tag whose release publishes no companion binary.
///
/// `None` means every release from `MIN_PAIRED_BINARY_TARGET` onward still
/// publishes the paired companion, which is the current release shape.
///
/// The companion gate is two-sided: below this bound the companion must be
/// present, owned by the invoking user, mode 700, and digest-matched, while at
/// or above it the companion must be *absent*. Arming this therefore has to
/// land in the same change that stops building, packaging, and installing the
/// companion, or every host verifies an absence the installer just wrote.
const FIRST_TAG_WITHOUT_COMPANION: Option<[u64; 3]> = None;
const MIN_AUTH_RESOLVER_TARGET: [u64; 3] = [0, 131, 0];
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
    /// Skip hosts already at (or ahead of) the target instead of reinstalling
    /// them. A reinstall cannot be rolled back and restarts the daemon.
    pub(super) lagging_only: bool,
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
    mode: RuntimeMode,
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
    if !args.apply {
        return run_fleet_update(args, mode, cwd, runtime_paths, json, stdout)
            .map_err(|failure| failure.failure);
    }
    // Apply, verify and any rollback run under one controller lock.
    let Some(_lock) = controller_lock::try_acquire(&runtime_paths.state_dir)
        .map_err(|error| CliFailure::new(1, error))?
    else {
        return Err(CliFailure::new(
            EXIT_CONTROLLER_BUSY,
            "another fleet rollout holds the controller lock; not starting a second one",
        ));
    };
    run_fleet_update(args, mode, cwd, runtime_paths, json, stdout)
        .map_err(|failure| failure.failure)
}

/// Exit code when another rollout holds the controller lock.
pub(super) const EXIT_CONTROLLER_BUSY: u8 = 75;

/// Why a rollout did not complete.
#[derive(Debug)]
pub(super) struct RolloutFailure {
    /// True when the release was refused before any host was touched.
    pub(super) ineligible: bool,
    /// The host that was mutated and could not be restored.
    pub(super) rollback_failed_host: Option<String>,
    pub(super) failure: CliFailure,
}

impl From<CliFailure> for RolloutFailure {
    fn from(failure: CliFailure) -> Self {
        Self {
            ineligible: false,
            rollback_failed_host: None,
            failure,
        }
    }
}

/// Plan or apply a rollout. The caller holds the controller lock for `apply`.
pub(super) fn run_fleet_update<W: Write>(
    args: &FleetUpdateArgs,
    _mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, RolloutFailure> {
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
        )
        .into());
    }
    let selected_classes = select_host_classes(&classes, &args.host_classes, args.all_hosts)?;
    // Eligibility is established once, before the first host can mutate. The
    // verifier independently binds live GitHub tag/release objects, downloaded
    // asset bytes, the checksum manifest, and signed build provenance.
    let release_authority = GitHubReleaseAuthorityVerifier::new(&config, cwd)
        .verify(&target)
        .map_err(|error| RolloutFailure {
            ineligible: true,
            rollback_failed_host: None,
            failure: CliFailure::new(1, format!("fleet release is ineligible: {error}")),
        })?;
    let mut plans = selected_classes
        .iter()
        .map(|class| host_update_plan_with_authority(class, &target, &release_authority))
        .collect::<Result<Vec<_>, _>>()?;
    order_controller_last(&mut plans);

    if !args.apply {
        render_plan(stdout, json, &target, &plans, args.all_hosts)?;
        return Ok(ExitCode::SUCCESS);
    }

    let mut skipped_current = Vec::new();
    if args.lagging_only {
        let (lagging, current) = partition_lagging(plans, &target, |plan| {
            reconcile::probe_version_at(
                &plan.class,
                plan.ssh.as_deref(),
                Some(&plan.binary.display().to_string()),
                None,
            )
        })?;
        plans = lagging;
        skipped_current = current;
    }

    let mut ops = LiveOps {
        config: &config,
        cwd,
        classes: &classes,
    };
    apply_plans(&plans, &skipped_current, &target, json, stdout, &mut ops)
}

/// Keep only the hosts behind `target`. A host at or ahead of it is skipped
/// (never reinstalled, never downgraded); an unreadable host fails closed
/// before any host is touched.
fn partition_lagging<P>(
    plans: Vec<HostUpdatePlan>,
    target: &str,
    mut probe: P,
) -> Result<(Vec<HostUpdatePlan>, Vec<String>), CliFailure>
where
    P: FnMut(&HostUpdatePlan) -> reconcile::HostVersion,
{
    let target_version = reconcile::parse_version(target)
        .ok_or_else(|| CliFailure::new(2, format!("target {target} is not vMAJOR.MINOR.PATCH")))?;
    let mut lagging = Vec::new();
    let mut current = Vec::new();
    for plan in plans {
        let probed = probe(&plan);
        let installed = probed
            .version
            .as_deref()
            .and_then(reconcile::parse_version)
            .ok_or_else(|| {
                CliFailure::new(
                    reconcile::EXIT_RECONCILE_UNKNOWN,
                    format!(
                        "cannot read the installed version on {} ({}); refusing to decide which hosts lag",
                        plan.class,
                        probed.error.as_deref().unwrap_or("unparseable")
                    ),
                )
            })?;
        if installed < target_version {
            lagging.push(plan);
        } else {
            current.push(plan.class.clone());
        }
    }
    Ok((lagging, current))
}

struct LiveOps<'a> {
    config: &'a LoadedConfig,
    cwd: &'a Path,
    classes: &'a [HostClassConfig],
}

impl HostOps for LiveOps<'_> {
    fn installed_version(&mut self, plan: &HostUpdatePlan) -> Option<String> {
        reconcile::probe_version_at(
            &plan.class,
            plan.ssh.as_deref(),
            Some(&plan.binary.display().to_string()),
            None,
        )
        .version
    }

    fn execute(&mut self, plan: &HostUpdatePlan) -> Result<HostUpdateEvidence, PlanExecutionError> {
        execute_plan(plan)
    }

    fn verify(&mut self, plan: &HostUpdatePlan, daemon_pid: u32) -> verify::HostVerification {
        verify::verify_host(plan, daemon_pid)
    }

    fn rollback(
        &mut self,
        plan: &HostUpdatePlan,
        previous: Option<&str>,
    ) -> rollback::RollbackOutcome {
        rollback_host(self.config, self.cwd, self.classes, plan, previous)
    }
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
            lagging_only: false,
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

/// The side effects of rolling one host, so the ordering and failure policy in
/// [`apply_plans`] can be tested without GitHub or hosts.
trait HostOps {
    /// The version installed before this rollout touches the host, read
    /// independently of the update transaction. `None` when unreadable.
    fn installed_version(&mut self, plan: &HostUpdatePlan) -> Option<String>;
    fn execute(&mut self, plan: &HostUpdatePlan) -> Result<HostUpdateEvidence, PlanExecutionError>;
    fn verify(&mut self, plan: &HostUpdatePlan, daemon_pid: u32) -> verify::HostVerification;
    /// Restore `previous` (the pre-update version) on this host.
    fn rollback(
        &mut self,
        plan: &HostUpdatePlan,
        previous: Option<&str>,
    ) -> rollback::RollbackOutcome;
}

/// Exit code when a host could not be rolled back and needs an operator.
pub(super) const EXIT_ROLLBACK_FAILED: u8 = 7;

/// Why one host stopped the rollout.
struct HostFailure {
    reason: String,
    /// Set when the host was mutated, then could not be restored.
    rollback_failed: bool,
}

/// Apply every plan in order, then independently verify each host.
///
/// Hosts are updated one at a time and the rollout stops at the first host
/// whose update or verification fails, so one bad release cannot spread. A
/// host whose transaction may already have committed (evidence rejected, pair
/// hashes disagreeing, a timeout, or a failed verification) is rolled back to
/// the version it ran before. Every outcome ends in a `fleet_summary` receipt
/// naming verified hosts, the failed host, hosts never attempted (and so still
/// lagging), and hosts skipped because they were already current.
fn apply_plans<W: Write, O: HostOps>(
    plans: &[HostUpdatePlan],
    skipped_current: &[String],
    target: &str,
    json: bool,
    stdout: &mut W,
    ops: &mut O,
) -> Result<ExitCode, RolloutFailure> {
    let mut installed_pair_sha256: Option<(String, Option<String>)> = None;
    let mut verified: Vec<verify::HostVerification> = Vec::new();
    for (index, plan) in plans.iter().enumerate() {
        let failure = apply_host(
            plan,
            target,
            json,
            stdout,
            ops,
            &mut installed_pair_sha256,
            &mut verified,
        )?;
        if let Some(failure) = failure {
            let not_attempted = plans[index + 1..]
                .iter()
                .map(|plan| plan.class.clone())
                .collect::<Vec<_>>();
            render_fleet_summary(
                stdout,
                json,
                target,
                &FleetSummary {
                    verified: &verified,
                    failed: Some((plan.class.as_str(), failure.reason.as_str())),
                    rollback_failed: failure.rollback_failed,
                    not_attempted: &not_attempted,
                    skipped_current,
                },
            )?;
            let lagging = std::iter::once(plan.class.clone())
                .chain(not_attempted)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(RolloutFailure {
                ineligible: false,
                rollback_failed_host: failure.rollback_failed.then(|| plan.class.clone()),
                failure: CliFailure::new(
                    if failure.rollback_failed {
                        EXIT_ROLLBACK_FAILED
                    } else {
                        1
                    },
                    format!(
                        "fleet update stopped after {}; hosts not verified at {target}: {lagging}",
                        failure.reason
                    ),
                ),
            });
        }
    }
    render_fleet_summary(
        stdout,
        json,
        target,
        &FleetSummary {
            verified: &verified,
            failed: None,
            rollback_failed: false,
            not_attempted: &[],
            skipped_current,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}

/// Roll back a host whose transaction may have committed, and turn the result
/// into the failure that stops the rollout.
fn roll_back_after<W: Write, O: HostOps>(
    plan: &HostUpdatePlan,
    target: &str,
    json: bool,
    stdout: &mut W,
    ops: &mut O,
    previous: Option<&str>,
    cause: &str,
) -> Result<Option<HostFailure>, CliFailure> {
    let outcome = ops.rollback(plan, previous);
    render_rollback(stdout, json, target, plan, &outcome)?;
    Ok(Some(HostFailure {
        reason: format!("{cause}; {}", outcome.summary()),
        rollback_failed: !matches!(outcome, rollback::RollbackOutcome::RolledBack { .. }),
    }))
}

/// Update, validate and verify one host. `Ok(Some(_))` stops the rollout.
fn apply_host<W: Write, O: HostOps>(
    plan: &HostUpdatePlan,
    target: &str,
    json: bool,
    stdout: &mut W,
    ops: &mut O,
    installed_pair_sha256: &mut Option<(String, Option<String>)>,
    verified: &mut Vec<verify::HostVerification>,
) -> Result<Option<HostFailure>, CliFailure> {
    // Read before mutating, so a rollback target exists even when the update
    // never returns evidence (a timeout).
    let before = ops.installed_version(plan);
    let evidence = match ops.execute(plan) {
        Ok(evidence) => evidence,
        Err(PlanExecutionError::TimedOut(error)) => {
            render_host_result(stdout, json, target, plan, false, None, Some(&error))?;
            // The transaction may have committed before the deadline.
            return roll_back_after(
                plan,
                target,
                json,
                stdout,
                ops,
                before.as_deref(),
                &format!("{} timed out: {error}", plan.class),
            );
        }
        Err(PlanExecutionError::Failed(error)) => {
            render_host_result(stdout, json, target, plan, false, None, Some(&error))?;
            return Ok(Some(HostFailure {
                reason: format!("{} failed: {error}", plan.class),
                rollback_failed: false,
            }));
        }
    };
    let previous = before.or_else(|| Some(evidence.before_pair.primary.semantic_version.clone()));
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
        return roll_back_after(
            plan,
            target,
            json,
            stdout,
            ops,
            previous.as_deref(),
            &format!("{} evidence failed: {error}", plan.class),
        );
    }
    let observed_pair = (
        evidence.after_pair.primary.sha256.clone(),
        evidence
            .after_pair
            .companion
            .as_ref()
            .map(|companion| companion.sha256.clone()),
    );
    if let Some(expected_pair) = installed_pair_sha256.as_ref()
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
        return roll_back_after(
            plan,
            target,
            json,
            stdout,
            ops,
            previous.as_deref(),
            &format!("{} evidence failed: {detail}", plan.class),
        );
    }
    installed_pair_sha256.get_or_insert(observed_pair);
    render_host_result(stdout, json, target, plan, true, Some(&evidence), None)?;
    let verification = ops.verify(plan, evidence.daemon_pid);
    render_verification(stdout, json, target, &verification)?;
    if verification.verified() {
        verified.push(verification);
        return Ok(None);
    }
    roll_back_after(
        plan,
        target,
        json,
        stdout,
        ops,
        previous.as_deref(),
        &format!(
            "{} failed post-rollout verification: {}",
            plan.class,
            verification.failures.join("; ")
        ),
    )
}

fn render_rollback<W: Write>(
    stdout: &mut W,
    json: bool,
    target: &str,
    plan: &HostUpdatePlan,
    outcome: &rollback::RollbackOutcome,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("host_rollback"));
        data.insert("target".to_owned(), Value::from(target));
        data.insert("host_class".to_owned(), Value::from(plan.class.clone()));
        data.insert("summary".to_owned(), Value::from(outcome.summary()));
        data.insert(
            "outcome".to_owned(),
            serde_json::to_value(outcome).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        write_json_envelope(stdout, "runner.fleet-update", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "{}: {}", plan.class, outcome.summary())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

/// Reinstall the version a host ran before this update, through the same
/// governed path, and verify it.
fn rollback_host(
    config: &LoadedConfig,
    cwd: &Path,
    classes: &[HostClassConfig],
    plan: &HostUpdatePlan,
    previous: Option<&str>,
) -> rollback::RollbackOutcome {
    let tag = match rollback::rollback_target(plan, previous) {
        Ok(tag) => tag,
        Err(reason) => return rollback::RollbackOutcome::Failed { to: None, reason },
    };
    let failed = |reason: String| rollback::RollbackOutcome::Failed {
        to: Some(tag.clone()),
        reason,
    };
    let Some(class) = classes.iter().find(|class| class.class == plan.class) else {
        return failed("host class vanished from configuration".to_owned());
    };
    let authority = match GitHubReleaseAuthorityVerifier::new(config, cwd).verify(&tag) {
        Ok(authority) => authority,
        Err(error) => return failed(format!("previous release is ineligible: {error}")),
    };
    let rollback_plan = match host_update_plan_with_authority(class, &tag, &authority) {
        Ok(plan) => plan,
        Err(error) => return failed(error.message),
    };
    let rollback_evidence = match execute_plan(&rollback_plan) {
        Ok(evidence) => evidence,
        Err(PlanExecutionError::TimedOut(error) | PlanExecutionError::Failed(error)) => {
            return failed(error);
        }
    };
    if let Err(error) = validate_evidence(&rollback_plan, &rollback_evidence) {
        return failed(format!("rollback evidence failed: {error}"));
    }
    let verification = verify::verify_host(&rollback_plan, rollback_evidence.daemon_pid);
    if verification.verified() {
        rollback::RollbackOutcome::RolledBack {
            to: tag,
            verification: Box::new(verification),
        }
    } else {
        failed(format!(
            "rollback did not verify: {}",
            verification.failures.join("; ")
        ))
    }
}

/// The controller's own host (no `ssh`) always goes last, whatever order the
/// configuration or the command line gave. Updating it restarts the daemon of
/// the machine running the rollout; every remote host is done by then.
fn order_controller_last(plans: &mut [HostUpdatePlan]) {
    plans.sort_by_key(|plan| plan.ssh.is_none());
}

fn render_verification<W: Write>(
    stdout: &mut W,
    json: bool,
    target: &str,
    verification: &verify::HostVerification,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("host_verification"));
        data.insert("target".to_owned(), Value::from(target));
        let Value::Object(fields) = serde_json::to_value(verification)
            .map_err(|error| CliFailure::new(1, error.to_string()))?
        else {
            return Err(CliFailure::new(1, "verification receipt must be an object"));
        };
        data.extend(fields);
        write_json_envelope(stdout, "runner.fleet-update", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "{}: verification {} (cli={}, daemon={}, pid={}, guards={}{})",
            verification.host_class,
            verification.verdict.to_uppercase(),
            verification.cli_version.as_deref().unwrap_or("unread"),
            verification.daemon_version.as_deref().unwrap_or("unread"),
            verification
                .daemon_pid
                .map_or_else(|| "unread".to_owned(), |pid| pid.to_string()),
            verification.guards,
            if verification.failures.is_empty() {
                String::new()
            } else {
                format!("; {}", verification.failures.join("; "))
            } + &if verification.guards_replaced.is_empty() {
                String::new()
            } else {
                format!(
                    "; replaced differing guards: {}",
                    verification.guards_replaced.join(", ")
                )
            }
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

struct FleetSummary<'a> {
    verified: &'a [verify::HostVerification],
    failed: Option<(&'a str, &'a str)>,
    rollback_failed: bool,
    not_attempted: &'a [String],
    skipped_current: &'a [String],
}

fn render_fleet_summary<W: Write>(
    stdout: &mut W,
    json: bool,
    target: &str,
    summary: &FleetSummary<'_>,
) -> Result<(), CliFailure> {
    let verified_names = summary
        .verified
        .iter()
        .map(|host| host.host_class.clone())
        .collect::<Vec<_>>();
    let io = |error: std::io::Error| CliFailure::new(1, error.to_string());
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("fleet_summary"));
        data.insert("target".to_owned(), Value::from(target));
        data.insert(
            "verdict".to_owned(),
            Value::from(match (summary.failed, summary.rollback_failed) {
                (None, _) => "verified",
                (Some(_), false) => "failed",
                (Some(_), true) => "rollback_failed",
            }),
        );
        data.insert("verified_hosts".to_owned(), Value::from(verified_names));
        data.insert(
            "failed_host".to_owned(),
            summary.failed.map_or(Value::Null, |(host, reason)| {
                serde_json::json!({
                    "host_class": host,
                    "reason": reason,
                    "needs_operator": summary.rollback_failed,
                })
            }),
        );
        data.insert(
            "not_attempted_hosts".to_owned(),
            Value::from(summary.not_attempted.to_vec()),
        );
        data.insert(
            "already_current_hosts".to_owned(),
            Value::from(summary.skipped_current.to_vec()),
        );
        write_json_envelope(stdout, "runner.fleet-update", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if let Some((host, reason)) = summary.failed {
        writeln!(
            stdout,
            "fleet {target}: FAILED at {host} ({reason}); verified: [{}]; not attempted: [{}]; already current: [{}]",
            verified_names.join(", "),
            summary.not_attempted.join(", "),
            summary.skipped_current.join(", ")
        )
        .map_err(io)?;
    } else {
        writeln!(
            stdout,
            "fleet {target}: every host verified: [{}]; already current: [{}]",
            verified_names.join(", "),
            summary.skipped_current.join(", ")
        )
        .map_err(io)?;
    }
    Ok(())
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
            "fleet-update requires v0.137.0 or newer with the authenticated sibling-guard auth generation capability; use the older release's documented manual rollback procedure",
        ));
    }
    Ok(format!("v{version}"))
}

fn tag_requires_companion(tag: &str) -> bool {
    companion_required_for_tag(tag, MIN_PAIRED_BINARY_TARGET, FIRST_TAG_WITHOUT_COMPANION)
}

/// Companion pairing covers the half-open tag range `[minimum, removed_at)`.
///
/// A target below `minimum` predates paired releases and a target at or above
/// `removed_at` postdates them; both are unpaired, and the callers treat an
/// unpaired target as one whose companion must be absent.
fn companion_required_for_tag(tag: &str, minimum: [u64; 3], removed_at: Option<[u64; 3]>) -> bool {
    tag_at_least(tag, minimum) && !removed_at.is_some_and(|bound| tag_at_least(tag, bound))
}

fn tag_supports_auth_resolver(tag: &str) -> bool {
    tag_at_least(tag, MIN_AUTH_RESOLVER_TARGET)
}

fn tag_at_least(tag: &str, minimum: [u64; 3]) -> bool {
    let parsed = tag
        .trim_start_matches('v')
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>();
    parsed.is_ok_and(|parts| parts.as_slice() >= minimum.as_slice())
}

#[allow(clippy::too_many_lines)] // One fail-closed validation boundary for the complete host profile.
fn host_update_plan_with_authority(
    class: &HostClassConfig,
    target: &str,
    release_authority: &ReleaseAuthority,
) -> Result<HostUpdatePlan, CliFailure> {
    let normalized_target = normalize_exact_tag(target)?;
    if normalized_target != target {
        return Err(CliFailure::new(
            2,
            "fleet-update plan target must be a normalized exact stable tag",
        ));
    }
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
    let auth_guard = state_dir.join("fleet-auth-support.guard");
    let auth_context = PathBuf::from(format!("{}.shipyard-context.json", auth_wrapper.display()));
    let mut managed_targets = vec![
        auth_helper.clone(),
        auth_wrapper.clone(),
        binary.clone(),
        companion_binary.clone(),
    ];
    if tag_supports_auth_resolver(target) {
        managed_targets.push(auth_context);
    }
    let mut transaction_paths = Vec::with_capacity(managed_targets.len() * 4);
    for target in managed_targets {
        transaction_paths.push(target.clone());
        transaction_paths.extend(transaction_marker_paths(&target));
    }
    let mut unique_paths = std::collections::HashSet::new();
    if transaction_paths
        .iter()
        .any(|path| !unique_paths.insert(path.clone()))
        || transaction_paths.iter().any(|path| {
            path == &auth_journal || path.starts_with(&auth_lock) || path.starts_with(&auth_guard)
        })
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

fn transaction_marker_paths(path: &Path) -> [PathBuf; 3] {
    let marker = |suffix: &str| {
        let mut value = path.as_os_str().to_os_string();
        value.push(suffix);
        PathBuf::from(value)
    };
    [
        marker(".shipyard-rollback"),
        marker(".shipyard-rollback.tmp"),
        marker(".shipyard-was-absent"),
    ]
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
        pr_close_guard: release_authority::SourceFileAuthority {
            path: "scripts/ghapp_pr_close_guard.py".to_owned(),
            blob_oid: "f".repeat(40),
            sha256: "0".repeat(64),
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
