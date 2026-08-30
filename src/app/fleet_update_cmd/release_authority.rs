//! Fail-closed release authority for governed fleet updates.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::LoadedConfig;
use crate::gh::{GhClient, GhSupervision};

mod download;

#[cfg(test)]
use download::sha256_file;
use download::{DownloadedAsset, download_asset_to_private_file};

const RELEASE_WORKFLOW: &str = ".github/workflows/release.yml";
const PLATFORM_ASSET: &str = "shipyard-macos-arm64.dmg";
const CHECKSUM_ASSET: &str = "checksums.sha256";
const INSTALLER_PATH: &str = "install.sh";
const AUTH_HELPER_PATH: &str = "scripts/shipyard-github-app-token";
const AUTH_WRAPPER_PATH: &str = "scripts/ghapp";
const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_mins(1);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct ReleaseAuthority {
    pub(super) repository: String,
    pub(super) tag: String,
    pub(super) tag_object_oid: String,
    pub(super) commit_oid: String,
    pub(super) tree_oid: String,
    pub(super) release_id: u64,
    pub(super) installer: InstallerAuthority,
    pub(super) auth_helper: SourceFileAuthority,
    pub(super) auth_wrapper: SourceFileAuthority,
    pub(super) checksum_manifest: ReleaseAssetAuthority,
    pub(super) platform_asset: ReleaseAssetAuthority,
    pub(super) identity_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct InstallerAuthority {
    pub(super) path: String,
    pub(super) blob_oid: String,
    pub(super) sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct SourceFileAuthority {
    pub(super) path: String,
    pub(super) blob_oid: String,
    pub(super) sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct ReleaseAssetAuthority {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) sha256: String,
    pub(super) attestation_statement_sha256: Option<String>,
}

pub(super) trait ReleaseAuthorityVerifier {
    fn verify(&self, tag: &str) -> Result<ReleaseAuthority, String>;
}

pub(super) struct GitHubReleaseAuthorityVerifier<'a> {
    config: &'a LoadedConfig,
    cwd: &'a Path,
}

impl<'a> GitHubReleaseAuthorityVerifier<'a> {
    pub(super) fn new(config: &'a LoadedConfig, cwd: &'a Path) -> Self {
        Self { config, cwd }
    }

    fn command(&self) -> Result<Command, String> {
        let repository = release_repository()?;
        GhClient::from_loaded_config(self.config)
            .map_err(|error| format!("failed to load governed GitHub auth: {error}"))?
            .with_repo_override(&repository)
            .map_err(|error| format!("failed to bind governed GitHub auth: {error}"))?
            .prepare_privileged_command_with_auth_timeout(
                self.cwd,
                GhSupervision::Supervised,
                AUTH_TIMEOUT,
            )
            .map_err(|error| {
                format!(
                    "fleet release authority requires a governed native GitHub CLI and bounded token source: {error}"
                )
            })
    }

    fn api_json(&self, endpoint: &str) -> Result<Value, String> {
        let mut command = self.command()?;
        command.args(["api", endpoint]).stdin(Stdio::null());
        let output = run(&mut command, "GitHub release-authority query")?;
        serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("GitHub release-authority response was invalid JSON: {error}"))
    }

    fn download_asset(&self, asset: &ObservedAsset) -> Result<DownloadedAsset, String> {
        let endpoint = format!(
            "repos/{}/releases/assets/{}",
            release_repository()?,
            asset.id
        );
        let mut command = self.command()?;
        command
            .args(["api", &endpoint, "-H", "Accept: application/octet-stream"])
            .stdin(Stdio::null());
        download_asset_to_private_file(&mut command, asset, COMMAND_TIMEOUT, None)
    }

    fn verify_attestation(
        &self,
        tag: &str,
        commit_oid: &str,
        asset: &ObservedAsset,
        path: &Path,
    ) -> Result<String, String> {
        let repository = release_repository()?;
        let signer_workflow = format!("{repository}/{RELEASE_WORKFLOW}");
        let mut command = self.command()?;
        command
            .args(["attestation", "verify"])
            .arg(path)
            .args([
                "--repo",
                &repository,
                "--signer-workflow",
                &signer_workflow,
                "--source-digest",
                commit_oid,
                "--source-ref",
                &format!("refs/tags/{tag}"),
                "--format",
                "json",
            ])
            .stdin(Stdio::null());
        let output = run(
            &mut command,
            &format!("verify release attestation for {}", asset.name),
        )
        .map_err(|error| {
            format!(
                "release asset {} has no acceptable GitHub build-provenance attestation; the release workflow must attest the exact asset from {} at refs/tags/{}: {error}",
                asset.name, commit_oid, tag
            )
        })?;
        verified_statement_identity(&output.stdout, asset, commit_oid, tag)
    }

    fn source_file_authority(
        &self,
        commit_oid: &str,
        expected_path: &str,
    ) -> Result<SourceFileAuthority, String> {
        let source = self.api_json(&format!(
            "repos/{}/contents/{expected_path}?ref={commit_oid}",
            release_repository()?
        ))?;
        if string(&source, "/type", "source object type")? != "file"
            || string(&source, "/path", "source path")? != expected_path
            || string(&source, "/encoding", "source encoding")? != "base64"
        {
            return Err(format!(
                "release commit did not contain one regular {expected_path} file"
            ));
        }
        let blob_oid = string(&source, "/sha", "source blob SHA")?;
        validate_oid(blob_oid, "source blob")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(
                string(&source, "/content", "source content")?
                    .bytes()
                    .filter(|byte| !byte.is_ascii_whitespace())
                    .collect::<Vec<_>>(),
            )
            .map_err(|error| format!("GitHub source content was invalid base64: {error}"))?;
        Ok(SourceFileAuthority {
            path: expected_path.to_owned(),
            blob_oid: blob_oid.to_owned(),
            sha256: sha256(&bytes),
        })
    }

    fn close_mint_window(
        &self,
        tag: &str,
        tag_object_oid: &str,
        release_id: u64,
        checksum: &ObservedAsset,
        platform: &ObservedAsset,
    ) -> Result<(), String> {
        let repository = release_repository()?;
        let final_tag = self.api_json(&format!("repos/{repository}/git/ref/tags/{tag}"))?;
        if string(&final_tag, "/object/sha", "final tag object SHA")? != tag_object_oid {
            return Err(
                "release tag changed while immutable authority was being minted".to_owned(),
            );
        }
        let final_release = self.api_json(&format!("repos/{repository}/releases/tags/{tag}"))?;
        if final_release.get("id").and_then(Value::as_u64) != Some(release_id)
            || final_release.get("draft").and_then(Value::as_bool) != Some(false)
        {
            return Err(
                "release identity changed while immutable authority was being minted".to_owned(),
            );
        }
        let final_assets = observed_assets(&final_release)?;
        if unique_asset(&final_assets, CHECKSUM_ASSET)? != checksum
            || unique_asset(&final_assets, PLATFORM_ASSET)? != platform
        {
            return Err(
                "release assets changed while immutable authority was being minted".to_owned(),
            );
        }
        Ok(())
    }
}

