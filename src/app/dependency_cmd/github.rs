use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;

use super::{CliFailure, failure, gh_json, output_detail, prepared_gh, release_asset_sha256};
use crate::dependency::{
    BuildAttestationReceipt, PulpDependencyConfig, ReleaseAssetMetadata, ReleaseAttestationProof,
    ReleaseMetadata, TagIdentity, compare_release_tags, sha256_hex,
};
use crate::gh::GhClient;

const MAX_TAG_PEEL_DEPTH: usize = 8;
const SLSA_PROVENANCE_V1: &str = "https://slsa.dev/provenance/v1";

#[derive(Debug)]
pub(super) enum AttestationFailure {
    Rejected(CliFailure),
    Operational(CliFailure),
}

impl AttestationFailure {
    fn rejected(message: impl Into<String>) -> Self {
        Self::Rejected(failure(message))
    }

    fn operational(error: CliFailure) -> Self {
        Self::Operational(error)
    }
}

#[derive(Deserialize)]
struct AttestationPage {
    attestations: Vec<AttestationSummary>,
}

#[derive(Deserialize)]
struct AttestationSummary {
    initiator: String,
}

pub(super) fn latest_release_candidates(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
) -> Result<Vec<ReleaseMetadata>, CliFailure> {
    let endpoint = format!("repos/{repo}/releases?per_page=100");
    let pages: Vec<Vec<ReleaseMetadata>> =
        gh_json(client, cwd, ["api", "--paginate", "--slurp", &endpoint])?;
    Ok(release_candidates_from_pages(pages))
}

pub(super) fn release_candidates_from_pages(
    pages: Vec<Vec<ReleaseMetadata>>,
) -> Vec<ReleaseMetadata> {
    let mut releases: Vec<_> = pages.into_iter().flatten().collect();
    releases.retain(|release| {
        !release.draft
            && !release.prerelease
            && compare_release_tags(&release.tag_name, &release.tag_name).is_ok()
    });
    releases.sort_by(|left, right| {
        compare_release_tags(&right.tag_name, &left.tag_name)
            .expect("retained release tags are valid")
    });
    releases
}

pub(super) fn release_by_tag(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    tag: &str,
) -> Result<ReleaseMetadata, CliFailure> {
    let endpoint = format!("repos/{repo}/releases/tags/{tag}");
    gh_json(client, cwd, ["api", &endpoint])
}

pub(super) fn release_with_authoritative_assets(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    mut release: ReleaseMetadata,
) -> Result<ReleaseMetadata, CliFailure> {
    let endpoint = format!("repos/{repo}/releases/{}/assets?per_page=100", release.id);
    let pages: Vec<Vec<ReleaseAssetMetadata>> =
        gh_json(client, cwd, ["api", "--paginate", "--slurp", &endpoint])?;
    release.assets = release_assets_from_pages(pages);
    Ok(release)
}

pub(super) fn release_assets_from_pages(
    pages: Vec<Vec<ReleaseAssetMetadata>>,
) -> Vec<ReleaseAssetMetadata> {
    pages.into_iter().flatten().collect()
}

#[derive(Deserialize)]
struct GitRefResponse {
    object: GitObject,
}

#[derive(Clone, Deserialize)]
struct GitObject {
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}

pub(super) fn tag_identity(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    tag: &str,
) -> Result<TagIdentity, CliFailure> {
    let endpoint = format!("repos/{repo}/git/ref/tags/{tag}");
    let reference: GitRefResponse = gh_json(client, cwd, ["api", &endpoint])?;
    let ref_sha = reference.object.sha.clone();
    let mut object = reference.object;
    for _ in 0..MAX_TAG_PEEL_DEPTH {
        match object.kind.as_str() {
            "commit" => {
                return Ok(TagIdentity {
                    ref_sha,
                    commit_sha: object.sha,
                });
            }
            "tag" => {
                let endpoint = format!("repos/{repo}/git/tags/{}", object.sha);
                object = gh_json::<GitRefResponse, _, _>(client, cwd, ["api", &endpoint])?.object;
            }
            kind => {
                return Err(failure(format!(
                    "tag {tag} peels to unsupported Git object type {kind:?}"
                )));
            }
        }
    }
    Err(failure(format!(
        "tag {tag} exceeds the maximum peel depth of {MAX_TAG_PEEL_DEPTH}"
    )))
}

