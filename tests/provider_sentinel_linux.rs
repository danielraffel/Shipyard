//! Production-binary proof for Linux provider descendant custody.

#![cfg(target_os = "linux")]

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wait_timeout::ChildExt;

const READY: &[u8] = b"shipyard-provider-sentinel-ready-v3\n";
const RESULT: &[u8] = b"shipyard-provider-sentinel-result-v3 ";

#[test]
fn production_supervisor_control_eof_reaps_adopted_descendant() {
    let directory = private_directory();
    let wrapper = compile_descendant_wrapper(directory.path());
    let descendant_pid = directory.path().join("descendant.pid");
    let spec = spec(&[], 10_000);
    let (mut supervisor, control, mut channel) = spawn_supervisor(
        &wrapper,
        &spec,
        [("DESCENDANT_PID_PATH", descendant_pid.as_path())],
    );

    let mut ready = vec![0; READY.len()];
    channel.read_exact(&mut ready).unwrap();
    if ready != READY {
        channel.read_to_end(&mut ready).unwrap();
        panic!(
            "supervisor did not become ready: {}",
            String::from_utf8_lossy(&ready)
        );
    }
    wait_until(Duration::from_secs(5), || descendant_pid.exists());
    let pid = fs::read_to_string(&descendant_pid).unwrap();

    drop(control);
    let status = supervisor
        .wait_timeout(Duration::from_secs(4))
        .unwrap()
        .expect("production supervisor exceeded bounded control-EOF cleanup");
    assert!(status.success());
    let result = read_result(&mut channel);
    assert_eq!(result["schema_version"], 3);
    assert_eq!(result["provider"], "control_eof");
    assert_eq!(result["cleanup"], "residual_terminated");
    assert_eq!(result["stdout"], json!([]));
    wait_until(Duration::from_secs(2), || {
        !Path::new(&format!("/proc/{pid}")).exists()
    });
}

#[test]
fn production_supervisor_request_and_captures_are_not_path_substitutable() {
    let directory = private_directory();
    let wrapper = compile_request_observer(directory.path());
    let observed = directory.path().join("observed");
    let diagnostic_request = directory.path().join("request.json");
    let replacement = directory.path().join("replacement.json");
    private_file(&diagnostic_request)
        .write_all(b"original on discarded path")
        .unwrap();
    private_file(&replacement)
        .write_all(b"substituted unauthorized operation")
        .unwrap();
    fs::rename(&replacement, &diagnostic_request).unwrap();

    let spec = spec(b"original authorized operation", 5_000);
    let (mut supervisor, control, mut channel) =
        spawn_supervisor(&wrapper, &spec, [("OBSERVED_PATH", observed.as_path())]);
    let status = supervisor
        .wait_timeout(Duration::from_secs(6))
        .unwrap()
        .expect("request-custody supervisor did not finish");
    assert!(status.success());
    drop(control);
    let mut frames = Vec::new();
    channel.read_to_end(&mut frames).unwrap();
    assert!(
        frames.starts_with(READY),
        "supervisor did not become ready: {}",
        String::from_utf8_lossy(&frames)
    );
    let result = parse_result(&frames[READY.len()..]);
    assert_eq!(result["provider"], "success");
    assert_eq!(result["cleanup"], "clean");
    assert_eq!(result["stdout"], json!([111, 107]));
    assert_eq!(
        fs::read(&observed).unwrap(),
        b"original authorized operation"
    );
    assert_ne!(
        fs::read(&observed).unwrap(),
        fs::read(&diagnostic_request).unwrap(),
        "a substituted path changed the executed request"
    );
}

#[test]
fn production_supervisor_relocates_a_request_that_initially_occupies_fd9() {
    let directory = private_directory();
    let wrapper = compile_request_observer(directory.path());
    let observed = directory.path().join("fd9-observed");
    let spec = spec(b"request survives fd9 reservation", 5_000);
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "exec 3</dev/null; exec 4</dev/null; exec 5</dev/null; exec 6</dev/null; exec 7</dev/null; exec 8</dev/null; exec \"$@\"",
            "shipyard-force-request-fd9",
            env!("CARGO_BIN_EXE_shipyard"),
            "provider-sentinel-supervisor",
        ])
        .env_clear()
        .env("OBSERVED_PATH", &observed)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let (mut supervisor, control, mut channel) = spawn_command(command, &wrapper, &spec);
    let status = supervisor
        .wait_timeout(Duration::from_secs(6))
        .unwrap()
        .expect("forced-fd9 supervisor did not finish");
    assert!(status.success());
    drop(control);
    let mut frames = Vec::new();
    channel.read_to_end(&mut frames).unwrap();
    assert!(frames.starts_with(READY));
    let result = parse_result(&frames[READY.len()..]);
    assert_eq!(result["provider"], "success");
    assert_eq!(result["cleanup"], "clean");
    assert_eq!(
        fs::read(observed).unwrap(),
        b"request survives fd9 reservation"
    );
}

