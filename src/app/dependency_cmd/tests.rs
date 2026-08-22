use super::consumer_pr::{
    new_branch_lease, parse_github_app_identity, pin_pr_body, pin_pr_title, validate_pin_pr,
};
use super::github::{
    BuildAttestationContext, attestation_inventory_has_initiator,
    build_attestation_policy_rejected, build_attestation_verify_args, build_verifier_records,
    parse_build_attestation, parse_release_attestation, release_assets_from_pages,
    release_attestation_policy_rejected, release_candidates_from_pages,
    release_verifier_record_contract, verifier_record_contract,
};
use super::*;
use base64::Engine as _;
use serde_json::Value;

fn config() -> PulpDependencyConfig {
    PulpDependencyConfig {
        repository: "Generous-Corp/pulp".to_owned(),
        channel: DependencyChannel::LatestQualified,
        required_assets: vec!["pulp-sdk-darwin-arm64.tar.gz".to_owned()],
        manifest_asset: "SHA256SUMS".to_owned(),
        signer_workflow: "github.com/Generous-Corp/pulp/.github/workflows/release-cli.yml"
            .to_owned(),
        lock_file: PathBuf::from(".shipyard/dependencies/pulp.lock.json"),
        stable_tag: None,
        fixed_tag: None,
        fixed_commit: None,
        base_branch: "main".to_owned(),
    }
}

fn release() -> ReleaseMetadata {
    ReleaseMetadata {
        id: 42,
        tag_name: "v1.2.3".to_owned(),
        draft: false,
        prerelease: false,
        published_at: Some("2026-08-21T00:00:00Z".to_owned()),
        assets: vec![ReleaseAssetMetadata {
            id: 7,
            name: "pulp-sdk-darwin-arm64.tar.gz".to_owned(),
            state: "uploaded".to_owned(),
            digest: Some(format!("sha256:{}", "a".repeat(64))),
            size: 10,
            download_url: "https://github.com/Generous-Corp/pulp/releases/download/v1.2.3/pulp-sdk-darwin-arm64.tar.gz".to_owned(),
        }],
    }
}

fn tag() -> TagIdentity {
    TagIdentity {
        ref_sha: "b".repeat(40),
        commit_sha: "c".repeat(40),
    }
}

fn app_identity() -> GitHubAppIdentity {
    GitHubAppIdentity {
        login: "shipyard-local[bot]".to_owned(),
        database_id: 288_178_668,
    }
}

fn parse_build(
    value: &Value,
    expected_receipt: Option<&BuildAttestationReceipt>,
) -> Result<BuildAttestationReceipt, String> {
    let config = config();
    let release = release();
    let tag = tag();
    let context = BuildAttestationContext {
        config: &config,
        release: &release,
        tag: &tag,
        asset: &release.assets[0],
        expected_receipt,
    };
    parse_build_attestation(value, &context)
}

fn verified_record(statement: &Value) -> Value {
    let bytes = serde_json::to_vec(&statement).expect("statement JSON");
    serde_json::json!({
        "attestation": {
            "bundle": {
                "dsseEnvelope": {
                    "payload": base64::engine::general_purpose::STANDARD.encode(bytes)
                }
            }
        },
        "verificationResult": { "statement": statement.clone() }
    })
}

fn release_record() -> Value {
    verified_record(&serde_json::json!({
        "subject": [
            {
                "uri": "pkg:github/Generous-Corp/pulp@v1.2.3",
                "digest": { "sha1": "b".repeat(40) }
            },
            {
                "name": "pulp-sdk-darwin-arm64.tar.gz",
                "digest": { "sha256": "a".repeat(64) }
            }
        ],
        "predicateType": "https://in-toto.io/attestation/release/v0.2",
        "predicate": {
            "databaseId": "42",
            "repository": "Generous-Corp/pulp",
            "tag": "v1.2.3"
        }
    }))
}

