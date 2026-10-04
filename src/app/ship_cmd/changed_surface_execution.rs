use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::CliFailure;
use super::super::changed_surface_cmd::{
    ChangedSurfacePlanArgs, observe_changed_surface_plan, observe_stale_base_shadow,
};
use super::executable_reuse_plan::{KeyRequest, Keyed, plan_keyed};
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
const MAX_STALE_POINTER_BYTES: u64 = 64 * 1024;

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
struct CurrentStaleGeneration<'a> {
    schema_version: u32,
    repository: &'a str,
    pull_request: u64,
    target: &'a str,
    head_sha: &'a str,
    live_base_sha: &'a str,
    context_digest: &'a str,
    stale_receipt_sha256: &'a str,
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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn apply_changed_surface_execution(
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    repo: &str,
    pr: Option<u64>,
    head_sha: &str,
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
                    && merge_base_promotion_refusal(machine.mode, &observation.receipt).is_none()
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
        // Fence the current generation before observing the protected base.
        // A snapshot taken afterwards could let an older observation treat a
        // newer pointer as permission to publish the older generation.
        let stale_pointer_snapshot = if machine.mode == MachineMode::ShadowCompare {
            let evidence_root = result_dir(state_dir, repo, pr, head_sha, &target.name);
            match read_current_stale_generation(&evidence_root) {
                Ok(pointer) => Some(pointer),
                Err(error) => {
                    persist_fallback_diagnostic(
                        &evidence_root,
                        &FallbackDiagnostic {
                            schema_version: 1,
                            repository: repo,
                            pull_request: pr,
                            target: &target.name,
                            machine_mode: machine.mode,
                            category: "stale_generation_pointer_unreadable",
                            diagnostic: bounded_diagnostic(&error.message),
                        },
                    )?;
                    continue;
                }
            }
        } else {
            None
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
        let keyed = keyed_shadow_plan(
            machine.mode,
            &observation,
            policy,
            &disposition,
            validation.reuse_record_repository.as_deref(),
            cwd,
            state_dir,
            &contract_digest,
        );
        if let Some(Keyed::Closeout {
            category,
            diagnostic,
        }) = &keyed
        {
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
                    category,
                    diagnostic: bounded_diagnostic(diagnostic),
                },
            )?;
        }
        let keyed = match keyed {
            Some(Keyed::Planned(plan)) => Some(plan),
            _ => None,
        };
        let mut stale_execution = None;
        if keyed.is_none()
            && machine.mode == MachineMode::ShadowCompare
            && observation.receipt.fallback_reason == Some(FallbackReason::StaleBase)
            && matches!(disposition, ExecutionDisposition::Full { .. })
        {
            let evidence_root = result_dir(state_dir, repo, pr, head_sha, &target.name);
            let pointer_before = stale_pointer_snapshot
                .as_ref()
                .expect("shadow comparison snapshots before observation");
            let assessment = observe_stale_base_shadow(&observation, cwd, &contract_digest)?;
            let context_digest =
                crate::changed_surface::stale_base_context_digest(&assessment.receipt);
            let evidence_dir = evidence_root
                .join("stale-generations")
                .join(&context_digest);
            let checkout_parent = evidence_dir.join("integration-checkouts");
            // Persist the shadow-only authority fence before any isolated
            // execution. If planning, persistence, or materialization cannot
            // prove the exact integration identity, ordinary full validation
            // remains untouched below.
            let shadow_receipt_digest =
                persist_stale_base_shadow(&evidence_dir, &assessment.receipt)
                    .and_then(|digest| {
                        publish_current_stale_generation(
                            &evidence_root,
                            pointer_before.as_deref(),
                            &CurrentStaleGeneration {
                                schema_version: 1,
                                repository: repo,
                                pull_request: pr,
                                target: &target.name,
                                head_sha: &assessment.receipt.head_sha,
                                live_base_sha: &assessment.receipt.live_protected_base_sha,
                                context_digest: &context_digest,
                                stale_receipt_sha256: &digest,
                            },
                        )?;
                        Ok(digest)
                    })
                    .ok();
            let cleanup_reconciliation =
                crate::changed_surface::integration_checkout::reconcile_pending_cleanup(
                    cwd,
                    &checkout_parent,
                    &assessment.receipt,
                );
            if let Some(stale_receipt_sha256) = shadow_receipt_digest
                && cleanup_reconciliation.is_ok()
                && !stale_generation_has_execution_evidence(&evidence_dir)
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
                match crate::changed_surface::integration_checkout::plan(
                    cwd,
                    &checkout_parent,
                    &assessment.receipt,
                ) {
                    Ok(checkout) => {
                        stale_execution = Some((
                            plan,
                            assessment.receipt,
                            checkout,
                            stale_receipt_sha256,
                            evidence_dir,
                        ));
                    }
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
        let (plan, stale_receipt, stale_checkout) = if let Some((
            plan,
            receipt,
            checkout,
            receipt_digest,
            evidence_dir,
        )) = stale_execution
        {
            (
                plan,
                Some((receipt, receipt_digest, evidence_dir)),
                Some(checkout),
            )
        } else if let Some(plan) = keyed {
            (plan, None, None)
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
                            diagnostic: bounded_diagnostic(&full_fallback_diagnostic(
                                reason,
                                observation.receipt.fallback_reason.as_ref(),
                                observation.receipt.fallback_detail.as_deref(),
                            )),
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
        if let Some(reason) = stale_receipt
            .is_none()
            .then(|| merge_base_promotion_refusal(machine.mode, &observation.receipt))
            .flatten()
        {
            persist_fallback_diagnostic(
                &result_dir(state_dir, repo, pr, &plan.head_sha, &target.name),
                &FallbackDiagnostic {
                    schema_version: 1,
                    repository: repo,
                    pull_request: pr,
                    target: &target.name,
                    machine_mode: machine.mode,
                    category: "merge_base_policy_diverged",
                    diagnostic: reason,
                },
            )?;
            continue;
        }
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
        let result_dir = stale_receipt.as_ref().map_or_else(
            || result_dir(state_dir, repo, pr, &plan.head_sha, &target.name),
            |(_, _, evidence_dir)| evidence_dir.clone(),
        );
        let compare = if machine.mode == MachineMode::ShadowCompare {
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
        if let Some((receipt, receipt_digest, _)) = stale_receipt.as_ref() {
            persist_stale_activation(
                &result_dir,
                &StaleActivationReceipt {
                    schema_version: activation.schema_version,
                    machine_mode: machine.mode,
                    merge_authority: receipt.merge_authority,
                    stale_context_digest: crate::changed_surface::stale_base_context_digest(
                        receipt,
                    ),
                    stale_receipt_sha256: receipt_digest.clone(),
                    plan: &plan,
                    original_build_command_sha256: activation.original_build_command_sha256,
                    original_test_command_sha256: activation.original_test_command_sha256,
                    substituted_build_command_sha256: activation.substituted_build_command_sha256,
                    substituted_test_command_sha256: activation.substituted_test_command_sha256,
                },
            )?;
        } else {
            let planned = activation_bytes(&activation)?;
            if let Ok(existing) =
                fs::read(result_dir.join(activation_file_name(activation.machine_mode)))
            {
                let context = fs::read(
                    result_dir
                        .join(crate::changed_surface::executable_reuse::KEYED_CONTEXT_RECEIPT),
                )
                .ok();
                if let Some(conflict) = activation_conflict(
                    &existing,
                    &planned,
                    context.as_deref(),
                    &plan.execution_payload,
                ) {
                    // A keyed shadow never breaks a ship: record why this head
                    // runs its configured stages unkeyed, and move on.
                    persist_fallback_diagnostic(
                        &result_dir,
                        &ActivationConflictDiagnostic {
                            schema_version: 1,
                            repository: repo,
                            pull_request: pr,
                            target: &target.name,
                            machine_mode: machine.mode,
                            category: "activation_conflict",
                            status: "unkeyed: activation_conflict",
                            conflict: &conflict,
                        },
                    )?;
                    continue;
                }
            }
            persist_activation(&result_dir, &activation)?;
            if plan.disposition != crate::changed_surface::BOUNDED {
                persist_named_receipt(
                    &result_dir,
                    crate::changed_surface::executable_reuse::KEYED_CONTEXT_RECEIPT,
                    &crate::changed_surface::executable_reuse::KeyedRunContext {
                        schema_version: 1,
                        repository: repo.to_owned(),
                        checkout: cwd.to_string_lossy().into_owned(),
                        execution_payload_b64: base64::Engine::encode(
                            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                            &plan.execution_payload,
                        ),
                    },
                )?;
            }
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

/// A keyed shadow plan replaces the configured stages only on a shadow host,
/// for a full or bounded plan that is not a stale-base comparison, and only
/// when the protected base declares executable reuse. `None` leaves the
/// existing behaviour untouched.
#[allow(clippy::too_many_arguments)]
fn keyed_shadow_plan(
    mode: MachineMode,
    observation: &super::super::changed_surface_cmd::ChangedSurfaceObservation,
    policy: &crate::changed_surface::ChangedSurfacePolicy,
    disposition: &ExecutionDisposition,
    record_repository: Option<&str>,
    cwd: &Path,
    state_dir: &Path,
    contract_digest: &str,
) -> Option<Keyed> {
    let reuse = policy.executable_reuse.as_ref()?;
    if !keys_this_plan(
        mode,
        observation.receipt.fallback_reason.as_ref(),
        disposition,
    ) {
        return None;
    }
    Some(plan_keyed(&KeyRequest {
        observation,
        policy,
        reuse,
        record_repository,
        cwd,
        state_dir,
        contract_digest,
    }))
}

/// Keyed runs are shadow measurement: never on an authoritative host, never
/// for a blocked plan, and never in place of a stale-base comparison.
fn keys_this_plan(
    mode: MachineMode,
    fallback_reason: Option<&FallbackReason>,
    disposition: &ExecutionDisposition,
) -> bool {
    mode == MachineMode::ShadowCompare
        && fallback_reason != Some(&FallbackReason::StaleBase)
        && !matches!(disposition, ExecutionDisposition::Blocked { .. })
}

fn read_current_stale_generation(path: &Path) -> Result<Option<Vec<u8>>, CliFailure> {
    read_bounded_stale_pointer(&path.join("stale-current.json"))
}

fn read_bounded_stale_pointer(path: &Path) -> Result<Option<Vec<u8>>, CliFailure> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(CliFailure::new(
                1,
                format!("inspect current stale generation: {error}"),
            ));
        }
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_STALE_POINTER_BYTES
    {
        return Err(CliFailure::new(
            1,
            "current stale generation is not a bounded regular file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000);
    }
    let file = options
        .open(path)
        .map_err(|error| CliFailure::new(1, format!("open current stale generation: {error}")))?;
    let opened = file
        .metadata()
        .map_err(|error| CliFailure::new(1, format!("inspect opened stale generation: {error}")))?;
    if !opened.is_file() || opened.len() > MAX_STALE_POINTER_BYTES {
        return Err(CliFailure::new(
            1,
            "opened stale generation is not a bounded regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_STALE_POINTER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| CliFailure::new(1, format!("read current stale generation: {error}")))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_STALE_POINTER_BYTES {
        return Err(CliFailure::new(
            1,
            "current stale generation exceeds size limit",
        ));
    }
    Ok(Some(bytes))
}

fn persist_stale_activation(
    path: &Path,
    receipt: &StaleActivationReceipt<'_>,
) -> Result<(), CliFailure> {
    persist_named_receipt(path, "stale-activation-shadow_compare.json", receipt)
}

fn publish_current_stale_generation(
    path: &Path,
    expected_current: Option<&[u8]>,
    generation: &CurrentStaleGeneration<'_>,
) -> Result<(), CliFailure> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    fs::create_dir_all(path).map_err(|error| {
        CliFailure::new(1, format!("create stale generation directory: {error}"))
    })?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.join(".stale-current.lock"))
        .map_err(|error| CliFailure::new(1, format!("open stale generation lock: {error}")))?;
    FileExt::lock_exclusive(&lock)
        .map_err(|error| CliFailure::new(1, format!("lock stale generation: {error}")))?;
    let mut payload = serde_json::to_vec_pretty(generation)
        .map_err(|error| CliFailure::new(1, format!("serialize stale generation: {error}")))?;
    payload.push(b'\n');
    let destination = path.join("stale-current.json");
    let current = read_bounded_stale_pointer(&destination)?;
    if current.as_deref() == Some(payload.as_slice()) {
        return Ok(());
    }
    if current.as_deref() != expected_current {
        return Err(CliFailure::new(
            1,
            "stale generation advanced after observation; refusing to regress the current pointer",
        ));
    }
    let temporary = path.join(format!(
        ".stale-current.{}.{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| CliFailure::new(1, format!("create stale generation temp: {error}")))?;
    file.write_all(&payload)
        .and_then(|()| file.sync_all())
        .map_err(|error| CliFailure::new(1, format!("write stale generation temp: {error}")))?;
    drop(file);
    fs::rename(&temporary, &destination)
        .map_err(|error| CliFailure::new(1, format!("publish stale generation: {error}")))?;
    #[cfg(unix)]
    sync_directory(path)?;
    Ok(())
}

fn stale_generation_has_execution_evidence(path: &Path) -> bool {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return true;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return true;
        };
        if name == "stale-activation-shadow_compare.json"
            || name == "stale-cleanup-shadow_compare.json"
            || name == ".stale-cleanup-shadow_compare.pending"
            || name.starts_with("result-")
            || name.starts_with("fallback-")
        {
            return true;
        }
    }
    false
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

fn activation_file_name(mode: MachineMode) -> String {
    format!("activation-{}.json", mode.as_str())
}

/// The exact bytes an activation receipt is stored as.
fn activation_bytes(receipt: &ActivationReceipt<'_>) -> Result<Vec<u8>, CliFailure> {
    let mut payload = serde_json::to_vec_pretty(receipt)
        .map_err(|error| CliFailure::new(1, format!("serialize selector activation: {error}")))?;
    payload.push(b'\n');
    Ok(payload)
}

/// Why a head's stored activation differs from the one this ship would
/// write, when either is keyed. A keyed plan binds the host's candidate
/// records at ship time, so the same head re-shipped after the store changes,
/// or first shipped before a base existed, plans differently.
#[derive(Debug, Eq, PartialEq, Serialize)]
struct ActivationConflict {
    existing_payload_sha256: Option<String>,
    planned_payload_sha256: Option<String>,
    differing_fields: Vec<String>,
}

/// Compare a stored activation with the one this ship would write. `None`
/// when they are identical, or when neither is keyed (an unkeyed plan that
/// changed is still refused, as before).
fn activation_conflict(
    existing_activation: &[u8],
    planned_activation: &[u8],
    existing_context: Option<&[u8]>,
    planned_payload: &[u8],
) -> Option<ActivationConflict> {
    if existing_activation == planned_activation {
        return None;
    }
    let parse = |bytes: &[u8]| serde_json::from_slice::<serde_json::Value>(bytes).ok();
    let existing = parse(existing_activation).unwrap_or_default();
    let planned = parse(planned_activation).unwrap_or_default();
    let plan_field =
        |value: &serde_json::Value, key: &str| value.pointer(&format!("/plan/{key}")).cloned();
    let keyed = |value: &serde_json::Value| {
        plan_field(value, "disposition")
            .and_then(|d| d.as_str().map(|d| d.starts_with("keyed_")))
            .unwrap_or(false)
    };
    if !keyed(&existing) && !keyed(&planned) {
        return None;
    }
    let mut differing = Vec::new();
    for key in [
        "disposition",
        "base_sha",
        "policy_digest",
        "selection_receipt_digest",
        "selected_tests_digest",
        "execution_payload_digest",
    ] {
        if plan_field(&existing, key) != plan_field(&planned, key) {
            differing.push(key.to_owned());
        }
    }
    let existing_binding = existing_context
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok())
        .and_then(|context| {
            context
                .get("execution_payload_b64")
                .and_then(serde_json::Value::as_str)
                .and_then(|b64| {
                    base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, b64)
                        .ok()
                })
        })
        .and_then(|payload| parse(&payload))
        .and_then(|payload| payload.get("executable_reuse").cloned());
    let planned_binding =
        parse(planned_payload).and_then(|payload| payload.get("executable_reuse").cloned());
    match (&existing_binding, &planned_binding) {
        (Some(old), Some(new)) => {
            for key in [
                "candidates",
                "sample_seed",
                "rules_digest",
                "derivation_code_sha256",
            ] {
                if old.get(key) != new.get(key) {
                    differing.push(format!("executable_reuse.{key}"));
                }
            }
        }
        (None, None) => {}
        _ => differing.push("executable_reuse".to_owned()),
    }
    if differing.is_empty() {
        differing.push("activation".to_owned());
    }
    let digest = |value: &serde_json::Value| {
        plan_field(value, "execution_payload_digest").and_then(|d| d.as_str().map(str::to_owned))
    };
    Some(ActivationConflict {
        existing_payload_sha256: digest(&existing),
        planned_payload_sha256: digest(&planned),
        differing_fields: differing,
    })
}

#[derive(Debug, Serialize)]
struct ActivationConflictDiagnostic<'a> {
    schema_version: u32,
    repository: &'a str,
    pull_request: u64,
    target: &'a str,
    machine_mode: MachineMode,
    category: &'a str,
    status: &'a str,
    #[serde(flatten)]
    conflict: &'a ActivationConflict,
}

fn persist_activation(path: &Path, receipt: &ActivationReceipt<'_>) -> Result<(), CliFailure> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    fs::create_dir_all(path).map_err(|error| {
        CliFailure::new(1, format!("create selector evidence directory: {error}"))
    })?;
    let payload = activation_bytes(receipt)?;
    let destination = path.join(activation_file_name(receipt.machine_mode));
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
    {
        Ok(mut file) => {
            file.write_all(&payload)
                .and_then(|()| file.sync_all())
                .map_err(|error| {
                    CliFailure::new(1, format!("write selector activation: {error}"))
                })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(&destination).map_err(|error| {
                CliFailure::new(1, format!("read existing selector activation: {error}"))
            })?;
            if existing != payload {
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

/// Name the planner's own fallback reason next to the execution reason, so a
/// full-suite fallback can be attributed to a wide change, a policy edit, or a
/// provenance failure without re-running the planner.
fn full_fallback_diagnostic(
    reason: crate::changed_surface::FullExecutionReason,
    fallback: Option<&FallbackReason>,
    detail: Option<&str>,
) -> String {
    let fallback = fallback.map_or_else(String::new, |fallback| format!(": {fallback:?}"));
    let detail = detail.map_or_else(String::new, |detail| format!(" ({detail})"));
    format!("{reason:?}{fallback}{detail}")
}

/// Authoritative execution of a plan made against the head's merge base,
/// rather than the PR's recorded base, requires the two bases to carry the
/// same selector policy; otherwise the plan may only run as a shadow.
fn merge_base_promotion_refusal(
    mode: MachineMode,
    receipt: &crate::changed_surface::SelectionReceipt,
) -> Option<String> {
    if mode != MachineMode::Authoritative || receipt.planned_base_sha.is_none() {
        return None;
    }
    match (&receipt.recorded_base_policy_digest, &receipt.policy_digest) {
        (Some(recorded), Some(planned)) if recorded == planned => None,
        (recorded, planned) => Some(format!(
            "planned at merge base {} whose selector policy {} differs from the recorded base's {}; \
             the plan may run only as a shadow",
            receipt.planned_base_sha.as_deref().unwrap_or("?"),
            planned.as_deref().unwrap_or("(none)"),
            recorded.as_deref().unwrap_or("(unparsed)"),
        )),
    }
}

fn persist_fallback_diagnostic<T: Serialize>(
    path: &Path,
    diagnostic: &T,
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
) -> Result<String, CliFailure> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    fs::create_dir_all(path).map_err(|error| {
        CliFailure::new(1, format!("create selector evidence directory: {error}"))
    })?;
    let mut payload = serde_json::to_vec_pretty(receipt)
        .map_err(|error| CliFailure::new(1, format!("serialize stale-base shadow: {error}")))?;
    payload.push(b'\n');
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
                .and_then(|()| file.sync_all())
                .map_err(|error| {
                    CliFailure::new(1, format!("write stale-base shadow receipt: {error}"))
                })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(&destination).map_err(|error| {
                CliFailure::new(1, format!("read stale-base shadow receipt: {error}"))
            })?;
            if existing != payload {
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
    Ok(sha256(&payload))
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
        CurrentStaleGeneration, FallbackDiagnostic, MAX_STALE_POINTER_BYTES, MachineMode,
        MachinePolicy, bounded_diagnostic, full_fallback_diagnostic, persist_fallback_diagnostic,
        publish_current_stale_generation, read_current_stale_generation, result_dir,
        selected_resume_block_reason, shell_quote, stale_generation_has_execution_evidence,
        target_declares_changed_surface_selection,
    };
    use crate::config::{LoadedConfig, LocalOverlaySource};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn full_fallback_diagnostic_names_the_planner_reason() {
        use crate::changed_surface::{FallbackReason, FullExecutionReason};
        assert_eq!(
            full_fallback_diagnostic(FullExecutionReason::ShadowPolicy, None, None),
            "ShadowPolicy"
        );
        assert_eq!(
            full_fallback_diagnostic(
                FullExecutionReason::PlannerSelectedFull,
                Some(&FallbackReason::UnmappedChangedPath),
                Some("unmapped paths: a/b.rs"),
            ),
            "PlannerSelectedFull: UnmappedChangedPath (unmapped paths: a/b.rs)"
        );
    }

    #[test]
    fn a_merge_base_plan_is_promoted_only_when_both_bases_share_a_policy() {
        use super::merge_base_promotion_refusal;
        let receipt = |planned: Option<&str>, recorded: Option<&str>, policy: &str| {
            let mut value = serde_json::json!({
                "schema_version": 1, "exact_head_verified": true, "shadow_only": true,
                "repository": "o/r", "pull_request": 1, "target": "mac", "protected_ref": "main",
                "pr_base_sha": "c".repeat(40), "protected_ref_sha": "c".repeat(40),
                "merge_base_sha": "b".repeat(40), "head_sha": "a".repeat(40),
                "tree_sha": "d".repeat(40), "changed_paths_digest": "e".repeat(64),
                "policy_digest": policy, "build_flags": [], "changed_paths": [],
                "selected_families": [], "selected_tests": [], "selected_build_targets": [],
                "baseline_tests": [], "family_coverage": {}, "secondary_proofs": [],
                "planned_suite": "bounded", "selection_tier": "affected",
                "authoritative_suite": "full",
                "outcomes": {"planner": "planned", "authoritative_execution": "x"},
                "elapsed_ms": 0
            });
            if let Some(planned) = planned {
                value["planned_base_sha"] = planned.into();
            }
            if let Some(recorded) = recorded {
                value["recorded_base_policy_digest"] = recorded.into();
            }
            serde_json::from_value::<crate::changed_surface::SelectionReceipt>(value)
                .expect("receipt")
        };
        let b = "b".repeat(40);
        let same = "1".repeat(64);
        let other = "2".repeat(64);
        // The control: a merge base whose policy differs refuses promotion.
        assert!(
            merge_base_promotion_refusal(
                MachineMode::Authoritative,
                &receipt(Some(&b), Some(&other), &same)
            )
            .is_some()
        );
        assert!(
            merge_base_promotion_refusal(
                MachineMode::Authoritative,
                &receipt(Some(&b), None, &same)
            )
            .is_some()
        );
        assert!(
            merge_base_promotion_refusal(
                MachineMode::Authoritative,
                &receipt(Some(&b), Some(&same), &same)
            )
            .is_none()
        );
        // Shadow comparison still runs, and an up-to-date plan is unaffected.
        assert!(
            merge_base_promotion_refusal(
                MachineMode::ShadowCompare,
                &receipt(Some(&b), Some(&other), &same)
            )
            .is_none()
        );
        assert!(
            merge_base_promotion_refusal(MachineMode::Authoritative, &receipt(None, None, &same))
                .is_none()
        );
    }

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
    fn stale_generation_restart_refuses_duplicate_execution_evidence() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!stale_generation_has_execution_evidence(temp.path()));
        fs::write(temp.path().join("stale-base-shadow-a.json"), b"{}").unwrap();
        assert!(!stale_generation_has_execution_evidence(temp.path()));
        fs::write(
            temp.path().join("stale-activation-shadow_compare.json"),
            b"{}",
        )
        .unwrap();
        assert!(stale_generation_has_execution_evidence(temp.path()));
    }

    #[test]
    fn stale_generation_publication_refuses_observer_regression() {
        let temp = tempfile::tempdir().unwrap();
        let newer = CurrentStaleGeneration {
            schema_version: 1,
            repository: "owner/repo",
            pull_request: 7,
            target: "mac",
            head_sha: "a",
            live_base_sha: "newer",
            context_digest: "newer-context",
            stale_receipt_sha256: "receipt",
        };
        let older = CurrentStaleGeneration {
            schema_version: 1,
            repository: "owner/repo",
            pull_request: 7,
            target: "mac",
            head_sha: "a",
            live_base_sha: "older",
            context_digest: "older-context",
            stale_receipt_sha256: "receipt",
        };
        let observed_empty = read_current_stale_generation(temp.path()).unwrap();
        publish_current_stale_generation(temp.path(), observed_empty.as_deref(), &newer).unwrap();

        let error =
            publish_current_stale_generation(temp.path(), observed_empty.as_deref(), &older)
                .unwrap_err();
        assert!(error.message.contains("refusing to regress"));
        let current = fs::read(temp.path().join("stale-current.json")).unwrap();
        assert!(
            String::from_utf8(current)
                .unwrap()
                .contains("newer-context")
        );
    }

    #[test]
    fn current_stale_generation_refuses_oversized_pointer() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("stale-current.json"),
            vec![b'x'; usize::try_from(MAX_STALE_POINTER_BYTES + 1).unwrap()],
        )
        .unwrap();
        let error = read_current_stale_generation(temp.path()).unwrap_err();
        assert!(error.message.contains("bounded regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn current_stale_generation_refuses_symlink_pointer() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.json");
        fs::write(&target, b"{}\n").unwrap();
        symlink(&target, temp.path().join("stale-current.json")).unwrap();
        let error = read_current_stale_generation(temp.path()).unwrap_err();
        assert!(error.message.contains("bounded regular file"));
    }

    #[test]
    fn concurrent_stale_generation_publishers_have_one_winner() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(2));
        let mut publishers = Vec::new();
        for (live_base_sha, context_digest) in
            [("base-one", "context-one"), ("base-two", "context-two")]
        {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            publishers.push(thread::spawn(move || {
                let expected = read_current_stale_generation(&root).unwrap();
                barrier.wait();
                let generation = CurrentStaleGeneration {
                    schema_version: 1,
                    repository: "owner/repo",
                    pull_request: 7,
                    target: "mac",
                    head_sha: "head",
                    live_base_sha,
                    context_digest,
                    stale_receipt_sha256: "receipt",
                };
                publish_current_stale_generation(&root, expected.as_deref(), &generation)
            }));
        }

        let outcomes = publishers
            .into_iter()
            .map(|publisher| publisher.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert_eq!(
            outcomes.iter().filter(|outcome| outcome.is_err()).count(),
            1
        );
        let current = fs::read_to_string(root.join("stale-current.json")).unwrap();
        assert!(current.contains("context-one") || current.contains("context-two"));
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

    #[test]
    fn only_a_shadow_host_keys_a_full_or_bounded_non_stale_plan() {
        use super::keys_this_plan;
        use crate::changed_surface::{ExecutionDisposition, FallbackReason, FullExecutionReason};
        let full = ExecutionDisposition::Full {
            reason: FullExecutionReason::PlannerSelectedFull,
        };
        let blocked = ExecutionDisposition::Blocked {
            reason: "waiting".to_owned(),
        };
        assert!(keys_this_plan(MachineMode::ShadowCompare, None, &full));
        assert!(keys_this_plan(
            MachineMode::ShadowCompare,
            Some(&FallbackReason::BaseRefNotProtected),
            &full
        ));
        assert!(!keys_this_plan(MachineMode::Authoritative, None, &full));
        assert!(!keys_this_plan(MachineMode::Off, None, &full));
        assert!(!keys_this_plan(MachineMode::ShadowCompare, None, &blocked));
        assert!(!keys_this_plan(
            MachineMode::ShadowCompare,
            Some(&FallbackReason::StaleBase),
            &full
        ));
    }

    fn activation_json(disposition: &str, payload_digest: &str) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 2,
            "machine_mode": "shadow_compare",
            "plan": {
                "disposition": disposition,
                "base_sha": "b".repeat(40),
                "policy_digest": "p".repeat(64),
                "selection_receipt_digest": "s".repeat(64),
                "selected_tests_digest": "",
                "execution_payload_digest": payload_digest,
            }
        }))
        .expect("activation");
        bytes.push(b'\n');
        bytes
    }

    fn keyed_payload(candidates: &[&str], seed: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"executable_reuse": {
            "candidates": candidates.iter().map(|run| serde_json::json!({"run_id": run})).collect::<Vec<_>>(),
            "sample_seed": seed,
            "rules_digest": "r",
            "derivation_code_sha256": "d",
        }}))
        .expect("payload")
    }

    fn context_for(payload: &[u8]) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "execution_payload_b64": base64::Engine::encode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                payload,
            )
        }))
        .expect("context")
    }

    #[test]
    fn a_head_reshipped_after_a_new_record_conflicts_on_its_candidates() {
        use super::activation_conflict;
        let first = keyed_payload(&["r1"], "seed1");
        let second = keyed_payload(&["r2", "r1"], "seed2");
        let conflict = activation_conflict(
            &activation_json("keyed_full_shadow", "aaaa"),
            &activation_json("keyed_full_shadow", "bbbb"),
            Some(&context_for(&first)),
            &second,
        )
        .expect("a conflict");
        assert_eq!(conflict.existing_payload_sha256.as_deref(), Some("aaaa"));
        assert_eq!(conflict.planned_payload_sha256.as_deref(), Some("bbbb"));
        assert_eq!(
            conflict.differing_fields,
            [
                "execution_payload_digest",
                "executable_reuse.candidates",
                "executable_reuse.sample_seed"
            ]
        );
    }

    #[test]
    fn a_head_first_shipped_unkeyed_conflicts_once_a_base_exists() {
        use super::activation_conflict;
        let conflict = activation_conflict(
            &activation_json("bounded", "aaaa"),
            &activation_json("keyed_bounded_shadow", "bbbb"),
            None,
            &keyed_payload(&["r1"], "seed"),
        )
        .expect("a conflict");
        assert_eq!(
            conflict.differing_fields,
            [
                "disposition",
                "execution_payload_digest",
                "executable_reuse"
            ]
        );
    }

    #[test]
    fn identical_or_wholly_unkeyed_activations_are_not_a_keyed_conflict() {
        use super::activation_conflict;
        let same = activation_json("keyed_full_shadow", "aaaa");
        assert!(activation_conflict(&same, &same, None, b"{}").is_none());
        assert!(
            activation_conflict(
                &activation_json("bounded", "aaaa"),
                &activation_json("bounded", "bbbb"),
                None,
                b"{}"
            )
            .is_none(),
            "an unkeyed plan that changed is still refused"
        );
    }
}
