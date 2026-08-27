use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::super::CliFailure;
use super::super::changed_surface_cmd::{ChangedSurfacePlanArgs, observe_changed_surface_plan};
use crate::changed_surface::ObservationStatus;
use crate::config::LoadedConfig;
use crate::executor::dispatch::ResolvedTarget;
use crate::metadata_authority::{
    MetadataAuthorityDecision, MetadataAuthorityObservation, MetadataAuthorityReceipt,
    authorize_metadata_only, parse_hosted_checks, trusted_policy,
};

#[derive(Debug, Serialize)]
struct MetadataAuthorityFallback<'a> {
    schema_version: u32,
    repository: &'a str,
    pull_request: u64,
    head_sha: &'a str,
    reason: &'a str,
}

pub(super) fn observe_and_authorize(
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    repository: &str,
    pull_request: u64,
    submitted_head_sha: &str,
    targets: &[ResolvedTarget],
) -> Result<Option<MetadataAuthorityReceipt>, CliFailure> {
    let policy = match trusted_policy(config, repository) {
        Ok(Some(policy)) => policy,
        Ok(None) => return Ok(None),
        Err(error) => {
            persist_fallback(
                state_dir,
                repository,
                pull_request,
                submitted_head_sha,
                &format!("trusted metadata authority policy is invalid: {error}"),
            )?;
            return Ok(None);
        }
    };
    let Some(target) = targets.first() else {
        return Err(CliFailure::new(
            2,
            "metadata authority requires at least one ordinarily resolved target",
        ));
    };
    let observed = match observe_changed_surface_plan(
        &ChangedSurfacePlanArgs {
            target: target.name.clone(),
            pr: pull_request,
            repo: Some(repository.to_owned()),
        },
        config,
        cwd,
        state_dir,
    ) {
        Ok(observed) => observed,
        Err(error) => {
            persist_fallback(
                state_dir,
                repository,
                pull_request,
                submitted_head_sha,
                &format!(
                    "exact metadata observation unavailable: {}",
                    error.message()
                ),
            )?;
            return Ok(None);
        }
    };
    let (live_head, _) =
        match crate::reconcile::fetch_head_and_provenanced_status_check_rollup_with_config(
            config,
            cwd,
            repository,
            pull_request,
        ) {
            Ok(observed) => observed,
            Err(error) => {
                persist_fallback(
                    state_dir,
                    repository,
                    pull_request,
                    submitted_head_sha,
                    &format!("hosted check observation unavailable: {error}"),
                )?;
                return Ok(None);
            }
        };
    let stable = match observe_changed_surface_plan(
        &ChangedSurfacePlanArgs {
            target: target.name.clone(),
            pr: pull_request,
            repo: Some(repository.to_owned()),
        },
        config,
        cwd,
        state_dir,
    ) {
        Ok(stable)
            if stable.input.pr_base_sha == observed.input.pr_base_sha
                && stable.input.protected_ref_sha == observed.input.protected_ref_sha =>
        {
            stable
        }
        Ok(_) => {
            persist_fallback(
                state_dir,
                repository,
                pull_request,
                submitted_head_sha,
                "protected base drifted while hosted checks were observed",
            )?;
            return Ok(None);
        }
        Err(error) => {
            persist_fallback(
                state_dir,
                repository,
                pull_request,
                submitted_head_sha,
                &format!(
                    "stable metadata observation unavailable: {}",
                    error.message()
                ),
            )?;
            return Ok(None);
        }
    };
    let (final_head, rollup) =
        match crate::reconcile::fetch_head_and_provenanced_status_check_rollup_with_config(
            config,
            cwd,
            repository,
            pull_request,
        ) {
            Ok(observed) => observed,
            Err(error) => {
                persist_fallback(
                    state_dir,
                    repository,
                    pull_request,
                    submitted_head_sha,
                    &format!("final hosted check observation unavailable: {error}"),
                )?;
                return Ok(None);
            }
        };
    let sealed = match observe_changed_surface_plan(
        &ChangedSurfacePlanArgs {
            target: target.name.clone(),
            pr: pull_request,
            repo: Some(repository.to_owned()),
        },
        config,
        cwd,
        state_dir,
    ) {
        Ok(sealed) => sealed,
        Err(error) => {
            persist_fallback(
                state_dir,
                repository,
                pull_request,
                submitted_head_sha,
                &format!(
                    "sealed metadata observation unavailable: {}",
                    error.message()
                ),
            )?;
            return Ok(None);
        }
    };
    if final_head != live_head
        || sealed.input.pr_head_sha != final_head
        || sealed.input.pr_base_sha != stable.input.pr_base_sha
        || sealed.input.protected_ref_sha != stable.input.protected_ref_sha
    {
        persist_fallback(
            state_dir,
            repository,
            pull_request,
            submitted_head_sha,
            "pull-request head drifted while metadata authority was observed",
        )?;
        return Ok(None);
    }
    let input = &sealed.input;
    let observation = MetadataAuthorityObservation {
        repository: input.repository.clone(),
        pull_request: input.pull_request,
        base_ref: input.base_ref.clone(),
        base_sha: input.pr_base_sha.clone(),
        protected_ref_sha: input.protected_ref_sha.clone(),
        protected_ref_protected: input.protected_ref_status
            == crate::changed_surface::ProtectedRefStatus::Protected,
        remote_head_sha: live_head,
        remote_tree_sha: input.remote_tree_sha.clone(),
        local_head_sha: input.local_head_sha.clone(),
        local_tree_sha: input.local_tree_sha.clone(),
        local_merge_base_sha: input.local_merge_base_sha.clone(),
        remote_merge_base_sha: input.remote_merge_base_sha.clone(),
        merge_base_is_ancestor: input.merge_base_is_ancestor,
        checkout_clean: input.checkout_clean,
        remote_changed_paths: input.remote_changed_paths.clone(),
        remote_changed_paths_complete: input.remote_changed_paths_status
            == ObservationStatus::Complete,
        local_changed_paths: input.local_changed_paths.clone(),
        local_changed_paths_complete: input.local_changed_paths_status
            == ObservationStatus::Complete,
        hosted_checks: parse_hosted_checks(&rollup),
    };
    match authorize_metadata_only(&policy, &observation, &target.name) {
        MetadataAuthorityDecision::Authorized(receipt) => {
            persist_json(&receipt_path(state_dir, &receipt), &receipt)?;
            Ok(Some(receipt))
        }
        MetadataAuthorityDecision::Full { reason } => {
            persist_fallback(
                state_dir,
                repository,
                pull_request,
                &observation.remote_head_sha,
                &reason,
            )?;
            Ok(None)
        }
    }
}

