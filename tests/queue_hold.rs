//! Concurrent contract tests for the live local queue-admission hold.

#![cfg(target_os = "macos")]

use std::fs;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use shipyard::job::{Job, JobStatus, Priority, ValidationMode};
use shipyard::queue::Queue;

const PURPOSE: &str = "tartci-pool-off";
const HOST: &str = "test-m1";
const SERVICE: &str = "actions.runner.test.service";

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_shipyard")
}

fn hold_command(state_dir: &Path, child: &[&str]) -> Command {
    let mut command = Command::new(binary());
    command
        .arg("--state-dir")
        .arg(state_dir)
        .args([
            "queue-hold",
            "exec",
            "--purpose",
            PURPOSE,
            "--host-id",
            HOST,
            "--service",
            SERVICE,
            "--repo",
            "owner/repo",
            "--runner",
            "runner-one",
            "--",
        ])
        .args(child);
    command
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn make_fifo(path: &Path) {
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(path)
            .status()
            .expect("create release fifo")
            .success()
    );
}

fn finish_hold(release: &Path, mut wrapper: Child) -> ExitStatus {
    fs::write(release, b"release\n").expect("release child");
    wrapper.wait().expect("wait for queue-hold wrapper")
}

fn read_env(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .expect("hold env")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn verify_command(state_dir: &Path, env: &[String], generation: u64) -> std::process::Output {
    Command::new(binary())
        .arg("--state-dir")
        .arg(state_dir)
        .arg("--json")
        .args([
            "queue-hold",
            "verify",
            "--hold-id",
            &env[0],
            "--generation",
            &generation.to_string(),
            "--host-id",
            &env[2],
            "--scope-digest",
            &env[3],
            "--owner-pid",
            &env[4],
            "--owner-process-start",
            &env[5],
            "--fd",
            &env[6],
        ])
        .output()
        .expect("queue-hold verify")
}

fn env_reporting_child(
    verified: &Path,
    environment: &Path,
    ready: &Path,
    release: &Path,
) -> Vec<String> {
    vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        concat!(
            "\"$1\" --state-dir \"$SHIPYARD_QUEUE_HOLD_STATE_DIR\" --json ",
            "queue-hold verify ",
            "--hold-id \"$SHIPYARD_QUEUE_HOLD_ID\" ",
            "--generation \"$SHIPYARD_QUEUE_HOLD_GENERATION\" ",
            "--host-id \"$SHIPYARD_QUEUE_HOLD_HOST_ID\" ",
            "--scope-digest \"$SHIPYARD_QUEUE_HOLD_SCOPE_DIGEST\" ",
            "--owner-pid \"$SHIPYARD_QUEUE_HOLD_OWNER_PID\" ",
            "--owner-process-start \"$SHIPYARD_QUEUE_HOLD_OWNER_PROCESS_START\" ",
            "--fd \"$SHIPYARD_QUEUE_HOLD_FD\" > \"$2\" || exit $?; ",
            "printf '%s\\n' ",
            "\"$SHIPYARD_QUEUE_HOLD_ID\" ",
            "\"$SHIPYARD_QUEUE_HOLD_GENERATION\" ",
            "\"$SHIPYARD_QUEUE_HOLD_HOST_ID\" ",
            "\"$SHIPYARD_QUEUE_HOLD_SCOPE_DIGEST\" ",
            "\"$SHIPYARD_QUEUE_HOLD_OWNER_PID\" ",
            "\"$SHIPYARD_QUEUE_HOLD_OWNER_PROCESS_START\" ",
            "\"$SHIPYARD_QUEUE_HOLD_FD\" > \"$3\"; ",
            "touch \"$4\"; read _ < \"$5\""
        )
        .to_owned(),
        "queue-hold-child".to_owned(),
        binary().to_owned(),
        verified.display().to_string(),
        environment.display().to_string(),
        ready.display().to_string(),
        release.display().to_string(),
    ]
}

#[test]
fn pending_worker_cannot_start_until_live_child_releases_queue_lock() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state_dir = temp.path().join("state");
    let mut queue = Queue::new(&state_dir).expect("queue");
    let job = queue
        .enqueue(Job::create(
            "a".repeat(40),
            "main",
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        ))
        .expect("enqueue");
    let ready = temp.path().join("ready");
    let release = temp.path().join("release");
    make_fifo(&release);
    let script = "touch \"$1\"; read _ < \"$2\"";
    let wrapper = hold_command(
        &state_dir,
        &[
            "/bin/sh",
            "-c",
            script,
            "queue-hold-child",
            ready.to_str().expect("ready path"),
            release.to_str().expect("release path"),
        ],
    )
    .spawn()
    .expect("spawn hold");
    wait_for_file(&ready);

    assert!(
        queue
            .acquire_drain_lock()
            .expect("lock observation")
            .is_none(),
        "pending worker must not acquire admission while child owns the hold"
    );
    assert_eq!(
        queue.get(&job.id).expect("queue read").expect("job").status,
        JobStatus::Pending
    );

    assert!(finish_hold(&release, wrapper).success());
    let lock = queue
        .acquire_drain_lock()
        .expect("lock after release")
        .expect("released lock");
    let started = queue
        .start_pending_jobs_for_drain(&lock, std::slice::from_ref(&job.id))
        .expect("start after release");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].status, JobStatus::Running);
}