fn build_record() -> Value {
    verified_record(&serde_json::json!({
        "subject": [{
            "name": "pulp-sdk-darwin-arm64.tar.gz",
            "digest": { "sha256": "a".repeat(64) }
        }],
        "predicateType": "https://slsa.dev/provenance/v1",
        "predicate": {
            "buildDefinition": {
                "externalParameters": { "workflow": {
                    "repository": "https://github.com/Generous-Corp/pulp",
                    "ref": "refs/tags/v1.2.3",
                    "path": ".github/workflows/release-cli.yml"
                }},
                "resolvedDependencies": [{
                    "uri": "git+https://github.com/Generous-Corp/pulp@refs/tags/v1.2.3",
                    "digest": { "gitCommit": "c".repeat(40) }
                }]
            },
            "runDetails": {
                "builder": { "id": "https://github.com/Generous-Corp/pulp/.github/workflows/release-cli.yml@refs/tags/v1.2.3" },
                "metadata": { "invocationId": "https://github.com/Generous-Corp/pulp/actions/runs/1/attempts/1" }
            }
        }
    }))
}

#[test]
fn release_attestation_parser_binds_signed_identity_and_assets() {
    let proof = parse_release_attestation(&release_record(), &config(), &release(), &tag())
        .expect("release proof");
    assert_eq!(proof.release_id, 42);
    assert_eq!(proof.ref_sha, "b".repeat(40));
    assert_eq!(
        proof.asset_digests["pulp-sdk-darwin-arm64.tar.gz"],
        "a".repeat(64)
    );
}

#[test]
fn attestation_parser_rejects_unsigned_statement_substitution() {
    let mut record = release_record();
    record["verificationResult"]["statement"]["predicate"]["tag"] = Value::from("v9.9.9");
    assert!(
        parse_release_attestation(&record, &config(), &release(), &tag())
            .unwrap_err()
            .contains("signed DSSE payload")
    );
}

#[test]
fn build_attestation_parser_binds_workflow_tag_commit_and_asset() {
    let value = Value::Array(vec![build_record()]);
    let receipt = parse_build(&value, None).expect("build proof");
    assert_eq!(receipt.source_commit, "c".repeat(40));

    let mut wrong = build_record();
    wrong["verificationResult"]["statement"]["predicate"]["buildDefinition"]["resolvedDependencies"]
        [0]["digest"]["gitCommit"] = Value::from("d".repeat(40));
    let decoded_wrong = wrong["verificationResult"]["statement"].clone();
    wrong = verified_record(&decoded_wrong);
    assert!(
        parse_build(&Value::Array(vec![wrong]), None)
            .unwrap_err()
            .contains("no verified build attestation")
    );

    let mut wrong_invocation = build_record();
    wrong_invocation["verificationResult"]["statement"]["predicate"]["runDetails"]["metadata"]["invocationId"] =
        Value::from("https://github.com/attacker/pulp/actions/runs/1/attempts/1");
    let decoded_wrong = wrong_invocation["verificationResult"]["statement"].clone();
    wrong_invocation = verified_record(&decoded_wrong);
    assert!(
        parse_build(&Value::Array(vec![wrong_invocation]), None)
            .unwrap_err()
            .contains("no verified build attestation")
    );
}

#[test]
fn build_attestation_selection_is_deterministic_and_can_match_a_tracked_receipt() {
    let first = build_record();
    let mut second = build_record();
    second["verificationResult"]["statement"]["predicate"]["runDetails"]["metadata"]["invocationId"] =
        Value::from("https://github.com/Generous-Corp/pulp/actions/runs/2/attempts/1");
    let decoded_second = second["verificationResult"]["statement"].clone();
    second = verified_record(&decoded_second);

    let forward = parse_build(&Value::Array(vec![first.clone(), second.clone()]), None)
        .expect("deterministic forward selection");
    let reverse = parse_build(&Value::Array(vec![second.clone(), first.clone()]), None)
        .expect("deterministic reverse selection");
    assert_eq!(forward, reverse);

    let tracked = parse_build(&Value::Array(vec![first]), None).expect("tracked receipt");
    let reproduced = parse_build(
        &Value::Array(vec![second.clone(), build_record()]),
        Some(&tracked),
    )
    .expect("tracked receipt remains selectable");
    assert_eq!(reproduced, tracked);

    let error = parse_build(&Value::Array(vec![second]), Some(&tracked))
        .expect_err("a different valid attestation cannot replace the tracked receipt");
    assert!(error.contains(&tracked.statement_sha256));
}

