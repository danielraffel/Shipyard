//! Authenticated transport for `shipyard changed-surface-plan`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::CliFailure;
use crate::changed_surface::{
    BuildType, ChangedSurfacePolicy, ExactHeadInput, MergeBasePlan, ObservationStatus,
    PlannedSuite, ProtectedRefStatus, SecondaryProof, SelectionReceipt, StaleBaseShadowInput,
    StaleBaseShadowReceipt, plan_selection, plan_stale_base_shadow, policy_digest,
    policy_from_base,
};
use crate::config::LoadedConfig;
use crate::evidence::EvidenceStore;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::output::write_json_envelope;

const FILES_PER_PAGE: usize = 100;
const MAX_FILE_PAGES: usize = 100;

pub(crate) struct ChangedSurfacePlanArgs {
    pub(crate) target: String,
    pub(crate) pr: u64,
    pub(crate) repo: Option<String>,
}

pub(crate) struct ChangedSurfaceObservation {
    pub(crate) receipt: SelectionReceipt,
    pub(crate) input: ExactHeadInput,
    pub(crate) policy: Result<ChangedSurfacePolicy, String>,
    pub(crate) workflow_digest: String,
}

pub(crate) struct StaleBaseShadowObservation {
    pub(crate) receipt: StaleBaseShadowReceipt,
    pub(crate) integration_input: Option<ExactHeadInput>,
    pub(crate) policy: Result<ChangedSurfacePolicy, String>,
    pub(crate) workflow_digest: String,
}

/// Recompute a strictly shadow-only stale-base plan from local, exact-object
/// observations. Missing objects, conflicts, and truncated path reads are
/// represented in the terminal assessment instead of guessed through.
pub(crate) fn observe_stale_base_shadow(
    observation: &ChangedSurfaceObservation,
    cwd: &Path,
    validation_contract_digest: &str,
) -> Result<StaleBaseShadowObservation, CliFailure> {
    let exact = &observation.input;
    let live_config = git_required(
        cwd,
        &[
            "show",
            &format!("{}:.shipyard/config.toml", exact.protected_ref_sha),
        ],
        "read selector policy from current protected base",
    );
    let live_workflow_digest = live_config.as_ref().map_or_else(
        |_| String::new(),
        |contents| format!("{:x}", Sha256::digest(contents.as_bytes())),
    );
    let live_policy = live_config
        .map_err(|error| error.message)
        .and_then(|contents| {
            policy_from_base(&contents, &exact.target, |path| {
                read_base_file(cwd, &exact.protected_ref_sha, path)
            })
        });
    let (protected_base_delta_paths, protected_base_delta_complete) = git_nul_paths(
        cwd,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            &format!("{}..{}", exact.pr_base_sha, exact.protected_ref_sha),
        ],
    )
    .map_or((Vec::new(), false), |paths| (paths, true));
    let (live_head_merge_base_sha, old_base_is_live_ancestor) = observe_stale_lineage(cwd, exact);
    let (integration_tree_sha, integration_commit_sha, integration_conflicted) =
        synthesize_integration_tree(cwd, &exact.protected_ref_sha, &exact.pr_head_sha);
    let (integration_changed_paths, integration_changed_paths_complete) = integration_tree_sha
        .as_deref()
        .filter(|_| !integration_conflicted)
        .map_or((Vec::new(), false), |tree| {
            git_nul_paths(
                cwd,
                &[
                    "diff",
                    "--name-only",
                    "--no-renames",
                    "-z",
                    &format!("{}..{tree}", exact.protected_ref_sha),
                ],
            )
            .map_or((Vec::new(), false), |paths| (paths, true))
        });
    let (live_base_tracked_paths, live_base_tracked_paths_complete) = git_nul_paths(
        cwd,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            "-z",
            &exact.protected_ref_sha,
        ],
    )
    .map_or((Vec::new(), false), |paths| (paths, true));
    let stale_input = StaleBaseShadowInput {
        old_policy: observation.policy.clone(),
        live_policy: live_policy.clone(),
        old_workflow_digest: observation.workflow_digest.clone(),
        live_workflow_digest,
        validation_contract_digest: validation_contract_digest.to_owned(),
        protected_base_delta_paths,
        protected_base_delta_status: observation_status(protected_base_delta_complete),
        live_head_merge_base_sha,
        old_base_is_live_ancestor,
        integration_changed_paths,
        integration_changed_paths_status: observation_status(integration_changed_paths_complete),
        integration_tree_sha: integration_tree_sha.unwrap_or_default(),
        integration_commit_sha: integration_commit_sha.unwrap_or_default(),
        integration_conflicted,
        live_base_tracked_paths,
        live_base_tracked_paths_status: observation_status(live_base_tracked_paths_complete),
        candidate: None,
    };
    let assessment = plan_stale_base_shadow(exact, &stale_input)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let integration_input = matches!(
        assessment.disposition,
        crate::changed_surface::StaleBaseShadowDisposition::Recomputed
            | crate::changed_surface::StaleBaseShadowDisposition::Reused
    )
    .then(|| crate::changed_surface::integration_exact_input(exact, &stale_input));
    Ok(StaleBaseShadowObservation {
        receipt: assessment,
        integration_input,
        policy: live_policy,
        workflow_digest: stale_input.live_workflow_digest,
    })
}

