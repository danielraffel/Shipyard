//! Exact-head single-flight ownership for `shipyard pr`.
//!
//! The lock itself is the only liveness authority. The JSON metadata is
//! diagnostic and is deliberately never interpreted as proof that a PID is
//! alive: stale files and PID reuse therefore cannot authorize cancellation.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct PrInvocationIdentity {
    pub(super) repo: String,
    pub(super) branch: String,
    pub(super) head: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct PrInvocationOwner {
    schema_version: u8,
    repo: String,
    branch: String,
    head: String,
    pid: u32,
    started_at: String,
}

#[derive(Debug)]
pub(super) enum PrInvocationAcquireError {
    Busy {
        identity: PrInvocationIdentity,
        owner: Option<String>,
    },
    Io(io::Error),
}

impl std::fmt::Display for PrInvocationAcquireError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy { identity, owner } => {
                write!(
                    formatter,
                    "shipyard pr already owns {}/{} at {}",
                    identity.repo,
                    identity.branch,
                    short_head(&identity.head)
                )?;
                if let Some(owner) = owner {
                    write!(formatter, " ({owner})")?;
                }
                write!(
                    formatter,
                    "; use the durable steward/queue status instead of starting duplicate validation"
                )
            }
            Self::Io(error) => write!(
                formatter,
                "could not acquire shipyard pr ownership: {error}"
            ),
        }
    }
}

impl std::error::Error for PrInvocationAcquireError {}

/// Kernel-held exact-head lease. Dropping or process death releases ownership.
pub(super) struct PrInvocationLease {
    file: File,
    identity: PrInvocationIdentity,
}

/// Brief repo+branch fence used only while observing or moving `HEAD`.
/// It closes the transition window without preventing different immutable
/// heads from proceeding independently once their exact leases are held.
pub(super) struct PrInvocationTransitionGuard {
    file: File,
}

impl PrInvocationTransitionGuard {
    pub(super) fn acquire_machine(repo: &str, branch: &str) -> io::Result<Self> {
        let directory = crate::paths::machine_coordination_state_dir()
            .join("pr-invocations")
            .join("transitions");
        Self::acquire(&directory, repo, branch)
    }

    fn acquire(directory: &Path, repo: &str, branch: &str) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(transition_lock_path(directory, repo, branch))?;
        FileExt::try_lock_exclusive(&file)?;
        Ok(Self { file })
    }
}

impl Drop for PrInvocationTransitionGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

impl PrInvocationLease {
    pub(super) fn acquire_machine(
        identity: PrInvocationIdentity,
    ) -> Result<Self, PrInvocationAcquireError> {
        Self::acquire(&crate::paths::machine_coordination_state_dir(), identity)
    }

    pub(super) fn acquire(
        state_dir: &Path,
        identity: PrInvocationIdentity,
    ) -> Result<Self, PrInvocationAcquireError> {
        let directory = state_dir.join("pr-invocations");
        fs::create_dir_all(&directory).map_err(PrInvocationAcquireError::Io)?;
        let path = lock_path(&directory, &identity);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(PrInvocationAcquireError::Io)?;
        if let Err(error) = FileExt::try_lock_exclusive(&file) {
            if error.kind() == io::ErrorKind::WouldBlock {
                return Err(PrInvocationAcquireError::Busy {
                    identity,
                    owner: read_owner_summary(&path),
                });
            }
            return Err(PrInvocationAcquireError::Io(error));
        }
        write_owner(&mut file, &identity).map_err(PrInvocationAcquireError::Io)?;
        Ok(Self { file, identity })
    }

    /// Move ownership to a post-amend/post-version-bump exact head without an
    /// unlocked window. The replacement is acquired before the old lease drops.
    pub(super) fn rebind(
        &mut self,
        state_dir: &Path,
        identity: PrInvocationIdentity,
    ) -> Result<(), PrInvocationAcquireError> {
        if self.identity == identity {
            return Ok(());
        }
        let replacement = Self::acquire(state_dir, identity)?;
        *self = replacement;
        Ok(())
    }

