use super::{
    BTreeSet, CliFailure, Duration, GITHUB_CALL_TIMEOUT_SECONDS, GITHUB_STDERR_LIMIT_BYTES,
    GITHUB_STDOUT_LIMIT_BYTES, GitHubActions, Instant, RecoveryRequest, RecoveryRequiredCheck,
    RequestDisposition, Value,
};
use crate::merge_steward::StewardCheck;
use crate::recovery_worker::RecoveryFailureFact;

pub(super) const STATUS_PAGE_SIZE: usize = 100;
pub(super) const MAX_STATUS_PAGES: u32 = 4;
const MAX_STATUS_WINDOW: usize = 400;
const CHECK_RUN_PAGE_SIZE: usize = 100;
const MAX_CHECK_RUN_PAGES: u32 = 4;
const MAX_CHECK_RUN_WINDOW: usize = 400;

fn bounded_gh_json(
    actions: &GitHubActions,
    args: &[String],
    purpose: &str,
    timeout: Duration,
) -> Result<Value, String> {
    let raw = actions
        .run_gh_with_timeout_bounded(
            args,
            timeout,
            GITHUB_STDOUT_LIMIT_BYTES,
            GITHUB_STDERR_LIMIT_BYTES,
        )
        .map_err(|error| format!("{purpose} failed: {error}"))?;
    serde_json::from_str(&raw)
        .map_err(|error| format!("{purpose} returned malformed JSON: {error}"))
}

pub(super) fn inspect_request(
    actions: &GitHubActions,
    request: &RecoveryRequest,
    deadline: Instant,
) -> Result<RequestDisposition, CliFailure> {
    let pull = bounded_gh_json(
        actions,
        &[
            "pr".to_owned(),
            "view".to_owned(),
            request.pr.to_string(),
            "--repo".to_owned(),
            request.repo.clone(),
            "--json".to_owned(),
            "state,isDraft,baseRefName,headRefOid,mergeStateStatus,labels,statusCheckRollup"
                .to_owned(),
        ],
        "recovery exact-target preflight",
        remaining_timeout(deadline, "recovery exact-target preflight")?
            .min(Duration::from_secs(GITHUB_CALL_TIMEOUT_SECONDS)),
    )
    .map_err(|error| CliFailure::new(1, error))?;
    match classify_pull_target(&pull, request)? {
        RequestDisposition::Current => {}
        superseded @ RequestDisposition::Superseded(_) => return Ok(superseded),
    }
    let checks_required = request
        .failure_facts
        .iter()
        .any(|fact| matches!(fact, RecoveryFailureFact::RequiredCheck { .. }));
    let (mut checks, rollup_truncated) = pull_checks(&pull)?;
    let complete_statuses = if checks_required && rollup_truncated {
        // A 100-entry GraphQL rollup is only a prefix. Replace it with both
        // complete REST check surfaces so the worker classifies the same
        // deterministic snapshot that allowed the steward to enqueue.
        checks = fetch_bounded_check_runs(actions, request, deadline)?;
        let statuses = fetch_bounded_complete_statuses(actions, request, deadline)?;
        checks.extend(parse_rest_statuses(&statuses)?);
        Some(statuses)
    } else {
        if checks_required
            && request
                .required_checks
                .iter()
                .any(|required| required.app_id.is_some())
        {
            checks.extend(fetch_bounded_check_runs(actions, request, deadline)?);
        }
        None
    };
    match classify_failure_evidence(&pull, request, &checks)? {
        RequestDisposition::Current => {}
        superseded @ RequestDisposition::Superseded(_) => return Ok(superseded),
    }
    // Phase 1 can prove the exact context/state transition but the ghapp
    // response currently omits status creator identity. This predicate is
    // sufficient only for read-only triage; it must not authorize phase-2
    // repository or GitHub mutation until issuer identity is available.
    let statuses = match complete_statuses {
        Some(statuses) => statuses,
        None => fetch_bounded_statuses(actions, request, deadline)?,
    };
    if let superseded @ RequestDisposition::Superseded(_) =
        classify_status_provenance(&statuses, request)?
    {
        return Ok(superseded);
    }
    Ok(RequestDisposition::Current)
}