#[test]
fn qualification_cache_key_changes_with_release_asset_identity() {
    let config = config();
    let release = release();
    let tag = tag();
    let proof = parse_release_attestation(&release_record(), &config, &release, &tag)
        .expect("release proof");
    let first =
        qualification_cache_key(&config, &release, &tag, &proof, b"manifest").expect("first key");
    let mut changed = release.clone();
    changed.assets[0].digest = Some(format!("sha256:{}", "d".repeat(64)));
    let second =
        qualification_cache_key(&config, &changed, &tag, &proof, b"manifest").expect("second key");
    assert_ne!(first, second);
}

#[test]
fn latest_qualified_discovery_includes_candidates_after_the_first_api_page() {
    let mut rejected = release();
    rejected.tag_name = "v9.9.9".to_owned();
    rejected.draft = true;
    let first_page = vec![rejected; 100];
    let mut qualified = release();
    qualified.tag_name = "v1.2.3".to_owned();

    let candidates = release_candidates_from_pages(vec![first_page, vec![qualified]]);
    assert_eq!(
        candidates
            .iter()
            .map(|release| release.tag_name.as_str())
            .collect::<Vec<_>>(),
        ["v1.2.3"]
    );
}

#[test]
fn authoritative_asset_inventory_combines_every_api_page() {
    let asset = release().assets.remove(0);
    let mut first_page = vec![asset.clone(); 100];
    for (index, asset) in first_page.iter_mut().enumerate() {
        asset.id = u64::try_from(index + 1).expect("asset id");
        asset.name = format!("asset-{index}.tar.gz");
    }
    let mut final_asset = asset;
    final_asset.id = 101;
    final_asset.name = "asset-100.tar.gz".to_owned();

    let assets = release_assets_from_pages(vec![first_page, vec![final_asset]]);
    assert_eq!(assets.len(), 101);
    assert_eq!(assets.last().map(|asset| asset.id), Some(101));
}

fn cached_lock_fixture() -> PulpDependencyLock {
    serde_json::from_value(serde_json::json!({
        "schema": "shipyard.pulp-dependency-lock.v1",
        "dependency": "pulp",
        "channel": "latest-qualified",
        "repository": "Generous-Corp/pulp",
        "tag": "v1.2.3",
        "tag_ref_sha": "b".repeat(40),
        "commit_sha": "c".repeat(40),
        "release_id": 42,
        "published_at": "2026-08-21T00:00:00Z",
        "release_assets": [
            {
                "id": 7,
                "name": "pulp-sdk-darwin-arm64.tar.gz",
                "sha256": "a".repeat(64),
                "size": 10,
                "download_url": "https://github.com/Generous-Corp/pulp/releases/download/v1.2.3/pulp-sdk-darwin-arm64.tar.gz"
            },
            {
                "id": 8,
                "name": "SHA256SUMS",
                "sha256": "f".repeat(64),
                "size": 80,
                "download_url": "https://github.com/Generous-Corp/pulp/releases/download/v1.2.3/SHA256SUMS"
            }
        ],
        "manifest": { "name": "SHA256SUMS", "sha256": "f".repeat(64) },
        "release_attestation": {
            "predicate_type": "https://in-toto.io/attestation/release/v0.2",
            "statement_sha256": "d".repeat(64)
        },
        "build_attestations": [{
            "asset": "pulp-sdk-darwin-arm64.tar.gz",
            "subject_sha256": "a".repeat(64),
            "predicate_type": "https://slsa.dev/provenance/v1",
            "signer_workflow": "github.com/Generous-Corp/pulp/.github/workflows/release-cli.yml",
            "source_repository": "Generous-Corp/pulp",
            "source_ref": "refs/tags/v1.2.3",
            "source_commit": "c".repeat(40),
            "statement_sha256": "e".repeat(64),
            "invocation_uri": "https://github.com/Generous-Corp/pulp/actions/runs/1/attempts/1"
        }]
    }))
    .expect("lock fixture")
}

#[test]
fn operational_qualification_failure_never_falls_back_to_an_older_release() {
    let mut newest = release();
    newest.tag_name = "v2.0.0".to_owned();
    let mut older = release();
    older.tag_name = "v1.9.0".to_owned();
    let mut attempted = Vec::new();
    let error = select_qualified_candidate(vec![newest, older], false, |release| {
        attempted.push(release.tag_name.clone());
        if release.tag_name == "v2.0.0" {
            Err(QualificationFailure::operational(failure(
                "transient download failure",
            )))
        } else {
            Ok(cached_lock_fixture())
        }
    })
    .expect_err("operational errors must abort selection");
    assert!(error.message().contains("transient download failure"));
    assert_eq!(attempted, ["v2.0.0"]);
}