fn attestation_exists(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    digest: &str,
    predicate_type: &str,
    initiator: &str,
) -> Result<bool, CliFailure> {
    let endpoint = format!("repos/{repo}/attestations/{digest}");
    let predicate = format!("predicate_type={predicate_type}");
    let mut command = prepared_gh(client, cwd)?;
    command.args([
        "api",
        "--method",
        "GET",
        "--paginate",
        "--slurp",
        &endpoint,
        "-f",
        "per_page=100",
        "-f",
        &predicate,
    ]);
    let output = command
        .output()
        .map_err(|error| failure(format!("failed to query GitHub attestations: {error}")))?;
    if !output.status.success() {
        let detail = output_detail(&output);
        if detail.contains("HTTP 404") {
            return Ok(false);
        }
        return Err(failure(format!(
            "GitHub attestation inventory failed: {detail}"
        )));
    }
    attestation_inventory_has_initiator(&output.stdout, initiator).map_err(failure)
}

pub(super) fn attestation_inventory_has_initiator(
    bytes: &[u8],
    initiator: &str,
) -> Result<bool, String> {
    let pages: Vec<AttestationPage> = serde_json::from_slice(bytes)
        .map_err(|error| format!("GitHub attestation inventory returned invalid JSON: {error}"))?;
    Ok(pages
        .iter()
        .flat_map(|page| &page.attestations)
        .any(|attestation| attestation.initiator == initiator))
}

pub(super) fn release_attestation(
    client: &GhClient,
    cwd: &Path,
    config: &PulpDependencyConfig,
    release: &ReleaseMetadata,
    tag: &TagIdentity,
) -> Result<ReleaseAttestationProof, AttestationFailure> {
    let digest = format!("sha1:{}", tag.ref_sha);
    let exists = attestation_exists(
        client,
        cwd,
        &config.repository,
        &digest,
        "release",
        "github",
    )
    .map_err(AttestationFailure::operational)?;
    if !exists {
        return Err(AttestationFailure::rejected(format!(
            "release {} has no GitHub immutable-release attestation",
            release.tag_name
        )));
    }
    let mut command = prepared_gh(client, cwd).map_err(AttestationFailure::operational)?;
    command.args([
        "release",
        "verify",
        &release.tag_name,
        "--repo",
        &config.repository,
        "--format",
        "json",
    ]);
    let output = command.output().map_err(|error| {
        AttestationFailure::operational(failure(format!(
            "failed to start GitHub release verifier: {error}"
        )))
    })?;
    if !output.status.success() {
        let detail = output_detail(&output);
        let error = failure(format!("GitHub release verification failed: {detail}"));
        return Err(if release_attestation_policy_rejected(&detail) {
            AttestationFailure::Rejected(error)
        } else {
            AttestationFailure::Operational(error)
        });
    }
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        AttestationFailure::operational(failure(format!(
            "GitHub release verifier returned invalid JSON: {error}"
        )))
    })?;
    release_verifier_record_contract(&value).map_err(|error| {
        AttestationFailure::operational(failure(format!(
            "GitHub release verifier output contract changed: {error}"
        )))
    })?;
    parse_release_attestation(&value, config, release, tag).map_err(AttestationFailure::rejected)
}

pub(super) fn release_attestation_policy_rejected(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    [
        "error parsing attestations for tag",
        "no attestations found for release",
        "duplicate attestations found for release",
    ]
    .iter()
    .any(|message| detail.contains(message))
}

