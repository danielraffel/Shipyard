//! Typed, immutable dependency-channel policy and lock materialization.
//!
//! This module deliberately contains no network or Git mutation. The CLI
//! adapter gathers cryptographically verified GitHub evidence, then passes it
//! through these fail-closed checks before a consumer lock may be written.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

mod validation;

use self::validation::{
    default_base_branch, default_lock_file, default_manifest_asset, reject_present, required,
    validate_actions_invocation, validate_asset_name, validate_branch_name, validate_git_sha,
    validate_relative_lock_path, validate_release_tag, validate_repo_slug, validate_sha256,
    validate_signer_workflow, version_tuple,
};

const LOCK_SCHEMA: &str = "shipyard.pulp-dependency-lock.v1";
const RELEASE_PREDICATE: &str = "https://in-toto.io/attestation/release/v0.2";
const BUILD_PREDICATE: &str = "https://slsa.dev/provenance/v1";

/// How a consumer follows Pulp releases.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DependencyChannel {
    /// Follow the newest release that passes every configured qualification.
    LatestQualified,
    /// Follow one explicitly reviewed and promoted release tag.
    Stable,
    /// Freeze one exact release tag and peeled source commit.
    Fixed,
}

impl DependencyChannel {
    /// Stable configuration spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LatestQualified => "latest-qualified",
            Self::Stable => "stable",
            Self::Fixed => "fixed",
        }
    }
}

/// Tracked `[dependencies.pulp]` policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PulpDependencyConfig {
    /// Upstream GitHub owner/repository slug.
    pub repository: String,
    /// Release-following policy. This is intentionally required: Shipyard does
    /// not silently opt unrelated repositories into latest-qualified.
    pub channel: DependencyChannel,
    /// Complete set of SDK assets this consumer needs qualified.
    pub required_assets: Vec<String>,
    /// Release checksum manifest asset.
    #[serde(default = "default_manifest_asset")]
    pub manifest_asset: String,
    /// Exact workflow identity accepted for build provenance attestations.
    pub signer_workflow: String,
    /// Tracked immutable lock written into the consumer repository.
    #[serde(default = "default_lock_file")]
    pub lock_file: PathBuf,
    /// Explicit promotion selected by the stable channel.
    pub stable_tag: Option<String>,
    /// Exact release selected by the fixed channel.
    pub fixed_tag: Option<String>,
    /// Exact peeled source commit selected by the fixed channel.
    pub fixed_commit: Option<String>,
    /// Consumer pull-request base branch.
    #[serde(default = "default_base_branch")]
    pub base_branch: String,
}

#[derive(Debug, Deserialize)]
struct TrackedConfig {
    dependencies: TrackedDependencies,
}

#[derive(Debug, Deserialize)]
struct TrackedDependencies {
    pulp: PulpDependencyConfig,
}

impl PulpDependencyConfig {
    /// Load only the tracked project policy, never a global or machine-local
    /// overlay. Dependency selection is repository-reviewed policy.
    pub fn load_tracked(repo_root: &Path) -> Result<Self, String> {
        let project_dir = repo_root.join(".shipyard");
        reject_symlink(&project_dir, "tracked Shipyard project directory")?;
        let path = project_dir.join("config.toml");
        reject_symlink(&path, "tracked Shipyard config")?;
        let text = fs::read_to_string(&path).map_err(|error| {
            format!(
                "failed to read tracked dependency policy {}: {error}",
                path.display()
            )
        })?;
        let tracked: TrackedConfig = toml::from_str(&text).map_err(|error| {
            format!(
                "invalid tracked [dependencies.pulp] policy in {}: {error}",
                path.display()
            )
        })?;
        let config = tracked.dependencies.pulp;
        config.validate()?;
        Ok(config)
    }