#[test]
fn verifier_failure_classification_never_treats_transport_as_candidate_rejection() {
    assert!(build_attestation_policy_rejected(
        "Error: Policy verification failed: source ref mismatch"
    ));
    assert!(!build_attestation_policy_rejected(
        "Error: Loading attestations from GitHub API failed: connection reset"
    ));
    assert!(!build_attestation_policy_rejected(
        "Error: Sigstore verification failed: trust root timed out"
    ));
    assert!(release_attestation_policy_rejected(
        "duplicate attestations found for release v1.2.3"
    ));
    assert!(!release_attestation_policy_rejected(
        "failed to verify attestations for tag v1.2.3: trust root timeout"
    ));
}

#[test]
fn empty_attestation_inventory_is_distinct_from_an_operational_parse_failure() {
    assert_eq!(
        attestation_inventory_has_initiator(br#"[{"attestations":[]}]"#, "user"),
        Ok(false)
    );
    assert!(attestation_inventory_has_initiator(b"not-json", "user").is_err());
    assert!(
        attestation_inventory_has_initiator(br"[{}]", "user")
            .expect_err("missing inventory field is contract drift")
            .contains("missing field")
    );
}

#[test]
fn verifier_schema_drift_is_an_output_contract_failure() {
    let mut record = build_record();
    record
        .get_mut("verificationResult")
        .expect("verification result")
        .as_object_mut()
        .expect("verification result object")
        .remove("statement");

    let error = verifier_record_contract(&record)
        .expect_err("missing verified statement must be operational contract drift");
    assert!(error.contains("verified statement"));
}

#[test]
fn inner_release_schema_drift_is_operational_contract_failure() {
    let mut missing_database_id = release_record()["verificationResult"]["statement"].clone();
    missing_database_id["predicate"]
        .as_object_mut()
        .expect("predicate")
        .remove("databaseId");

    let mut wrong_repository_type = release_record()["verificationResult"]["statement"].clone();
    wrong_repository_type["predicate"]["repository"] = Value::from(17);

    let mut missing_subject_digest = release_record()["verificationResult"]["statement"].clone();
    missing_subject_digest["subject"][1]["digest"]
        .as_object_mut()
        .expect("asset digest")
        .remove("sha256");

    for statement in [
        missing_database_id,
        wrong_repository_type,
        missing_subject_digest,
    ] {
        let error = release_verifier_record_contract(&verified_record(&statement))
            .expect_err("inner schema drift must abort rather than reject a candidate");
        assert!(!error.is_empty());
    }
}

#[test]
fn signed_release_identity_mismatch_remains_a_candidate_rejection() {
    let mut statement = release_record()["verificationResult"]["statement"].clone();
    statement["predicate"]["repository"] = Value::from("attacker/pulp");
    let record = verified_record(&statement);

    release_verifier_record_contract(&record).expect("identity mismatch retains valid schema");
    assert!(
        parse_release_attestation(&record, &config(), &release(), &tag())
            .expect_err("signed identity mismatch must reject the candidate")
            .contains("repository/tag mismatch")
    );
}

#[test]
fn malformed_inner_build_record_aborts_the_whole_verifier_result() {
    let mut statement = build_record()["verificationResult"]["statement"].clone();
    statement["predicate"]["runDetails"]["metadata"]
        .as_object_mut()
        .expect("metadata")
        .remove("invocationId");
    let malformed = verified_record(&statement);

    let error = build_verifier_records(&Value::Array(vec![build_record(), malformed]))
        .expect_err("one malformed record must not be dropped by identity filtering");
    assert!(error.contains("invocationId"));
}

#[test]
fn successful_empty_build_verifier_output_is_contract_drift() {
    let error = build_verifier_records(&Value::Array(Vec::new()))
        .expect_err("successful empty output must abort qualification");
    assert!(error.contains("no verification records"));
}

#[test]
fn build_verifier_binds_certificate_source_ref_and_digest() {
    let config = config();
    let release = release();
    let tag = tag();
    let context = BuildAttestationContext {
        config: &config,
        release: &release,
        tag: &tag,
        asset: &release.assets[0],
        expected_receipt: None,
    };

    let args = build_attestation_verify_args("/tmp/sdk.tar.gz", &context);

    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--source-ref" && pair[1] == "refs/tags/v1.2.3")
    );
    let commit = "c".repeat(40);
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--source-digest" && pair[1] == commit)
    );
}

