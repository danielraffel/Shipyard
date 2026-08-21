use std::path::Path;
use std::time::{Duration, Instant};

use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::recovery_worker::{
    RecoveryOutput, RecoveryRecord, RecoveryRequest, RecoveryStatus, RecoveryStore,
};

use super::terminal::ClaimedRecovery;
use super::{
    ClaimPolicyRefresh, CliFailure, GlobalModelLease, MAX_OUTPUT_WORDS, MAX_RECEIPT_DETAIL_BYTES,
    PREFLIGHT_BUDGET_SECONDS, RecoveryEnqueueLease, RecoveryWorkerPolicy, RecoveryWorkerReport,
    RequestDisposition, TERMINAL_PERSIST_TIMEOUT_SECONDS, acquire_recovery_enqueue_lease,
    acquire_recovery_enqueue_read_lease, bounded_detail, inspect_request, recovery_prompt, report,
    run_worker_process, verify_recovery_witness, worker_generation,
};

struct RecordContext<'a> {
    store: RecoveryStore,
    record: &'a RecoveryRecord,
    apply: bool,
    startup_policy: &'a RecoveryWorkerPolicy,
    startup_policy_signature: &'a str,
    trusted_config: &'a LoadedConfig,
    model_lease: Option<&'a GlobalModelLease>,
    state_dir: &'a Path,
    scratch_dir: &'a Path,
    deadline: Instant,
}

struct ReadyToClaim {
    policy_signature: String,
    config: LoadedConfig,
    worker_generation: String,
}

struct ValidatedModelOutput {
    output: RecoveryOutput,
    actions: GitHubActions,
    config: LoadedConfig,
    policy_signature: String,
}

enum PreClaimOutcome {
    Report(RecoveryWorkerReport),
    Ready(ReadyToClaim),
}

#[derive(Clone, Copy)]
pub(super) struct ProcessRecordInputs<'a> {
    pub(super) store: &'a RecoveryStore,
    pub(super) record: &'a RecoveryRecord,
    pub(super) apply: bool,
    pub(super) policy: &'a RecoveryWorkerPolicy,
    pub(super) policy_signature: &'a str,
    pub(super) trusted_config: &'a LoadedConfig,
    pub(super) model_lease: Option<&'a GlobalModelLease>,
    pub(super) state_dir: &'a Path,
    pub(super) scratch_dir: &'a Path,
}

pub(super) fn process_record(
    inputs: ProcessRecordInputs<'_>,
) -> Result<RecoveryWorkerReport, CliFailure> {
    let deadline = Instant::now()
        + Duration::from_secs(
            inputs
                .policy
                .timeout_seconds
                .saturating_add(PREFLIGHT_BUDGET_SECONDS),
        );
    let context = RecordContext {
        store: inputs.store.clone().with_lock_deadline(deadline),
        record: inputs.record,
        apply: inputs.apply,
        startup_policy: inputs.policy,
        startup_policy_signature: inputs.policy_signature,
        trusted_config: inputs.trusted_config,
        model_lease: inputs.model_lease,
        state_dir: inputs.state_dir,
        scratch_dir: inputs.scratch_dir,
        deadline,
    };
    let prepared = match prepare_record(&context) {
        Ok(prepared) => prepared,
        Err(error) if context.apply => return Err(context.defer_preclaim_error(error)),
        Err(error) => return Err(error),
    };
    match prepared {
        PreClaimOutcome::Report(report) => Ok(report),
        PreClaimOutcome::Ready(ready) => process_claimed_record(&context, &ready),
    }
}