fn classify_status_provenance(
    statuses: &[Value],
    request: &RecoveryRequest,
) -> Result<RequestDisposition, CliFailure> {
    if latest_status_state(statuses, super::super::HANDOFF_CONTEXT)? != Some("success") {
        return Ok(RequestDisposition::Superseded(format!(
            "exact head {} no longer has successful {} provenance",
            request.head_sha,
            super::super::HANDOFF_CONTEXT
        )));
    }
    if latest_status_state(statuses, super::super::RECOVERY_CONTEXT)? != Some("failure") {
        return Ok(RequestDisposition::Superseded(format!(
            "exact head {} no longer has an active recovery failure signal",
            request.head_sha
        )));
    }
    Ok(RequestDisposition::Current)
}

fn fetch_bounded_statuses(
    actions: &GitHubActions,
    request: &RecoveryRequest,
    deadline: Instant,
) -> Result<Vec<Value>, CliFailure> {
    let mut statuses = Vec::new();
    for page_number in 1..=MAX_STATUS_PAGES {
        let purpose = format!("recovery provenance status preflight page {page_number}");
        let payload = bounded_gh_json(
            actions,
            &[
                "api".to_owned(),
                format!(
                    "repos/{}/commits/{}/statuses?per_page={STATUS_PAGE_SIZE}&page={page_number}",
                    request.repo, request.head_sha
                ),
            ],
            &purpose,
            remaining_timeout(deadline, &purpose)?
                .min(Duration::from_secs(GITHUB_CALL_TIMEOUT_SECONDS)),
        )
        .map_err(|error| CliFailure::new(1, error))?;
        let page = payload
            .as_array()
            .ok_or_else(|| CliFailure::new(1, "commit statuses response was not an array"))?;
        if append_status_page(&mut statuses, page, page_number)? {
            return Ok(statuses);
        }
    }
    unreachable!("the final bounded status page always returns or errors")
}

fn fetch_bounded_complete_statuses(
    actions: &GitHubActions,
    request: &RecoveryRequest,
    deadline: Instant,
) -> Result<Vec<Value>, CliFailure> {
    let mut statuses = Vec::new();
    for page_number in 1..=MAX_STATUS_PAGES {
        let purpose = format!("recovery complete status preflight page {page_number}");
        let payload = bounded_gh_json(
            actions,
            &[
                "api".to_owned(),
                format!(
                    "repos/{}/commits/{}/statuses?per_page={STATUS_PAGE_SIZE}&page={page_number}",
                    request.repo, request.head_sha
                ),
            ],
            &purpose,
            remaining_timeout(deadline, &purpose)?
                .min(Duration::from_secs(GITHUB_CALL_TIMEOUT_SECONDS)),
        )
        .map_err(|error| CliFailure::new(1, error))?;
        let page = payload
            .as_array()
            .ok_or_else(|| CliFailure::new(1, "commit statuses response was not an array"))?;
        if append_complete_status_page(&mut statuses, page, page_number)? {
            return Ok(statuses);
        }
    }
    unreachable!("the final bounded status page always returns or errors")
}

fn parse_rest_statuses(statuses: &[Value]) -> Result<Vec<StewardCheck>, CliFailure> {
    statuses
        .iter()
        .map(|value| {
            super::super::observation::parse_rest_status(value).ok_or_else(|| {
                CliFailure::new(1, "complete status response contained a malformed status")
            })
        })
        .collect()
}

pub(super) fn append_status_page(
    statuses: &mut Vec<Value>,
    page: &[Value],
    page_number: u32,
) -> Result<bool, CliFailure> {
    validate_status_page(page, page_number)?;
    statuses.extend_from_slice(page);
    let has_context = |expected| {
        statuses
            .iter()
            .any(|status| status.get("context").and_then(Value::as_str) == Some(expected))
    };
    if has_context(super::super::HANDOFF_CONTEXT) && has_context(super::super::RECOVERY_CONTEXT) {
        return Ok(true);
    }
    if page.len() < STATUS_PAGE_SIZE {
        return Ok(true);
    }
    if page_number == MAX_STATUS_PAGES {
        return Err(CliFailure::new(
            1,
            format!(
                "required steward statuses were not found within the bounded {MAX_STATUS_WINDOW}-status window"
            ),
        ));
    }
    Ok(false)
}

