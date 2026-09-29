//! Persisted per-flag ledger: when each flag was first seen, whether it was
//! addressed, which sticky comment this tool owns on each pull request, and
//! the digest claim.
//!
//! A flag's clock restarts when it is addressed: the condition stopped
//! holding, the pull request got a new head since the flag was first seen,
//! the pull request closed or merged, or someone added the
//! [`super::ACK_LABEL`] label. The digest only carries flags that have held on
//! the same head for the minimum age, so a person who is already iterating is
//! never paged about the head they just replaced.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::flags::{DigestRoute, Flag, FlagKind};
use crate::file_lock::LockedFile;

/// Ledger schema identifier.
pub const LEDGER_SCHEMA: &str = "shipyard.pr-watch.ledger/v1";
/// Addressed entries older than this are pruned.
pub const PRUNE_AFTER_DAYS: i64 = 14;

/// The whole persisted state for one repository and base.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    /// [`LEDGER_SCHEMA`].
    pub schema: String,
    /// `OWNER/REPO`.
    pub repo: String,
    /// Base branch.
    pub base: String,
    /// Flag episodes by flag id (`pr:kind:key`).
    pub entries: BTreeMap<String, LedgerEntry>,
    /// Sticky comments this tool created, by pull request.
    pub comments: BTreeMap<u64, CommentRecord>,
    /// Last completed scan.
    pub last_scan_at: Option<DateTime<Utc>>,
    /// Last delivered digest.
    pub last_digest_at: Option<DateTime<Utc>>,
    /// A digest claimed but not yet confirmed delivered.
    pub digest_claim: Option<DigestClaim>,
    /// When each shared-failure test was last announced by a digest.
    #[serde(default)]
    pub shared_announced: BTreeMap<String, DateTime<Utc>>,
}

impl Ledger {
    /// An empty ledger.
    #[must_use]
    pub fn new(repo: &str, base: &str) -> Self {
        Self {
            schema: LEDGER_SCHEMA.to_owned(),
            repo: repo.to_owned(),
            base: base.to_owned(),
            entries: BTreeMap::new(),
            comments: BTreeMap::new(),
            last_scan_at: None,
            last_digest_at: None,
            digest_claim: None,
            shared_announced: BTreeMap::new(),
        }
    }

    /// Active (unaddressed) entries of one pull request.
    pub fn active_for(&self, pr: u64) -> impl Iterator<Item = &LedgerEntry> {
        self.entries
            .values()
            .filter(move |entry| entry.pr == pr && entry.addressed_at.is_none())
    }
}

/// One flag episode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Pull request.
    pub pr: u64,
    /// Rule.
    pub kind: FlagKind,
    /// Discriminator.
    pub key: String,
    /// Pull request title when last seen.
    pub title: String,
    /// Pull request URL.
    pub url: String,
    /// Latest verdict.
    pub verdict: String,
    /// Latest evidence line.
    pub evidence: String,
    /// Head when the episode began.
    pub head_sha: String,
    /// Episode start.
    pub first_seen_at: DateTime<Utc>,
    /// Latest scan that still saw it.
    pub last_seen_at: DateTime<Utc>,
    /// When it stopped holding (or was acknowledged).
    pub addressed_at: Option<DateTime<Utc>>,
    /// `cleared`, `new_head`, `merged`, `closed`, or `ack_label`.
    pub addressed_reason: Option<String>,
    /// When a digest carried it.
    pub digested_at: Option<DateTime<Utc>>,
    /// How the digest treats it.
    #[serde(default)]
    pub route: DigestRoute,
    /// Tests shared with other pull requests ([`DigestRoute::Shared`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_tests: Vec<String>,
    /// Other pull requests failing them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_prs: Vec<u64>,
}

/// A sticky comment this tool owns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentRecord {
    /// Issue-comment id.
    pub comment_id: u64,
    /// SHA-256 of the body last written.
    pub body_sha256: String,
}

/// A digest claimed for delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestClaim {
    /// When it was claimed.
    pub claimed_at: DateTime<Utc>,
    /// Entry ids it carries.
    pub ids: Vec<String>,
    /// Shared-failure test keys it announces.
    #[serde(default)]
    pub shared: Vec<String>,
}

