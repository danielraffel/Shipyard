//! Find Shipyard webhooks that no live, advertising daemon owns.
//!
//! Every daemon registers its own hook on each repository it advertises and
//! deletes it on a clean shutdown. Hooks outlive their owner when a host is
//! renamed or retired, when a daemon stops advertising a repository, or when a
//! shutdown could not delete. GitHub keeps delivering to them, and each failed
//! delivery is noise in every delivery-health reading.
//!
//! Classification is pure so it can be tested without GitHub. Two kinds of
//! hook are prunable, and each needs its own evidence:
//!
//! - **This host's hook on a repository the running daemon does not
//!   advertise.** The daemon's own advertised set is authoritative for its own
//!   hooks, so no delivery evidence is needed.
//! - **A peer host's hook whose endpoint has answered nothing but gateway
//!   failures for a sustained window.** A peer is never pruned for an
//!   application-level refusal (400, 401, 404, 405): that proves a daemon is
//!   answering. A window too short or too sparse to prove the endpoint is gone
//!   leaves the hook in place and says why.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use crate::webhook_reconcile::DeliveryOutcome;

/// Fewest failed deliveries that can show a peer endpoint is gone.
pub const DEAD_TARGET_MIN_FAILURES: usize = 5;

/// Shortest span of uninterrupted failures that can show a peer endpoint is
/// gone. A daemon that is restarting, or briefly wedged, recovers well inside
/// this; a retired or renamed host never does.
pub const DEAD_TARGET_MIN_SPAN_HOURS: i64 = 24;

/// One delivery attempt as the classifier needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryFact {
    /// When GitHub attempted the delivery.
    pub delivered_at: DateTime<Utc>,
    /// Who answered, if anyone.
    pub outcome: DeliveryOutcome,
}

/// One webhook as GitHub reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookFacts {
    /// GitHub hook id.
    pub id: u64,
    /// Callback URL.
    pub url: String,
    /// Subscribed events.
    pub events: Vec<String>,
    /// Recent deliveries, newest first, when they were read.
    pub deliveries: Option<Vec<DeliveryFact>>,
}

/// Why a hook should be deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PruneReason {
    /// This host's hook on a repository its running daemon does not advertise.
    OwnUnadvertised,
    /// A peer's hook on a tailnet host that no longer exists (renamed or
    /// removed), whose endpoint has only failed at the gateway.
    RetiredHost {
        /// Failed deliveries in the window.
        failures: usize,
    },
    /// A peer's hook whose endpoint has only failed at the gateway.
    DeadTarget {
        /// Failed deliveries in the window.
        failures: usize,
        /// Hours between the oldest and newest failure.
        span_hours: i64,
    },
}

/// The classifier's decision for one hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Not a Shipyard daemon hook; never touched.
    Foreign,
    /// A live hook that stays.
    Keep(String),
    /// Delete this hook.
    Prune(PruneReason),
    /// Possibly stale, but the evidence cannot prove it; left in place.
    Undecided(String),
}

impl PruneReason {
    /// One-line operator explanation.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::OwnUnadvertised => {
                "this host's hook, but the running daemon does not advertise this repository"
                    .to_owned()
            }
            Self::RetiredHost { failures } => format!(
                "no tailnet node has this name any more, and its last {failures} deliveries all failed at the gateway"
            ),
            Self::DeadTarget {
                failures,
                span_hours,
            } => format!(
                "endpoint answered nothing but gateway failures: {failures} deliveries over {span_hours}h, none delivered"
            ),
        }
    }
}

/// The host a callback URL points at, when it is a Shipyard daemon callback.
///
/// Shipyard registers `https://<host>/webhook`; older daemons registered the
/// bare `https://<host>` root, which is still recognised so their leftovers can
/// be cleaned up.
#[must_use]
pub fn daemon_callback_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    (!host.is_empty() && (path.is_empty() || path == "webhook")).then_some(host)
}