fn append_complete_status_page(
    statuses: &mut Vec<Value>,
    page: &[Value],
    page_number: u32,
) -> Result<bool, CliFailure> {
    validate_status_page(page, page_number)?;
    statuses.extend_from_slice(page);
    if page.len() < STATUS_PAGE_SIZE {
        return Ok(true);
    }
    if page_number == MAX_STATUS_PAGES {
        return Err(CliFailure::new(
            1,
            format!(
                "current-head commit statuses exceed the bounded {MAX_STATUS_WINDOW}-entry identity window"
            ),
        ));
    }
    Ok(false)
}

fn validate_status_page(page: &[Value], page_number: u32) -> Result<(), CliFailure> {
    if page_number == 0 || page_number > MAX_STATUS_PAGES || page.len() > STATUS_PAGE_SIZE {
        return Err(CliFailure::new(
            1,
            "commit status page violated the bounded pagination contract",
        ));
    }
    for status in page {
        status
            .get("context")
            .and_then(Value::as_str)
            .ok_or_else(|| CliFailure::new(1, "commit status omitted string context"))?;
    }
    Ok(())
}

fn fetch_bounded_check_runs(
    actions: &GitHubActions,
    request: &RecoveryRequest,
    deadline: Instant,
) -> Result<Vec<StewardCheck>, CliFailure> {
    let mut checks = Vec::new();
    for page_number in 1..=MAX_CHECK_RUN_PAGES {
        let purpose = format!("recovery check identity preflight page {page_number}");
        let payload = bounded_gh_json(
            actions,
            &[
                "api".to_owned(),
                format!(
                    "repos/{}/commits/{}/check-runs?per_page={CHECK_RUN_PAGE_SIZE}&page={page_number}",
                    request.repo, request.head_sha
                ),
            ],
            &purpose,
            remaining_timeout(deadline, &purpose)?
                .min(Duration::from_secs(GITHUB_CALL_TIMEOUT_SECONDS)),
        )
        .map_err(|error| CliFailure::new(1, error))?;
        let page = payload
            .get("check_runs")
            .and_then(Value::as_array)
            .ok_or_else(|| CliFailure::new(1, "check identity response omitted check_runs"))?;
        if append_check_run_page(&mut checks, page, page_number)? {
            return Ok(checks);
        }
    }
    unreachable!("the final bounded check-run page always returns or errors")
}

