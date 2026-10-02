//! End-to-end check that the public internet can reach this daemon.
//!
//! `tailscale funnel status` naming the daemon's port proves only the local
//! half of the path. The public relays can still refuse this host's name (TLS
//! reset straight after the `ClientHello`), and GitHub then records every
//! delivery as `EOF` while the local check reports a healthy tunnel. So the
//! daemon periodically requests `/ingress-probe/<nonce>` from itself through
//! every public relay address its funnel host resolves to, and the listener
//! echoes the nonce back.
//!
//! Only a failure that proves the relay path is broken counts against the
//! tunnel: a TLS failure, an empty reply, a refused connection, or a gateway
//! status. Anything that may be this host's own outbound network (DNS lookup,
//! timeout, a missing `curl`) is inconclusive and never tears the tunnel down.

use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use wait_timeout::ChildExt;

/// Listener path prefix the probe requests; the remainder is the nonce.
pub const INGRESS_PROBE_PATH: &str = "/ingress-probe/";

/// Consecutive failing probes before the tunnel is treated as lost.
pub const INGRESS_FAILURES_BEFORE_LOST: u32 = 2;

/// Probe on every this-many tunnel verifications (30s apart: five minutes).
pub const INGRESS_PROBE_EVERY_VERIFIES: u32 = 10;

/// Minimum wait after each funnel re-apply before another may be forced, in
/// seconds. The last entry is the cap; [`INGRESS_MAX_REAPPLIES`] bounds the
/// count. A relay that never heals is re-applied a handful of times, then
/// only reported.
pub const INGRESS_REAPPLY_BACKOFF_SECS: [f64; 6] = [60.0, 300.0, 900.0, 3600.0, 3600.0, 3600.0];

/// Funnel re-applies forced per failing episode; a reachable probe resets it.
#[allow(clippy::cast_possible_truncation)] // A six-entry table.
pub const INGRESS_MAX_REAPPLIES: u32 = INGRESS_REAPPLY_BACKOFF_SECS.len() as u32;

const DNS_OVER_HTTPS_URL: &str = "https://1.1.1.1/dns-query";
const CURL_TIMEOUT: Duration = Duration::from_secs(15);

/// What one probe concluded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngressVerdict {
    /// Every relay delivered the request to a daemon.
    Reachable,
    /// At least one relay could not deliver to this host.
    Failing(String),
    /// The probe could not tell; nothing is concluded.
    Inconclusive(String),
}

impl IngressVerdict {
    /// Stable token for status output.
    #[must_use]
    pub const fn state(&self) -> &'static str {
        match self {
            Self::Reachable => "ok",
            Self::Failing(_) => "failing",
            Self::Inconclusive(_) => "inconclusive",
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::Reachable => "",
            Self::Failing(detail) | Self::Inconclusive(detail) => detail,
        }
    }
}

/// The last probe result, as `daemon status` reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct IngressStatus {
    /// Last verdict.
    pub verdict: IngressVerdict,
    /// When it was taken, Unix seconds.
    pub checked_at: f64,
    /// Failing probes in a row.
    pub consecutive_failures: u32,
    /// Funnel re-applies forced in the current failing episode.
    pub reapplies: u32,
    /// Earliest time another re-apply may be forced, when one is pending.
    pub next_reapply_at: Option<f64>,
}

impl IngressStatus {
    /// JSON shape for the daemon status frame.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "state": self.verdict.state(),
            "detail": self.verdict.detail(),
            "checked_at": self.checked_at,
            "consecutive_failures": self.consecutive_failures,
            "self_heal": {
                "reapplies": self.reapplies,
                "max_reapplies": INGRESS_MAX_REAPPLIES,
                "next_reapply_at": self.next_reapply_at,
                "exhausted": self.reapplies >= INGRESS_MAX_REAPPLIES,
            },
        })
    }
}