impl ReleaseAuthorityVerifier for GitHubReleaseAuthorityVerifier<'_> {
    fn verify(&self, tag: &str) -> Result<ReleaseAuthority, String> {
        let repository = release_repository()?;
        let tag_ref = self.api_json(&format!("repos/{repository}/git/ref/tags/{tag}"))?;
        let tag_object_oid = string(&tag_ref, "/object/sha", "tag object SHA")?;
        validate_oid(tag_object_oid, "tag object")?;
        if string(&tag_ref, "/object/type", "tag object type")? != "tag" {
            return Err(
                "fleet release authority requires an immutable annotated tag object; lightweight tags are ineligible"
                    .to_owned(),
            );
        }
        let tag_object = self.api_json(&format!("repos/{repository}/git/tags/{tag_object_oid}"))?;
        if string(&tag_object, "/tag", "annotated tag name")? != tag
            || string(&tag_object, "/object/type", "annotated tag target type")? != "commit"
        {
            return Err(
                "annotated release tag did not bind the requested tag to a commit".to_owned(),
            );
        }
        let commit_oid = string(&tag_object, "/object/sha", "release commit SHA")?;
        validate_oid(commit_oid, "release commit")?;
        let commit = self.api_json(&format!("repos/{repository}/git/commits/{commit_oid}"))?;
        if string(&commit, "/sha", "commit SHA")? != commit_oid {
            return Err("GitHub commit response drifted from the annotated tag target".to_owned());
        }
        let tree_oid = string(&commit, "/tree/sha", "release tree SHA")?;
        validate_oid(tree_oid, "release tree")?;
        let installer_source = self.source_file_authority(commit_oid, INSTALLER_PATH)?;
        let installer_authority = InstallerAuthority {
            path: installer_source.path,
            blob_oid: installer_source.blob_oid,
            sha256: installer_source.sha256,
        };
        let auth_helper = self.source_file_authority(commit_oid, AUTH_HELPER_PATH)?;
        let auth_wrapper = self.source_file_authority(commit_oid, AUTH_WRAPPER_PATH)?;

        let release = self.api_json(&format!("repos/{repository}/releases/tags/{tag}"))?;
        if string(&release, "/tag_name", "release tag")? != tag
            || release.get("draft").and_then(Value::as_bool) != Some(false)
            || release.get("prerelease").and_then(Value::as_bool) != Some(false)
        {
            return Err(
                "fleet release authority requires one published stable exact-tag release"
                    .to_owned(),
            );
        }
        let release_id = release
            .get("id")
            .and_then(Value::as_u64)
            .filter(|id| *id != 0)
            .ok_or_else(|| "release authority omitted a nonzero release ID".to_owned())?;
        let assets = observed_assets(&release)?;
        let checksum = unique_asset(&assets, CHECKSUM_ASSET)?;
        let platform = unique_asset(&assets, PLATFORM_ASSET)?;
        let checksum_download = self.download_asset(checksum)?;
        let platform_download = self.download_asset(platform)?;
        let checksum_bytes = checksum_download.read_all()?;
        verify_checksum_manifest(&checksum_bytes, platform)?;
        let platform_attestation =
            self.verify_attestation(tag, commit_oid, platform, platform_download.path())?;

        let mut authority = ReleaseAuthority {
            repository,
            tag: tag.to_owned(),
            tag_object_oid: tag_object_oid.to_owned(),
            commit_oid: commit_oid.to_owned(),
            tree_oid: tree_oid.to_owned(),
            release_id,
            installer: installer_authority,
            auth_helper,
            auth_wrapper,
            checksum_manifest: checksum.with_attestation(None),
            platform_asset: platform.with_attestation(Some(platform_attestation)),
            identity_sha256: String::new(),
        };
        authority.identity_sha256 = authority_identity(&authority)?;
        // Freeze one authority after a final live equality check; hosts never
        // independently re-resolve mutable GitHub state during the rollout.
        self.close_mint_window(tag, tag_object_oid, release_id, checksum, platform)?;
        Ok(authority)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ObservedAsset {
    id: u64,
    name: String,
    sha256: String,
    size: u64,
}

impl ObservedAsset {
    fn with_attestation(
        &self,
        attestation_statement_sha256: Option<String>,
    ) -> ReleaseAssetAuthority {
        ReleaseAssetAuthority {
            id: self.id,
            name: self.name.clone(),
            sha256: self.sha256.clone(),
            attestation_statement_sha256,
        }
    }
}

fn observed_assets(release: &Value) -> Result<Vec<ObservedAsset>, String> {
    let values = release
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| "release authority omitted its asset inventory".to_owned())?;
    values
        .iter()
        .map(|value| {
            let id = value
                .get("id")
                .and_then(Value::as_u64)
                .filter(|id| *id != 0)
                .ok_or_else(|| "release asset omitted a nonzero ID".to_owned())?;
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| {
                    !name.is_empty()
                        && !name.chars().any(char::is_control)
                        && !name.contains(['/', '\\', '='])
                        && !name.contains("SHIPYARD_FLEET_")
                })
                .ok_or_else(|| "release asset had an unsafe or missing name".to_owned())?;
            let digest = value
                .get("digest")
                .and_then(Value::as_str)
                .and_then(|value| value.strip_prefix("sha256:"))
                .ok_or_else(|| format!("release asset {name} omitted its GitHub SHA-256 digest"))?;
            validate_sha256(digest, &format!("release asset {name}"))?;
            let size = value
                .get("size")
                .and_then(Value::as_u64)
                .filter(|size| *size > 0)
                .ok_or_else(|| format!("release asset {name} omitted a positive size"))?;
            Ok(ObservedAsset {
                id,
                name: name.to_owned(),
                sha256: digest.to_owned(),
                size,
            })
        })
        .collect()
}

