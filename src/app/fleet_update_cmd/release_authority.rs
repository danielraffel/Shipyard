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

const REPOSITORY: &str = "danielraffel/Shipyard";
const SIGNER_WORKFLOW: &str = "danielraffel/Shipyard/.github/workflows/release.yml";
const PLATFORM_ASSET: &str = "shipyard-macos-arm64.dmg";
const CHECKSUM_ASSET: &str = "checksums.sha256";
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
        GhClient::from_loaded_config(self.config)
            .map_err(|error| format!("failed to load governed GitHub auth: {error}"))?
            .with_repo_override(REPOSITORY)
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

    fn download_asset(&self, asset: &ObservedAsset) -> Result<Vec<u8>, String> {
        let endpoint = format!("repos/{REPOSITORY}/releases/assets/{}", asset.id);
        let mut command = self.command()?;
        command
            .args(["api", &endpoint, "-H", "Accept: application/octet-stream"])
            .stdin(Stdio::null());
        let output = run(
            &mut command,
            &format!("download release asset {}", asset.name),
        )?;
        let actual = sha256(&output.stdout);
        if actual != asset.sha256 {
            return Err(format!(
                "release asset {} changed during authority resolution: metadata {}, downloaded {}",
                asset.name, asset.sha256, actual
            ));
        }
        Ok(output.stdout)
    }

    fn verify_attestation(
        &self,
        tag: &str,
        commit_oid: &str,
        asset: &ObservedAsset,
        bytes: &[u8],
    ) -> Result<String, String> {
        let temp = tempfile::tempdir()
            .map_err(|error| format!("could not create attestation staging directory: {error}"))?;
        let path = temp.path().join(&asset.name);
        std::fs::write(&path, bytes)
            .map_err(|error| format!("could not stage {} for attestation: {error}", asset.name))?;
        let mut command = self.command()?;
        command
            .args(["attestation", "verify"])
            .arg(&path)
            .args([
                "--repo",
                REPOSITORY,
                "--signer-workflow",
                SIGNER_WORKFLOW,
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

    fn installer_authority(&self, commit_oid: &str) -> Result<InstallerAuthority, String> {
        let installer = self.api_json(&format!(
            "repos/{REPOSITORY}/contents/install.sh?ref={commit_oid}"
        ))?;
        if string(&installer, "/type", "installer object type")? != "file"
            || string(&installer, "/path", "installer path")? != "install.sh"
            || string(&installer, "/encoding", "installer encoding")? != "base64"
        {
            return Err("release commit did not contain one regular install.sh file".to_owned());
        }
        let blob_oid = string(&installer, "/sha", "installer blob SHA")?;
        validate_oid(blob_oid, "installer blob")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(
                string(&installer, "/content", "installer content")?
                    .bytes()
                    .filter(|byte| !byte.is_ascii_whitespace())
                    .collect::<Vec<_>>(),
            )
            .map_err(|error| format!("GitHub installer content was invalid base64: {error}"))?;
        Ok(InstallerAuthority {
            path: "install.sh".to_owned(),
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
        let final_tag = self.api_json(&format!("repos/{REPOSITORY}/git/ref/tags/{tag}"))?;
        if string(&final_tag, "/object/sha", "final tag object SHA")? != tag_object_oid {
            return Err(
                "release tag changed while immutable authority was being minted".to_owned(),
            );
        }
        let final_release = self.api_json(&format!("repos/{REPOSITORY}/releases/tags/{tag}"))?;
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
        let tag_ref = self.api_json(&format!("repos/{REPOSITORY}/git/ref/tags/{tag}"))?;
        let tag_object_oid = string(&tag_ref, "/object/sha", "tag object SHA")?;
        validate_oid(tag_object_oid, "tag object")?;
        if string(&tag_ref, "/object/type", "tag object type")? != "tag" {
            return Err(
                "fleet release authority requires an immutable annotated tag object; lightweight tags are ineligible"
                    .to_owned(),
            );
        }
        let tag_object = self.api_json(&format!("repos/{REPOSITORY}/git/tags/{tag_object_oid}"))?;
        if string(&tag_object, "/tag", "annotated tag name")? != tag
            || string(&tag_object, "/object/type", "annotated tag target type")? != "commit"
        {
            return Err(
                "annotated release tag did not bind the requested tag to a commit".to_owned(),
            );
        }
        let commit_oid = string(&tag_object, "/object/sha", "release commit SHA")?;
        validate_oid(commit_oid, "release commit")?;
        let commit = self.api_json(&format!("repos/{REPOSITORY}/git/commits/{commit_oid}"))?;
        if string(&commit, "/sha", "commit SHA")? != commit_oid {
            return Err("GitHub commit response drifted from the annotated tag target".to_owned());
        }
        let tree_oid = string(&commit, "/tree/sha", "release tree SHA")?;
        validate_oid(tree_oid, "release tree")?;
        let installer_authority = self.installer_authority(commit_oid)?;

        let release = self.api_json(&format!("repos/{REPOSITORY}/releases/tags/{tag}"))?;
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
        let checksum_bytes = self.download_asset(checksum)?;
        let platform_bytes = self.download_asset(platform)?;
        verify_checksum_manifest(&checksum_bytes, platform)?;
        let platform_attestation =
            self.verify_attestation(tag, commit_oid, platform, &platform_bytes)?;

        let mut authority = ReleaseAuthority {
            repository: REPOSITORY.to_owned(),
            tag: tag.to_owned(),
            tag_object_oid: tag_object_oid.to_owned(),
            commit_oid: commit_oid.to_owned(),
            tree_oid: tree_oid.to_owned(),
            release_id,
            installer: installer_authority,
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
            Ok(ObservedAsset {
                id,
                name: name.to_owned(),
                sha256: digest.to_owned(),
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
    let expected_repository = format!("https://github.com/{REPOSITORY}");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str, digest: char) -> ObservedAsset {
        ObservedAsset {
            id: 7,
            name: name.to_owned(),
            sha256: digest.to_string().repeat(64),
        }
    }

    fn verified_output(asset: &ObservedAsset, commit: &str, tag: &str) -> Vec<u8> {
        let statement = serde_json::json!({
            "_type": "https://in-toto.io/Statement/v1",
            "subject": [{"name": asset.name, "digest": {"sha256": asset.sha256}}],
            "predicateType": "https://slsa.dev/provenance/v1",
            "predicate": {
                "buildDefinition": {
                    "resolvedDependencies": [{
                        "uri": format!("git+https://github.com/{REPOSITORY}@refs/tags/{tag}"),
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
                {"id": 1, "name": CHECKSUM_ASSET, "digest": format!("sha256:{}", "a".repeat(64))},
                {"id": 2, "name": PLATFORM_ASSET, "digest": format!("sha256:{}", "b".repeat(64))},
                {"id": 3, "name": PLATFORM_ASSET, "digest": format!("sha256:{}", "c".repeat(64))}
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
                "assets": [{"id": 1, "name": hostile, "digest": format!("sha256:{}", "a".repeat(64))}]
            });
            assert!(observed_assets(&hostile).is_err());
        }
    }
}