#[test]
fn exact_verify_succeeds_then_stale_and_revoked_authority_refuse() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state_dir = temp.path().join("state");
    let verified = temp.path().join("verified.json");
    let environment = temp.path().join("environment");
    let ready = temp.path().join("ready");
    let release = temp.path().join("release");
    make_fifo(&release);
    let child = env_reporting_child(&verified, &environment, &ready, &release);
    let child_refs = child.iter().map(String::as_str).collect::<Vec<_>>();
    let wrapper = hold_command(&state_dir, &child_refs)
        .spawn()
        .expect("spawn hold");
    wait_for_file(&ready);

    let positive: Value = serde_json::from_slice(&fs::read(&verified).expect("verified output"))
        .expect("verified JSON");
    assert_eq!(positive["status"], "held");
    assert_eq!(positive["reason"], Value::Null);
    let env = read_env(&environment);
    assert_eq!(env.len(), 7);
    let generation = env[1].parse::<u64>().expect("generation");

    let unrelated = verify_command(&state_dir, &env, generation);
    assert_eq!(unrelated.status.code(), Some(3));
    let unrelated: Value =
        serde_json::from_slice(&unrelated.stdout).expect("unrelated verifier JSON");
    assert_eq!(unrelated["reason"], "owner_identity_mismatch");

    let stale = verify_command(&state_dir, &env, generation + 1);
    assert_eq!(stale.status.code(), Some(3));
    let stale: Value = serde_json::from_slice(&stale.stdout).expect("stale JSON");
    assert_eq!(stale["status"], "refused");
    assert_eq!(stale["reason"], "stale_generation");

    let revoked = Command::new(binary())
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--json")
        .args([
            "queue-hold",
            "revoke",
            "--hold-id",
            &env[0],
            "--generation",
            &env[1],
            "--reason",
            "test revocation",
        ])
        .output()
        .expect("revoke");
    assert!(revoked.status.success());
    let revoked: Value = serde_json::from_slice(&revoked.stdout).expect("revoke JSON");
    assert_eq!(revoked["status"], "revoked");

    let refused = verify_command(&state_dir, &env, generation);
    assert_eq!(refused.status.code(), Some(3));
    let refused: Value = serde_json::from_slice(&refused.stdout).expect("refusal JSON");
    assert_eq!(refused["reason"], "revoked");
    assert!(finish_hold(&release, wrapper).success());
}

#[test]
fn exact_owner_process_can_verify_in_place() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state_dir = temp.path().join("state");
    let script = concat!(
        "exec \"$1\" --state-dir \"$SHIPYARD_QUEUE_HOLD_STATE_DIR\" --json ",
        "queue-hold verify ",
        "--hold-id \"$SHIPYARD_QUEUE_HOLD_ID\" ",
        "--generation \"$SHIPYARD_QUEUE_HOLD_GENERATION\" ",
        "--host-id \"$SHIPYARD_QUEUE_HOLD_HOST_ID\" ",
        "--scope-digest \"$SHIPYARD_QUEUE_HOLD_SCOPE_DIGEST\" ",
        "--owner-pid \"$SHIPYARD_QUEUE_HOLD_OWNER_PID\" ",
        "--owner-process-start \"$SHIPYARD_QUEUE_HOLD_OWNER_PROCESS_START\" ",
        "--fd \"$SHIPYARD_QUEUE_HOLD_FD\""
    );
    let output = hold_command(
        &state_dir,
        &["/bin/sh", "-c", script, "queue-hold-child", binary()],
    )
    .output()
    .expect("verify in exact owner process");

    assert!(output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout).expect("held JSON");
    assert_eq!(response["status"], "held");
    assert_eq!(response["reason"], Value::Null);
}

