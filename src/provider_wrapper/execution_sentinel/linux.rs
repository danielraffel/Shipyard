//! Linux-only provider descendant custody in a dedicated subreaper process.

use std::fs::File;
use std::io::{IoSliceMut, Read, Seek, SeekFrom, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::super::EXECUTION_SENTINEL_FD_ENV;

const SPEC_SCHEMA_VERSION: u32 = 3;
const RESULT_SCHEMA_VERSION: u32 = 3;
const STARTUP_MARKER: u8 = 1;
pub(crate) const READY_FRAME: &[u8] = b"shipyard-provider-sentinel-ready-v3\n";
pub(crate) const RESULT_FRAME_PREFIX: &[u8] = b"shipyard-provider-sentinel-result-v3 ";
pub(crate) const MAX_SPEC_BYTES: usize = 320 * 1024;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_CAPTURE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CHILDREN_BYTES: usize = 1024 * 1024;
pub(crate) const SPEC_ADMISSION_BUDGET: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const CLEANUP_BUDGET: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LinuxSentinelSupervisorSpecV3 {
    pub(crate) schema_version: u32,
    pub(crate) request_bytes: Vec<u8>,
    pub(crate) max_stdout_bytes: u64,
    pub(crate) max_stderr_bytes: u64,
    pub(crate) provider_deadline_millis: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LinuxSupervisorProviderV3 {
    Success,
    Nonzero,
    TimedOut,
    WaitUnknown,
    ControlEof,
    StartupUnproven,
    OutputLimit,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LinuxSupervisorCleanupV3 {
    Clean,
    ResidualTerminated,
    Unproven,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LinuxSupervisorResultV3 {
    pub(crate) schema_version: u32,
    pub(crate) provider: LinuxSupervisorProviderV3,
    pub(crate) cleanup: LinuxSupervisorCleanupV3,
    pub(crate) stdout: Vec<u8>,
}

enum ControlState {
    Alive,
    Eof,
    Unreadable,
}

struct SupervisorIo {
    input: File,
    output: File,
    error: File,
    sentinel: File,
    startup_guard: std::os::fd::OwnedFd,
    sentinel_writer: std::os::fd::OwnedFd,
}

/// Run the production Linux supervisor from anonymous inherited capabilities.
pub(crate) fn run_linux_sentinel_supervisor() -> Result<(), String> {
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .map_err(|error| format!("provider sentinel cannot disable dumpability: {error}"))?;
    let stdin = std::io::stdin();
    let mut control = stdin.lock();
    let (executable, spec) = receive_admission(&mut control)?;
    rustix::process::set_child_subreaper(rustix::process::Pid::from_raw(1))
        .map_err(|error| format!("provider sentinel cannot become subreaper: {error}"))?;

    let SupervisorIo {
        input,
        mut output,
        error,
        mut sentinel,
        startup_guard,
        sentinel_writer,
    } = prepare_supervisor_io(&spec)?;

    let provider_deadline = Instant::now()
        .checked_add(Duration::from_millis(spec.provider_deadline_millis))
        .ok_or_else(|| "provider sentinel deadline overflows".to_owned())?;
    let output_child = output
        .try_clone()
        .map_err(|error| format!("provider sentinel stdout cannot be cloned: {error}"))?;
    let error_child = error
        .try_clone()
        .map_err(|error| format!("provider sentinel stderr cannot be cloned: {error}"))?;
    let mut provider = launch_provider(
        &executable,
        input,
        output_child,
        error_child,
        sentinel_writer,
    )
    .map_err(|error| format!("provider sentinel cannot launch wrapper: {error}"))?;
    let provider_pid = rustix::process::Pid::from_child(&provider);
    let mut channel = std::io::stdout().lock();

    let startup = await_startup(
        &mut sentinel,
        &mut control,
        &mut provider,
        &output,
        &error,
        &spec,
        provider_deadline,
    );
    let mut startup_guard = Some(startup_guard);
    let (provider_state, publication_error) = match startup {
        Ok(()) => {
            drop(startup_guard.take());
            match channel
                .write_all(READY_FRAME)
                .and_then(|()| channel.flush())
            {
                Ok(()) => (
                    await_provider(
                        &mut control,
                        &mut provider,
                        &output,
                        &error,
                        &spec,
                        provider_deadline,
                    ),
                    None,
                ),
                Err(error) => (
                    LinuxSupervisorProviderV3::StartupUnproven,
                    Some(format!(
                        "provider sentinel ready frame cannot be published: {error}"
                    )),
                ),
            }
        }
        Err(state) => (state, None),
    };
    drop(startup_guard);

    let cleanup_deadline = Instant::now()
        .checked_add(CLEANUP_BUDGET)
        .unwrap_or(provider_deadline);
    let mut cleanup = cleanup_descendants(
        &mut sentinel,
        provider_pid,
        provider_state,
        cleanup_deadline,
    );
    if publication_error.is_some() {
        cleanup = LinuxSupervisorCleanupV3::Unproven;
    }
    let result_write = publish_result(
        &mut channel,
        provider_state,
        cleanup,
        &mut output,
        &error,
        &spec,
    );
    if let Some(error) = publication_error {
        return Err(error);
    }
    result_write
}

fn receive_admission(
    control: &mut impl Read,
) -> Result<(OwnedFd, LinuxSentinelSupervisorSpecV3), String> {
    let stdin_flags = rustix::fs::fcntl_getfl(std::io::stdin())
        .map_err(|error| format!("provider sentinel control flags are unreadable: {error}"))?;
    rustix::fs::fcntl_setfl(std::io::stdin(), stdin_flags | rustix::fs::OFlags::NONBLOCK)
        .map_err(|error| format!("provider sentinel control cannot be nonblocking: {error}"))?;
    let admission_deadline = Instant::now() + SPEC_ADMISSION_BUDGET;
    let executable = receive_executable(admission_deadline)?;
    validate_received_executable(&executable)?;
    ensure_admission_deadline(admission_deadline)?;
    close_unrelated_inherited_fds(executable.as_raw_fd())?;
    let spec = read_control_spec(control, admission_deadline)?;
    Ok((executable, spec))
}

fn prepare_supervisor_io(spec: &LinuxSentinelSupervisorSpecV3) -> Result<SupervisorIo, String> {
    let input = tempfile::tempfile()
        .map_err(|error| format!("provider sentinel input cannot be created: {error}"))?;
    let mut input = avoid_reserved_sentinel_fd(input)?;
    input
        .write_all(&spec.request_bytes)
        .and_then(|()| input.seek(SeekFrom::Start(0)).map(drop))
        .map_err(|error| format!("provider sentinel input cannot be prepared: {error}"))?;
    let output = tempfile::tempfile()
        .map_err(|error| format!("provider sentinel stdout cannot be created: {error}"))?;
    let error = tempfile::tempfile()
        .map_err(|error| format!("provider sentinel stderr cannot be created: {error}"))?;
    let (sentinel_reader, startup_guard) = rustix::pipe::pipe_with(
        rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
    )
    .map_err(|error| format!("provider sentinel pipe cannot be created: {error}"))?;
    let sentinel_writer = rustix::io::fcntl_dupfd_cloexec(&startup_guard, 3)
        .map_err(|error| format!("provider sentinel writer cannot be duplicated: {error}"))?;
    Ok(SupervisorIo {
        input,
        output,
        error,
        sentinel: File::from(sentinel_reader),
        startup_guard,
        sentinel_writer,
    })
}

fn avoid_reserved_sentinel_fd(file: File) -> Result<File, String> {
    if file.as_raw_fd() != 9 {
        return Ok(file);
    }
    Ok(File::from(
        rustix::io::fcntl_dupfd_cloexec(&file, 10)
            .map_err(|error| format!("provider request descriptor cannot avoid fd9: {error}"))?,
    ))
}

fn publish_result(
    channel: &mut impl Write,
    provider_state: LinuxSupervisorProviderV3,
    cleanup: LinuxSupervisorCleanupV3,
    output: &mut File,
    error: &File,
    spec: &LinuxSentinelSupervisorSpecV3,
) -> Result<(), String> {
    let (provider, stdout) = if output_exceeds(error, spec.max_stderr_bytes) {
        (LinuxSupervisorProviderV3::OutputLimit, Vec::new())
    } else {
        match read_capture(output, spec.max_stdout_bytes) {
            Some(bytes) => (provider_state, bytes),
            None => (LinuxSupervisorProviderV3::OutputLimit, Vec::new()),
        }
    };
    let bytes = serde_json::to_vec(&LinuxSupervisorResultV3 {
        schema_version: RESULT_SCHEMA_VERSION,
        provider,
        cleanup,
        stdout,
    })
    .map_err(|error| format!("provider sentinel result cannot be serialized: {error}"))?;
    channel
        .write_all(RESULT_FRAME_PREFIX)
        .and_then(|()| channel.write_all(&bytes))
        .and_then(|()| channel.write_all(b"\n"))
        .and_then(|()| channel.flush())
        .map_err(|error| format!("provider sentinel result frame cannot be published: {error}"))
}

fn read_control_spec(
    control: &mut impl Read,
    deadline: Instant,
) -> Result<LinuxSentinelSupervisorSpecV3, String> {
    let mut length = [0u8; 4];
    read_exact_until(control, &mut length, deadline)
        .map_err(|error| format!("provider sentinel spec length cannot be read: {error}"))?;
    let length = usize::try_from(u32::from_be_bytes(length))
        .map_err(|_| "provider sentinel spec length cannot be represented".to_owned())?;
    if length == 0 || length > MAX_SPEC_BYTES {
        return Err("provider sentinel spec is not bounded".into());
    }
    let mut bytes = vec![0; length];
    read_exact_until(control, &mut bytes, deadline)
        .map_err(|error| format!("provider sentinel spec cannot be read: {error}"))?;
    let spec = parse_spec_until(&bytes, deadline)?;
    validate_spec_until(&spec, deadline)?;
    Ok(spec)
}

fn parse_spec_until(
    bytes: &[u8],
    deadline: Instant,
) -> Result<LinuxSentinelSupervisorSpecV3, String> {
    let spec = serde_json::from_slice(bytes)
        .map_err(|error| format!("provider sentinel spec is malformed: {error}"))?;
    ensure_admission_deadline(deadline)?;
    Ok(spec)
}

fn validate_spec_until(
    spec: &LinuxSentinelSupervisorSpecV3,
    deadline: Instant,
) -> Result<(), String> {
    validate_spec(spec)?;
    ensure_admission_deadline(deadline)
}

fn ensure_admission_deadline(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        Err("provider sentinel executable admission timed out".into())
    } else {
        Ok(())
    }
}

fn read_exact_until(
    reader: &mut impl Read,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> std::io::Result<()> {
    while !bytes.is_empty() {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "admission deadline elapsed",
            ));
        }
        match reader.read(bytes) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => bytes = &mut bytes[count..],
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(
                    POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn validate_spec(spec: &LinuxSentinelSupervisorSpecV3) -> Result<(), String> {
    if spec.schema_version != SPEC_SCHEMA_VERSION
        || spec.request_bytes.len() > MAX_REQUEST_BYTES
        || spec.max_stdout_bytes == 0
        || spec.max_stdout_bytes > MAX_CAPTURE_BYTES
        || spec.max_stderr_bytes == 0
        || spec.max_stderr_bytes > MAX_CAPTURE_BYTES
        || spec.provider_deadline_millis == 0
        || spec.provider_deadline_millis > 86_400_000
    {
        return Err("provider sentinel spec is invalid".into());
    }
    Ok(())
}

fn receive_executable(deadline: Instant) -> Result<OwnedFd, String> {
    while Instant::now() < deadline {
        let mut marker = [0u8; 1];
        let mut iov = [IoSliceMut::new(&mut marker)];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
        let mut ancillary = rustix::net::RecvAncillaryBuffer::new(&mut space);
        match rustix::net::recvmsg(
            std::io::stdin(),
            &mut iov,
            &mut ancillary,
            rustix::net::RecvFlags::CMSG_CLOEXEC | rustix::net::RecvFlags::DONTWAIT,
        ) {
            Ok(message)
                if message.bytes == 1
                    && marker[0] == STARTUP_MARKER
                    && !message.flags.intersects(
                        rustix::net::ReturnFlags::TRUNC | rustix::net::ReturnFlags::CTRUNC,
                    ) =>
            {
                let mut received = None;
                for message in ancillary.drain() {
                    match message {
                        rustix::net::RecvAncillaryMessage::ScmRights(fds) => {
                            for fd in fds {
                                if received.replace(fd).is_some() {
                                    return Err("provider sentinel received multiple executable descriptors".into());
                                }
                            }
                        }
                        _ => {
                            return Err(
                                "provider sentinel received unexpected ancillary data".into()
                            );
                        }
                    }
                }
                return received.ok_or_else(|| {
                    "provider sentinel executable descriptor is missing".to_owned()
                });
            }
            Ok(_) => return Err("provider sentinel executable frame is malformed".into()),
            Err(rustix::io::Errno::AGAIN) => std::thread::sleep(
                POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            ),
            Err(error) => {
                return Err(format!(
                    "provider sentinel executable descriptor cannot be received: {error}"
                ));
            }
        }
    }
    Err("provider sentinel executable admission timed out".into())
}

fn validate_received_executable(executable: &OwnedFd) -> Result<(), String> {
    let required_seals = rustix::fs::SealFlags::WRITE
        | rustix::fs::SealFlags::GROW
        | rustix::fs::SealFlags::SHRINK
        | rustix::fs::SealFlags::SEAL;
    let observed_seals = rustix::fs::fcntl_get_seals(executable)
        .map_err(|error| format!("provider sentinel executable seals are unreadable: {error}"))?;
    if !observed_seals.contains(required_seals) {
        return Err("provider sentinel executable is not fully sealed".into());
    }
    let duplicate = rustix::io::fcntl_dupfd_cloexec(executable, 3)
        .map_err(|error| format!("provider sentinel executable cannot be inspected: {error}"))?;
    let metadata = File::from(duplicate)
        .metadata()
        .map_err(|error| format!("provider sentinel executable metadata is unreadable: {error}"))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.permissions().mode() & 0o111 == 0 {
        return Err("provider sentinel executable metadata is invalid".into());
    }
    Ok(())
}

fn close_unrelated_inherited_fds(executable_fd: i32) -> Result<(), String> {
    let descriptors = {
        let entries = std::fs::read_dir("/proc/self/fd")
            .map_err(|error| format!("provider sentinel fd inventory is unreadable: {error}"))?;
        let mut descriptors = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("provider sentinel fd inventory is unstable: {error}"))?;
            if let Ok(raw) = entry.file_name().to_string_lossy().parse::<i32>() {
                descriptors.push(raw);
            }
        }
        descriptors
    };
    for raw in descriptors {
        if raw > 2 && raw != executable_fd {
            let _ = nix::unistd::close(raw);
        }
    }
    Ok(())
}

fn launch_provider(
    executable: &OwnedFd,
    input: File,
    output: File,
    error: File,
    sentinel_writer: std::os::fd::OwnedFd,
) -> std::io::Result<std::process::Child> {
    rustix::io::fcntl_setfd(&input, rustix::io::FdFlags::empty())?;
    let input_fd = input.as_raw_fd();
    let executable_child = rustix::io::fcntl_dupfd_cloexec(executable, 10)?;
    rustix::io::fcntl_setfd(&executable_child, rustix::io::FdFlags::empty())?;
    let executable_path = format!("/proc/self/fd/{}", executable_child.as_raw_fd());
    let script = format!(
        "exec 9>&0 || exit 126\nexec 0<\"/proc/self/fd/{input_fd}\" || exit 126\nexec {input_fd}<&- || exit 126\nprintf '\\001' >&9 || exit 126\nexec \"$@\""
    );
    let environment = std::env::vars_os()
        .filter(|(name, _)| name != "SHIPYARD_PROVIDER_SENTINEL_SUPERVISOR_INTERNAL");
    let mut command = Command::new("/bin/sh");
    command.args(["-c", &script, "shipyard-provider-sentinel"]);
    command.arg(executable_path);
    let child = command
        .env_clear()
        .envs(environment)
        .env(EXECUTION_SENTINEL_FD_ENV, "9")
        .stdin(Stdio::from(sentinel_writer))
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(error))
        .spawn();
    drop(input);
    drop(executable_child);
    child
}

fn output_exceeds(file: &File, limit: u64) -> bool {
    file.metadata()
        .map_or(true, |metadata| metadata.len() > limit)
}

fn read_capture(file: &mut File, limit: u64) -> Option<Vec<u8>> {
    if output_exceeds(file, limit) {
        return None;
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

fn await_startup(
    sentinel: &mut File,
    control: &mut impl Read,
    provider: &mut std::process::Child,
    output: &File,
    error: &File,
    spec: &LinuxSentinelSupervisorSpecV3,
    deadline: Instant,
) -> Result<(), LinuxSupervisorProviderV3> {
    let mut marker = [0u8; 1];
    while Instant::now() < deadline {
        if output_exceeds(output, spec.max_stdout_bytes)
            || output_exceeds(error, spec.max_stderr_bytes)
        {
            return Err(LinuxSupervisorProviderV3::OutputLimit);
        }
        if !matches!(control_state(control), ControlState::Alive) {
            return Err(LinuxSupervisorProviderV3::ControlEof);
        }
        match sentinel.read(&mut marker) {
            Ok(1) if marker[0] == STARTUP_MARKER => return Ok(()),
            Ok(1) => return Err(LinuxSupervisorProviderV3::StartupUnproven),
            Ok(0) => {}
            Ok(_) => unreachable!("one-byte sentinel read returned more than one byte"),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return Err(LinuxSupervisorProviderV3::StartupUnproven),
        }
        match provider.try_wait() {
            Ok(Some(_)) | Err(_) => return Err(LinuxSupervisorProviderV3::StartupUnproven),
            Ok(None) => {}
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
    Err(LinuxSupervisorProviderV3::TimedOut)
}

fn await_provider(
    control: &mut impl Read,
    provider: &mut std::process::Child,
    output: &File,
    error: &File,
    spec: &LinuxSentinelSupervisorSpecV3,
    deadline: Instant,
) -> LinuxSupervisorProviderV3 {
    while Instant::now() < deadline {
        if output_exceeds(output, spec.max_stdout_bytes)
            || output_exceeds(error, spec.max_stderr_bytes)
        {
            return LinuxSupervisorProviderV3::OutputLimit;
        }
        match control_state(control) {
            ControlState::Alive => {}
            ControlState::Eof | ControlState::Unreadable => {
                return LinuxSupervisorProviderV3::ControlEof;
            }
        }
        match provider.try_wait() {
            Ok(Some(status)) => return provider_status(status),
            Ok(None) => {}
            Err(_) => return LinuxSupervisorProviderV3::WaitUnknown,
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
    LinuxSupervisorProviderV3::TimedOut
}

fn control_state(control: &mut impl Read) -> ControlState {
    let mut byte = [0u8; 1];
    match control.read(&mut byte) {
        Ok(0) => ControlState::Eof,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => ControlState::Alive,
        Ok(_) | Err(_) => ControlState::Unreadable,
    }
}

fn provider_status(status: ExitStatus) -> LinuxSupervisorProviderV3 {
    if status.success() {
        LinuxSupervisorProviderV3::Success
    } else {
        LinuxSupervisorProviderV3::Nonzero
    }
}

fn cleanup_descendants(
    sentinel: &mut File,
    provider_pid: rustix::process::Pid,
    provider_state: LinuxSupervisorProviderV3,
    deadline: Instant,
) -> LinuxSupervisorCleanupV3 {
    let mut residual = false;
    while Instant::now() < deadline {
        let Some(children) = direct_children(deadline) else {
            return LinuxSupervisorCleanupV3::Unproven;
        };
        for pid in children {
            if Instant::now() >= deadline {
                return LinuxSupervisorCleanupV3::Unproven;
            }
            if pid != provider_pid || matches!(provider_state, LinuxSupervisorProviderV3::Success) {
                residual = true;
            }
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
        if !reap_available(deadline) {
            return LinuxSupervisorCleanupV3::Unproven;
        }
        match sentinel_is_eof(sentinel, deadline) {
            Some(true) => {
                let Some(after_eof) = direct_children(deadline) else {
                    return LinuxSupervisorCleanupV3::Unproven;
                };
                if after_eof.is_empty() && reap_available(deadline) {
                    return if residual {
                        LinuxSupervisorCleanupV3::ResidualTerminated
                    } else {
                        LinuxSupervisorCleanupV3::Clean
                    };
                }
            }
            Some(false) => {}
            None => return LinuxSupervisorCleanupV3::Unproven,
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
    LinuxSupervisorCleanupV3::Unproven
}

fn direct_children(deadline: Instant) -> Option<Vec<rustix::process::Pid>> {
    if Instant::now() >= deadline {
        return None;
    }
    let tid = rustix::thread::gettid().as_raw_nonzero().get();
    let mut raw_children = std::collections::BTreeSet::new();
    collect_children(tid, deadline, &mut raw_children)?;
    #[cfg(test)]
    {
        let leader = i32::try_from(std::process::id()).ok()?;
        if leader != tid {
            collect_children(leader, deadline, &mut raw_children)?;
        }
    }
    raw_children
        .into_iter()
        .map(rustix::process::Pid::from_raw)
        .collect()
}

fn collect_children(
    tid: i32,
    deadline: Instant,
    children: &mut std::collections::BTreeSet<i32>,
) -> Option<()> {
    if Instant::now() >= deadline {
        return None;
    }
    let file = File::open(format!("/proc/self/task/{tid}/children")).ok()?;
    let mut bytes = Vec::with_capacity(MAX_CHILDREN_BYTES.min(4096));
    file.take(MAX_CHILDREN_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_CHILDREN_BYTES || Instant::now() >= deadline {
        return None;
    }
    parse_children(&bytes, deadline, children)
}

fn parse_children(
    bytes: &[u8],
    deadline: Instant,
    children: &mut std::collections::BTreeSet<i32>,
) -> Option<()> {
    let text = std::str::from_utf8(bytes).ok()?;
    for token in text.split_ascii_whitespace() {
        if Instant::now() >= deadline {
            return None;
        }
        if token.starts_with('0') || !token.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        children.insert(token.parse::<i32>().ok()?);
    }
    Some(())
}

fn reap_available(deadline: Instant) -> bool {
    while Instant::now() < deadline {
        match rustix::process::wait(rustix::process::WaitOptions::NOHANG) {
            Ok(None) | Err(rustix::io::Errno::CHILD) => return true,
            Ok(Some(_)) | Err(rustix::io::Errno::INTR) => {}
            Err(_) => return false,
        }
    }
    false
}

fn sentinel_is_eof(sentinel: &mut File, deadline: Instant) -> Option<bool> {
    let mut buffer = [0u8; 64];
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        match sentinel.read(&mut buffer) {
            Ok(0) => return Some(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Some(false),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LinuxSentinelSupervisorSpecV3, parse_spec_until, read_control_spec, sentinel_is_eof,
        validate_spec_until,
    };
    use std::io::{Cursor, Read, Write};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn control_spec_is_exact_bounded_and_path_free() {
        let spec = LinuxSentinelSupervisorSpecV3 {
            schema_version: 3,
            request_bytes: b"original".to_vec(),
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
            provider_deadline_millis: 1,
        };
        let bytes = serde_json::to_vec(&spec).unwrap();
        let mut framed = Vec::new();
        framed.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
        framed.extend_from_slice(&bytes);
        let decoded = read_control_spec(
            &mut Cursor::new(framed),
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(decoded.request_bytes, b"original");
        let mut truncated = Vec::new();
        truncated.extend_from_slice(&8u32.to_be_bytes());
        truncated.extend_from_slice(b"tiny");
        assert!(
            read_control_spec(
                &mut Cursor::new(truncated),
                std::time::Instant::now() + std::time::Duration::from_secs(1)
            )
            .is_err()
        );
    }

    #[test]
    fn control_spec_refuses_deadline_expiry_after_read_parse_and_validation() {
        struct DelayedBody {
            bytes: Cursor<Vec<u8>>,
            reads: usize,
        }

        impl Read for DelayedBody {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                if self.reads == 2 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                self.bytes.read(buffer)
            }
        }

        let spec = LinuxSentinelSupervisorSpecV3 {
            schema_version: 3,
            request_bytes: b"original".to_vec(),
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
            provider_deadline_millis: 1,
        };
        let bytes = serde_json::to_vec(&spec).unwrap();
        let mut framed = Vec::new();
        framed.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
        framed.extend_from_slice(&bytes);
        let mut delayed = DelayedBody {
            bytes: Cursor::new(framed),
            reads: 0,
        };
        assert!(
            read_control_spec(
                &mut delayed,
                std::time::Instant::now() + std::time::Duration::from_millis(10)
            )
            .unwrap_err()
            .contains("timed out")
        );
        let expired = std::time::Instant::now();
        assert!(
            parse_spec_until(&bytes, expired)
                .unwrap_err()
                .contains("timed out")
        );
        assert!(
            validate_spec_until(&spec, expired)
                .unwrap_err()
                .contains("timed out")
        );
    }

    #[test]
    fn received_executable_must_be_nonempty_executable_and_fully_sealed() {
        let unsealed = tempfile::tempfile().unwrap();
        assert!(super::validate_received_executable(&unsealed.into()).is_err());

        let descriptor = rustix::fs::memfd_create(
            "shipyard-provider-wrapper-test",
            rustix::fs::MemfdFlags::ALLOW_SEALING | rustix::fs::MemfdFlags::EXEC,
        )
        .unwrap();
        let mut sealed = std::fs::File::from(descriptor);
        sealed.write_all(b"executable snapshot").unwrap();
        sealed
            .set_permissions(std::fs::Permissions::from_mode(0o500))
            .unwrap();
        let required_seals = rustix::fs::SealFlags::WRITE
            | rustix::fs::SealFlags::GROW
            | rustix::fs::SealFlags::SHRINK
            | rustix::fs::SealFlags::SEAL;
        rustix::fs::fcntl_add_seals(&sealed, required_seals).unwrap();
        assert!(super::validate_received_executable(&sealed.into()).is_ok());
    }

    #[test]
    fn maximal_children_parse_obeys_an_already_expired_deadline() {
        let bytes = vec![b'1'; super::MAX_CHILDREN_BYTES];
        let mut children = std::collections::BTreeSet::new();
        let started = std::time::Instant::now();
        assert!(super::parse_children(&bytes, started, &mut children).is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(children.is_empty());
    }

    #[test]
    fn anonymous_sentinel_requires_marker_then_eof() {
        let (reader, writer) = rustix::pipe::pipe_with(
            rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
        )
        .unwrap();
        let mut reader = std::fs::File::from(reader);
        let mut writer = std::fs::File::from(writer);
        writer.write_all(&[super::STARTUP_MARKER]).unwrap();
        let mut marker = [0u8; 1];
        reader.read_exact(&mut marker).unwrap();
        assert_eq!(marker[0], super::STARTUP_MARKER);
        assert_eq!(
            sentinel_is_eof(
                &mut reader,
                std::time::Instant::now() + std::time::Duration::from_secs(1)
            ),
            Some(false)
        );
        drop(writer);
        assert_eq!(
            sentinel_is_eof(
                &mut reader,
                std::time::Instant::now() + std::time::Duration::from_secs(1)
            ),
            Some(true)
        );
    }

    #[test]
    fn empty_children_cannot_prove_cleanup_while_sentinel_writer_remains() {
        let (reader, _writer) = rustix::pipe::pipe_with(
            rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
        )
        .unwrap();
        let mut reader = std::fs::File::from(reader);
        let cleanup = super::cleanup_descendants(
            &mut reader,
            rustix::process::Pid::from_raw(999_999).unwrap(),
            super::LinuxSupervisorProviderV3::Success,
            std::time::Instant::now() + std::time::Duration::from_millis(30),
        );
        assert_eq!(cleanup, super::LinuxSupervisorCleanupV3::Unproven);
    }

    #[test]
    fn flooding_sentinel_still_honors_absolute_cleanup_deadline() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let (reader, writer) = rustix::pipe::pipe_with(
            rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
        )
        .unwrap();
        let mut reader = std::fs::File::from(reader);
        let mut writer = std::fs::File::from(writer);
        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = Arc::clone(&stop);
        let flood = std::thread::spawn(move || {
            let bytes = [7u8; 1024];
            while !writer_stop.load(Ordering::Relaxed) {
                match writer.write(&bytes) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("sentinel flood failed: {error}"),
                }
            }
        });
        let started = std::time::Instant::now();
        let cleanup = super::cleanup_descendants(
            &mut reader,
            rustix::process::Pid::from_raw(999_999).unwrap(),
            super::LinuxSupervisorProviderV3::Success,
            started + std::time::Duration::from_millis(30),
        );
        stop.store(true, Ordering::Relaxed);
        flood.join().unwrap();
        assert_eq!(cleanup, super::LinuxSupervisorCleanupV3::Unproven);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
}
