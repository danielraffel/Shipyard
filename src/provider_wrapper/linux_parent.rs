//! Linux parent-side provider supervisor state machine.

use std::fs::File;
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

use super::{
    LinuxSentinelSupervisorSpecV3, LinuxSupervisorCleanupV3, LinuxSupervisorProviderV3,
    MAX_SPEC_BYTES, POLL_INTERVAL, PreparedExecutable, ProviderWrapperConfig,
    ProviderWrapperEnvironment, ProviderWrapperRefusal, ProviderWrapperRequestV1,
    ProviderWrapperRunResult, READY_FRAME, RESULT_FRAME_PREFIX, SENTINEL_TEARDOWN_BUDGET,
    SPEC_ADMISSION_BUDGET, TEARDOWN_BUDGET, drain_linux_supervisor_channel,
    linux_supervisor_command, linux_supervisor_protocol_slice, map_response,
    parse_linux_supervisor_frames, refusal, uncertain,
};
use crate::process::ProcessTree;

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_lines)]
pub(super) fn run_provider_wrapper_linux_supervised(
    config: &ProviderWrapperConfig,
    environment: &ProviderWrapperEnvironment,
    request: &ProviderWrapperRequestV1,
    request_bytes: &[u8],
    prepared: &PreparedExecutable,
) -> Result<ProviderWrapperRunResult, ProviderWrapperRefusal> {
    let spec = LinuxSentinelSupervisorSpecV3 {
        schema_version: 3,
        request_bytes: request_bytes.to_vec(),
        max_stdout_bytes: config.max_stdout_bytes,
        max_stderr_bytes: config.max_stderr_bytes,
        provider_deadline_millis: Duration::from_secs(config.deadline_seconds)
            .as_millis()
            .try_into()
            .map_err(|_| refusal("provider wrapper deadline cannot be represented"))?,
    };
    let spec_bytes = serde_json::to_vec(&spec)
        .map_err(|_| refusal("provider wrapper supervisor spec cannot be serialized"))?;
    if spec_bytes.is_empty() || spec_bytes.len() > MAX_SPEC_BYTES {
        return Err(refusal("provider wrapper supervisor spec is not bounded"));
    }
    let spec_length = u32::try_from(spec_bytes.len())
        .map_err(|_| refusal("provider wrapper supervisor spec length cannot be represented"))?;
    let mut framed_spec = Vec::with_capacity(spec_bytes.len() + 4);
    framed_spec.extend_from_slice(&spec_length.to_be_bytes());
    framed_spec.extend_from_slice(&spec_bytes);
    let max_frames = usize::try_from(config.max_stdout_bytes)
        .ok()
        .and_then(|limit| limit.checked_mul(4))
        .and_then(|limit| limit.checked_add(MAX_SPEC_BYTES))
        .ok_or_else(|| refusal("provider wrapper supervisor result limit cannot be represented"))?;
    let (control_parent, control_child) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC | rustix::net::SocketFlags::NONBLOCK,
        None,
    )
    .map_err(|_| refusal("provider supervisor control socket cannot be created"))?;

    let (channel_reader, channel_writer) = rustix::pipe::pipe_with(
        rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
    )
    .map_err(|_| refusal("provider supervisor channel cannot be created"))?;
    let channel_writer_flags = rustix::fs::fcntl_getfl(&channel_writer)
        .map_err(|_| refusal("provider supervisor channel flags are unreadable"))?;
    rustix::fs::fcntl_setfl(
        &channel_writer,
        channel_writer_flags & !rustix::fs::OFlags::NONBLOCK,
    )
    .map_err(|_| refusal("provider supervisor result channel cannot be blocking"))?;
    let mut supervisor_channel = File::from(channel_reader);
    let mut command = linux_supervisor_command();
    command
        .env_clear()
        .envs(environment.0.iter())
        .stdin(Stdio::from(control_child))
        .stdout(Stdio::from(channel_writer))
        .stderr(Stdio::null());
    let deadline = Instant::now() + Duration::from_secs(config.deadline_seconds);
    let admission_deadline = Instant::now()
        + SPEC_ADMISSION_BUDGET.min(deadline.saturating_duration_since(Instant::now()));
    let Ok(mut supervisor) = ProcessTree::spawn(&mut command) else {
        return Ok(uncertain("verified-wrapper-launch-outcome-unknown"));
    };
    let mut control = std::os::unix::net::UnixStream::from(control_parent);
    if !send_linux_supervisor_admission(
        &mut control,
        &prepared.file,
        &framed_spec,
        admission_deadline,
    ) {
        drop(control);
        supervisor.terminate_until(Instant::now() + TEARDOWN_BUDGET);
        return Ok(uncertain("provider-wrapper-cleanup-unproven"));
    }
    let mut supervisor_frames = Vec::new();
    let mut parent_reason = None;
    let mut startup_proven = false;
    while Instant::now() < deadline {
        if !drain_linux_supervisor_channel(
            &mut supervisor_channel,
            &mut supervisor_frames,
            max_frames,
        ) {
            parent_reason = Some("provider-wrapper-cleanup-unproven");
            break;
        }
        if linux_supervisor_protocol_slice(&supervisor_frames).starts_with(READY_FRAME) {
            startup_proven = true;
            break;
        }
        let protocol = linux_supervisor_protocol_slice(&supervisor_frames);
        if !READY_FRAME.starts_with(protocol) && !RESULT_FRAME_PREFIX.starts_with(protocol) {
            parent_reason = Some("provider-wrapper-cleanup-unproven");
            break;
        }
        match supervisor.try_wait() {
            Ok(Some(_)) => {
                let channel_ok = drain_linux_supervisor_channel(
                    &mut supervisor_channel,
                    &mut supervisor_frames,
                    max_frames,
                );
                startup_proven = channel_ok
                    && linux_supervisor_protocol_slice(&supervisor_frames).starts_with(READY_FRAME);
                if !startup_proven {
                    parent_reason = Some("provider-wrapper-cleanup-unproven");
                }
                break;
            }
            Err(_) => {
                parent_reason = Some("provider-wrapper-cleanup-unproven");
                break;
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
        }
    }
    if !startup_proven && parent_reason.is_none() {
        parent_reason = Some("provider-wrapper-timeout");
    }
    if startup_proven {
        while Instant::now() < deadline {
            if !drain_linux_supervisor_channel(
                &mut supervisor_channel,
                &mut supervisor_frames,
                max_frames,
            ) {
                parent_reason = Some("provider-wrapper-cleanup-unproven");
                break;
            }
            match supervisor.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => std::thread::sleep(POLL_INTERVAL),
                Err(_) => {
                    parent_reason = Some("provider-wrapper-wait-outcome-unknown");
                    break;
                }
            }
        }
        if Instant::now() >= deadline && supervisor.try_wait().ok().flatten().is_none() {
            parent_reason.get_or_insert("provider-wrapper-timeout");
        }
    }

    // Closing this capability requests cleanup without killing the subreaper.
    // It retains its own bounded cleanup budget and writes the exact result
    // before the parent considers a hard fallback.
    drop(control);
    let supervisor_deadline = Instant::now() + SENTINEL_TEARDOWN_BUDGET + Duration::from_secs(1);
    let status = loop {
        if !drain_linux_supervisor_channel(
            &mut supervisor_channel,
            &mut supervisor_frames,
            max_frames,
        ) {
            supervisor.terminate_until(Instant::now() + TEARDOWN_BUDGET);
            return Ok(uncertain("provider-wrapper-cleanup-unproven"));
        }
        match supervisor.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < supervisor_deadline => {
                std::thread::sleep(POLL_INTERVAL);
            }
            Ok(None) | Err(_) => break None,
        }
    };
    let Some(status) = status else {
        supervisor.terminate_until(Instant::now() + TEARDOWN_BUDGET);
        return Ok(uncertain("provider-wrapper-cleanup-unproven"));
    };
    // Disarm ProcessTree drop only after the supervisor has completed its
    // cleanup/result transaction.
    let _ = supervisor.wait();
    if !drain_linux_supervisor_channel(&mut supervisor_channel, &mut supervisor_frames, max_frames)
    {
        return Ok(uncertain("provider-wrapper-cleanup-unproven"));
    }
    finish_linux_supervisor_result(
        request,
        status.success(),
        startup_proven,
        parent_reason,
        &supervisor_frames,
    )
}

