use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::CliFailure;
use super::super::changed_surface_cmd::{
    ChangedSurfacePlanArgs, observe_changed_surface_plan, observe_stale_base_shadow,
};
use crate::changed_surface::trial::{TrialIdentity, result_directory};
use crate::changed_surface::{
    ExecutionCommandTransport, ExecutionDisposition, FallbackReason, StaleBaseShadowReceipt,
    plan_authoritative_execution,
};
use crate::config::LoadedConfig;
use crate::evidence::canonical_repository;
use crate::executor::dispatch::{ResolvedBackend, ResolvedTarget, ResolvedValidation};
use crate::queue_request::validation_contract_digest;

const MODE_KEY: &str = "changed_surface_execution.mode";
const ACCEPTED_POLICY_DIGEST_KEY: &str = "changed_surface_execution.accepted_shadow_policy_digest";
const ACCEPTED_POLICY_DIGESTS_KEY: &str =
    "changed_surface_execution.accepted_shadow_policy_digests";
const MAX_DIAGNOSTIC_CHARS: usize = 512;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MachineMode {
    #[default]
    Off,
    ShadowCompare,
    Authoritative,
}

impl MachineMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::ShadowCompare => "shadow_compare",
            Self::Authoritative => "authoritative",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MachinePolicy {
    mode: MachineMode,
    legacy_accepted_shadow_policy_digest: Option<String>,
    accepted_shadow_policy_digests: BTreeMap<(String, String), String>,
}

impl MachinePolicy {
    fn from_global(config: &LoadedConfig) -> Result<Self, CliFailure> {
        let trusted = LoadedConfig::load_machine_global_from_dir(config.global_dir.clone())
            .map_err(|error| {
                CliFailure::new(1, format!("load trusted selector policy: {error}"))
            })?;
        let mode = match trusted.get_str(MODE_KEY) {
            None | Some("off") => MachineMode::Off,
            Some("shadow_compare") => MachineMode::ShadowCompare,
            Some("authoritative") => MachineMode::Authoritative,
            Some(value) => Err(CliFailure::new(
                2,
                format!("invalid trusted {MODE_KEY} value '{value}'"),
            ))?,
        };
        let legacy_accepted_shadow_policy_digest = match trusted.get(ACCEPTED_POLICY_DIGEST_KEY) {
            None => None,
            Some(value) => Some(value.as_str().ok_or_else(|| {
                CliFailure::new(
                    2,
                    format!("invalid trusted {ACCEPTED_POLICY_DIGEST_KEY}: expected a string"),
                )
            })?),
        }
        .map(ToOwned::to_owned);
        if legacy_accepted_shadow_policy_digest
            .as_deref()
            .is_some_and(|digest| !valid_policy_digest(digest))
        {
            return Err(CliFailure::new(
                2,
                format!("invalid trusted {ACCEPTED_POLICY_DIGEST_KEY}"),
            ));
        }
        let scoped_policy_configured = trusted.get(ACCEPTED_POLICY_DIGESTS_KEY).is_some();
        let accepted_shadow_policy_digests = parse_scoped_policy_digests(&trusted)?;
        if legacy_accepted_shadow_policy_digest.is_some() && scoped_policy_configured {
            return Err(CliFailure::new(
                2,
                format!(
                    "ambiguous trusted changed-surface policy: configure either legacy {ACCEPTED_POLICY_DIGEST_KEY} or scoped {ACCEPTED_POLICY_DIGESTS_KEY}, not both"
                ),
            ));
        }
        Ok(Self {
            mode,
            legacy_accepted_shadow_policy_digest,
            accepted_shadow_policy_digests,
        })
    }

    fn permits_authoritative(&self, repository: &str, target: &str, policy_digest: &str) -> bool {
        self.mode != MachineMode::Authoritative
            || if self.accepted_shadow_policy_digests.is_empty() {
                self.legacy_accepted_shadow_policy_digest.as_deref() == Some(policy_digest)
            } else {
                self.accepted_shadow_policy_digests
                    .get(&(canonical_repository(repository), target.to_owned()))
                    .is_some_and(|accepted| accepted == policy_digest)
            }
    }
}

