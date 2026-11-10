//! Short-lived on-disk cache for the GitHub reads JIT runner admission repeats.
//!
//! `runner admission-clean` runs before every VM a `TartCI` lane mints, and each
//! clean `--apply` verdict re-reads the same repository policy and the same
//! per-head check and status lists up to four times (observation, fence,
//! revalidation and its fence). Those reads are keyed by an immutable head SHA
//! or by repository policy that changes rarely, so repeating them inside a few
//! minutes buys no freshness and spends the shared App installation's REST
//! budget, which is exactly what starves admission when agents are busy.
//!
//! Only GET endpoints whose answer is a function of their argv are cached:
//! repository settings, evaluated branch rules, classic required status checks,
//! and the check runs and commit statuses of a full 40-hex commit SHA. Pull
//! request lists, the merge queue and workflow runs are never cached; they are
//! the observations admission exists to make fresh. Failures are never cached.
//!
//! The cache is advisory. Every filesystem error degrades to a live read, and
//! writes take the production writer domain without waiting.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How long an admission read stays reusable when nothing overrides it.
pub(crate) const DEFAULT_ADMISSION_READ_CACHE_TTL: Duration = Duration::from_mins(3);
/// Overrides the TTL in whole seconds; `0` disables the cache.
pub(crate) const ADMISSION_READ_CACHE_TTL_ENV: &str = "SHIPYARD_ADMISSION_READ_CACHE_SECS";
/// Longest TTL the override accepts. Check identity is only stable per head;
/// a longer window would hide a newly posted handoff status for too long.
const MAX_ADMISSION_READ_CACHE_TTL: Duration = Duration::from_mins(10);

#[derive(Debug)]
pub(crate) struct ReadCache {
    dir: PathBuf,
    ttl: Duration,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    args: Vec<String>,
    stored_at_secs: u64,
    body: String,
}

impl ReadCache {
    /// The admission cache under `state_dir`, or `None` when the operator
    /// disabled it with [`ADMISSION_READ_CACHE_TTL_ENV`]`=0`. An unparseable
    /// override keeps the default rather than failing admission.
    pub(crate) fn for_admission(state_dir: &Path) -> Option<Self> {
        let ttl = std::env::var(ADMISSION_READ_CACHE_TTL_ENV)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .map_or(DEFAULT_ADMISSION_READ_CACHE_TTL, Duration::from_secs)
            .min(MAX_ADMISSION_READ_CACHE_TTL);
        Self::new(
            state_dir
                .join("runner-admission-observation")
                .join("read-cache"),
            ttl,
        )
    }

    pub(crate) fn new(dir: PathBuf, ttl: Duration) -> Option<Self> {
        if ttl.is_zero() {
            return None;
        }
        let cache = Self { dir, ttl };
        cache.prune();
        Some(cache)
    }

    /// A fresh cached answer for `args`, if this read is cacheable and one
    /// was stored within the TTL.
    pub(crate) fn get(&self, args: &[String]) -> Option<String> {
        if !is_cacheable(args) {
            return None;
        }
        let raw = fs::read(self.path_for(args)).ok()?;
        let entry: Entry = serde_json::from_slice(&raw).ok()?;
        // The digest names the file; the stored argv proves it.
        if entry.args != args {
            return None;
        }
        let age = now_secs().checked_sub(entry.stored_at_secs)?;
        (age < self.ttl.as_secs()).then_some(entry.body)
    }

    /// Remember a successful answer for `args`. Best effort.
    pub(crate) fn put(&self, args: &[String], body: &str) {
        if !is_cacheable(args) {
            return;
        }
        let _ = self.try_put(args, body);
    }

