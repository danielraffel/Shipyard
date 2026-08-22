use super::*;

fn config(channel: DependencyChannel) -> PulpDependencyConfig {
    PulpDependencyConfig {
        repository: "Generous-Corp/pulp".to_owned(),
        channel,
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

fn release(tag: &str) -> ReleaseMetadata {
    let sdk_digest = "a".repeat(64);
    let manifest = format!("{sdk_digest}  pulp-sdk-darwin-arm64.tar.gz\n");
    ReleaseMetadata {
        id: 42,
        tag_name: tag.to_owned(),
        draft: false,
        prerelease: false,
        published_at: Some("2026-08-21T00:00:00Z".to_owned()),
        assets: vec![
            ReleaseAssetMetadata {
                id: 1,
                name: "pulp-sdk-darwin-arm64.tar.gz".to_owned(),
                state: "uploaded".to_owned(),
                digest: Some(format!("sha256:{sdk_digest}")),
                size: 123,
                download_url: format!(
                    "https://github.com/Generous-Corp/pulp/releases/download/{tag}/pulp-sdk-darwin-arm64.tar.gz"
                ),
            },
            ReleaseAssetMetadata {
                id: 2,
                name: "SHA256SUMS".to_owned(),
                state: "uploaded".to_owned(),
                digest: Some(format!("sha256:{}", sha256_hex(manifest.as_bytes()))),
                size: u64::try_from(manifest.len()).expect("manifest size"),
                download_url: format!(
                    "https://github.com/Generous-Corp/pulp/releases/download/{tag}/SHA256SUMS"
                ),
            },
        ],
    }
}

fn tag() -> TagIdentity {
    TagIdentity {
        ref_sha: "b".repeat(40),
        commit_sha: "c".repeat(40),
    }
}

fn release_proof(release: &ReleaseMetadata, tag: &TagIdentity) -> ReleaseAttestationProof {
    ReleaseAttestationProof {
        predicate_type: RELEASE_PREDICATE.to_owned(),
        statement_sha256: "d".repeat(64),
        release_id: release.id,
        tag: release.tag_name.clone(),
        ref_sha: tag.ref_sha.clone(),
        asset_digests: release
            .assets
            .iter()
            .map(|asset| {
                (
                    asset.name.clone(),
                    asset
                        .digest
                        .as_deref()
                        .expect("digest")
                        .trim_start_matches("sha256:")
                        .to_owned(),
                )
            })
            .collect(),
    }
}

fn build_proof(
    config: &PulpDependencyConfig,
    release: &ReleaseMetadata,
) -> Vec<BuildAttestationReceipt> {
    vec![BuildAttestationReceipt {
        asset: config.required_assets[0].clone(),
        subject_sha256: "a".repeat(64),
        predicate_type: BUILD_PREDICATE.to_owned(),
        signer_workflow: config.signer_workflow.clone(),
        source_repository: config.repository.clone(),
        source_ref: format!("refs/tags/{}", release.tag_name),
        source_commit: "c".repeat(40),
        statement_sha256: "e".repeat(64),
        invocation_uri: "https://github.com/Generous-Corp/pulp/actions/runs/1/attempts/1"
            .to_owned(),
    }]
}

fn qualify(
    config: &PulpDependencyConfig,
    release: &ReleaseMetadata,
) -> Result<PulpDependencyLock, String> {
    let tag = tag();
    let proof = release_proof(release, &tag);
    let manifest = format!("{}  pulp-sdk-darwin-arm64.tar.gz\n", "a".repeat(64));
    qualify_pulp_release(
        config,
        release,
        &tag,
        &proof,
        manifest.as_bytes(),
        &build_proof(config, release),
    )
}

#[test]
fn tracked_channel_is_required_and_never_defaults_unrelated_repos() {
    let text = r#"
[dependencies.pulp]
repository = "Generous-Corp/pulp"
required_assets = ["pulp-sdk-darwin-arm64.tar.gz"]
signer_workflow = "github.com/Generous-Corp/pulp/.github/workflows/release-cli.yml"
"#;
    assert!(toml::from_str::<TrackedConfig>(text).is_err());
    assert!(version_tuple("main").is_err());
    assert!(version_tuple("v1.2.3-rc.1").is_err());
}

#[test]
fn draft_and_prerelease_never_qualify() {
    let config = config(DependencyChannel::LatestQualified);
    let mut candidate = release("v1.2.3");
    candidate.draft = true;
    assert!(qualify(&config, &candidate).unwrap_err().contains("draft"));
    candidate.draft = false;
    candidate.prerelease = true;
    assert!(
        qualify(&config, &candidate)
            .unwrap_err()
            .contains("prerelease")
    );
}

#[test]
fn incomplete_or_changed_asset_sets_fail_closed() {
    let config = config(DependencyChannel::LatestQualified);
    let mut candidate = release("v1.2.3");
    candidate.assets.remove(0);
    let error = qualify(&config, &candidate).expect_err("missing SDK must fail");
    assert!(error.contains("manifest") || error.contains("attestation"));

    let candidate = release("v1.2.3");
    let tag = tag();
    let mut proof = release_proof(&candidate, &tag);
    proof.asset_digests.remove("SHA256SUMS");
    let manifest = format!("{}  pulp-sdk-darwin-arm64.tar.gz\n", "a".repeat(64));
    let error = qualify_pulp_release(
        &config,
        &candidate,
        &tag,
        &proof,
        manifest.as_bytes(),
        &build_proof(&config, &candidate),
    )
    .expect_err("changed attested asset set must fail");
    assert!(error.contains("asset set"));
}

#[test]
fn same_version_identity_swap_is_rejected() {
    let config = config(DependencyChannel::LatestQualified);
    let current = qualify(&config, &release("v1.2.3")).expect("current lock");
    let mut replacement = current.clone();
    replacement.commit_sha = "f".repeat(40);
    replacement.build_attestations[0].source_commit = "f".repeat(40);
    let error = validate_lock_transition(Some(&current), &replacement)
        .expect_err("identity swap must fail");
    assert!(error.contains("same-version identity swap"));
}

#[test]
fn policy_rejects_noncanonical_tags_and_ambiguous_workflow_paths() {
    assert!(version_tuple("v01.2.3").is_err());
    let mut policy = config(DependencyChannel::LatestQualified);
    policy.signer_workflow =
        "github.com/Generous-Corp/pulp/.github/workflows/../release-cli.yml".to_owned();
    assert!(policy.validate().unwrap_err().contains("workflow path"));

    let mut policy = config(DependencyChannel::LatestQualified);
    policy.lock_file = PathBuf::from(".shipyard/dependencies/pulp?ref=attacker.json");
    assert!(policy.validate().unwrap_err().contains("components"));

    for control_path in [
        ".shipyard",
        ".shipyard/config.toml",
        ".shipyard/quarantine.toml",
        ".shipyard/dependencies",
    ] {
        let mut policy = config(DependencyChannel::LatestQualified);
        policy.lock_file = PathBuf::from(control_path);
        assert!(
            policy.validate().unwrap_err().contains("reserved"),
            "{control_path} must not be usable as a dependency lock"
        );
    }

    let mut policy = config(DependencyChannel::LatestQualified);
    policy.base_branch = "main&state=closed".to_owned();
    assert!(policy.validate().unwrap_err().contains("branch name"));

    for invalid_ref in ["release.lock/hotfix", "release/.hidden"] {
        let mut policy = config(DependencyChannel::LatestQualified);
        policy.base_branch = invalid_ref.to_owned();
        assert!(
            policy.validate().unwrap_err().contains("branch name"),
            "{invalid_ref} must follow Git ref component rules"
        );
    }
}

#[test]
fn release_asset_urls_are_bound_to_the_configured_repository_tag_and_name() {
    let config = config(DependencyChannel::LatestQualified);
    let mut candidate = release("v1.2.3");
    candidate.assets[0].download_url =
        "https://github.com/attacker/pulp/releases/download/v1.2.3/pulp-sdk-darwin-arm64.tar.gz"
            .to_owned();
    assert!(qualify(&config, &candidate).unwrap_err().contains("URL"));
}

#[test]
fn downgrade_requires_exact_fixed_override() {
    let latest = config(DependencyChannel::LatestQualified);
    let current = qualify(&latest, &release("v2.0.0")).expect("current lock");
    let candidate = qualify(&latest, &release("v1.9.0")).expect("candidate lock");
    assert!(
        validate_lock_transition(Some(&current), &candidate)
            .unwrap_err()
            .contains("downgrade")
    );

    let mut fixed = config(DependencyChannel::Fixed);
    fixed.fixed_tag = Some("v1.9.0".to_owned());
    fixed.fixed_commit = Some("c".repeat(40));
    let candidate = qualify(&fixed, &release("v1.9.0")).expect("fixed override");
    assert_eq!(
        validate_lock_transition(Some(&current), &candidate),
        Ok(LockTransition::Update)
    );
}

#[test]
fn stable_is_an_explicit_reviewed_promotion() {
    let mut stable = config(DependencyChannel::Stable);
    assert!(stable.validate().unwrap_err().contains("stable_tag"));
    stable.stable_tag = Some("v1.2.3".to_owned());
    stable.validate().expect("reviewed stable config");
    assert!(qualify(&stable, &release("v1.2.4")).is_err());
    assert!(qualify(&stable, &release("v1.2.3")).is_ok());
}

#[test]
fn deterministic_lock_is_idempotent() {
    let config = config(DependencyChannel::LatestQualified);
    let lock = qualify(&config, &release("v1.2.3")).expect("lock");
    assert_eq!(
        validate_lock_transition(Some(&lock), &lock),
        Ok(LockTransition::Unchanged)
    );
    assert_eq!(
        render_lock(&lock).expect("render"),
        render_lock(&lock).expect("render")
    );
}

#[cfg(unix)]
#[test]
fn tracked_config_and_lock_paths_reject_symlinks() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().join(".shipyard");
    std::fs::create_dir(&project_dir).expect("project directory");
    let external = temp.path().join("external-config.toml");
    std::fs::write(
        &external,
        r#"
[dependencies.pulp]
repository = "Generous-Corp/pulp"
channel = "latest-qualified"
required_assets = ["pulp-sdk-darwin-arm64.tar.gz"]
signer_workflow = "github.com/Generous-Corp/pulp/.github/workflows/release-cli.yml"
"#,
    )
    .expect("external config");
    symlink(&external, project_dir.join("config.toml")).expect("config symlink");
    assert!(
        PulpDependencyConfig::load_tracked(temp.path())
            .unwrap_err()
            .contains("must not be a symlink")
    );

    let policy = config(DependencyChannel::LatestQualified);
    symlink(
        temp.path().join("elsewhere"),
        project_dir.join("dependencies"),
    )
    .expect("lock directory symlink");
    assert!(
        policy
            .validate_lock_location(temp.path())
            .unwrap_err()
            .contains("must not be a symlink")
    );

    let dangling = temp.path().join("dangling-lock.json");
    symlink(temp.path().join("missing-lock.json"), &dangling).expect("dangling lock symlink");
    assert!(
        PulpDependencyLock::read_if_present(&dangling)
            .unwrap_err()
            .contains("must not be a symlink")
    );
}