pub(super) fn parse_release_attestation(
    value: &Value,
    config: &PulpDependencyConfig,
    release: &ReleaseMetadata,
    tag: &TagIdentity,
) -> Result<ReleaseAttestationProof, String> {
    let (statement_sha256, statement) = verified_statement(value)?;
    let predicate_type = string_at(statement, "/predicateType")?;
    let predicate = object_at(statement, "/predicate")?;
    if predicate.get("repository").and_then(Value::as_str) != Some(&config.repository)
        || predicate.get("tag").and_then(Value::as_str) != Some(&release.tag_name)
    {
        return Err("release attestation predicate repository/tag mismatch".to_owned());
    }
    let release_id = value_as_u64(
        predicate
            .get("databaseId")
            .ok_or_else(|| "release attestation has no databaseId".to_owned())?,
    )?;
    let subjects = array_at(statement, "/subject")?;
    let expected_uri = format!("pkg:github/{}@{}", config.repository, release.tag_name);
    let mut saw_release_identity = false;
    let mut assets = BTreeMap::new();
    for subject in subjects {
        if let Some(uri) = subject.get("uri").and_then(Value::as_str) {
            if uri == expected_uri {
                if subject.pointer("/digest/sha1").and_then(Value::as_str)
                    != Some(tag.ref_sha.as_str())
                {
                    return Err("release attestation tag object digest mismatch".to_owned());
                }
                saw_release_identity = true;
            }
            continue;
        }
        let name = subject
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "release attestation contains an unidentified subject".to_owned())?;
        let digest = subject
            .pointer("/digest/sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("release attestation subject {name} has no SHA-256"))?;
        if assets.insert(name.to_owned(), digest.to_owned()).is_some() {
            return Err(format!("duplicate release attestation subject {name}"));
        }
    }
    if !saw_release_identity {
        return Err("release attestation has no matching tag identity subject".to_owned());
    }
    Ok(ReleaseAttestationProof {
        predicate_type: predicate_type.to_owned(),
        statement_sha256,
        release_id,
        tag: release.tag_name.clone(),
        ref_sha: tag.ref_sha.clone(),
        asset_digests: assets,
    })
}

pub(super) struct BuildAttestationContext<'a> {
    pub(super) config: &'a PulpDependencyConfig,
    pub(super) release: &'a ReleaseMetadata,
    pub(super) tag: &'a TagIdentity,
    pub(super) asset: &'a ReleaseAssetMetadata,
    pub(super) expected_receipt: Option<&'a BuildAttestationReceipt>,
}

pub(super) fn build_attestation(
    client: &GhClient,
    cwd: &Path,
    path: &Path,
    context: &BuildAttestationContext<'_>,
) -> Result<BuildAttestationReceipt, AttestationFailure> {
    let path_text = path.to_str().ok_or_else(|| {
        AttestationFailure::operational(failure("temporary asset path is not valid UTF-8"))
    })?;
    let expected_digest =
        release_asset_sha256(context.asset).map_err(AttestationFailure::rejected)?;
    let digest = format!("sha256:{expected_digest}");
    let exists = attestation_exists(
        client,
        cwd,
        &context.config.repository,
        &digest,
        SLSA_PROVENANCE_V1,
        "user",
    )
    .map_err(AttestationFailure::operational)?;
    if !exists {
        return Err(AttestationFailure::rejected(format!(
            "release asset {} has no build-provenance attestation",
            context.asset.name
        )));
    }
    let mut command = prepared_gh(client, cwd).map_err(AttestationFailure::operational)?;
    command.args(build_attestation_verify_args(path_text, context));
    let output = command.output().map_err(|error| {
        AttestationFailure::operational(failure(format!(
            "failed to start GitHub build-attestation verifier: {error}"
        )))
    })?;
    if !output.status.success() {
        let detail = output_detail(&output);
        let error = failure(format!(
            "GitHub build-attestation verification failed: {detail}"
        ));
        return Err(if build_attestation_policy_rejected(&detail) {
            AttestationFailure::Rejected(error)
        } else {
            AttestationFailure::Operational(error)
        });
    }
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        AttestationFailure::operational(failure(format!(
            "GitHub build-attestation verifier returned invalid JSON: {error}"
        )))
    })?;
    build_verifier_records(&value).map_err(|error| {
        AttestationFailure::operational(failure(format!(
            "GitHub build-attestation verifier output contract changed: {error}"
        )))
    })?;
    parse_build_attestation(&value, context).map_err(AttestationFailure::rejected)
}