    /// Validate that the policy is explicit, immutable, and path-safe.
    pub fn validate(&self) -> Result<(), String> {
        validate_repo_slug(&self.repository)?;
        if self.required_assets.is_empty() {
            return Err("dependencies.pulp.required_assets must not be empty".to_owned());
        }
        let mut names = BTreeSet::new();
        for asset in &self.required_assets {
            validate_asset_name(asset, "required asset")?;
            if asset == &self.manifest_asset {
                return Err(format!(
                    "manifest asset {asset:?} cannot also be a build-provenance asset"
                ));
            }
            if !names.insert(asset) {
                return Err(format!("duplicate required asset {asset:?}"));
            }
        }
        validate_asset_name(&self.manifest_asset, "manifest asset")?;
        validate_relative_lock_path(&self.lock_file)?;
        validate_branch_name(&self.base_branch)?;

        validate_signer_workflow(&self.repository, &self.signer_workflow)?;

        match self.channel {
            DependencyChannel::LatestQualified => {
                reject_present(self.stable_tag.as_deref(), "stable_tag", self.channel)?;
                reject_present(self.fixed_tag.as_deref(), "fixed_tag", self.channel)?;
                reject_present(self.fixed_commit.as_deref(), "fixed_commit", self.channel)?;
            }
            DependencyChannel::Stable => {
                validate_release_tag(required(self.stable_tag.as_deref(), "stable_tag")?)?;
                reject_present(self.fixed_tag.as_deref(), "fixed_tag", self.channel)?;
                reject_present(self.fixed_commit.as_deref(), "fixed_commit", self.channel)?;
            }
            DependencyChannel::Fixed => {
                validate_release_tag(required(self.fixed_tag.as_deref(), "fixed_tag")?)?;
                validate_git_sha(
                    required(self.fixed_commit.as_deref(), "fixed_commit")?,
                    "fixed_commit",
                )?;
                reject_present(self.stable_tag.as_deref(), "stable_tag", self.channel)?;
            }
        }
        Ok(())
    }

    /// Exact tag requested by stable/fixed, or `None` for a scan.
    #[must_use]
    pub fn requested_tag(&self) -> Option<&str> {
        match self.channel {
            DependencyChannel::LatestQualified => None,
            DependencyChannel::Stable => self.stable_tag.as_deref(),
            DependencyChannel::Fixed => self.fixed_tag.as_deref(),
        }
    }

    /// Resolve the tracked lock path below `repo_root`.
    #[must_use]
    pub fn lock_path(&self, repo_root: &Path) -> PathBuf {
        repo_root.join(&self.lock_file)
    }

    /// Reject a lock path whose existing components escape through symlinks.
    pub fn validate_lock_location(&self, repo_root: &Path) -> Result<(), String> {
        let mut path = repo_root.to_path_buf();
        for component in self.lock_file.components() {
            path.push(component.as_os_str());
            reject_symlink(&path, "dependency lock path")?;
        }
        Ok(())
    }
}

/// GitHub REST release metadata used during qualification.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ReleaseMetadata {
    /// Stable GitHub release database id.
    pub id: u64,
    /// Exact semantic-version tag.
    pub tag_name: String,
    /// Draft releases never qualify.
    pub draft: bool,
    /// Prereleases never qualify.
    pub prerelease: bool,
    /// Publication timestamp from GitHub.
    pub published_at: Option<String>,
    /// Complete published asset inventory.
    pub assets: Vec<ReleaseAssetMetadata>,
}

/// One GitHub release asset.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ReleaseAssetMetadata {
    /// Stable GitHub asset database id.
    pub id: u64,
    /// Published basename.
    pub name: String,
    /// GitHub upload state.
    pub state: String,
    /// GitHub-computed digest (`sha256:<hex>`).
    pub digest: Option<String>,
    /// Published byte count.
    pub size: u64,
    /// Immutable versioned download URL.
    #[serde(rename = "browser_download_url")]
    pub download_url: String,
}

/// Exact tag ref and the commit it peels to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TagIdentity {
    /// Git tag object SHA (or commit SHA for a lightweight tag).
    pub ref_sha: String,
    /// Fully peeled source commit SHA.
    pub commit_sha: String,
}

