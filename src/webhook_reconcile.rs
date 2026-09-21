//! Reconcile the webhook Shipyard INTENDS against the webhook GitHub actually HOLDS.
//!
//! # The pattern: desired vs observed, reconciled and alarmed
//!
//! A whole class of outages in this system share one shape. Something knows the
//! DESIRED state. Something else holds the OBSERVED state. Nobody compares them,
//! so the gap is invisible until a human trips over it:
//!
//! | desired                          | observed                              |
//! |----------------------------------|---------------------------------------|
//! | the daemon's own tunnel URL      | the callback URL GitHub has registered |
//! | labels a required job asks for   | labels runners actually advertise      |
//! | VMs cloned for a lane            | jobs that lane actually served         |
//! | pull requests queued             | pull requests actually merged          |
//!
//! Each side is individually healthy and individually truthful. `daemon status`
//! prints a correct tunnel URL; GitHub serves a correct hook record. Only the
//! comparison is missing, and a missing comparison has no symptom.
//!
//! Three rules make an instance of this pattern safe, and this module is written
//! to demonstrate all three:
//!
//! 1. **Compare, don't cache.** A local note that says "I registered URL X"
//!    records an intention, not a fact. Re-read the remote side on a schedule
//!    and compare; never let a cached success suppress the next comparison.
//! 2. **An unreadable side is never "no drift".** If the desired side cannot be
//!    determined (the identity source is unreachable) or the observed side
//!    cannot be fetched (permission denied, network down), the answer is an
//!    ALARM, never silence. Treating an unreadable input as an empty input is
//!    how a measurement turns into a false clean bill of health. Every entry
//!    point below fails loudly instead.
//! 3. **Name the remedy, and say who must perform it.** A drift this process can
//!    repair and a drift only a human can grant are different states with
//!    different exits. A permission that must be granted in a web UI must never
//!    be retried in a loop; it must be reported until a human acts.
//!
//! # The observed instance
//!
//! Tailscale re-registered a host under a collision suffix, so the node name the
//! tunnel published changed and the old name stopped resolving. The registered
//! hook kept the dead name, every delivery failed to connect, and nothing
//! noticed because nothing was subscribed to the feed. The daemon could not
//! repair it either: the GitHub App lacked `repository_hooks: write`, which
//! surfaced only as a repeating line in a log.
//!
//! # A trap encoded here on purpose
//!
//! `PATCH /repos/{owner}/{repo}/hooks/{id}` **replaces** the `config` object; it
//! does not merge into it. Patching only `config.url` therefore silently CLEARS
//! the shared secret, and the hook keeps working right up until signature
//! verification starts failing. Any repair must send the complete config and
//! then verify the secret survived — which is why [`FindingCode::SecretMissing`]
//! exists and why [`reconcile`] raises it at [`Severity::Alarm`].

use std::fmt;

/// Consecutive failed deliveries that constitute an alarm.
///
/// Chosen from measurement rather than taste:
///
/// * A structurally dead endpoint produces an unbroken run. The motivating
///   incident measured 15 of 15 recent deliveries failing with a connection
///   error, and such a run only grows.
/// * A healthy endpoint still produces short bursts of failure. A live host
///   observed while this was written returned scattered non-2xx responses whose
///   longest consecutive run was 2, interleaved with 200s throughout.
///
/// Five sits clear of the observed noise floor (2, with margin) and far below
/// the dead-host signature (15), so it separates the two without tuning. At the
/// event rate a busy repository generates, five consecutive failures accumulate
/// within seconds, so the alarm is prompt as well as specific. It also matches
/// the repeat threshold the external daemon-health watchdog already uses for a
/// wedged-registration signature, so the two layers cannot disagree about what
/// counts as "repeating" and flap against each other.
pub const CONSECUTIVE_FAILED_DELIVERY_ALARM: usize = 5;

/// GitHub App permission required to create or repair a repository webhook.
pub const WEBHOOK_WRITE_PERMISSION: &str = "repository_hooks: write";

/// GitHub App permission required merely to OBSERVE a repository webhook.
///
/// Distinct from [`WEBHOOK_WRITE_PERMISSION`]: without read access the
/// reconciler cannot even tell whether drift exists, which is a strictly worse
/// state than being unable to repair known drift.
pub const WEBHOOK_READ_PERMISSION: &str = "repository_hooks: read";

/// The host's own network identity, the DESIRED side's input.
///
/// [`HostIdentity::Unreadable`] is a first-class variant rather than an
/// `Option`, because an absent identity and an unreadable identity demand
/// opposite handling: the caller must never be able to pattern-match a failure
/// into the same arm as "nothing to do".
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostIdentity {
    /// Authoritative node name for this host, trailing dot already stripped.
    Known(String),
    /// The identity source could not be read. NOT equivalent to "unchanged".
    Unreadable {
        /// Non-secret explanation suitable for an operator-facing log line.
        detail: String,
    },
}