#[test]
fn production_receiver_refuses_malformed_multiple_and_truncated_fd_admission() {
    let directory = private_directory();
    let wrapper = compile_request_observer(directory.path());
    let executables = [
        sealed_executable(&wrapper),
        sealed_executable(&wrapper),
        sealed_executable(&wrapper),
    ];
    for (marker, count) in [(2, 1), (1, 2), (1, 3)] {
        let (mut supervisor, control, mut channel) = bare_supervisor();
        send_executable_frame(&control, marker, &executables[..count]);
        drop(control);
        let status = supervisor
            .wait_timeout(Duration::from_secs(3))
            .unwrap()
            .expect("malformed admission did not fail within its bound");
        assert!(
            !status.success(),
            "malformed admission unexpectedly succeeded"
        );
        let mut frames = Vec::new();
        channel.read_to_end(&mut frames).unwrap();
        assert!(frames.is_empty(), "malformed admission published authority");
    }
}

#[test]
fn production_supervisor_sigkill_after_ready_publishes_no_result_and_exposes_residual() {
    let directory = private_directory();
    let provider_pid = directory.path().join("provider.pid");
    let wrapper = compile_wrapper(
        directory.path(),
        r#"FILE *file = fopen(getenv("PROVIDER_PID_PATH"), "w");
if (file == NULL) return 2;
fprintf(file, "%d", getpid());
fclose(file);
sleep(30);
return 0;"#,
    );
    let spec = spec(&[], 10_000);
    let (mut supervisor, control, mut channel) = spawn_supervisor(
        &wrapper,
        &spec,
        [("PROVIDER_PID_PATH", provider_pid.as_path())],
    );
    let mut ready = vec![0; READY.len()];
    channel.read_exact(&mut ready).unwrap();
    assert_eq!(ready, READY);
    wait_until(Duration::from_secs(5), || provider_pid.exists());
    let pid = fs::read_to_string(&provider_pid).unwrap();

    supervisor.kill().unwrap();
    let status = supervisor.wait().unwrap();
    assert!(!status.success());
    drop(control);
    let mut remainder = Vec::new();
    channel.read_to_end(&mut remainder).unwrap();
    let residual_was_live = Path::new(&format!("/proc/{pid}")).exists();
    let _ = Command::new("/bin/kill").args(["-KILL", &pid]).status();
    wait_until(Duration::from_secs(2), || {
        !Path::new(&format!("/proc/{pid}")).exists()
    });

    assert!(
        remainder.is_empty(),
        "crashed supervisor forged a result frame"
    );
    assert!(
        residual_was_live,
        "crash canary failed to expose residual custody"
    );
}

fn spec(request: &[u8], deadline_millis: u64) -> Value {
    json!({
        "schema_version": 3,
        "request_bytes": request,
        "max_stdout_bytes": 16_384,
        "max_stderr_bytes": 16_384,
        "provider_deadline_millis": deadline_millis
    })
}

fn spawn_supervisor<'a, const N: usize>(
    executable: &Path,
    spec: &Value,
    environment: [(&'a str, &'a Path); N],
) -> (
    Child,
    std::os::unix::net::UnixStream,
    std::process::ChildStdout,
) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_shipyard"));
    command
        .arg("provider-sentinel-supervisor")
        .env_clear()
        .envs(environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    spawn_command(command, executable, spec)
}

fn spawn_command(
    mut command: Command,
    executable: &Path,
    spec: &Value,
) -> (
    Child,
    std::os::unix::net::UnixStream,
    std::process::ChildStdout,
) {
    use std::io::IoSlice;
    use std::os::fd::AsFd;

    let (parent_socket, child_socket) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC | rustix::net::SocketFlags::NONBLOCK,
        None,
    )
    .unwrap();
    command.stdin(Stdio::from(child_socket));
    let mut child = command.spawn().unwrap();
    let mut control = std::os::unix::net::UnixStream::from(parent_socket);
    let channel = child.stdout.take().unwrap();
    let bytes = serde_json::to_vec(spec).unwrap();
    let executable = sealed_executable(executable);
    let fds = [executable.as_fd()];
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = rustix::net::SendAncillaryBuffer::new(&mut space);
    assert!(ancillary.push(rustix::net::SendAncillaryMessage::ScmRights(&fds)));
    assert_eq!(
        rustix::net::sendmsg(
            &control,
            &[IoSlice::new(&[1])],
            &mut ancillary,
            rustix::net::SendFlags::empty()
        )
        .unwrap(),
        1
    );
    let mut framed = Vec::with_capacity(bytes.len() + 4);
    framed.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
    framed.extend_from_slice(&bytes);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut remaining = framed.as_slice();
    while !remaining.is_empty() {
        match control.write(remaining) {
            Ok(count) => remaining = &remaining[count..],
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("spec send failed: {error}"),
        }
    }
    (child, control, channel)
}