#[test]
fn owner_death_releases_lock_and_dead_owner_verification_refuses() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state_dir = temp.path().join("state");
    let verified = temp.path().join("verified.json");
    let environment = temp.path().join("environment");
    let ready = temp.path().join("ready");
    let never_release = temp.path().join("never-release");
    make_fifo(&never_release);
    let child = env_reporting_child(&verified, &environment, &ready, &never_release);
    let child_refs = child.iter().map(String::as_str).collect::<Vec<_>>();
    let mut wrapper = hold_command(&state_dir, &child_refs)
        .spawn()
        .expect("spawn hold");
    wait_for_file(&ready);
    let env = read_env(&environment);
    let owner_pid = env[4].parse::<u32>().expect("owner pid");

    let killed = Command::new("/bin/kill")
        .args(["-KILL", &owner_pid.to_string()])
        .status()
        .expect("kill owner");
    assert!(killed.success());
    assert_eq!(wrapper.wait().expect("wait wrapper").signal(), Some(9));

    assert!(
        Queue::new(&state_dir)
            .expect("queue")
            .acquire_drain_lock()
            .expect("lock observation")
            .is_some(),
        "the last inherited descriptor must close when the owner dies"
    );
    let refused = verify_command(&state_dir, &env, env[1].parse().expect("generation"));
    assert_eq!(refused.status.code(), Some(3));
    let refused: Value = serde_json::from_slice(&refused.stdout).expect("dead-owner JSON");
    assert_eq!(refused["reason"], "owner_dead");
}

#[test]
fn reopened_exact_inode_fd_is_not_authority_even_while_another_process_holds_queue_lock() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state_dir = temp.path().join("state");
    let ready = temp.path().join("ready");
    let release = temp.path().join("release");
    make_fifo(&release);
    let script = concat!(
        "fd=$SHIPYARD_QUEUE_HOLD_FD; ",
        "eval \"exec ${fd}>&-\"; ",
        "eval \"exec ${fd}>\\\"$SHIPYARD_QUEUE_HOLD_STATE_DIR/queue.lock\\\"\"; ",
        "touch \"$2\"; read _ < \"$3\"; ",
        "\"$1\" --state-dir \"$SHIPYARD_QUEUE_HOLD_STATE_DIR\" --json ",
        "queue-hold verify ",
        "--hold-id \"$SHIPYARD_QUEUE_HOLD_ID\" ",
        "--generation \"$SHIPYARD_QUEUE_HOLD_GENERATION\" ",
        "--host-id \"$SHIPYARD_QUEUE_HOLD_HOST_ID\" ",
        "--scope-digest \"$SHIPYARD_QUEUE_HOLD_SCOPE_DIGEST\" ",
        "--owner-pid \"$SHIPYARD_QUEUE_HOLD_OWNER_PID\" ",
        "--owner-process-start \"$SHIPYARD_QUEUE_HOLD_OWNER_PROCESS_START\" ",
        "--fd \"$SHIPYARD_QUEUE_HOLD_FD\""
    );
    let wrapper = hold_command(
        &state_dir,
        &[
            "/bin/sh",
            "-c",
            script,
            "queue-hold-child",
            binary(),
            ready.to_str().expect("ready path"),
            release.to_str().expect("release path"),
        ],
    )
    .stdout(Stdio::piped())
    .spawn()
    .expect("spawn forged exact-inode hold");
    wait_for_file(&ready);
    let competing_lock = Queue::new(&state_dir)
        .expect("queue")
        .acquire_drain_lock()
        .expect("competing lock observation")
        .expect("reopened descriptor did not retain the original lock");
    fs::write(&release, b"verify\n").expect("release forged verifier");
    let output = wrapper.wait_with_output().expect("wait forged verifier");

    assert_eq!(output.status.code(), Some(3));
    let response: Value = serde_json::from_slice(&output.stdout).expect("refusal JSON");
    assert_eq!(response["status"], "refused");
    assert_eq!(response["reason"], "lock_fd_invalid");
    drop(competing_lock);
}

#[test]
fn contended_timeout_is_124_and_spawns_no_child() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state_dir = temp.path().join("state");
    let ready = temp.path().join("ready");
    let release = temp.path().join("release");
    make_fifo(&release);
    let script = "touch \"$1\"; read _ < \"$2\"";
    let wrapper = hold_command(
        &state_dir,
        &[
            "/bin/sh",
            "-c",
            script,
            "queue-hold-child",
            ready.to_str().expect("ready path"),
            release.to_str().expect("release path"),
        ],
    )
    .spawn()
    .expect("spawn first hold");
    wait_for_file(&ready);

    let invoked = temp.path().join("must-not-exist");
    let timed_out = Command::new(binary())
        .arg("--state-dir")
        .arg(&state_dir)
        .args([
            "queue-hold",
            "exec",
            "--purpose",
            PURPOSE,
            "--host-id",
            HOST,
            "--service",
            SERVICE,
            "--timeout-seconds",
            "0",
            "--",
            "/usr/bin/touch",
        ])
        .arg(&invoked)
        .status()
        .expect("run contended hold");
    assert_eq!(timed_out.code(), Some(124));
    assert!(!invoked.exists(), "timeout must spawn no child");
    assert!(finish_hold(&release, wrapper).success());
}