#[cfg(target_os = "linux")]
pub(super) fn finish_linux_supervisor_result(
    request: &ProviderWrapperRequestV1,
    supervisor_success: bool,
    startup_proven: bool,
    parent_reason: Option<&'static str>,
    supervisor_frames: &[u8],
) -> Result<ProviderWrapperRunResult, ProviderWrapperRefusal> {
    if !supervisor_success {
        return Ok(uncertain("provider-wrapper-cleanup-unproven"));
    }
    let Some((frame_startup, result)) = parse_linux_supervisor_frames(supervisor_frames) else {
        return Ok(uncertain("provider-wrapper-cleanup-unproven"));
    };
    if result.schema_version != 3 || frame_startup != startup_proven {
        return Ok(uncertain("provider-wrapper-cleanup-unproven"));
    }
    match result.cleanup {
        LinuxSupervisorCleanupV3::Unproven => {
            return Ok(uncertain("provider-wrapper-cleanup-unproven"));
        }
        LinuxSupervisorCleanupV3::ResidualTerminated => {
            return Ok(uncertain("provider-wrapper-descendant-violation"));
        }
        LinuxSupervisorCleanupV3::Clean => {}
    }
    if let Some(reason) = parent_reason {
        return Ok(uncertain(reason));
    }
    match result.provider {
        LinuxSupervisorProviderV3::Success => {}
        LinuxSupervisorProviderV3::Nonzero => {
            return Ok(uncertain("provider-wrapper-nonzero-post-launch"));
        }
        LinuxSupervisorProviderV3::TimedOut => {
            return Ok(uncertain("provider-wrapper-timeout"));
        }
        LinuxSupervisorProviderV3::WaitUnknown => {
            return Ok(uncertain("provider-wrapper-wait-outcome-unknown"));
        }
        LinuxSupervisorProviderV3::ControlEof | LinuxSupervisorProviderV3::StartupUnproven => {
            return Ok(uncertain("provider-wrapper-cleanup-unproven"));
        }
        LinuxSupervisorProviderV3::OutputLimit => {
            return Ok(uncertain("provider-wrapper-output-limit"));
        }
    }
    map_response(request, &result.stdout)
}