#[test]
fn deterministic_rejection_can_advance_to_the_next_qualified_release() {
    let mut newest = release();
    newest.tag_name = "v2.0.0".to_owned();
    let mut older = release();
    older.tag_name = "v1.9.0".to_owned();
    let mut attempted = Vec::new();
    let selected = select_qualified_candidate(vec![newest, older], false, |release| {
        attempted.push(release.tag_name.clone());
        if release.tag_name == "v2.0.0" {
            Err(QualificationFailure::rejected("missing required asset"))
        } else {
            let mut lock = cached_lock_fixture();
            lock.tag.clone_from(&release.tag_name);
            Ok(lock)
        }
    })
    .expect("deterministically rejected candidate may be skipped");
    assert_eq!(selected.tag, "v1.9.0");
    assert_eq!(attempted, ["v2.0.0", "v1.9.0"]);
}

#[test]
fn qualification_cache_only_reproduces_an_exact_tracked_release_identity() {
    let tracked = cached_lock_fixture();
    assert!(reusable_cached_lock(None, Ok(tracked.clone())).is_none());
    assert_eq!(
        reusable_cached_lock(Some(&tracked), Ok(tracked.clone())),
        Some(tracked.clone())
    );

    let mut different_proof = tracked.clone();
    different_proof.build_attestations[0].statement_sha256 = "0".repeat(64);
    assert!(reusable_cached_lock(Some(&tracked), Ok(different_proof)).is_none());
}

#[test]
fn dependency_branch_binds_the_complete_rendered_lock() {
    let commit = "c".repeat(40);
    let base = "b".repeat(40);
    let first = dependency_branch("v1.2.3", &commit, &base, b"first lock\n");
    let second = dependency_branch("v1.2.3", &commit, &base, b"second lock\n");
    let moved_base = dependency_branch("v1.2.3", &commit, &"d".repeat(40), b"first lock\n");
    assert_ne!(first, second);
    assert_ne!(first, moved_base);
    assert!(first.starts_with("shipyard/pulp-1.2.3-cccccccccccc-bbbbbbbbbbbb-"));
}

#[test]
fn app_identity_and_existing_pr_envelope_are_exact() {
    let app = app_identity();
    let parsed = parse_github_app_identity(&serde_json::json!({
        "data": { "viewer": { "login": app.login.clone(), "databaseId": app.database_id } }
    }))
    .expect("App viewer identity");
    assert_eq!(parsed, app);
    assert!(
        parse_github_app_identity(&serde_json::json!({
            "data": { "viewer": { "login": "human", "databaseId": 1 } }
        }))
        .is_err()
    );

    let lock = cached_lock_fixture();
    let config = config();
    let base_sha = "b".repeat(40);
    let head_sha = "c".repeat(40);
    let branch = "shipyard/pulp-1.2.3-head-base-lock";
    let client = GhClient::ambient();
    let publication = PinPublication {
        client: &client,
        cwd: Path::new("/consumer"),
        repo: "Generous-Corp/consumer",
        config: &config,
        lock: &lock,
        branch,
        lock_bytes: b"lock",
        base_sha: &base_sha,
        app: &app,
    };
    let exact = serde_json::json!({
        "number": 42,
        "html_url": "https://github.com/Generous-Corp/consumer/pull/42",
        "state": "open",
        "draft": false,
        "title": pin_pr_title(&lock),
        "body": pin_pr_body(&lock, &base_sha),
        "user": { "login": app.login.clone(), "id": app.database_id, "type": "Bot" },
        "head": {
            "ref": branch,
            "sha": head_sha.clone(),
            "repo": { "full_name": "Generous-Corp/consumer" }
        },
        "base": { "ref": config.base_branch.clone(), "sha": base_sha.clone() }
    });
    validate_pin_pr(&exact, &publication, &head_sha).expect("exact App-authored PR");

    let mut attacker = exact;
    attacker["user"]["login"] = Value::from("attacker");
    assert!(
        validate_pin_pr(&attacker, &publication, &head_sha)
            .unwrap_err()
            .message()
            .contains("pinned GitHub App actor")
    );
}

