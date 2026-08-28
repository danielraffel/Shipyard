//! Durable, restartable worker-tree termination transactions.

#[cfg(unix)]
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Child;
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt;

use crate::execution_supervisor::WorkerReceipt;

const TERMINATION_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminationAction {
    Cancel,
    Defer,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminationPhase {
    Frozen,
    TreeDead,
    LeasesReleased,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TerminationTransaction {
    schema_version: u32,
    pub(crate) job_id: String,
    pub(crate) worker_generation: String,
    pub(crate) root_pid: u32,
    root_command: String,
    root_start_identity: String,
    descendants: Vec<FrozenProcessIdentity>,
    pub(crate) action: TerminationAction,
    pub(crate) phase: TerminationPhase,
}

impl TerminationTransaction {
    pub(crate) fn matches_receipt(&self, receipt: &WorkerReceipt) -> bool {
        self.job_id == receipt.job_id
            && self.worker_generation == receipt.generation
            && self.root_pid == receipt.pid
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FrozenProcessIdentity {
    pid: u32,
    command: String,
    start_identity: String,
}

pub(crate) struct TerminationStore {
    dir: PathBuf,
}

impl TerminationStore {
    pub(crate) fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("queue-terminations"),
        }
    }

    pub(crate) fn load(&self, job_id: &str) -> io::Result<Option<TerminationTransaction>> {
        validate_job_id(job_id)?;
        let path = self.path(job_id);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let transaction: TerminationTransaction = serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if transaction.schema_version != TERMINATION_SCHEMA_VERSION
            || transaction.job_id != job_id
            || transaction.worker_generation.is_empty()
            || transaction.root_pid == 0
            || transaction.root_start_identity.is_empty()
            || transaction
                .descendants
                .iter()
                .any(|identity| identity.pid == 0 || identity.start_identity.is_empty())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid durable worker termination transaction",
            ));
        }
        Ok(Some(transaction))
    }

    pub(crate) fn list(&self) -> io::Result<Vec<TerminationTransaction>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut transactions = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(job_id) = path.file_stem().and_then(|value| value.to_str()) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid worker termination transaction filename",
                ));
            };
            let transaction = self.load(job_id)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "worker termination transaction disappeared during scan",
                )
            })?;
            transactions.push(transaction);
        }
        Ok(transactions)
    }

    pub(crate) fn begin(
        &self,
        receipt: &WorkerReceipt,
        action: TerminationAction,
    ) -> io::Result<TerminationTransaction> {
        if let Some(existing) = self.load(&receipt.job_id)? {
            if existing.worker_generation != receipt.generation
                || existing.root_pid != receipt.pid
                || existing.action != action
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "worker termination transaction identity changed",
                ));
            }
            return Ok(existing);
        }
        let (root_command, root_start_identity, descendants) = freeze_complete_tree(receipt)?;
        let transaction = TerminationTransaction {
            schema_version: TERMINATION_SCHEMA_VERSION,
            job_id: receipt.job_id.clone(),
            worker_generation: receipt.generation.clone(),
            root_pid: receipt.pid,
            root_command,
            root_start_identity,
            descendants,
            action,
            phase: TerminationPhase::Frozen,
        };
        if let Err(error) = self.save(&transaction) {
            resume_frozen_tree(&transaction);
            return Err(error);
        }
        Ok(transaction)
    }

    pub(crate) fn prove_tree_dead(
        &self,
        transaction: &mut TerminationTransaction,
        child: Option<&mut Child>,
    ) -> io::Result<bool> {
        if transaction.phase >= TerminationPhase::TreeDead {
            return Ok(true);
        }
        if !frozen_snapshot_is_safe(transaction)? {
            return Ok(false);
        }
        kill_frozen_snapshot(transaction);
        if let Some(child) = child {
            let _ = child.wait_timeout(Duration::from_secs(5))?;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !snapshot_is_dead(transaction)? && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !snapshot_is_dead(transaction)? {
            return Ok(false);
        }
        transaction.phase = TerminationPhase::TreeDead;
        self.save(transaction)?;
        Ok(true)
    }

    pub(crate) fn mark_leases_released(
        &self,
        transaction: &mut TerminationTransaction,
    ) -> io::Result<()> {
        if transaction.phase < TerminationPhase::TreeDead {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cannot release leases before worker-tree death",
            ));
        }
        if transaction.phase < TerminationPhase::LeasesReleased {
            transaction.phase = TerminationPhase::LeasesReleased;
            self.save(transaction)?;
        }
        Ok(())
    }

    pub(crate) fn promote_to_cancel(
        &self,
        transaction: &mut TerminationTransaction,
    ) -> io::Result<()> {
        if transaction.action == TerminationAction::Cancel {
            return Ok(());
        }
        transaction.action = TerminationAction::Cancel;
        self.save(transaction)
    }

    pub(crate) fn remove(&self, job_id: &str) -> io::Result<()> {
        validate_job_id(job_id)?;
        let path = self.path(job_id);
        let _writer = crate::writer_domain_lease::acquire_for_protected_path(&path)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn save(&self, transaction: &TerminationTransaction) -> io::Result<()> {
        validate_job_id(&transaction.job_id)?;
        crate::writer_domain_lease::ensure_protected_dir_all(&self.dir)?;
        let path = self.path(&transaction.job_id);
        let _writer = crate::writer_domain_lease::acquire_for_protected_path(&path)?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer_pretty(&mut temp, transaction).map_err(io::Error::other)?;
        temp.write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|error| error.error)?;
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    fn path(&self, job_id: &str) -> PathBuf {
        self.dir.join(format!("{job_id}.json"))
    }
}

