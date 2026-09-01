#[cfg(unix)]
use std::collections::VecDeque;
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::net::Shutdown;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::sync::mpsc::{self, SyncSender, TrySendError};
#[cfg(unix)]
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use crate::actionable_wake_producer::ActionableWakeProducerStatus;
use crate::workstream_continuation_runtime::ContinuationRuntimeStatus;
use serde_json::Value;
#[cfg(unix)]
use serde_json::json;

/// IPC protocol version. Bump when the wire contract changes.
pub const IPC_PROTOCOL_VERSION: u32 = 3;
/// Number of historical events replayed to new subscribers.
pub const RING_BUFFER_SIZE: usize = 100;
/// Maximum number of concurrent wait subscribers served by one daemon.
pub const MAX_SUBSCRIBERS: usize = 64;
/// Maximum number of concurrent IPC clients, including short status requests.
pub const MAX_IPC_CLIENTS: usize = 128;
/// Per-client outbound frame capacity before the daemon closes a slow client.
pub const CLIENT_WRITER_QUEUE_CAPACITY: usize = RING_BUFFER_SIZE + 16;
/// Maximum bytes accepted for one newline-delimited client request.
pub const MAX_IPC_FRAME_BYTES: usize = 64 * 1024;
/// Maximum serialized bytes retained for one outbound IPC frame.
pub const MAX_IPC_OUTBOUND_FRAME_BYTES: usize = 64 * 1024;
/// Typed retryable refusal when all daemon IPC client slots are occupied.
pub const IPC_ERROR_CLIENT_CAPACITY: &str = "ipc_client_capacity_exceeded";
/// Typed retryable refusal when all daemon wait subscriber slots are occupied.
pub const IPC_ERROR_SUBSCRIBER_CAPACITY: &str = "subscriber_capacity_exceeded";
/// Typed non-retryable refusal for duplicate subscription on one connection.
pub const IPC_ERROR_ALREADY_SUBSCRIBED: &str = "already_subscribed";
/// Typed non-retryable refusal for an oversized inbound request frame.
pub const IPC_ERROR_FRAME_TOO_LARGE: &str = "ipc_frame_too_large";
/// Typed non-retryable refusal for an oversized daemon response or event.
pub const IPC_ERROR_RESPONSE_TOO_LARGE: &str = "ipc_response_frame_too_large";

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
    /// Repositories the daemon is configured to watch, independent of whether
    /// webhook registration has currently succeeded.
    pub configured_repos: Vec<String>,
    /// Exact optional daemon lanes fully available in this process.
    pub capabilities: Vec<String>,
    /// Rate-limit snapshot if known.
    pub rate_limit: Option<Value>,
    /// Redacted durable-continuation lane state.
    pub workstream_continuation: ContinuationRuntimeStatus,
    /// Redacted durable status of the exact-head actionable wake producer.
    #[cfg(unix)]
    pub(crate) actionable_wake_producer: ActionableWakeProducerStatus,
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
struct Subscriber {
    sender: SyncSender<Arc<[u8]>>,
    shutdown: Arc<UnixStream>,
}

#[cfg(unix)]
enum FrameRead {
    Complete,
    Pending,
    End,
    TooLarge,
    Failed,
}

#[cfg(unix)]
#[derive(Default)]
struct SharedState {
    ring: VecDeque<Arc<[u8]>>,
    subscribers: std::collections::BTreeMap<usize, Subscriber>,
    next_id: usize,
    clients: std::collections::BTreeMap<usize, Arc<UnixStream>>,
    next_client_id: usize,
}

#[cfg(unix)]
struct ClientSlot {
    shared: Arc<Mutex<SharedState>>,
    id: usize,
    shutdown: Arc<UnixStream>,
}

#[cfg(unix)]
enum SubscribeError {
    AtCapacity,
    WriterUnavailable,
}