/// True when the hook's events look like a Shipyard daemon's: `workflow_run`
/// plus nothing outside the current subscription. Older daemons subscribed to
/// a subset (before `release` was added), and their leftovers must still be
/// recognised.
#[must_use]
pub fn has_daemon_events(events: &[String], subscribed: &[&str]) -> bool {
    events.iter().any(|event| event == "workflow_run")
        && events
            .iter()
            .all(|event| subscribed.contains(&event.as_str()))
}

/// What this host knows about the fleet, for classification.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostContext<'a> {
    /// This host's tailnet name.
    pub this_host: Option<&'a str>,
    /// Repositories the running daemon advertises.
    pub advertised: Option<&'a BTreeSet<String>>,
    /// Every node name on this host's tailnet, offline nodes included.
    pub tailnet_nodes: Option<&'a BTreeSet<String>>,
}

/// True when `host` is a fleet daemon host: on this host's tailnet, sharing
/// its domain after the first label. Only fleet hosts can be judged; any other
/// receiver belongs to someone else, however daemon-shaped it looks.
fn is_fleet_host(host: &str, this_host: &str) -> bool {
    let suffix = |name: &str| name.split_once('.').map(|(_, rest)| rest.to_owned());
    suffix(this_host).is_some() && suffix(host) == suffix(this_host)
}

/// True when `host` is a fleet host name that is no longer a tailnet node.
fn is_retired_tailnet_host(host: &str, context: &HostContext<'_>) -> bool {
    let (Some(this_host), Some(nodes)) = (context.this_host, context.tailnet_nodes) else {
        return false;
    };
    is_fleet_host(host, this_host) && !nodes.contains(host)
}

/// Decide one hook.
#[must_use]
pub fn classify_hook(
    repo: &str,
    hook: &HookFacts,
    context: &HostContext<'_>,
    subscribed: &[&str],
) -> Verdict {
    let this_host = context.this_host;
    let advertised = context.advertised;
    let Some(host) = daemon_callback_host(&hook.url) else {
        return Verdict::Foreign;
    };
    if !has_daemon_events(&hook.events, subscribed) {
        return Verdict::Foreign;
    }

    if this_host == Some(host) {
        return match advertised {
            Some(advertised) if advertised.contains(&repo.to_ascii_lowercase()) => {
                Verdict::Keep("this host's hook on an advertised repository".to_owned())
            }
            Some(_) => Verdict::Prune(PruneReason::OwnUnadvertised),
            None => Verdict::Undecided(
                "this host's hook, but no running daemon reported what it advertises".to_owned(),
            ),
        };
    }

    let Some(this_host) = this_host else {
        return Verdict::Undecided(
            "this host's tailnet identity is unreadable, so no peer can be judged".to_owned(),
        );
    };
    if !is_fleet_host(host, this_host) {
        return Verdict::Keep(
            "not a host on this tailnet; only fleet daemon hooks are ever pruned".to_owned(),
        );
    }
    let Some(deliveries) = &hook.deliveries else {
        return Verdict::Undecided("peer hook whose deliveries could not be read".to_owned());
    };
    let verdict = dead_target_verdict(deliveries);
    // A host that left the tailnet cannot be restarting: the failure count
    // alone proves the hook is orphaned, however short the window.
    if matches!(verdict, Verdict::Undecided(_))
        && is_retired_tailnet_host(host, context)
        && deliveries.len() >= DEAD_TARGET_MIN_FAILURES
        && deliveries
            .iter()
            .all(|delivery| is_gateway_failure(&delivery.outcome))
    {
        return Verdict::Prune(PruneReason::RetiredHost {
            failures: deliveries.len(),
        });
    }
    verdict
}