fn append_check_run_page(
    checks: &mut Vec<StewardCheck>,
    page: &[Value],
    page_number: u32,
) -> Result<bool, CliFailure> {
    if page_number == 0 || page_number > MAX_CHECK_RUN_PAGES || page.len() > CHECK_RUN_PAGE_SIZE {
        return Err(CliFailure::new(
            1,
            "check-run page violated the bounded pagination contract",
        ));
    }
    checks.extend(
        page.iter()
            .map(|value| {
                super::super::observation::parse_rest_check(value).ok_or_else(|| {
                    CliFailure::new(1, "check identity response contained a malformed check run")
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
    );
    if page.len() < CHECK_RUN_PAGE_SIZE {
        return Ok(true);
    }
    if page_number == MAX_CHECK_RUN_PAGES {
        return Err(CliFailure::new(
            1,
            format!(
                "current-head check runs exceed the bounded {MAX_CHECK_RUN_WINDOW}-entry identity window"
            ),
        ));
    }
    Ok(false)
}

fn classify_pull_target(
    pull: &Value,
    request: &RecoveryRequest,
) -> Result<RequestDisposition, CliFailure> {
    let state = pull
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| CliFailure::new(1, "pull-request response omitted string state"))?;
    match state {
        "OPEN" => {}
        "CLOSED" | "MERGED" => {
            return Ok(RequestDisposition::Superseded(format!(
                "PR #{} is no longer open",
                request.pr
            )));
        }
        other => {
            return Err(CliFailure::new(
                1,
                format!("pull-request response returned unknown state `{other}`"),
            ));
        }
    }
    let is_draft = pull
        .get("isDraft")
        .and_then(Value::as_bool)
        .ok_or_else(|| CliFailure::new(1, "pull-request response omitted boolean isDraft"))?;
    if is_draft {
        return Ok(RequestDisposition::Superseded(format!(
            "PR #{} became a draft",
            request.pr
        )));
    }
    let current_base = pull
        .get("baseRefName")
        .and_then(Value::as_str)
        .filter(|base| !base.is_empty())
        .ok_or_else(|| CliFailure::new(1, "pull-request response omitted target base"))?;
    if current_base != request.base_ref {
        return Ok(RequestDisposition::Superseded(format!(
            "PR #{} target changed from {} to {}",
            request.pr, request.base_ref, current_base
        )));
    }
    let current_head = pull
        .get("headRefOid")
        .and_then(Value::as_str)
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| {
            CliFailure::new(
                1,
                "pull-request response omitted a full hexadecimal head SHA",
            )
        })?;
    if !current_head.eq_ignore_ascii_case(&request.head_sha) {
        return Ok(RequestDisposition::Superseded(format!(
            "PR #{} head changed from {} to {}",
            request.pr, request.head_sha, current_head
        )));
    }
    let labels = pull_labels(pull)?;
    if labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(&request.opt_out_label))
    {
        return Ok(RequestDisposition::Superseded(format!(
            "PR #{} now carries configured opt-out label {}",
            request.pr, request.opt_out_label
        )));
    }
    if !labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(super::super::MANAGED_LABEL))
    {
        return Ok(RequestDisposition::Superseded(format!(
            "PR #{} no longer carries required provenance label {}",
            request.pr,
            super::super::MANAGED_LABEL
        )));
    }
    if !labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(super::super::NEEDS_AGENT_LABEL))
    {
        return Ok(RequestDisposition::Superseded(format!(
            "PR #{} no longer carries {}",
            request.pr,
            super::super::NEEDS_AGENT_LABEL
        )));
    }
    Ok(RequestDisposition::Current)
}

fn pull_labels(pull: &Value) -> Result<BTreeSet<&str>, CliFailure> {
    pull.get("labels")
        .and_then(Value::as_array)
        .ok_or_else(|| CliFailure::new(1, "pull-request response omitted labels"))?
        .iter()
        .map(|label| {
            label
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| CliFailure::new(1, "pull-request label omitted string name"))
        })
        .collect()
}

fn classify_failure_evidence(
    pull: &Value,
    request: &RecoveryRequest,
    checks: &[StewardCheck],
) -> Result<RequestDisposition, CliFailure> {
    let merge_state = pull
        .get("mergeStateStatus")
        .and_then(Value::as_str)
        .filter(|state| !state.is_empty())
        .ok_or_else(|| CliFailure::new(1, "pull-request response omitted merge state"))?;
    if request
        .failure_facts
        .iter()
        .all(|fact| matches!(fact, RecoveryFailureFact::MergeState { .. }))
    {
        if request.failure_facts.iter().any(|fact| {
            let RecoveryFailureFact::MergeState { state } = fact else {
                unreachable!("failure fact kind was checked above")
            };
            !merge_state.eq_ignore_ascii_case(state)
        }) {
            return Ok(stale_evidence(request));
        }
        return Ok(RequestDisposition::Current);
    }
    if !request
        .failure_facts
        .iter()
        .all(|fact| matches!(fact, RecoveryFailureFact::RequiredCheck { .. }))
    {
        return Err(CliFailure::new(
            1,
            "recovery request mixes merge-state and required-check evidence",
        ));
    }
    // Match the deterministic steward's precedence exactly. Native merge-queue
    // repositories can continue classifying required failures while BEHIND;
    // non-queue repositories must first produce a new validated head.
    let normalized_merge_state = merge_state.to_ascii_uppercase();
    if matches!(normalized_merge_state.as_str(), "DIRTY" | "CONFLICTING")
        || (!request.merge_queue && normalized_merge_state == "BEHIND")
    {
        return Ok(stale_evidence(request));
    }
    let expected_failures = request
        .failure_facts
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let Some(observed_failures) = current_failed_required_checks(checks, &request.required_checks)?
    else {
        // Missing or non-terminal required checks deterministically classify as
        // WaitingRequired, even when every originally failed check still
        // fails. Do not spend the one model attempt on that stale decision.
        return Ok(stale_evidence(request));
    };
    if observed_failures != expected_failures {
        return Ok(stale_evidence(request));
    }
    Ok(RequestDisposition::Current)
}