/// Classify one relay attempt from `curl`'s exit code, the HTTP status it
/// saw, and the body.
#[must_use]
pub fn classify_relay_attempt(
    curl_exit: i32,
    http_code: u16,
    body: &str,
    nonce: &str,
) -> IngressVerdict {
    match curl_exit {
        0 if (502..=504).contains(&http_code) => {
            IngressVerdict::Failing(format!("relay answered HTTP {http_code}"))
        }
        0 if http_code == 200 && body.trim() == nonce => IngressVerdict::Reachable,
        // Any other HTTP answer came from a daemon behind the relay (an older
        // daemon without the probe route answers 404 or 405): ingress works.
        0 if http_code != 0 => IngressVerdict::Reachable,
        0 => IngressVerdict::Inconclusive("no HTTP status".to_owned()),
        7 => IngressVerdict::Failing("relay refused the connection".to_owned()),
        35 => IngressVerdict::Failing("TLS handshake failed at the relay".to_owned()),
        52 => IngressVerdict::Failing("relay closed the connection with no reply".to_owned()),
        56 => IngressVerdict::Failing("relay reset the connection".to_owned()),
        6 => IngressVerdict::Inconclusive("could not resolve the funnel host".to_owned()),
        28 => IngressVerdict::Inconclusive("probe timed out".to_owned()),
        other => IngressVerdict::Inconclusive(format!("curl exited {other}")),
    }
}

/// Combine per-relay verdicts. GitHub may use any relay, so one failing relay
/// fails the probe; an inconclusive relay makes it inconclusive only when no
/// relay failed.
#[must_use]
pub fn combine_relay_verdicts(verdicts: &[(String, IngressVerdict)]) -> IngressVerdict {
    if verdicts.is_empty() {
        return IngressVerdict::Inconclusive("no public relay address resolved".to_owned());
    }
    let failing = verdicts
        .iter()
        .filter_map(|(relay, verdict)| match verdict {
            IngressVerdict::Failing(detail) => Some(format!("{relay}: {detail}")),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !failing.is_empty() {
        return IngressVerdict::Failing(failing.join("; "));
    }
    verdicts
        .iter()
        .find_map(|(relay, verdict)| match verdict {
            IngressVerdict::Inconclusive(detail) => {
                Some(IngressVerdict::Inconclusive(format!("{relay}: {detail}")))
            }
            _ => None,
        })
        .unwrap_or(IngressVerdict::Reachable)
}

/// IPv4 addresses from a DNS-over-HTTPS JSON answer.
#[must_use]
pub fn relay_addresses_from_doh(raw: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let mut addresses = value
        .get("Answer")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|answer| answer.get("type").and_then(Value::as_u64) == Some(1))
        .filter_map(|answer| answer.get("data").and_then(Value::as_str))
        .filter(|data| data.parse::<std::net::Ipv4Addr>().is_ok())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    addresses.sort();
    addresses.dedup();
    addresses
}

/// Decides when failing probes mean the tunnel should be re-applied.
#[derive(Clone, Debug, Default)]
pub struct IngressMonitor {
    verifies: u32,
    consecutive_failures: u32,
    reapplies: u32,
    next_reapply_at: Option<f64>,
}

impl IngressMonitor {
    /// Whether this verification should run a probe. The first one always does.
    pub fn probe_due(&mut self) -> bool {
        let due = self.verifies.is_multiple_of(INGRESS_PROBE_EVERY_VERIFIES);
        self.verifies = self.verifies.wrapping_add(1);
        due
    }

    /// Record a probe; returns the status to publish and whether the funnel
    /// should be torn down and re-applied now.
    ///
    /// A re-apply needs [`INGRESS_FAILURES_BEFORE_LOST`] failing probes in a
    /// row, the backoff since the previous re-apply to have elapsed, and fewer
    /// than [`INGRESS_MAX_REAPPLIES`] re-applies in this episode. Otherwise
    /// the failure is only reported. Inconclusive probes neither count nor
    /// reset; a reachable probe ends the episode.
    pub fn observe(&mut self, verdict: IngressVerdict, checked_at: f64) -> (IngressStatus, bool) {
        match verdict {
            IngressVerdict::Reachable => {
                self.consecutive_failures = 0;
                self.reapplies = 0;
                self.next_reapply_at = None;
            }
            IngressVerdict::Failing(_) => self.consecutive_failures += 1,
            IngressVerdict::Inconclusive(_) => {}
        }
        let reapply = self.consecutive_failures >= INGRESS_FAILURES_BEFORE_LOST
            && self.reapplies < INGRESS_MAX_REAPPLIES
            && self.next_reapply_at.is_none_or(|at| checked_at >= at);
        let status_failures = self.consecutive_failures;
        if reapply {
            let backoff = INGRESS_REAPPLY_BACKOFF_SECS[usize::try_from(self.reapplies)
                .unwrap_or(usize::MAX)
                .min(INGRESS_REAPPLY_BACKOFF_SECS.len() - 1)];
            self.reapplies += 1;
            self.next_reapply_at = Some(checked_at + backoff);
            // The funnel is about to be re-applied; probe straight after it.
            self.consecutive_failures = 0;
            self.verifies = 0;
        }
        let status = IngressStatus {
            verdict,
            checked_at,
            consecutive_failures: status_failures,
            reapplies: self.reapplies,
            next_reapply_at: self.next_reapply_at,
        };
        (status, reapply)
    }
}

/// Shared slot for the last public-ingress probe, read by `daemon status`.
pub type IngressReport = std::sync::Arc<std::sync::Mutex<Option<IngressStatus>>>;

/// The periodic self-check a tunnel backend runs from its `verify`, on the
/// tunnel supervisor's own thread: never on the webhook listener's.
#[derive(Clone, Debug)]
pub struct IngressCheck {
    /// Funnel host name, known once the tunnel is up.
    pub(crate) public_host: Option<String>,
    monitor: IngressMonitor,
    report: Option<IngressReport>,
    pub(crate) probe: fn(&str, &str) -> IngressVerdict,
    last_state: Option<&'static str>,
}

impl Default for IngressCheck {
    fn default() -> Self {
        Self {
            public_host: None,
            monitor: IngressMonitor::default(),
            report: None,
            probe: probe_public_ingress,
            last_state: None,
        }
    }
}

impl IngressCheck {
    /// A check that publishes each probe result to `report`.
    #[must_use]
    pub fn reporting_to(report: IngressReport) -> Self {
        Self {
            report: Some(report),
            ..Self::default()
        }
    }

    /// Record the public URL the tunnel came up on.
    pub fn set_public_url(&mut self, public_url: &str) {
        self.public_host = public_url
            .strip_prefix("https://")
            .map(|host| host.trim_end_matches('/').to_owned());
    }

    /// Probe when due. Returns false when the funnel should be re-applied;
    /// every probe result, including a failure that only gets reported, is
    /// published to the report.
    pub fn verify(&mut self, now: f64) -> bool {
        let Some(host) = self.public_host.clone() else {
            return true;
        };
        if !self.monitor.probe_due() {
            return true;
        }
        let nonce = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        );
        let verdict = (self.probe)(&host, &nonce);
        let (status, reapply) = self.monitor.observe(verdict, now);
        if let Some(line) = transition_line(self.last_state, &status, reapply) {
            let _ = crate::writer_domain_lease::write_stderr(format_args!("{line}"));
        }
        self.last_state = Some(status.verdict.state());
        if let Some(report) = &self.report
            && let Ok(mut slot) = report.lock()
        {
            *slot = Some(status);
        }
        !reapply
    }
}

