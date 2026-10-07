//! How well the hand-back calls owners back: a summary of the `wake.*`
//! events in the ledger's event log.
//!
//! An episode is one ledger entry id with one `first_seen_at`. It is
//! `raised` when a delivering pass first sees it owner-actionable, `sent`
//! when a channel accepted it, `seen` when the owner's session displayed it
//! inside an agent turn, and `resolved` when it stopped being actionable.
//! Delivery is not receipt: only `seen` says the agent got it.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value;

use super::ledger::LedgerEvent;

/// One episode, folded from its events.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct WakeEpisode {
    /// Ledger entry id.
    pub id: String,
    /// Pull request.
    pub pr: u64,
    /// The episode's start.
    pub episode: Option<DateTime<Utc>>,
    /// First `wake.raised`.
    pub raised_at: Option<DateTime<Utc>>,
    /// First `wake.sent`.
    pub sent_at: Option<DateTime<Utc>>,
    /// Session the first send went to, else the owner session at raise.
    pub session: Option<String>,
    /// Owner state when raised (`live`, `dead`, `unknown`, `unreachable`,
    /// `none`).
    pub owner: Option<String>,
    /// Why nothing was attempted (`wake.unsent`), if logged.
    pub unsent_reason: Option<String>,
    /// First `wake.seen`.
    pub seen_at: Option<DateTime<Utc>>,
    /// `wake.resolved`, and how.
    pub resolved_at: Option<DateTime<Utc>>,
    /// `addressed`, `closed`, `gone`, `new_episode`, `not_actionable`.
    pub resolved_how: Option<String>,
    /// `wake.failed` count.
    pub failures: usize,
    /// `wake.escalated` count.
    pub escalations: usize,
}

/// Minutes at the 50th and 90th percentile, over the episodes that have both
/// ends of the interval.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Latency {
    /// Episodes measured.
    pub count: usize,
    /// Median, in minutes.
    pub p50_minutes: Option<f64>,
    /// 90th percentile, in minutes.
    pub p90_minutes: Option<f64>,
}

/// The report.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct WakeSummary {
    /// Window start.
    pub since: Option<DateTime<Utc>>,
    /// Report time.
    pub at: Option<DateTime<Utc>>,
    /// Episodes raised in the window.
    pub raised: usize,
    /// Of those, sent at least once.
    pub sent: usize,
    /// Of those sent, seen by the owner's session.
    pub seen: usize,
    /// `seen / sent`, when anything was sent.
    pub seen_rate: Option<f64>,
    /// Resolved, by how.
    pub resolved: BTreeMap<String, usize>,
    /// Still open.
    pub open: usize,
    /// Episodes with at least one failed delivery.
    pub failed: usize,
    /// Escalations.
    pub escalated: usize,
    /// Raised to first send.
    pub time_to_send: Latency,
    /// First send to seen.
    pub time_to_seen: Latency,
    /// Raised to resolved.
    pub time_to_resolve: Latency,
    /// Open episodes sent but not seen for longer than the threshold, oldest
    /// first: the calls nobody has answered.
    pub unseen: Vec<WakeEpisode>,
    /// Open episodes raised longer ago than the threshold and never sent (a
    /// dead, unknown, or unresolved owner, or no channel), oldest first: the
    /// calls nobody made.
    pub unsent: Vec<WakeEpisode>,
    /// Event-log lines that did not parse.
    pub skipped_lines: usize,
}

fn time_field(detail: Option<&Value>, key: &str) -> Option<DateTime<Utc>> {
    detail?
        .get(key)?
        .as_str()
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.with_timezone(&Utc))
}