fn validate_job_id(job_id: &str) -> io::Result<()> {
    if job_id.is_empty()
        || job_id.len() > 255
        || matches!(job_id, "." | "..")
        || job_id
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid worker termination job id",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn freeze_complete_tree(
    receipt: &WorkerReceipt,
) -> io::Result<(String, String, Vec<FrozenProcessIdentity>)> {
    freeze_complete_tree_with_hook(receipt, |_| {})
}

#[cfg(unix)]
fn freeze_complete_tree_with_hook(
    receipt: &WorkerReceipt,
    mut during_scan: impl FnMut(usize),
) -> io::Result<(String, String, Vec<FrozenProcessIdentity>)> {
    signal(receipt.pid, "-STOP")?;
    let mut known = BTreeSet::from([receipt.pid]);
    let mut stable_scans = 0_u8;
    for scan_index in 0..64 {
        let processes = process_snapshot()?;
        if !exact_worker_is_stopped(receipt, &processes) {
            resume_pids(&known);
            return Err(io::Error::other(
                "exact worker identity was not frozen before tree snapshot",
            ));
        }
        during_scan(scan_index);
        let mut closure_changed = true;
        while closure_changed {
            closure_changed = false;
            for process in processes.values() {
                if known.contains(&process.parent) && known.insert(process.pid) {
                    closure_changed = true;
                }
            }
        }
        let mut newly_stopped = false;
        for pid in known.iter().copied().filter(|pid| *pid != receipt.pid) {
            let Some(process) = processes.get(&pid) else {
                continue;
            };
            if !process.stopped {
                signal(pid, "-STOP")?;
                newly_stopped = true;
            }
        }
        let all_frozen = known.iter().all(|pid| {
            processes
                .get(pid)
                .is_none_or(|process| process.stopped || process.zombie)
        });
        if !newly_stopped && all_frozen {
            stable_scans += 1;
            if stable_scans >= 2 {
                let root_command = processes
                    .get(&receipt.pid)
                    .expect("exact stopped worker remains in stable snapshot")
                    .command
                    .clone();
                let root_start_identity = processes
                    .get(&receipt.pid)
                    .expect("exact stopped worker remains in stable snapshot")
                    .start_identity
                    .clone();
                let descendants = known
                    .into_iter()
                    .filter(|pid| *pid != receipt.pid)
                    .filter_map(|pid| {
                        processes.get(&pid).map(|process| FrozenProcessIdentity {
                            pid,
                            command: process.command.clone(),
                            start_identity: process.start_identity.clone(),
                        })
                    })
                    .collect();
                return Ok((root_command, root_start_identity, descendants));
            }
        } else {
            stable_scans = 0;
        }
        thread::sleep(Duration::from_millis(10));
    }
    resume_pids(&known);
    Err(io::Error::other(
        "worker process tree did not reach a frozen fixed point",
    ))
}