/// The daemon-log line for a probe that changed the picture: the state moved,
/// or the funnel is about to be re-applied. Repeats of the same state stay
/// quiet; `daemon status` carries the current reading.
#[must_use]
pub fn transition_line(
    previous: Option<&'static str>,
    status: &IngressStatus,
    reapply: bool,
) -> Option<String> {
    let state = status.verdict.state();
    if reapply {
        return Some(format!(
            "shipyard daemon: public ingress failing ({}); re-applying the funnel (attempt {} of {INGRESS_MAX_REAPPLIES})",
            status.verdict.detail(),
            status.reapplies
        ));
    }
    if previous == Some(state) {
        return None;
    }
    Some(match state {
        "ok" => "shipyard daemon: public ingress reachable".to_owned(),
        "failing" => format!(
            "shipyard daemon: public ingress failing ({}); GitHub cannot reach this daemon, waits fall back to polling",
            status.verdict.detail()
        ),
        other => format!(
            "shipyard daemon: public ingress check {other} ({})",
            status.verdict.detail()
        ),
    })
}

/// Probe `host` through every public relay it resolves to.
#[must_use]
pub fn probe_public_ingress(host: &str, nonce: &str) -> IngressVerdict {
    let doh = match run_curl(&[
        "-sS",
        "-H",
        "accept: application/dns-json",
        &format!("{DNS_OVER_HTTPS_URL}?name={host}&type=A"),
    ]) {
        Ok((0, body)) => body,
        Ok((code, _)) => {
            return IngressVerdict::Inconclusive(format!("public DNS lookup failed (curl {code})"));
        }
        Err(error) => return IngressVerdict::Inconclusive(error),
    };
    let verdicts = relay_addresses_from_doh(&doh)
        .into_iter()
        .map(|relay| {
            let verdict = match run_curl(&[
                "-sS",
                "-o",
                "-",
                "-w",
                "\n%{http_code}",
                "--resolve",
                &format!("{host}:443:{relay}"),
                &format!("https://{host}{INGRESS_PROBE_PATH}{nonce}"),
            ]) {
                Ok((code, output)) => {
                    let (body, status) = output.rsplit_once('\n').unwrap_or(("", &output));
                    classify_relay_attempt(code, status.trim().parse().unwrap_or(0), body, nonce)
                }
                Err(error) => IngressVerdict::Inconclusive(error),
            };
            (relay, verdict)
        })
        .collect::<Vec<_>>();
    combine_relay_verdicts(&verdicts)
}