/// What a pull request looks like right now, for [`reconcile`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrNow {
    /// Open at the scan instant.
    pub open: bool,
    /// Merged (when not open).
    pub merged: bool,
    /// Current head.
    pub head_sha: String,
    /// Has [`super::ACK_LABEL`].
    pub acknowledged: bool,
    /// Title.
    pub title: String,
    /// URL.
    pub url: String,
}

/// One ledger change, for the audit log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEvent {
    /// When.
    pub at: DateTime<Utc>,
    /// Entry id.
    pub id: String,
    /// `opened` or `addressed:<reason>`.
    pub change: String,
    /// Evidence at the time.
    pub evidence: String,
}

/// Fold one scan's flags into the ledger.
#[allow(clippy::too_many_lines)]
pub fn reconcile(
    ledger: &mut Ledger,
    flags: &[Flag],
    prs: &BTreeMap<u64, PrNow>,
    now: DateTime<Utc>,
) -> Vec<LedgerEvent> {
    let mut events = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for flag in flags {
        let id = flag.id();
        seen.insert(id.clone());
        let context = prs.get(&flag.pr);
        let acknowledged = context.is_some_and(|pr| pr.acknowledged);
        let (title, url) = context
            .map(|pr| (pr.title.clone(), pr.url.clone()))
            .unwrap_or_default();
        let fresh = |head: &str| LedgerEntry {
            pr: flag.pr,
            kind: flag.kind,
            key: flag.key.clone(),
            title: title.clone(),
            url: url.clone(),
            verdict: flag.verdict.clone(),
            evidence: flag.evidence.clone(),
            head_sha: head.to_owned(),
            first_seen_at: now,
            last_seen_at: now,
            addressed_at: None,
            addressed_reason: None,
            digested_at: None,
            route: flag.route,
            shared_tests: flag.shared_tests.clone(),
            related_prs: flag.related_prs.clone(),
        };
        match ledger.entries.get_mut(&id) {
            Some(entry)
                if entry.addressed_at.is_none()
                    && (entry.head_sha == flag.head_sha || !flag.kind.restarts_on_new_head()) =>
            {
                entry.last_seen_at = now;
                entry.head_sha.clone_from(&flag.head_sha);
                entry.verdict.clone_from(&flag.verdict);
                entry.evidence.clone_from(&flag.evidence);
                entry.route = flag.route;
                entry.shared_tests.clone_from(&flag.shared_tests);
                entry.related_prs.clone_from(&flag.related_prs);
                entry.title.clone_from(&title);
                entry.url.clone_from(&url);
                if acknowledged {
                    entry.addressed_at = Some(now);
                    entry.addressed_reason = Some("ack_label".to_owned());
                    events.push(event(now, &id, "addressed:ack_label", &entry.evidence));
                }
            }
            Some(entry)
                if entry.addressed_reason.as_deref() == Some("ack_label") && acknowledged =>
            {
                // Still acknowledged: stay quiet.
                entry.last_seen_at = now;
            }
            Some(entry) => {
                if entry.addressed_at.is_none() {
                    events.push(event(now, &id, "addressed:new_head", &entry.evidence));
                }
                *entry = fresh(&flag.head_sha);
                events.push(event(now, &id, "opened", &flag.evidence));
                if acknowledged {
                    entry.addressed_at = Some(now);
                    entry.addressed_reason = Some("ack_label".to_owned());
                }
            }
            None => {
                let mut entry = fresh(&flag.head_sha);
                if acknowledged {
                    entry.addressed_at = Some(now);
                    entry.addressed_reason = Some("ack_label".to_owned());
                }
                events.push(event(now, &id, "opened", &flag.evidence));
                ledger.entries.insert(id, entry);
            }
        }
    }
    for (id, entry) in &mut ledger.entries {
        if seen.contains(id) || entry.addressed_at.is_some() {
            continue;
        }
        let reason = match prs.get(&entry.pr) {
            Some(pr) if !pr.open && pr.merged => "merged",
            Some(pr) if !pr.open => "closed",
            Some(pr) if pr.head_sha != entry.head_sha && entry.kind.restarts_on_new_head() => {
                "new_head"
            }
            _ => "cleared",
        };
        entry.addressed_at = Some(now);
        entry.addressed_reason = Some(reason.to_owned());
        events.push(event(
            now,
            id,
            &format!("addressed:{reason}"),
            &entry.evidence,
        ));
    }
    let horizon = now - Duration::days(PRUNE_AFTER_DAYS);
    ledger
        .entries
        .retain(|_, entry| entry.addressed_at.is_none_or(|at| at > horizon));
    ledger.last_scan_at = Some(now);
    events
}