#[cfg(not(unix))]
fn freeze_complete_tree(
    _receipt: &WorkerReceipt,
) -> io::Result<(String, String, Vec<FrozenProcessIdentity>)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "durable daemon tree termination is unavailable on this platform",
    ))
}

#[cfg(unix)]
fn frozen_snapshot_is_safe(transaction: &TerminationTransaction) -> io::Result<bool> {
    let processes = process_snapshot()?;
    Ok(frozen_snapshot_matches_processes(transaction, &processes))
}

#[cfg(unix)]
fn frozen_snapshot_matches_processes(
    transaction: &TerminationTransaction,
    processes: &BTreeMap<u32, ProcessSnapshot>,
) -> bool {
    let root_safe = processes.get(&transaction.root_pid).is_none_or(|process| {
        process.zombie
            || (process.stopped
                && process.command == transaction.root_command
                && process.start_identity == transaction.root_start_identity)
    });
    let descendants_safe = transaction.descendants.iter().all(|identity| {
        processes.get(&identity.pid).is_none_or(|process| {
            process.zombie
                || (process.stopped
                    && process.command == identity.command
                    && process.start_identity == identity.start_identity)
        })
    });
    root_safe && descendants_safe
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // Keep the platform implementations behind one fallible API.
fn frozen_snapshot_is_safe(_transaction: &TerminationTransaction) -> io::Result<bool> {
    Ok(false)
}

#[cfg(unix)]
fn kill_frozen_snapshot(transaction: &TerminationTransaction) {
    for identity in transaction.descendants.iter().rev() {
        let _ = signal(identity.pid, "-KILL");
    }
    let _ = signal(transaction.root_pid, "-KILL");
}

#[cfg(not(unix))]
fn kill_frozen_snapshot(_transaction: &TerminationTransaction) {}

#[cfg(unix)]
fn snapshot_is_dead(transaction: &TerminationTransaction) -> io::Result<bool> {
    let processes = process_snapshot()?;
    Ok(std::iter::once(transaction.root_pid)
        .chain(transaction.descendants.iter().map(|identity| identity.pid))
        .all(|pid| processes.get(&pid).is_none_or(|process| process.zombie)))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // Keep the platform implementations behind one fallible API.
fn snapshot_is_dead(_transaction: &TerminationTransaction) -> io::Result<bool> {
    Ok(false)
}

#[cfg(unix)]
fn resume_frozen_tree(transaction: &TerminationTransaction) {
    let mut pids = transaction
        .descendants
        .iter()
        .map(|identity| identity.pid)
        .collect::<Vec<_>>();
    pids.push(transaction.root_pid);
    resume_pids(&pids.into_iter().collect());
}

#[cfg(not(unix))]
fn resume_frozen_tree(_transaction: &TerminationTransaction) {}

#[cfg(unix)]
fn resume_pids(pids: &BTreeSet<u32>) {
    for pid in pids.iter().copied() {
        let _ = signal(pid, "-CONT");
    }
}

#[cfg(unix)]
fn signal(pid: u32, signal: &str) -> io::Result<()> {
    let status = Command::new("/bin/kill")
        .args([signal, "--", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("could not signal process {pid}")))
    }
}

#[cfg(unix)]
#[derive(Clone, Debug)]
struct ProcessSnapshot {
    pid: u32,
    parent: u32,
    stopped: bool,
    zombie: bool,
    command: String,
    start_identity: String,
}

#[cfg(unix)]
fn process_snapshot() -> io::Result<BTreeMap<u32, ProcessSnapshot>> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,lstart=,stat=,command="])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("process-tree observation failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent = fields.next()?.parse().ok()?;
            let start_identity = [
                fields.next()?,
                fields.next()?,
                fields.next()?,
                fields.next()?,
                fields.next()?,
            ]
            .join(" ");
            let state = fields.next()?;
            Some((
                pid,
                ProcessSnapshot {
                    pid,
                    parent,
                    stopped: state.starts_with('T'),
                    zombie: state.starts_with('Z'),
                    command: fields.collect::<Vec<_>>().join(" "),
                    start_identity,
                },
            ))
        })
        .collect())
}