/// Receipt from `gh release verify` after GitHub's cryptographic check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseAttestationProof {
    /// Verified predicate type.
    pub predicate_type: String,
    /// SHA-256 of the decoded signed statement payload.
    pub statement_sha256: String,
    /// Release database id claimed by the statement.
    pub release_id: u64,
    /// Release tag claimed by the statement.
    pub tag: String,
    /// Git tag object SHA claimed by the statement.
    pub ref_sha: String,
    /// Complete asset name to SHA-256 map claimed by the statement.
    pub asset_digests: BTreeMap<String, String>,
}

/// Receipt from `gh attestation verify` after SLSA provenance verification.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildAttestationReceipt {
    /// Asset whose bytes were verified.
    pub asset: String,
    /// Verified subject SHA-256.
    pub subject_sha256: String,
    /// Verified predicate type.
    pub predicate_type: String,
    /// Exact signer workflow policy.
    pub signer_workflow: String,
    /// Verified source repository.
    pub source_repository: String,
    /// Verified source tag ref.
    pub source_ref: String,
    /// Verified source commit.
    pub source_commit: String,
    /// SHA-256 of the decoded signed provenance statement.
    pub statement_sha256: String,
    /// GitHub Actions invocation URI carried by the verified statement.
    pub invocation_uri: String,
}

/// Immutable release asset written to a consumer lock.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedReleaseAsset {
    /// Stable GitHub asset database id.
    pub id: u64,
    /// Published basename.
    pub name: String,
    /// SHA-256 of the exact bytes.
    pub sha256: String,
    /// Published byte count.
    pub size: u64,
    /// Immutable versioned download URL.
    pub download_url: String,
}

/// Manifest identity written to a consumer lock.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedManifest {
    /// Manifest asset basename.
    pub name: String,
    /// SHA-256 of the exact manifest bytes.
    pub sha256: String,
}

/// GitHub immutable-release attestation identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedReleaseAttestation {
    /// Verified predicate type.
    pub predicate_type: String,
    /// SHA-256 of the signed release statement.
    pub statement_sha256: String,
}

/// Deterministic, tracked Pulp dependency lock.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PulpDependencyLock {
    /// Lock schema identity.
    pub schema: String,
    /// Dependency name.
    pub dependency: String,
    /// Policy that selected the release.
    pub channel: DependencyChannel,
    /// Upstream repository.
    pub repository: String,
    /// Exact release tag.
    pub tag: String,
    /// Exact tag object (or lightweight ref) SHA.
    pub tag_ref_sha: String,
    /// Exact fully peeled source commit.
    pub commit_sha: String,
    /// Stable GitHub release id.
    pub release_id: u64,
    /// Publication timestamp copied from GitHub.
    pub published_at: String,
    /// Complete release asset inventory, not only the consumer subset.
    pub release_assets: Vec<LockedReleaseAsset>,
    /// Checksum manifest identity.
    pub manifest: LockedManifest,
    /// Immutable GitHub-release attestation identity.
    pub release_attestation: LockedReleaseAttestation,
    /// Build-provenance receipts for every configured required SDK asset.
    pub build_attestations: Vec<BuildAttestationReceipt>,
}