#[test]
fn dependency_branch_push_uses_an_absence_lease() {
    assert_eq!(
        new_branch_lease("shipyard/pulp-1.2.3"),
        "--force-with-lease=refs/heads/shipyard/pulp-1.2.3:"
    );
}

#[cfg(unix)]
#[test]
fn dependency_commit_disables_repository_hooks_and_keeps_an_exact_diff() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().expect("tempdir");
    let git = |args: &[&str]| {
        let output = crate::supervised::git_supervised()
            .args(args)
            .current_dir(temp.path())
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    git(&["init", "-b", "main"]);
    std::fs::write(temp.path().join("README.md"), "fixture\n").expect("readme");
    let hooks = temp.path().join(".githooks");
    std::fs::create_dir(&hooks).expect("hooks directory");
    let hook = hooks.join("pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf 'ran\\n' > hook-ran\ngit add hook-ran\n",
    )
    .expect("hook");
    let mut permissions = std::fs::metadata(&hook)
        .expect("hook metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&hook, permissions).expect("hook permissions");
    git(&["add", "README.md", ".githooks/pre-commit"]);
    git(&[
        "-c",
        "user.name=fixture",
        "-c",
        "user.email=fixture@example.com",
        "commit",
        "-m",
        "fixture",
    ]);
    git(&["config", "core.hooksPath", ".githooks"]);
    let parent = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
        .expect("parent SHA")
        .trim()
        .to_owned();

    let lock_file = Path::new(".shipyard/dependencies/pulp.lock.json");
    let lock_bytes = b"{\"qualified\":true}\n";
    atomic_write(&temp.path().join(lock_file), lock_bytes).expect("lock write");
    let auth_dir = tempfile::tempdir().expect("auth dir");
    std::fs::write(
        auth_dir.path().join("config.toml"),
        format!(
            "[github.auth]\nprivileged_git_binary = {}\n",
            toml::Value::String("/usr/bin/git".to_owned())
        ),
    )
    .expect("auth config");
    let config = LoadedConfig::load_machine_global_from_dir(auth_dir.path().to_path_buf())
        .expect("load auth config");
    let client = GhClient::from_loaded_config(&config).expect("trusted git client");
    commit_lock(
        &client,
        temp.path(),
        lock_file,
        "v1.2.3",
        lock_bytes,
        &parent,
        &app_identity(),
    )
    .expect("verified dependency commit");
    assert!(!temp.path().join("hook-ran").exists());
}

#[test]
fn dependency_mutations_require_github_app_installation_auth() {
    let ambient = GhAuthSummary {
        source: GhAuthSourceSummary::GhCli,
        token_kind: None,
        expires_at: None,
    };
    assert!(validate_github_app_auth(&ambient).is_err());
    let wrong_helper = GhAuthSummary {
        source: GhAuthSourceSummary::Command,
        token_kind: Some("oauth".to_owned()),
        expires_at: None,
    };
    assert!(validate_github_app_auth(&wrong_helper).is_err());
    let app = GhAuthSummary {
        source: GhAuthSourceSummary::Command,
        token_kind: Some("github-app-installation".to_owned()),
        expires_at: None,
    };
    validate_github_app_auth(&app).expect("GitHub App authority");
}

#[cfg(unix)]
#[test]
fn proof_bearing_repo_root_uses_the_configured_trusted_git() {
    let config = LoadedConfig {
        data: r#"
            [github.auth]
            privileged_git_binary = "/usr/bin/false"
            "#
        .parse::<toml::Table>()
        .expect("trusted Git config"),
        global_dir: PathBuf::from("/tmp/shipyard-global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: crate::config::LocalOverlaySource::None,
    };
    let client = GhClient::from_loaded_config(&config).expect("trusted Git client");

    let error = trusted_repo_root(&client, Path::new("/tmp"))
        .expect_err("configured failing Git must control root discovery");
    assert!(
        error
            .message
            .contains("trusted git rev-parse --show-toplevel")
    );
}