fn parse_scoped_policy_digests(
    trusted: &LoadedConfig,
) -> Result<BTreeMap<(String, String), String>, CliFailure> {
    let Some(value) = trusted.get(ACCEPTED_POLICY_DIGESTS_KEY) else {
        return Ok(BTreeMap::new());
    };
    let repositories = value.as_table().ok_or_else(|| {
        CliFailure::new(
            2,
            format!("invalid trusted {ACCEPTED_POLICY_DIGESTS_KEY}: expected a table"),
        )
    })?;
    if repositories.is_empty() {
        return Err(CliFailure::new(
            2,
            format!("invalid trusted {ACCEPTED_POLICY_DIGESTS_KEY}: table is empty"),
        ));
    }
    let mut accepted = BTreeMap::new();
    let mut canonical_repositories = BTreeSet::new();
    for (repository, targets) in repositories {
        if !valid_repository_slug(repository) {
            return Err(CliFailure::new(
                2,
                format!("invalid trusted {ACCEPTED_POLICY_DIGESTS_KEY} repository '{repository}'"),
            ));
        }
        let canonical = canonical_repository(repository);
        if !canonical_repositories.insert(canonical.clone()) {
            return Err(CliFailure::new(
                2,
                format!(
                    "ambiguous trusted {ACCEPTED_POLICY_DIGESTS_KEY}: repository '{repository}' duplicates canonical repository '{canonical}'"
                ),
            ));
        }
        let targets = targets.as_table().ok_or_else(|| {
            CliFailure::new(
                2,
                format!(
                    "invalid trusted {ACCEPTED_POLICY_DIGESTS_KEY}.{repository}: expected a target table"
                ),
            )
        })?;
        if targets.is_empty() {
            return Err(CliFailure::new(
                2,
                format!(
                    "invalid trusted {ACCEPTED_POLICY_DIGESTS_KEY}.{repository}: target table is empty"
                ),
            ));
        }
        for (target, digest) in targets {
            if target.is_empty() || target.trim() != target {
                return Err(CliFailure::new(
                    2,
                    format!(
                        "invalid trusted {ACCEPTED_POLICY_DIGESTS_KEY}.{repository} target '{target}'"
                    ),
                ));
            }
            let digest = digest.as_str().ok_or_else(|| {
                CliFailure::new(
                    2,
                    format!(
                        "invalid trusted {ACCEPTED_POLICY_DIGESTS_KEY}.{repository}.{target}: expected a string"
                    ),
                )
            })?;
            if !valid_policy_digest(digest) {
                return Err(CliFailure::new(
                    2,
                    format!("invalid trusted {ACCEPTED_POLICY_DIGESTS_KEY}.{repository}.{target}"),
                ));
            }
            accepted.insert((canonical.clone(), target.clone()), digest.to_owned());
        }
    }
    Ok(accepted)
}

fn valid_repository_slug(repository: &str) -> bool {
    let mut parts = repository.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !owner.is_empty()
        && !name.is_empty()
        && owner.chars().chain(name.chars()).all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn valid_policy_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Serialize)]
struct ActivationReceipt<'a> {
    schema_version: u32,
    machine_mode: MachineMode,
    plan: &'a crate::changed_surface::AuthoritativeExecutionPlan,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_build_command_sha256: Option<String>,
    original_test_command_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    substituted_build_command_sha256: Option<String>,
    substituted_test_command_sha256: String,
}

#[derive(Debug, Serialize)]
struct StaleActivationReceipt<'a> {
    schema_version: u32,
    machine_mode: MachineMode,
    merge_authority: crate::changed_surface::MergeAuthority,
    stale_context_digest: String,
    stale_receipt_sha256: String,
    plan: &'a crate::changed_surface::AuthoritativeExecutionPlan,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_build_command_sha256: Option<String>,
    original_test_command_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    substituted_build_command_sha256: Option<String>,
    substituted_test_command_sha256: String,
}

#[derive(Debug, Serialize)]
struct FallbackDiagnostic<'a> {
    schema_version: u32,
    repository: &'a str,
    pull_request: u64,
    target: &'a str,
    machine_mode: MachineMode,
    category: &'a str,
    diagnostic: String,
}