pub(super) fn build_verifier_records(value: &Value) -> Result<&[Value], String> {
    let records = value
        .as_array()
        .ok_or_else(|| "result is not an array".to_owned())?;
    if records.is_empty() {
        return Err("successful result contains no verification records".to_owned());
    }
    for record in records {
        build_verifier_record_contract(record)?;
    }
    Ok(records)
}

pub(super) fn build_attestation_verify_args(
    path: &str,
    context: &BuildAttestationContext<'_>,
) -> Vec<String> {
    vec![
        "attestation".to_owned(),
        "verify".to_owned(),
        path.to_owned(),
        "--repo".to_owned(),
        context.config.repository.clone(),
        "--signer-workflow".to_owned(),
        context.config.signer_workflow.clone(),
        "--source-ref".to_owned(),
        format!("refs/tags/{}", context.release.tag_name),
        "--source-digest".to_owned(),
        context.tag.commit_sha.clone(),
        "--limit".to_owned(),
        "1000".to_owned(),
        "--format".to_owned(),
        "json".to_owned(),
    ]
}

pub(super) fn build_attestation_policy_rejected(detail: &str) -> bool {
    detail
        .to_ascii_lowercase()
        .contains("policy verification failed")
}

pub(super) fn parse_build_attestation(
    value: &Value,
    context: &BuildAttestationContext<'_>,
) -> Result<BuildAttestationReceipt, String> {
    let records = build_verifier_records(value)?;
    let expected_digest = release_asset_sha256(context.asset)?;
    let workflow_path = context
        .config
        .signer_workflow
        .strip_prefix(&format!("github.com/{}/", context.config.repository))
        .ok_or_else(|| "signer workflow is outside the configured repository".to_owned())?;

    let mut matches = Vec::new();
    for record in records {
        let Some(receipt) =
            matching_build_receipt(record, context, expected_digest, workflow_path)?
        else {
            continue;
        };
        if context.expected_receipt.is_none_or(|expected| {
            receipt.statement_sha256 == expected.statement_sha256
                && receipt.invocation_uri == expected.invocation_uri
        }) {
            matches.push(receipt);
        }
    }
    // Sorting makes selection stable when GitHub returns more than one valid
    // proof for the same immutable asset identity.
    matches.sort_by(|left, right| {
        left.statement_sha256
            .cmp(&right.statement_sha256)
            .then_with(|| left.invocation_uri.cmp(&right.invocation_uri))
    });
    matches.into_iter().next().ok_or_else(|| {
        let expected = context
            .expected_receipt
            .map_or_else(String::new, |receipt| {
                format!(
                    ", statement {}, invocation {}",
                    receipt.statement_sha256, receipt.invocation_uri
                )
            });
        format!(
            "no verified build attestation binds {} to {}, {}, {}{}",
            context.asset.name,
            context.release.tag_name,
            context.tag.commit_sha,
            context.config.signer_workflow,
            expected
        )
    })
}

