use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::CliFailure;
use super::cli::{DependencyCommand, PulpDependencyCommand};
use crate::config::LoadedConfig;
use crate::dependency::{
    BuildAttestationReceipt, DependencyChannel, LockTransition, PulpDependencyConfig,
    PulpDependencyLock, ReleaseAssetMetadata, ReleaseAttestationProof, ReleaseMetadata,
    TagIdentity, preflight_pulp_release, qualify_pulp_release, render_lock, sha256_hex,
    validate_lock_transition,
};
use crate::gh::{GhAuthSourceSummary, GhAuthSummary, GhClient, GhSupervision};
use crate::output::write_pretty_json;
use crate::paths::RuntimePaths;

mod consumer_pr;
mod github;

const CACHE_SCHEMA: &str = "shipyard.pulp-qualification-cache.v1";

use self::consumer_pr::{
    ExistingPin, GitHubAppIdentity, PinPr, PinPublication, TemporaryWorktree, atomic_write,
    commit_lock, consumer_repo_slug, create_pin_pr, dependency_branch, ensure_base_unchanged,
    ensure_clean, existing_pin_pr, fetch_base, github_app_identity, push_head,
};
use self::github::{
    AttestationFailure, BuildAttestationContext, build_attestation, latest_release_candidates,
    release_attestation, release_by_tag, release_with_authoritative_assets, tag_identity,
};

pub(super) fn dependency_command<W: Write>(
    command: DependencyCommand,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        DependencyCommand::Pulp { command } => match command {
            PulpDependencyCommand::Show => pulp_show(cwd, json, stdout),
            PulpDependencyCommand::Update { no_pr } => {
                pulp_update(cwd, runtime_paths, no_pr, json, stdout)
            }
            PulpDependencyCommand::Verify => pulp_verify(cwd, runtime_paths, json, stdout),
        },
    }
}

#[derive(Debug, Serialize)]
struct DependencyReport {
    status: String,
    channel: DependencyChannel,
    repository: String,
    tag: Option<String>,
    commit_sha: Option<String>,
    lock_file: PathBuf,
    pr_number: Option<u64>,
    pr_url: Option<String>,
}