fn unique_asset<'a>(assets: &'a [ObservedAsset], name: &str) -> Result<&'a ObservedAsset, String> {
    let matches = assets
        .iter()
        .filter(|asset| asset.name == name)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(format!(
            "release authority requires exactly one {name} asset, observed {}",
            matches.len()
        ));
    }
    Ok(matches[0])
}

fn verify_checksum_manifest(bytes: &[u8], platform: &ObservedAsset) -> Result<(), String> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| "checksums.sha256 was not UTF-8".to_owned())?;
    let mut matches = Vec::new();
    for line in text.lines() {
        let Some((digest, name)) = line.split_once("  ") else {
            if line.split_whitespace().last() == Some(platform.name.as_str()) {
                return Err(format!(
                    "checksums.sha256 contained a malformed entry for {}",
                    platform.name
                ));
            }
            continue;
        };
        if name.trim_start_matches('*') == platform.name {
            validate_sha256(digest, "checksums.sha256 platform entry")?;
            matches.push(digest);
        }
    }
    if matches.as_slice() != [platform.sha256.as_str()] {
        return Err(format!(
            "checksums.sha256 must contain exactly one exact digest for {}; expected {}",
            platform.name, platform.sha256
        ));
    }
    Ok(())
}

fn verified_statement_identity(
    bytes: &[u8],
    asset: &ObservedAsset,
    commit_oid: &str,
    tag: &str,
) -> Result<String, String> {
    let records: Vec<Value> = serde_json::from_slice(bytes)
        .map_err(|error| format!("attestation verifier returned invalid JSON: {error}"))?;
    let expected_ref = format!("refs/tags/{tag}");
    let expected_repository = format!("https://github.com/{}", release_repository()?);
    let expected_dependency = format!("git+{expected_repository}@{expected_ref}");
    let mut identities = Vec::new();
    for record in records {
        let payload = record
            .pointer("/attestation/bundle/dsseEnvelope/payload")
            .and_then(Value::as_str)
            .ok_or_else(|| "verified attestation omitted its signed DSSE payload".to_owned())?;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|error| format!("verified attestation payload was invalid base64: {error}"))?;
        let signed: Value = serde_json::from_slice(&decoded)
            .map_err(|error| format!("verified attestation payload was invalid JSON: {error}"))?;
        let statement = record
            .pointer("/verificationResult/statement")
            .ok_or_else(|| "attestation verifier omitted the verified statement".to_owned())?;
        if &signed != statement {
            return Err("verified statement disagreed with the signed DSSE payload".to_owned());
        }
        if statement.get("predicateType").and_then(Value::as_str)
            != Some("https://slsa.dev/provenance/v1")
        {
            continue;
        }
        let subject_matches = statement
            .get("subject")
            .and_then(Value::as_array)
            .is_some_and(|subjects| {
                subjects.iter().any(|subject| {
                    subject.get("name").and_then(Value::as_str) == Some(asset.name.as_str())
                        && subject.pointer("/digest/sha256").and_then(Value::as_str)
                            == Some(asset.sha256.as_str())
                })
            });
        let source_matches = statement
            .pointer("/predicate/buildDefinition/resolvedDependencies")
            .and_then(Value::as_array)
            .is_some_and(|dependencies| {
                dependencies.iter().any(|dependency| {
                    dependency.get("uri").and_then(Value::as_str)
                        == Some(expected_dependency.as_str())
                        && dependency
                            .pointer("/digest/gitCommit")
                            .and_then(Value::as_str)
                            == Some(commit_oid)
                })
            });
        if subject_matches && source_matches {
            identities.push(sha256(&decoded));
        }
    }
    identities.sort();
    identities.dedup();
    if identities.len() != 1 {
        return Err(format!(
            "expected exactly one verified build-provenance statement for {} at {} {}, observed {}",
            asset.name,
            commit_oid,
            expected_ref,
            identities.len()
        ));
    }
    Ok(identities.remove(0))
}

