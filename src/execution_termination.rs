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

const TERMINATION_SCHEMA_VERSION: u32 = 1;

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
        let (root_command, descendants) = freeze_complete_tree(receipt)?;
        let transaction = TerminationTransaction {
            schema_version: TERMINATION_SCHEMA_VERSION,
            job_id: receipt.job_id.clone(),
            worker_generation: receipt.generation.clone(),
            root_pid: receipt.pid,
            root_command,
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
) -> io::Result<(String, Vec<FrozenProcessIdentity>)> {
    freeze_complete_tree_with_hook(receipt, |_| {})
}

#[cfg(unix)]
fn freeze_complete_tree_with_hook(
    receipt: &WorkerReceipt,
    mut during_scan: impl FnMut(usize),
) -> io::Result<(String, Vec<FrozenProcessIdentity>)> {
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
                let descendants = known
                    .into_iter()
                    .filter(|pid| *pid != receipt.pid)
                    .filter_map(|pid| {
                        processes.get(&pid).map(|process| FrozenProcessIdentity {
                            pid,
                            command: process.command.clone(),
                        })
                    })
                    .collect();
                return Ok((root_command, descendants));
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
) -> io::Result<(String, Vec<FrozenProcessIdentity>)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "durable daemon tree termination is unavailable on this platform",
    ))
}

#[cfg(unix)]
fn frozen_snapshot_is_safe(transaction: &TerminationTransaction) -> io::Result<bool> {
    let processes = process_snapshot()?;
    let root_safe = processes.get(&transaction.root_pid).is_none_or(|process| {
        process.zombie || (process.stopped && process.command == transaction.root_command)
    });
    let descendants_safe = transaction.descendants.iter().all(|identity| {
        processes.get(&identity.pid).is_none_or(|process| {
            process.zombie || (process.stopped && process.command == identity.command)
        })
    });
    Ok(root_safe && descendants_safe)
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the Unix safety probe is fallible and callers share one cross-platform API"
)]
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
#[expect(
    clippy::unnecessary_wraps,
    reason = "the Unix liveness probe is fallible and callers share one cross-platform API"
)]
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
}

#[cfg(unix)]
fn process_snapshot() -> io::Result<BTreeMap<u32, ProcessSnapshot>> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,stat=,command="])
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
            let state = fields.next()?;
            Some((
                pid,
                ProcessSnapshot {
                    pid,
                    parent,
                    stopped: state.starts_with('T'),
                    zombie: state.starts_with('Z'),
                    command: fields.collect::<Vec<_>>().join(" "),
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
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use chrono::Utc;

    use super::*;

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
            descendants: Vec::new(),
            action: TerminationAction::Cancel,
            phase: TerminationPhase::Frozen,
        };
        assert!(transaction.matches_receipt(&receipt));
        let mut replacement = receipt;
        replacement.generation = "generation-b".to_owned();
        assert!(!transaction.matches_receipt(&replacement));
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
        let mut child = Command::new(script)
            .args([
                "execution-worker",
                "--job-id",
                "detached",
                "--generation",
                "g-detached",
            ])
            .spawn()
            .expect("worker fixture");
        let detached = wait_for_numeric_pid(&detached_pid);
        let receipt = receipt_for(&child, "detached", "g-detached");
        let store = TerminationStore::new(temp.path());

        let mut transaction = store
            .begin(&receipt, TerminationAction::Cancel)
            .expect("freeze detached tree");
        assert!(
            transaction
                .descendants
                .iter()
                .any(|identity| identity.pid == detached),
            "a child in a detached process group remains part of the frozen parent tree"
        );
        assert!(
            store
                .prove_tree_dead(&mut transaction, Some(&mut child))
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
                "/usr/bin/perl -e 'open(my $ready, \">\", q{{{}}}); print $ready q{{ready}}; close($ready); while (!-e q{{{}}}) {{ select undef, undef, undef, 0.001 }}; my $pid = fork(); if ($pid == 0) {{ exec q{{/bin/sleep}}, q{{300}} }} open(my $fh, \">\", q{{{}}}); print $fh $pid; close($fh); wait' &\nwait",
                ready.display(),
                trigger.display(),
                forked_pid.display()
            ),
        );
        let mut child = Command::new(script)
            .args([
                "execution-worker",
                "--job-id",
                "fork-race",
                "--generation",
                "g-fork",
            ])
            .spawn()
            .expect("worker fixture");
        wait_for_file(&ready);
        let receipt = receipt_for(&child, "fork-race", "g-fork");
        let mut forked = None;
        let (root_command, descendants) = freeze_complete_tree_with_hook(&receipt, |scan| {
            if scan == 0 && forked.is_none() {
                fs::write(&trigger, b"go").expect("fork trigger");
                forked = Some(wait_for_numeric_pid(&forked_pid));
            }
        })
        .expect("freeze racing fork tree");
        let forked = forked.expect("forked pid");
        assert!(descendants.iter().any(|identity| identity.pid == forked));
        let store = TerminationStore::new(temp.path());
        let mut transaction = TerminationTransaction {
            schema_version: TERMINATION_SCHEMA_VERSION,
            job_id: receipt.job_id.clone(),
            worker_generation: receipt.generation.clone(),
            root_pid: receipt.pid,
            root_command,
            descendants,
            action: TerminationAction::Cancel,
            phase: TerminationPhase::Frozen,
        };
        store.save(&transaction).expect("save frozen transaction");
        assert!(
            store
                .prove_tree_dead(&mut transaction, Some(&mut child))
                .expect("kill complete fork tree")
        );
    }

    fn wait_for_file(path: &Path) {
        // Full macOS CI runs thousands of process tests concurrently. The
        // fixture is healthy once its file appears; allow scheduler pressure
        // without turning a delayed fork into a product verdict.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(path.exists(), "fixture did not publish {}", path.display());
    }

    fn wait_for_numeric_pid(path: &Path) -> u32 {
        // Creating and writing a PID file are separate filesystem operations.
        // Observe the complete value rather than treating an empty file as a
        // published fixture under full-suite scheduler pressure.
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut last = String::new();
        while Instant::now() < deadline {
            if let Ok(contents) = fs::read_to_string(path) {
                last = contents;
                if let Ok(pid) = last.trim().parse::<u32>() {
                    return pid;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!(
            "fixture did not publish numeric PID at {} (last value: {last:?})",
            path.display()
        );
    }
}