// Keep the fail-open-to-full branches adjacent to their exact diagnostics and
// mutation point; splitting them risks one error path accidentally substituting
// a command or losing its durable reason.
#[allow(clippy::too_many_lines)]
pub(super) fn apply_changed_surface_execution(
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    repo: &str,
    pr: Option<u64>,
    resume_from: Option<&str>,
    targets: &mut [ResolvedTarget],
) -> Result<(), CliFailure> {
    let machine = MachinePolicy::from_global(config)?;
    if machine.mode == MachineMode::Off || !cfg!(unix) {
        return Ok(());
    }
    let Some(pr) = pr.filter(|number| *number != 0) else {
        return Ok(());
    };

    if resume_from == Some("test") {
        // This pass is deliberately read-only. A later schema-v3 target must
        // refuse the whole invocation before an earlier schema-v2 target can
        // persist activation evidence or mutate its stages. Merged config is
        // only a negative prefilter; the protected-base policy and exact-head
        // receipt remain the authority.
        for target in targets.iter() {
            if !target_declares_changed_surface_selection(config, &target.name) {
                continue;
            }
            let ResolvedValidation::Local(validation) = &target.validation else {
                continue;
            };
            if validation.command.is_some() || !validation.stages.contains_key("test") {
                continue;
            }
            let Some(contract_digest) = validation_contract_digest(target) else {
                continue;
            };
            let Ok(observation) = observe_changed_surface_plan(
                &ChangedSurfacePlanArgs {
                    target: target.name.clone(),
                    pr,
                    repo: (!repo.is_empty()).then(|| repo.to_owned()),
                },
                config,
                cwd,
                state_dir,
            ) else {
                continue;
            };
            let Ok(policy) = observation.policy.as_ref() else {
                continue;
            };
            let Ok(ExecutionDisposition::Bounded(plan)) = plan_authoritative_execution(
                &observation.receipt,
                &observation.input,
                policy,
                true,
                ExecutionCommandTransport::PosixShell,
                &contract_digest,
                &observation.workflow_digest,
            ) else {
                continue;
            };
            let would_activate =
                machine.permits_authoritative(repo, &target.name, &plan.policy_digest)
                    && (plan.stage != "build_and_test" || validation.stages.contains_key("build"));
            if let Some(reason) =
                selected_resume_block_reason(&plan.stage, resume_from, would_activate)
            {
                return Err(CliFailure::new(2, reason));
            }
        }
        // Schema v2 safely resumes the original test stage. Do not observe a
        // second time or activate a newly changed plan after this preflight.
        return Ok(());
    }

    for target in targets {
        // This merged-layer check is only a negative performance prefilter.
        // Authorization is always reparsed from the authenticated base below.
        if !target_declares_changed_surface_selection(config, &target.name) {
            continue;
        }
        let ResolvedValidation::Local(validation) = &target.validation else {
            continue;
        };
        if validation.command.is_some() || !validation.stages.contains_key("test") {
            continue;
        }
        let Some(contract_digest) = validation_contract_digest(target) else {
            continue;
        };
        let observation = match observe_changed_surface_plan(
            &ChangedSurfacePlanArgs {
                target: target.name.clone(),
                pr,
                repo: (!repo.is_empty()).then(|| repo.to_owned()),
            },
            config,
            cwd,
            state_dir,
        ) {
            Ok(observation) => observation,
            Err(error) => {
                persist_fallback_diagnostic(
                    &result_dir(state_dir, repo, pr, "unresolved", &target.name),
                    &FallbackDiagnostic {
                        schema_version: 1,
                        repository: repo,
                        pull_request: pr,
                        target: &target.name,
                        machine_mode: machine.mode,
                        category: "observation_error",
                        diagnostic: bounded_diagnostic(&error.message),
                    },
                )?;
                continue;
            }
        };
        let Ok(policy) = observation.policy.as_ref() else {
            persist_fallback_diagnostic(
                &result_dir(
                    state_dir,
                    repo,
                    pr,
                    &observation.receipt.head_sha,
                    &target.name,
                ),
                &FallbackDiagnostic {
                    schema_version: 1,
                    repository: repo,
                    pull_request: pr,
                    target: &target.name,
                    machine_mode: machine.mode,
                    category: "protected_policy_error",
                    diagnostic: bounded_diagnostic(
                        observation
                            .policy
                            .as_ref()
                            .expect_err("checked policy error"),
                    ),
                },
            )?;
            continue;
        };
        let disposition = match plan_authoritative_execution(
            &observation.receipt,
            &observation.input,
            policy,
            true,
            ExecutionCommandTransport::PosixShell,
            &contract_digest,
            &observation.workflow_digest,
        ) {
            Ok(disposition) => disposition,
            Err(error) => {
                persist_fallback_diagnostic(
                    &result_dir(
                        state_dir,
                        repo,
                        pr,
                        &observation.receipt.head_sha,
                        &target.name,
                    ),
                    &FallbackDiagnostic {
                        schema_version: 1,
                        repository: repo,
                        pull_request: pr,
                        target: &target.name,
                        machine_mode: machine.mode,
                        category: "promotion_error",
                        diagnostic: bounded_diagnostic(&error.to_string()),
                    },
                )?;
                continue;
            }
        };
        let mut stale_execution = None;
        if machine.mode == MachineMode::ShadowCompare
            && observation.receipt.fallback_reason == Some(FallbackReason::StaleBase)
            && matches!(disposition, ExecutionDisposition::Full { .. })
        {
            let assessment = observe_stale_base_shadow(&observation, cwd, &contract_digest)?;
            let evidence_dir = result_dir(
                state_dir,
                repo,
                pr,
                &observation.receipt.head_sha,
                &target.name,
            );
            // Persist the shadow-only authority fence before any isolated
            // execution. If planning, persistence, or materialization cannot
            // prove the exact integration identity, ordinary full validation
            // remains untouched below.
            let shadow_receipt_persisted =
                persist_stale_base_shadow(&evidence_dir, &assessment.receipt).is_ok();
            if shadow_receipt_persisted
                && let (Some(integration_input), Ok(policy), Some(selection)) = (
                    assessment.integration_input.as_ref(),
                    assessment.policy.as_ref(),
                    assessment.receipt.shadow_selection.as_deref(),
                )
                && let Ok(ExecutionDisposition::Bounded(plan)) = plan_authoritative_execution(
                    &{
                        let mut execution_selection = selection.clone();
                        // The outer stale receipt, not the repository adapter,
                        // owns this cross-receipt linkage. Re-derivation sees
                        // the original planner shape and the activation below
                        // separately binds its digest to the outer context.
                        execution_selection.shadow_context_digest = None;
                        execution_selection
                    },
                    integration_input,
                    policy,
                    true,
                    ExecutionCommandTransport::PosixShell,
                    &contract_digest,
                    &assessment.workflow_digest,
                )
            {
                match crate::changed_surface::integration_checkout::materialize(
                    cwd,
                    &evidence_dir.join("integration-checkouts"),
                    &assessment.receipt,
                ) {
                    Ok(checkout) => stale_execution = Some((plan, assessment.receipt, checkout)),
                    Err(error) => {
                        let _ = persist_fallback_diagnostic(
                            &evidence_dir,
                            &FallbackDiagnostic {
                                schema_version: 1,
                                repository: repo,
                                pull_request: pr,
                                target: &target.name,
                                machine_mode: machine.mode,
                                category: "stale_integration_materialization",
                                diagnostic: bounded_diagnostic(&error),
                            },
                        );
                    }
                }
            }
        }
        let (plan, stale_receipt, stale_checkout) =
            if let Some((plan, receipt, checkout)) = stale_execution {
                (plan, Some(receipt), Some(checkout))
            } else {
                let plan = match disposition {
                    ExecutionDisposition::Bounded(plan) => plan,
                    ExecutionDisposition::Full { reason } => {
                        persist_fallback_diagnostic(
                            &result_dir(
                                state_dir,
                                repo,
                                pr,
                                &observation.receipt.head_sha,
                                &target.name,
                            ),
                            &FallbackDiagnostic {
                                schema_version: 1,
                                repository: repo,
                                pull_request: pr,
                                target: &target.name,
                                machine_mode: machine.mode,
                                category: "full_fallback",
                                diagnostic: bounded_diagnostic(&format!("{reason:?}")),
                            },
                        )?;
                        continue;
                    }
                    ExecutionDisposition::Blocked { reason } => {
                        persist_fallback_diagnostic(
                            &result_dir(
                                state_dir,
                                repo,
                                pr,
                                &observation.receipt.head_sha,
                                &target.name,
                            ),
                            &FallbackDiagnostic {
                                schema_version: 1,
                                repository: repo,
                                pull_request: pr,
                                target: &target.name,
                                machine_mode: machine.mode,
                                category: "blocked",
                                diagnostic: bounded_diagnostic(&reason),
                            },
                        )?;
                        return Err(CliFailure::new(1, bounded_diagnostic(&reason)));
                    }
                };
                (plan, None, None)
            };
        if !machine.permits_authoritative(repo, &target.name, &plan.policy_digest) {
            persist_fallback_diagnostic(
                &result_dir(state_dir, repo, pr, &plan.head_sha, &target.name),
                &FallbackDiagnostic {
                    schema_version: 1,
                    repository: repo,
                    pull_request: pr,
                    target: &target.name,
                    machine_mode: machine.mode,
                    category: "graduation_fence",
                    diagnostic: "authoritative mode requires the exact reviewed shadow policy digest for this repository and target"
                        .to_owned(),
                },
            )?;
            continue;
        }
        let original_test = validation
            .stages
            .get("test")
            .expect("checked local test stage")
            .clone();
        let original_build = if plan.stage == "build_and_test" {
            let Some(build) = validation.stages.get("build").cloned() else {
                persist_fallback_diagnostic(
                    &result_dir(state_dir, repo, pr, &plan.head_sha, &target.name),
                    &FallbackDiagnostic {
                        schema_version: 1,
                        repository: repo,
                        pull_request: pr,
                        target: &target.name,
                        machine_mode: machine.mode,
                        category: "full_fallback",
                        diagnostic: "selected build-and-test requires a canonical build stage; preserving the original validation stages"
                            .to_owned(),
                    },
                )?;
                continue;
            };
            Some(build)
        } else {
            None
        };
        let result_dir = result_dir(
            state_dir,
            repo,
            pr,
            stale_receipt
                .as_ref()
                .map_or(plan.head_sha.as_str(), |receipt| receipt.head_sha.as_str()),
            &target.name,
        );
        let compare = if stale_receipt.is_some() {
            "0"
        } else if machine.mode == MachineMode::ShadowCompare {
            "1"
        } else {
            "0"
        };
        let substituted = format!(
            "SHIPYARD_CHANGED_SURFACE_RESULT_DIR={} SHIPYARD_CHANGED_SURFACE_COMPARE_FULL={} {}",
            shell_quote(&result_dir),
            compare,
            plan.command
        );
        let activation = ActivationReceipt {
            schema_version: u32::from(plan.stage == "build_and_test") + 1,
            machine_mode: machine.mode,
            plan: &plan,
            original_build_command_sha256: original_build
                .as_ref()
                .map(|command| sha256(command.as_bytes())),
            original_test_command_sha256: sha256(original_test.as_bytes()),
            substituted_build_command_sha256: (plan.stage == "build_and_test")
                .then(|| sha256(substituted.as_bytes())),
            substituted_test_command_sha256: sha256(
                if plan.stage == "build_and_test" {
                    ":"
                } else {
                    &substituted
                }
                .as_bytes(),
            ),
        };
        if let Some(receipt) = stale_receipt.as_ref() {
            persist_stale_activation(
                &result_dir,
                &StaleActivationReceipt {
                    schema_version: activation.schema_version,
                    machine_mode: machine.mode,
                    merge_authority: receipt.merge_authority,
                    stale_context_digest: crate::changed_surface::stale_base_context_digest(
                        receipt,
                    ),
                    stale_receipt_sha256: sha256(&serde_json::to_vec(receipt).map_err(
                        |error| {
                            CliFailure::new(1, format!("serialize stale activation link: {error}"))
                        },
                    )?),
                    plan: &plan,
                    original_build_command_sha256: activation.original_build_command_sha256,
                    original_test_command_sha256: activation.original_test_command_sha256,
                    substituted_build_command_sha256: activation.substituted_build_command_sha256,
                    substituted_test_command_sha256: activation.substituted_test_command_sha256,
                },
            )?;
        } else {
            persist_activation(&result_dir, &activation)?;
        }
        if let Some(checkout) = stale_checkout {
            let ResolvedBackend::Local(local) = &mut target.backend else {
                return Err(CliFailure::new(
                    1,
                    "stale integration execution requires the local backend",
                ));
            };
            local.cwd = Some(checkout.path.clone());
            let ResolvedValidation::Local(validation) = &mut target.validation else {
                unreachable!();
            };
            validation.integration_cleanup = Some(Box::new(checkout));
        }
        let ResolvedValidation::Local(validation) = &mut target.validation else {
            unreachable!();
        };
        if plan.stage == "build_and_test" {
            validation.stages.insert("build".to_owned(), substituted);
            validation.stages.insert("test".to_owned(), ":".to_owned());
        } else {
            validation.stages.insert("test".to_owned(), substituted);
        }
    }
    Ok(())
}

