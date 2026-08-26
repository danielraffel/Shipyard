//! Authenticated transport for `shipyard changed-surface-plan`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use chrono::Utc;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::CliFailure;
use crate::changed_surface::{
    BuildType, ChangedSurfacePolicy, ExactHeadInput, ObservationStatus, PlannedSuite,
    ProtectedRefStatus, SecondaryProof, SelectionReceipt, plan_selection, policy_from_toml,
};
use crate::config::LoadedConfig;
use crate::evidence::EvidenceStore;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::output::write_json_envelope;

const FILES_PER_PAGE: usize = 100;
const MAX_FILE_PAGES: usize = 100;

pub(super) struct ChangedSurfacePlanArgs {
    pub(super) target: String,
    pub(super) pr: u64,
    pub(super) repo: Option<String>,
}

pub(super) struct ChangedSurfaceObservation {
    pub(super) receipt: SelectionReceipt,
    pub(super) input: ExactHeadInput,
    pub(super) policy: Result<ChangedSurfacePolicy, String>,
    pub(super) workflow_digest: String,
}

#[derive(Debug, Deserialize)]
struct PullRef {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
}

#[derive(Debug, Deserialize)]
struct PullMetadata {
    number: u64,
    base: PullRef,
    head: PullRef,
}

#[derive(Debug, Deserialize)]
struct BranchCommit {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct BranchMetadata {
    protected: bool,
    commit: BranchCommit,
}

#[derive(Debug, Deserialize)]
struct TreeIdentity {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct CommitIdentity {
    tree: TreeIdentity,
}

#[derive(Debug, Deserialize)]
struct MergeBaseIdentity {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct CompareMetadata {
    merge_base_commit: MergeBaseIdentity,
}

#[derive(Debug, Deserialize)]
struct PullFile {
    filename: String,
    #[serde(default)]
    previous_filename: Option<String>,
}

#[allow(clippy::too_many_lines)]
pub(super) fn changed_surface_plan_command<W: Write>(
    args: &ChangedSurfacePlanArgs,
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let observation = observe_changed_surface_plan(args, config, cwd, state_dir)?;
    let receipt_path = receipt_path(
        state_dir,
        &observation.receipt.repository,
        args.pr,
        &observation.receipt.head_sha,
        &args.target,
    );
    store_receipt(&receipt_path, &observation.receipt)?;

    emit_receipt(&observation.receipt, &receipt_path, json, stdout)?;
    if observation.receipt.planned_suite == PlannedSuite::Blocked {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn observe_changed_surface_plan(
    args: &ChangedSurfacePlanArgs,
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
) -> Result<ChangedSurfaceObservation, CliFailure> {
    if args.pr == 0 || args.target.trim().is_empty() {
        return Err(CliFailure::new(2, "--pr and --target must be nonempty"));
    }
    let started = Instant::now();
    let observed_at = Utc::now();
    let repo = super::runner_cmd::resolve_repo_slug(args.repo.clone(), cwd)?;
    let client = GhClient::from_loaded_config(config)
        .map_err(|error| CliFailure::new(1, format!("load GitHub auth: {error}")))?
        .with_repo_override(&repo)
        .map_err(|error| CliFailure::new(1, format!("resolve repository identity: {error}")))?;

    // PR head/base identity and the head tree are load-bearing. A failure here
    // emits no receipt because there is no exact head to bind evidence to.
    let pull: PullMetadata = gh_api_json(&client, cwd, &format!("repos/{repo}/pulls/{}", args.pr))?;
    if pull.number != args.pr {
        return Err(CliFailure::new(
            1,
            "GitHub returned a different pull-request identity",
        ));
    }
    let remote_commit: CommitIdentity = gh_api_json(
        &client,
        cwd,
        &format!("repos/{repo}/git/commits/{}", pull.head.sha),
    )?;
    let local_head = git_required(cwd, &["rev-parse", "HEAD"], "resolve local HEAD")?;
    let local_tree = git_required(cwd, &["rev-parse", "HEAD^{tree}"], "resolve local tree")?;
    let checkout_clean = git_required(
        cwd,
        &["status", "--porcelain", "--untracked-files=normal"],
        "inspect checkout state",
    )?
    .is_empty();

    // Protected-ref, merge-base, diff, and policy ambiguity are conservative
    // full-suite fallbacks after the exact head/tree boundary is established.
    let branch_endpoint = format!(
        "repos/{repo}/branches/{}",
        percent_encode_component(&pull.base.name)
    );
    let branch = gh_api_json::<BranchMetadata>(&client, cwd, &branch_endpoint).ok();
    let compare_endpoint = format!("repos/{repo}/compare/{}...{}", pull.base.sha, pull.head.sha);
    let compare = gh_api_json::<CompareMetadata>(&client, cwd, &compare_endpoint).ok();
    let local_merge_base = git_optional(cwd, &["merge-base", &pull.base.sha, &pull.head.sha]);
    let merge_base_is_ancestor = local_merge_base.as_deref().is_some_and(|merge_base| {
        git_status_success(
            cwd,
            &["merge-base", "--is-ancestor", merge_base, &pull.head.sha],
        )
    });
    let (remote_changed_paths, remote_changed_paths_complete) =
        fetch_changed_paths(&client, cwd, &repo, args.pr).unwrap_or_default();
    let (local_changed_paths, local_changed_paths_complete) =
        local_merge_base.as_deref().map_or_else(
            || (Vec::new(), false),
            |merge_base| {
                git_nul_paths(
                    cwd,
                    &[
                        "diff",
                        "--name-only",
                        "--no-renames",
                        "-z",
                        &format!("{merge_base}..{}", pull.head.sha),
                    ],
                )
                .map_or((Vec::new(), false), |paths| (paths, true))
            },
        );
    let protected_config = git_required(
        cwd,
        &["show", &format!("{}:.shipyard/config.toml", pull.base.sha)],
        "read selector policy from authenticated base",
    );
    let workflow_digest = protected_config.as_ref().map_or_else(
        |_| String::new(),
        |contents| format!("{:x}", Sha256::digest(contents.as_bytes())),
    );
    let policy = protected_config
        .map_err(|error| error.message)
        .and_then(|contents| policy_from_toml(&contents, &args.target));
    let (base_tracked_paths, base_tracked_paths_complete) =
        git_nul_paths(cwd, &["ls-tree", "-r", "--name-only", "-z", &pull.base.sha])
            .map_or((Vec::new(), false), |paths| (paths, true));
    let secondary_proofs = collect_secondary_proofs(
        policy.as_ref().ok(),
        state_dir,
        cwd,
        &repo,
        &pull.head.sha,
        &remote_commit.tree.sha,
    );

    let input = ExactHeadInput {
        repository: repo.clone(),
        pull_request: pull.number,
        target: args.target.clone(),
        observed_at,
        base_ref: pull.base.name,
        pr_base_sha: pull.base.sha,
        protected_ref_sha: branch
            .as_ref()
            .map_or_else(String::new, |branch| branch.commit.sha.clone()),
        protected_ref_status: branch
            .as_ref()
            .map_or(ProtectedRefStatus::Unresolved, |branch| {
                if branch.protected {
                    ProtectedRefStatus::Protected
                } else {
                    ProtectedRefStatus::Unprotected
                }
            }),
        pr_head_sha: pull.head.sha,
        remote_tree_sha: remote_commit.tree.sha,
        local_head_sha: local_head,
        local_tree_sha: local_tree,
        local_merge_base_sha: local_merge_base.unwrap_or_default(),
        remote_merge_base_sha: compare
            .map_or_else(String::new, |compare| compare.merge_base_commit.sha),
        merge_base_is_ancestor,
        checkout_clean,
        remote_changed_paths,
        remote_changed_paths_status: if remote_changed_paths_complete {
            ObservationStatus::Complete
        } else {
            ObservationStatus::Incomplete
        },
        local_changed_paths,
        local_changed_paths_status: if local_changed_paths_complete {
            ObservationStatus::Complete
        } else {
            ObservationStatus::Incomplete
        },
        base_tracked_paths,
        base_tracked_paths_status: if base_tracked_paths_complete {
            ObservationStatus::Complete
        } else {
            ObservationStatus::Incomplete
        },
        secondary_proofs,
    };
    let mut receipt = plan_selection(&input, policy.clone())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    receipt.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(ChangedSurfaceObservation {
        receipt,
        input,
        policy,
        workflow_digest,
    })
}

fn collect_secondary_proofs(
    policy: Option<&ChangedSurfacePolicy>,
    state_dir: &Path,
    cwd: &Path,
    repository: &str,
    head_sha: &str,
    tree_sha: &str,
) -> Vec<SecondaryProof> {
    let Some(policy) = policy else {
        return Vec::new();
    };
    let Ok(store) = EvidenceStore::open_existing(state_dir.join("evidence")) else {
        return Vec::new();
    };
    let repository_scope = crate::evidence::repository_evidence_scope(repository);
    let ship_scope_prefix = crate::evidence::repository_ship_evidence_scope_prefix(repository);
    let run_scope = crate::evidence::run_evidence_scope(cwd);
    policy
        .families
        .iter()
        .filter_map(|family| {
            Some((
                family.required_secondary_target.as_ref()?,
                family.required_secondary_build_type?,
            ))
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter_map(|(target, build_type)| {
            let expected_contract = policy.secondary_contract_digests.get(target)?;
            let mut candidates =
                store.passing_records_for_target_sha_scoped(&repository_scope, target, head_sha);
            candidates.extend(store.passing_records_for_target_sha_scoped_prefix(
                &ship_scope_prefix,
                target,
                head_sha,
            ));
            candidates
                .extend(store.passing_records_for_target_sha_scoped(&run_scope, target, head_sha));
            candidates.sort_by_key(|evidence| std::cmp::Reverse(evidence.completed_at));
            candidates.into_iter().find_map(|evidence| {
                let passed = evidence.passed();
                let reused = evidence.reused();
                let observed_build_type = evidence
                    .validation_build_type
                    .as_deref()
                    .and_then(parse_build_type);
                (observed_build_type == Some(build_type)
                    && evidence.contract_digest.as_ref() == Some(expected_contract)
                    && evidence.source_head_sha.as_deref() == Some(head_sha)
                    && evidence.source_tree_sha.as_deref() == Some(tree_sha)
                    && evidence.source_checkout_clean == Some(true)
                    && evidence.full_execution == Some(true))
                .then_some(SecondaryProof {
                    target: target.clone(),
                    build_type,
                    head_sha: evidence.sha,
                    tree_sha: evidence.source_tree_sha.expect("matched tree identity"),
                    full_execution: true,
                    passed,
                    reused,
                    completed_at: evidence.completed_at,
                    contract_digest: evidence.contract_digest,
                })
            })
        })
        .collect()
}

fn emit_receipt<W: Write>(
    receipt: &SelectionReceipt,
    receipt_path: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json {
        write_json_envelope(
            stdout,
            "changed-surface-plan",
            BTreeMap::from([
                (
                    "receipt".to_owned(),
                    serde_json::to_value(receipt)
                        .map_err(|error| CliFailure::new(1, error.to_string()))?,
                ),
                (
                    "receipt_path".to_owned(),
                    Value::String(receipt_path.display().to_string()),
                ),
            ]),
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        let planned = match receipt.planned_suite {
            PlannedSuite::Bounded => "bounded (shadow only)",
            PlannedSuite::Full => "full suite",
            PlannedSuite::Blocked => "blocked pending required secondary proof",
        };
        writeln!(
            stdout,
            "Exact head {} verified; planned {planned}; authoritative execution remains full suite.\nReceipt: {}",
            short_sha(&receipt.head_sha),
            receipt_path.display()
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn gh_api_json<T: DeserializeOwned>(
    client: &GhClient,
    cwd: &Path,
    endpoint: &str,
) -> Result<T, CliFailure> {
    let output = client
        .prepare_command(
            cwd,
            None,
            GhSupervision::Unsupervised,
            GhAuthPolicy::Default,
        )
        .map_err(|error| CliFailure::new(1, format!("prepare GitHub query: {error}")))?
        .args(["api", "--method", "GET", endpoint])
        .output()
        .map_err(|error| CliFailure::new(1, format!("start GitHub query: {error}")))?;
    if !output.status.success() {
        return Err(CliFailure::new(
            1,
            format!(
                "GitHub query {endpoint} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|error| {
        CliFailure::new(1, format!("parse GitHub response for {endpoint}: {error}"))
    })
}

fn fetch_changed_paths(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    pr: u64,
) -> Result<(Vec<String>, bool), CliFailure> {
    let mut paths = Vec::new();
    for page in 1..=MAX_FILE_PAGES {
        let endpoint =
            format!("repos/{repo}/pulls/{pr}/files?per_page={FILES_PER_PAGE}&page={page}");
        let files: Vec<PullFile> = gh_api_json(client, cwd, &endpoint)?;
        let count = files.len();
        paths.extend(pull_file_paths(files));
        if count < FILES_PER_PAGE {
            return Ok((paths, true));
        }
    }
    Ok((paths, false))
}

fn git_required(cwd: &Path, args: &[&str], context: &str) -> Result<String, CliFailure> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| CliFailure::new(1, format!("{context}: {error}")))?;
    if !output.status.success() {
        return Err(CliFailure::new(
            1,
            format!(
                "{context}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_optional(cwd: &Path, args: &[&str]) -> Option<String> {
    git_required(cwd, args, "git provenance query").ok()
}

fn git_status_success(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .is_ok_and(|status| status.success())
}

fn git_nul_paths(cwd: &Path, args: &[&str]) -> Result<Vec<String>, CliFailure> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| CliFailure::new(1, format!("compute local changed paths: {error}")))?;
    if !output.status.success() {
        return Err(CliFailure::new(1, "compute local changed paths failed"));
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            String::from_utf8(path.to_vec()).map_err(|_| {
                CliFailure::new(
                    1,
                    "local changed path is not valid UTF-8; selector diff is ambiguous",
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()
}

fn pull_file_paths(files: Vec<PullFile>) -> Vec<String> {
    files
        .into_iter()
        .flat_map(|file| file.previous_filename.into_iter().chain([file.filename]))
        .collect()
}

fn receipt_path(state_dir: &Path, repo: &str, pr: u64, head: &str, target: &str) -> PathBuf {
    state_dir
        .join("changed-surface")
        .join(percent_encode_component(repo))
        .join(pr.to_string())
        .join(head)
        .join(format!("{}.json", percent_encode_component(target)))
}

fn store_receipt(
    path: &Path,
    receipt: &crate::changed_surface::SelectionReceipt,
) -> Result<(), CliFailure> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let parent = path
        .parent()
        .ok_or_else(|| CliFailure::new(1, "receipt path has no parent"))?;
    fs::create_dir_all(parent)
        .map_err(|error| CliFailure::new(1, format!("create receipt directory: {error}")))?;
    let payload = serde_json::to_vec_pretty(receipt)
        .map_err(|error| CliFailure::new(1, format!("serialize receipt: {error}")))?;
    let temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| CliFailure::new(1, format!("create receipt temporary file: {error}")))?;
    fs::write(temporary.path(), [payload.as_slice(), b"\n"].concat())
        .map_err(|error| CliFailure::new(1, format!("write receipt: {error}")))?;
    temporary
        .persist(path)
        .map_err(|error| CliFailure::new(1, format!("persist receipt: {error}")))?;
    Ok(())
}

fn percent_encode_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn parse_build_type(value: &str) -> Option<BuildType> {
    match value {
        "debug" => Some(BuildType::Debug),
        "release" => Some(BuildType::Release),
        "rel_with_deb_info" => Some(BuildType::RelWithDebInfo),
        "min_size_rel" => Some(BuildType::MinSizeRel),
        _ => None,
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changed_surface::TestFamily;
    use crate::evidence::EvidenceRecord;

    #[test]
    fn branch_ref_is_encoded_as_one_api_path_component() {
        assert_eq!(
            percent_encode_component("release/1.0 candidate"),
            "release%2F1.0%20candidate"
        );
    }

    #[test]
    fn receipt_target_cannot_escape_state_directory() {
        let path = receipt_path(
            Path::new("/state"),
            "owner/repo",
            42,
            "abc",
            "../../mac target",
        );
        assert_eq!(
            path,
            Path::new("/state/changed-surface/owner%2Frepo/42/abc/..%2F..%2Fmac%20target.json")
        );
        assert_ne!(
            receipt_path(Path::new("/state"), "owner/repo", 42, "abc", "mac/release"),
            receipt_path(Path::new("/state"), "owner/repo", 42, "abc", "mac_release")
        );
    }

    #[test]
    fn renamed_pull_files_include_source_and_destination_paths() {
        let paths = pull_file_paths(vec![PullFile {
            filename: "docs/new.md".to_owned(),
            previous_filename: Some("schema/selector.json".to_owned()),
        }]);
        assert_eq!(paths, ["schema/selector.json", "docs/new.md"]);
    }

    #[test]
    fn secondary_collector_requires_clean_executed_head_and_tree() {
        let temp = tempfile::tempdir().expect("tempdir");
        let head = "a".repeat(40);
        let tree = "b".repeat(40);
        let mut policy = ChangedSurfacePolicy {
            schema_version: 1,
            full_test_count: 2,
            build_type: BuildType::Debug,
            build_flags: Vec::new(),
            baseline_tests: vec!["smoke".to_owned()],
            baseline_build_targets: Vec::new(),
            baseline_only_paths: Vec::new(),
            ios_compile_skip_safe_paths: Vec::new(),
            full_required_paths: Vec::new(),
            policy_paths: Vec::new(),
            test_topology_paths: vec!["tests/**".to_owned()],
            families: vec![TestFamily {
                name: "sdk".to_owned(),
                paths: vec!["sdk/**".to_owned()],
                tests: vec!["installed SDK".to_owned()],
                build_targets: Vec::new(),
                risk_class: crate::changed_surface::RiskClass::Low,
                extended_tests: Vec::new(),
                supported_build_types: vec![BuildType::Release],
                required_secondary_target: Some("release-sdk".to_owned()),
                required_secondary_build_type: Some(BuildType::Release),
            }],
            execution: None,
            secondary_contract_digests: BTreeMap::from([(
                "release-sdk".to_owned(),
                "contract".to_owned(),
            )]),
        };
        let store = EvidenceStore::new(temp.path().join("evidence")).expect("store");
        let mut evidence = EvidenceRecord {
            sha: head.clone(),
            branch: "feature".to_owned(),
            workload_scope: None,
            target_name: "release-sdk".to_owned(),
            validation_build_type: Some("release".to_owned()),
            platform: "macos-arm64".to_owned(),
            status: "pass".to_owned(),
            backend: "local".to_owned(),
            source_head_sha: Some(head.clone()),
            source_tree_sha: Some(tree.clone()),
            source_checkout_clean: Some(true),
            full_execution: Some(true),
            completed_at: Utc::now(),
            duration_secs: None,
            host: None,
            primary_backend: None,
            failover_reason: None,
            provider: None,
            runner_profile: None,
            failure_class: None,
            reused_from: None,
            contract_digest: Some("contract".to_owned()),
            stages_signature: None,
        };
        let repository = "owner/repo";
        let workload_scope = crate::evidence::run_evidence_scope(temp.path());
        store
            .record_scoped(&workload_scope, &evidence)
            .expect("record");
        assert_eq!(
            collect_secondary_proofs(
                Some(&policy),
                temp.path(),
                temp.path(),
                repository,
                &head,
                &tree,
            )
            .len(),
            1
        );

        evidence.source_checkout_clean = Some(false);
        store
            .record_scoped(&workload_scope, &evidence)
            .expect("replace record");
        assert!(
            collect_secondary_proofs(
                Some(&policy),
                temp.path(),
                temp.path(),
                repository,
                &head,
                &tree,
            )
            .is_empty()
        );
        evidence.source_checkout_clean = Some(true);
        evidence.full_execution = Some(false);
        store
            .record_scoped(&workload_scope, &evidence)
            .expect("replace record");
        assert!(
            collect_secondary_proofs(
                Some(&policy),
                temp.path(),
                temp.path(),
                repository,
                &head,
                &tree,
            )
            .is_empty()
        );
        evidence.full_execution = Some(true);
        store
            .record_scoped(
                &crate::evidence::repository_ship_evidence_scope(repository, 42),
                &evidence,
            )
            .expect("record ship proof");
        assert_eq!(
            collect_secondary_proofs(
                Some(&policy),
                temp.path(),
                temp.path(),
                repository,
                &head,
                &tree,
            )
            .len(),
            1
        );
        policy.secondary_contract_digests.clear();
        assert!(
            collect_secondary_proofs(
                Some(&policy),
                temp.path(),
                temp.path(),
                repository,
                &head,
                &tree,
            )
            .is_empty()
        );
    }
}