impl HostIdentity {
    /// Build a known identity from a raw node name, stripping the trailing dot
    /// that `tailscale status --json` includes in `Self.DNSName`.
    ///
    /// An empty or whitespace-only name is [`HostIdentity::Unreadable`], never
    /// a `Known("")` that would later render as a plausible-looking URL.
    #[must_use]
    pub fn from_node_name(raw: &str) -> Self {
        let trimmed = raw.trim().trim_end_matches('.').trim();
        if trimmed.is_empty() {
            Self::Unreadable {
                detail: "identity source returned an empty node name".to_owned(),
            }
        } else {
            Self::Known(trimmed.to_owned())
        }
    }

    /// The callback URL this host intends GitHub to hold, when readable.
    #[must_use]
    pub fn desired_callback_url(&self) -> Option<String> {
        match self {
            Self::Known(name) => Some(format!("https://{name}/webhook")),
            Self::Unreadable { .. } => None,
        }
    }
}

/// The webhook state this host intends GitHub to hold.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesiredWebhook {
    /// Full callback URL including path.
    pub callback_url: String,
    /// Event subscription, order-insensitive.
    pub events: Vec<String>,
}

impl DesiredWebhook {
    /// Build the desired state for a readable identity.
    #[must_use]
    pub fn for_identity(identity: &HostIdentity, events: &[&str]) -> Option<Self> {
        Some(Self {
            callback_url: identity.desired_callback_url()?,
            events: events.iter().map(|event| (*event).to_owned()).collect(),
        })
    }
}

/// Outcome of one webhook delivery attempt, classified by WHO answered.
///
/// The distinction is load-bearing. A non-2xx answer proves the endpoint is
/// reachable and speaking HTTP, so the fault is in the payload or the signature.
/// A connection error proves nothing answered at all, which is the signature of
/// a URL pointing at a host that no longer exists. Collapsing both into
/// "failed" would make a renamed host and a mis-signed payload indistinguishable
/// and send the operator to the wrong remedy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliveryOutcome {
    /// Endpoint answered 2xx.
    Delivered,
    /// Endpoint answered, but not with success. Reachable.
    Rejected {
        /// HTTP status the endpoint returned.
        status_code: u16,
    },
    /// Nothing answered: connection error, timeout, or DNS failure.
    Unreachable {
        /// Non-secret explanation from the delivery record.
        detail: String,
    },
}

impl DeliveryOutcome {
    /// Classify a GitHub delivery record's status code and status text.
    ///
    /// GitHub reports an unreachable endpoint as a synthesized `502` carrying a
    /// `connection_error`-style status string, which is why the status TEXT is
    /// consulted and not only the numeric code.
    #[must_use]
    pub fn classify(status_code: u16, status_text: &str) -> Self {
        let lowered = status_text.to_ascii_lowercase();
        let unreachable_text = lowered.contains("connection")
            || lowered.contains("timed out")
            || lowered.contains("timeout")
            || lowered.contains("could not resolve")
            || lowered.contains("failed to connect")
            || lowered.contains("no route");
        if status_code == 0 || unreachable_text {
            return Self::Unreachable {
                detail: if status_text.trim().is_empty() {
                    format!("status {status_code} with no response")
                } else {
                    status_text.trim().to_owned()
                },
            };
        }
        if (200..300).contains(&status_code) {
            Self::Delivered
        } else {
            Self::Rejected { status_code }
        }
    }

    /// Whether this attempt reached the endpoint at all.
    #[must_use]
    pub const fn reached_endpoint(&self) -> bool {
        !matches!(self, Self::Unreachable { .. })
    }

    /// Whether this attempt succeeded.
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        matches!(self, Self::Delivered)
    }
}

/// The webhook state GitHub actually holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedWebhook {
    /// GitHub webhook ID.
    pub hook_id: u64,
    /// Callback URL GitHub currently holds.
    pub callback_url: String,
    /// Whether GitHub will attempt deliveries.
    pub active: bool,
    /// Event subscription GitHub currently holds.
    pub events: Vec<String>,
    /// Whether a shared secret is configured.
    ///
    /// GitHub never returns the secret itself, only a fixed mask when one is
    /// set, so presence is the only observable fact — and the only one needed
    /// to detect a config-replacing PATCH having cleared it.
    pub secret_present: bool,
    /// Recent delivery attempts, newest first, as GitHub orders them.
    pub recent_deliveries: Vec<DeliveryOutcome>,
}

/// Why the observed side could not be read.
///
/// Every variant is an alarm. None of them may be folded into "no drift".
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservationFailure {
    /// The credential is not authorized to read or write repository hooks.
    PermissionDenied {
        /// The exact permission a human must grant, e.g. `repository_hooks: write`.
        permission: &'static str,
        /// Non-secret explanation from the API client.
        detail: String,
    },
    /// No hook is registered for this host, or the one local provenance names
    /// no longer exists on the remote.
    HookMissing {
        /// Hook ID recorded in local provenance, when there is one.
        hook_id: Option<u64>,
    },
    /// The remote could not be read for any other reason.
    Unreadable {
        /// Non-secret explanation from the API client.
        detail: String,
    },
}

/// How loudly a finding must be reported.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Severity {
    /// Desired and observed agree.
    Ok,
    /// A difference that does not stop deliveries.
    Warn,
    /// Deliveries are failing or will fail; this process should repair it.
    Alarm,
    /// Repair requires an action this process cannot perform.
    Blocked,
}

impl fmt::Display for Severity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Alarm => "alarm",
            Self::Blocked => "blocked",
        })
    }
}