fn authority_identity(authority: &ReleaseAuthority) -> Result<String, String> {
    let mut canonical = authority.clone();
    canonical.identity_sha256.clear();
    serde_json::to_vec(&canonical)
        .map(|bytes| sha256(&bytes))
        .map_err(|error| format!("could not canonicalize release authority: {error}"))
}

fn run(command: &mut Command, label: &str) -> Result<Output, String> {
    crate::process::run_output_until(command, Instant::now() + COMMAND_TIMEOUT, label)
        .map_err(|error| error.to_string())
        .and_then(|output| {
            if output.status.success() {
                Ok(output)
            } else {
                let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                Err(if detail.is_empty() {
                    format!("{label} exited {}", output.status.code().unwrap_or(-1))
                } else {
                    format!("{label} failed: {detail}")
                })
            }
        })
}

fn string<'a>(value: &'a Value, pointer: &str, label: &str) -> Result<&'a str, String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("GitHub response omitted {label}"))
}

fn validate_oid(value: &str, label: &str) -> Result<(), String> {
    if value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(format!(
            "{label} was not a canonical full 40-character Git object ID"
        ))
    }
}

fn validate_sha256(value: &str, label: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(format!("{label} was not a canonical lowercase SHA-256"))
    }
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn release_repository() -> Result<String, String> {
    let url = env!("CARGO_PKG_REPOSITORY").trim_end_matches(['/', '\\']);
    let slug = url
        .strip_prefix("https://github.com/")
        .and_then(|value| value.strip_suffix(".git").or(Some(value)))
        .ok_or_else(|| "package repository must be an HTTPS GitHub repository".to_owned())?;
    let mut parts = slug.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if owner.is_empty()
        || name.is_empty()
        || parts.next().is_some()
        || !owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("package repository did not contain one safe owner/name slug".to_owned());
    }
    Ok(format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str, digest: char) -> ObservedAsset {
        ObservedAsset {
            id: 7,
            name: name.to_owned(),
            sha256: digest.to_string().repeat(64),
            size: 1,
        }
    }

    #[cfg(unix)]
    fn cat_command(path: &Path) -> Command {
        let mut command = Command::new("/bin/cat");
        command.arg(path).env_clear();
        command
    }

    fn verified_output(asset: &ObservedAsset, commit: &str, tag: &str) -> Vec<u8> {
        let repository = release_repository().expect("package repository");
        let statement = serde_json::json!({
            "_type": "https://in-toto.io/Statement/v1",
            "subject": [{"name": asset.name, "digest": {"sha256": asset.sha256}}],
            "predicateType": "https://slsa.dev/provenance/v1",
            "predicate": {
                "buildDefinition": {
                    "resolvedDependencies": [{
                        "uri": format!("git+https://github.com/{repository}@refs/tags/{tag}"),
                        "digest": {"gitCommit": commit}
                    }]
                }
            }
        });
        let payload = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&statement).expect("statement"));
        serde_json::to_vec(&vec![serde_json::json!({
            "attestation": {"bundle": {"dsseEnvelope": {"payload": payload}}},
            "verificationResult": {"statement": statement}
        })])
        .expect("output")
    }

    #[test]
    fn exact_manifest_and_attestation_bind_platform_asset_to_source() {
        let platform = asset(PLATFORM_ASSET, 'a');
        let manifest = format!("{}  {}\n", platform.sha256, platform.name);
        verify_checksum_manifest(manifest.as_bytes(), &platform).expect("manifest");
        let commit = "b".repeat(40);
        let identity = verified_statement_identity(
            &verified_output(&platform, &commit, "v1.2.3"),
            &platform,
            &commit,
            "v1.2.3",
        )
        .expect("verified identity");
        assert_eq!(identity.len(), 64);
    }

    #[test]
    fn manifest_refuses_duplicate_malformed_or_drifted_platform_identity() {
        let platform = asset(PLATFORM_ASSET, 'a');
        for manifest in [
            format!(
                "{}  {}\n{}  {}\n",
                platform.sha256, platform.name, platform.sha256, platform.name
            ),
            format!("{} {}\n", platform.sha256, platform.name),
            format!("{}  {}\n", "c".repeat(64), platform.name),
        ] {
            assert!(verify_checksum_manifest(manifest.as_bytes(), &platform).is_err());
        }
    }

    #[test]
    fn attestation_refuses_subject_source_and_signed_payload_drift() {
        let platform = asset(PLATFORM_ASSET, 'a');
        let commit = "b".repeat(40);
        assert!(
            verified_statement_identity(
                &verified_output(&platform, &"c".repeat(40), "v1.2.3"),
                &platform,
                &commit,
                "v1.2.3"
            )
            .is_err()
        );
        let mut output: Value =
            serde_json::from_slice(&verified_output(&platform, &commit, "v1.2.3")).expect("json");
        output[0]["verificationResult"]["statement"]["subject"][0]["digest"]["sha256"] =
            Value::from("d".repeat(64));
        assert!(
            verified_statement_identity(
                &serde_json::to_vec(&output).expect("json"),
                &platform,
                &commit,
                "v1.2.3"
            )
            .is_err()
        );
    }

    #[test]
    fn release_inventory_refuses_missing_digest_and_duplicate_required_asset() {
        let release = serde_json::json!({
            "assets": [
                {"id": 1, "name": CHECKSUM_ASSET, "size": 1, "digest": format!("sha256:{}", "a".repeat(64))},
                {"id": 2, "name": PLATFORM_ASSET, "size": 2, "digest": format!("sha256:{}", "b".repeat(64))},
                {"id": 3, "name": PLATFORM_ASSET, "size": 3, "digest": format!("sha256:{}", "c".repeat(64))}
            ]
        });
        let assets = observed_assets(&release).expect("inventory");
        assert!(unique_asset(&assets, PLATFORM_ASSET).is_err());
        let malformed = serde_json::json!({"assets": [{"id": 1, "name": PLATFORM_ASSET}]});
        assert!(observed_assets(&malformed).is_err());
        for hostile in [
            "bad\nSHIPYARD_FLEET_AUTHORITY_ID=fake",
            "bad=marker",
            "../asset",
        ] {
            let hostile = serde_json::json!({
                "assets": [{"id": 1, "name": hostile, "size": 1, "digest": format!("sha256:{}", "a".repeat(64))}]
            });
            assert!(observed_assets(&hostile).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn release_asset_download_streams_more_than_generic_capture_limit() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().expect("fixture");
        let bytes = vec![0x5a; 8 * 1024 * 1024 + 4096];
        let source = fixture.path().join("large.dmg");
        std::fs::write(&source, &bytes).expect("large fixture");
        let asset = ObservedAsset {
            id: 42,
            name: PLATFORM_ASSET.to_owned(),
            sha256: sha256(&bytes),
            size: bytes.len() as u64,
        };
        let staging = tempfile::tempdir().expect("staging parent");
        let mut command = cat_command(&source);
        let downloaded = download_asset_to_private_file(
            &mut command,
            &asset,
            Duration::from_secs(10),
            Some(staging.path()),
        )
        .expect("streamed download");

        assert_eq!(
            downloaded.path().metadata().expect("metadata").len(),
            asset.size
        );
        assert_eq!(
            downloaded
                .path()
                .parent()
                .expect("private directory")
                .metadata()
                .expect("private metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            downloaded
                .path()
                .metadata()
                .expect("asset metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            sha256_file(downloaded.path(), asset.size).expect("digest"),
            asset.sha256
        );
        drop(downloaded);
        assert_eq!(
            std::fs::read_dir(staging.path())
                .expect("staging listing")
                .count(),
            0,
            "successful staging must be removed when authority resolution releases it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn release_asset_download_rejects_truncation_and_cleans_partial_file() {
        let fixture = tempfile::tempdir().expect("fixture");
        let source = fixture.path().join("truncated.dmg");
        std::fs::write(&source, b"short").expect("fixture");
        let asset = ObservedAsset {
            id: 43,
            name: PLATFORM_ASSET.to_owned(),
            sha256: sha256(b"short plus expected bytes"),
            size: b"short plus expected bytes".len() as u64,
        };
        let staging = tempfile::tempdir().expect("staging parent");
        let error = download_asset_to_private_file(
            &mut cat_command(&source),
            &asset,
            Duration::from_secs(5),
            Some(staging.path()),
        )
        .expect_err("truncated download");

        assert!(error.contains("was truncated"), "{error}");
        assert_eq!(
            std::fs::read_dir(staging.path())
                .expect("staging listing")
                .count(),
            0,
            "failed staging must be removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn release_asset_download_does_not_wait_for_escaped_pipe_holder() {
        let fixture = tempfile::tempdir().expect("fixture");
        let pid_file = fixture.path().join("escaped.pid");
        let script = r#"
use strict;
use warnings;
use POSIX qw(setsid);
my $pid_file = shift @ARGV;
my $pid = fork();
die "fork failed: $!" unless defined $pid;
if ($pid == 0) {
    setsid() >= 0 or die "setsid failed: $!";
    open my $handle, '>', $pid_file or die "pid file: $!";
    print {$handle} "$$\n";
    close $handle;
    sleep 30;
    exit 0;
}
select undef, undef, undef, 0.05;
print "payload";
exit 0;
"#;
        let mut command = Command::new("/usr/bin/perl");
        command
            .args(["-MPOSIX=setsid", "-e", script])
            .arg(&pid_file)
            .env_clear();
        let asset = ObservedAsset {
            id: 47,
            name: PLATFORM_ASSET.to_owned(),
            sha256: sha256(b"payload"),
            size: b"payload".len() as u64,
        };
        let started = Instant::now();
        let result = download_asset_to_private_file(
            &mut command,
            &asset,
            Duration::from_secs(2),
            Some(fixture.path()),
        );
        if let Ok(pid) = std::fs::read_to_string(&pid_file).map(|value| value.trim().to_owned()) {
            let _ = Command::new("/bin/kill").args(["-KILL", &pid]).status();
        }
        result.expect("escaped pipe holder must not retain capture readers");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "capture readers exceeded the bounded cleanup interval"
        );
    }

    #[cfg(unix)]
    #[test]
    fn release_asset_download_deadline_survives_escaped_pipe_flood() {
        let fixture = tempfile::tempdir().expect("fixture");
        let pid_file = fixture.path().join("escaped-flood.pid");
        let script = r#"
use strict;
use warnings;
use POSIX qw(setsid);
my $pid_file = shift @ARGV;
my $pid = fork();
die "fork failed: $!" unless defined $pid;
if ($pid == 0) {
    setsid() >= 0 or die "setsid failed: $!";
    open my $handle, '>', $pid_file or die "pid file: $!";
    print {$handle} "$$\n";
    close $handle;
    my $chunk = 'x' x 4096;
    while (1) { print $chunk or last; }
    exit 0;
}
select undef, undef, undef, 0.05;
exit 0;
"#;
        let mut command = Command::new("/usr/bin/perl");
        command
            .args(["-MPOSIX=setsid", "-e", script])
            .arg(&pid_file)
            .env_clear();
        let asset = ObservedAsset {
            id: 48,
            name: PLATFORM_ASSET.to_owned(),
            sha256: "a".repeat(64),
            size: 512 * 1024 * 1024,
        };
        let started = Instant::now();
        let result = download_asset_to_private_file(
            &mut command,
            &asset,
            Duration::from_secs(2),
            Some(fixture.path()),
        );
        if let Ok(pid) = std::fs::read_to_string(&pid_file).map(|value| value.trim().to_owned()) {
            let _ = Command::new("/bin/kill").args(["-KILL", &pid]).status();
        }
        let error = result.expect_err("escaped flood cannot satisfy exact asset authority");
        assert!(error.contains("was truncated"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "continuous escaped output exceeded the bounded drain interval"
        );
    }

    #[cfg(unix)]
    #[test]
    fn release_asset_download_rejects_overflow_and_command_failure() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().expect("fixture");
        let should_not_run = fixture.path().join("oversize-command-ran");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", ": > \"$1\"", "sh"])
            .arg(&should_not_run)
            .env_clear();
        let unsupported = ObservedAsset {
            id: 46,
            name: PLATFORM_ASSET.to_owned(),
            sha256: "a".repeat(64),
            size: 512 * 1024 * 1024 + 1,
        };
        let error = download_asset_to_private_file(
            &mut command,
            &unsupported,
            Duration::from_secs(5),
            Some(fixture.path()),
        )
        .expect_err("fixed upper bound");
        assert!(error.contains("exceeded the supported range"), "{error}");
        assert!(
            !should_not_run.exists(),
            "oversize response command must not spawn"
        );

        let source = fixture.path().join("overflow.dmg");
        std::fs::write(&source, b"too many bytes").expect("fixture");
        let staging = tempfile::tempdir().expect("staging parent");
        let overflow = ObservedAsset {
            id: 44,
            name: PLATFORM_ASSET.to_owned(),
            sha256: sha256(b"tiny"),
            size: b"tiny".len() as u64,
        };
        let error = download_asset_to_private_file(
            &mut cat_command(&source),
            &overflow,
            Duration::from_secs(5),
            Some(staging.path()),
        )
        .expect_err("oversized response");
        assert!(error.contains("exceeded its declared"), "{error}");
        assert_eq!(
            std::fs::read_dir(staging.path()).expect("listing").count(),
            0
        );

        let failing = fixture.path().join("failing-download");
        std::fs::write(
            &failing,
            "#!/bin/sh\nprintf partial\nprintf download-failed >&2\nexit 17\n",
        )
        .expect("script");
        std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o700))
            .expect("script mode");
        let failed_asset = ObservedAsset {
            id: 45,
            name: PLATFORM_ASSET.to_owned(),
            sha256: sha256(b"partial"),
            size: b"partial".len() as u64,
        };
        let mut command = Command::new(&failing);
        command.env_clear();
        let error = download_asset_to_private_file(
            &mut command,
            &failed_asset,
            Duration::from_secs(5),
            Some(staging.path()),
        )
        .expect_err("nonzero download");
        assert!(error.contains("download-failed"), "{error}");
        assert_eq!(
            std::fs::read_dir(staging.path()).expect("listing").count(),
            0
        );
    }
}
