//! Approval from the fleet's independent admission check instead of comments.
//!
//! ## Why
//!
//! A verdict comment is text posted through the shared fleet App, which every
//! agent can use, so a comment, a `reviewer:` signer line or a session stamp
//! proves nothing about who reviewed the head. The fleet's trust root posts a
//! `fleet-guard/admission` check run on every pull-request head from its own
//! GitHub App, whose credentials no agent can read. A check run's `app.id` is
//! set by GitHub, not by the poster, so it is evidence a fleet agent cannot
//! forge.
//!
//! ## The setting
//!
//! `[auto_merge] approval_source = "guard"` on the protected base switches
//! every caller of the approval gate (the steward's enqueue, exact-head merge
//! and native arm, the carrier, `shipyard review-verdict` and the
//! `review-verdict` status) to [`judge`]. It needs `guard_app_id`, the
//! admission App's numeric id; `guard_admission_check` renames the check
//! (default [`DEFAULT_GUARD_CHECK`]). `approval_source = "comments"`, the
//! default, keeps the comment and review rule as a transition fallback.
//! There is no mixed mode: in guard mode no comment or review is read for
//! authorization, and approvals never carry from an earlier head, because a
//! new head needs its own admission. The merge-queue ejection cap still
//! applies to an admitted head (see [`evaluate`]).
//!
//! A host can refuse the fallback outright: a [`HOST_REQUIRE_FILE`] in the
//! guards directory makes every comment-mode policy fail closed, so a base
//! config edit cannot quietly return such a host to comments.
//!
//! ## What counts
//!
//! Only check runs named after the configured check, on the exact head, from
//! the configured App. The newest of them (highest id) decides: completed
//! with conclusion `success` and an `external_id` that binds the same head
//! admits; completed with any other conclusion refuses; queued or in progress
//! waits. No such run waits. A run whose `external_id` is missing, not JSON,
//! or binds another head counts as absent. Runs of that name from any other
//! App are reported, never counted.

use serde::Serialize;
use serde_json::Value;

use super::{Approval, ApprovalBlocker, HeadGate, RunGh};
use crate::reviewer_receipt::SHARED_ACTIONS_APP_ID;

/// `[auto_merge]` key choosing what authorizes a head.
pub const APPROVAL_SOURCE_KEY: &str = "approval_source";
/// `[auto_merge]` key naming the admission check run.
pub const GUARD_CHECK_KEY: &str = "guard_admission_check";
/// `[auto_merge]` key naming the admission App's numeric id.
pub const GUARD_APP_ID_KEY: &str = "guard_app_id";
/// The admission check the fleet trust root posts.
pub const DEFAULT_GUARD_CHECK: &str = "fleet-guard/admission";
/// Guards-directory file that makes this host refuse comment-mode policies:
/// the host's machine-global downgrade guard (an accident guard; the agent
/// user can remove it).
pub const HOST_REQUIRE_FILE: &str = "require-guard-admission";
/// App slugs every agent or workflow can post as; never admission authority.
const SHARED_APP_SLUGS: &[&str] = &["github-actions", "shipyard-local"];

/// What authorizes a head on this base.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ApprovalSource {
    /// Reviewer comments and reviews (the transition fallback).
    #[default]
    Comments,
    /// The admission check run from one App.
    Guard {
        /// Check-run name.
        check: String,
        /// The only App whose run counts.
        app_id: u64,
    },
    /// The base asked for guard mode but its settings are unusable. Every
    /// head is refused rather than judged by comments.
    Misconfigured {
        /// What is wrong.
        detail: String,
    },
}

impl ApprovalSource {
    /// Whether comments and reviews decide.
    #[must_use]
    pub const fn is_comments(&self) -> bool {
        matches!(self, Self::Comments)
    }
}

/// Read the approval source from an `[auto_merge]` table.
///
/// # Errors
///
/// A source other than `comments` or `guard`, or a guard source without a
/// usable App id or check name. Callers that cannot fail map the error to
/// [`ApprovalSource::Misconfigured`], never to comments.
pub fn source_from_section(section: Option<&toml::Table>) -> Result<ApprovalSource, String> {
    let Some(section) = section else {
        return Ok(ApprovalSource::Comments);
    };
    match section.get(APPROVAL_SOURCE_KEY) {
        None => return Ok(ApprovalSource::Comments),
        Some(toml::Value::String(value)) if value.trim() == "comments" => {
            return Ok(ApprovalSource::Comments);
        }
        Some(toml::Value::String(value)) if value.trim() == "guard" => {}
        Some(other) => {
            return Err(format!(
                "[auto_merge] {APPROVAL_SOURCE_KEY} must be \"guard\" or \"comments\", not {other}"
            ));
        }
    }
    let app_id = match section.get(GUARD_APP_ID_KEY) {
        Some(toml::Value::Integer(id)) if *id > 0 => u64::try_from(*id).unwrap_or(0),
        Some(other) => {
            return Err(format!(
                "[auto_merge] {GUARD_APP_ID_KEY} must be a positive App id, not {other}"
            ));
        }
        None => {
            return Err(format!(
                "[auto_merge] {APPROVAL_SOURCE_KEY} = \"guard\" needs {GUARD_APP_ID_KEY}"
            ));
        }
    };
    if app_id == SHARED_ACTIONS_APP_ID {
        return Err(format!(
            "[auto_merge] {GUARD_APP_ID_KEY} may not be GitHub Actions ({SHARED_ACTIONS_APP_ID}): \
             any workflow can post as it"
        ));
    }
    let check = match section.get(GUARD_CHECK_KEY) {
        None => DEFAULT_GUARD_CHECK.to_owned(),
        Some(toml::Value::String(name)) if !name.trim().is_empty() => name.trim().to_owned(),
        Some(other) => {
            return Err(format!(
                "[auto_merge] {GUARD_CHECK_KEY} must be a non-empty check name, not {other}"
            ));
        }
    };
    Ok(ApprovalSource::Guard { check, app_id })
}