/// Fold `wake.*` events into episodes keyed by (id, episode start).
#[must_use]
pub fn episodes(events: &[LedgerEvent]) -> Vec<WakeEpisode> {
    let mut by_key: BTreeMap<(String, Option<DateTime<Utc>>), WakeEpisode> = BTreeMap::new();
    for event in events {
        let Some(kind) = event.change.strip_prefix("wake.") else {
            continue;
        };
        let detail = event.detail.as_ref();
        let episode_start = time_field(detail, "episode");
        let key = (event.id.clone(), episode_start);
        let episode = by_key.entry(key).or_insert_with(|| WakeEpisode {
            id: event.id.clone(),
            episode: episode_start,
            ..WakeEpisode::default()
        });
        if let Some(pr) = detail.and_then(|d| d.get("pr")).and_then(Value::as_u64) {
            episode.pr = pr;
        }
        match kind {
            "raised" => {
                episode.raised_at.get_or_insert(event.at);
                let field = |key: &str| {
                    detail
                        .and_then(|d| d.get(key))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                };
                if episode.owner.is_none() {
                    episode.owner = field("owner");
                }
                if episode.session.is_none() {
                    episode.session = field("session");
                }
            }
            "unsent" => {
                if episode.unsent_reason.is_none() {
                    episode.unsent_reason = detail
                        .and_then(|d| d.get("reason"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
            "sent" => {
                if episode.sent_at.is_none() {
                    episode.sent_at = Some(event.at);
                    episode.session = detail
                        .and_then(|d| d.get("session"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
            "seen" => {
                episode.seen_at.get_or_insert(event.at);
            }
            "resolved" => {
                episode.resolved_at.get_or_insert(event.at);
                if episode.resolved_how.is_none() {
                    episode.resolved_how = detail
                        .and_then(|d| d.get("how"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
            "failed" => episode.failures += 1,
            "escalated" => episode.escalations += 1,
            _ => {}
        }
    }
    by_key.into_values().collect()
}

#[allow(clippy::cast_precision_loss)] // Minutes of a bounded window.
fn latency(mut seconds: Vec<i64>) -> Latency {
    seconds.sort_unstable();
    let pick = |fraction: f64| -> Option<f64> {
        if seconds.is_empty() {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let index = ((seconds.len() - 1) as f64 * fraction).round() as usize;
        Some(seconds[index] as f64 / 60.0)
    };
    Latency {
        count: seconds.len(),
        p50_minutes: pick(0.5),
        p90_minutes: pick(0.9),
    }
}

fn seconds(from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>) -> Option<i64> {
    Some((to? - from?).num_seconds().max(0))
}

/// Summarize the episodes raised since `since`. An open episode sent more
/// than `unseen_after` ago with no `seen` is listed in `unseen`.
#[must_use]
#[allow(clippy::cast_precision_loss)] // Episode counts.
pub fn summarize(
    events: &[LedgerEvent],
    since: DateTime<Utc>,
    now: DateTime<Utc>,
    unseen_after: Duration,
) -> WakeSummary {
    let mut summary = WakeSummary {
        since: Some(since),
        at: Some(now),
        ..WakeSummary::default()
    };
    let mut to_send = Vec::new();
    let mut to_seen = Vec::new();
    let mut to_resolve = Vec::new();
    for episode in episodes(events) {
        if episode.raised_at.is_none_or(|at| at < since) {
            continue;
        }
        summary.raised += 1;
        if episode.sent_at.is_some() {
            summary.sent += 1;
        }
        if episode.sent_at.is_some() && episode.seen_at.is_some() {
            summary.seen += 1;
        }
        if episode.failures > 0 {
            summary.failed += 1;
        }
        summary.escalated += episode.escalations;
        to_send.extend(seconds(episode.raised_at, episode.sent_at));
        to_seen.extend(seconds(episode.sent_at, episode.seen_at));
        to_resolve.extend(seconds(episode.raised_at, episode.resolved_at));
        if let Some(how) = &episode.resolved_how {
            *summary.resolved.entry(how.clone()).or_default() += 1;
        } else {
            summary.open += 1;
            match episode.sent_at {
                Some(sent) if episode.seen_at.is_none() && now - sent > unseen_after => {
                    summary.unseen.push(episode);
                }
                None if episode.raised_at.is_some_and(|at| now - at > unseen_after) => {
                    let mut episode = episode;
                    // Every channel failed: still nobody was reached, but say
                    // why rather than leave it reasonless.
                    if episode.failures > 0 && episode.unsent_reason.is_none() {
                        episode.unsent_reason = Some("delivery_failed".to_owned());
                    }
                    summary.unsent.push(episode);
                }
                _ => {}
            }
        }
    }
    summary.seen_rate = (summary.sent > 0).then(|| summary.seen as f64 / summary.sent as f64);
    summary.time_to_send = latency(to_send);
    summary.time_to_seen = latency(to_seen);
    summary.time_to_resolve = latency(to_resolve);
    summary.unseen.sort_by_key(|episode| episode.sent_at);
    summary.unsent.sort_by_key(|episode| episode.raised_at);
    summary
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;
    use serde_json::json;

    use super::*;

    fn t(minutes: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 7, 0, 0, 0).unwrap() + Duration::minutes(minutes)
    }

    fn wake(at: i64, id: &str, change: &str, episode: i64, extra: &Value) -> LedgerEvent {
        let mut detail = json!({"pr": 7, "episode": t(episode)});
        if let (Some(target), Some(fields)) = (detail.as_object_mut(), extra.as_object()) {
            target.extend(fields.clone());
        }
        LedgerEvent {
            at: t(at),
            id: id.to_owned(),
            change: format!("wake.{change}"),
            evidence: String::new(),
            detail: Some(detail),
        }
    }

    #[test]
    fn delivery_is_not_receipt_and_the_unanswered_calls_are_listed() {
        let none = json!({});
        let events = vec![
            // a: raised 0, sent 2, seen 12, resolved 40 (addressed).
            wake(0, "a", "raised", -5, &none),
            wake(2, "a", "sent", -5, &json!({"session": "s1", "rung": "1"})),
            wake(12, "a", "seen", -5, &none),
            wake(40, "a", "resolved", -5, &json!({"how": "addressed"})),
            // b: raised 10, sent 11, never seen, still open: the call nobody
            // answered (the shape of 10:24Z -> 14:20Z).
            wake(10, "b", "raised", 9, &none),
            wake(11, "b", "sent", 9, &json!({"session": "s2", "rung": "1"})),
            // e: raised 5 for a dead owner, never sent, still open: the call
            // nobody made (the shape of 02:42Z reported late).
            wake(
                5,
                "e",
                "raised",
                4,
                &json!({"owner": "dead", "session": "s9"}),
            ),
            // f: raised 6, every channel failed, still open.
            wake(
                6,
                "f",
                "raised",
                5,
                &json!({"owner": "live", "session": "s3"}),
            ),
            wake(6, "f", "failed", 5, &none),
            // c: raised 20, every channel failed, then closed.
            wake(20, "c", "raised", 19, &none),
            wake(20, "c", "failed", 19, &none),
            wake(30, "c", "resolved", 19, &json!({"how": "closed"})),
            // d: raised before the window.
            wake(-100, "d", "raised", -101, &none),
            // Not a wake event.
            LedgerEvent {
                at: t(1),
                id: "x".to_owned(),
                change: "opened".to_owned(),
                evidence: String::new(),
                detail: None,
            },
        ];
        let summary = summarize(&events, t(-10), t(60), Duration::minutes(30));
        assert_eq!(summary.raised, 5);
        assert_eq!(summary.sent, 2);
        assert_eq!(summary.seen, 1);
        assert_eq!(summary.seen_rate, Some(0.5));
        assert_eq!(summary.failed, 2);
        assert_eq!(summary.open, 3);
        assert_eq!(summary.resolved.get("addressed"), Some(&1));
        assert_eq!(summary.resolved.get("closed"), Some(&1));
        assert_eq!(summary.time_to_seen.count, 1);
        assert_eq!(summary.time_to_seen.p50_minutes, Some(10.0));
        assert_eq!(summary.time_to_send.count, 2);
        let unseen: Vec<&str> = summary.unseen.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(unseen, vec!["b"]);
        assert_eq!(summary.unseen[0].session.as_deref(), Some("s2"));
        // Never sent is not unseen: it is listed apart, with its owner state.
        let unsent: Vec<&str> = summary.unsent.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(unsent, vec!["e", "f"]);
        assert_eq!(summary.unsent[0].owner.as_deref(), Some("dead"));
        assert_eq!(summary.unsent[0].session.as_deref(), Some("s9"));
        assert_eq!(summary.unsent[0].unsent_reason, None);
        // Open with every channel failed: unsent, and it says why.
        assert_eq!(
            summary.unsent[1].unsent_reason.as_deref(),
            Some("delivery_failed")
        );
        // Within the threshold neither list has it yet.
        let early = summarize(&events, t(-10), t(30), Duration::minutes(30));
        assert!(early.unseen.is_empty());
        assert!(early.unsent.is_empty());
    }

    #[test]
    fn a_new_episode_of_the_same_entry_is_counted_separately() {
        let none = json!({});
        let events = vec![
            wake(0, "a", "raised", 0, &none),
            wake(5, "a", "resolved", 0, &json!({"how": "new_episode"})),
            wake(5, "a", "raised", 4, &none),
        ];
        let summary = summarize(&events, t(-10), t(60), Duration::minutes(30));
        assert_eq!(summary.raised, 2);
        assert_eq!(summary.open, 1);
        assert_eq!(summary.resolved.get("new_episode"), Some(&1));
    }
}
