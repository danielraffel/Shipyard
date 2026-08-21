use super::{
    BoundedStream, CliFailure, Command, Duration, GlobalModelLease, Instant, Path, ProcessTree,
    Read, RecoveryWorkerPolicy, Stdio, Value, WorkerProcessOutput, fs, thread,
};

pub(super) fn run_worker_process(
    policy: &RecoveryWorkerPolicy,
    request: &Value,
    model_lease: &GlobalModelLease,
    scratch_dir: &Path,
    deadline: Instant,
) -> Result<WorkerProcessOutput, CliFailure> {
    if Instant::now() >= deadline {
        return Ok(timed_out_output());
    }
    fs::create_dir_all(scratch_dir).map_err(|error| {
        CliFailure::new(
            1,
            format!(
                "failed to create recovery-worker scratch directory {}: {error}",
                scratch_dir.display()
            ),
        )
    })?;
    // The locked file is both the bounded JSON input and an inherited capacity
    // handle. Because the parent never explicitly unlocks it, the operating
    // system keeps the machine-global lease held if Shipyard crashes while the
    // model process is still alive.
    let stdin = model_lease.worker_stdin(request)?;

    let argv = policy.argv();
    let (program, arguments) = argv
        .split_first()
        .ok_or_else(|| CliFailure::new(1, "recovery-worker command is empty"))?;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(scratch_dir)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.env_clear();
    command.env("CODEX_HOME", &policy.codex_home);
    command.env("HOME", scratch_dir);
    command.env("TMPDIR", scratch_dir);
    command.env("PATH", minimal_path(program));
    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SYSTEMROOT") {
        command.env("SYSTEMROOT", system_root);
    }

    // Input preparation and command construction are part of the record-wide
    // budget. Never launch a model after setup consumed the absolute deadline.
    if Instant::now() >= deadline {
        return Ok(timed_out_output());
    }

    let mut child = ProcessTree::spawn(&mut command).map_err(|error| {
        CliFailure::new(
            1,
            format!("failed to launch recovery worker `{program}`: {error}"),
        )
    })?;
    let child_stdout = child
        .take_stdout()
        .ok_or_else(|| CliFailure::new(1, "failed to capture recovery-worker stdout"))?;
    let child_stderr = child
        .take_stderr()
        .ok_or_else(|| CliFailure::new(1, "failed to capture recovery-worker stderr"))?;
    let tail_limit = policy.max_log_tail_bytes;
    let stdout_reader = thread::spawn(move || read_bounded_tail(child_stdout, tail_limit));
    let stderr_reader = thread::spawn(move || read_bounded_tail(child_stderr, tail_limit));

    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (Some(status), false),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(50)),
                );
            }
            Ok(None) => {
                child.terminate();
                break (None, true);
            }
            Err(error) => {
                child.terminate();
                return Err(CliFailure::new(
                    1,
                    format!("failed to supervise recovery worker: {error}"),
                ));
            }
        }
    };
    let (stdout, stderr) = finish_readers(&mut child, stdout_reader, stderr_reader)?;
    Ok(WorkerProcessOutput {
        exit_code: status.and_then(|value| value.code()),
        timed_out,
        stdout_truncated: stdout.total_bytes > stdout.tail.len(),
        stdout: stdout.tail,
        stderr: stderr.tail,
    })
}

fn timed_out_output() -> WorkerProcessOutput {
    WorkerProcessOutput {
        exit_code: None,
        timed_out: true,
        stdout: Vec::new(),
        stdout_truncated: false,
        stderr: Vec::new(),
    }
}

pub(super) fn finish_readers(
    child: &mut ProcessTree,
    stdout_reader: thread::JoinHandle<std::io::Result<BoundedStream>>,
    stderr_reader: thread::JoinHandle<std::io::Result<BoundedStream>>,
) -> Result<(BoundedStream, BoundedStream), CliFailure> {
    let first_deadline = Instant::now() + Duration::from_secs(5);
    while !(stdout_reader.is_finished() && stderr_reader.is_finished())
        && Instant::now() < first_deadline
    {
        thread::sleep(Duration::from_millis(20));
    }
    if !(stdout_reader.is_finished() && stderr_reader.is_finished()) {
        // The direct child may have exited while a descendant retained an I/O
        // handle. Terminate the supervised group/Job instead of joining an
        // unbounded pipe reader.
        child.terminate();
        let final_deadline = Instant::now() + Duration::from_secs(2);
        while !(stdout_reader.is_finished() && stderr_reader.is_finished())
            && Instant::now() < final_deadline
        {
            thread::sleep(Duration::from_millis(20));
        }
    }
    if !(stdout_reader.is_finished() && stderr_reader.is_finished()) {
        return Err(CliFailure::new(
            1,
            "recovery-worker descendants retained captured output after termination",
        ));
    }
    Ok((
        join_reader(stdout_reader, "stdout")?,
        join_reader(stderr_reader, "stderr")?,
    ))
}

pub(super) fn join_reader(
    reader: thread::JoinHandle<std::io::Result<BoundedStream>>,
    stream: &str,
) -> Result<BoundedStream, CliFailure> {
    reader
        .join()
        .map_err(|_| CliFailure::new(1, format!("recovery-worker {stream} reader panicked")))?
        .map_err(|error| {
            CliFailure::new(
                1,
                format!("failed reading recovery-worker {stream}: {error}"),
            )
        })
}

pub(super) fn read_bounded_tail(
    mut input: impl Read,
    limit: usize,
) -> std::io::Result<BoundedStream> {
    let mut tail = Vec::with_capacity(limit.min(8 * 1024));
    let mut total_bytes = 0_usize;
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let count = input.read(&mut chunk)?;
        if count == 0 {
            return Ok(BoundedStream { tail, total_bytes });
        }
        total_bytes = total_bytes.saturating_add(count);
        if count >= limit {
            tail.clear();
            tail.extend_from_slice(&chunk[count - limit..count]);
            continue;
        }
        let overflow = tail.len().saturating_add(count).saturating_sub(limit);
        if overflow > 0 {
            tail.drain(..overflow);
        }
        tail.extend_from_slice(&chunk[..count]);
    }
}

pub(super) fn minimal_path(program: &str) -> String {
    let mut entries = Vec::new();
    if let Some(parent) = Path::new(program).parent() {
        entries.push(parent.display().to_string());
    }
    #[cfg(windows)]
    entries.push(r"C:\Windows\System32".to_owned());
    #[cfg(not(windows))]
    entries.extend(["/usr/bin".to_owned(), "/bin".to_owned()]);
    entries.join(if cfg!(windows) { ";" } else { ":" })
}