fn matching_build_receipt(
    record: &Value,
    context: &BuildAttestationContext<'_>,
    expected_digest: &str,
    workflow_path: &str,
) -> Result<Option<BuildAttestationReceipt>, String> {
    let (statement_sha256, statement) = verified_statement(record)?;
    if string_at(statement, "/predicateType")? != "https://slsa.dev/provenance/v1" {
        return Ok(None);
    }
    let config = context.config;
    let release = context.release;
    let tag = context.tag;
    let asset = context.asset;
    let expected_ref = format!("refs/tags/{}", release.tag_name);
    let expected_repository = format!("https://github.com/{}", config.repository);
    let expected_builder = format!("https://{}@{expected_ref}", config.signer_workflow);
    let dependency_uri = format!("git+{expected_repository}@{expected_ref}");
    let mut subject_matches = false;
    for subject in array_at(statement, "/subject")? {
        let name = string_at(subject, "/name")?;
        let digest = string_at(subject, "/digest/sha256")?;
        if name == asset.name && digest == expected_digest {
            subject_matches = true;
        }
    }
    if !subject_matches
        || string_at(
            statement,
            "/predicate/buildDefinition/externalParameters/workflow/repository",
        )? != expected_repository
        || string_at(
            statement,
            "/predicate/buildDefinition/externalParameters/workflow/ref",
        )? != expected_ref
        || string_at(
            statement,
            "/predicate/buildDefinition/externalParameters/workflow/path",
        )? != workflow_path
        || string_at(statement, "/predicate/runDetails/builder/id")? != expected_builder
    {
        return Ok(None);
    }
    let mut dependency_matches = false;
    for dependency in array_at(statement, "/predicate/buildDefinition/resolvedDependencies")? {
        let uri = string_at(dependency, "/uri")?;
        let digest = string_at(dependency, "/digest/gitCommit")?;
        if uri == dependency_uri && digest == tag.commit_sha {
            dependency_matches = true;
        }
    }
    if !dependency_matches {
        return Ok(None);
    }
    let invocation_uri = string_at(statement, "/predicate/runDetails/metadata/invocationId")?;
    if !invocation_uri.starts_with(&format!(
        "https://github.com/{}/actions/runs/",
        config.repository
    )) || !invocation_uri.contains("/attempts/")
    {
        return Ok(None);
    }
    Ok(Some(BuildAttestationReceipt {
        asset: asset.name.clone(),
        subject_sha256: expected_digest.to_owned(),
        predicate_type: "https://slsa.dev/provenance/v1".to_owned(),
        signer_workflow: config.signer_workflow.clone(),
        source_repository: config.repository.clone(),
        source_ref: expected_ref,
        source_commit: tag.commit_sha.clone(),
        statement_sha256,
        invocation_uri: invocation_uri.to_owned(),
    }))
}

// The verifier's top-level success is not enough: its decoded statement is an
// external schema boundary. Validate every field whose absence or type would
// otherwise be indistinguishable from a signed identity mismatch. Callers map
// these contract failures to Operational, which prevents latest-qualified
// polling from silently falling back to an older release during schema drift.
pub(super) fn release_verifier_record_contract(value: &Value) -> Result<(), String> {
    verifier_record_contract(value)?;
    let (_, statement) = verified_statement(value)?;
    string_at(statement, "/predicateType")?;
    object_at(statement, "/predicate")?;
    string_at(statement, "/predicate/repository")?;
    string_at(statement, "/predicate/tag")?;
    value_as_u64(
        statement
            .pointer("/predicate/databaseId")
            .ok_or_else(|| "release attestation has no databaseId".to_owned())?,
    )?;
    let subjects = array_at(statement, "/subject")?;
    for (index, subject) in subjects.iter().enumerate() {
        if !subject.is_object() {
            return Err(format!("release subject {index} is not an object"));
        }
        object_at(subject, "/digest")?;
        let mut identified = false;
        if subject.get("uri").is_some() {
            string_at(subject, "/uri")?;
            string_at(subject, "/digest/sha1")?;
            identified = true;
        }
        if subject.get("name").is_some() {
            string_at(subject, "/name")?;
            string_at(subject, "/digest/sha256")?;
            identified = true;
        }
        if !identified {
            return Err(format!(
                "release subject {index} has neither a uri nor a name"
            ));
        }
    }
    Ok(())
}