fn prepare_record(context: &RecordContext<'_>) -> Result<PreClaimOutcome, CliFailure> {
    let request = &context.record.request;
    // Enqueue publishes the record and witness under the exclusive form of
    // this lease. The shared read prevents a worker from observing the record
    // in the middle of that transaction and creates nothing during dry-run.
    let initial_enqueue_lease =
        acquire_recovery_enqueue_read_lease(context.store.root(), context.deadline)?;
    let durable_snapshot = context
        .store
        .get(&request.id)
        .map_err(|error| {
            CliFailure::new(
                1,
                format!("failed to refresh recovery request {}: {error}", request.id),
            )
        })?
        .ok_or_else(|| CliFailure::new(1, "pending recovery request disappeared"))?;
    if durable_snapshot.request.config_signature != request.config_signature
        || durable_snapshot.receipt.status != context.record.receipt.status
    {
        return Ok(PreClaimOutcome::Report(report(
            request,
            "stale_snapshot",
            "durable recovery request changed before this worker acquired its evidence lease",
        )));
    }
    if let Err(error) = verify_recovery_witness(context.state_dir, request) {
        return context.superseded(error.message(), "superseded", "would_supersede");
    }
    if request.config_signature != context.startup_policy_signature {
        return context.superseded(
            format!(
                "trusted config signature drift: request {}, current {}",
                request.config_signature, context.startup_policy_signature
            ),
            "superseded",
            "would_supersede",
        );
    }
    drop(initial_enqueue_lease);

    let repo_path = checked_repo_path(context.startup_policy, request)?;
    let actions = GitHubActions::from_loaded_config(repo_path, context.trusted_config)
        .with_repo_override(&request.repo);
    if let RequestDisposition::Superseded(reason) =
        inspect_request(&actions, request, context.deadline)?
    {
        return context.superseded(reason, "superseded", "would_supersede");
    }
    if !context.apply {
        return Ok(PreClaimOutcome::Report(report(
            request,
            "would_run",
            format!(
                "exact managed head; provider={} model={}",
                context.startup_policy.provider, context.startup_policy.first_line_model
            ),
        )));
    }

    // Any GitHub error in this final pre-claim read leaves the one allowed
    // attempt pending. A changed exact-head signal is superseded instead.
    let generation = worker_generation(context.scratch_dir)?;
    if let RequestDisposition::Superseded(reason) =
        inspect_request(&actions, request, context.deadline)?
    {
        return context.superseded(reason, "superseded_before_run", "superseded_before_run");
    }

    // Reload immediately before the durable claim. Cached startup authority
    // cannot spend an attempt after the machine policy changes.
    let refreshed = RecoveryWorkerPolicy::refresh_for_claim(
        &context.trusted_config.global_dir,
        context.startup_policy_signature,
    )?;
    let snapshot = match refreshed {
        ClaimPolicyRefresh::Current(snapshot) => *snapshot,
        ClaimPolicyRefresh::Drifted { observed_signature } => {
            return context.superseded(
                format!(
                    "trusted recovery-worker policy drifted before claim: queued {}, current {observed_signature}",
                    context.startup_policy_signature
                ),
                "superseded_policy_before_run",
                "superseded_policy_before_run",
            );
        }
    };
    checked_repo_path(&snapshot.policy, request)?;
    Ok(PreClaimOutcome::Ready(ReadyToClaim {
        policy_signature: snapshot.signature,
        config: snapshot.config,
        worker_generation: generation,
    }))
}

impl RecordContext<'_> {
    fn defer_preclaim_error(&self, error: CliFailure) -> CliFailure {
        let detail = bounded_detail(
            &format!("pre-claim validation deferred: {}", error.message()),
            MAX_RECEIPT_DETAIL_BYTES,
        );
        let store = self.store.clone().with_lock_deadline(
            Instant::now() + Duration::from_secs(TERMINAL_PERSIST_TIMEOUT_SECONDS),
        );
        match store.defer_pending(
            &self.record.request.id,
            &self.record.request.config_signature,
            detail,
        ) {
            Ok(_) => error,
            Err(defer_error) => CliFailure::new(
                1,
                format!(
                    "{}; failed to durably defer the pending request: {defer_error}",
                    error.message()
                ),
            ),
        }
    }

    fn superseded(
        &self,
        reason: impl AsRef<str>,
        applied_action: &'static str,
        dry_run_action: &'static str,
    ) -> Result<PreClaimOutcome, CliFailure> {
        let request = &self.record.request;
        let reason = bounded_detail(reason.as_ref(), MAX_RECEIPT_DETAIL_BYTES);
        if self.apply {
            self.store
                .supersede(&request.id, None, &reason)
                .map_err(|error| {
                    CliFailure::new(
                        1,
                        format!("failed to mark {} superseded: {error}", request.id),
                    )
                })?;
        }
        Ok(PreClaimOutcome::Report(report(
            request,
            if self.apply {
                applied_action
            } else {
                dry_run_action
            },
            reason,
        )))
    }
}

