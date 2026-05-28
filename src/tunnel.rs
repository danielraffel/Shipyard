//! Tunnel readiness and supervisor policy shared by the daemon runtime.
//!
//! The daemon starts IPC before it starts the public tunnel, then keeps a
//! background supervisor alive forever. This module keeps those contracts
//! separate from process/runtime wiring so retry semantics can be tested
//! without binding sockets or touching Tailscale.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use wait_timeout::ChildExt;

const FUNNEL_CAP_KEYS: [&str; 2] = ["https://tailscale.com/cap/funnel", "funnel"];
const TAILSCALE_CANDIDATE_BINARIES: [&str; 4] = [
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/opt/homebrew/bin/tailscale",
    "/usr/local/bin/tailscale",
    "/usr/bin/tailscale",
];
const TAILSCALE_STATUS_TIMEOUT: Duration = Duration::from_secs(5);
const TAILSCALE_FUNNEL_TIMEOUT: Duration = Duration::from_secs(10);
const TAILSCALE_RESET_SETTLE: Duration = Duration::from_millis(500);

/// Probe backoffs used while Tailscale is warming up.
pub const TAILSCALE_PROBE_BACKOFFS: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(6),
    Duration::from_secs(12),
];

/// Supervisor retry backoffs for tunnel bring-up and crash recovery.
// Keep `rust-version = 1.92`; newer Clippy suggests `from_mins`, which is
// unavailable on the declared MSRV.
#[allow(unknown_lints, clippy::duration_suboptimal_units)]
pub const TUNNEL_RETRY_BACKOFFS: [Duration; 7] = [
    Duration::from_secs(2),
    Duration::from_secs(6),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
];

/// Cadence for verifying a tunnel after it comes up.
pub const TUNNEL_VERIFY_INTERVAL: Duration = Duration::from_secs(30);

/// Public-facing tunnel details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TunnelInfo {
    /// Public URL routed to the daemon webhook listener.
    pub public_url: String,
    /// Backend that supplied the tunnel.
    pub backend: String,
}

/// Current daemon-visible tunnel state.
#[derive(Clone, Debug, PartialEq)]
pub struct TunnelSnapshot {
    /// Active backend name, or `inactive` when no tunnel is up.
    pub backend: String,
    /// Public tunnel URL when available.
    pub url: Option<String>,
    /// Verification timestamp in Unix seconds.
    pub verified_at: Option<f64>,
}

impl TunnelSnapshot {
    /// Return an inactive snapshot.
    #[must_use]
    pub fn inactive() -> Self {
        Self {
            backend: "inactive".to_owned(),
            url: None,
            verified_at: None,
        }
    }

    /// Return a snapshot from a successfully verified tunnel.
    #[must_use]
    pub fn verified(info: &TunnelInfo, verified_at: f64) -> Self {
        Self {
            backend: info.backend.clone(),
            url: Some(info.public_url.clone()),
            verified_at: Some(verified_at),
        }
    }

    /// Return a development override snapshot.
    #[must_use]
    pub fn development(url: String, verified_at: f64) -> Self {
        Self {
            backend: "development".to_owned(),
            url: Some(url),
            verified_at: Some(verified_at),
        }
    }
}

/// Coarse tunnel failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TunnelErrorKind {
    /// Backend is installed but not ready or authenticated enough.
    NotReady,
    /// Backend start command failed.
    StartFailed,
    /// Process or filesystem boundary failed.
    Io,
    /// Unexpected backend failure; the supervisor still retries it.
    Unexpected,
}

/// Error returned by a tunnel backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TunnelError {
    /// Failure category.
    pub kind: TunnelErrorKind,
    /// Human-readable diagnostic.
    pub message: String,
}

