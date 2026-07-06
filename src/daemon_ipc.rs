#[cfg(unix)]
use std::collections::VecDeque;
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::sync::mpsc::{self, Sender};
#[cfg(unix)]
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::Duration;

use serde_json::Value;
#[cfg(unix)]
use serde_json::json;

/// IPC protocol version. Bump when the wire contract changes.
pub const IPC_PROTOCOL_VERSION: u32 = 2;
/// Number of historical events replayed to new subscribers.
pub const RING_BUFFER_SIZE: usize = 100;

/// Wire prefix that marks a `last_error` string as a GitHub-auth-degraded
/// pause reason for the menu-bar app.
///
/// The daemon↔menu-bar reason channel is the flat `last_error: Option<String>`
/// field on the status update (the same channel the webhook-scope hint uses).
/// The menu-bar app (shipyard-macos-gui PR #31) decodes an auth-degraded pause
/// by matching this case-insensitive prefix and taking the trailing text as the
/// human detail, e.g.
///
/// ```text
/// github_auth_degraded: unauthenticated (anonymous 60/hr) — token invalid or missing
/// ```
///
/// The GUI trims a leading `": "` and falls back to a generic message when the
/// detail is empty. Keep this prefix in lockstep with the GUI decoder; do NOT
/// turn it into a structured serde discriminator.
pub const GITHUB_AUTH_DEGRADED_PREFIX: &str = "github_auth_degraded:";

/// Format a `last_error` value that the menu-bar app decodes as a
/// GitHub-auth-degraded pause reason. `detail` is a short human explanation;
/// an empty detail is tolerated (the GUI supplies its own fallback text).
#[must_use]
pub fn github_auth_degraded_message(detail: &str) -> String {
    let detail = detail.trim();
    if detail.is_empty() {
        GITHUB_AUTH_DEGRADED_PREFIX.to_owned()
    } else {
        format!("{GITHUB_AUTH_DEGRADED_PREFIX} {detail}")
    }
}

/// GitHub's anonymous (unauthenticated) REST core rate-limit ceiling. When a
/// `gh api rate_limit` snapshot reports this as the core limit, the request is
/// hitting GitHub without a valid token — i.e. auth is degraded, not merely
/// throttled on an authenticated 5000/hr bucket.
pub const ANONYMOUS_CORE_RATE_LIMIT: u64 = 60;

/// True when a `rate_limit` snapshot indicates the anonymous 60/hr bucket,
/// meaning the token isn't authenticating. Accepts either the full
/// `gh api rate_limit` shape (`.resources.core.limit`) or the flattened
/// `.rate.limit` shape. Returns false for the authenticated bucket, unknown
/// shapes, or a missing snapshot.
#[must_use]
pub fn rate_limit_is_anonymous(rate_limit: &Value) -> bool {
    let core_limit = rate_limit
        .get("resources")
        .and_then(|resources| resources.get("core"))
        .and_then(|core| core.get("limit"))
        .or_else(|| rate_limit.get("rate").and_then(|rate| rate.get("limit")))
        .or_else(|| rate_limit.get("limit"))
        .and_then(Value::as_u64);
    core_limit == Some(ANONYMOUS_CORE_RATE_LIMIT)
}

/// Server-side view of daemon state exposed over the IPC socket.
#[derive(Clone, Debug, PartialEq)]
pub struct IpcState {
    /// Active tunnel backend.
    pub tunnel_backend: String,
    /// Public tunnel URL when available.
    pub tunnel_url: Option<String>,
    /// Verification timestamp.
    pub tunnel_verified_at: Option<f64>,
    /// Connected subscriber count.
    pub subscribers: usize,
    /// Last event timestamp.
    pub last_event_at: Option<f64>,
    /// Registered repo slugs.
    pub registered_repos: Vec<String>,
    /// Rate-limit snapshot if known.
    pub rate_limit: Option<Value>,
    /// Last recoverable daemon warning/error, if any. Doubles as the
    /// menu-bar app's pause-reason channel: an auth-degraded pause is encoded
    /// here via [`github_auth_degraded_message`].
    pub last_error: Option<String>,
}

#[cfg(unix)]
type StatusProvider = Arc<dyn Fn() -> IpcState + Send + Sync>;
#[cfg(unix)]
type StopRequestCallback = Arc<dyn Fn() + Send + Sync>;
#[cfg(unix)]
type ShipStateListProvider = Arc<dyn Fn() -> Vec<Value> + Send + Sync>;

#[cfg(unix)]
#[derive(Clone)]
struct Subscriber {
    sender: Sender<WriterMessage>,
}