#[cfg(unix)]
impl Drop for ClientSlot {
    fn drop(&mut self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.clients.remove(&self.id);
        }
    }
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

    /// Install a ship-state-list provider for IPC protocol v2 and later.
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
        let writer_domain =
            crate::writer_domain_lease::acquire_for_protected_path(&self.socket_path)?;
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
                        let Ok(shutdown) = stream.try_clone() else {
                            continue;
                        };
                        let Some(client_slot) =
                            try_acquire_client(&shared, MAX_IPC_CLIENTS, Arc::new(shutdown))
                        else {
                            reject_client_at_capacity(stream);
                            continue;
                        };
                        let shared = Arc::clone(&shared);
                        let running = Arc::clone(&running);
                        let status_provider = Arc::clone(&status_provider);
                        let on_stop_request = on_stop_request.clone();
                        let ship_state_list_provider = ship_state_list_provider.clone();
                        thread::spawn(move || {
                            let shutdown = Arc::clone(&client_slot.shutdown);
                            let _client_slot = client_slot;
                            handle_client(
                                stream,
                                &shared,
                                &running,
                                &status_provider,
                                on_stop_request.as_ref(),
                                ship_state_list_provider.as_ref(),
                                &shutdown,
                            );
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }

            if let Ok(_writer_domain) =
                crate::writer_domain_lease::acquire_for_protected_path(&socket_path)
            {
                let _ = std::fs::remove_file(socket_path);
            }
        }));
        drop(writer_domain);

        Ok(())
    }

    /// Stop the listener, drop subscribers, and remove the socket.
    pub fn stop(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.running.store(false, Ordering::Release);
        let clients = self.shared.lock().map_or_else(
            |_| Vec::new(),
            |shared| shared.clients.values().cloned().collect::<Vec<_>>(),
        );
        for client in &clients {
            let _ = client.shutdown(Shutdown::Read);
        }
        if let Some(thread) = self.listener_thread.take() {
            let _ = thread.join();
        }
        close_lingering_clients(&self.shared, Duration::from_secs(1));
        if self.socket_path.exists() || self.socket_path.is_symlink() {
            let _writer_domain =
                crate::writer_domain_lease::acquire_for_protected_path(&self.socket_path)?;
            let _ = std::fs::remove_file(&self.socket_path);
        }
        Ok(())
    }

    /// Broadcast an event to connected subscribers and append it to the ring buffer.
    pub fn broadcast_event(&self, event: Value) {
        let frame = encode_json_line(event_frame(event));
        let mut shared = self.shared.lock().expect("shared lock");
        let Some(frame) = frame else {
            let error = encoded_error_frame(
                IPC_ERROR_RESPONSE_TOO_LARGE,
                "daemon IPC event exceeds 65536 serialized bytes",
                false,
            );
            for (_, subscriber) in std::mem::take(&mut shared.subscribers) {
                if subscriber.sender.try_send(error.clone()).is_ok() {
                    let _ = subscriber.shutdown.shutdown(Shutdown::Read);
                } else {
                    let _ = subscriber.shutdown.shutdown(Shutdown::Both);
                }
            }
            return;
        };
        shared.ring.push_back(frame.clone());
        while shared.ring.len() > RING_BUFFER_SIZE {
            let _ = shared.ring.pop_front();
        }

        let evicted = shared
            .subscribers
            .iter()
            .filter_map(|(id, subscriber)| {
                subscriber.sender.try_send(frame.clone()).err().map(|_| *id)
            })
            .collect::<Vec<_>>();
        for id in evicted {
            if let Some(subscriber) = shared.subscribers.remove(&id) {
                let _ = subscriber.shutdown.shutdown(Shutdown::Both);
            }
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
fn try_acquire_client(
    shared: &Arc<Mutex<SharedState>>,
    max_clients: usize,
    shutdown: Arc<UnixStream>,
) -> Option<ClientSlot> {
    let mut state = shared.lock().ok()?;
    if state.clients.len() >= max_clients {
        return None;
    }
    let id = state.next_client_id;
    state.next_client_id = state.next_client_id.wrapping_add(1);
    state.clients.insert(id, Arc::clone(&shutdown));
    drop(state);
    Some(ClientSlot {
        shared: Arc::clone(shared),
        id,
        shutdown,
    })
}

#[cfg(unix)]
fn close_lingering_clients(shared: &Arc<Mutex<SharedState>>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let clients = shared.lock().map_or_else(
            |_| Vec::new(),
            |shared| shared.clients.values().cloned().collect::<Vec<_>>(),
        );
        if clients.is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            let clients = shared.lock().map_or_else(
                |_| clients,
                |mut shared| std::mem::take(&mut shared.clients).into_values().collect(),
            );
            for client in clients {
                let _ = client.shutdown(Shutdown::Both);
            }
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn reject_client_at_capacity(mut stream: UnixStream) {
    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
    let _ = write_json_line(
        &mut stream,
        &error_frame(
            IPC_ERROR_CLIENT_CAPACITY,
            "daemon IPC client capacity reached",
            true,
        ),
    );
    let _ = stream.shutdown(Shutdown::Both);
}

#[cfg(unix)]
#[allow(clippy::too_many_lines)] // Linear protocol dispatch stays visible in one bounded loop.
fn handle_client(
    stream: UnixStream,
    shared: &Arc<Mutex<SharedState>>,
    running: &Arc<AtomicBool>,
    status_provider: &StatusProvider,
    on_stop_request: Option<&StopRequestCallback>,
    ship_state_list_provider: Option<&ShipStateListProvider>,
    shutdown: &Arc<UnixStream>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let (sender, receiver) = mpsc::sync_channel(CLIENT_WRITER_QUEUE_CAPACITY);
    let writer_stream = stream.try_clone().ok();
    let writer_thread = writer_stream.map(|writer_stream| {
        let _ = writer_stream.set_write_timeout(Some(Duration::from_millis(250)));
        thread::spawn(move || writer_loop(writer_stream, receiver))
    });
    if !send_json(
        &sender,
        Some(shutdown),
        json!({
            "type": "hello",
            "protocol": IPC_PROTOCOL_VERSION,
            "shipyard_version": env!("CARGO_PKG_VERSION"),
        }),
    ) {
        return;
    }

    let mut subscriber_id = None;
    let mut reader = BufReader::new(stream);
    let mut frame = Vec::new();

    while running.load(Ordering::Acquire) {
        match read_bounded_frame(&mut reader, &mut frame) {
            FrameRead::Pending => {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            FrameRead::TooLarge => {
                let _ = send_json(
                    &sender,
                    Some(shutdown),
                    error_frame(
                        IPC_ERROR_FRAME_TOO_LARGE,
                        "daemon IPC request frame exceeds 65536 bytes",
                        false,
                    ),
                );
                break;
            }
            FrameRead::End | FrameRead::Failed => break,
            FrameRead::Complete => {}
        }
        let Ok(message) = serde_json::from_slice::<Value>(&frame) else {
            frame.clear();
            continue;
        };
        frame.clear();
        let Some(msg_type) = message.get("type").and_then(Value::as_str) else {
            continue;
        };

        match msg_type {
            "subscribe" => {
                if subscriber_id.is_some() {
                    let _ = send_json(
                        &sender,
                        Some(shutdown),
                        error_frame(
                            IPC_ERROR_ALREADY_SUBSCRIBED,
                            "IPC connection is already subscribed",
                            false,
                        ),
                    );
                    break;
                }
                match register_subscriber(shared, &sender, Some(shutdown)) {
                    Ok(id) => subscriber_id = Some(id),
                    Err(SubscribeError::AtCapacity) => {
                        let _ = send_json(
                            &sender,
                            Some(shutdown),
                            error_frame(
                                IPC_ERROR_SUBSCRIBER_CAPACITY,
                                "daemon wait subscriber capacity reached",
                                true,
                            ),
                        );
                        break;
                    }
                    Err(SubscribeError::WriterUnavailable) => break,
                }
            }
            "status" => {
                let subscribers = shared.lock().expect("shared lock").subscribers.len();
                let mut state = status_provider();
                state.subscribers = subscribers;
                if !send_json(&sender, Some(shutdown), status_frame(&state)) {
                    break;
                }
            }
            "stop" => {
                if let Some(callback) = on_stop_request {
                    callback();
                }
            }
            "ship-state-list" => {
                let states = ship_state_list_provider
                    .map(|provider| provider())
                    .unwrap_or_default();
                if !send_json(
                    &sender,
                    Some(shutdown),
                    json!({"type": "ship-state-list", "states": states}),
                ) {
                    break;
                }
            }
            _ => {}
        }
    }

    if let Some(id) = subscriber_id {
        let _ = shared
            .lock()
            .map(|mut shared| shared.subscribers.remove(&id));
    }
    if enqueue_writer(&sender, goodbye_frame()).is_err() {
        close_stream(Some(shutdown));
    }
    drop(sender);
    if let Some(writer_thread) = writer_thread {
        let _ = writer_thread.join();
    }
}

#[cfg(unix)]
fn register_subscriber(
    shared: &Arc<Mutex<SharedState>>,
    sender: &SyncSender<Arc<[u8]>>,
    shutdown: Option<&Arc<UnixStream>>,
) -> Result<usize, SubscribeError> {
    let mut shared = shared.lock().expect("shared lock");
    if shared.subscribers.len() >= MAX_SUBSCRIBERS {
        return Err(SubscribeError::AtCapacity);
    }
    let id = shared.next_id;
    shared.next_id = shared.next_id.wrapping_add(1);
    if !shared
        .ring
        .iter()
        .all(|event| enqueue_writer(sender, Arc::clone(event)).is_ok())
    {
        close_stream(shutdown.map(Arc::as_ref));
        return Err(SubscribeError::WriterUnavailable);
    }
    let shutdown = shutdown.cloned().ok_or(SubscribeError::WriterUnavailable)?;
    shared.subscribers.insert(
        id,
        Subscriber {
            sender: sender.clone(),
            shutdown,
        },
    );
    Ok(id)
}

#[cfg(unix)]
fn send_json(sender: &SyncSender<Arc<[u8]>>, shutdown: Option<&UnixStream>, value: Value) -> bool {
    let Some(frame) = encode_json_line(value) else {
        if enqueue_writer(
            sender,
            encoded_error_frame(
                IPC_ERROR_RESPONSE_TOO_LARGE,
                "daemon IPC response exceeds 65536 serialized bytes",
                false,
            ),
        )
        .is_err()
        {
            close_stream(shutdown);
        }
        return false;
    };
    if enqueue_writer(sender, frame).is_ok() {
        true
    } else {
        close_stream(shutdown);
        false
    }
}

#[cfg(unix)]
fn read_bounded_frame(reader: &mut BufReader<UnixStream>, frame: &mut Vec<u8>) -> FrameRead {
    loop {
        let available = match reader.fill_buf() {
            Ok([]) if frame.is_empty() => return FrameRead::End,
            Ok([]) => return FrameRead::Complete,
            Ok(available) => available,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return FrameRead::Pending;
            }
            Err(_) => return FrameRead::Failed,
        };

        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        if frame.len().saturating_add(consumed) > MAX_IPC_FRAME_BYTES {
            return FrameRead::TooLarge;
        }
        frame.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if newline.is_some() {
            return FrameRead::Complete;
        }
    }
}

#[cfg(unix)]
fn enqueue_writer(
    sender: &SyncSender<Arc<[u8]>>,
    message: Arc<[u8]>,
) -> Result<(), TrySendError<Arc<[u8]>>> {
    sender.try_send(message)
}

#[cfg(unix)]
fn encode_json_line(value: Value) -> Option<Arc<[u8]>> {
    let mut frame = serde_json::to_vec(&value).ok()?;
    drop(value);
    frame.push(b'\n');
    (frame.len() <= MAX_IPC_OUTBOUND_FRAME_BYTES).then(|| Arc::from(frame.into_boxed_slice()))
}

#[cfg(unix)]
fn encoded_error_frame(code: &str, message: &str, retryable: bool) -> Arc<[u8]> {
    encode_json_line(error_frame(code, message, retryable)).expect("small IPC error frame")
}

#[cfg(unix)]
fn goodbye_frame() -> Arc<[u8]> {
    Arc::from(&b"{\"type\":\"goodbye\"}\n"[..])
}

#[cfg(unix)]
fn close_stream(stream: Option<&UnixStream>) {
    if let Some(stream) = stream {
        let _ = stream.shutdown(Shutdown::Both);
    }
}

#[cfg(unix)]
fn writer_loop(mut stream: UnixStream, receiver: mpsc::Receiver<Arc<[u8]>>) {
    for frame in receiver {
        if stream.write_all(&frame).is_err() || stream.flush().is_err() {
            break;
        }
        if frame.as_ref() == b"{\"type\":\"goodbye\"}\n" {
            break;
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
        "configured_repos": state.configured_repos,
        "capabilities": state.capabilities,
        "rate_limit": state.rate_limit,
        "workstream_continuation": state.workstream_continuation,
        "actionable_wake_producer": state.actionable_wake_producer,
        "last_error": state.last_error,
        "shipyard_version": env!("CARGO_PKG_VERSION"),
        "protocol": IPC_PROTOCOL_VERSION,
    })
}

#[cfg(unix)]
fn event_frame(event: Value) -> Value {
    match event {
        Value::Object(mut frame) => {
            frame.insert("type".to_owned(), Value::from("event"));
            Value::Object(frame)
        }
        event => json!({
            "type": "event",
            "payload": event,
        }),
    }
}

#[cfg(unix)]
fn error_frame(code: &str, message: &str, retryable: bool) -> Value {
    json!({
        "type": "error",
        "code": code,
        "message": message,
        "retryable": retryable,
    })
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
        IPC_PROTOCOL_VERSION, IpcServer, IpcState, Subscriber, read_daemon_ship_state_list,
        read_daemon_status,
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
            configured_repos: vec!["org/repo".to_owned(), "org/pending".to_owned()],
            capabilities: Vec::new(),
            rate_limit: None,
            workstream_continuation:
                crate::workstream_continuation_runtime::ContinuationRuntimeStatus::default(),
            actionable_wake_producer:
                crate::actionable_wake_producer::ActionableWakeProducerStatus::default(),
            last_error: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn status_frame_exposes_only_redacted_continuation_state() {
        let mut state = dummy_state();
        state.workstream_continuation =
            crate::workstream_continuation_runtime::ContinuationRuntimeStatus {
                state: crate::workstream_continuation_runtime::ContinuationRuntimeState::Refused,
                reason_code: Some("activation_drift".to_owned()),
            };
        let frame = super::status_frame(&state);
        assert_eq!(frame["workstream_continuation"]["state"], "refused");
        assert_eq!(
            frame["workstream_continuation"]["reason_code"],
            "activation_drift"
        );
        let encoded = frame.to_string();
        assert!(!encoded.contains("wake-") && !encoded.contains("route-"));
    }

    #[cfg(unix)]
    #[test]
    fn daemon_capability_is_absent_by_default_and_exact_when_available() {
        assert_eq!(
            super::status_frame(&dummy_state())["capabilities"],
            json!([])
        );
        let mut present = dummy_state();
        present.capabilities =
            vec![crate::parallel_proof_canary_job_adapter::DAEMON_CANARY_JOB_CAPABILITY.to_owned()];
        assert_eq!(
            super::status_frame(&present)["capabilities"],
            json!(["parallel_proof_canary_job_v1"])
        );
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
    fn subscriber_capacity_is_typed_and_fail_closed() {
        let shared = std::sync::Arc::new(std::sync::Mutex::new(super::SharedState::default()));
        let (sender, _receiver) = std::sync::mpsc::sync_channel(1);
        let (shutdown, _peer) = UnixStream::pair().expect("socket pair");
        for id in 0..super::MAX_SUBSCRIBERS {
            shared.lock().expect("shared").subscribers.insert(
                id,
                Subscriber {
                    sender: sender.clone(),
                    shutdown: std::sync::Arc::new(shutdown.try_clone().expect("clone shutdown")),
                },
            );
        }

        let shutdown = std::sync::Arc::new(shutdown);
        let result = super::register_subscriber(&shared, &sender, Some(&shutdown));
        assert!(matches!(result, Err(super::SubscribeError::AtCapacity)));
        let error = super::error_frame(super::IPC_ERROR_SUBSCRIBER_CAPACITY, "capacity", true);
        assert_eq!(error["type"], "error");
        assert_eq!(error["code"], "subscriber_capacity_exceeded");
        assert_eq!(error["retryable"], true);
    }

    #[cfg(unix)]
    #[test]
    fn ipc_client_capacity_rejects_before_spawning_another_handler() {
        let shared = std::sync::Arc::new(std::sync::Mutex::new(super::SharedState::default()));
        let (shutdown, _peer) = UnixStream::pair().expect("slot socket");
        let shutdown = std::sync::Arc::new(shutdown);
        let first = super::try_acquire_client(&shared, 1, shutdown.clone()).expect("first slot");
        assert!(super::try_acquire_client(&shared, 1, shutdown.clone()).is_none());

        let (server, client) = UnixStream::pair().expect("socket pair");
        super::reject_client_at_capacity(server);
        let mut line = String::new();
        BufReader::new(client)
            .read_line(&mut line)
            .expect("capacity error");
        let error: Value = serde_json::from_str(line.trim()).expect("json");
        assert_eq!(error["type"], "error");
        assert_eq!(error["code"], "ipc_client_capacity_exceeded");
        assert_eq!(error["retryable"], true);
        drop(first);
        assert!(super::try_acquire_client(&shared, 1, shutdown).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_subscribe_does_not_allocate_another_slot() {
        let socket_path = short_socket_path();
        let mut server = IpcServer::new(socket_path.clone(), dummy_state);
        server.start().expect("start");

        let mut stream = UnixStream::connect(&socket_path).expect("connect");
        stream
            .write_all(b"{\"type\":\"subscribe\"}\n{\"type\":\"subscribe\"}\n")
            .expect("duplicate subscribe");
        let lines = read_lines(stream, 3);

        assert_eq!(lines[0]["type"], "hello");
        assert_eq!(lines[1]["type"], "error");
        assert_eq!(lines[1]["code"], "already_subscribed");
        assert_eq!(lines[1]["retryable"], false);
        assert_eq!(lines[2]["type"], "goodbye");
        let deadline = Instant::now() + Duration::from_secs(2);
        while server.subscriber_count() != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(server.subscriber_count(), 0);

        server.stop().expect("stop");
    }

    #[cfg(unix)]
    #[test]
    fn full_subscriber_queue_evicts_and_closes_the_client() {
        let socket_path = short_socket_path();
        let server = IpcServer::new(socket_path, dummy_state);
        let (sender, _receiver) = std::sync::mpsc::sync_channel(1);
        sender
            .try_send(super::encode_json_line(json!({"type":"occupied"})).expect("frame"))
            .expect("fill queue");
        let (shutdown, mut peer) = UnixStream::pair().expect("socket pair");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("read timeout");
        server.shared.lock().expect("shared").subscribers.insert(
            7,
            Subscriber {
                sender,
                shutdown: std::sync::Arc::new(shutdown),
            },
        );

        server.broadcast_event(json!({"kind":"workflow_run"}));

        assert_eq!(server.subscriber_count(), 0);
        let mut byte = [0_u8; 1];
        assert_eq!(std::io::Read::read(&mut peer, &mut byte).expect("read"), 0);
    }

    #[cfg(unix)]
    #[test]
    fn oversized_event_is_not_retained_and_closes_subscriber_with_typed_error() {
        let socket_path = short_socket_path();
        let mut server = IpcServer::new(socket_path.clone(), dummy_state);
        server.start().expect("start");

        let client = std::thread::spawn(move || {
            let mut stream = UnixStream::connect(socket_path).expect("connect");
            stream
                .write_all(b"{\"type\":\"subscribe\"}\n")
                .expect("subscribe");
            read_lines(stream, 3)
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while server.subscriber_count() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

        server.broadcast_event(json!({
            "payload": "x".repeat(super::MAX_IPC_OUTBOUND_FRAME_BYTES),
        }));
        let lines = client.join().expect("join");

        assert_eq!(lines[0]["type"], "hello");
        assert_eq!(lines[1]["type"], "error");
        assert_eq!(lines[1]["code"], "ipc_response_frame_too_large");
        assert_eq!(lines[1]["retryable"], false);
        assert_eq!(lines[2]["type"], "goodbye");
        assert!(server.shared.lock().expect("shared").ring.is_empty());
        server.stop().expect("stop");
    }

    #[cfg(unix)]
    #[test]
    fn stop_closes_idle_admitted_client_after_goodbye() {
        let socket_path = short_socket_path();
        let mut server = IpcServer::new(socket_path.clone(), dummy_state);
        server.start().expect("start");

        let stream = UnixStream::connect(socket_path).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("hello");
        assert_eq!(
            serde_json::from_str::<Value>(line.trim()).expect("hello json")["type"],
            "hello"
        );

        server.stop().expect("stop");
        line.clear();
        reader.read_line(&mut line).expect("goodbye");
        assert_eq!(
            serde_json::from_str::<Value>(line.trim()).expect("goodbye json")["type"],
            "goodbye"
        );
        line.clear();
        assert_eq!(reader.read_line(&mut line).expect("closed"), 0);
        assert!(server.shared.lock().expect("shared").clients.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn oversized_inbound_frame_is_typed_and_closed() {
        let socket_path = short_socket_path();
        let mut server = IpcServer::new(socket_path.clone(), dummy_state);
        server.start().expect("start");

        let mut stream = UnixStream::connect(&socket_path).expect("connect");
        let mut oversized = vec![b'x'; super::MAX_IPC_FRAME_BYTES + 1];
        oversized.push(b'\n');
        stream.write_all(&oversized).expect("oversized frame");
        let lines = read_lines(stream, 3);

        assert_eq!(lines[0]["type"], "hello");
        assert_eq!(lines[1]["type"], "error");
        assert_eq!(lines[1]["code"], "ipc_frame_too_large");
        assert_eq!(lines[1]["retryable"], false);
        assert_eq!(lines[2]["type"], "goodbye");

        server.stop().expect("stop");
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
        assert_eq!(lines[1]["configured_repos"][1], "org/pending");
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
        assert_eq!(status["configured_repos"][1], "org/pending");
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