fn dead_target_verdict(deliveries: &[DeliveryFact]) -> Verdict {
    if deliveries
        .iter()
        .any(|delivery| delivery.outcome == DeliveryOutcome::Delivered)
    {
        return Verdict::Keep("peer hook with a recent successful delivery".to_owned());
    }
    if let Some(answered) = deliveries
        .iter()
        .find(|delivery| !is_gateway_failure(&delivery.outcome))
    {
        return Verdict::Keep(format!(
            "peer endpoint is answering (latest non-gateway outcome: {:?})",
            answered.outcome
        ));
    }
    if deliveries.len() < DEAD_TARGET_MIN_FAILURES {
        return Verdict::Undecided(format!(
            "only {} failed deliveries on record; {DEAD_TARGET_MIN_FAILURES} are needed",
            deliveries.len()
        ));
    }
    let newest = deliveries
        .iter()
        .map(|delivery| delivery.delivered_at)
        .max();
    let oldest = deliveries
        .iter()
        .map(|delivery| delivery.delivered_at)
        .min();
    let span_hours = match (newest, oldest) {
        (Some(newest), Some(oldest)) => (newest - oldest).num_hours(),
        _ => 0,
    };
    if span_hours < DEAD_TARGET_MIN_SPAN_HOURS {
        return Verdict::Undecided(format!(
            "failures span {span_hours}h; {DEAD_TARGET_MIN_SPAN_HOURS}h are needed to tell a retired host from a restarting one"
        ));
    }
    Verdict::Prune(PruneReason::DeadTarget {
        failures: deliveries.len(),
        span_hours,
    })
}

/// True once reading older deliveries cannot change the verdict: the window
/// already holds an attempt a daemon answered, or its gateway failures span the
/// dead-target threshold.
#[must_use]
pub fn window_is_conclusive(deliveries: &[DeliveryFact]) -> bool {
    !matches!(dead_target_verdict(deliveries), Verdict::Undecided(_))
}

/// Nothing answered, or only a tunnel or proxy answered on a dead backend's
/// behalf (502, 503, 504). Anything else came from a running daemon.
fn is_gateway_failure(outcome: &DeliveryOutcome) -> bool {
    match outcome {
        DeliveryOutcome::Delivered => false,
        DeliveryOutcome::Unreachable { .. } => true,
        DeliveryOutcome::Rejected { status_code } => matches!(status_code, 502..=504),
    }
}

/// Why a local registration record should be dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StaleRegistration {
    /// The running daemon does not advertise the repository.
    Unadvertised,
    /// GitHub no longer holds the recorded hook.
    HookGone,
}

impl StaleRegistration {
    /// One-line operator explanation.
    #[must_use]
    pub const fn describe(&self) -> &'static str {
        match self {
            Self::Unadvertised => "the running daemon does not advertise this repository",
            Self::HookGone => "GitHub no longer holds the recorded hook",
        }
    }
}

/// Local `registrations.json` records that no longer describe a live hook.
///
/// `live_hook_ids` holds, per repository, the hook ids GitHub reported; a
/// repository whose hooks could not be read is absent and its record is kept.
#[must_use]
pub fn stale_registrations(
    registrations: &BTreeMap<String, u64>,
    advertised: Option<&BTreeSet<String>>,
    live_hook_ids: &BTreeMap<String, BTreeSet<u64>>,
) -> Vec<(String, u64, StaleRegistration)> {
    registrations
        .iter()
        .filter_map(|(repo, hook_id)| {
            let reason = if advertised.is_some_and(|advertised| !advertised.contains(repo)) {
                Some(StaleRegistration::Unadvertised)
            } else if live_hook_ids
                .get(repo)
                .is_some_and(|ids| !ids.contains(hook_id))
            {
                Some(StaleRegistration::HookGone)
            } else {
                None
            };
            reason.map(|reason| (repo.clone(), *hook_id, reason))
        })
        .collect()
}