/// Stable machine-readable finding identity.
///
/// Consumers outside this process (the external daemon-health watchdog, agents,
/// shell callers) branch on these codes, so they are part of the contract and
/// must not be renamed for style.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FindingCode {
    /// Desired and observed agree on every compared field.
    InSync,
    /// GitHub holds a different callback URL than this host intends.
    UrlDrift,
    /// GitHub holds no shared secret. The config-replacing PATCH trap.
    SecretMissing,
    /// GitHub holds the hook but will not deliver to it.
    HookInactive,
    /// GitHub holds a different event subscription.
    EventsDrift,
    /// Consecutive deliveries never reached the endpoint.
    EndpointUnreachable,
    /// Consecutive deliveries reached the endpoint and were refused.
    EndpointRejecting,
    /// This host's own identity could not be read.
    IdentityUnreadable,
    /// The remote hook state could not be read.
    ObservationUnreadable,
    /// A human must grant a permission before repair is possible.
    PermissionDenied,
    /// Local provenance names a hook the remote no longer has.
    HookMissing,
}

impl FindingCode {
    /// Stable lowercase token for logs, JSON, and shell branching.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InSync => "in_sync",
            Self::UrlDrift => "url_drift",
            Self::SecretMissing => "secret_missing",
            Self::HookInactive => "hook_inactive",
            Self::EventsDrift => "events_drift",
            Self::EndpointUnreachable => "endpoint_unreachable",
            Self::EndpointRejecting => "endpoint_rejecting",
            Self::IdentityUnreadable => "identity_unreadable",
            Self::ObservationUnreadable => "observation_unreadable",
            Self::PermissionDenied => "permission_denied",
            Self::HookMissing => "hook_missing",
        }
    }

    /// Whether this process can repair the condition on its own.
    ///
    /// A condition that is not self-repairable must be REPORTED, never retried:
    /// retrying a permission grant that only a human can perform is the thrash
    /// this module exists to prevent.
    #[must_use]
    pub const fn is_self_repairable(self) -> bool {
        matches!(
            self,
            Self::UrlDrift
                | Self::SecretMissing
                | Self::HookInactive
                | Self::EventsDrift
                | Self::HookMissing
        )
    }
}

/// One reconciliation result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Finding {
    /// Stable machine-readable identity.
    pub code: FindingCode,
    /// How loudly to report it.
    pub severity: Severity,
    /// One-line operator-facing description. Never contains a secret.
    pub summary: String,
    /// What to do about it, and by whom.
    pub remedy: String,
}

impl Finding {
    /// Build a finding. Public so callers that observe a comparison this module
    /// cannot perform itself (for example, a running daemon's advertised URL
    /// against this host's identity) report through the same channel and the
    /// same severity ladder instead of inventing a parallel one.
    #[must_use]
    pub fn new(code: FindingCode, severity: Severity, summary: String, remedy: String) -> Self {
        Self {
            code,
            severity,
            summary,
            remedy,
        }
    }
}

/// The verdict of one reconciliation pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconcileReport {
    /// Every finding, most severe first.
    pub findings: Vec<Finding>,
}

impl ReconcileReport {
    /// Highest severity present. An empty report is impossible: [`reconcile`]
    /// always emits at least one finding, so there is no silent outcome.
    #[must_use]
    pub fn severity(&self) -> Severity {
        self.findings
            .iter()
            .map(|finding| finding.severity)
            .max()
            .unwrap_or(Severity::Alarm)
    }

    /// Whether every compared field agreed and deliveries look healthy.
    #[must_use]
    pub fn is_in_sync(&self) -> bool {
        self.severity() == Severity::Ok
    }

    /// Whether this process should attempt a repair now.
    ///
    /// False when anything is [`Severity::Blocked`]: repairing under a denied
    /// permission cannot succeed, and attempting it is the retry loop that
    /// masked the original incident.
    #[must_use]
    pub fn should_self_heal(&self) -> bool {
        if self
            .findings
            .iter()
            .any(|finding| finding.severity == Severity::Blocked)
        {
            return false;
        }
        self.findings
            .iter()
            .any(|finding| finding.code.is_self_repairable())
    }

    /// Whether the report contains a given code.
    #[must_use]
    pub fn contains(&self, code: FindingCode) -> bool {
        self.findings.iter().any(|finding| finding.code == code)
    }

    /// Process exit code: 0 ok, 1 warn, 2 alarm, 3 blocked on a human.
    #[must_use]
    pub const fn exit_code(severity: Severity) -> i32 {
        match severity {
            Severity::Ok => 0,
            Severity::Warn => 1,
            Severity::Alarm => 2,
            Severity::Blocked => 3,
        }
    }
}

/// Length of the run of consecutive failures at the head of a newest-first
/// delivery list, counting only attempts that never reached the endpoint.
#[must_use]
pub fn leading_unreachable_run(deliveries: &[DeliveryOutcome]) -> usize {
    deliveries
        .iter()
        .take_while(|outcome| !outcome.reached_endpoint())
        .count()
}

/// Length of the run of consecutive endpoint refusals at the head of a
/// newest-first delivery list.
#[must_use]
pub fn leading_rejected_run(deliveries: &[DeliveryOutcome]) -> usize {
    deliveries
        .iter()
        .take_while(|outcome| matches!(outcome, DeliveryOutcome::Rejected { .. }))
        .count()
}