impl TunnelError {
    /// Build a tunnel error.
    #[must_use]
    pub fn new(kind: TunnelErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TunnelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TunnelError {}

/// Minimal backend contract the supervisor needs.
pub trait TunnelBackend {
    /// Human-readable backend name.
    fn name(&self) -> &'static str;

    /// Bring the tunnel up for the local daemon webhook port.
    fn start(&mut self, local_port: u16) -> Result<TunnelInfo, TunnelError>;

    /// Verify the tunnel still proxies to the local daemon webhook port.
    fn verify(&mut self, local_port: u16) -> Result<bool, TunnelError>;

    /// Tear down the tunnel if configured.
    fn stop(&mut self) -> Result<(), TunnelError>;
}

/// Supervisor retry policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TunnelSupervisorPolicy {
    retry_backoffs: Vec<Duration>,
    verify_interval: Duration,
}

impl Default for TunnelSupervisorPolicy {
    fn default() -> Self {
        Self {
            retry_backoffs: TUNNEL_RETRY_BACKOFFS.to_vec(),
            verify_interval: TUNNEL_VERIFY_INTERVAL,
        }
    }
}

impl TunnelSupervisorPolicy {
    /// Build a policy with explicit retry backoffs and verify cadence.
    #[must_use]
    pub fn new(retry_backoffs: Vec<Duration>, verify_interval: Duration) -> Self {
        assert!(
            !retry_backoffs.is_empty(),
            "tunnel retry policy requires at least one backoff"
        );
        Self {
            retry_backoffs,
            verify_interval,
        }
    }

    /// Return the capped delay for an attempt index.
    #[must_use]
    pub fn delay_for_attempt(&self, attempt: usize) -> Duration {
        self.retry_backoffs
            .get(attempt)
            .copied()
            .unwrap_or_else(|| *self.retry_backoffs.last().expect("non-empty backoffs"))
    }

    /// Return the verification cadence.
    #[must_use]
    pub fn verify_interval(&self) -> Duration {
        self.verify_interval
    }
}

/// Mutable supervisor state that stays independent of backend IO.
#[derive(Clone, Debug, PartialEq)]
pub struct TunnelSupervisorState {
    crash_attempt: usize,
    snapshot: TunnelSnapshot,
}

impl Default for TunnelSupervisorState {
    fn default() -> Self {
        Self {
            crash_attempt: 0,
            snapshot: TunnelSnapshot::inactive(),
        }
    }
}

impl TunnelSupervisorState {
    /// Mark a tunnel as up and reset the crash backoff bucket.
    pub fn mark_up(&mut self, info: &TunnelInfo, verified_at: f64) {
        self.crash_attempt = 0;
        self.snapshot = TunnelSnapshot::verified(info, verified_at);
    }

    /// Mark the tunnel as lost.
    pub fn mark_down(&mut self) {
        self.snapshot = TunnelSnapshot::inactive();
    }

    /// Record a supervisor-level crash and return the capped retry delay.
    pub fn record_crash(&mut self, policy: &TunnelSupervisorPolicy) -> Duration {
        let delay = policy.delay_for_attempt(self.crash_attempt);
        self.crash_attempt += 1;
        delay
    }

    /// Return the visible tunnel snapshot.
    #[must_use]
    pub fn snapshot(&self) -> &TunnelSnapshot {
        &self.snapshot
    }

    /// Return the current crash-attempt bucket.
    #[must_use]
    pub fn crash_attempt(&self) -> usize {
        self.crash_attempt
    }
}

/// Result of a tunnel watch pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TunnelWatchExit {
    /// Caller requested stop before loss was observed.
    Stopped,
    /// Backend reported that the tunnel no longer proxies correctly.
    Lost,
    /// Backend verification raised an error; the supervisor should re-enter
    /// bring-up rather than dying.
    VerifyError(TunnelError),
}

/// Retry `backend.start` with capped backoff until it succeeds or stop is set.
pub fn start_with_retry<B, Stop, Sleep>(
    backend: &mut B,
    local_port: u16,
    policy: &TunnelSupervisorPolicy,
    mut should_stop: Stop,
    mut sleep: Sleep,
) -> Option<TunnelInfo>
where
    B: TunnelBackend,
    Stop: FnMut() -> bool,
    Sleep: FnMut(Duration),
{
    let mut attempt = 0;
    loop {
        if should_stop() {
            return None;
        }
        match backend.start(local_port) {
            Ok(info) => return Some(info),
            Err(_error) => {
                let delay = policy.delay_for_attempt(attempt);
                attempt += 1;
                if should_stop() {
                    return None;
                }
                sleep(delay);
            }
        }
    }
}