#[cfg(unix)]
enum WriterMessage {
    Json(Value),
    Goodbye,
}

#[cfg(unix)]
#[derive(Default)]
struct SharedState {
    ring: VecDeque<Value>,
    subscribers: std::collections::BTreeMap<usize, Subscriber>,
    next_id: usize,
}

/// Owns the Unix socket listener and fans out events to subscribers.
#[cfg(unix)]
pub struct IpcServer {
    socket_path: PathBuf,
    status_provider: StatusProvider,
    on_stop_request: Option<StopRequestCallback>,
    ship_state_list_provider: Option<ShipStateListProvider>,
    shared: Arc<Mutex<SharedState>>,
    running: Arc<AtomicBool>,
    listener_thread: Option<thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl IpcServer {
    /// Create a new IPC server with a status provider.
    pub fn new<S>(socket_path: PathBuf, status_provider: S) -> Self
    where
        S: Fn() -> IpcState + Send + Sync + 'static,
    {
        Self {
            socket_path,
            status_provider: Arc::new(status_provider),
            on_stop_request: None,
            ship_state_list_provider: None,
            shared: Arc::new(Mutex::new(SharedState::default())),
            running: Arc::new(AtomicBool::new(false)),
            listener_thread: None,
        }
    }

    /// Install a callback invoked when a client sends `{"type":"stop"}`.
    #[must_use]
    pub fn with_stop_request<F>(mut self, callback: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.on_stop_request = Some(Arc::new(callback));
        self
    }

    /// Install a ship-state-list provider for IPC protocol v2.
    #[must_use]
    pub fn with_ship_state_list_provider<F>(mut self, provider: F) -> Self
    where
        F: Fn() -> Vec<Value> + Send + Sync + 'static,
    {
        self.ship_state_list_provider = Some(Arc::new(provider));
        self
    }

    /// Start the listener thread and bind the socket.
    pub fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        std::fs::create_dir_all(
            self.socket_path
                .parent()
                .ok_or("socket path must have parent")?,
        )?;
        if self.socket_path.exists() || self.socket_path.is_symlink() {
            let _ = std::fs::remove_file(&self.socket_path);
        }

        let listener = UnixListener::bind(&self.socket_path)?;
        listener.set_nonblocking(true)?;
        self.running.store(true, Ordering::Release);

        let socket_path = self.socket_path.clone();
        let shared = Arc::clone(&self.shared);
        let running = Arc::clone(&self.running);
        let status_provider = Arc::clone(&self.status_provider);
        let on_stop_request = self.on_stop_request.clone();
        let ship_state_list_provider = self.ship_state_list_provider.clone();

        self.listener_thread = Some(thread::spawn(move || {
            while running.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let shared = Arc::clone(&shared);
                        let running = Arc::clone(&running);
                        let status_provider = Arc::clone(&status_provider);
                        let on_stop_request = on_stop_request.clone();
                        let ship_state_list_provider = ship_state_list_provider.clone();
                        thread::spawn(move || {
                            handle_client(
                                stream,
                                &shared,
                                &running,
                                &status_provider,
                                on_stop_request.as_ref(),
                                ship_state_list_provider.as_ref(),
                            );
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }

            let _ = std::fs::remove_file(socket_path);
        }));

        Ok(())
    }

    /// Stop the listener, drop subscribers, and remove the socket.
    pub fn stop(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.running.store(false, Ordering::Release);
        if let Ok(shared) = self.shared.lock() {
            for subscriber in shared.subscribers.values() {
                let _ = subscriber.sender.send(WriterMessage::Goodbye);
            }
        }
        if let Some(thread) = self.listener_thread.take() {
            let _ = thread.join();
        }
        if self.socket_path.exists() || self.socket_path.is_symlink() {
            let _ = std::fs::remove_file(&self.socket_path);
        }
        Ok(())
    }

    /// Broadcast an event to connected subscribers and append it to the ring buffer.
    pub fn broadcast_event(&self, event: Value) {
        let frame = event_frame(&event);
        let subscribers = {
            let mut shared = self.shared.lock().expect("shared lock");
            shared.ring.push_back(event);
            while shared.ring.len() > RING_BUFFER_SIZE {
                let _ = shared.ring.pop_front();
            }
            shared
                .subscribers
                .values()
                .cloned()
                .collect::<Vec<Subscriber>>()
        };

        for subscriber in subscribers {
            let _ = subscriber.sender.send(WriterMessage::Json(frame.clone()));
        }
    }

    /// Return the number of actively subscribed clients.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.shared
            .lock()
            .map_or(0, |shared| shared.subscribers.len())
    }
}