/// Whether this host refuses comment-mode policies ([`HOST_REQUIRE_FILE`]).
#[must_use]
pub fn host_requires_guard() -> bool {
    host_guards_dir().is_some_and(|dir| dir.join(HOST_REQUIRE_FILE).is_file())
}

#[cfg(not(test))]
fn host_guards_dir() -> Option<std::path::PathBuf> {
    super::guards_dir()
}

/// Tests read no host state: only [`super::with_guards_dir`]'s directory.
#[cfg(test)]
fn host_guards_dir() -> Option<std::path::PathBuf> {
    super::guards_dir_override()
}

/// The admission decision for one head.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "admission", rename_all = "snake_case")]
pub enum Judgement {
    /// The newest counting run admits the head.
    Admitted {
        /// Check-run id.
        check_run_id: u64,
        /// Policy the trust root judged under.
        policy_sha: String,
        /// `routine` or `enforcement`, as the trust root classified it.
        classification: String,
    },
    /// No counting run decides yet (none, or the newest is not complete).
    Waiting {
        /// Why.
        detail: String,
    },
    /// The newest counting run refuses the head.
    Refused {
        /// Check-run id.
        check_run_id: u64,
        /// Its conclusion.
        conclusion: String,
    },
}

/// Judge `head` from its check runs, as `repos/{repo}/commits/{head}/check-runs`
/// returns them. `events` receives one note per ignored run.
#[must_use]
pub fn judge(
    runs: &[Value],
    head: &str,
    check: &str,
    app_id: u64,
    events: &mut Vec<String>,
) -> Judgement {
    let mut counting = Vec::new();
    for run in runs {
        if run.get("name").and_then(Value::as_str) != Some(check) {
            continue;
        }
        let id = run.get("id").and_then(Value::as_u64).unwrap_or(0);
        let run_app = run.pointer("/app/id").and_then(Value::as_u64);
        let slug = run
            .pointer("/app/slug")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if run_app != Some(app_id) || SHARED_APP_SLUGS.contains(&slug) {
            events.push(format!(
                "excluded_admission:run {id} from app {} ({slug})",
                run_app.map_or_else(|| "none".to_owned(), |value| value.to_string())
            ));
            continue;
        }
        if !run
            .get("head_sha")
            .and_then(Value::as_str)
            .is_some_and(|sha| sha.eq_ignore_ascii_case(head))
        {
            events.push(format!("excluded_admission:run {id} is on another head"));
            continue;
        }
        counting.push((id, run));
    }
    let Some((id, run)) = counting.into_iter().max_by_key(|(id, _)| *id) else {
        return Judgement::Waiting {
            detail: format!("no `{check}` check run from App {app_id} on this head yet"),
        };
    };
    if !run
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| status.eq_ignore_ascii_case("completed"))
    {
        return Judgement::Waiting {
            detail: format!("`{check}` run {id} has not completed"),
        };
    }
    let conclusion = run
        .get("conclusion")
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_ascii_lowercase();
    if conclusion != "success" {
        return Judgement::Refused {
            check_run_id: id,
            conclusion,
        };
    }
    match bound_record(run, head) {
        Ok((policy_sha, classification)) => Judgement::Admitted {
            check_run_id: id,
            policy_sha,
            classification,
        },
        Err(why) => {
            events.push(format!("excluded_admission:run {id}: {why}"));
            Judgement::Waiting {
                detail: format!("`{check}` run {id} does not bind this head ({why})"),
            }
        }
    }
}

/// `(policy_sha, classification)` from a run's `external_id`, when it binds
/// `head`.
fn bound_record(run: &Value, head: &str) -> Result<(String, String), String> {
    let raw = run
        .get("external_id")
        .and_then(Value::as_str)
        .filter(|raw| !raw.trim().is_empty())
        .ok_or("no external_id")?;
    let record: Value =
        serde_json::from_str(raw).map_err(|error| format!("external_id is not JSON: {error}"))?;
    let field = |key: &str| record.get(key).and_then(Value::as_str).map(str::trim);
    if !field("head_sha").is_some_and(|sha| sha.eq_ignore_ascii_case(head)) {
        return Err("external_id names another head".to_owned());
    }
    let policy_sha = field("policy_sha")
        .filter(|sha| sha.len() == 40 && sha.chars().all(|ch| ch.is_ascii_hexdigit()))
        .ok_or("external_id has no full policy_sha")?;
    let classification = field("classification")
        .filter(|value| !value.is_empty())
        .ok_or("external_id has no classification")?;
    Ok((policy_sha.to_ascii_lowercase(), classification.to_owned()))
}