fn build_verifier_record_contract(value: &Value) -> Result<(), String> {
    verifier_record_contract(value)?;
    let (_, statement) = verified_statement(value)?;
    string_at(statement, "/predicateType")?;
    for (index, subject) in array_at(statement, "/subject")?.iter().enumerate() {
        if !subject.is_object() {
            return Err(format!("build subject {index} is not an object"));
        }
        string_at(subject, "/name")?;
        string_at(subject, "/digest/sha256")?;
    }
    object_at(statement, "/predicate")?;
    object_at(statement, "/predicate/buildDefinition")?;
    object_at(statement, "/predicate/buildDefinition/externalParameters")?;
    object_at(
        statement,
        "/predicate/buildDefinition/externalParameters/workflow",
    )?;
    string_at(
        statement,
        "/predicate/buildDefinition/externalParameters/workflow/repository",
    )?;
    string_at(
        statement,
        "/predicate/buildDefinition/externalParameters/workflow/ref",
    )?;
    string_at(
        statement,
        "/predicate/buildDefinition/externalParameters/workflow/path",
    )?;
    for (index, dependency) in
        array_at(statement, "/predicate/buildDefinition/resolvedDependencies")?
            .iter()
            .enumerate()
    {
        if !dependency.is_object() {
            return Err(format!("build dependency {index} is not an object"));
        }
        string_at(dependency, "/uri")?;
        string_at(dependency, "/digest/gitCommit")?;
    }
    object_at(statement, "/predicate/runDetails")?;
    object_at(statement, "/predicate/runDetails/builder")?;
    string_at(statement, "/predicate/runDetails/builder/id")?;
    object_at(statement, "/predicate/runDetails/metadata")?;
    string_at(statement, "/predicate/runDetails/metadata/invocationId")?;
    Ok(())
}

fn verified_statement(value: &Value) -> Result<(String, &Value), String> {
    let payload = value
        .pointer("/attestation/bundle/dsseEnvelope/payload")
        .and_then(Value::as_str)
        .ok_or_else(|| "verified attestation has no DSSE payload".to_owned())?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|error| format!("verified attestation has invalid DSSE payload: {error}"))?;
    let decoded_value: Value = serde_json::from_slice(&decoded)
        .map_err(|error| format!("verified attestation payload is not JSON: {error}"))?;
    let statement = value
        .pointer("/verificationResult/statement")
        .ok_or_else(|| "attestation has no verified statement".to_owned())?;
    if &decoded_value != statement {
        return Err("verified statement does not match the signed DSSE payload".to_owned());
    }
    Ok((sha256_hex(&decoded), statement))
}

pub(super) fn verifier_record_contract(value: &Value) -> Result<(), String> {
    let payload = value
        .pointer("/attestation/bundle/dsseEnvelope/payload")
        .and_then(Value::as_str)
        .ok_or_else(|| "record has no DSSE payload string".to_owned())?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|error| format!("record has invalid DSSE payload encoding: {error}"))?;
    let decoded_value: Value = serde_json::from_slice(&decoded)
        .map_err(|error| format!("record DSSE payload is not JSON: {error}"))?;
    if !decoded_value.is_object() {
        return Err("record DSSE payload is not a JSON object".to_owned());
    }
    if !value
        .pointer("/verificationResult/statement")
        .is_some_and(Value::is_object)
    {
        return Err("record has no verified statement object".to_owned());
    }
    Ok(())
}

fn object_at<'a>(
    value: &'a Value,
    pointer: &str,
) -> Result<&'a serde_json::Map<String, Value>, String> {
    value
        .pointer(pointer)
        .and_then(Value::as_object)
        .ok_or_else(|| format!("attestation field {pointer} is not an object"))
}

fn array_at<'a>(value: &'a Value, pointer: &str) -> Result<&'a Vec<Value>, String> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("attestation field {pointer} is not an array"))
}

fn string_at<'a>(value: &'a Value, pointer: &str) -> Result<&'a str, String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("attestation field {pointer} is not a string"))
}

fn value_as_u64(value: &Value) -> Result<u64, String> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| "attestation databaseId is not an unsigned integer".to_owned())
}