#[cfg(target_os = "linux")]
pub(super) fn write_linux_supervisor_spec(
    control: &mut std::os::unix::net::UnixStream,
    mut bytes: &[u8],
    deadline: Instant,
) -> bool {
    let Ok(flags) = rustix::fs::fcntl_getfl(&*control) else {
        return false;
    };
    if rustix::fs::fcntl_setfl(&*control, flags | rustix::fs::OFlags::NONBLOCK).is_err() {
        return false;
    }
    while !bytes.is_empty() && Instant::now() < deadline {
        match control.write(bytes) {
            Ok(0) => return false,
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(
                    POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    bytes.is_empty()
}

#[cfg(target_os = "linux")]
pub(super) fn send_linux_supervisor_admission(
    control: &mut std::os::unix::net::UnixStream,
    executable: &File,
    framed_spec: &[u8],
    deadline: Instant,
) -> bool {
    use std::io::IoSlice;
    use std::os::fd::AsFd;

    let marker = [1u8];
    while Instant::now() < deadline {
        let fds = [executable.as_fd()];
        let message = rustix::net::SendAncillaryMessage::ScmRights(&fds);
        let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut ancillary = rustix::net::SendAncillaryBuffer::new(&mut space);
        if !ancillary.push(message) {
            return false;
        }
        match rustix::net::sendmsg(
            &*control,
            &[IoSlice::new(&marker)],
            &mut ancillary,
            rustix::net::SendFlags::DONTWAIT | rustix::net::SendFlags::NOSIGNAL,
        ) {
            Ok(1) => return write_linux_supervisor_spec(control, framed_spec, deadline),
            Err(rustix::io::Errno::AGAIN) => std::thread::sleep(
                POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            ),
            Ok(_) | Err(_) => return false,
        }
    }
    false
}