fn pulp_show<W: Write>(cwd: &Path, json: bool, stdout: &mut W) -> Result<ExitCode, CliFailure> {
    let repo_root = repo_root(cwd)?;
    let config = PulpDependencyConfig::load_tracked(&repo_root).map_err(failure)?;
    config.validate_lock_location(&repo_root).map_err(failure)?;
    let lock_path = config.lock_path(&repo_root);
    let lock = PulpDependencyLock::read_if_present(&lock_path).map_err(failure)?;
    emit_report(
        stdout,
        json,
        DependencyReport {
            status: if lock.is_some() { "locked" } else { "unlocked" }.to_owned(),
            channel: config.channel,
            repository: config.repository,
            tag: lock.as_ref().map(|value| value.tag.clone()),
            commit_sha: lock.as_ref().map(|value| value.commit_sha.clone()),
            lock_file: config.lock_file,
            pr_number: None,
            pr_url: None,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}

fn pulp_update<W: Write>(
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    no_pr: bool,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if no_pr {
        let repo_root = repo_root(cwd)?;
        return pulp_update_checkout(&repo_root, runtime_paths, json, stdout);
    }
    let local_git = trusted_local_git_client(runtime_paths)?;
    let repo_root = trusted_repo_root(&local_git, cwd)?;
    pulp_update_pr(&repo_root, runtime_paths, &local_git, json, stdout)
}

fn pulp_update_pr<W: Write>(
    repo_root: &Path,
    runtime_paths: &RuntimePaths,
    local_git: &GhClient,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    ensure_clean(local_git, repo_root)?;
    let checkout_config = PulpDependencyConfig::load_tracked(repo_root).map_err(failure)?;
    checkout_config
        .validate_lock_location(repo_root)
        .map_err(failure)?;
    let consumer_repo = consumer_repo_slug(local_git, repo_root)?;
    let consumer_auth = trusted_app_client(runtime_paths, repo_root, &consumer_repo)?;
    let base_sha = fetch_base(
        &consumer_auth.client,
        repo_root,
        &consumer_repo,
        &checkout_config.base_branch,
    )?;

    let temporary = TemporaryWorktree::create(
        &consumer_auth.client,
        &consumer_repo,
        &checkout_config.base_branch,
        &base_sha,
    )?;
    let base_config = PulpDependencyConfig::load_tracked(temporary.path()).map_err(failure)?;
    base_config
        .validate_lock_location(temporary.path())
        .map_err(failure)?;
    if base_config != checkout_config {
        return Err(failure(
            "tracked [dependencies.pulp] policy differs from the fetched base; update from a checkout whose policy matches the target base",
        ));
    }
    let lock_path = base_config.lock_path(temporary.path());
    let current = PulpDependencyLock::read_if_present(&lock_path).map_err(failure)?;
    let upstream_auth =
        trusted_app_client(runtime_paths, temporary.path(), &base_config.repository)?;
    let candidate = resolve_qualified_release(
        &upstream_auth.client,
        temporary.path(),
        &base_config,
        &runtime_paths.state_dir,
        None,
        CachePolicy::Allow,
        current.as_ref(),
    )?;
    let transition = validate_lock_transition(current.as_ref(), &candidate).map_err(failure)?;
    if transition == LockTransition::Unchanged {
        emit_candidate_report(stdout, json, "unchanged", &base_config, &candidate, None)?;
        return Ok(ExitCode::SUCCESS);
    }

    let lock_bytes = render_lock(&candidate).map_err(failure)?;
    let branch = dependency_branch(
        &candidate.tag,
        &candidate.commit_sha,
        &base_sha,
        &lock_bytes,
    );
    ensure_base_unchanged(
        &consumer_auth.client,
        temporary.path(),
        &consumer_repo,
        &base_config.base_branch,
        &base_sha,
    )?;
    let publication = PinPublication {
        client: &consumer_auth.client,
        cwd: temporary.path(),
        repo: &consumer_repo,
        config: &base_config,
        lock: &candidate,
        branch: &branch,
        lock_bytes: &lock_bytes,
        base_sha: &base_sha,
        app: &consumer_auth.identity,
    };
    let existing = existing_pin_pr(&publication)?;
    let pin = PinContext {
        client: &consumer_auth.client,
        app: &consumer_auth.identity,
        cwd: temporary.path(),
        repo: &consumer_repo,
        config: &base_config,
        branch: &branch,
        candidate: &candidate,
        expected_base_sha: &base_sha,
    };
    if reuse_existing_pin(&pin, existing, json, stdout)? {
        return Ok(ExitCode::SUCCESS);
    }

    let pr = publish_new_pin(&pin, &lock_path, &lock_bytes)?;
    emit_candidate_report(
        stdout,
        json,
        "opened-pr",
        &base_config,
        &candidate,
        Some(pr),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn publish_new_pin(
    pin: &PinContext<'_>,
    lock_path: &Path,
    lock_bytes: &[u8],
) -> Result<PinPr, CliFailure> {
    atomic_write(lock_path, lock_bytes).map_err(failure)?;
    commit_lock(
        pin.client,
        pin.cwd,
        &pin.config.lock_file,
        &pin.candidate.tag,
        lock_bytes,
        pin.expected_base_sha,
        pin.app,
    )?;
    ensure_base_unchanged(
        pin.client,
        pin.cwd,
        pin.repo,
        &pin.config.base_branch,
        pin.expected_base_sha,
    )?;
    push_head(pin.client, pin.cwd, pin.repo, pin.branch)?;
    ensure_base_unchanged(
        pin.client,
        pin.cwd,
        pin.repo,
        &pin.config.base_branch,
        pin.expected_base_sha,
    )?;
    create_pin_pr(&PinPublication {
        client: pin.client,
        cwd: pin.cwd,
        repo: pin.repo,
        config: pin.config,
        lock: pin.candidate,
        branch: pin.branch,
        lock_bytes,
        base_sha: pin.expected_base_sha,
        app: pin.app,
    })
}

struct PinContext<'a> {
    client: &'a GhClient,
    app: &'a GitHubAppIdentity,
    cwd: &'a Path,
    repo: &'a str,
    config: &'a PulpDependencyConfig,
    branch: &'a str,
    candidate: &'a PulpDependencyLock,
    expected_base_sha: &'a str,
}

fn reuse_existing_pin<W: Write>(
    pin: &PinContext<'_>,
    existing: ExistingPin,
    json: bool,
    stdout: &mut W,
) -> Result<bool, CliFailure> {
    match existing {
        ExistingPin::Open(pr) => {
            ensure_base_unchanged(
                pin.client,
                pin.cwd,
                pin.repo,
                &pin.config.base_branch,
                pin.expected_base_sha,
            )?;
            emit_candidate_report(
                stdout,
                json,
                "existing-pr",
                pin.config,
                pin.candidate,
                Some(pr),
            )?;
            Ok(true)
        }
        ExistingPin::Absent => Ok(false),
    }
}

fn pulp_update_checkout<W: Write>(
    repo_root: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = PulpDependencyConfig::load_tracked(repo_root).map_err(failure)?;
    config.validate_lock_location(repo_root).map_err(failure)?;
    let lock_path = config.lock_path(repo_root);
    let current = PulpDependencyLock::read_if_present(&lock_path).map_err(failure)?;
    let upstream_auth = trusted_app_client(runtime_paths, repo_root, &config.repository)?;
    let candidate = resolve_qualified_release(
        &upstream_auth.client,
        repo_root,
        &config,
        &runtime_paths.state_dir,
        None,
        CachePolicy::Allow,
        current.as_ref(),
    )?;
    let transition = validate_lock_transition(current.as_ref(), &candidate).map_err(failure)?;
    let status = if transition == LockTransition::Unchanged {
        "unchanged"
    } else {
        let bytes = render_lock(&candidate).map_err(failure)?;
        atomic_write(&lock_path, &bytes).map_err(failure)?;
        "updated-checkout"
    };
    emit_candidate_report(stdout, json, status, &config, &candidate, None)?;
    Ok(ExitCode::SUCCESS)
}

fn pulp_verify<W: Write>(
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let repo_root = repo_root(cwd)?;
    let config = PulpDependencyConfig::load_tracked(&repo_root).map_err(failure)?;
    config.validate_lock_location(&repo_root).map_err(failure)?;
    let lock = PulpDependencyLock::read_if_present(&config.lock_path(&repo_root))
        .map_err(failure)?
        .ok_or_else(|| failure("tracked Pulp dependency lock is missing"))?;
    let upstream_auth = trusted_app_client(runtime_paths, &repo_root, &config.repository)?;
    let verified = resolve_qualified_release(
        &upstream_auth.client,
        &repo_root,
        &config,
        &runtime_paths.state_dir,
        Some(&lock.tag),
        CachePolicy::Bypass,
        Some(&lock),
    )?;
    if lock != verified {
        return Err(failure(
            "fresh GitHub release and build attestations do not reproduce the tracked Pulp lock exactly",
        ));
    }
    emit_candidate_report(stdout, json, "verified", &config, &verified, None)?;
    Ok(ExitCode::SUCCESS)
}

fn trusted_app_client(
    runtime_paths: &RuntimePaths,
    cwd: &Path,
    repo: &str,
) -> Result<TrustedAppClient, CliFailure> {
    let config = LoadedConfig::load_machine_global_from_dir(runtime_paths.global_dir.clone())
        .map_err(|error| failure(error.to_string()))?;
    let mut client = GhClient::from_loaded_config(&config)
        .map_err(|error| failure(error.to_string()))?
        .with_repo_override(repo)
        .map_err(|error| failure(error.to_string()))?;
    let summary = client
        .pin_command_auth(cwd)
        .map_err(|error| failure(format!("failed to resolve Shipyard GitHub auth: {error}")))?;
    validate_github_app_auth(&summary).map_err(failure)?;
    let identity = github_app_identity(&client, cwd)?;
    Ok(TrustedAppClient { client, identity })
}

fn trusted_local_git_client(runtime_paths: &RuntimePaths) -> Result<GhClient, CliFailure> {
    let config = LoadedConfig::load_machine_global_from_dir(runtime_paths.global_dir.clone())
        .map_err(|error| failure(error.to_string()))?;
    GhClient::from_loaded_config(&config).map_err(|error| failure(error.to_string()))
}

fn trusted_repo_root(client: &GhClient, cwd: &Path) -> Result<PathBuf, CliFailure> {
    let output = client
        .prepare_privileged_git_command(cwd)
        .map_err(|error| failure(error.to_string()))?
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| failure(format!("failed to inspect Git checkout: {error}")))?;
    require_success(&output, "trusted git rev-parse --show-toplevel")?;
    let root = String::from_utf8(output.stdout)
        .map_err(|_| failure("Git checkout root is not valid UTF-8"))?;
    Ok(PathBuf::from(root.trim()))
}

struct TrustedAppClient {
    client: GhClient,
    identity: GitHubAppIdentity,
}

fn validate_github_app_auth(summary: &GhAuthSummary) -> Result<(), String> {
    if summary.source != GhAuthSourceSummary::Command
        || summary.token_kind.as_deref() != Some("github-app-installation")
    {
        return Err(
            "Pulp dependency qualification and pin PRs require trusted machine-global Shipyard GitHub App auth (command helper token_kind=github-app-installation)"
                .to_owned(),
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CachePolicy {
    Allow,
    Bypass,
}

#[derive(Debug)]
enum QualificationFailure {
    Rejected(CliFailure),
    Operational(CliFailure),
}

impl QualificationFailure {
    fn rejected(message: impl Into<String>) -> Self {
        Self::Rejected(failure(message))
    }

    fn operational(error: CliFailure) -> Self {
        Self::Operational(error)
    }
}

impl From<AttestationFailure> for QualificationFailure {
    fn from(error: AttestationFailure) -> Self {
        match error {
            AttestationFailure::Rejected(error) => Self::Rejected(error),
            AttestationFailure::Operational(error) => Self::Operational(error),
        }
    }
}

fn resolve_qualified_release(
    client: &GhClient,
    cwd: &Path,
    config: &PulpDependencyConfig,
    state_dir: &Path,
    exact_tag: Option<&str>,
    cache_policy: CachePolicy,
    expected_lock: Option<&PulpDependencyLock>,
) -> Result<PulpDependencyLock, CliFailure> {
    let releases = if let Some(tag) = exact_tag.or_else(|| config.requested_tag()) {
        vec![release_by_tag(client, cwd, &config.repository, tag)?]
    } else {
        latest_release_candidates(client, cwd, &config.repository)?
    };
    let exact = exact_tag.is_some() || config.requested_tag().is_some();
    select_qualified_candidate(releases, exact, |release| {
        let release =
            release_with_authoritative_assets(client, cwd, &config.repository, release.clone())
                .map_err(QualificationFailure::operational)?;
        qualify_candidate(
            client,
            cwd,
            config,
            state_dir,
            &release,
            cache_policy,
            expected_lock,
        )
    })
}

fn select_qualified_candidate<F>(
    releases: Vec<ReleaseMetadata>,
    exact: bool,
    mut qualify: F,
) -> Result<PulpDependencyLock, CliFailure>
where
    F: FnMut(&ReleaseMetadata) -> Result<PulpDependencyLock, QualificationFailure>,
{
    let mut rejected = Vec::new();
    for release in releases {
        match qualify(&release) {
            Ok(lock) => return Ok(lock),
            Err(QualificationFailure::Operational(error)) => {
                return Err(failure(format!(
                    "operational failure while qualifying {}: {}",
                    release.tag_name,
                    error.message()
                )));
            }
            Err(QualificationFailure::Rejected(error)) if exact => return Err(error),
            Err(QualificationFailure::Rejected(error)) => {
                rejected.push(format!("{}: {}", release.tag_name, error.message()));
            }
        }
    }
    let detail = if rejected.is_empty() {
        "no published semantic-version releases were returned".to_owned()
    } else {
        format!("rejected candidates: {}", rejected.join("; "))
    };
    Err(failure(format!(
        "no latest-qualified Pulp release was found; {detail}"
    )))
}

fn qualify_candidate(
    client: &GhClient,
    cwd: &Path,
    config: &PulpDependencyConfig,
    state_dir: &Path,
    release: &ReleaseMetadata,
    cache_policy: CachePolicy,
    expected_lock: Option<&PulpDependencyLock>,
) -> Result<PulpDependencyLock, QualificationFailure> {
    let expected_lock = expected_lock.filter(|lock| lock.tag == release.tag_name);
    let tag = tag_identity(client, cwd, &config.repository, &release.tag_name)
        .map_err(QualificationFailure::operational)?;
    let release_proof = release_attestation(client, cwd, config, release, &tag)
        .map_err(QualificationFailure::from)?;
    let scratch = tempfile::tempdir().map_err(|error| {
        QualificationFailure::operational(failure(format!(
            "failed to create qualification directory: {error}"
        )))
    })?;
    let manifest_asset = release
        .assets
        .iter()
        .find(|asset| asset.name == config.manifest_asset)
        .ok_or_else(|| {
            QualificationFailure::rejected(format!(
                "missing manifest asset {}",
                config.manifest_asset
            ))
        })?;
    let manifest_path = scratch.path().join(&config.manifest_asset);
    download_asset(
        client,
        cwd,
        &config.repository,
        manifest_asset,
        &manifest_path,
    )
    .map_err(QualificationFailure::operational)?;
    let manifest_bytes = fs::read(&manifest_path).map_err(|error| {
        QualificationFailure::operational(failure(format!(
            "failed to read downloaded manifest {}: {error}",
            manifest_path.display()
        )))
    })?;
    preflight_pulp_release(config, release, &tag, &release_proof, &manifest_bytes)
        .map_err(QualificationFailure::rejected)?;
    let key = qualification_cache_key(config, release, &tag, &release_proof, &manifest_bytes)
        .map_err(QualificationFailure::operational)?;
    if cache_policy == CachePolicy::Allow
        && let Some(receipts) = read_cached_receipts(state_dir, &key)
        && let Some(lock) = reusable_cached_lock(
            expected_lock,
            qualify_pulp_release(
                config,
                release,
                &tag,
                &release_proof,
                &manifest_bytes,
                &receipts,
            ),
        )
    {
        return Ok(lock);
    }

    let mut receipts = Vec::with_capacity(config.required_assets.len());
    for name in &config.required_assets {
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == *name)
            .ok_or_else(|| {
                QualificationFailure::rejected(format!("missing required release asset {name}"))
            })?;
        let path = scratch.path().join(name);
        download_asset(client, cwd, &config.repository, asset, &path)
            .map_err(QualificationFailure::operational)?;
        let expected_receipt = expected_build_receipt(expected_lock, name)?;
        let context = BuildAttestationContext {
            config,
            release,
            tag: &tag,
            asset,
            expected_receipt,
        };
        receipts.push(
            build_attestation(client, cwd, &path, &context).map_err(QualificationFailure::from)?,
        );
    }
    let lock = qualify_pulp_release(
        config,
        release,
        &tag,
        &release_proof,
        &manifest_bytes,
        &receipts,
    )
    .map_err(QualificationFailure::rejected)?;
    if cache_policy == CachePolicy::Allow {
        write_cached_receipts(state_dir, &key, &receipts)
            .map_err(QualificationFailure::operational)?;
    }
    Ok(lock)
}

fn expected_build_receipt<'a>(
    expected_lock: Option<&'a PulpDependencyLock>,
    asset: &str,
) -> Result<Option<&'a BuildAttestationReceipt>, QualificationFailure> {
    expected_lock
        .map(|lock| {
            lock.build_attestations
                .iter()
                .find(|receipt| receipt.asset == asset)
                .ok_or_else(|| {
                    QualificationFailure::operational(failure(format!(
                        "tracked dependency lock has no build attestation for {asset}"
                    )))
                })
        })
        .transpose()
}

fn reusable_cached_lock(
    expected_lock: Option<&PulpDependencyLock>,
    cached_lock: Result<PulpDependencyLock, String>,
) -> Option<PulpDependencyLock> {
    let expected = expected_lock?;
    cached_lock
        .ok()
        .filter(|candidate| expected.same_release_identity(candidate))
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct QualificationCache {
    schema: String,
    key: String,
    build_attestations: Vec<BuildAttestationReceipt>,
}

fn qualification_cache_key(
    config: &PulpDependencyConfig,
    release: &ReleaseMetadata,
    tag: &TagIdentity,
    proof: &ReleaseAttestationProof,
    manifest_bytes: &[u8],
) -> Result<String, CliFailure> {
    let mut assets: Vec<_> = release
        .assets
        .iter()
        .map(|asset| {
            serde_json::json!({
                "id": asset.id,
                "name": asset.name,
                "digest": asset.digest,
                "size": asset.size,
                "download_url": asset.download_url,
                "state": asset.state,
            })
        })
        .collect();
    assets.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    let mut required = config.required_assets.clone();
    required.sort();
    let value = serde_json::json!({
        "repository": config.repository,
        "tag": release.tag_name,
        "tag_ref_sha": tag.ref_sha,
        "commit_sha": tag.commit_sha,
        "release_id": release.id,
        "published_at": release.published_at,
        "assets": assets,
        "manifest_asset": config.manifest_asset,
        "manifest_sha256": sha256_hex(manifest_bytes),
        "release_statement_sha256": proof.statement_sha256,
        "signer_workflow": config.signer_workflow,
        "required_assets": required,
    });
    let bytes = serde_json::to_vec(&value)
        .map_err(|error| failure(format!("failed to build qualification cache key: {error}")))?;
    Ok(sha256_hex(&bytes))
}

fn cache_path(state_dir: &Path, key: &str) -> PathBuf {
    state_dir
        .join("dependencies")
        .join("pulp")
        .join("qualification")
        .join(format!("{key}.json"))
}

fn read_cached_receipts(state_dir: &Path, key: &str) -> Option<Vec<BuildAttestationReceipt>> {
    let bytes = fs::read(cache_path(state_dir, key)).ok()?;
    let cache: QualificationCache = serde_json::from_slice(&bytes).ok()?;
    (cache.schema == CACHE_SCHEMA && cache.key == key).then_some(cache.build_attestations)
}

fn write_cached_receipts(
    state_dir: &Path,
    key: &str,
    receipts: &[BuildAttestationReceipt],
) -> Result<(), CliFailure> {
    let path = cache_path(state_dir, key);
    let bytes = serde_json::to_vec_pretty(&QualificationCache {
        schema: CACHE_SCHEMA.to_owned(),
        key: key.to_owned(),
        build_attestations: receipts.to_vec(),
    })
    .map_err(|error| failure(format!("failed to serialize qualification cache: {error}")))?;
    atomic_write(&path, &bytes).map_err(failure)
}

fn download_asset(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    asset: &ReleaseAssetMetadata,
    destination: &Path,
) -> Result<(), CliFailure> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            failure(format!(
                "failed to create asset directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let file = File::create(destination).map_err(|error| {
        failure(format!(
            "failed to create asset destination {}: {error}",
            destination.display()
        ))
    })?;
    let endpoint = format!("repos/{repo}/releases/assets/{}", asset.id);
    let mut command = prepared_gh(client, cwd)?;
    command
        .args(["api", &endpoint, "-H", "Accept: application/octet-stream"])
        .stdout(Stdio::from(file));
    let output = command.output().map_err(|error| {
        failure(format!(
            "failed to download release asset {}: {error}",
            asset.name
        ))
    })?;
    require_success(&output, &format!("download release asset {}", asset.name))?;
    let actual = sha256_file(destination)?;
    let expected = release_asset_sha256(asset).map_err(failure)?;
    if actual != expected {
        return Err(failure(format!(
            "downloaded asset {} digest {actual} does not match GitHub digest {expected}",
            asset.name
        )));
    }
    Ok(())
}

fn release_asset_sha256(asset: &ReleaseAssetMetadata) -> Result<&str, String> {
    asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .ok_or_else(|| format!("release asset {} has no SHA-256 digest", asset.name))
}

fn sha256_file(path: &Path) -> Result<String, CliFailure> {
    let mut file = File::open(path)
        .map_err(|error| failure(format!("failed to hash {}: {error}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| failure(format!("failed to hash {}: {error}", path.display())))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn gh_json<T, I, S>(client: &GhClient, cwd: &Path, args: I) -> Result<T, CliFailure>
where
    T: for<'de> Deserialize<'de>,
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = prepared_gh(client, cwd)?;
    command.args(args);
    let output = command
        .output()
        .map_err(|error| failure(format!("failed to start GitHub CLI: {error}")))?;
    require_success(&output, "GitHub CLI request")?;
    serde_json::from_slice(&output.stdout)
        .map_err(|error| failure(format!("GitHub CLI returned invalid JSON: {error}")))
}

fn prepared_gh(client: &GhClient, cwd: &Path) -> Result<Command, CliFailure> {
    client
        .prepare_privileged_command(cwd, GhSupervision::Unsupervised)
        .map_err(|error| failure(error.to_string()))
}

fn require_success(output: &Output, operation: &str) -> Result<(), CliFailure> {
    if output.status.success() {
        return Ok(());
    }
    Err(failure(format!(
        "{operation} failed: {}",
        output_detail(output)
    )))
}

fn output_detail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    } else {
        stderr
    }
}

fn repo_root(cwd: &Path) -> Result<PathBuf, CliFailure> {
    let output = crate::supervised::git_supervised()
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .map_err(|error| failure(format!("failed to inspect Git checkout: {error}")))?;
    require_success(&output, "git rev-parse --show-toplevel")?;
    let root = String::from_utf8(output.stdout)
        .map_err(|_| failure("Git checkout root is not valid UTF-8"))?;
    Ok(PathBuf::from(root.trim()))
}

fn emit_candidate_report<W: Write>(
    stdout: &mut W,
    json: bool,
    status: &str,
    config: &PulpDependencyConfig,
    lock: &PulpDependencyLock,
    pr: Option<PinPr>,
) -> Result<(), CliFailure> {
    emit_report(
        stdout,
        json,
        DependencyReport {
            status: status.to_owned(),
            channel: config.channel,
            repository: config.repository.clone(),
            tag: Some(lock.tag.clone()),
            commit_sha: Some(lock.commit_sha.clone()),
            lock_file: config.lock_file.clone(),
            pr_number: pr.as_ref().map(|value| value.number),
            pr_url: pr.map(|value| value.url),
        },
    )
}

fn emit_report<W: Write>(
    stdout: &mut W,
    json: bool,
    report: DependencyReport,
) -> Result<(), CliFailure> {
    if json {
        write_pretty_json(stdout, &report).map_err(|error| failure(error.to_string()))
    } else {
        writeln!(stdout, "status: {}", report.status)
            .and_then(|()| writeln!(stdout, "channel: {}", report.channel.as_str()))
            .and_then(|()| writeln!(stdout, "repository: {}", report.repository))
            .and_then(|()| {
                if let Some(tag) = report.tag {
                    writeln!(stdout, "tag: {tag}")
                } else {
                    Ok(())
                }
            })
            .and_then(|()| {
                if let Some(commit) = report.commit_sha {
                    writeln!(stdout, "commit: {commit}")
                } else {
                    Ok(())
                }
            })
            .and_then(|()| writeln!(stdout, "lock: {}", report.lock_file.display()))
            .and_then(|()| {
                if let Some(url) = report.pr_url {
                    writeln!(stdout, "pull request: {url}")
                } else {
                    Ok(())
                }
            })
            .map_err(|error| failure(error.to_string()))
    }
}

fn failure(message: impl Into<String>) -> CliFailure {
    CliFailure::new(1, message)
}

#[cfg(test)]
mod tests;