#[cfg(unix)]
fn exact_worker_is_stopped(
    receipt: &WorkerReceipt,
    processes: &BTreeMap<u32, ProcessSnapshot>,
) -> bool {
    processes.get(&receipt.pid).is_some_and(|process| {
        process.stopped
            && process.command.contains("execution-worker")
            && process.command.contains(&receipt.job_id)
            && process.command.contains(&receipt.generation)
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use chrono::Utc;

    use super::*;

    const FIXTURE_PUBLICATION_TIMEOUT: Duration = Duration::from_secs(15);

    #[test]
    fn durable_transaction_is_bound_to_exact_worker_generation() {
        let receipt = WorkerReceipt {
            job_id: "job".to_owned(),
            generation: "generation-a".to_owned(),
            pid: 42,
            started_at: Utc::now(),
        };
        let transaction = TerminationTransaction {
            schema_version: TERMINATION_SCHEMA_VERSION,
            job_id: receipt.job_id.clone(),
            worker_generation: receipt.generation.clone(),
            root_pid: receipt.pid,
            root_command: "worker".to_owned(),
            root_start_identity: "Fri Aug 28 12:00:00 2026".to_owned(),
            descendants: Vec::new(),
            action: TerminationAction::Cancel,
            phase: TerminationPhase::Frozen,
        };
        assert!(transaction.matches_receipt(&receipt));
        let mut replacement = receipt;
        replacement.generation = "generation-b".to_owned();
        assert!(!transaction.matches_receipt(&replacement));
    }

    #[test]
    fn frozen_snapshot_refuses_pid_reuse_with_same_command() {
        let transaction = TerminationTransaction {
            schema_version: TERMINATION_SCHEMA_VERSION,
            job_id: "job".to_owned(),
            worker_generation: "generation-a".to_owned(),
            root_pid: 42,
            root_command: "execution-worker job generation-a".to_owned(),
            root_start_identity: "Fri Aug 28 12:00:00 2026".to_owned(),
            descendants: vec![FrozenProcessIdentity {
                pid: 43,
                command: "/bin/sleep 300".to_owned(),
                start_identity: "Fri Aug 28 12:00:01 2026".to_owned(),
            }],
            action: TerminationAction::Cancel,
            phase: TerminationPhase::Frozen,
        };
        let processes = BTreeMap::from([
            (
                42,
                ProcessSnapshot {
                    pid: 42,
                    parent: 1,
                    stopped: true,
                    zombie: false,
                    command: transaction.root_command.clone(),
                    start_identity: "Fri Aug 28 12:01:00 2026".to_owned(),
                },
            ),
            (
                43,
                ProcessSnapshot {
                    pid: 43,
                    parent: 42,
                    stopped: true,
                    zombie: false,
                    command: transaction.descendants[0].command.clone(),
                    start_identity: transaction.descendants[0].start_identity.clone(),
                },
            ),
        ]);

        assert!(!frozen_snapshot_matches_processes(&transaction, &processes));
    }

    fn executable_script(temp: &Path, body: &str) -> PathBuf {
        let path = temp.join("execution-worker-fixture.sh");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("fixture script");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("permissions");
        path
    }

    fn receipt_for(child: &Child, job_id: &str, generation: &str) -> WorkerReceipt {
        WorkerReceipt {
            job_id: job_id.to_owned(),
            generation: generation.to_owned(),
            pid: child.id(),
            started_at: Utc::now(),
        }
    }

    struct FixtureProcess {
        child: Child,
        root_pid: u32,
        recorded_pids: BTreeSet<u32>,
    }

    impl FixtureProcess {
        fn spawn(script: &Path, args: &[&str]) -> Self {
            let mut command = Command::new(script);
            command.args(args).process_group(0).stdin(Stdio::null());
            let child = command.spawn().expect("worker fixture");
            Self {
                root_pid: child.id(),
                child,
                recorded_pids: BTreeSet::new(),
            }
        }

        fn child(&self) -> &Child {
            &self.child
        }

        fn child_mut(&mut self) -> &mut Child {
            &mut self.child
        }

        fn wait_for_recorded_pid(&mut self, path: &Path) -> u32 {
            let pid = wait_for_pid(path);
            self.recorded_pids.insert(pid);
            pid
        }
    }

    impl Drop for FixtureProcess {
        fn drop(&mut self) {
            let mut exact_pids = self.recorded_pids.clone();
            exact_pids.insert(self.root_pid);
            if let Ok(processes) = process_snapshot() {
                let mut changed = true;
                while changed {
                    changed = false;
                    for process in processes.values() {
                        if exact_pids.contains(&process.parent) && exact_pids.insert(process.pid) {
                            changed = true;
                        }
                    }
                }
            }

            // Every fixture gets its own process group. Kill both that group and
            // the observed tree so detached descendants and stopped processes
            // cannot retain the test harness output descriptors after a panic.
            let process_group = format!("-{}", self.root_pid);
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &process_group])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            for pid in exact_pids.into_iter().rev() {
                let _ = signal(pid, "-KILL");
            }
            let _ = self.child.wait_timeout(Duration::from_secs(2));
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn frozen_fixed_point_captures_detached_process_group() {
        let temp = tempfile::tempdir().expect("tempdir");
        let detached_pid = temp.path().join("detached.pid");
        let script = executable_script(
            temp.path(),
            &format!(
                "/usr/bin/perl -MPOSIX -e 'POSIX::setsid(); open(my $fh, \">\", q{{{}}}); print $fh $$; close($fh); exec q{{/bin/sleep}}, q{{300}}' &\nwait",
                detached_pid.display()
            ),
        );
        let mut fixture = FixtureProcess::spawn(
            &script,
            &[
                "execution-worker",
                "--job-id",
                "detached",
                "--generation",
                "g-detached",
            ],
        );
        let detached = fixture.wait_for_recorded_pid(&detached_pid);
        let receipt = receipt_for(fixture.child(), "detached", "g-detached");
        let store = TerminationStore::new(temp.path());

        let mut transaction = store
            .begin(&receipt, TerminationAction::Cancel)
            .expect("freeze detached tree");
        assert!(
            transaction
                .descendants
                .iter()
                .any(|identity| { identity.pid == detached }),
            "a child in a detached process group remains part of the frozen parent tree"
        );
        assert!(
            store
                .prove_tree_dead(&mut transaction, Some(fixture.child_mut()))
                .expect("kill detached tree")
        );
    }

    #[test]
    fn frozen_fixed_point_rescans_child_forked_after_first_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let trigger = temp.path().join("fork-now");
        let ready = temp.path().join("fork-ready");
        let forked_pid = temp.path().join("forked.pid");
        let script = executable_script(
            temp.path(),
            &format!(
                "/usr/bin/perl -e 'open(my $ready, \">\", q{{{}}}); print $ready q{{ready}}; close($ready); while (!-e q{{{}}}) {{ select undef, undef, undef, 0.001 }}; my $pid = fork(); if ($pid == 0) {{ exec q{{/bin/sleep}}, q{{300}} }} my $tmp = q{{{}}} . q{{.tmp.}} . $$; open(my $fh, \">\", $tmp) or die $!; print $fh $pid; close($fh) or die $!; rename($tmp, q{{{}}}) or die $!; wait' &\nwait",
                ready.display(),
                trigger.display(),
                forked_pid.display(),
                forked_pid.display()
            ),
        );
        let mut fixture = FixtureProcess::spawn(
            &script,
            &[
                "execution-worker",
                "--job-id",
                "fork-race",
                "--generation",
                "g-fork",
            ],
        );
        wait_for_file(&ready);
        let receipt = receipt_for(fixture.child(), "fork-race", "g-fork");
        let mut triggered = false;
        let mut published_fork = None;
        let (root_command, root_start_identity, descendants) =
            freeze_complete_tree_with_hook(&receipt, |scan| {
                if scan == 0 && !triggered {
                    fs::write(&trigger, b"go").expect("fork trigger");
                    published_fork = Some(fixture.wait_for_recorded_pid(&forked_pid));
                    triggered = true;
                }
            })
            .expect("freeze racing fork tree");
        let forked = published_fork.expect("fork hook published a PID");
        assert!(descendants.iter().any(|identity| identity.pid == forked));
        let store = TerminationStore::new(temp.path());
        let mut transaction = TerminationTransaction {
            schema_version: TERMINATION_SCHEMA_VERSION,
            job_id: receipt.job_id.clone(),
            worker_generation: receipt.generation.clone(),
            root_pid: receipt.pid,
            root_command,
            root_start_identity,
            descendants,
            action: TerminationAction::Cancel,
            phase: TerminationPhase::Frozen,
        };
        store.save(&transaction).expect("save frozen transaction");
        assert!(
            store
                .prove_tree_dead(&mut transaction, Some(fixture.child_mut()))
                .expect("kill complete fork tree")
        );
    }

    fn wait_for_file(path: &Path) {
        let deadline = Instant::now() + FIXTURE_PUBLICATION_TIMEOUT;
        while !path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(path.exists(), "fixture did not publish {}", path.display());
    }

    fn wait_for_pid(path: &Path) -> u32 {
        let deadline = Instant::now() + FIXTURE_PUBLICATION_TIMEOUT;
        while Instant::now() < deadline {
            if let Ok(contents) = fs::read_to_string(path)
                && let Ok(pid) = contents.trim().parse::<u32>()
                && pid != 0
            {
                return pid;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!(
            "fixture did not publish a nonempty numeric PID to {}",
            path.display()
        );
    }

    fn wait_for_process_exit(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if signal(pid, "-0").is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("fixture process {pid} survived bounded cleanup");
    }

    #[test]
    fn pid_wait_ignores_an_existing_empty_file_until_contents_are_parseable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("forked.pid");
        fs::write(&path, b"").expect("empty publication window");
        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            fs::write(writer_path, b"4242\n").expect("publish numeric PID");
        });

        assert_eq!(wait_for_pid(&path), 4242);
        writer.join().expect("PID writer");
    }

    #[test]
    fn fixture_guard_reaps_process_group_during_panic_unwind() {
        let temp = tempfile::tempdir().expect("tempdir");
        let descendant_pid_path = temp.path().join("descendant.pid");
        let script = executable_script(
            temp.path(),
            &format!(
                "/bin/sleep 300 &\nprintf '%s\\n' $! > '{}'\nwait",
                descendant_pid_path.display()
            ),
        );
        let mut root_pid = 0;
        let mut descendant_pid = 0;

        let panic = catch_unwind(AssertUnwindSafe(|| {
            let mut fixture = FixtureProcess::spawn(&script, &["execution-worker"]);
            root_pid = fixture.root_pid;
            descendant_pid = fixture.wait_for_recorded_pid(&descendant_pid_path);
            panic!("exercise panic-safe fixture cleanup");
        }));

        assert!(panic.is_err());
        assert_ne!(root_pid, 0);
        assert_ne!(descendant_pid, 0);
        wait_for_process_exit(root_pid);
        wait_for_process_exit(descendant_pid);
    }
}
