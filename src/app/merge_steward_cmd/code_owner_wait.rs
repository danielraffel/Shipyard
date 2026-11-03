//! Hold an admissible head that still waits on a code-owner review, before
//! any queue mutation.
//!
//! A base whose ruleset requires a code-owner review refuses an enqueue (or
//! an exact-head merge) until one of the changed files' owners approves.
//! Asking anyway turns a predictable refusal into a mutation record; before
//! refusals were settled automatically it left an uncertain mutation that
//! blocked the pull request until an operator resolved it (vellum #141).
//!
//! So, after every other admission check passes, the steward judges the head
//! with [`crate::code_owners`] (`CODEOWNERS` at the base, reviews on the exact
//! head; never `reviewDecision`, which is `null` on a base that requires zero
//! approvals whether or not the owner approved). A head that waits is
//! reported `waiting_code_owner:<owners>`, a waiting state rather than an
//! error, with no mutation, and its owners are told once per head.
//!
//! Only a definite "missing" holds the head. A read failure or a verdict
//! only GitHub can make falls through to the mutation, where GitHub decides
//! and a refusal is recorded `rejected` with GitHub's reason.

use chrono::Utc;

use super::{MutationApplyContext, ObservedPr, StewardLedger, record_audit, save_ledger};
use crate::code_owners::CodeOwnerStatus;

/// `blocked_on` / mutation prefix for a head waiting on a code-owner review.
pub(super) const WAITING_CODE_OWNER_PREFIX: &str = "waiting_code_owner:";
/// Mutation part when the once-per-head notice could not be posted.
pub(super) const NOTICE_FAILED: &str = "code_owner_notice_failed";

/// The token naming the owners a head waits on, `+`-joined (the steward
/// joins mutation parts with commas).
#[must_use]
pub(super) fn token(owners: &[String]) -> String {
    format!("{WAITING_CODE_OWNER_PREFIX}{}", owners.join("+"))
}

/// Whether one mutation-report part is a code-owner wait.
#[must_use]
pub(super) fn is_wait_token(part: &str) -> bool {
    part.starts_with(WAITING_CODE_OWNER_PREFIX)
}

/// The code-owner verdict on `pr`'s current head, read-only.
pub(super) fn read_status(
    actions: &crate::cloud::GitHubActions,
    repo: &str,
    base: &str,
    pr: &ObservedPr,
) -> Result<CodeOwnerStatus, String> {
    let run_gh = |argv: &[String]| actions.run_gh(argv).map_err(|error| error.to_string());
    crate::code_owners::read_status(
        &run_gh,
        repo,
        base,
        pr.fact.number,
        &pr.fact.head_sha,
        pr.fact.author.as_deref(),
    )
}

/// The mutation report for a head that must not be admitted yet because it
/// waits on a code-owner review; `None` lets admission continue.
pub(super) fn admission_wait(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    ledger: &mut StewardLedger,
) -> Option<String> {
    let repo = context.observation.repo.as_str();
    let subject = format!("pr:{}:{}", pr.fact.number, pr.fact.head_sha);
    let status = match read_status(context.actions, repo, &context.observation.base, pr) {
        Ok(status) => status,
        Err(error) => {
            record_audit(
                ledger,
                repo,
                &subject,
                &format!("code_owner_check_unread:{}", compact(&error)),
            );
            return None;
        }
    };
    let CodeOwnerStatus::Missing { owners, files } = status else {
        return None;
    };
    let token = token(&owners);
    let mut report = token.clone();
    if let Err(error) = notify_once(context, pr, &owners, &files, ledger) {
        record_audit(
            ledger,
            repo,
            &subject,
            &format!("{NOTICE_FAILED}:{}", compact(&error)),
        );
        report = format!("{report},{NOTICE_FAILED}");
    }
    Some(report)
}

/// Post one comment per head naming the owners it waits on. Any existing
/// record for this head, even an unfinished one, means it was (or may have
/// been) posted: the notice is at most once.
fn notify_once(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    owners: &[String],
    files: &[String],
    ledger: &mut StewardLedger,
) -> Result<(), String> {
    let repo = context.observation.repo.as_str();
    let key = format!(
        "{repo}:{}:{}:waiting_code_owner",
        pr.fact.number, pr.fact.head_sha
    );
    if ledger.wake_records.contains_key(&key) {
        return Ok(());
    }
    let now = Utc::now().to_rfc3339();
    ledger
        .wake_records
        .insert(key.clone(), format!("intent|{}|{now}", owners.join("+")));
    save_ledger(context.ledger_path, ledger).map_err(|error| error.message().to_owned())?;
    record_audit(
        ledger,
        repo,
        &format!("pr:{}:{}", pr.fact.number, pr.fact.head_sha),
        &token(owners),
    );
    if let Err(error) = super::handoff::run_steward_write(
        context.actions,
        &[
            "api".to_owned(),
            "-X".to_owned(),
            "POST".to_owned(),
            format!("repos/{repo}/issues/{}/comments", pr.fact.number),
            "-f".to_owned(),
            format!("body={}", notice_body(pr, owners, files)),
        ],
    ) {
        // GitHub answered with an error, so nothing was posted: drop the
        // intent so the next pass tries again.
        ledger.wake_records.remove(&key);
        let _ = save_ledger(context.ledger_path, ledger);
        return Err(format!("could not post the code-owner notice: {error}"));
    }
    ledger
        .wake_records
        .insert(key, format!("posted|{}|{now}", owners.join("+")));
    save_ledger(context.ledger_path, ledger).map_err(|error| error.message().to_owned())
}

fn notice_body(pr: &ObservedPr, owners: &[String], files: &[String]) -> String {
    let mentions = owners
        .iter()
        .map(|owner| format!("@{owner}"))
        .collect::<Vec<_>>()
        .join(", ");
    let listed = files
        .iter()
        .map(|file| format!("`{file}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Shipyard steward: head `{head}` passed every admission check and waits only on a \
         code-owner review from {mentions} (CODEOWNERS on the base owns {listed}). Approve \
         this exact head and the steward admits it on its next pass; nothing else is needed. \
         This notice is posted once per head.",
        head = pr.fact.head_sha,
    )
}

fn compact(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(',', ";")
        .chars()
        .take(160)
        .collect()
}