fn persist_stale_activation(
    path: &Path,
    receipt: &StaleActivationReceipt<'_>,
) -> Result<(), CliFailure> {
    persist_named_receipt(path, "stale-activation-shadow_compare.json", receipt)
}

fn persist_named_receipt<T: Serialize>(
    path: &Path,
    name: &str,
    receipt: &T,
) -> Result<(), CliFailure> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    fs::create_dir_all(path).map_err(|error| {
        CliFailure::new(1, format!("create selector evidence directory: {error}"))
    })?;
    let mut payload = serde_json::to_vec_pretty(receipt)
        .map_err(|error| CliFailure::new(1, format!("serialize selector receipt: {error}")))?;
    payload.push(b'\n');
    let destination = path.join(name);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
    {
        Ok(mut file) => file
            .write_all(&payload)
            .and_then(|()| file.sync_all())
            .map_err(|error| CliFailure::new(1, format!("write selector receipt: {error}")))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::read(&destination).map_err(|error| {
                CliFailure::new(1, format!("read existing selector receipt: {error}"))
            })? != payload
            {
                return Err(CliFailure::new(
                    1,
                    "immutable selector receipt already exists with different bytes",
                ));
            }
        }
        Err(error) => {
            return Err(CliFailure::new(
                1,
                format!("create selector receipt: {error}"),
            ));
        }
    }
    #[cfg(unix)]
    sync_directory(path)?;
    Ok(())
}