impl PulpDependencyLock {
    /// Read and validate a tracked lock if present.
    pub fn read_if_present(path: &Path) -> Result<Option<Self>, String> {
        reject_symlink(path, "dependency lock")?;
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path).map_err(|error| {
            format!("failed to read dependency lock {}: {error}", path.display())
        })?;
        let lock: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid dependency lock {}: {error}", path.display()))?;
        lock.validate()?;
        Ok(Some(lock))
    }

    /// Validate invariants that must remain true even for a hand-edited lock.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != LOCK_SCHEMA || self.dependency != "pulp" {
            return Err("unsupported Pulp dependency lock schema or identity".to_owned());
        }
        validate_repo_slug(&self.repository)?;
        validate_release_tag(&self.tag)?;
        if self.release_id == 0 || self.published_at.is_empty() {
            return Err("dependency lock has no release id or publication timestamp".to_owned());
        }
        validate_git_sha(&self.tag_ref_sha, "tag_ref_sha")?;
        validate_git_sha(&self.commit_sha, "commit_sha")?;
        validate_sha256(&self.manifest.sha256, "manifest.sha256")?;
        validate_sha256(
            &self.release_attestation.statement_sha256,
            "release_attestation.statement_sha256",
        )?;
        if self.release_attestation.predicate_type != RELEASE_PREDICATE {
            return Err("unexpected release attestation predicate".to_owned());
        }
        if self.release_assets.is_empty() || self.build_attestations.is_empty() {
            return Err(
                "dependency lock is missing release assets or build attestations".to_owned(),
            );
        }
        let mut asset_names = BTreeSet::new();
        for asset in &self.release_assets {
            validate_asset_name(&asset.name, "locked asset")?;
            validate_sha256(&asset.sha256, "asset sha256")?;
            if asset.id == 0 || asset.size == 0 || !asset_names.insert(asset.name.as_str()) {
                return Err(format!(
                    "locked asset {} has an invalid id, size, or duplicate name",
                    asset.name
                ));
            }
            let expected_url = format!(
                "https://github.com/{}/releases/download/{}/{}",
                self.repository, self.tag, asset.name
            );
            if asset.download_url != expected_url {
                return Err(format!(
                    "locked asset {} has no immutable versioned GitHub URL",
                    asset.name
                ));
            }
        }
        validate_asset_name(&self.manifest.name, "manifest asset")?;
        let manifest_asset = self
            .release_assets
            .iter()
            .find(|asset| asset.name == self.manifest.name)
            .ok_or_else(|| "dependency lock manifest is absent from release assets".to_owned())?;
        if manifest_asset.sha256 != self.manifest.sha256 {
            return Err("dependency lock manifest digest disagrees with release assets".to_owned());
        }
        let mut attested_assets = BTreeSet::new();
        for receipt in &self.build_attestations {
            if receipt.predicate_type != BUILD_PREDICATE {
                return Err(format!(
                    "{} has unexpected build attestation predicate",
                    receipt.asset
                ));
            }
            validate_sha256(&receipt.subject_sha256, "attestation subject")?;
            validate_git_sha(&receipt.source_commit, "attestation source commit")?;
            validate_sha256(&receipt.statement_sha256, "attestation statement")?;
            validate_signer_workflow(&self.repository, &receipt.signer_workflow)?;
            validate_actions_invocation(&self.repository, &receipt.invocation_uri)?;
            if !attested_assets.insert(receipt.asset.as_str())
                || receipt.source_repository != self.repository
                || receipt.source_ref != format!("refs/tags/{}", self.tag)
                || receipt.source_commit != self.commit_sha
            {
                return Err(format!(
                    "{} has inconsistent or duplicate build attestation identity",
                    receipt.asset
                ));
            }
            let asset = self
                .release_assets
                .iter()
                .find(|asset| asset.name == receipt.asset)
                .ok_or_else(|| {
                    format!("{} build attestation has no locked asset", receipt.asset)
                })?;
            if asset.sha256 != receipt.subject_sha256 {
                return Err(format!(
                    "{} build attestation digest disagrees with the locked asset",
                    receipt.asset
                ));
            }
        }
        Ok(())
    }

    /// Whether two locks identify the same immutable upstream release. Channel
    /// is intentionally excluded so a policy-profile change can rewrite only
    /// the selection annotation without being mistaken for an identity swap.
    #[must_use]
    pub fn same_release_identity(&self, other: &Self) -> bool {
        self.repository == other.repository
            && self.tag == other.tag
            && self.tag_ref_sha == other.tag_ref_sha
            && self.commit_sha == other.commit_sha
            && self.release_id == other.release_id
            && self.published_at == other.published_at
            && self.release_assets == other.release_assets
            && self.manifest == other.manifest
            && self.release_attestation == other.release_attestation
            && self.build_attestations == other.build_attestations
    }
}

fn reject_symlink(path: &Path, label: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("{label} {} must not be a symlink", path.display()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "failed to inspect {label} {}: {error}",
            path.display()
        )),
    }
}