/// Build the classifier's delivery facts from GitHub delivery records.
///
/// A record without a readable timestamp cannot contribute to a span and is
/// dropped; a record without a readable status code counts as unreachable,
/// matching the reconcile's decoding.
#[must_use]
pub fn delivery_facts(deliveries: &[serde_json::Value]) -> Vec<DeliveryFact> {
    deliveries
        .iter()
        .filter_map(|delivery| {
            let delivered_at = delivery
                .get("delivered_at")
                .and_then(serde_json::Value::as_str)
                .and_then(|text| DateTime::parse_from_rfc3339(text).ok())?
                .with_timezone(&Utc);
            let status_text = delivery
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let outcome = delivery
                .get("status_code")
                .and_then(serde_json::Value::as_u64)
                .and_then(|code| u16::try_from(code).ok())
                .map_or_else(
                    || DeliveryOutcome::Unreachable {
                        detail: "delivery record had no readable status code".to_owned(),
                    },
                    |code| DeliveryOutcome::classify(code, status_text),
                );
            Some(DeliveryFact {
                delivered_at,
                outcome,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    const EVENTS: [&str; 3] = ["check_run", "pull_request", "workflow_run"];

    fn events() -> Vec<String> {
        EVENTS.iter().map(|event| (*event).to_owned()).collect()
    }

    fn at(hours_ago: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-02T12:00:00Z")
            .expect("time")
            .with_timezone(&Utc)
            - Duration::hours(hours_ago)
    }

    fn failures(count: usize, every_hours: i64, code: u16) -> Vec<DeliveryFact> {
        (0..count)
            .map(|index| DeliveryFact {
                delivered_at: at(i64::try_from(index).expect("index") * every_hours),
                outcome: DeliveryOutcome::Rejected { status_code: code },
            })
            .collect()
    }

    fn hook(url: &str, deliveries: Option<Vec<DeliveryFact>>) -> HookFacts {
        HookFacts {
            id: 7,
            url: url.to_owned(),
            events: events(),
            deliveries,
        }
    }

    fn advertised(repos: &[&str]) -> BTreeSet<String> {
        repos.iter().map(|repo| (*repo).to_owned()).collect()
    }

    fn ctx(advertised: Option<&BTreeSet<String>>) -> HostContext<'_> {
        HostContext {
            this_host: Some("me.ts.net"),
            advertised,
            tailnet_nodes: None,
        }
    }

    #[test]
    fn callback_host_recognises_current_and_legacy_daemon_urls_only() {
        assert_eq!(
            daemon_callback_host("https://a.ts.net/webhook"),
            Some("a.ts.net")
        );
        assert_eq!(daemon_callback_host("https://a.ts.net"), Some("a.ts.net"));
        assert_eq!(daemon_callback_host("https://a.ts.net/"), Some("a.ts.net"));
        assert_eq!(
            daemon_callback_host("https://ci.example/hooks/github"),
            None
        );
        assert_eq!(daemon_callback_host("http://a.ts.net/webhook"), None);
    }

    #[test]
    fn daemon_events_accept_older_subsets_but_nothing_foreign() {
        let set = |events: &[&str]| {
            events
                .iter()
                .map(|event| (*event).to_owned())
                .collect::<Vec<_>>()
        };
        assert!(has_daemon_events(&set(&EVENTS), &EVENTS));
        assert!(has_daemon_events(
            &set(&["workflow_run", "check_run"]),
            &EVENTS
        ));
        assert!(!has_daemon_events(&set(&["check_run"]), &EVENTS));
        assert!(!has_daemon_events(&set(&["workflow_run", "push"]), &EVENTS));
        assert!(!has_daemon_events(&[], &EVENTS));
    }

    #[test]
    fn a_hook_with_other_events_or_url_is_never_touched() {
        let mut other = hook("https://gone.ts.net/webhook", Some(failures(40, 2, 502)));
        other.events = vec!["push".to_owned()];
        assert_eq!(
            classify_hook("o/r", &other, &ctx(None), &EVENTS),
            Verdict::Foreign
        );
        let foreign = hook("https://hooks.example/x", Some(failures(40, 2, 502)));
        assert_eq!(
            classify_hook("o/r", &foreign, &ctx(None), &EVENTS),
            Verdict::Foreign
        );
    }

    #[test]
    fn own_hook_is_pruned_only_when_the_running_daemon_stops_advertising_the_repo() {
        let own = hook("https://me.ts.net/webhook", None);
        let adv = advertised(&["o/r"]);
        assert_eq!(
            classify_hook("o/gone", &own, &ctx(Some(&adv)), &EVENTS),
            Verdict::Prune(PruneReason::OwnUnadvertised)
        );
        assert!(matches!(
            classify_hook("o/r", &own, &ctx(Some(&adv)), &EVENTS),
            Verdict::Keep(_)
        ));
        assert!(matches!(
            classify_hook("o/gone", &own, &ctx(None), &EVENTS),
            Verdict::Undecided(_)
        ));
    }

    #[test]
    fn peer_hook_with_sustained_gateway_failures_is_pruned() {
        let dead = hook("https://gone.ts.net/webhook", Some(failures(30, 2, 502)));
        assert_eq!(
            classify_hook("o/r", &dead, &ctx(None), &EVENTS),
            Verdict::Prune(PruneReason::DeadTarget {
                failures: 30,
                span_hours: 58,
            })
        );
    }

    #[test]
    fn peer_hook_is_kept_when_a_daemon_is_answering_or_recently_delivered() {
        // A wedged-then-restarted daemon: hours of 502s, then a success.
        let mut recovering = failures(30, 2, 502);
        recovering[0].outcome = DeliveryOutcome::Delivered;
        assert!(matches!(
            classify_hook(
                "o/r",
                &hook("https://peer.ts.net/webhook", Some(recovering)),
                &ctx(None),
                &EVENTS
            ),
            Verdict::Keep(_)
        ));
        // A daemon refusing deliveries is alive, however long it refuses.
        assert!(matches!(
            classify_hook(
                "o/r",
                &hook("https://peer.ts.net/webhook", Some(failures(30, 2, 400))),
                &ctx(None),
                &EVENTS
            ),
            Verdict::Keep(_)
        ));
    }

    #[test]
    fn short_or_sparse_failure_windows_cannot_prune_a_peer() {
        // Ten hours of 502s: a daemon down overnight, not a retired host.
        let brief = hook("https://peer.ts.net/webhook", Some(failures(100, 0, 502)));
        assert!(matches!(
            classify_hook("o/r", &brief, &ctx(None), &EVENTS),
            Verdict::Undecided(_)
        ));
        let sparse = hook("https://peer.ts.net/webhook", Some(failures(4, 48, 502)));
        assert!(matches!(
            classify_hook("o/r", &sparse, &ctx(None), &EVENTS),
            Verdict::Undecided(_)
        ));
        let unread = hook("https://peer.ts.net/webhook", None);
        assert!(matches!(
            classify_hook("o/r", &unread, &ctx(None), &EVENTS),
            Verdict::Undecided(_)
        ));
    }

    #[test]
    fn a_peer_that_left_the_tailnet_is_pruned_on_failure_count_alone() {
        let nodes = BTreeSet::from(["me.tail.ts.net".to_owned(), "peer.tail.ts.net".to_owned()]);
        let context = HostContext {
            this_host: Some("me.tail.ts.net"),
            advertised: None,
            tailnet_nodes: Some(&nodes),
        };
        // Eight hours of failures: too short for the span rule.
        let retired = hook(
            "https://old-name.tail.ts.net/webhook",
            Some(failures(5, 2, 502)),
        );
        assert_eq!(
            classify_hook("o/r", &retired, &context, &EVENTS),
            Verdict::Prune(PruneReason::RetiredHost { failures: 5 })
        );
        // A node still on the tailnet, offline or wedged, is not retired.
        let offline = hook(
            "https://peer.tail.ts.net/webhook",
            Some(failures(5, 2, 502)),
        );
        assert!(matches!(
            classify_hook("o/r", &offline, &context, &EVENTS),
            Verdict::Undecided(_)
        ));
        // A host on another domain is not a fleet host, so it is kept.
        let elsewhere = hook(
            "https://old-name.other.ts.net/webhook",
            Some(failures(5, 2, 502)),
        );
        assert!(matches!(
            classify_hook("o/r", &elsewhere, &context, &EVENTS),
            Verdict::Keep(_)
        ));
        // Too few failures still decides nothing.
        let thin = hook(
            "https://old-name.tail.ts.net/webhook",
            Some(failures(4, 2, 502)),
        );
        assert!(matches!(
            classify_hook("o/r", &thin, &context, &EVENTS),
            Verdict::Undecided(_)
        ));
    }

    #[test]
    fn a_receiver_outside_this_tailnet_is_never_pruned_however_dead() {
        // Daemon-shaped URL and events, 48h of gateway failures, but not a
        // fleet host: someone else's receiver.
        let foreign = hook(
            "https://hooks.example.com/webhook",
            Some(failures(100, 0, 502)),
        );
        let mut long = failures(100, 0, 502);
        for (index, delivery) in long.iter_mut().enumerate() {
            delivery.delivered_at = at(i64::try_from(index).expect("index") / 2);
        }
        let foreign_long = hook("https://hooks.example.com/webhook", Some(long));
        for candidate in [&foreign, &foreign_long] {
            assert!(
                matches!(
                    classify_hook("o/r", candidate, &ctx(None), &EVENTS),
                    Verdict::Keep(_)
                ),
                "{candidate:?}"
            );
        }
        let span = foreign_long
            .deliveries
            .as_ref()
            .map(|deliveries| {
                (deliveries[0].delivered_at - deliveries[deliveries.len() - 1].delivered_at)
                    .num_hours()
            })
            .expect("span");
        assert!(
            span >= 48,
            "the control window must exceed the threshold: {span}h"
        );
        // Without a readable identity, no peer can be judged at all.
        let unknown = HostContext::default();
        assert!(matches!(
            classify_hook(
                "o/r",
                &hook("https://gone.ts.net/webhook", Some(failures(30, 2, 502))),
                &unknown,
                &EVENTS
            ),
            Verdict::Undecided(_)
        ));
    }

    #[test]
    fn a_peer_github_gave_up_on_with_eof_for_a_day_is_pruned() {
        let records = (0..30)
            .map(|index| {
                serde_json::json!({
                    "delivered_at": at(index).to_rfc3339(),
                    "status_code": 500,
                    "status": "POST https://gone.ts.net/webhook giving up after 1 attempt(s): Post \"https://gone.ts.net/webhook\": EOF",
                })
            })
            .collect::<Vec<_>>();
        let dead = hook(
            "https://gone.ts.net/webhook",
            Some(delivery_facts(&records)),
        );
        assert_eq!(
            classify_hook("o/r", &dead, &ctx(None), &EVENTS),
            Verdict::Prune(PruneReason::DeadTarget {
                failures: 30,
                span_hours: 29,
            })
        );
    }

    #[test]
    fn registrations_drop_unadvertised_repos_and_vanished_hooks_only() {
        let registrations = BTreeMap::from([
            ("o/kept".to_owned(), 1),
            ("o/unadvertised".to_owned(), 2),
            ("o/vanished".to_owned(), 3),
            ("o/unread".to_owned(), 4),
        ]);
        let live = BTreeMap::from([
            ("o/kept".to_owned(), BTreeSet::from([1])),
            ("o/unadvertised".to_owned(), BTreeSet::from([2])),
            ("o/vanished".to_owned(), BTreeSet::from([99])),
        ]);
        let stale = stale_registrations(
            &registrations,
            Some(&advertised(&["o/kept", "o/vanished", "o/unread"])),
            &live,
        );
        assert_eq!(
            stale,
            vec![
                (
                    "o/unadvertised".to_owned(),
                    2,
                    StaleRegistration::Unadvertised
                ),
                ("o/vanished".to_owned(), 3, StaleRegistration::HookGone),
            ]
        );
    }

    #[test]
    fn delivery_records_decode_into_facts() {
        let records = serde_json::json!([
            {"delivered_at": "2026-10-02T06:32:07.755Z", "status_code": 502, "status": "Invalid HTTP Response: 502"},
            {"delivered_at": "2026-10-01T06:32:07Z", "status_code": 200, "status": "OK"},
            {"status_code": 200, "status": "OK"},
        ]);
        let facts = delivery_facts(records.as_array().expect("array"));
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[1].outcome, DeliveryOutcome::Delivered);
        assert_ne!(facts[0].outcome, DeliveryOutcome::Delivered);
    }
}