fn persist_fallback(
    state_dir: &Path,
    repository: &str,
    pull_request: u64,
    head_sha: &str,
    reason: &str,
) -> Result<(), CliFailure> {
    persist_json(
        &fallback_path(state_dir, repository, pull_request, head_sha, reason),
        &MetadataAuthorityFallback {
            schema_version: 1,
            repository,
            pull_request,
            head_sha,
            reason,
        },
    )
}

fn receipt_path(state_dir: &Path, receipt: &MetadataAuthorityReceipt) -> PathBuf {
    authority_dir(
        state_dir,
        &receipt.repository,
        receipt.pull_request,
        &receipt.head_sha,
    )
    .join("receipt.json")
}

fn fallback_path(
    state_dir: &Path,
    repository: &str,
    pull_request: u64,
    head_sha: &str,
    reason: &str,
) -> PathBuf {
    let digest = format!("{:x}", Sha256::digest(reason.as_bytes()));
    authority_dir(state_dir, repository, pull_request, head_sha)
        .join(format!("fallback-{digest}.json"))
}

fn authority_dir(state_dir: &Path, repository: &str, pull_request: u64, head_sha: &str) -> PathBuf {
    state_dir
        .join("metadata-authority")
        .join(repository.to_ascii_lowercase().replace('/', "--"))
        .join(format!("pr-{pull_request}"))
        .join(head_sha)
}

fn persist_json(path: &Path, value: &impl Serialize) -> Result<(), CliFailure> {
    let parent = path
        .parent()
        .ok_or_else(|| CliFailure::new(1, "metadata receipt path has no parent"))?;
    crate::writer_domain_lease::ensure_protected_dir_all(parent)
        .map_err(|error| CliFailure::new(1, format!("create metadata receipt dir: {error}")))?;
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| CliFailure::new(1, format!("serialize metadata receipt: {error}")))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temporary = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| CliFailure::new(1, format!("create metadata receipt: {error}")))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| CliFailure::new(1, format!("write metadata receipt: {error}")))?;
    drop(file);
    let published = match fs::hard_link(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(path)
                .map_err(|error| CliFailure::new(1, format!("read metadata receipt: {error}")))?;
            if existing == bytes {
                Ok(())
            } else {
                Err(CliFailure::new(
                    1,
                    format!(
                        "refuse to replace immutable metadata receipt {}",
                        path.display()
                    ),
                ))
            }
        }
        Err(error) => Err(CliFailure::new(
            1,
            format!("publish metadata receipt: {error}"),
        )),
    };
    let _ = fs::remove_file(&temporary);
    published?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| CliFailure::new(1, format!("sync metadata receipt dir: {error}")))
}