fn pull_checks(pull: &Value) -> Result<(Vec<StewardCheck>, bool), CliFailure> {
    let rollup = pull
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .ok_or_else(|| CliFailure::new(1, "pull-request response omitted status check rollup"))?;
    if rollup.len() > STATUS_PAGE_SIZE {
        return Err(CliFailure::new(
            1,
            "status check rollup violated its bounded 100-entry contract",
        ));
    }
    let truncated = rollup.len() == STATUS_PAGE_SIZE;
    let checks = if truncated {
        Vec::new()
    } else {
        rollup
            .iter()
            .map(|value| {
                super::super::observation::parse_check(value).ok_or_else(|| {
                    CliFailure::new(1, "status check rollup contained a malformed check")
                })
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    Ok((checks, truncated))
}

fn current_failed_required_checks(
    checks: &[StewardCheck],
    required_checks: &[RecoveryRequiredCheck],
) -> Result<Option<BTreeSet<RecoveryFailureFact>>, CliFailure> {
    let mut failed = BTreeSet::new();
    for required in required_checks {
        match required_check_state(checks, &required.context, required.app_id)? {
            RequiredCheckState::Waiting => return Ok(None),
            RequiredCheckState::Passing => {}
            RequiredCheckState::Failing { conclusion, run_id } => {
                failed.insert(RecoveryFailureFact::RequiredCheck {
                    context: required.context.clone(),
                    app_id: required.app_id,
                    conclusion,
                    run_id,
                });
            }
        }
    }
    Ok(Some(failed))
}

#[cfg(test)]
pub(super) fn classify_pull_response(
    pull: &Value,
    request: &RecoveryRequest,
    hydrated_checks: &[StewardCheck],
) -> Result<RequestDisposition, CliFailure> {
    match classify_pull_target(pull, request)? {
        RequestDisposition::Current => {
            let (mut checks, truncated) = pull_checks(pull)?;
            if truncated {
                return Err(CliFailure::new(
                    1,
                    "status check rollup reached its bounded 100-entry window without complete REST hydration",
                ));
            }
            checks.extend_from_slice(hydrated_checks);
            classify_failure_evidence(pull, request, &checks)
        }
        superseded @ RequestDisposition::Superseded(_) => Ok(superseded),
    }
}

fn stale_evidence(request: &RecoveryRequest) -> RequestDisposition {
    RequestDisposition::Superseded(format!(
        "exact target {}/{}#{} no longer has the recorded deterministic failure",
        request.repo, request.base_ref, request.pr
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RequiredCheckState {
    Waiting,
    Passing,
    Failing {
        conclusion: String,
        run_id: Option<u64>,
    },
}

fn required_check_state(
    checks: &[StewardCheck],
    context: &str,
    required_app_id: Option<u64>,
) -> Result<RequiredCheckState, CliFailure> {
    let candidates = checks
        .iter()
        .filter(|check| {
            check.name.eq_ignore_ascii_case(context)
                && required_app_id.is_none_or(|app_id| check.app_id == Some(app_id))
        })
        .collect::<Vec<_>>();
    let Some(newest_key) = candidates.iter().map(|check| check_recency(check)).max() else {
        return Ok(RequiredCheckState::Waiting);
    };
    let mut newest_state = None;
    for check in candidates
        .into_iter()
        .filter(|check| check_recency(check) == newest_key)
    {
        let state = terminal_check_state(check)?;
        if newest_state
            .as_ref()
            .is_some_and(|observed| observed != &state)
        {
            return Err(CliFailure::new(
                1,
                format!("required check `{context}` has ambiguous equally recent states"),
            ));
        }
        newest_state = Some(state);
    }
    newest_state.ok_or_else(|| {
        CliFailure::new(
            1,
            format!("required check `{context}` had no selectable current state"),
        )
    })
}

fn check_recency(check: &StewardCheck) -> (bool, &str, bool) {
    (
        check.observed_at.is_none() && !check.status.eq_ignore_ascii_case("COMPLETED"),
        check.observed_at.as_deref().unwrap_or_default(),
        check.status.eq_ignore_ascii_case("COMPLETED"),
    )
}

fn terminal_check_state(check: &StewardCheck) -> Result<RequiredCheckState, CliFailure> {
    let status = check.status.to_ascii_uppercase();
    if !matches!(
        status.as_str(),
        "COMPLETED" | "IN_PROGRESS" | "PENDING" | "QUEUED" | "REQUESTED" | "WAITING"
    ) {
        return Err(CliFailure::new(
            1,
            format!(
                "required check `{}` has unknown status `{status}`",
                check.name
            ),
        ));
    }
    if status != "COMPLETED" {
        return Ok(RequiredCheckState::Waiting);
    }
    let conclusion = check
        .conclusion
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CliFailure::new(
                1,
                format!(
                    "completed required check `{}` omitted conclusion",
                    check.name
                ),
            )
        })?
        .to_ascii_uppercase();
    if !matches!(
        conclusion.as_str(),
        "ACTION_REQUIRED"
            | "CANCELLED"
            | "FAILURE"
            | "NEUTRAL"
            | "SKIPPED"
            | "STALE"
            | "STARTUP_FAILURE"
            | "SUCCESS"
            | "TIMED_OUT"
    ) {
        return Err(CliFailure::new(
            1,
            format!(
                "required check `{}` has unknown conclusion `{conclusion}`",
                check.name
            ),
        ));
    }
    Ok(
        if matches!(conclusion.as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED") {
            RequiredCheckState::Passing
        } else {
            RequiredCheckState::Failing {
                conclusion,
                run_id: check.run_id,
            }
        },
    )
}

pub(super) fn remaining_timeout(deadline: Instant, purpose: &str) -> Result<Duration, CliFailure> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| {
            CliFailure::new(1, format!("{purpose} exceeded the overall record deadline"))
        })
}

pub(super) fn latest_status_state<'a>(
    statuses: &'a [Value],
    context: &str,
) -> Result<Option<&'a str>, CliFailure> {
    let mut matches = Vec::new();
    let mut seen_ids = BTreeSet::new();
    for status in statuses {
        let observed_context = status
            .get("context")
            .and_then(Value::as_str)
            .ok_or_else(|| CliFailure::new(1, "commit status omitted string context"))?;
        if observed_context != context {
            continue;
        }
        let created_at = status
            .get("created_at")
            .and_then(Value::as_str)
            .ok_or_else(|| CliFailure::new(1, format!("status `{context}` omitted created_at")))?;
        let timestamp = chrono::DateTime::parse_from_rfc3339(created_at).map_err(|error| {
            CliFailure::new(
                1,
                format!("status `{context}` has invalid created_at: {error}"),
            )
        })?;
        let state = status
            .get("state")
            .and_then(Value::as_str)
            .ok_or_else(|| CliFailure::new(1, format!("status `{context}` omitted state")))?;
        if !matches!(state, "error" | "failure" | "pending" | "success") {
            return Err(CliFailure::new(
                1,
                format!("status `{context}` has unknown state `{state}`"),
            ));
        }
        let id = status
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| CliFailure::new(1, format!("status `{context}` omitted numeric id")))?;
        if !seen_ids.insert(id) {
            return Err(CliFailure::new(
                1,
                format!("status `{context}` repeated id {id}"),
            ));
        }
        matches.push((timestamp, id, state));
    }
    Ok(matches
        .into_iter()
        .max_by_key(|(timestamp, id, _)| (*timestamp, *id))
        .map(|(_, _, state)| state))
}

#[cfg(test)]
#[path = "github_tests.rs"]
mod tests;