fn observe_stale_lineage(cwd: &Path, exact: &ExactHeadInput) -> (String, bool) {
    let live_head_merge_base_sha = git_optional(
        cwd,
        &["merge-base", &exact.protected_ref_sha, &exact.pr_head_sha],
    )
    .unwrap_or_default();
    let old_base_is_live_ancestor = git_status_success(
        cwd,
        &[
            "merge-base",
            "--is-ancestor",
            &exact.pr_base_sha,
            &exact.protected_ref_sha,
        ],
    );
    (live_head_merge_base_sha, old_base_is_live_ancestor)
}

fn observation_status(complete: bool) -> ObservationStatus {
    if complete {
        ObservationStatus::Complete
    } else {
        ObservationStatus::Incomplete
    }
}

fn synthesize_integration_tree(
    cwd: &Path,
    live_base: &str,
    head: &str,
) -> (Option<String>, Option<String>, bool) {
    let output = Command::new("git")
        .args(["merge-tree", "--write-tree", live_base, head])
        .current_dir(cwd)
        .output();
    let Ok(output) = output else {
        return (None, None, true);
    };
    let tree = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::trim)
        .filter(|value| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_owned);
    if !output.status.success() {
        return (tree, None, true);
    }
    let commit = tree
        .as_deref()
        .and_then(|tree| synthesize_integration_commit(cwd, tree, live_base, head));
    let unavailable = commit.is_none();
    (tree, commit, unavailable)
}