/// Compare the webhook this host intends against the one GitHub holds.
///
/// Emits at least one finding in every case. There is deliberately no code path
/// that returns an empty report, because an empty report reads as "checked, all
/// clear" and the whole point of this module is that an unperformed check must
/// never be able to impersonate a passed one.
#[must_use]
pub fn reconcile(
    identity: &HostIdentity,
    desired: Option<&DesiredWebhook>,
    observation: Result<&ObservedWebhook, &ObservationFailure>,
    consecutive_failure_alarm: usize,
) -> ReconcileReport {
    let mut findings = Vec::new();

    // The desired side. An unreadable identity is an ALARM, never silence: it
    // is exactly the reading that a naive probe records as "no drift found".
    if let HostIdentity::Unreadable { detail } = identity {
        findings.push(Finding::new(
            FindingCode::IdentityUnreadable,
            Severity::Alarm,
            format!("this host's own network identity could not be read: {detail}"),
            "Resolve the identity source before trusting any drift verdict. An \
             unreadable identity is not evidence of agreement; until it is \
             readable this host cannot know which callback URL it should own."
                .to_owned(),
        ));
    }

    // The observed side. Every failure mode is reported; none is swallowed.
    let observed = match observation {
        Ok(observed) => Some(observed),
        Err(ObservationFailure::PermissionDenied { permission, detail }) => {
            findings.push(Finding::new(
                FindingCode::PermissionDenied,
                Severity::Blocked,
                format!(
                    "GitHub refused the webhook request: {}. Missing permission: {permission}",
                    detail.trim()
                ),
                format!(
                    "A human must grant `{permission}` to the GitHub App installation \
                     in the App's settings, then reinstall or accept the updated \
                     permissions on the target repositories. This process cannot \
                     grant it and must not retry until it is granted."
                ),
            ));
            None
        }
        Err(ObservationFailure::HookMissing { hook_id }) => {
            findings.push(Finding::new(
                FindingCode::HookMissing,
                Severity::Alarm,
                hook_id.map_or_else(
                    || "no webhook is registered for this repository".to_owned(),
                    |hook_id| {
                        format!("local provenance names hook {hook_id}, which GitHub no longer has")
                    },
                ),
                "Re-register the webhook; any stale local binding should be \
                 dropped and a fresh hook created."
                    .to_owned(),
            ));
            None
        }
        Err(ObservationFailure::Unreadable { detail }) => {
            findings.push(Finding::new(
                FindingCode::ObservationUnreadable,
                Severity::Alarm,
                format!(
                    "the registered webhook state could not be read: {}",
                    detail.trim()
                ),
                "Retry once the remote is reachable. Until the remote state is \
                 readable, drift is UNKNOWN rather than absent."
                    .to_owned(),
            ));
            None
        }
    };

    // Only when BOTH sides are readable can agreement be asserted.
    if let (Some(desired), Some(observed)) = (desired, observed) {
        findings.extend(field_findings(desired, observed));
    }

    // Delivery health is evaluated whenever the remote state is readable, even
    // if the URL agrees: a URL can be correct while the endpoint is dead behind
    // it, and a secret can be revoked without any field changing.
    if let Some(observed) = observed {
        findings.extend(delivery_findings(observed, consecutive_failure_alarm));
    }

    if findings.is_empty() {
        findings.push(Finding::new(
            FindingCode::InSync,
            Severity::Ok,
            "the registered webhook matches this host's intent".to_owned(),
            "No action required.".to_owned(),
        ));
    }

    findings.sort_by_key(|finding| std::cmp::Reverse(finding.severity));
    ReconcileReport { findings }
}

/// Compare the fields both sides declare.
fn field_findings(desired: &DesiredWebhook, observed: &ObservedWebhook) -> Vec<Finding> {
    let mut findings = Vec::new();
    {
        if desired.callback_url != observed.callback_url {
            findings.push(Finding::new(
                FindingCode::UrlDrift,
                Severity::Alarm,
                format!(
                    "GitHub holds callback URL {} but this host intends {}",
                    observed.callback_url, desired.callback_url
                ),
                "Repair the hook with the COMPLETE config object (url, \
                 content_type, insecure_ssl and secret together): a PATCH \
                 replaces the config rather than merging into it, so a \
                 url-only patch clears the shared secret."
                    .to_owned(),
            ));
        }

        if !observed.secret_present {
            findings.push(Finding::new(
                FindingCode::SecretMissing,
                Severity::Alarm,
                format!(
                    "GitHub hook {} has no shared secret configured",
                    observed.hook_id
                ),
                "Re-send the complete config including the secret. A config \
                 object PATCHed without the secret field clears it silently, \
                 and an unsigned feed cannot be authenticated."
                    .to_owned(),
            ));
        }

        if !observed.active {
            findings.push(Finding::new(
                FindingCode::HookInactive,
                Severity::Alarm,
                format!("GitHub hook {} is inactive", observed.hook_id),
                "Re-send the hook with `active: true`.".to_owned(),
            ));
        }

        let mut desired_events = desired.events.clone();
        desired_events.sort_unstable();
        desired_events.dedup();
        let mut observed_events = observed.events.clone();
        observed_events.sort_unstable();
        observed_events.dedup();
        if desired_events != observed_events {
            findings.push(Finding::new(
                FindingCode::EventsDrift,
                Severity::Warn,
                format!(
                    "GitHub hook {} subscribes to {observed_events:?}; this host intends {desired_events:?}",
                    observed.hook_id
                ),
                "Re-send the hook with the intended event list.".to_owned(),
            ));
        }
    }
    findings
}