#[cfg(unix)]
fn handle_client(
    stream: UnixStream,
    shared: &Arc<Mutex<SharedState>>,
    running: &Arc<AtomicBool>,
    status_provider: &StatusProvider,
    on_stop_request: Option<&StopRequestCallback>,
    ship_state_list_provider: Option<&ShipStateListProvider>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let (sender, receiver) = mpsc::channel::<WriterMessage>();
    let writer_stream = stream.try_clone().ok();
    let writer_thread = writer_stream
        .map(|writer_stream| thread::spawn(move || writer_loop(writer_stream, receiver)));
    let _ = sender.send(WriterMessage::Json(json!({
        "type": "hello",
        "protocol": IPC_PROTOCOL_VERSION,
        "shipyard_version": env!("CARGO_PKG_VERSION"),
    })));

    let mut subscriber_id = None;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    while running.load(Ordering::Acquire) {
        line.clear();
        let bytes_read = match reader.read_line(&mut line) {
            Ok(bytes_read) => bytes_read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(_) => break,
        };
        if bytes_read == 0 {
            break;
        }
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(msg_type) = message.get("type").and_then(Value::as_str) else {
            continue;
        };

        match msg_type {
            "subscribe" => {
                let (id, backlog) = {
                    let mut shared = shared.lock().expect("shared lock");
                    let id = shared.next_id;
                    shared.next_id += 1;
                    shared.subscribers.insert(
                        id,
                        Subscriber {
                            sender: sender.clone(),
                        },
                    );
                    let backlog = shared.ring.iter().cloned().collect::<Vec<_>>();
                    (id, backlog)
                };
                subscriber_id = Some(id);
                for event in backlog {
                    let _ = sender.send(WriterMessage::Json(event_frame(&event)));
                }
            }
            "status" => {
                let subscribers = shared.lock().expect("shared lock").subscribers.len();
                let mut state = status_provider();
                state.subscribers = subscribers;
                let _ = sender.send(WriterMessage::Json(status_frame(&state)));
            }
            "stop" => {
                if let Some(callback) = &on_stop_request {
                    callback();
                }
            }
            "ship-state-list" => {
                let states = ship_state_list_provider
                    .as_ref()
                    .map(|provider| provider())
                    .unwrap_or_default();
                let _ = sender.send(WriterMessage::Json(json!({
                    "type": "ship-state-list",
                    "states": states,
                })));
            }
            _ => {}
        }
    }

    if let Some(id) = subscriber_id {
        let _ = shared
            .lock()
            .map(|mut shared| shared.subscribers.remove(&id));
    }
    let _ = sender.send(WriterMessage::Goodbye);
    if let Some(writer_thread) = writer_thread {
        let _ = writer_thread.join();
    }
}

#[cfg(unix)]
fn writer_loop(mut stream: UnixStream, receiver: mpsc::Receiver<WriterMessage>) {
    for message in receiver {
        match message {
            WriterMessage::Json(value) => {
                if write_json_line(&mut stream, &value).is_err() {
                    break;
                }
            }
            WriterMessage::Goodbye => {
                let _ = write_json_line(&mut stream, &json!({"type": "goodbye"}));
                break;
            }
        }
    }
}