fn checked_repo_path<'a>(
    policy: &'a RecoveryWorkerPolicy,
    request: &RecoveryRequest,
) -> Result<&'a Path, CliFailure> {
    let repo_path = policy.repo_path(&request.repo)?;
    if !repo_path.is_dir() {
        return Err(CliFailure::new(
            1,
            format!(
                "trusted repository path for `{}` is not a directory: {}",
                request.repo,
                repo_path.display()
            ),
        ));
    }
    Ok(repo_path)
}

fn process_claimed_record(
    context: &RecordContext<'_>,
    ready: &ReadyToClaim,
) -> Result<RecoveryWorkerReport, CliFailure> {
    let request = &context.record.request;
    ClaimedRecovery::begin(
        &context.store,
        request,
        &ready.policy_signature,
        &ready.worker_generation,
    )?
    .run(|claim| {
        let validated = run_claimed_model(context, ready, claim)?;
        finalize_claimed_record(context, validated)
    })
}

fn run_claimed_model(
    context: &RecordContext<'_>,
    ready: &ReadyToClaim,
    claim: &ClaimedRecovery<'_>,
) -> Result<ValidatedModelOutput, CliFailure> {
    let request = &context.record.request;
    // A policy edit racing the claim may spend the attempt, but this refresh
    // prevents launching a disabled or reconfigured model process.
    let refreshed =
        RecoveryWorkerPolicy::refresh_for_claim(&ready.config.global_dir, &ready.policy_signature)?;
    let snapshot = match refreshed {
        ClaimPolicyRefresh::Current(snapshot) => *snapshot,
        ClaimPolicyRefresh::Drifted { observed_signature } => {
            return Err(CliFailure::new(
                1,
                format!(
                    "trusted recovery-worker policy drifted before process launch: claimed {}, current {observed_signature}",
                    ready.policy_signature
                ),
            ));
        }
    };
    debug_assert_eq!(snapshot.signature, ready.policy_signature);
    let repo_path = checked_repo_path(&snapshot.policy, request).map_err(|error| {
        CliFailure::new(
            1,
            format!("pre-launch repository policy failed: {}", error.message()),
        )
    })?;
    let actions = GitHubActions::from_loaded_config(repo_path, &snapshot.config)
        .with_repo_override(&request.repo);
    let prompt = recovery_prompt(&snapshot.policy.provider, request).map_err(|error| {
        CliFailure::new(1, format!("failed to serialize recovery prompt: {error}"))
    })?;
    let worker_deadline =
        claim.worker_deadline(context.deadline, snapshot.policy.timeout_seconds)?;
    let model_lease = context.model_lease.ok_or_else(|| {
        CliFailure::new(
            1,
            "claimed recovery request lacks the machine-global model lease",
        )
    })?;
    let output = run_worker_process(
        &snapshot.policy,
        &prompt,
        model_lease,
        context.scratch_dir,
        worker_deadline,
    )
    .map_err(|error| CliFailure::new(1, format!("worker launch failed: {}", error.message())))?;
    validate_process_output(&snapshot.policy, request, &output).map(|parsed| ValidatedModelOutput {
        output: parsed,
        actions,
        config: snapshot.config,
        policy_signature: snapshot.signature,
    })
}

fn validate_process_output(
    policy: &RecoveryWorkerPolicy,
    request: &RecoveryRequest,
    output: &super::WorkerProcessOutput,
) -> Result<RecoveryOutput, CliFailure> {
    if output.timed_out {
        return Err(CliFailure::new(
            1,
            format!("worker timed out after {} seconds", policy.timeout_seconds),
        ));
    }
    if output.exit_code != Some(0) {
        return Err(CliFailure::new(
            1,
            super::process_failure_detail(output.exit_code, &output.stderr),
        ));
    }
    if output.stdout_truncated {
        return Err(CliFailure::new(
            1,
            "worker stdout exceeded the configured byte limit",
        ));
    }
    if output
        .stdout
        .split(u8::is_ascii_whitespace)
        .filter(|word| !word.is_empty())
        .count()
        >= MAX_OUTPUT_WORDS
    {
        return Err(CliFailure::new(
            1,
            format!("worker output exceeded the {MAX_OUTPUT_WORDS}-word phase-1 limit"),
        ));
    }
    let parsed = serde_json::from_slice::<RecoveryOutput>(&output.stdout)
        .map_err(|error| CliFailure::new(1, format!("worker returned invalid JSON: {error}")))?;
    parsed.validate_for_request(request).map_err(|error| {
        CliFailure::new(
            1,
            format!("worker output failed schema validation: {error}"),
        )
    })?;
    Ok(parsed)
}