fn event(at: DateTime<Utc>, id: &str, change: &str, evidence: &str) -> LedgerEvent {
    LedgerEvent {
        at,
        id: id.to_owned(),
        change: change.to_owned(),
        evidence: evidence.to_owned(),
    }
}

/// Default ledger path for a repository/base pair.
#[must_use]
pub fn default_path(state_root: &Path, repo: &str, base: &str) -> PathBuf {
    let digest = format!("{:x}", Sha256::digest(format!("{repo}\0{base}").as_bytes()));
    state_root
        .join("pr-watch")
        .join(format!("{}-{}.json", repo.replace('/', "-"), &digest[..24]))
}

/// The ledger's exclusive lock, held across one read-modify-write.
///
/// # Errors
/// When the lock file cannot be created or locked.
pub fn lock(path: &Path) -> Result<LockedFile, String> {
    let lock_path = path.with_extension("lock");
    if let Some(parent) = lock_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("open {}: {error}", lock_path.display()))?;
    FileExt::lock_exclusive(&file)
        .map_err(|error| format!("lock {}: {error}", lock_path.display()))?;
    Ok(LockedFile::new(file))
}

/// Load the ledger, or a fresh one when the file does not exist.
///
/// # Errors
/// When the file exists but cannot be read or parsed, or belongs to another
/// repository.
pub fn load(path: &Path, repo: &str, base: &str) -> Result<Ledger, String> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let ledger: Ledger = serde_json::from_str(&raw)
                .map_err(|error| format!("parse {}: {error}", path.display()))?;
            if ledger.schema != LEDGER_SCHEMA {
                return Err(format!("{} has schema {}", path.display(), ledger.schema));
            }
            if !ledger.repo.eq_ignore_ascii_case(repo) || ledger.base != base {
                return Err(format!(
                    "{} belongs to {}:{}, not {repo}:{base}",
                    path.display(),
                    ledger.repo,
                    ledger.base
                ));
            }
            Ok(ledger)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::new(repo, base)),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

/// Atomically persist the ledger.
///
/// # Errors
/// When the file cannot be written.
pub fn save(path: &Path, ledger: &Ledger) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("temporary ledger file: {error}"))?;
    serde_json::to_writer_pretty(&mut temp, ledger)
        .map_err(|error| format!("encode ledger: {error}"))?;
    temp.write_all(b"\n")
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|error| format!("write ledger: {error}"))?;
    temp.persist(path)
        .map_err(|error| format!("persist {}: {}", path.display(), error.error))?;
    Ok(())
}

/// Append audit events as NDJSON beside the ledger.
///
/// # Errors
/// When the log cannot be written.
pub fn append_events(path: &Path, events: &[LedgerEvent]) -> Result<(), String> {
    if events.is_empty() {
        return Ok(());
    }
    let log = path.with_extension("events.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|error| format!("open {}: {error}", log.display()))?;
    let mut payload = Vec::new();
    for event in events {
        serde_json::to_writer(&mut payload, event).map_err(|error| error.to_string())?;
        payload.push(b'\n');
    }
    file.write_all(&payload)
        .map_err(|error| format!("append {}: {error}", log.display()))
}

/// SHA-256 hex of a comment body.
#[must_use]
pub fn body_sha256(body: &str) -> String {
    format!("{:x}", Sha256::digest(body.as_bytes()))
}