/// Result of comparing a qualified candidate with the tracked lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockTransition {
    /// Exact deterministic lock already exists.
    Unchanged,
    /// A new lock may be materialized.
    Update,
}

/// Build a deterministic lock after validating all release evidence.
pub fn qualify_pulp_release(
    config: &PulpDependencyConfig,
    release: &ReleaseMetadata,
    tag: &TagIdentity,
    release_proof: &ReleaseAttestationProof,
    manifest_bytes: &[u8],
    build_attestations: &[BuildAttestationReceipt],
) -> Result<PulpDependencyLock, String> {
    let manifest_digest = sha256_hex(manifest_bytes);
    let release_assets =
        preflight_pulp_release(config, release, tag, release_proof, manifest_bytes)?;
    validate_build_attestations(config, release, tag, &release_assets, build_attestations)?;

    let mut attestations = build_attestations.to_vec();
    attestations.sort_by(|left, right| left.asset.cmp(&right.asset));
    let lock = PulpDependencyLock {
        schema: LOCK_SCHEMA.to_owned(),
        dependency: "pulp".to_owned(),
        channel: config.channel,
        repository: config.repository.clone(),
        tag: release.tag_name.clone(),
        tag_ref_sha: tag.ref_sha.clone(),
        commit_sha: tag.commit_sha.clone(),
        release_id: release.id,
        published_at: release.published_at.clone().ok_or_else(|| {
            "release publication timestamp disappeared after preflight".to_owned()
        })?,
        release_assets,
        manifest: LockedManifest {
            name: config.manifest_asset.clone(),
            sha256: manifest_digest,
        },
        release_attestation: LockedReleaseAttestation {
            predicate_type: release_proof.predicate_type.clone(),
            statement_sha256: release_proof.statement_sha256.clone(),
        },
        build_attestations: attestations,
    };
    lock.validate()?;
    Ok(lock)
}

/// Validate release identity, complete asset inventory, checksum manifest, and
/// channel binding before any large SDK asset needs to be downloaded.
pub fn preflight_pulp_release(
    config: &PulpDependencyConfig,
    release: &ReleaseMetadata,
    tag: &TagIdentity,
    release_proof: &ReleaseAttestationProof,
    manifest_bytes: &[u8],
) -> Result<Vec<LockedReleaseAsset>, String> {
    config.validate()?;
    validate_release_tag(&release.tag_name)?;
    if release.draft {
        return Err(format!("{} is a draft release", release.tag_name));
    }
    if release.prerelease {
        return Err(format!("{} is a prerelease", release.tag_name));
    }
    if release.published_at.as_deref().is_none_or(str::is_empty) {
        return Err(format!("{} has no publication timestamp", release.tag_name));
    }
    validate_git_sha(&tag.ref_sha, "tag ref")?;
    validate_git_sha(&tag.commit_sha, "peeled tag commit")?;
    match config.channel {
        DependencyChannel::LatestQualified => {}
        DependencyChannel::Stable => {
            if config.stable_tag.as_deref() != Some(release.tag_name.as_str()) {
                return Err("release does not match the reviewed stable_tag".to_owned());
            }
        }
        DependencyChannel::Fixed => {
            if config.fixed_tag.as_deref() != Some(release.tag_name.as_str()) {
                return Err("release does not match fixed_tag".to_owned());
            }
            if config.fixed_commit.as_deref() != Some(tag.commit_sha.as_str()) {
                return Err(format!(
                    "fixed_commit does not match {} peeled commit {}",
                    release.tag_name, tag.commit_sha
                ));
            }
        }
    }

    let release_assets = validate_release_assets(config, release)?;
    validate_release_proof(release, tag, release_proof, &release_assets)?;
    let manifest_digest = sha256_hex(manifest_bytes);
    let manifest_asset = release_assets
        .iter()
        .find(|asset| asset.name == config.manifest_asset)
        .ok_or_else(|| format!("missing manifest asset {}", config.manifest_asset))?;
    if manifest_asset.sha256 != manifest_digest {
        return Err(format!(
            "manifest bytes digest {manifest_digest} does not match GitHub asset digest {}",
            manifest_asset.sha256
        ));
    }
    validate_manifest(manifest_bytes, &config.manifest_asset, &release_assets)?;
    for required in &config.required_assets {
        if !release_assets.iter().any(|asset| asset.name == *required) {
            return Err(format!("missing required release asset {required}"));
        }
    }
    Ok(release_assets)
}