    pub(super) fn rebind_machine(
        &mut self,
        identity: PrInvocationIdentity,
    ) -> Result<(), PrInvocationAcquireError> {
        self.rebind(&crate::paths::machine_coordination_state_dir(), identity)
    }

    pub(super) fn transition_guard_machine(&self) -> io::Result<PrInvocationTransitionGuard> {
        PrInvocationTransitionGuard::acquire_machine(&self.identity.repo, &self.identity.branch)
    }

    pub(super) fn owns(&self, identity: &PrInvocationIdentity) -> bool {
        self.identity == *identity
    }
}

impl Drop for PrInvocationLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn write_owner(file: &mut File, identity: &PrInvocationIdentity) -> io::Result<()> {
    let owner = PrInvocationOwner {
        schema_version: 1,
        repo: identity.repo.clone(),
        branch: identity.branch.clone(),
        head: identity.head.clone(),
        pid: std::process::id(),
        started_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
    };
    let bytes = serde_json::to_vec(&owner).map_err(io::Error::other)?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&bytes)?;
    file.sync_data()
}

fn read_owner_summary(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let owner: PrInvocationOwner = serde_json::from_slice(&bytes).ok()?;
    Some(format!("pid={} since {}", owner.pid, owner.started_at))
}

fn lock_path(directory: &Path, identity: &PrInvocationIdentity) -> PathBuf {
    let mut digest = Sha256::new();
    for field in [&identity.repo, &identity.branch, &identity.head] {
        digest.update(field.as_bytes());
        digest.update([0]);
    }
    directory.join(format!("{}.lock", hex::encode(&digest.finalize()[..16])))
}

fn transition_lock_path(directory: &Path, repo: &str, branch: &str) -> PathBuf {
    let mut digest = Sha256::new();
    for field in [repo, branch] {
        digest.update(field.as_bytes());
        digest.update([0]);
    }
    directory.join(format!("{}.lock", hex::encode(&digest.finalize()[..16])))
}