/// The check runs named `check` on `head`, every page.
fn read_runs(run_gh: RunGh<'_>, repo: &str, head: &str, check: &str) -> Result<Vec<Value>, String> {
    let raw = run_gh(&[
        "api".to_owned(),
        "--paginate".to_owned(),
        format!(
            "repos/{repo}/commits/{head}/check-runs?check_name={}&filter=all&per_page=100",
            crate::required_check_policy::encode_path_segment(check)
        ),
        "--jq".to_owned(),
        ".check_runs[] | {id, name, head_sha, status, conclusion, external_id, app: {id: .app.id, slug: .app.slug}}"
            .to_owned(),
    ])?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .map_err(|error| format!("malformed `{check}` check run: {error}"))
        })
        .collect()
}

/// A gate, the ignored evidence behind it, and why it refuses (if it does).
pub(super) type GateOutcome = (HeadGate, Vec<String>, Option<ApprovalBlocker>);

/// The gate for a policy whose source is not comments, or `None` when
/// comments decide. Reads only the head's admission check runs.
///
/// The merge-queue ejection cap still applies: it is the steward's
/// loop-breaker, not an approval rule. An admitted head the queue removed
/// [`super::EJECTION_CAP`] times is [`HeadGate::EjectionCap`]. Unlike a
/// reviewer's fresh comment, a later admission run does not lift it (the
/// trust root re-posts admission every tick), so only a new head does;
/// `latest_was_interruption` excuses the newest removal, as in comment mode.
///
/// # Errors
///
/// When the check runs cannot be read. Callers must not arm on an error.
pub(super) fn evaluate(
    run_gh: RunGh<'_>,
    repo: &str,
    head: &str,
    source: &ApprovalSource,
    queue: &Value,
    latest_was_interruption: bool,
) -> Result<Option<GateOutcome>, String> {
    let (check, app_id) = match source {
        ApprovalSource::Comments if host_requires_guard() => {
            return Ok(Some(refuse(
                head,
                format!(
                    "this host requires fleet-guard admission ({HOST_REQUIRE_FILE}), but the \
                     base's [auto_merge] {APPROVAL_SOURCE_KEY} selects comments"
                ),
            )));
        }
        ApprovalSource::Comments => return Ok(None),
        ApprovalSource::Misconfigured { detail } => {
            return Ok(Some(refuse(head, detail.clone())));
        }
        ApprovalSource::Guard { check, app_id } => (check, *app_id),
    };
    let runs = read_runs(run_gh, repo, head, check)?;
    let mut events = Vec::new();
    Ok(Some(match judge(&runs, head, check, app_id, &mut events) {
        Judgement::Admitted {
            check_run_id,
            policy_sha,
            classification,
        } => match capped(queue, head, latest_was_interruption) {
            Some(ejections) => {
                let at = ejections.last().and_then(|ejection| ejection.at.clone());
                (
                    HeadGate::EjectionCap {
                        head: head.to_owned(),
                        ejections,
                    },
                    events,
                    Some(ApprovalBlocker::EjectionCap { at }),
                )
            }
            None => (
                HeadGate::Approved {
                    approval: Approval::Admission {
                        check: check.clone(),
                        app_id,
                        check_run_id,
                        policy_sha,
                        classification,
                    },
                },
                events,
                None,
            ),
        },
        Judgement::Waiting { detail } => {
            let (gate, mut more, blocker) = refuse(head, detail);
            more.append(&mut events);
            (gate, more, blocker)
        }
        Judgement::Refused {
            check_run_id,
            conclusion,
        } => {
            let detail = format!("`{check}` run {check_run_id} concluded {conclusion}");
            let (gate, mut more, _) = refuse(head, detail.clone());
            more.append(&mut events);
            (
                gate,
                more,
                Some(ApprovalBlocker::AdmissionRefused {
                    check_run_id,
                    detail,
                }),
            )
        }
    }))
}

/// The head's queue removals when they reach the cap, else `None`.
fn capped(
    queue: &Value,
    head: &str,
    latest_was_interruption: bool,
) -> Option<Vec<super::Ejection>> {
    let mut ejections = super::head_ejections(queue, head);
    if latest_was_interruption {
        ejections.pop();
    }
    (ejections.len() >= super::EJECTION_CAP).then_some(ejections)
}

fn refuse(head: &str, detail: String) -> GateOutcome {
    (
        HeadGate::NotAdmitted {
            head: head.to_owned(),
            detail: detail.clone(),
        },
        Vec::new(),
        Some(ApprovalBlocker::AdmissionMissing { detail }),
    )
}

#[cfg(test)]
mod tests;