/// Compare exact stable release tags by semantic version.
pub fn compare_release_tags(left: &str, right: &str) -> Result<std::cmp::Ordering, String> {
    Ok(version_tuple(left)?.cmp(&version_tuple(right)?))
}

/// Enforce idempotence, no same-version identity swap, and no implicit
/// downgrade. An exact fixed tag+commit is the one reviewed downgrade escape.
pub fn validate_lock_transition(
    current: Option<&PulpDependencyLock>,
    candidate: &PulpDependencyLock,
) -> Result<LockTransition, String> {
    candidate.validate()?;
    let Some(current) = current else {
        return Ok(LockTransition::Update);
    };
    current.validate()?;
    if current.tag == candidate.tag && !current.same_release_identity(candidate) {
        return Err(format!(
            "refusing same-version identity swap for {}: tag, commit, release, asset set, manifest, or attestation changed",
            candidate.tag
        ));
    }
    if current == candidate {
        return Ok(LockTransition::Unchanged);
    }
    if version_tuple(&candidate.tag)? < version_tuple(&current.tag)?
        && candidate.channel != DependencyChannel::Fixed
    {
        return Err(format!(
            "refusing Pulp downgrade {} -> {}; use channel=\"fixed\" with exact fixed_tag and fixed_commit for a reviewed override",
            current.tag, candidate.tag
        ));
    }
    Ok(LockTransition::Update)
}