#[cfg(unix)]
fn write_json_line(stream: &mut UnixStream, value: &Value) -> Result<(), std::io::Error> {
    serde_json::to_writer(&mut *stream, value)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

#[cfg(unix)]
fn status_frame(state: &IpcState) -> Value {
    json!({
        "type": "status",
        "tunnel": {
            "backend": state.tunnel_backend,
            "url": state.tunnel_url,
            "verified_at": state.tunnel_verified_at,
        },
        "subscribers": state.subscribers,
        "last_event_at": state.last_event_at,
        "registered_repos": state.registered_repos,
        "rate_limit": state.rate_limit,
        "last_error": state.last_error,
        "shipyard_version": env!("CARGO_PKG_VERSION"),
        "protocol": IPC_PROTOCOL_VERSION,
    })
}

#[cfg(unix)]
fn event_frame(event: &Value) -> Value {
    if let Value::Object(map) = event {
        let mut frame = map.clone();
        frame.insert("type".to_owned(), Value::from("event"));
        Value::Object(frame)
    } else {
        json!({
            "type": "event",
            "payload": event,
        })
    }
}

/// Read daemon status from the IPC socket. Returns `None` if the daemon
/// is not reachable or no status reply is observed.
#[cfg(unix)]
#[must_use]
pub fn read_daemon_status(state_dir: &Path) -> Option<Value> {
    request_daemon_frame(state_dir, br#"{"type":"status"}"#, "status")
}

/// Read the daemon-served ship-state list from the IPC socket.
#[cfg(unix)]
#[must_use]
pub fn read_daemon_ship_state_list(state_dir: &Path) -> Option<Vec<Value>> {
    let reply = request_daemon_frame(
        state_dir,
        br#"{"type":"ship-state-list"}"#,
        "ship-state-list",
    )?;
    Some(
        reply
            .get("states")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    )
}

#[cfg(unix)]
fn request_daemon_frame(state_dir: &Path, request: &[u8], response_type: &str) -> Option<Value> {
    let socket_path = state_dir.join("daemon").join("daemon.sock");
    if !socket_path.exists() {
        return None;
    }

    let mut stream = UnixStream::connect(socket_path).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    stream.write_all(request).ok()?;
    stream.write_all(b"\n").ok()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let value = serde_json::from_str::<Value>(line.trim()).ok()?;
        if value.get("type").and_then(Value::as_str) == Some(response_type) {
            return Some(value);
        }
    }
}

/// Non-Unix platforms do not currently support daemon IPC.
#[cfg(not(unix))]
#[must_use]
pub fn read_daemon_status(_state_dir: &Path) -> Option<Value> {
    None
}

#[cfg(not(unix))]
/// Non-Unix builds do not currently support daemon IPC ship-state list reads.
#[must_use]
pub fn read_daemon_ship_state_list(_state_dir: &Path) -> Option<Vec<Value>> {
    None
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::io::{BufRead, BufReader, ErrorKind, Write};
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;
    #[cfg(not(unix))]
    use std::path::Path;
    #[cfg(unix)]
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    use serde_json::{Value, json};

    #[cfg(unix)]
    use super::{
        IPC_PROTOCOL_VERSION, IpcServer, IpcState, read_daemon_ship_state_list, read_daemon_status,
    };

    #[test]
    fn github_auth_degraded_message_prefixes_detail() {
        let message = super::github_auth_degraded_message(
            "unauthenticated (anonymous 60/hr) — token invalid or missing",
        );
        assert!(message.starts_with(super::GITHUB_AUTH_DEGRADED_PREFIX));
        let (prefix, detail) = message
            .split_once(':')
            .map(|(p, rest)| (format!("{p}:"), rest.trim_start()))
            .expect("colon");
        assert_eq!(prefix, super::GITHUB_AUTH_DEGRADED_PREFIX);
        assert_eq!(
            detail,
            "unauthenticated (anonymous 60/hr) — token invalid or missing"
        );
    }

    #[test]
    fn github_auth_degraded_message_tolerates_empty_detail() {
        // GUI supplies its own fallback text when the detail is empty.
        assert_eq!(
            super::github_auth_degraded_message("   "),
            super::GITHUB_AUTH_DEGRADED_PREFIX
        );
    }

    #[test]
    fn anonymous_core_rate_limit_is_detected() {
        let resources = serde_json::json!({
            "resources": { "core": { "limit": 60, "remaining": 0 } }
        });
        assert!(super::rate_limit_is_anonymous(&resources));

        let flat = serde_json::json!({ "rate": { "limit": 60 } });
        assert!(super::rate_limit_is_anonymous(&flat));
    }

    #[test]
    fn authenticated_rate_limit_is_not_anonymous() {
        let resources = serde_json::json!({
            "resources": { "core": { "limit": 5000, "remaining": 4999 } }
        });
        assert!(!super::rate_limit_is_anonymous(&resources));
        assert!(!super::rate_limit_is_anonymous(&serde_json::json!({})));
    }

    #[cfg(unix)]
    fn dummy_state() -> IpcState {
        IpcState {
            tunnel_backend: "tailscale".to_owned(),
            tunnel_url: Some("https://example.ts.net".to_owned()),
            tunnel_verified_at: None,
            subscribers: 0,
            last_event_at: None,
            registered_repos: vec!["org/repo".to_owned()],
            rate_limit: None,
            last_error: None,
        }
    }

    #[cfg(unix)]
    fn read_lines(stream: UnixStream, count: usize) -> Vec<Value> {
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("timeout");
        let mut reader = BufReader::new(stream);
        let mut lines = Vec::new();
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while lines.len() < count && Instant::now() < deadline {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => lines.push(serde_json::from_str(line.trim()).expect("json")),
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(error) => panic!("read: {error}"),
            }
        }
        assert_eq!(
            lines.len(),
            count,
            "timed out waiting for {count} IPC frame(s); got {lines:?}",
        );
        lines
    }

    #[cfg(unix)]
    fn short_socket_path() -> PathBuf {
        tempfile::Builder::new()
            .prefix("sy-ipc-")
            .tempdir_in("/tmp")
            .expect("tempdir")
            .keep()
            .join("daemon.sock")
    }

    #[cfg(unix)]
    #[test]
    fn subscribe_then_receive_broadcast() {
        let socket_path = short_socket_path();
        let mut server = IpcServer::new(socket_path.clone(), dummy_state);
        server.start().expect("start");

        let client = std::thread::spawn(move || {
            let mut stream = UnixStream::connect(socket_path).expect("connect");
            stream
                .write_all(br#"{"type":"subscribe"}"#)
                .expect("subscribe");
            stream.write_all(b"\n").expect("newline");
            read_lines(stream, 2)
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while server.subscriber_count() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        server.broadcast_event(json!({"kind":"workflow_run","payload":{"x":1}}));
        let lines = client.join().expect("join");
        server.stop().expect("stop");

        assert_eq!(lines[0]["type"], "hello");
        assert_eq!(lines[0]["protocol"], IPC_PROTOCOL_VERSION);
        assert_eq!(lines[1]["type"], "event");
        assert_eq!(lines[1]["kind"], "workflow_run");
        assert_eq!(lines[1]["payload"]["x"], 1);
    }

    #[cfg(unix)]
    #[test]
    fn late_subscriber_gets_ring_buffer_backlog() {
        let socket_path = short_socket_path();
        let mut server = IpcServer::new(socket_path.clone(), dummy_state);
        server.start().expect("start");
        server.broadcast_event(json!({"kind":"workflow_run","payload":{"id":1}}));
        server.broadcast_event(json!({"kind":"workflow_run","payload":{"id":2}}));

        let lines = {
            let mut stream = UnixStream::connect(socket_path).expect("connect");
            stream
                .write_all(br#"{"type":"subscribe"}"#)
                .expect("subscribe");
            stream.write_all(b"\n").expect("newline");
            read_lines(stream, 3)
        };
        server.stop().expect("stop");

        assert_eq!(lines[0]["type"], "hello");
        assert_eq!(lines[1]["payload"]["id"], 1);
        assert_eq!(lines[2]["payload"]["id"], 2);
    }

    #[cfg(unix)]
    #[test]
    fn status_request_returns_snapshot() {
        let socket_path = short_socket_path();
        let mut server = IpcServer::new(socket_path.clone(), dummy_state);
        server.start().expect("start");

        let lines = {
            let mut stream = UnixStream::connect(socket_path).expect("connect");
            stream.write_all(br#"{"type":"status"}"#).expect("status");
            stream.write_all(b"\n").expect("newline");
            read_lines(stream, 2)
        };
        server.stop().expect("stop");

        assert_eq!(lines[0]["type"], "hello");
        assert_eq!(lines[1]["type"], "status");
        assert_eq!(lines[1]["tunnel"]["backend"], "tailscale");
        assert_eq!(lines[1]["registered_repos"][0], "org/repo");
    }

    #[cfg(unix)]
    #[test]
    fn read_daemon_status_sees_past_hello_line() {
        let tempdir = tempfile::Builder::new()
            .prefix("sy-ipc-state-")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let state_dir = tempdir.path().to_path_buf();
        let socket_path = state_dir.join("daemon").join("daemon.sock");
        let mut server = IpcServer::new(socket_path, dummy_state);
        server.start().expect("start");

        let status = read_daemon_status(&state_dir).expect("status");
        server.stop().expect("stop");

        assert_eq!(status["type"], "status");
        assert_eq!(status["registered_repos"][0], "org/repo");
    }

    #[cfg(unix)]
    #[test]
    fn read_daemon_ship_state_list_returns_states_reply() {
        let tempdir = tempfile::Builder::new()
            .prefix("sy-ipc-list-")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let state_dir = tempdir.path().to_path_buf();
        let socket_path = state_dir.join("daemon").join("daemon.sock");
        std::fs::create_dir_all(socket_path.parent().expect("parent")).expect("daemon dir");
        let mut server = IpcServer::new(socket_path, dummy_state)
            .with_ship_state_list_provider(|| vec![json!({"pr": 151, "repo": "o/r"})]);
        server.start().expect("start");

        let states = read_daemon_ship_state_list(&state_dir).expect("states");
        server.stop().expect("stop");

        assert_eq!(states.len(), 1);
        assert_eq!(states[0]["pr"], 151);
        assert_eq!(states[0]["repo"], "o/r");
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_read_daemon_status_is_none() {
        assert!(super::read_daemon_status(Path::new(".")).is_none());
    }
}