/// Verify until the tunnel is lost, verification errors, or stop is set.
pub fn watch_until_lost<B, Stop, Sleep>(
    backend: &mut B,
    local_port: u16,
    policy: &TunnelSupervisorPolicy,
    mut should_stop: Stop,
    mut sleep: Sleep,
) -> TunnelWatchExit
where
    B: TunnelBackend,
    Stop: FnMut() -> bool,
    Sleep: FnMut(Duration),
{
    loop {
        if should_stop() {
            return TunnelWatchExit::Stopped;
        }
        sleep(policy.verify_interval());
        if should_stop() {
            return TunnelWatchExit::Stopped;
        }
        match backend.verify(local_port) {
            Ok(true) => {}
            Ok(false) => return TunnelWatchExit::Lost,
            Err(error) => return TunnelWatchExit::VerifyError(error),
        }
    }
}

/// External hooks needed by the synchronous supervisor loop.
pub struct TunnelSupervisorHooks<Stop, Sleep, Now, Publish> {
    should_stop: Stop,
    sleep: Sleep,
    now: Now,
    publish: Publish,
}

impl<Stop, Sleep, Now, Publish> TunnelSupervisorHooks<Stop, Sleep, Now, Publish> {
    /// Construct supervisor hooks.
    #[must_use]
    pub fn new(should_stop: Stop, sleep: Sleep, now: Now, publish: Publish) -> Self {
        Self {
            should_stop,
            sleep,
            now,
            publish,
        }
    }
}

/// Run the full supervisor loop until stop is requested.
pub fn supervise_tunnel<B, Stop, Sleep, Now, Publish>(
    backend: &mut B,
    local_port: u16,
    policy: &TunnelSupervisorPolicy,
    state: &mut TunnelSupervisorState,
    mut hooks: TunnelSupervisorHooks<Stop, Sleep, Now, Publish>,
) where
    B: TunnelBackend,
    Stop: FnMut() -> bool,
    Sleep: FnMut(Duration),
    Now: FnMut() -> f64,
    Publish: FnMut(TunnelSnapshot),
{
    while !(hooks.should_stop)() {
        let Some(info) = start_with_retry(
            backend,
            local_port,
            policy,
            &mut hooks.should_stop,
            &mut hooks.sleep,
        ) else {
            break;
        };
        state.mark_up(&info, (hooks.now)());
        (hooks.publish)(state.snapshot().clone());

        match watch_until_lost(
            backend,
            local_port,
            policy,
            &mut hooks.should_stop,
            &mut hooks.sleep,
        ) {
            TunnelWatchExit::Stopped => break,
            TunnelWatchExit::Lost | TunnelWatchExit::VerifyError(_) => {
                state.mark_down();
                (hooks.publish)(state.snapshot().clone());
            }
        }
    }

    let _ = backend.stop();
    state.mark_down();
    (hooks.publish)(state.snapshot().clone());
}

/// Parsed `tailscale status --json` readiness snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TailscaleStatus {
    /// Resolved Tailscale CLI path.
    pub binary_path: Option<PathBuf>,
    /// `BackendState` from `tailscale status --json`.
    pub backend_state: Option<String>,
    /// Current node DNS name.
    pub dns_name: Option<String>,
    /// Current node Tailscale IPs.
    pub tailscale_ips: Vec<String>,
    /// `Self.Online` from `tailscale status --json`, when present.
    pub online: Option<bool>,
    /// Whether Funnel is permitted by the tailnet capability map.
    pub funnel_permitted: bool,
}

impl TailscaleStatus {
    /// Return whether this snapshot has enough private tailnet information for
    /// controller/client reachability. Funnel permissions are intentionally not
    /// part of this predicate.
    #[must_use]
    pub fn is_tailnet_reachable(&self) -> bool {
        self.binary_path.is_some()
            && self.backend_state.as_deref() == Some("Running")
            && self.online != Some(false)
            && self.dns_name.as_deref().is_some_and(|dns| !dns.is_empty())
    }

    /// Return the `MagicDNS` name without a trailing dot.
    #[must_use]
    pub fn magic_dns_name(&self) -> Option<String> {
        if !self.is_tailnet_reachable() {
            return None;
        }
        let trimmed = self.dns_name.as_deref()?.trim_end_matches('.');
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    }

    /// Return whether this snapshot is ready to start Funnel.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.is_tailnet_reachable() && self.funnel_permitted
    }

    /// Return the HTTPS Funnel URL derived from DNS name when ready.
    #[must_use]
    pub fn funnel_url(&self) -> Option<String> {
        if !self.is_ready() {
            return None;
        }
        Some(format!("https://{}", self.magic_dns_name()?))
    }
}