/// Render deterministic, newline-terminated lock JSON.
pub fn render_lock(lock: &PulpDependencyLock) -> Result<Vec<u8>, String> {
    lock.validate()?;
    let mut bytes = serde_json::to_vec_pretty(lock)
        .map_err(|error| format!("failed to serialize dependency lock: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// SHA-256 as lowercase hexadecimal.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn validate_release_assets(
    config: &PulpDependencyConfig,
    release: &ReleaseMetadata,
) -> Result<Vec<LockedReleaseAsset>, String> {
    if release.assets.is_empty() {
        return Err(format!("{} has no release assets", release.tag_name));
    }
    let mut seen = BTreeSet::new();
    let mut assets = Vec::with_capacity(release.assets.len());
    for asset in &release.assets {
        validate_asset_name(&asset.name, "release asset")?;
        if !seen.insert(asset.name.as_str()) {
            return Err(format!("duplicate release asset {:?}", asset.name));
        }
        if asset.state != "uploaded" {
            return Err(format!(
                "release asset {} is not uploaded (state={})",
                asset.name, asset.state
            ));
        }
        let digest = asset
            .digest
            .as_deref()
            .and_then(|value| value.strip_prefix("sha256:"))
            .ok_or_else(|| format!("release asset {} has no SHA-256 digest", asset.name))?;
        validate_sha256(digest, "release asset digest")?;
        if asset.size == 0 {
            return Err(format!("release asset {} is empty", asset.name));
        }
        let expected_url = format!(
            "https://github.com/{}/releases/download/{}/{}",
            config.repository, release.tag_name, asset.name
        );
        if asset.download_url != expected_url {
            return Err(format!(
                "release asset {} does not use an immutable versioned GitHub URL",
                asset.name
            ));
        }
        assets.push(LockedReleaseAsset {
            id: asset.id,
            name: asset.name.clone(),
            sha256: digest.to_owned(),
            size: asset.size,
            download_url: asset.download_url.clone(),
        });
    }
    assets.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(assets)
}

fn validate_release_proof(
    release: &ReleaseMetadata,
    tag: &TagIdentity,
    proof: &ReleaseAttestationProof,
    assets: &[LockedReleaseAsset],
) -> Result<(), String> {
    if proof.predicate_type != RELEASE_PREDICATE {
        return Err(format!(
            "unexpected release attestation predicate {}",
            proof.predicate_type
        ));
    }
    validate_sha256(&proof.statement_sha256, "release statement")?;
    if proof.release_id != release.id
        || proof.tag != release.tag_name
        || proof.ref_sha != tag.ref_sha
    {
        return Err(
            "release attestation identity does not match GitHub release/tag metadata".to_owned(),
        );
    }
    let expected: BTreeMap<_, _> = assets
        .iter()
        .map(|asset| (asset.name.clone(), asset.sha256.clone()))
        .collect();
    if proof.asset_digests != expected {
        return Err(
            "release attestation asset set does not exactly match GitHub release assets".to_owned(),
        );
    }
    Ok(())
}

fn validate_manifest(
    bytes: &[u8],
    manifest_name: &str,
    assets: &[LockedReleaseAsset],
) -> Result<(), String> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| format!("{manifest_name} is not valid UTF-8"))?;
    let mut entries = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let digest = parts
            .next()
            .ok_or_else(|| format!("{manifest_name}: malformed line {}", index + 1))?;
        let name = parts
            .next()
            .map(|value| value.trim_start_matches('*'))
            .ok_or_else(|| format!("{manifest_name}: malformed line {}", index + 1))?;
        if parts.next().is_some() {
            return Err(format!("{manifest_name}: malformed line {}", index + 1));
        }
        validate_sha256(digest, "manifest digest")?;
        validate_asset_name(name, "manifest entry")?;
        if entries.insert(name.to_owned(), digest.to_owned()).is_some() {
            return Err(format!("{manifest_name}: duplicate entry {name:?}"));
        }
    }
    let expected: BTreeMap<_, _> = assets
        .iter()
        .filter(|asset| asset.name != manifest_name)
        .map(|asset| (asset.name.clone(), asset.sha256.clone()))
        .collect();
    if entries != expected {
        return Err(format!(
            "{manifest_name} does not exactly cover the published non-manifest asset set"
        ));
    }
    Ok(())
}

fn validate_build_attestations(
    config: &PulpDependencyConfig,
    release: &ReleaseMetadata,
    tag: &TagIdentity,
    assets: &[LockedReleaseAsset],
    attestations: &[BuildAttestationReceipt],
) -> Result<(), String> {
    let required: BTreeSet<_> = config.required_assets.iter().cloned().collect();
    let available: BTreeMap<_, _> = assets
        .iter()
        .map(|asset| (asset.name.as_str(), asset.sha256.as_str()))
        .collect();
    let mut seen = BTreeSet::new();
    for receipt in attestations {
        if !required.contains(&receipt.asset) {
            return Err(format!(
                "unexpected build attestation for unconfigured asset {}",
                receipt.asset
            ));
        }
        if !seen.insert(receipt.asset.clone()) {
            return Err(format!("duplicate build attestation for {}", receipt.asset));
        }
        if available.get(receipt.asset.as_str()) != Some(&receipt.subject_sha256.as_str()) {
            return Err(format!(
                "build attestation subject does not match release asset {}",
                receipt.asset
            ));
        }
        if receipt.predicate_type != BUILD_PREDICATE
            || receipt.signer_workflow != config.signer_workflow
            || receipt.source_repository != config.repository
            || receipt.source_ref != format!("refs/tags/{}", release.tag_name)
            || receipt.source_commit != tag.commit_sha
        {
            return Err(format!(
                "build attestation identity does not match reviewed policy for {}",
                receipt.asset
            ));
        }
        validate_sha256(&receipt.statement_sha256, "build attestation statement")?;
        validate_actions_invocation(&config.repository, &receipt.invocation_uri)?;
    }
    if seen != required {
        let missing: Vec<_> = required.difference(&seen).cloned().collect();
        return Err(format!(
            "missing verified build attestations for required assets: {}",
            missing.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