fn result_dir(state_dir: &Path, repo: &str, pr: u64, head: &str, target: &str) -> PathBuf {
    result_directory(
        state_dir,
        &TrialIdentity {
            repository: repo.to_owned(),
            pull_request: pr,
            target: target.to_owned(),
            head_sha: head.to_owned(),
        },
    )
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn persist_activation(path: &Path, receipt: &ActivationReceipt<'_>) -> Result<(), CliFailure> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    fs::create_dir_all(path).map_err(|error| {
        CliFailure::new(1, format!("create selector evidence directory: {error}"))
    })?;
    let payload = serde_json::to_vec_pretty(receipt)
        .map_err(|error| CliFailure::new(1, format!("serialize selector activation: {error}")))?;
    let destination = path.join(format!("activation-{}.json", receipt.machine_mode.as_str()));
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
    {
        Ok(mut file) => {
            file.write_all(&payload)
                .and_then(|()| file.write_all(b"\n"))
                .and_then(|()| file.sync_all())
                .map_err(|error| {
                    CliFailure::new(1, format!("write selector activation: {error}"))
                })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(&destination).map_err(|error| {
                CliFailure::new(1, format!("read existing selector activation: {error}"))
            })?;
            let mut expected = payload;
            expected.push(b'\n');
            if existing != expected {
                return Err(CliFailure::new(
                    1,
                    "immutable selector activation receipt already exists with different bytes",
                ));
            }
        }
        Err(error) => {
            return Err(CliFailure::new(
                1,
                format!("create selector activation receipt: {error}"),
            ));
        }
    }
    #[cfg(unix)]
    sync_directory(path)?;
    Ok(())
}