/// Decode `tailscale status --json` into a readiness snapshot.
#[must_use]
pub fn decode_tailscale_status(raw_json: &[u8], binary_path: Option<PathBuf>) -> TailscaleStatus {
    let Ok(value) = serde_json::from_slice::<Value>(raw_json) else {
        return tailscale_not_ready(binary_path);
    };
    let Some(object) = value.as_object() else {
        return tailscale_not_ready(binary_path);
    };

    let backend_state = object
        .get("BackendState")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let self_object = object.get("Self").and_then(Value::as_object);
    let dns_name = self_object
        .and_then(|entry| entry.get("DNSName"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let tailscale_ips = self_object
        .and_then(|entry| entry.get("TailscaleIPs"))
        .and_then(Value::as_array)
        .map(|ips| {
            ips.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let online = self_object
        .and_then(|entry| entry.get("Online"))
        .and_then(Value::as_bool);
    let funnel_permitted = self_object
        .and_then(|entry| entry.get("CapMap"))
        .and_then(Value::as_object)
        .is_some_and(|cap_map| FUNNEL_CAP_KEYS.iter().any(|key| cap_map.contains_key(*key)));

    TailscaleStatus {
        binary_path,
        backend_state,
        dns_name,
        tailscale_ips,
        online,
        funnel_permitted,
    }
}

/// Probe the installed Tailscale CLI once.
#[must_use]
pub fn probe_tailscale() -> TailscaleStatus {
    let candidates = TAILSCALE_CANDIDATE_BINARIES
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let Some(binary) = resolve_tailscale_binary_from(&candidates) else {
        return tailscale_not_ready(None);
    };
    match run_tailscale(&binary, &["status", "--json"], TAILSCALE_STATUS_TIMEOUT) {
        Ok(output) => decode_tailscale_status(output.output.as_bytes(), Some(binary)),
        Err(_error) => tailscale_not_ready(Some(binary)),
    }
}

fn tailscale_not_ready(binary_path: Option<PathBuf>) -> TailscaleStatus {
    TailscaleStatus {
        binary_path,
        backend_state: None,
        dns_name: None,
        tailscale_ips: Vec::new(),
        online: None,
        funnel_permitted: false,
    }
}

/// Tailscale Funnel backend used by the daemon supervisor.
#[derive(Clone, Debug, Default)]
pub struct TailscaleFunnelBackend {
    binary: Option<PathBuf>,
    configured_port: Option<u16>,
}

impl TailscaleFunnelBackend {
    fn verify_configured(&mut self, local_port: u16) -> Result<bool, TunnelError> {
        let Some(binary) = self.binary.clone() else {
            return Ok(false);
        };
        let output = run_tailscale(&binary, &["funnel", "status"], TAILSCALE_FUNNEL_TIMEOUT)?;
        Ok(output.output.contains(&format!("127.0.0.1:{local_port}")))
    }
}

impl TunnelBackend for TailscaleFunnelBackend {
    fn name(&self) -> &'static str {
        "tailscale"
    }

    fn start(&mut self, local_port: u16) -> Result<TunnelInfo, TunnelError> {
        let status =
            probe_tailscale_with_retry(probe_tailscale, &TAILSCALE_PROBE_BACKOFFS, thread::sleep);
        let Some(public_url) = status.funnel_url() else {
            return Err(TunnelError::new(
                TunnelErrorKind::NotReady,
                format!(
                    "Tailscale Funnel isn't ready after retries: backend={:?} funnel_permitted={}",
                    status.backend_state, status.funnel_permitted
                ),
            ));
        };
        let binary = status.binary_path.clone().ok_or_else(|| {
            TunnelError::new(TunnelErrorKind::NotReady, "Tailscale binary missing")
        })?;
        self.binary = Some(binary.clone());

        let reset = run_tailscale(&binary, &["funnel", "reset"], TAILSCALE_FUNNEL_TIMEOUT)?;
        if reset.code != 0 {
            return Err(TunnelError::new(
                TunnelErrorKind::StartFailed,
                format!("funnel reset failed: {}", reset.output.trim()),
            ));
        }
        thread::sleep(TAILSCALE_RESET_SETTLE);

        let mut last_output = String::new();
        for attempt in 1..=3 {
            let output = run_tailscale(
                &binary,
                &["funnel", "--bg", &local_port.to_string()],
                TAILSCALE_FUNNEL_TIMEOUT,
            )?;
            last_output.clone_from(&output.output);
            if output.code != 0 {
                return Err(TunnelError::new(
                    TunnelErrorKind::StartFailed,
                    format!("funnel --bg failed: {}", output.output.trim()),
                ));
            }
            if self.verify_configured(local_port)? {
                self.configured_port = Some(local_port);
                return Ok(TunnelInfo {
                    public_url,
                    backend: self.name().to_owned(),
                });
            }
            thread::sleep(Duration::from_millis(500 * attempt));
        }

        Err(TunnelError::new(
            TunnelErrorKind::StartFailed,
            format!(
                "funnel --bg returned 0 but serve config didn't persist. last output: {}",
                last_output.trim()
            ),
        ))
    }

    fn verify(&mut self, local_port: u16) -> Result<bool, TunnelError> {
        self.verify_configured(local_port)
    }

    fn stop(&mut self) -> Result<(), TunnelError> {
        let binary = self.binary.clone().or_else(|| {
            let candidates = TAILSCALE_CANDIDATE_BINARIES
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            resolve_tailscale_binary_from(&candidates)
        });
        let Some(binary) = binary else {
            self.configured_port = None;
            return Ok(());
        };
        let output = run_tailscale(&binary, &["funnel", "reset"], TAILSCALE_FUNNEL_TIMEOUT)?;
        self.configured_port = None;
        if output.code == 0 {
            Ok(())
        } else {
            Err(TunnelError::new(
                TunnelErrorKind::StartFailed,
                format!("funnel reset failed: {}", output.output.trim()),
            ))
        }
    }
}

/// Probe Tailscale repeatedly, returning the first ready status or the last
/// not-ready status after all backoffs have been consumed.
pub fn probe_tailscale_with_retry<Probe, Sleep>(
    mut probe: Probe,
    backoffs: &[Duration],
    mut sleep: Sleep,
) -> TailscaleStatus
where
    Probe: FnMut() -> TailscaleStatus,
    Sleep: FnMut(Duration),
{
    let mut status = probe();
    if status.is_ready() {
        return status;
    }
    for delay in backoffs {
        sleep(*delay);
        status = probe();
        if status.is_ready() {
            break;
        }
    }
    status
}

/// Return the first executable Tailscale binary from a candidate list.
#[must_use]
pub fn resolve_tailscale_binary_from(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|candidate| is_executable_file(candidate))
        .cloned()
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommandOutput {
    code: i32,
    output: String,
}

fn run_tailscale(
    binary: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<CommandOutput, TunnelError> {
    let mut child = Command::new(binary)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            TunnelError::new(
                TunnelErrorKind::Io,
                format!("failed to start {}: {error}", binary.display()),
            )
        })?;
    let timed_out = child
        .wait_timeout(timeout)
        .map_err(|error| TunnelError::new(TunnelErrorKind::Io, error.to_string()))?
        .is_none();
    if timed_out {
        let _ = child.kill();
    }
    let output = child
        .wait_with_output()
        .map_err(|error| TunnelError::new(TunnelErrorKind::Io, error.to_string()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = if stderr.trim().is_empty() {
        stdout.into_owned()
    } else if stdout.trim().is_empty() {
        stderr.into_owned()
    } else {
        format!("{stdout}{stderr}")
    };
    if timed_out {
        return Err(TunnelError::new(
            TunnelErrorKind::Unexpected,
            format!("{} timed out after {timeout:?}", binary.display()),
        ));
    }
    Ok(CommandOutput {
        code: output.status.code().unwrap_or(-1),
        output: combined,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::time::Duration;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::{
        TAILSCALE_PROBE_BACKOFFS, TailscaleStatus, TunnelBackend, TunnelError, TunnelErrorKind,
        TunnelInfo, TunnelSnapshot, TunnelSupervisorHooks, TunnelSupervisorPolicy,
        TunnelSupervisorState, TunnelWatchExit, decode_tailscale_status,
        probe_tailscale_with_retry, resolve_tailscale_binary_from, start_with_retry,
        supervise_tunnel, watch_until_lost,
    };

    #[derive(Default)]
    struct FakeBackend {
        start_plan: Vec<Result<TunnelInfo, TunnelError>>,
        verify_plan: Vec<Result<bool, TunnelError>>,
        start_calls: usize,
        verify_calls: usize,
        stop_calls: usize,
    }

    impl FakeBackend {
        fn with_start_plan(start_plan: Vec<Result<TunnelInfo, TunnelError>>) -> Self {
            Self {
                start_plan,
                ..Self::default()
            }
        }

        fn with_verify_plan(verify_plan: Vec<Result<bool, TunnelError>>) -> Self {
            Self {
                verify_plan,
                ..Self::default()
            }
        }
    }

    impl TunnelBackend for FakeBackend {
        fn name(&self) -> &'static str {
            "tailscale"
        }

        fn start(&mut self, _local_port: u16) -> Result<TunnelInfo, TunnelError> {
            let index = self.start_calls;
            self.start_calls += 1;
            self.start_plan
                .get(index)
                .or_else(|| self.start_plan.last())
                .cloned()
                .expect("fake start plan")
        }

        fn verify(&mut self, _local_port: u16) -> Result<bool, TunnelError> {
            let index = self.verify_calls;
            self.verify_calls += 1;
            self.verify_plan
                .get(index)
                .or_else(|| self.verify_plan.last())
                .cloned()
                .expect("fake verify plan")
        }

        fn stop(&mut self) -> Result<(), TunnelError> {
            self.stop_calls += 1;
            Ok(())
        }
    }

    fn ok_info() -> TunnelInfo {
        TunnelInfo {
            public_url: "https://fake.ts.net".to_owned(),
            backend: "tailscale".to_owned(),
        }
    }

    fn not_ready() -> TunnelError {
        TunnelError::new(TunnelErrorKind::NotReady, "simulated: not ready")
    }

    fn unexpected() -> TunnelError {
        TunnelError::new(TunnelErrorKind::Unexpected, "simulated: unexpected")
    }

    fn ready_status() -> TailscaleStatus {
        TailscaleStatus {
            binary_path: Some("/bin/tailscale".into()),
            backend_state: Some("Running".to_owned()),
            dns_name: Some("node.tailnet.ts.net.".to_owned()),
            tailscale_ips: vec!["100.64.0.1".to_owned()],
            online: Some(true),
            funnel_permitted: true,
        }
    }

    fn not_ready_status() -> TailscaleStatus {
        TailscaleStatus {
            binary_path: Some("/bin/tailscale".into()),
            backend_state: Some("Starting".to_owned()),
            dns_name: Some("node.tailnet.ts.net.".to_owned()),
            tailscale_ips: Vec::new(),
            online: Some(false),
            funnel_permitted: false,
        }
    }

    #[test]
    fn tailscale_status_decode_requires_running_dns_and_funnel_cap() {
        let status = decode_tailscale_status(
            br#"{
              "BackendState": "Running",
              "Self": {
                "DNSName": "node.tailnet.ts.net.",
                "Online": true,
                "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"],
                "CapMap": {"https://tailscale.com/cap/funnel": []}
              }
            }"#,
            Some("/Applications/Tailscale.app/Contents/MacOS/Tailscale".into()),
        );

        assert!(status.is_ready());
        assert!(status.is_tailnet_reachable());
        assert_eq!(
            status.magic_dns_name().as_deref(),
            Some("node.tailnet.ts.net")
        );
        assert_eq!(
            status.tailscale_ips,
            vec!["100.64.0.1".to_owned(), "fd7a:115c:a1e0::1".to_owned()]
        );
        assert_eq!(status.online, Some(true));
        assert_eq!(
            status.funnel_url().as_deref(),
            Some("https://node.tailnet.ts.net")
        );
    }

    #[test]
    fn tailscale_status_tailnet_reachable_does_not_require_funnel_cap() {
        let status = decode_tailscale_status(
            br#"{
              "BackendState": "Running",
              "Self": {
                "DNSName": "node.tailnet.ts.net.",
                "Online": true,
                "TailscaleIPs": ["100.64.0.1"]
              }
            }"#,
            Some("/Applications/Tailscale.app/Contents/MacOS/Tailscale".into()),
        );

        assert!(status.is_tailnet_reachable());
        assert!(!status.is_ready());
        assert_eq!(
            status.magic_dns_name().as_deref(),
            Some("node.tailnet.ts.net")
        );
        assert_eq!(status.funnel_url(), None);
    }

    #[test]
    fn tailscale_status_tailnet_reachable_rejects_explicit_offline_self() {
        let status = decode_tailscale_status(
            br#"{
              "BackendState": "Running",
              "Self": {
                "DNSName": "node.tailnet.ts.net.",
                "Online": false,
                "TailscaleIPs": ["100.64.0.1"]
              }
            }"#,
            Some("/Applications/Tailscale.app/Contents/MacOS/Tailscale".into()),
        );

        assert!(!status.is_tailnet_reachable());
        assert_eq!(status.magic_dns_name(), None);
        assert_eq!(status.funnel_url(), None);
    }

    #[test]
    fn tailscale_status_decode_accepts_short_funnel_cap_key() {
        let status = decode_tailscale_status(
            br#"{
              "BackendState": "Running",
              "Self": {
                "DNSName": "node.tailnet.ts.net",
                "CapMap": {"funnel": {}}
              }
            }"#,
            Some("/usr/local/bin/tailscale".into()),
        );

        assert!(status.is_ready());
        assert_eq!(
            status.funnel_url().as_deref(),
            Some("https://node.tailnet.ts.net")
        );
    }

    #[test]
    fn tailscale_status_decode_malformed_json_is_not_ready() {
        let status = decode_tailscale_status(b"not-json", Some("/bin/tailscale".into()));

        assert!(!status.is_ready());
        assert_eq!(status.backend_state, None);
        assert_eq!(status.funnel_url(), None);
    }

    #[test]
    fn tailscale_probe_retries_until_ready() {
        let calls = Cell::new(0);
        let mut sleeps = Vec::new();

        let status = probe_tailscale_with_retry(
            || {
                let next = calls.get() + 1;
                calls.set(next);
                if next < 3 {
                    not_ready_status()
                } else {
                    ready_status()
                }
            },
            &TAILSCALE_PROBE_BACKOFFS,
            |delay| sleeps.push(delay),
        );

        assert!(status.is_ready());
        assert_eq!(calls.get(), 3);
        assert_eq!(sleeps, vec![Duration::from_secs(2), Duration::from_secs(6)]);
    }

    #[test]
    fn tailscale_probe_first_ready_skips_retry_sleep() {
        let calls = Cell::new(0);
        let mut sleeps = Vec::new();

        let status = probe_tailscale_with_retry(
            || {
                calls.set(calls.get() + 1);
                ready_status()
            },
            &TAILSCALE_PROBE_BACKOFFS,
            |delay| sleeps.push(delay),
        );

        assert!(status.is_ready());
        assert_eq!(calls.get(), 1);
        assert!(sleeps.is_empty());
    }

    #[test]
    fn tailscale_probe_returns_last_status_after_retry_budget() {
        let calls = Cell::new(0);
        let status = probe_tailscale_with_retry(
            || {
                calls.set(calls.get() + 1);
                not_ready_status()
            },
            &[Duration::from_millis(1), Duration::from_millis(2)],
            |_| {},
        );

        assert!(!status.is_ready());
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn supervisor_policy_caps_retry_delay() {
        let policy = TunnelSupervisorPolicy::new(
            vec![Duration::from_secs(1), Duration::from_secs(3)],
            Duration::from_secs(30),
        );

        assert_eq!(policy.delay_for_attempt(0), Duration::from_secs(1));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_secs(3));
        assert_eq!(policy.delay_for_attempt(99), Duration::from_secs(3));
    }

    #[test]
    fn start_with_retry_eventually_succeeds() {
        let policy = TunnelSupervisorPolicy::new(
            vec![Duration::from_millis(2), Duration::from_millis(6)],
            Duration::from_secs(30),
        );
        let mut backend =
            FakeBackend::with_start_plan(vec![Err(not_ready()), Err(not_ready()), Ok(ok_info())]);
        let mut sleeps = Vec::new();

        let info = start_with_retry(
            &mut backend,
            12_345,
            &policy,
            || false,
            |delay| {
                sleeps.push(delay);
            },
        );

        assert_eq!(info, Some(ok_info()));
        assert_eq!(backend.start_calls, 3);
        assert_eq!(
            sleeps,
            vec![Duration::from_millis(2), Duration::from_millis(6)]
        );
    }

    #[test]
    fn start_with_retry_stops_before_next_attempt() {
        let policy = TunnelSupervisorPolicy::new(vec![Duration::ZERO], Duration::from_secs(30));
        let mut backend = FakeBackend::with_start_plan(vec![Err(not_ready()), Ok(ok_info())]);
        let stop_calls = Cell::new(0);

        let info = start_with_retry(
            &mut backend,
            12_345,
            &policy,
            || {
                stop_calls.set(stop_calls.get() + 1);
                stop_calls.get() > 1
            },
            |_| {},
        );

        assert_eq!(info, None);
        assert_eq!(backend.start_calls, 1);
    }

    #[test]
    fn watch_until_lost_returns_when_verify_reports_false() {
        let policy = TunnelSupervisorPolicy::new(vec![Duration::ZERO], Duration::from_millis(5));
        let mut backend = FakeBackend::with_verify_plan(vec![Ok(true), Ok(false)]);
        let mut sleeps = Vec::new();

        let exit = watch_until_lost(
            &mut backend,
            12_345,
            &policy,
            || false,
            |delay| {
                sleeps.push(delay);
            },
        );

        assert_eq!(exit, TunnelWatchExit::Lost);
        assert_eq!(backend.verify_calls, 2);
        assert_eq!(
            sleeps,
            vec![Duration::from_millis(5), Duration::from_millis(5)]
        );
    }

    #[test]
    fn watch_until_lost_turns_verify_errors_into_recovery_signal() {
        let policy = TunnelSupervisorPolicy::new(vec![Duration::ZERO], Duration::from_millis(5));
        let mut backend = FakeBackend::with_verify_plan(vec![Err(unexpected())]);

        let exit = watch_until_lost(&mut backend, 12_345, &policy, || false, |_| {});

        assert_eq!(exit, TunnelWatchExit::VerifyError(unexpected()));
        assert_eq!(backend.verify_calls, 1);
    }

    #[test]
    fn crash_backoff_resets_after_successful_bring_up() {
        let policy = TunnelSupervisorPolicy::new(
            vec![Duration::from_millis(200), Duration::from_secs(10)],
            Duration::from_secs(30),
        );
        let mut state = TunnelSupervisorState::default();

        assert_eq!(state.record_crash(&policy), Duration::from_millis(200));
        assert_eq!(state.crash_attempt(), 1);
        state.mark_up(&ok_info(), 42.0);
        assert_eq!(state.crash_attempt(), 0);
        assert_eq!(state.record_crash(&policy), Duration::from_millis(200));
    }

    #[test]
    fn supervisor_loop_publishes_recovery_after_loss() {
        let policy = TunnelSupervisorPolicy::new(vec![Duration::ZERO], Duration::ZERO);
        let mut backend = FakeBackend {
            start_plan: vec![Ok(ok_info()), Ok(ok_info())],
            verify_plan: vec![Ok(false)],
            ..FakeBackend::default()
        };
        let mut state = TunnelSupervisorState::default();
        let publish_count = Cell::new(0);
        let mut snapshots = Vec::new();

        supervise_tunnel(
            &mut backend,
            12_345,
            &policy,
            &mut state,
            TunnelSupervisorHooks::new(
                || publish_count.get() >= 3,
                |_| {},
                || 42.0,
                |snapshot| {
                    publish_count.set(publish_count.get() + 1);
                    snapshots.push(snapshot);
                },
            ),
        );

        assert_eq!(backend.start_calls, 2);
        assert_eq!(backend.verify_calls, 1);
        assert!(backend.stop_calls >= 1);
        assert_eq!(snapshots[0].backend, "tailscale");
        assert_eq!(snapshots[1].backend, "inactive");
        assert_eq!(snapshots[2].backend, "tailscale");
        assert_eq!(
            snapshots.last().expect("final snapshot").backend,
            "inactive"
        );
    }

    #[test]
    fn tunnel_snapshot_development_override_is_visible() {
        let snapshot = TunnelSnapshot::development("https://dev.example".to_owned(), 10.0);

        assert_eq!(snapshot.backend, "development");
        assert_eq!(snapshot.url.as_deref(), Some("https://dev.example"));
        assert_eq!(snapshot.verified_at, Some(10.0));
    }

    #[test]
    fn fake_backend_stop_is_part_of_contract() {
        let mut backend = FakeBackend::default();

        assert_eq!(backend.name(), "tailscale");
        assert_eq!(backend.stop(), Ok(()));
        assert_eq!(backend.stop_calls, 1);
    }

    #[test]
    fn resolve_tailscale_binary_skips_missing_candidates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("missing");
        let executable = tmp.path().join("tailscale");
        fs::write(&executable, "#!/bin/sh\n").expect("write executable");
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&executable).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&executable, permissions).expect("chmod");
        }

        assert_eq!(
            resolve_tailscale_binary_from(&[missing, executable.clone()]),
            Some(executable)
        );
    }
}