fn finalize_claimed_record(
    context: &RecordContext<'_>,
    validated: ValidatedModelOutput,
) -> Result<RecoveryWorkerReport, CliFailure> {
    let request = &context.record.request;
    if let RequestDisposition::Superseded(reason) =
        inspect_request(&validated.actions, request, context.deadline).map_err(|error| {
            CliFailure::new(
                1,
                format!("final exact-head revalidation failed: {}", error.message()),
            )
        })?
    {
        context
            .store
            .supersede(&request.id, None, &reason)
            .map_err(|error| {
                CliFailure::new(
                    1,
                    format!("failed to mark {} superseded: {error}", request.id),
                )
            })?;
        return Ok(report(request, "superseded_after_run", reason));
    }

    // Steward enqueue and completion share this lease, making the witness and
    // matching terminal receipt one serialized deterministic transition.
    let enqueue_lease = acquire_recovery_enqueue_lease(context.store.root(), context.deadline)
        .map_err(|error| {
            CliFailure::new(
                1,
                format!(
                    "failed to serialize final steward revalidation: {}",
                    error.message()
                ),
            )
        })?;
    complete_claimed_record_under_lease(context, validated, &enqueue_lease)
}

fn complete_claimed_record_under_lease(
    context: &RecordContext<'_>,
    validated: ValidatedModelOutput,
    enqueue_lease: &RecoveryEnqueueLease,
) -> Result<RecoveryWorkerReport, CliFailure> {
    let request = &context.record.request;
    if !enqueue_lease.covers(context.store.root()) {
        return Err(CliFailure::new(
            1,
            "final completion lease does not cover this recovery store",
        ));
    }
    let durable = context
        .store
        .get(&request.id)
        .map_err(|error| {
            CliFailure::new(
                1,
                format!("failed to reload claimed recovery request: {error}"),
            )
        })?
        .ok_or_else(|| CliFailure::new(1, "claimed recovery request disappeared"))?;
    if durable.receipt.status == RecoveryStatus::Superseded {
        return Ok(report(
            request,
            "superseded_during_run",
            durable
                .receipt
                .detail
                .as_deref()
                .unwrap_or("same-head deterministic authority changed during execution"),
        ));
    }
    if durable.receipt.status != RecoveryStatus::Running {
        return Err(CliFailure::new(
            1,
            format!(
                "claimed recovery request changed to unexpected {:?} state",
                durable.receipt.status
            ),
        ));
    }
    verify_recovery_witness(context.state_dir, request).map_err(|error| {
        CliFailure::new(
            1,
            format!(
                "final steward policy/evidence revalidation failed: {}",
                error.message()
            ),
        )
    })?;
    let verdict = format!("{:?}", validated.output.verdict).to_ascii_lowercase();
    // Refresh machine policy only after obtaining the same lease that fences
    // steward publication, and immediately before the terminal write. A
    // worker waiting for this lease cannot commit under its stale snapshot.
    revalidate_final_policy(&validated)?;
    context
        .store
        .complete(&request.id, &validated.policy_signature, validated.output)
        .map_err(|error| {
            CliFailure::new(
                1,
                format!("failed to persist terminal recovery receipt: {error}"),
            )
        })?;
    Ok(report(
        request,
        "completed",
        format!("validated triage verdict: {verdict}"),
    ))
}