fn persist_fallback_diagnostic(
    path: &Path,
    diagnostic: &FallbackDiagnostic<'_>,
) -> Result<(), CliFailure> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    fs::create_dir_all(path).map_err(|error| {
        CliFailure::new(1, format!("create selector diagnostic directory: {error}"))
    })?;
    let payload = serde_json::to_vec(diagnostic)
        .map_err(|error| CliFailure::new(1, format!("serialize selector diagnostic: {error}")))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for sequence in 0..100_u8 {
        let destination = path.join(format!(
            "fallback-{nanos}-{}-{sequence}.json",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
        {
            Ok(mut file) => {
                file.write_all(&payload)
                    .and_then(|()| file.write_all(b"\n"))
                    .and_then(|()| file.sync_all())
                    .map_err(|error| {
                        CliFailure::new(1, format!("write selector diagnostic: {error}"))
                    })?;
                #[cfg(unix)]
                sync_directory(path)?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(CliFailure::new(
                    1,
                    format!("create selector diagnostic: {error}"),
                ));
            }
        }
    }
    Err(CliFailure::new(
        1,
        "cannot allocate immutable selector diagnostic",
    ))
}

fn persist_stale_base_shadow(
    path: &Path,
    receipt: &StaleBaseShadowReceipt,
) -> Result<(), CliFailure> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    fs::create_dir_all(path).map_err(|error| {
        CliFailure::new(1, format!("create selector evidence directory: {error}"))
    })?;
    let payload = serde_json::to_vec_pretty(receipt)
        .map_err(|error| CliFailure::new(1, format!("serialize stale-base shadow: {error}")))?;
    let destination = path.join(format!(
        "stale-base-shadow-{}-{}-{}.json",
        receipt.live_protected_base_sha,
        receipt.protected_base_delta_digest,
        crate::changed_surface::stale_base_context_digest(receipt)
    ));
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
    {
        Ok(mut file) => {
            file.write_all(&payload)
                .and_then(|()| file.write_all(b"\n"))
                .and_then(|()| file.sync_all())
                .map_err(|error| {
                    CliFailure::new(1, format!("write stale-base shadow receipt: {error}"))
                })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(&destination).map_err(|error| {
                CliFailure::new(1, format!("read stale-base shadow receipt: {error}"))
            })?;
            let mut expected = payload;
            expected.push(b'\n');
            if existing != expected {
                return Err(CliFailure::new(
                    1,
                    "immutable stale-base shadow receipt already exists with different bytes",
                ));
            }
        }
        Err(error) => {
            return Err(CliFailure::new(
                1,
                format!("create stale-base shadow receipt: {error}"),
            ));
        }
    }
    #[cfg(unix)]
    sync_directory(path)?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), CliFailure> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| CliFailure::new(1, format!("sync selector evidence directory: {error}")))
}

fn bounded_diagnostic(value: &str) -> String {
    value.chars().take(MAX_DIAGNOSTIC_CHARS).collect()
}

fn target_declares_changed_surface_selection(config: &LoadedConfig, target: &str) -> bool {
    config
        .get("targets")
        .and_then(toml::Value::as_table)
        .and_then(|targets| targets.get(target))
        .and_then(toml::Value::as_table)
        .and_then(|target| target.get("changed_surface_selection"))
        .is_some()
}