/// Judge whether deliveries are actually arriving.
fn delivery_findings(observed: &ObservedWebhook, consecutive_failure_alarm: usize) -> Vec<Finding> {
    let mut findings = Vec::new();
    {
        let unreachable = leading_unreachable_run(&observed.recent_deliveries);
        if unreachable >= consecutive_failure_alarm {
            findings.push(Finding::new(
                FindingCode::EndpointUnreachable,
                Severity::Alarm,
                format!(
                    "the last {unreachable} deliveries to hook {} never reached the endpoint",
                    observed.hook_id
                ),
                "Nothing is answering at the registered URL. Check that the \
                 registered URL still names a live host, that the tunnel is up, \
                 and that nothing filters inbound requests."
                    .to_owned(),
            ));
        }

        let rejected = leading_rejected_run(&observed.recent_deliveries);
        if rejected >= consecutive_failure_alarm {
            findings.push(Finding::new(
                FindingCode::EndpointRejecting,
                Severity::Alarm,
                format!(
                    "the last {rejected} deliveries to hook {} reached the endpoint and were refused",
                    observed.hook_id
                ),
                "The endpoint is alive and rejecting. A sustained 401 means the \
                 shared secret GitHub signs with no longer matches the one the \
                 endpoint verifies; re-send the complete config including the \
                 secret."
                    .to_owned(),
            ));
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::{
        CONSECUTIVE_FAILED_DELIVERY_ALARM, DeliveryOutcome, DesiredWebhook, Finding, FindingCode,
        HostIdentity, ObservationFailure, ObservedWebhook, ReconcileReport, Severity,
        WEBHOOK_WRITE_PERMISSION, leading_rejected_run, leading_unreachable_run, reconcile,
    };

    const EVENTS: [&str; 2] = ["workflow_job", "workflow_run"];

    fn desired(url: &str) -> DesiredWebhook {
        DesiredWebhook {
            callback_url: url.to_owned(),
            events: EVENTS.iter().map(|event| (*event).to_owned()).collect(),
        }
    }

    fn healthy_observed(url: &str) -> ObservedWebhook {
        ObservedWebhook {
            hook_id: 667_647_843,
            callback_url: url.to_owned(),
            active: true,
            events: EVENTS.iter().map(|event| (*event).to_owned()).collect(),
            secret_present: true,
            recent_deliveries: vec![DeliveryOutcome::Delivered; 10],
        }
    }

    fn unreachable(times: usize) -> Vec<DeliveryOutcome> {
        vec![
            DeliveryOutcome::Unreachable {
                detail: "connection_error".to_owned(),
            };
            times
        ]
    }

    fn codes(report: &ReconcileReport) -> Vec<FindingCode> {
        report.findings.iter().map(|finding| finding.code).collect()
    }

    // --- the control -------------------------------------------------------
    //
    // Every negative assertion below ("no drift was reported") is only
    // meaningful if this same call can report drift at all. This is that proof.

    #[test]
    fn agreeing_sides_are_in_sync_and_drifting_sides_are_not() {
        let identity = HostIdentity::from_node_name("daniels-mac-studio-3.taile2001.ts.net.");
        let url = "https://daniels-mac-studio-3.taile2001.ts.net/webhook";
        let desired = desired(url);

        let agreeing = reconcile(
            &identity,
            Some(&desired),
            Ok(&healthy_observed(url)),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        assert!(
            agreeing.is_in_sync(),
            "control: agreeing sides must be in sync"
        );
        assert_eq!(codes(&agreeing), vec![FindingCode::InSync]);

        let stale = healthy_observed("https://daniels-mac-studio.taile2001.ts.net/webhook");
        let drifting = reconcile(
            &identity,
            Some(&desired),
            Ok(&stale),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        assert!(drifting.contains(FindingCode::UrlDrift));
        assert!(!drifting.is_in_sync());
        assert_eq!(drifting.severity(), Severity::Alarm);
        assert!(drifting.should_self_heal());
    }

    // --- the bug class that must never recur -------------------------------

    #[test]
    fn an_unreadable_identity_never_renders_as_no_drift() {
        // The cautionary shape: a probe returns nothing, and nothing is
        // recorded as agreement. An unreadable identity must ALARM.
        let identity = HostIdentity::Unreadable {
            detail: "tailscale binary not found on PATH".to_owned(),
        };
        let report = reconcile(
            &identity,
            None,
            Ok(&healthy_observed(
                "https://daniels-mac-studio-3.taile2001.ts.net/webhook",
            )),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );

        assert!(report.contains(FindingCode::IdentityUnreadable));
        assert!(
            !report.contains(FindingCode::InSync),
            "an unreadable identity must never produce an in-sync finding"
        );
        assert!(!report.is_in_sync());
        assert_eq!(report.severity(), Severity::Alarm);
    }

    #[test]
    fn an_empty_node_name_is_unreadable_not_a_valid_identity() {
        // The exact reading a mis-resolved binary produces: empty output.
        for raw in ["", "   ", ".", " . "] {
            let identity = HostIdentity::from_node_name(raw);
            assert!(
                matches!(identity, HostIdentity::Unreadable { .. }),
                "empty node name {raw:?} must not become a Known identity"
            );
            assert_eq!(identity.desired_callback_url(), None);
        }
        // Control: a real name on the same code path resolves and strips the
        // trailing dot that `tailscale status --json` includes.
        let identity = HostIdentity::from_node_name("daniels-mac-studio-3.taile2001.ts.net.");
        assert_eq!(
            identity.desired_callback_url().as_deref(),
            Some("https://daniels-mac-studio-3.taile2001.ts.net/webhook")
        );
    }

    #[test]
    fn an_unreadable_observation_never_renders_as_no_drift() {
        let identity = HostIdentity::from_node_name("daniels-mac-studio-3.taile2001.ts.net");
        let url = "https://daniels-mac-studio-3.taile2001.ts.net/webhook";
        let failure = ObservationFailure::Unreadable {
            detail: "gh api timed out".to_owned(),
        };
        let report = reconcile(
            &identity,
            Some(&desired(url)),
            Err(&failure),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );

        assert!(report.contains(FindingCode::ObservationUnreadable));
        assert!(
            !report.contains(FindingCode::InSync),
            "an unreadable remote must never produce an in-sync finding"
        );
        assert_eq!(report.severity(), Severity::Alarm);
    }

    // --- permission denied is a distinct, terminal, human-owned state ------

    #[test]
    fn permission_denied_blocks_and_names_the_permission_instead_of_retrying() {
        let identity = HostIdentity::from_node_name("daniels-mac-studio-3.taile2001.ts.net");
        let url = "https://daniels-mac-studio-3.taile2001.ts.net/webhook";
        let failure = ObservationFailure::PermissionDenied {
            permission: WEBHOOK_WRITE_PERMISSION,
            detail: "403 Resource not accessible by integration".to_owned(),
        };
        let report = reconcile(
            &identity,
            Some(&desired(url)),
            Err(&failure),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );

        assert!(report.contains(FindingCode::PermissionDenied));
        assert_eq!(report.severity(), Severity::Blocked);
        assert_eq!(ReconcileReport::exit_code(report.severity()), 3);
        assert!(
            !report.should_self_heal(),
            "a denied permission must not trigger a repair loop"
        );
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.code == FindingCode::PermissionDenied)
            .expect("permission finding");
        assert!(
            finding.summary.contains("repository_hooks: write"),
            "the missing permission must be named verbatim, got: {}",
            finding.summary
        );
        assert!(finding.remedy.contains("human"));
    }

    #[test]
    fn a_missing_remote_hook_is_repairable_but_not_silent() {
        let identity = HostIdentity::from_node_name("daniels-mac-studio-3.taile2001.ts.net");
        let url = "https://daniels-mac-studio-3.taile2001.ts.net/webhook";
        let failure = ObservationFailure::HookMissing { hook_id: Some(42) };
        let report = reconcile(
            &identity,
            Some(&desired(url)),
            Err(&failure),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        assert!(report.contains(FindingCode::HookMissing));
        assert!(!report.contains(FindingCode::InSync));
        assert!(report.should_self_heal());

        // An absent registration is reported too, with no hook id to name.
        let absent = ObservationFailure::HookMissing { hook_id: None };
        let report = reconcile(
            &identity,
            Some(&desired(url)),
            Err(&absent),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        assert!(report.contains(FindingCode::HookMissing));
        assert!(!report.is_in_sync());
    }

    // --- the config-replacing PATCH trap -----------------------------------

    #[test]
    fn a_cleared_secret_alarms_even_when_the_url_is_correct() {
        // PATCH /hooks/{id} REPLACES config. A url-only patch leaves a hook
        // whose URL is perfect and whose secret is gone; every other field
        // agrees, so only an explicit secret check can see it.
        let identity = HostIdentity::from_node_name("daniels-mac-studio-3.taile2001.ts.net");
        let url = "https://daniels-mac-studio-3.taile2001.ts.net/webhook";
        let mut observed = healthy_observed(url);
        observed.secret_present = false;

        let report = reconcile(
            &identity,
            Some(&desired(url)),
            Ok(&observed),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        assert!(report.contains(FindingCode::SecretMissing));
        assert!(!report.contains(FindingCode::UrlDrift), "the URL agrees");
        assert_eq!(report.severity(), Severity::Alarm);
        assert!(report.should_self_heal());
    }

    #[test]
    fn an_inactive_or_mis_subscribed_hook_is_reported() {
        let identity = HostIdentity::from_node_name("daniels-mac-studio-3.taile2001.ts.net");
        let url = "https://daniels-mac-studio-3.taile2001.ts.net/webhook";

        let mut inactive = healthy_observed(url);
        inactive.active = false;
        let report = reconcile(
            &identity,
            Some(&desired(url)),
            Ok(&inactive),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        assert!(report.contains(FindingCode::HookInactive));

        let mut narrowed = healthy_observed(url);
        narrowed.events = vec!["workflow_job".to_owned()];
        let report = reconcile(
            &identity,
            Some(&desired(url)),
            Ok(&narrowed),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        assert!(report.contains(FindingCode::EventsDrift));
        assert_eq!(report.severity(), Severity::Warn);

        // Control: event ORDER must not be mistaken for drift.
        let mut reordered = healthy_observed(url);
        reordered.events = vec!["workflow_run".to_owned(), "workflow_job".to_owned()];
        let report = reconcile(
            &identity,
            Some(&desired(url)),
            Ok(&reordered),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        assert!(!report.contains(FindingCode::EventsDrift));
        assert!(report.is_in_sync());
    }

    // --- delivery health ---------------------------------------------------

    #[test]
    fn delivery_classification_separates_unreachable_from_refused() {
        assert_eq!(
            DeliveryOutcome::classify(200, "OK"),
            DeliveryOutcome::Delivered
        );
        assert_eq!(
            DeliveryOutcome::classify(400, "Invalid HTTP Response: 400"),
            DeliveryOutcome::Rejected { status_code: 400 }
        );
        assert_eq!(
            DeliveryOutcome::classify(401, "Unauthorized"),
            DeliveryOutcome::Rejected { status_code: 401 }
        );
        // The measured signature of a host that no longer exists: GitHub
        // synthesizes a 502 and puts the real cause in the status text.
        let dead = DeliveryOutcome::classify(502, "connection_error");
        assert!(!dead.reached_endpoint());
        assert!(matches!(dead, DeliveryOutcome::Unreachable { .. }));
        assert!(!DeliveryOutcome::classify(0, "").reached_endpoint());
        // Control: a plain 502 FROM the endpoint is a refusal, not a rename.
        assert!(DeliveryOutcome::classify(502, "Bad Gateway").reached_endpoint());
    }

    #[test]
    fn the_alarm_threshold_separates_measured_noise_from_a_measured_dead_host() {
        let identity = HostIdentity::from_node_name("daniels-mac-studio-3.taile2001.ts.net");
        let url = "https://daniels-mac-studio-3.taile2001.ts.net/webhook";

        // Transcribed from one page of deliveries on a LIVE, working host:
        // eight refusals scattered through twenty attempts, longest
        // consecutive run 2. A rule that counted total failures rather than
        // the CURRENT run would alarm here (8 >= 5) and call a healthy host
        // dead, so this fixture is deliberately large enough to tell the two
        // rules apart.
        let ok = DeliveryOutcome::Delivered;
        let bad = DeliveryOutcome::Rejected { status_code: 400 };
        let mut noisy = healthy_observed(url);
        noisy.recent_deliveries = vec![
            ok.clone(),
            ok.clone(),
            bad.clone(),
            bad.clone(),
            ok.clone(),
            ok.clone(),
            ok.clone(),
            bad.clone(),
            bad.clone(),
            ok.clone(),
            ok.clone(),
            bad.clone(),
            ok.clone(),
            bad.clone(),
            bad.clone(),
            ok.clone(),
            ok.clone(),
            ok.clone(),
            bad.clone(),
            ok.clone(),
        ];
        assert_eq!(
            noisy
                .recent_deliveries
                .iter()
                .filter(|outcome| !outcome.succeeded())
                .count(),
            8,
            "fixture must carry enough total failures to trip a total-count rule"
        );
        assert_eq!(
            leading_rejected_run(&noisy.recent_deliveries),
            0,
            "and must currently be delivering"
        );
        let report = reconcile(
            &identity,
            Some(&desired(url)),
            Ok(&noisy),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        assert!(
            report.is_in_sync(),
            "intermittent failure on a live host must not alarm: {:?}",
            codes(&report)
        );

        // Measured on the DEAD host: an unbroken run of connection errors.
        let mut dead = healthy_observed(url);
        dead.recent_deliveries = unreachable(15);
        let report = reconcile(
            &identity,
            Some(&desired(url)),
            Ok(&dead),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        assert!(report.contains(FindingCode::EndpointUnreachable));
        assert_eq!(report.severity(), Severity::Alarm);
    }

    #[test]
    fn the_threshold_is_exact_at_its_boundary() {
        let identity = HostIdentity::from_node_name("daniels-mac-studio-3.taile2001.ts.net");
        let url = "https://daniels-mac-studio-3.taile2001.ts.net/webhook";

        let mut below = healthy_observed(url);
        below.recent_deliveries = unreachable(CONSECUTIVE_FAILED_DELIVERY_ALARM - 1);
        below.recent_deliveries.push(DeliveryOutcome::Delivered);
        let report = reconcile(
            &identity,
            Some(&desired(url)),
            Ok(&below),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        assert!(!report.contains(FindingCode::EndpointUnreachable));

        let mut at = healthy_observed(url);
        at.recent_deliveries = unreachable(CONSECUTIVE_FAILED_DELIVERY_ALARM);
        at.recent_deliveries.push(DeliveryOutcome::Delivered);
        let report = reconcile(
            &identity,
            Some(&desired(url)),
            Ok(&at),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        assert!(report.contains(FindingCode::EndpointUnreachable));
    }

    #[test]
    fn a_sustained_refusal_alarms_separately_from_unreachability() {
        // A revoked secret: the endpoint is alive and rejecting every signature.
        let identity = HostIdentity::from_node_name("daniels-mac-studio-3.taile2001.ts.net");
        let url = "https://daniels-mac-studio-3.taile2001.ts.net/webhook";
        let mut refusing = healthy_observed(url);
        refusing.recent_deliveries =
            vec![DeliveryOutcome::Rejected { status_code: 401 }; CONSECUTIVE_FAILED_DELIVERY_ALARM];

        let report = reconcile(
            &identity,
            Some(&desired(url)),
            Ok(&refusing),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        assert!(report.contains(FindingCode::EndpointRejecting));
        assert!(
            !report.contains(FindingCode::EndpointUnreachable),
            "a refusal proves the endpoint is reachable and must not read as a dead host"
        );
    }

    #[test]
    fn run_length_helpers_count_only_the_leading_run() {
        let deliveries = vec![
            DeliveryOutcome::Unreachable {
                detail: "connection_error".to_owned(),
            },
            DeliveryOutcome::Unreachable {
                detail: "connection_error".to_owned(),
            },
            DeliveryOutcome::Delivered,
            DeliveryOutcome::Unreachable {
                detail: "connection_error".to_owned(),
            },
        ];
        assert_eq!(leading_unreachable_run(&deliveries), 2);
        assert_eq!(leading_rejected_run(&deliveries), 0);
        assert_eq!(leading_unreachable_run(&[]), 0);

        // The helpers measure the CURRENT run, not a total. A history with
        // many failures that is delivering again right now must read as zero,
        // which is what separates "is broken" from "has ever been broken".
        let dead = DeliveryOutcome::Unreachable {
            detail: "connection_error".to_owned(),
        };
        let refused = DeliveryOutcome::Rejected { status_code: 401 };
        let recovered = vec![
            DeliveryOutcome::Delivered,
            dead.clone(),
            refused.clone(),
            dead.clone(),
            refused.clone(),
            dead.clone(),
            refused.clone(),
            dead.clone(),
            refused.clone(),
            dead.clone(),
            refused.clone(),
        ];
        assert_eq!(leading_unreachable_run(&recovered), 0);
        assert_eq!(leading_rejected_run(&recovered), 0);
    }

    // --- report mechanics --------------------------------------------------

    #[test]
    fn a_reconcile_pass_is_never_silent_and_orders_by_severity() {
        let identity = HostIdentity::Unreadable {
            detail: "unreadable".to_owned(),
        };
        let failure = ObservationFailure::PermissionDenied {
            permission: WEBHOOK_WRITE_PERMISSION,
            detail: "403".to_owned(),
        };
        let report = reconcile(
            &identity,
            None,
            Err(&failure),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        assert!(
            !report.findings.is_empty(),
            "a pass must always say something"
        );
        assert_eq!(report.findings[0].severity, Severity::Blocked);
        assert_eq!(report.severity(), Severity::Blocked);
    }

    #[test]
    fn exit_codes_are_stable_for_shell_callers() {
        assert_eq!(ReconcileReport::exit_code(Severity::Ok), 0);
        assert_eq!(ReconcileReport::exit_code(Severity::Warn), 1);
        assert_eq!(ReconcileReport::exit_code(Severity::Alarm), 2);
        assert_eq!(ReconcileReport::exit_code(Severity::Blocked), 3);
    }

    #[test]
    fn finding_codes_have_stable_distinct_tokens() {
        let all = [
            FindingCode::InSync,
            FindingCode::UrlDrift,
            FindingCode::SecretMissing,
            FindingCode::HookInactive,
            FindingCode::EventsDrift,
            FindingCode::EndpointUnreachable,
            FindingCode::EndpointRejecting,
            FindingCode::IdentityUnreadable,
            FindingCode::ObservationUnreadable,
            FindingCode::PermissionDenied,
            FindingCode::HookMissing,
        ];
        let mut tokens = all.iter().map(|code| code.as_str()).collect::<Vec<_>>();
        tokens.sort_unstable();
        let count = tokens.len();
        tokens.dedup();
        assert_eq!(tokens.len(), count, "finding codes must be distinct");
        assert!(!FindingCode::PermissionDenied.is_self_repairable());
        assert!(!FindingCode::IdentityUnreadable.is_self_repairable());
        assert!(FindingCode::UrlDrift.is_self_repairable());
    }

    #[test]
    fn findings_never_carry_a_secret_value() {
        // A reconciler that prints what it compares would leak the shared
        // secret into logs. Only PRESENCE is ever observed or rendered.
        let identity = HostIdentity::from_node_name("daniels-mac-studio-3.taile2001.ts.net");
        let url = "https://daniels-mac-studio-3.taile2001.ts.net/webhook";
        let mut observed = healthy_observed(url);
        observed.secret_present = false;
        let report = reconcile(
            &identity,
            Some(&desired(url)),
            Ok(&observed),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        let rendered = report
            .findings
            .iter()
            .map(|finding: &Finding| [finding.summary.as_str(), finding.remedy.as_str()].join(" "))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!rendered.contains("********"));
        assert!(rendered.contains("secret"), "control: the word must appear");
    }
}