    fn try_put(&self, args: &[String], body: &str) -> std::io::Result<()> {
        let path = self.path_for(args);
        let Ok(_writer) = crate::writer_domain_lease::acquire_for_protected_path_with_timeout(
            &path,
            Duration::ZERO,
        ) else {
            return Ok(());
        };
        crate::writer_domain_lease::ensure_protected_dir_all(&self.dir)?;
        let entry = Entry {
            args: args.to_vec(),
            stored_at_secs: now_secs(),
            body: body.to_owned(),
        };
        let bytes = serde_json::to_vec(&entry)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.dir)?;
        temp.write_all(&bytes)?;
        temp.persist(&path).map_err(|error| error.error)?;
        Ok(())
    }

    /// Drop entries well past their TTL so per-head files do not accumulate.
    fn prune(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let horizon = self.ttl.saturating_mul(4);
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let stale = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age > horizon);
            if stale {
                let _ = fs::remove_file(path);
            }
        }
    }

    fn path_for(&self, args: &[String]) -> PathBuf {
        let digest = Sha256::digest(args.join("\0").as_bytes());
        self.dir.join(format!("{digest:x}.json"))
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Whether `args` is one of the argv-determined GET reads listed in the module
/// docs. Anything with a method, field or header flag is refused, so a write
/// can never be answered from the cache.
pub(crate) fn is_cacheable(args: &[String]) -> bool {
    let (endpoint, rest) = match args {
        [api, endpoint, rest @ ..] if api == "api" => (endpoint.as_str(), rest),
        _ => return false,
    };
    let Some(path) = endpoint.strip_prefix("repos/") else {
        return false;
    };
    let mut segments = path.split('/');
    let (Some(owner), Some(name)) = (segments.next(), segments.next()) else {
        return false;
    };
    if owner.is_empty() || name.is_empty() || name.contains('?') {
        return false;
    }
    let tail = segments.collect::<Vec<_>>();
    match (tail.as_slice(), rest) {
        ([], []) => true,
        (["rules", "branches", branch], [paginate, slurp]) => {
            !branch.is_empty() && paginate == "--paginate" && slurp == "--slurp"
        }
        (["branches", branch, "protection", "required_status_checks"], []) => !branch.is_empty(),
        (["commits", sha, list], []) => {
            let list = list.split_once('?').map_or(*list, |(list, _)| list);
            is_full_sha(sha) && matches!(list, "check-runs" | "statuses")
        }
        _ => false,
    }
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn only_argv_determined_reads_are_cacheable() {
        for cacheable in [
            argv(&["api", "repos/o/r"]),
            argv(&[
                "api",
                "repos/o/r/rules/branches/main",
                "--paginate",
                "--slurp",
            ]),
            argv(&[
                "api",
                "repos/o/r/branches/main/protection/required_status_checks",
            ]),
            argv(&[
                "api",
                &format!("repos/o/r/commits/{SHA}/check-runs?per_page=100&page=1"),
            ]),
            argv(&[
                "api",
                &format!("repos/o/r/commits/{SHA}/statuses?per_page=100&page=2"),
            ]),
        ] {
            assert!(is_cacheable(&cacheable), "{cacheable:?}");
        }
        for live in [
            argv(&["pr", "list", "--repo", "o/r"]),
            argv(&["api", "graphql", "-f", "query=x"]),
            argv(&[
                "api",
                "repos/o/r/actions/runs?status=queued&per_page=100&page=1",
            ]),
            argv(&[
                "api",
                "repos/o/r/commits/main/check-runs?per_page=100&page=1",
            ]),
            argv(&["api", "repos/o/r/pulls/7"]),
            argv(&["api", "-X", "POST", "repos/o/r"]),
            argv(&["api", "repos/o/r", "-X", "PATCH"]),
            argv(&[
                "api",
                &format!("repos/o/r/commits/{SHA}/statuses"),
                "-f",
                "state=success",
            ]),
            argv(&["api", "repos/o/r/rules/branches/main"]),
        ] {
            assert!(!is_cacheable(&live), "{live:?}");
        }
    }

    #[test]
    fn a_stored_read_is_served_until_its_ttl_and_never_for_other_argv() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ReadCache::new(temp.path().join("cache"), Duration::from_mins(1)).unwrap();
        let read = argv(&[
            "api",
            &format!("repos/o/r/commits/{SHA}/statuses?per_page=100&page=1"),
        ]);
        assert_eq!(cache.get(&read), None);
        cache.put(&read, "[]");
        assert_eq!(cache.get(&read).as_deref(), Some("[]"));
        let other = argv(&[
            "api",
            &format!("repos/o/r/commits/{SHA}/check-runs?per_page=100&page=1"),
        ]);
        assert_eq!(cache.get(&other), None);

        // An entry older than the TTL is a miss.
        let path = cache.path_for(&read);
        let mut entry: Entry = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        entry.stored_at_secs -= 61;
        fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();
        assert_eq!(cache.get(&read), None);

        // A file whose stored argv differs from the request is never served.
        entry.stored_at_secs = now_secs();
        entry.args = other.clone();
        fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();
        assert_eq!(cache.get(&read), None);
    }

    #[test]
    fn uncacheable_reads_are_never_stored() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("cache");
        let cache = ReadCache::new(dir.clone(), Duration::from_mins(1)).unwrap();
        let live = argv(&[
            "api",
            "repos/o/r/actions/runs?status=queued&per_page=100&page=1",
        ]);
        cache.put(&live, "{}");
        assert_eq!(cache.get(&live), None);
        assert!(
            !dir.exists(),
            "an uncacheable read must not create the cache"
        );
    }

    #[test]
    fn a_zero_ttl_disables_the_cache() {
        let temp = tempfile::tempdir().unwrap();
        assert!(ReadCache::new(temp.path().join("cache"), Duration::ZERO).is_none());
    }
}