fn revalidate_final_policy(validated: &ValidatedModelOutput) -> Result<(), CliFailure> {
    let (_, final_policy_signature, _) = RecoveryWorkerPolicy::load(&validated.config.global_dir)
        .map_err(|error| {
        CliFailure::new(
            1,
            format!(
                "final trusted-policy revalidation failed: {}",
                error.message()
            ),
        )
    })?;
    if final_policy_signature != validated.policy_signature {
        return Err(CliFailure::new(
            1,
            format!(
                "trusted recovery-worker policy drifted during execution: started {}, current {final_policy_signature}",
                validated.policy_signature
            ),
        ));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::super::recovery_test_codex_binary;
    use super::*;
    use crate::recovery_worker::{
        RECOVERY_SCHEMA_VERSION, RecoveryCategory, RecoveryConfidence, RecoveryFailureFact,
        RecoveryRequiredCheck, RecoveryVerdict,
    };

    fn policy_config(repo_path: &Path, max_log_tail_bytes: usize) -> String {
        let codex_binary =
            toml::Value::String(recovery_test_codex_binary().to_string_lossy().into_owned());
        let codex_home = toml::Value::String("/trusted/codex-home".to_owned());
        let repo_path = toml::Value::String(repo_path.to_string_lossy().into_owned());
        format!(
            r#"
[merge_steward.recovery_worker]
enabled = true
provider = "codex"
codex_binary = {codex_binary}
codex_home = {codex_home}
timeout_seconds = 30
max_attempts_per_head = 1
max_log_tail_bytes = {max_log_tail_bytes}
allowed_repositories = ["Generous-Corp/pulp"]

[merge_steward.recovery_worker.repo_paths]
"Generous-Corp/pulp" = {repo_path}
"#
        )
    }

    #[test]
    fn policy_refresh_after_completion_lease_rejects_wait_time_drift() {
        let temp = tempfile::tempdir().expect("tempdir");
        let global_dir = temp.path().join("global");
        let state_dir = temp.path().join("state");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&global_dir).expect("global config directory");
        fs::create_dir_all(&repo_path).expect("repository fixture");
        let config_path = global_dir.join("config.toml");
        fs::write(&config_path, policy_config(&repo_path, 4096)).expect("initial policy");
        let (policy, policy_signature, trusted_config) =
            RecoveryWorkerPolicy::load(&global_dir).expect("initial trusted policy");
        let request = RecoveryRequest::new(
            "Generous-Corp/pulp",
            42,
            "main",
            "0123456789abcdef0123456789abcdef01234567",
            "failure-fingerprint",
            "required check failed",
            vec![RecoveryRequiredCheck {
                context: "macos".to_owned(),
                app_id: None,
            }],
            vec![RecoveryFailureFact::RequiredCheck {
                context: "macos".to_owned(),
                app_id: None,
                conclusion: "FAILURE".to_owned(),
                run_id: None,
            }],
            "steward-policy",
            &policy_signature,
        )
        .expect("recovery request");
        let store = RecoveryStore::new(super::super::recovery_store_root(&state_dir))
            .expect("recovery store");
        store.enqueue(request.clone()).expect("enqueue request");
        store
            .begin(&request.id, &policy_signature, "worker-generation")
            .expect("claim request");
        super::super::write_recovery_witness(
            &state_dir,
            &request.repo,
            request.pr,
            &request.id,
            &request.head_sha,
            &request.policy_signature,
            &request.failure_fingerprint,
        )
        .expect("recovery witness");
        let running = store
            .get(&request.id)
            .expect("load running request")
            .expect("running request");
        let scratch_dir = temp.path().join("scratch");
        let context = RecordContext {
            store: store
                .clone()
                .with_lock_deadline(Instant::now() + Duration::from_secs(5)),
            record: &running,
            apply: true,
            startup_policy: &policy,
            startup_policy_signature: &policy_signature,
            trusted_config: &trusted_config,
            model_lease: None,
            state_dir: &state_dir,
            scratch_dir: &scratch_dir,
            deadline: Instant::now() + Duration::from_secs(5),
        };
        let validated = ValidatedModelOutput {
            output: RecoveryOutput {
                schema_version: RECOVERY_SCHEMA_VERSION,
                verdict: RecoveryVerdict::Escalate,
                category: RecoveryCategory::Unknown,
                confidence: RecoveryConfidence::Low,
                evidence: Vec::new(),
                candidate_paths: Vec::new(),
                focused_tests: Vec::new(),
            },
            actions: GitHubActions::new(&repo_path),
            config: trusted_config.clone(),
            policy_signature: policy_signature.clone(),
        };

        let completion_lease =
            acquire_recovery_enqueue_lease(store.root(), context.deadline).expect("lease");
        fs::write(&config_path, policy_config(&repo_path, 8192))
            .expect("policy drift after acquiring completion lease");
        let error = complete_claimed_record_under_lease(&context, validated, &completion_lease)
            .expect_err("post-lease policy drift must reject completion");

        assert!(
            error
                .message()
                .contains("trusted recovery-worker policy drifted during execution")
        );
        assert_eq!(
            store
                .get(&request.id)
                .expect("load rejected request")
                .expect("rejected request")
                .receipt
                .status,
            RecoveryStatus::Running
        );
    }
}