fn short_head(head: &str) -> &str {
    head.get(..12).unwrap_or(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(repo: &str, branch: &str, head: char) -> PrInvocationIdentity {
        PrInvocationIdentity {
            repo: repo.to_owned(),
            branch: branch.to_owned(),
            head: head.to_string().repeat(40),
        }
    }

    #[test]
    fn duplicate_exact_head_is_single_flight() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = PrInvocationLease::acquire(temp.path(), identity("o/r", "feature", 'a'))
            .expect("first lease");
        let duplicate = PrInvocationLease::acquire(temp.path(), identity("o/r", "feature", 'a'));
        assert!(matches!(
            duplicate,
            Err(PrInvocationAcquireError::Busy { .. })
        ));
        drop(first);
        PrInvocationLease::acquire(temp.path(), identity("o/r", "feature", 'a'))
            .expect("released lease is reusable");
    }

    #[test]
    fn different_heads_and_repositories_coexist() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _first = PrInvocationLease::acquire(temp.path(), identity("o/r", "feature", 'a'))
            .expect("first");
        let _new_head = PrInvocationLease::acquire(temp.path(), identity("o/r", "feature", 'b'))
            .expect("different head");
        let _other_repo = PrInvocationLease::acquire(temp.path(), identity("o/s", "feature", 'a'))
            .expect("different repo");
    }

    #[test]
    fn stale_metadata_and_pid_reuse_do_not_block_recovery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = identity("o/r", "feature", 'a');
        let directory = temp.path().join("pr-invocations");
        fs::create_dir_all(&directory).expect("directory");
        fs::write(
            lock_path(&directory, &key),
            br#"{"schema_version":1,"repo":"o/r","branch":"feature","head":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","pid":1,"started_at":"stale"}"#,
        )
        .expect("stale owner");

        PrInvocationLease::acquire(temp.path(), key)
            .expect("an unlocked stale PID record has no authority");
    }

    #[test]
    fn rebind_acquires_new_head_before_releasing_old_head() {
        let temp = tempfile::tempdir().expect("tempdir");
        let old = identity("o/r", "feature", 'a');
        let new = identity("o/r", "feature", 'b');
        let mut lease = PrInvocationLease::acquire(temp.path(), old.clone()).expect("old");
        let blocker = PrInvocationLease::acquire(temp.path(), new.clone()).expect("new blocker");
        assert!(matches!(
            lease.rebind(temp.path(), new),
            Err(PrInvocationAcquireError::Busy { .. })
        ));
        assert!(matches!(
            PrInvocationLease::acquire(temp.path(), old),
            Err(PrInvocationAcquireError::Busy { .. })
        ));
        drop(blocker);
    }

    #[test]
    fn lease_identity_rejects_a_concurrently_advanced_head() {
        let temp = tempfile::tempdir().expect("tempdir");
        let old = identity("o/r", "feature", 'a');
        let new = identity("o/r", "feature", 'b');
        let lease = PrInvocationLease::acquire(temp.path(), old.clone()).expect("lease");
        assert!(lease.owns(&old));
        assert!(!lease.owns(&new));
    }

    #[test]
    fn head_transition_guard_fails_fast_but_not_exact_head_lifetime() {
        let temp = tempfile::tempdir().expect("tempdir");
        let directory = temp.path().join("transitions");
        let first = PrInvocationTransitionGuard::acquire(&directory, "o/r", "feature")
            .expect("first guard");
        assert!(
            PrInvocationTransitionGuard::acquire(&directory, "o/r", "feature")
                .is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock),
            "second observer must fail in-flight instead of hanging"
        );
        drop(first);
        PrInvocationTransitionGuard::acquire(&directory, "o/r", "feature")
            .expect("observer proceeds after transition");

        let _old = PrInvocationLease::acquire(temp.path(), identity("o/r", "feature", 'a'))
            .expect("old exact head");
        let _new = PrInvocationLease::acquire(temp.path(), identity("o/r", "feature", 'b'))
            .expect("different exact heads coexist after observation");
    }

    #[cfg(unix)]
    #[test]
    fn owner_death_releases_the_kernel_lease_without_pid_recovery() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let temp = tempfile::tempdir().expect("tempdir");
        let ready = temp.path().join("ready");
        let mut holder = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "app::pr_invocation::tests::lease_holder_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("SHIPYARD_LEASE_TEST_STATE", temp.path())
            .env("SHIPYARD_LEASE_TEST_READY", &ready)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn holder");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "lease holder did not become ready");
        assert!(matches!(
            PrInvocationLease::acquire(temp.path(), identity("o/r", "feature", 'a')),
            Err(PrInvocationAcquireError::Busy { .. })
        ));

        let status = Command::new("kill")
            .args(["-KILL", &holder.id().to_string()])
            .status()
            .expect("kill holder");
        assert!(status.success());
        holder.wait().expect("reap holder");
        PrInvocationLease::acquire(temp.path(), identity("o/r", "feature", 'a'))
            .expect("kernel released lease when owner died");
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess helper for owner_death_releases_the_kernel_lease_without_pid_recovery"]
    fn lease_holder_helper() {
        let state = std::env::var_os("SHIPYARD_LEASE_TEST_STATE")
            .map(PathBuf::from)
            .expect("state path");
        let ready = std::env::var_os("SHIPYARD_LEASE_TEST_READY")
            .map(PathBuf::from)
            .expect("ready path");
        let _lease = PrInvocationLease::acquire(&state, identity("o/r", "feature", 'a'))
            .expect("holder lease");
        fs::write(ready, b"ready").expect("ready marker");
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_refusal_does_not_kill_an_unrelated_sibling() {
        use std::process::{Command, Stdio};

        let temp = tempfile::tempdir().expect("tempdir");
        let _lease = PrInvocationLease::acquire(temp.path(), identity("o/r", "feature", 'a'))
            .expect("lease");
        let mut sibling = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("sibling");

        assert!(matches!(
            PrInvocationLease::acquire(temp.path(), identity("o/r", "feature", 'a')),
            Err(PrInvocationAcquireError::Busy { .. })
        ));
        assert!(
            sibling.try_wait().expect("sibling status").is_none(),
            "single-flight refusal must not signal sibling work"
        );
        sibling.kill().expect("cleanup sibling");
        sibling.wait().expect("reap sibling");
    }
}