fn bare_supervisor() -> (
    Child,
    std::os::unix::net::UnixStream,
    std::process::ChildStdout,
) {
    let (parent_socket, child_socket) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC | rustix::net::SocketFlags::NONBLOCK,
        None,
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_shipyard"))
        .arg("provider-sentinel-supervisor")
        .env_clear()
        .stdin(Stdio::from(child_socket))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let channel = child.stdout.take().unwrap();
    (
        child,
        std::os::unix::net::UnixStream::from(parent_socket),
        channel,
    )
}

fn send_executable_frame(control: &std::os::unix::net::UnixStream, marker: u8, files: &[fs::File]) {
    use std::io::IoSlice;
    use std::os::fd::AsFd;

    let fds: Vec<_> = files.iter().map(fs::File::as_fd).collect();
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
    let mut ancillary = rustix::net::SendAncillaryBuffer::new(&mut space);
    assert!(ancillary.push(rustix::net::SendAncillaryMessage::ScmRights(&fds)));
    assert_eq!(
        rustix::net::sendmsg(
            control,
            &[IoSlice::new(&[marker])],
            &mut ancillary,
            rustix::net::SendFlags::empty(),
        )
        .unwrap(),
        1
    );
}

fn sealed_executable(path: &Path) -> fs::File {
    let mut source = fs::File::open(path).unwrap();
    let descriptor = rustix::fs::memfd_create(
        "shipyard-provider-wrapper-integration",
        rustix::fs::MemfdFlags::ALLOW_SEALING
            | rustix::fs::MemfdFlags::CLOEXEC
            | rustix::fs::MemfdFlags::EXEC,
    )
    .unwrap();
    let mut sealed = fs::File::from(descriptor);
    std::io::copy(&mut source, &mut sealed).unwrap();
    sealed
        .set_permissions(fs::Permissions::from_mode(0o500))
        .unwrap();
    let required_seals = rustix::fs::SealFlags::WRITE
        | rustix::fs::SealFlags::GROW
        | rustix::fs::SealFlags::SHRINK
        | rustix::fs::SealFlags::SEAL;
    rustix::fs::fcntl_add_seals(&sealed, required_seals).unwrap();
    sealed.seek(SeekFrom::Start(0)).unwrap();
    sealed
}

fn read_result(channel: &mut impl Read) -> Value {
    let mut bytes = Vec::new();
    channel.read_to_end(&mut bytes).unwrap();
    parse_result(&bytes)
}

fn parse_result(bytes: &[u8]) -> Value {
    let json_bytes = bytes
        .strip_prefix(RESULT)
        .and_then(|bytes| bytes.strip_suffix(b"\n"))
        .unwrap();
    serde_json::from_slice(json_bytes).unwrap()
}

fn private_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

fn private_file(path: &Path) -> fs::File {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap()
}

fn compile_descendant_wrapper(directory: &Path) -> std::path::PathBuf {
    compile_wrapper(
        directory,
        r#"pid_t child = fork();
if (child == 0) {
    setsid();
    prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
    signal(SIGTERM, SIG_IGN);
    FILE *file = fopen(getenv("DESCENDANT_PID_PATH"), "w");
    fprintf(file, "%d", getpid());
    fclose(file);
    sleep(30);
    return 0;
}
pause();
return 0;"#,
    )
}

fn compile_request_observer(directory: &Path) -> std::path::PathBuf {
    compile_wrapper(
        directory,
        r#"FILE *observed = fopen(getenv("OBSERVED_PATH"), "w");
if (observed == NULL) return 2;
char buffer[256];
size_t count = fread(buffer, 1, sizeof(buffer), stdin);
if (ferror(stdin) || fwrite(buffer, 1, count, observed) != count) return 3;
fclose(observed);
fputs("ok", stdout);
return 0;"#,
    )
}

fn compile_wrapper(directory: &Path, body: &str) -> std::path::PathBuf {
    let source = directory.join("wrapper.c");
    let executable = directory.join("wrapper");
    let mut file = private_file(&source);
    write!(
        file,
        "#include <signal.h>\n#include <stdio.h>\n#include <stdlib.h>\n#include <sys/prctl.h>\n#include <sys/types.h>\n#include <unistd.h>\nint main(void) {{\n{body}\n}}\n"
    )
    .unwrap();
    file.sync_all().unwrap();
    let status = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .status()
        .unwrap();
    assert!(status.success());
    executable
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(Instant::now() < deadline, "condition exceeded {timeout:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