fn selected_resume_block_reason(
    plan_stage: &str,
    resume_from: Option<&str>,
    would_activate: bool,
) -> Option<&'static str> {
    (plan_stage == "build_and_test" && resume_from == Some("test") && would_activate).then_some(
        "resume-from test cannot prove a changed-surface build/test transaction; restart from build or start a fresh validation",
    )
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::{
        FallbackDiagnostic, MachineMode, MachinePolicy, bounded_diagnostic,
        persist_fallback_diagnostic, result_dir, selected_resume_block_reason, shell_quote,
        target_declares_changed_surface_selection,
    };
    use crate::config::{LoadedConfig, LocalOverlaySource};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    #[test]
    fn machine_mode_is_default_off_and_ignores_repo_layers() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path()).unwrap();
        let mut config = LoadedConfig::load(
            Some(temp.path().to_path_buf()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        config.data.insert(
            "changed_surface_execution".to_owned(),
            toml::toml! { mode = "authoritative" }.into(),
        );
        assert_eq!(
            MachinePolicy::from_global(&config).unwrap().mode,
            MachineMode::Off
        );
    }

    #[test]
    fn path_inputs_are_bounded_before_shell_use() {
        let state = Path::new("/state");
        assert_ne!(
            result_dir(state, "a/b", 1, "head", "mac"),
            result_dir(state, "a_b", 1, "head", "mac")
        );
        assert_eq!(
            result_dir(state, "a/b", 1, "head", "mac"),
            result_dir(state, "a/b", 1, "head", "mac")
        );
        assert_eq!(shell_quote(std::path::Path::new("a'b")), "'a'\"'\"'b'");
    }

    #[test]
    fn invalid_global_mode_fails_closed_instead_of_enabling() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            "[changed_surface_execution]\nmode = 'fast'\n",
        )
        .unwrap();
        let config = LoadedConfig::load(
            Some(temp.path().to_path_buf()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        let error = MachinePolicy::from_global(&config).unwrap_err();
        assert!(error.message.contains("invalid trusted"));
    }

    #[test]
    fn accepted_shadow_digest_requires_canonical_lowercase_sha256() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            format!(
                "[changed_surface_execution]\nmode = 'authoritative'\naccepted_shadow_policy_digest = '{}'\n",
                "A".repeat(64)
            ),
        )
        .unwrap();
        let config = LoadedConfig::load(
            Some(temp.path().to_path_buf()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        assert!(MachinePolicy::from_global(&config).is_err());
    }

    #[test]
    fn authoritative_requires_exact_accepted_shadow_policy_digest() {
        let digest = "a".repeat(64);
        let missing = MachinePolicy {
            mode: MachineMode::Authoritative,
            legacy_accepted_shadow_policy_digest: None,
            accepted_shadow_policy_digests: BTreeMap::new(),
        };
        assert!(!missing.permits_authoritative("Generous-Corp/pulp", "mac", &digest));
        let accepted = MachinePolicy {
            mode: MachineMode::Authoritative,
            legacy_accepted_shadow_policy_digest: Some(digest.clone()),
            accepted_shadow_policy_digests: BTreeMap::new(),
        };
        assert!(accepted.permits_authoritative("Generous-Corp/pulp", "mac", &digest));
        assert!(!accepted.permits_authoritative("Generous-Corp/pulp", "mac", &"b".repeat(64)));
        let shadow = MachinePolicy {
            mode: MachineMode::ShadowCompare,
            legacy_accepted_shadow_policy_digest: None,
            accepted_shadow_policy_digests: BTreeMap::new(),
        };
        assert!(shadow.permits_authoritative("Generous-Corp/pulp", "mac", &digest));
    }

    #[test]
    fn legacy_scalar_config_remains_authoritative_without_scoped_table() {
        let temp = tempfile::tempdir().unwrap();
        let digest = "a".repeat(64);
        fs::write(
            temp.path().join("config.toml"),
            format!(
                "[changed_surface_execution]\nmode = 'authoritative'\naccepted_shadow_policy_digest = '{digest}'\n"
            ),
        )
        .unwrap();
        let config = LoadedConfig::load(
            Some(temp.path().to_path_buf()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        let policy = MachinePolicy::from_global(&config).unwrap();

        assert!(policy.permits_authoritative("Generous-Corp/pulp", "mac", &digest));
        assert!(!policy.permits_authoritative("Generous-Corp/pulp", "mac", &"b".repeat(64)));
    }

    #[test]
    fn scoped_digests_authorize_pulp_and_forge_without_cross_authorizing() {
        let temp = tempfile::tempdir().unwrap();
        let pulp_digest = "a".repeat(64);
        let forge_digest = "b".repeat(64);
        fs::write(
            temp.path().join("config.toml"),
            format!(
                "[changed_surface_execution]\nmode = 'authoritative'\n\
                 [changed_surface_execution.accepted_shadow_policy_digests.\"Generous-Corp/pulp\"]\nmac = '{pulp_digest}'\n\
                 [changed_surface_execution.accepted_shadow_policy_digests.\"Generous-Corp/forge\"]\nmac = '{forge_digest}'\n"
            ),
        )
        .unwrap();
        let config = LoadedConfig::load(
            Some(temp.path().to_path_buf()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        let policy = MachinePolicy::from_global(&config).unwrap();

        assert!(policy.permits_authoritative("generous-corp/PULP", "mac", &pulp_digest));
        assert!(policy.permits_authoritative("Generous-Corp/forge", "mac", &forge_digest));
        assert!(!policy.permits_authoritative("Generous-Corp/pulp", "mac", &forge_digest));
        assert!(!policy.permits_authoritative("Generous-Corp/forge", "mac", &pulp_digest));
        assert!(!policy.permits_authoritative("Generous-Corp/pulp", "linux", &pulp_digest));
        assert!(!policy.permits_authoritative("Generous-Corp/vellum", "mac", &pulp_digest));
    }

    #[test]
    fn scalar_and_scoped_digests_are_rejected_as_ambiguous() {
        let temp = tempfile::tempdir().unwrap();
        let digest = "a".repeat(64);
        fs::write(
            temp.path().join("config.toml"),
            format!(
                "[changed_surface_execution]\nmode = 'authoritative'\naccepted_shadow_policy_digest = '{digest}'\n\
                 [changed_surface_execution.accepted_shadow_policy_digests.\"Generous-Corp/pulp\"]\nmac = '{digest}'\n"
            ),
        )
        .unwrap();
        let config = LoadedConfig::load(
            Some(temp.path().to_path_buf()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        let error = MachinePolicy::from_global(&config).unwrap_err();
        assert!(error.message.contains("ambiguous trusted"));
    }

    #[test]
    fn canonical_repository_collisions_are_rejected_as_ambiguous() {
        let temp = tempfile::tempdir().unwrap();
        let digest = "a".repeat(64);
        fs::write(
            temp.path().join("config.toml"),
            format!(
                "[changed_surface_execution]\nmode = 'authoritative'\n\
                 [changed_surface_execution.accepted_shadow_policy_digests.\"Generous-Corp/pulp\"]\nmac = '{digest}'\n\
                 [changed_surface_execution.accepted_shadow_policy_digests.\"generous-corp/PULP\"]\nlinux = '{digest}'\n"
            ),
        )
        .unwrap();
        let config = LoadedConfig::load(
            Some(temp.path().to_path_buf()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        let error = MachinePolicy::from_global(&config).unwrap_err();
        assert!(error.message.contains("duplicates canonical repository"));
    }

    #[test]
    fn explicit_empty_scoped_digest_table_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            "[changed_surface_execution]\nmode = 'authoritative'\n\
             [changed_surface_execution.accepted_shadow_policy_digests]\n",
        )
        .unwrap();
        let config = LoadedConfig::load(
            Some(temp.path().to_path_buf()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        let error = MachinePolicy::from_global(&config).unwrap_err();
        assert!(error.message.contains("table is empty"));
    }

    #[test]
    fn fallback_diagnostics_are_bounded_append_only_receipts() {
        let temp = tempfile::tempdir().unwrap();
        let diagnostic = FallbackDiagnostic {
            schema_version: 1,
            repository: "owner/repo",
            pull_request: 7,
            target: "mac",
            machine_mode: MachineMode::ShadowCompare,
            category: "observation_error",
            diagnostic: bounded_diagnostic(&"x".repeat(2_000)),
        };
        assert_eq!(diagnostic.diagnostic.chars().count(), 512);
        persist_fallback_diagnostic(temp.path(), &diagnostic).unwrap();
        persist_fallback_diagnostic(temp.path(), &diagnostic).unwrap();
        assert_eq!(
            fs::read_dir(temp.path())
                .unwrap()
                .filter_map(Result::ok)
                .count(),
            2
        );
    }

    #[test]
    fn changed_surface_target_detection_is_exact() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            "[targets.mac.changed_surface_selection]\npolicy = '.shipyard/policy.toml'\n",
        )
        .unwrap();
        let config = LoadedConfig::load(
            Some(temp.path().to_path_buf()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        assert!(target_declares_changed_surface_selection(&config, "mac"));
        assert!(!target_declares_changed_surface_selection(&config, "linux"));

        assert_eq!(
            selected_resume_block_reason("build_and_test", Some("test"), true),
            Some(
                "resume-from test cannot prove a changed-surface build/test transaction; restart from build or start a fresh validation"
            )
        );
        assert_eq!(
            selected_resume_block_reason("test", Some("test"), true),
            None
        );
        assert_eq!(
            selected_resume_block_reason("build_and_test", Some("build"), true),
            None
        );
        assert_eq!(
            selected_resume_block_reason("build_and_test", Some("test"), false),
            None
        );
    }
}
