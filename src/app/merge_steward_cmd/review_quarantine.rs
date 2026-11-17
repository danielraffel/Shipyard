//! Tell the author when the standing reviewer quarantined the head.
//!
//! The pass-runner quarantines a head it could not review after its bounded
//! retries (`review-quarantine.txt`, one `<repo>#<pr> <head> <reason>
//! since=<epoch>` line per head), and a reviewer agent then posts one plain
//! comment naming the cause: `Review quarantine diagnosis: <why>. <what would
//! make it reviewable>`. Until a new head arrives nothing will review this
//! one, yet the row read `no_verdict` ("awaiting the standing reviewer;
//! nothing for the author to do") for hours.
//!
//! A row that waits on a reviewer and whose current head the quarantine file
//! lists becomes `review_quarantined`, with the diagnosis's first line as its
//! detail and the author as the actor. The file is the marker (it already
//! binds the exact head); the comment is read only for such rows, so the cost
//! is one read per quarantined head per pass, and none for any other row. A
//! missing or unreadable comment keeps the token and says to read the PR.
//! The comment is untrusted text: it only fills the detail, never changes
//! what the steward does.

use serde_json::Value;

use super::scheduling_loop::Quarantine;
use super::{GitHubActions, PrReport, RepoObservation, StewardPolicy};

/// The diagnosis comment's opening words, matched case-insensitively.
const MARKER: &str = "review quarantine diagnosis:";
/// Longest diagnosis line kept in the row.
const MAX_DETAIL: usize = 240;

/// The tokens that read "waiting on a reviewer" and so become
/// `review_quarantined` when the head is quarantined. Every other token (an
/// approved, green, red or held head) keeps its real state.
fn waits_on_reviewer(report: &PrReport, policy: &StewardPolicy) -> bool {
    let token = report.blocked.blocked_on.as_str();
    let kind = token.split_once(':').map_or(token, |(kind, _)| kind);
    match kind {
        "no_verdict" | "awaiting_review" | "waiting_head_approval" => true,
        // The approval status is `pending` while the head awaits its
        // reviewer: only when it is the sole pending context.
        "required_pending" => {
            let approval = policy.approval_context.trim();
            !approval.is_empty()
                && report
                    .blocked
                    .blocked_detail
                    .as_deref()
                    .is_some_and(|detail| {
                        detail
                            .split(',')
                            .all(|name| name.trim().eq_ignore_ascii_case(approval))
                    })
        }
        _ => false,
    }
}

/// Mark every reviewer-waiting row whose current head the quarantine file
/// lists. Reads one PR's comments per such row; nothing otherwise.
pub(super) fn annotate(
    actions: &GitHubActions,
    observation: &RepoObservation,
    policy: &StewardPolicy,
    quarantine: &Quarantine,
    reports: &mut [PrReport],
) {
    if quarantine.is_empty() {
        return;
    }
    let repo = observation.repo.to_ascii_lowercase();
    for report in reports.iter_mut() {
        if super::pass_budget::is_deferred(report) || !waits_on_reviewer(report, policy) {
            continue;
        }
        let Some((head, since)) = quarantine.get(&format!("{repo}#{}", report.number)) else {
            continue;
        };
        if !head.eq_ignore_ascii_case(&report.head_sha) {
            continue;
        }
        let Some(pr) = observation
            .prs
            .iter()
            .find(|pr| pr.fact.number == report.number)
        else {
            continue;
        };
        let quarantined_at = since.map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        let comment = read_diagnosis(
            actions,
            &observation.repo,
            report.number,
            quarantined_at.as_deref(),
        );
        let mut blocked = std::mem::take(&mut report.blocked);
        "review_quarantined".clone_into(&mut blocked.blocked_on);
        blocked.blocked_detail = Some(match &comment {
            Some((line, _)) => line.clone(),
            None => "no diagnosis comment was read; the reviewer's comment on the pull request \
                     names the cause"
                .to_owned(),
        });
        // The state began at the quarantine, or the diagnosis when the file
        // has no time; otherwise the earlier waiting date stands.
        if let Some(at) = quarantined_at {
            blocked.since = Some(at);
            blocked.since_source = Some("review_quarantine");
        } else if let Some((_, at)) = comment {
            blocked.since = Some(at);
            blocked.since_source = Some("quarantine_comment");
        }
        blocked.next_action = Some(super::next_action::for_row(
            &super::next_action::RowContext {
                repo: &observation.repo,
                base: &observation.base,
                pr: &pr.fact,
                policy,
                error: report.error.as_deref(),
            },
            &blocked,
        ));
        report.blocked = blocked;
    }
}

/// The newest diagnosis comment on the pull request, no older than the
/// quarantine (when its time is known): its first line and creation time.
fn read_diagnosis(
    actions: &GitHubActions,
    repo: &str,
    number: u64,
    not_before: Option<&str>,
) -> Option<(String, String)> {
    let lines = actions
        .run_gh(&[
            "api".to_owned(),
            "--paginate".to_owned(),
            format!("repos/{repo}/issues/{number}/comments?per_page=100"),
            "--jq".to_owned(),
            format!(
                r#".[] | select((.body // "") | ascii_downcase | ltrimstr(" ") | startswith("{MARKER}")) | {{created_at, body}} | tojson"#
            ),
        ])
        .ok()?;
    diagnosis_from_lines(&lines, not_before)
}

/// The newest `{created_at, body}` JSON line that is no older than
/// `not_before`, as the diagnosis's first line and its time.
fn diagnosis_from_lines(lines: &str, not_before: Option<&str>) -> Option<(String, String)> {
    lines
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| {
            let at = value.get("created_at")?.as_str()?.to_owned();
            let body = value.get("body")?.as_str()?;
            Some((at, first_line(body)?))
        })
        .filter(|(at, _)| not_before.is_none_or(|floor| at.as_str() >= floor))
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(at, line)| (line, at))
}

/// The diagnosis text of a comment's first line, with the marker removed and
/// the length bounded.
fn first_line(body: &str) -> Option<String> {
    let line = body.trim().lines().next()?.trim();
    let text = line
        .get(MARKER.len()..)
        .filter(|_| line.to_ascii_lowercase().starts_with(MARKER))?
        .trim();
    if text.is_empty() {
        return None;
    }
    let mut out: String = text.chars().take(MAX_DETAIL).collect();
    if text.chars().count() > MAX_DETAIL {
        out.push_str("...");
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_line_of_the_diagnosis_is_kept_without_the_marker() {
        let body = "Review quarantine diagnosis: this head bundles removals. Please split.\n\nmore";
        assert_eq!(
            first_line(body).as_deref(),
            Some("this head bundles removals. Please split.")
        );
        assert_eq!(first_line("Review quarantine diagnosis:"), None);
        assert_eq!(first_line("looks fine to me"), None);
    }

    #[test]
    fn the_newest_diagnosis_since_the_quarantine_wins() {
        let lines = [
            r#"{"created_at":"2026-11-17T03:05:55Z","body":"Review quarantine diagnosis: old"}"#,
            r#"{"created_at":"2026-11-17T03:06:22Z","body":"Review quarantine diagnosis: new"}"#,
        ]
        .join("\n");
        assert_eq!(
            diagnosis_from_lines(&lines, None),
            Some(("new".to_owned(), "2026-11-17T03:06:22Z".to_owned()))
        );
        // A comment older than the quarantine belongs to an earlier head.
        assert_eq!(
            diagnosis_from_lines(&lines, Some("2026-11-17T04:00:00Z")),
            None
        );
    }
}