fn run_curl(args: &[&str]) -> Result<(i32, String), String> {
    let mut child = Command::new("curl")
        .args(["--max-time", "10"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("curl unavailable: {error}"))?;
    if child
        .wait_timeout(CURL_TIMEOUT)
        .map_err(|error| error.to_string())?
        .is_none()
    {
        let _ = child.kill();
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    Ok((
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "live network probe; run by hand"]
    fn live_probe() {
        for host in std::env::var("INGRESS_HOSTS")
            .unwrap_or_default()
            .split(',')
        {
            if !host.is_empty() {
                eprintln!("LIVE {host}: {:?}", probe_public_ingress(host, "abc123"));
            }
        }
    }

    #[test]
    fn relay_attempts_separate_a_broken_path_from_a_local_unknown() {
        let nonce = "n1";
        assert_eq!(
            classify_relay_attempt(0, 200, "n1\n", nonce),
            IngressVerdict::Reachable
        );
        // A daemon answering anything at all proves the path.
        assert_eq!(
            classify_relay_attempt(0, 405, "", nonce),
            IngressVerdict::Reachable
        );
        for (exit, code) in [(35, 0), (52, 0), (56, 0), (7, 0), (0, 502), (0, 504)] {
            assert!(
                matches!(
                    classify_relay_attempt(exit, code, "", nonce),
                    IngressVerdict::Failing(_)
                ),
                "exit {exit} http {code}"
            );
        }
        for exit in [6, 28, 99] {
            assert!(matches!(
                classify_relay_attempt(exit, 0, "", nonce),
                IngressVerdict::Inconclusive(_)
            ));
        }
    }

    #[test]
    fn one_failing_relay_fails_the_probe() {
        let verdicts = vec![
            ("208.111.35.209".to_owned(), IngressVerdict::Reachable),
            (
                "208.111.34.11".to_owned(),
                IngressVerdict::Failing("TLS handshake failed at the relay".to_owned()),
            ),
        ];
        assert_eq!(
            combine_relay_verdicts(&verdicts),
            IngressVerdict::Failing("208.111.34.11: TLS handshake failed at the relay".to_owned())
        );
        let unknown = vec![
            ("a".to_owned(), IngressVerdict::Reachable),
            (
                "b".to_owned(),
                IngressVerdict::Inconclusive("probe timed out".to_owned()),
            ),
        ];
        assert!(matches!(
            combine_relay_verdicts(&unknown),
            IngressVerdict::Inconclusive(_)
        ));
        assert!(matches!(
            combine_relay_verdicts(&[]),
            IngressVerdict::Inconclusive(_)
        ));
    }

    #[test]
    fn doh_answers_yield_sorted_ipv4_relays_only() {
        let raw = r#"{"Status":0,"Answer":[
            {"name":"h.","type":5,"data":"cname.example."},
            {"name":"h.","type":1,"data":"208.111.35.209"},
            {"name":"h.","type":1,"data":"208.111.34.11"},
            {"name":"h.","type":1,"data":"208.111.34.11"}
        ]}"#;
        assert_eq!(
            relay_addresses_from_doh(raw),
            vec!["208.111.34.11".to_owned(), "208.111.35.209".to_owned()]
        );
        assert!(relay_addresses_from_doh("not json").is_empty());
    }

    #[test]
    fn two_failing_probes_in_a_row_reapply_and_unknowns_do_not_count() {
        let mut monitor = IngressMonitor::default();
        let failing = || IngressVerdict::Failing("TLS".to_owned());
        let unknown = || IngressVerdict::Inconclusive("probe timed out".to_owned());

        assert!(!monitor.observe(failing(), 1.0).1);
        // An inconclusive probe neither counts nor clears the run.
        let (status, reapply) = monitor.observe(unknown(), 2.0);
        assert!(!reapply);
        assert_eq!(status.consecutive_failures, 1);
        let (status, reapply) = monitor.observe(failing(), 3.0);
        assert!(reapply);
        assert_eq!(status.consecutive_failures, 2);
        assert_eq!(status.to_json()["state"], "failing");
        assert_eq!(status.to_json()["self_heal"]["reapplies"], 1);
        // Unknowns alone never re-apply.
        let mut quiet = IngressMonitor::default();
        for at in 0..10 {
            assert!(!quiet.observe(unknown(), f64::from(at)).1);
        }
    }

    #[test]
    fn reapplies_back_off_cap_and_then_only_report() {
        let mut monitor = IngressMonitor::default();
        let failing = || IngressVerdict::Failing("TLS".to_owned());
        // Probe every 30s for two days of a relay that never heals.
        let mut reapply_times = Vec::new();
        let mut last = None;
        for tick in 0..5760 {
            let now = f64::from(tick) * 30.0;
            let (status, reapply) = monitor.observe(failing(), now);
            if reapply {
                reapply_times.push(now);
            }
            last = Some(status);
        }
        assert_eq!(
            u32::try_from(reapply_times.len()).expect("count"),
            INGRESS_MAX_REAPPLIES,
            "re-applies must stop at the cap: {reapply_times:?}"
        );
        let gaps = reapply_times
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .collect::<Vec<_>>();
        for (gap, backoff) in gaps.iter().zip(INGRESS_REAPPLY_BACKOFF_SECS) {
            assert!(
                *gap >= backoff,
                "gap {gap} under backoff {backoff}: {gaps:?}"
            );
        }
        assert!(
            gaps.windows(2).all(|pair| pair[1] >= pair[0]),
            "gaps must grow: {gaps:?}"
        );
        let last = last.expect("status").to_json();
        assert_eq!(
            last["state"], "failing",
            "a capped self-heal still reports failing"
        );
        assert_eq!(last["self_heal"]["exhausted"], true);

        // The first reachable probe ends the episode and restores the budget.
        let (status, _) = monitor.observe(IngressVerdict::Reachable, 200_000.0);
        assert_eq!(status.to_json()["self_heal"]["reapplies"], 0);
        assert!(!monitor.observe(failing(), 200_030.0).1);
        assert!(monitor.observe(failing(), 200_060.0).1);
    }

    #[test]
    fn transitions_and_reapplies_are_logged_and_repeats_are_not() {
        let mut monitor = IngressMonitor::default();
        let failing = || IngressVerdict::Failing("TLS handshake failed at the relay".to_owned());
        let (first, reapply) = monitor.observe(failing(), 1.0);
        let line = transition_line(Some("ok"), &first, reapply).expect("ok to failing");
        assert!(line.contains("public ingress failing (TLS"), "{line}");
        let (second, reapply) = monitor.observe(failing(), 2.0);
        let line = transition_line(Some("failing"), &second, reapply).expect("re-apply");
        assert!(
            line.contains("re-applying the funnel (attempt 1 of 6)"),
            "{line}"
        );
        let (third, reapply) = monitor.observe(failing(), 3.0);
        assert!(!reapply);
        assert_eq!(transition_line(Some("failing"), &third, reapply), None);
        let (healed, _) = monitor.observe(IngressVerdict::Reachable, 4.0);
        assert_eq!(
            transition_line(Some("failing"), &healed, false).as_deref(),
            Some("shipyard daemon: public ingress reachable")
        );
    }

    #[test]
    fn probes_run_on_the_first_verification_then_every_tenth() {
        let mut monitor = IngressMonitor::default();
        let due = (0..25).filter(|_| monitor.probe_due()).count();
        assert_eq!(due, 3);
        let mut fresh = IngressMonitor::default();
        assert!(fresh.probe_due());
        assert!(!fresh.probe_due());
    }
}