fn synthesize_integration_commit(
    cwd: &Path,
    tree: &str,
    live_base: &str,
    head: &str,
) -> Option<String> {
    let mut child = Command::new("git")
        .args(["commit-tree", tree, "-p", live_base, "-p", head])
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Shipyard integration")
        .env("GIT_AUTHOR_EMAIL", "shipyard@example.invalid")
        .env("GIT_AUTHOR_DATE", "@0 +0000")
        .env("GIT_COMMITTER_NAME", "Shipyard integration")
        .env("GIT_COMMITTER_EMAIL", "shipyard@example.invalid")
        .env("GIT_COMMITTER_DATE", "@0 +0000")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    child
        .stdin
        .take()?
        .write_all(b"Shipyard shadow integration\n")
        .ok()?;
    let output = child.wait_with_output().ok()?;
    let commit = String::from_utf8(output.stdout).ok()?;
    let commit = commit.trim();
    (output.status.success()
        && commit.len() == 40
        && commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
    .then(|| commit.to_owned())
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
pub(crate) fn observe_changed_surface_plan(
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
    // A head that sits on an older commit of the protected branch than the
    // PR's recorded base is planned against that merge base: the lane tests
    // the head tree, so its own base's policy, tree and inventory are the only
    // consistent ones. The recorded base's policy digest is kept so promotion
    // can require the two to agree.
    let planning_base =
        merge_base_behind_recorded(cwd, local_merge_base.as_deref(), &pull.base.sha);
    let policy_base = planning_base
        .clone()
        .unwrap_or_else(|| pull.base.sha.clone());
    let read_policy = |base: &str| {
        let config = git_required(
            cwd,
            &["show", &format!("{base}:.shipyard/config.toml")],
            "read selector policy from authenticated base",
        );
        let digest = config.as_ref().map_or_else(
            |_| String::new(),
            |contents| format!("{:x}", Sha256::digest(contents.as_bytes())),
        );
        let policy = config.map_err(|error| error.message).and_then(|contents| {
            policy_from_base(&contents, &args.target, |path| {
                read_base_file(cwd, base, path)
            })
        });
        (policy, digest)
    };
    let (policy, workflow_digest) = read_policy(&policy_base);
    let merge_base_plan = planning_base.as_ref().map(|_| MergeBasePlan {
        recorded_base_policy_digest: read_policy(&pull.base.sha)
            .0
            .ok()
            .map(|recorded| policy_digest(&recorded)),
    });
    let (base_tracked_paths, base_tracked_paths_complete) =
        git_nul_paths(cwd, &["ls-tree", "-r", "--name-only", "-z", &policy_base])
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
        merge_base_plan,
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

/// Schema of a shadow-plan record written by `changed-surface-plan --record`.
pub(crate) const SHADOW_PLAN_RECORD_SCHEMA_VERSION: u32 = 1;
/// Origin stamped on every shadow-plan record. Proxies separate these plans,
/// which can never execute, from lane plans that could activate.
pub(crate) const SHADOW_PLAN_RECORD_ORIGIN: &str = "shadow_plan_step";

/// One shadow-plan observation, successful or not. A failed observation is
/// itself a labelled outcome (`planner_error`), never a missing record.
#[derive(Debug, Serialize)]
pub(crate) struct ShadowPlanRecord {
    pub(crate) schema_version: u32,
    pub(crate) origin: &'static str,
    pub(crate) shadow_only: bool,
    pub(crate) recorded_at: DateTime<Utc>,
    pub(crate) repository: String,
    pub(crate) pull_request: u64,
    pub(crate) target: String,
    pub(crate) head_sha: Option<String>,
    pub(crate) outcome: &'static str,
    pub(crate) planned_suite: Option<PlannedSuite>,
    pub(crate) planner_reason: Option<String>,
    pub(crate) elapsed_ms: u64,
    pub(crate) receipt: Option<SelectionReceipt>,
    pub(crate) error: Option<String>,
}

impl ShadowPlanRecord {
    fn new(repository: String, pull_request: u64, target: String) -> Self {
        Self {
            schema_version: SHADOW_PLAN_RECORD_SCHEMA_VERSION,
            origin: SHADOW_PLAN_RECORD_ORIGIN,
            shadow_only: true,
            recorded_at: Utc::now(),
            repository,
            pull_request,
            target,
            head_sha: None,
            outcome: "planner_error",
            planned_suite: None,
            planner_reason: Some("planner_error".to_owned()),
            elapsed_ms: 0,
            receipt: None,
            error: None,
        }
    }

    pub(crate) fn planned(receipt: SelectionReceipt) -> Self {
        let mut record = Self::new(
            receipt.repository.clone(),
            receipt.pull_request,
            receipt.target.clone(),
        );
        record.head_sha = Some(receipt.head_sha.clone());
        record.outcome = "planned";
        record.planned_suite = Some(receipt.planned_suite);
        record.planner_reason = receipt.fallback_reason.as_ref().map(|reason| {
            serde_json::to_value(reason)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| format!("{reason:?}"))
        });
        record.elapsed_ms = receipt.elapsed_ms;
        record.receipt = Some(receipt);
        record
    }

    pub(crate) fn planner_error(
        repository: Option<&str>,
        pull_request: u64,
        target: &str,
        error: &str,
    ) -> Self {
        let mut record = Self::new(
            repository.unwrap_or("unknown").to_owned(),
            pull_request,
            target.to_owned(),
        );
        record.error = Some(error.to_owned());
        record
    }

    /// `<dir>/<repo>/<pr>/<head>/<target>.json`, or
    /// `<dir>/<repo>/<pr>/planner-error-<target>.json` when no head was bound.
    pub(crate) fn path(&self, record_dir: &Path) -> PathBuf {
        let pr_dir = record_dir
            .join(percent_encode_component(&self.repository))
            .join(self.pull_request.to_string());
        let target = percent_encode_component(&self.target);
        match &self.head_sha {
            Some(head) => pr_dir.join(head).join(format!("{target}.json")),
            None => pr_dir.join(format!("planner-error-{target}.json")),
        }
    }

    pub(crate) fn write(&self, record_dir: &Path) -> Result<PathBuf, CliFailure> {
        let path = self.path(record_dir);
        let parent = path
            .parent()
            .ok_or_else(|| CliFailure::new(1, "shadow-plan record path has no parent"))?;
        fs::create_dir_all(parent).map_err(|error| {
            CliFailure::new(1, format!("create shadow-plan record directory: {error}"))
        })?;
        let payload = serde_json::to_vec_pretty(self).map_err(|error| {
            CliFailure::new(1, format!("serialize shadow-plan record: {error}"))
        })?;
        fs::write(&path, [payload.as_slice(), b"\n"].concat())
            .map_err(|error| CliFailure::new(1, format!("write shadow-plan record: {error}")))?;
        Ok(path)
    }
}

/// `changed-surface-plan --record <dir>`: plan in shadow and write a record,
/// leaving the host's ship state untouched. Every planner outcome, including
/// a blocked one, succeeds; a planner failure writes a `planner_error` record
/// and still fails, so a broken instrument stays visible to the caller.
pub(super) fn changed_surface_plan_record_command<W: Write>(
    args: &ChangedSurfacePlanArgs,
    config: Result<LoadedConfig, CliFailure>,
    cwd: &Path,
    state_dir: &Path,
    record_dir: &Path,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let observed =
        config.and_then(|config| observe_changed_surface_plan(args, &config, cwd, state_dir));
    let (record, failure) = match observed {
        Ok(observation) => (ShadowPlanRecord::planned(observation.receipt), None),
        Err(failure) => (
            ShadowPlanRecord::planner_error(
                args.repo.as_deref(),
                args.pr,
                &args.target,
                &failure.message,
            ),
            Some(failure),
        ),
    };
    let path = record.write(record_dir)?;
    let label = record
        .planner_reason
        .as_deref()
        .unwrap_or(match record.planned_suite {
            Some(PlannedSuite::Bounded) => "bounded",
            _ => "unlabelled",
        });
    writeln!(stdout, "Shadow plan recorded ({label}): {}", path.display())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    failure.map_or(Ok(ExitCode::SUCCESS), Err)
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

/// The head's merge base when it is a strict ancestor of the PR's recorded
/// base: the head sits on an older commit of the protected branch.
pub(crate) fn merge_base_behind_recorded(
    cwd: &Path,
    merge_base: Option<&str>,
    recorded_base: &str,
) -> Option<String> {
    merge_base
        .filter(|merge_base| {
            *merge_base != recorded_base
                && git_status_success(
                    cwd,
                    &["merge-base", "--is-ancestor", merge_base, recorded_base],
                )
        })
        .map(ToOwned::to_owned)
}

/// A tracked file's bytes at an authenticated commit, for a selector
/// declaration's `families_file`.
pub(crate) fn read_base_file(cwd: &Path, sha: &str, path: &str) -> Result<String, String> {
    git_required(
        cwd,
        &["show", &format!("{sha}:{path}")],
        "read selector families_file from authenticated base",
    )
    .map_err(|error| error.message)
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

    fn recorded_receipt(fallback_reason: Option<&str>, planned_suite: &str) -> SelectionReceipt {
        serde_json::from_value(serde_json::json!({
            "schema_version": 3, "exact_head_verified": true, "shadow_only": true,
            "repository": "Generous-Corp/pulp", "pull_request": 9262, "target": "mac",
            "protected_ref": "main", "pr_base_sha": "b".repeat(40),
            "protected_ref_sha": "b".repeat(40), "merge_base_sha": "b".repeat(40),
            "head_sha": "a".repeat(40), "tree_sha": "c".repeat(40),
            "changed_paths_digest": "d".repeat(64), "build_flags": [], "changed_paths": [],
            "selected_families": [], "selected_tests": [], "selected_build_targets": [],
            "baseline_tests": [], "family_coverage": {}, "secondary_proofs": [],
            "planned_suite": planned_suite, "selection_tier": "full",
            "authoritative_suite": "full",
            "outcomes": {"planner": "planned",
                         "authoritative_execution": "not_observed_by_shadow_planner"},
            "selected_count": null, "full_count": null,
            "fallback_reason": fallback_reason, "elapsed_ms": 412,
        }))
        .expect("receipt fixture")
    }

    #[test]
    fn a_shadow_plan_record_carries_its_origin_and_the_planner_reason() {
        let record =
            ShadowPlanRecord::planned(recorded_receipt(Some("base_policy_mismatch"), "full"));
        let value = serde_json::to_value(&record).expect("record json");
        assert_eq!(value["origin"], "shadow_plan_step");
        assert_eq!(value["shadow_only"], true);
        assert_eq!(value["outcome"], "planned");
        assert_eq!(value["planner_reason"], "base_policy_mismatch");
        assert_eq!(value["planned_suite"], "full");
        assert_eq!(value["elapsed_ms"], 412);
        assert_eq!(value["receipt"]["head_sha"], "a".repeat(40));
        assert_eq!(
            record.path(Path::new("/records")),
            Path::new("/records/Generous-Corp%2Fpulp/9262")
                .join("a".repeat(40))
                .join("mac.json")
        );

        let bounded = ShadowPlanRecord::planned(recorded_receipt(None, "bounded"));
        assert_eq!(bounded.planner_reason, None);
        assert_eq!(bounded.planned_suite, Some(PlannedSuite::Bounded));
    }

    #[test]
    fn a_planner_failure_is_recorded_as_planner_error_and_still_fails() {
        let records = tempfile::tempdir().expect("records");
        let state = tempfile::tempdir().expect("state");
        let mut stdout = Vec::new();
        let failure = changed_surface_plan_record_command(
            &ChangedSurfacePlanArgs {
                target: "mac".to_owned(),
                pr: 9262,
                repo: Some("Generous-Corp/pulp".to_owned()),
            },
            Err(CliFailure::new(1, "config: no .shipyard/config.toml")),
            records.path(),
            state.path(),
            records.path(),
            &mut stdout,
        )
        .expect_err("a planner failure stays visible");
        assert_eq!(failure.message, "config: no .shipyard/config.toml");
        let path = records
            .path()
            .join("Generous-Corp%2Fpulp/9262/planner-error-mac.json");
        let value: Value =
            serde_json::from_slice(&fs::read(&path).expect("planner_error record")).expect("json");
        assert_eq!(value["outcome"], "planner_error");
        assert_eq!(value["planner_reason"], "planner_error");
        assert_eq!(value["shadow_only"], true);
        assert_eq!(value["error"], "config: no .shipyard/config.toml");
        // Recording never writes the host's ship state.
        assert_eq!(fs::read_dir(state.path()).expect("state").count(), 0);
        assert!(String::from_utf8_lossy(&stdout).contains("planner_error"));
    }

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
    fn only_a_merge_base_strictly_behind_the_recorded_base_is_planned_against() {
        let temp = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(temp.path())
                .output()
                .expect("git");
            assert!(output.status.success(), "git {args:?}");
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Shipyard Test"]);
        git(&["config", "user.email", "shipyard@example.invalid"]);
        git(&["commit", "-q", "--allow-empty", "-m", "old base"]);
        let old_base = git(&["rev-parse", "HEAD"]);
        git(&["commit", "-q", "--allow-empty", "-m", "recorded base"]);
        let recorded = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "-q", "-b", "side", &old_base]);
        git(&["commit", "-q", "--allow-empty", "-m", "unrelated"]);
        let side = git(&["rev-parse", "HEAD"]);
        let cwd = temp.path();
        assert_eq!(
            super::merge_base_behind_recorded(cwd, Some(&old_base), &recorded),
            Some(old_base.clone())
        );
        assert_eq!(
            super::merge_base_behind_recorded(cwd, Some(&recorded), &recorded),
            None
        );
        assert_eq!(
            super::merge_base_behind_recorded(cwd, Some(&side), &recorded),
            None
        );
        assert_eq!(
            super::merge_base_behind_recorded(cwd, None, &recorded),
            None
        );
    }

    #[test]
    fn families_file_is_read_from_the_authenticated_commit_not_the_checkout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(temp.path())
                .output()
                .expect("git");
            assert!(output.status.success(), "git {args:?}");
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Shipyard Test"]);
        git(&["config", "user.email", "shipyard@example.invalid"]);
        fs::create_dir_all(temp.path().join(".shipyard")).expect("dir");
        let path = temp.path().join(".shipyard/families.toml");
        fs::write(&path, "base\n").expect("base");
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        let base = git(&["rev-parse", "HEAD"]);
        fs::write(&path, "working tree\n").expect("edit");
        assert_eq!(
            super::read_base_file(temp.path(), &base, ".shipyard/families.toml").expect("read"),
            "base"
        );
        assert!(super::read_base_file(temp.path(), &base, ".shipyard/absent.toml").is_err());
    }

    #[test]
    fn synthesized_integration_tree_reports_clean_and_conflicting_merges() {
        let temp = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(temp.path())
                .output()
                .expect("git");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Shipyard Test"]);
        git(&["config", "user.email", "shipyard@example.invalid"]);
        fs::write(temp.path().join("base.txt"), "base\n").expect("base");
        fs::write(temp.path().join("shared.txt"), "base\n").expect("shared");
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        let base = git(&["rev-parse", "HEAD"]);

        git(&["checkout", "-qb", "head"]);
        fs::write(temp.path().join("head.txt"), "head\n").expect("head");
        git(&["add", "."]);
        git(&["commit", "-qm", "head"]);
        let head = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "-q", "--detach", &base]);
        fs::write(temp.path().join("live.txt"), "live\n").expect("live");
        git(&["add", "."]);
        git(&["commit", "-qm", "live"]);
        let live = git(&["rev-parse", "HEAD"]);

        let (tree, commit, conflicted) = synthesize_integration_tree(temp.path(), &live, &head);
        assert!(!conflicted);
        let tree = tree.expect("tree");
        let commit = commit.expect("commit");
        let (_, repeated_commit, repeated_conflicted) =
            synthesize_integration_tree(temp.path(), &live, &head);
        assert!(!repeated_conflicted);
        assert_eq!(repeated_commit.as_deref(), Some(commit.as_str()));
        assert_eq!(git(&["rev-parse", &format!("{commit}^{{tree}}")]), tree);
        assert_eq!(git(&["rev-parse", &format!("{commit}^1")]), live);
        assert_eq!(git(&["rev-parse", &format!("{commit}^2")]), head);

        git(&["checkout", "-q", "--detach", &base]);
        fs::write(temp.path().join("shared.txt"), "head side\n").expect("head conflict");
        git(&["add", "."]);
        git(&["commit", "-qm", "head conflict"]);
        let conflict_head = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "-q", "--detach", &base]);
        fs::write(temp.path().join("shared.txt"), "live side\n").expect("live conflict");
        git(&["add", "."]);
        git(&["commit", "-qm", "live conflict"]);
        let conflict_live = git(&["rev-parse", "HEAD"]);
        let (_, _, conflicted) =
            synthesize_integration_tree(temp.path(), &conflict_live, &conflict_head);
        assert!(conflicted);
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
            executable_reuse: None,
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
