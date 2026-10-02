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

/// Decides when failing probes mean the tunnel is lost.
#[derive(Clone, Debug, Default)]
pub struct IngressMonitor {
    verifies: u32,
    consecutive_failures: u32,
}

impl IngressMonitor {
    /// Whether this verification should run a probe. The first one always does.
    pub fn probe_due(&mut self) -> bool {
        let due = self.verifies.is_multiple_of(INGRESS_PROBE_EVERY_VERIFIES);
        self.verifies = self.verifies.wrapping_add(1);
        due
    }

    /// Record a probe; returns the status to publish and whether the tunnel
    /// should be treated as lost (and so re-applied). Inconclusive probes
    /// neither count nor reset the failure run.
    pub fn observe(&mut self, verdict: IngressVerdict, checked_at: f64) -> (IngressStatus, bool) {
        match verdict {
            IngressVerdict::Reachable => self.consecutive_failures = 0,
            IngressVerdict::Failing(_) => self.consecutive_failures += 1,
            IngressVerdict::Inconclusive(_) => {}
        }
        let lost = self.consecutive_failures >= INGRESS_FAILURES_BEFORE_LOST;
        let status = IngressStatus {
            verdict,
            checked_at,
            consecutive_failures: self.consecutive_failures,
        };
        if lost {
            // The tunnel is about to be re-applied; the next run starts fresh.
            self.consecutive_failures = 0;
            self.verifies = 0;
        }
        (status, lost)
    }
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
    fn two_failing_probes_in_a_row_lose_the_tunnel_and_unknowns_do_not() {
        let mut monitor = IngressMonitor::default();
        let failing = || IngressVerdict::Failing("TLS".to_owned());
        let unknown = || IngressVerdict::Inconclusive("probe timed out".to_owned());

        assert!(!monitor.observe(failing(), 1.0).1);
        // An inconclusive probe neither counts nor clears the run.
        let (status, lost) = monitor.observe(unknown(), 2.0);
        assert!(!lost);
        assert_eq!(status.consecutive_failures, 1);
        let (status, lost) = monitor.observe(failing(), 3.0);
        assert!(lost);
        assert_eq!(status.consecutive_failures, 2);
        assert_eq!(status.to_json()["state"], "failing");
        // After a re-apply the run starts again, and a reachable probe resets it.
        assert!(!monitor.observe(failing(), 4.0).1);
        assert!(!monitor.observe(IngressVerdict::Reachable, 5.0).1);
        assert!(!monitor.observe(failing(), 6.0).1);
        // Unknowns alone never lose the tunnel.
        for at in 0..10 {
            assert!(!monitor.observe(unknown(), f64::from(at)).1);
        }
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
