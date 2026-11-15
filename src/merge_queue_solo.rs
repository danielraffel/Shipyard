//! Solo merge-queue reservations: hold one repository's queue for one landing.
//!
//! GitHub's merge queue batches entries, so a pull request that must land
//! alone (a release coalesce, a fix for a red base) shares its merge group
//! with whatever was queued ahead of it. A reservation names that pull
//! request. While it is active:
//!
//! * every Shipyard admission into the repository's queue for any other pull
//!   request is refused before GitHub is contacted (the steward reports
//!   `held_solo_reservation:#<n>`), and
//! * the `ghapp` queue-removal guard accepts a `solo` removal of an entry
//!   *ahead of* the reserved pull request, and nothing else.
//!
//! A reservation is a pause and a bound on removals, never evidence of
//! approval. It is created only for a pull request whose current head carries
//! a verified exact-head approval ([`crate::head_approval`]); the reserved
//! pull request itself still passes every admission gate unchanged. It ends
//! when the pull request merges, closes, changes head, leaves the queue after
//! being seen in it, or when its TTL runs out.
//!
//! It fails closed: a reservation file that cannot be read or checked pauses
//! every admission into that repository's queue and grants no removal, until
//! `shipyard merge-queue solo-release` removes it.
//!
//! The record lives under the machine state root and is protected like the
//! other merge-queue control files: 0600 in a 0700 directory, never followed
//! through a link. That ties it to this user's Shipyard; it is not a boundary
//! against another process of the same user forging Shipyard's own state.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Directory under the state root holding one reservation per repository.
pub const SOLO_DIR: &str = "merge_queue/solo";
/// Audit log of every reservation created, latched and ended.
pub const SOLO_AUDIT_FILE: &str = "merge_queue/solo/audit.jsonl";
/// A reservation's lifetime unless the operator asks for less.
pub const DEFAULT_TTL_MINUTES: i64 = 120;
/// The longest lifetime a reservation may ask for.
pub const MAX_TTL_MINUTES: i64 = 480;
const SCHEMA: u32 = 1;
const KIND: &str = "shipyard.merge-queue-solo";

/// The admission actions a reservation pauses. Anything that puts a pull
/// request into a queue or merges it: enqueue, arm, jump, re-arm, and the
/// exact-head merge of a base without a queue.
const ADMISSION_ACTIONS: &[&str] = &[
    crate::merge_queue_control::ARM_ACTION,
    "enqueue pull request",
    "runner steward enqueue pull request",
    "runner steward restore hosted setup eviction priority",
    "runner steward move the base fix to the front of the merge queue",
    "runner steward exact-head merge on a server-enforced base",
    "runner carrier rearm exact head",
];

/// Whether a merge-queue mutation `action` admits a pull request, and so is
/// paused by another pull request's reservation.
#[must_use]
pub fn is_admission_mutation(action: &str) -> bool {
    ADMISSION_ACTIONS.contains(&action)
}

/// The audited mutation guard's backstop: refuse an admission `action` for
/// `ship`'s pull request while another one holds its queue alone, or
/// while the reservation is unreadable, so no admission path can miss it.
pub fn refuse_admission(
    state_root: &Path,
    ship: &crate::ship_state::ShipState,
    action: &str,
) -> Result<(), String> {
    if !is_admission_mutation(action) {
        return Ok(());
    }
    let state = read(state_root, &ship.repo, Utc::now());
    match state.admission_hold(&ship.base_branch, ship.pr) {
        Some(token) => Err(format!("{token}: {}", state.describe())),
        None => Ok(()),
    }
}

/// Why a pull request needs to land alone. Closed set: the guard's audit and
/// the steward's report name it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SoloReason {
    /// A release or release coalesce that must be its own merge group.
    Release,
    /// The fix for a red base, which must not share a group with work that
    /// cannot pass until it lands.
    MainRedFix,
    /// An incident fix the operator needs to land before anything else.
    Incident,
}

impl SoloReason {
    /// Parse the CLI spelling.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "release" => Ok(Self::Release),
            "main-red-fix" => Ok(Self::MainRedFix),
            "incident" => Ok(Self::Incident),
            other => Err(format!(
                "`{other}` is not a solo reason class; use release, main-red-fix or incident"
            )),
        }
    }

    /// The CLI and audit spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::MainRedFix => "main-red-fix",
            Self::Incident => "incident",
        }
    }
}

/// One repository's reservation, as written to disk.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    /// Record schema.
    pub schema: u32,
    /// Record kind.
    pub kind: String,
    /// `owner/name`.
    pub repo: String,
    /// The base branch whose queue is reserved.
    pub base: String,
    /// The reserved pull request.
    pub pr: u64,
    /// The head approved when the reservation was made. A new head ends it.
    pub head: String,
    /// Why it must land alone.
    pub reason: SoloReason,
    /// What the operator said.
    pub note: String,
    /// The approval that admitted the reservation, as the gate described it.
    pub approval: String,
    /// When it was made.
    pub created_at: DateTime<Utc>,
    /// When it ends regardless of the pull request.
    pub expires_at: DateTime<Utc>,
    /// Set once the steward saw the pull request in the queue; leaving the
    /// queue after that ends the reservation.
    #[serde(default)]
    pub seen_in_queue: bool,
}

impl Reservation {
    /// A fresh reservation lasting `ttl_minutes` from `now`.
    #[allow(clippy::too_many_arguments)] // One flat record; every field is named at the call.
    pub fn new(
        repo: &str,
        base: &str,
        pr: u64,
        head: &str,
        reason: SoloReason,
        note: &str,
        approval: &str,
        ttl_minutes: i64,
        now: DateTime<Utc>,
    ) -> Result<Self, String> {
        if !(1..=MAX_TTL_MINUTES).contains(&ttl_minutes) {
            return Err(format!(
                "a solo reservation lasts 1 to {MAX_TTL_MINUTES} minutes, not {ttl_minutes}"
            ));
        }
        let reservation = Self {
            schema: SCHEMA,
            kind: KIND.to_owned(),
            repo: repo.to_owned(),
            base: base.to_owned(),
            pr,
            head: head.to_owned(),
            reason,
            note: note.trim().to_owned(),
            approval: approval.to_owned(),
            created_at: now,
            expires_at: now + Duration::minutes(ttl_minutes),
            seen_in_queue: false,
        };
        reservation.validate(repo)?;
        Ok(reservation)
    }

    fn validate(&self, repo: &str) -> Result<(), String> {
        if self.schema != SCHEMA || self.kind != KIND {
            return Err(format!(
                "unsupported reservation record (schema {}, kind {})",
                self.schema, self.kind
            ));
        }
        if !self.repo.eq_ignore_ascii_case(repo) {
            return Err(format!(
                "the record names {} but is filed under {repo}",
                self.repo
            ));
        }
        if self.pr == 0 {
            return Err("the record names no pull request".to_owned());
        }
        if self.head.len() != 40 || !self.head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("the record's head is not a full commit SHA".to_owned());
        }
        if self.base.trim().is_empty() || self.note.is_empty() {
            return Err("the record has no base or no note".to_owned());
        }
        let lifetime = self.expires_at - self.created_at;
        if lifetime <= Duration::zero() || lifetime > Duration::minutes(MAX_TTL_MINUTES) {
            return Err("the record's lifetime is outside the allowed TTL".to_owned());
        }
        Ok(())
    }

    fn same_identity(&self, other: &Self) -> bool {
        self.pr == other.pr
            && self.head == other.head
            && self.created_at == other.created_at
            && self.repo.eq_ignore_ascii_case(&other.repo)
    }
}

/// What the state root says about one repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SoloState {
    /// No reservation.
    Absent,
    /// A live reservation.
    Active(Reservation),
    /// A reservation whose TTL ran out; it pauses nothing.
    Expired(Reservation),
    /// A file that is there but cannot be trusted. Pauses every admission
    /// into the repository's queue and grants no removal.
    Unreadable {
        /// The file.
        path: PathBuf,
        /// Why it cannot be read.
        detail: String,
    },
}

impl SoloState {
    /// The steward's token for a pull request this state keeps out of the
    /// queue on `base`, or `None` when it may proceed through its own gates.
    #[must_use]
    pub fn admission_hold(&self, base: &str, pr: u64) -> Option<String> {
        match self {
            Self::Active(reservation) if reservation.base == base && reservation.pr != pr => {
                Some(format!("held_solo_reservation:#{}", reservation.pr))
            }
            Self::Unreadable { .. } => Some("held_solo_reservation_unreadable".to_owned()),
            _ => None,
        }
    }

    /// One line for a refusal or a report.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Absent => "no solo reservation".to_owned(),
            Self::Active(reservation) => format!(
                "{}#{} holds the {} queue alone ({}: {}) until {}",
                reservation.repo,
                reservation.pr,
                reservation.base,
                reservation.reason.as_str(),
                reservation.note,
                reservation.expires_at.to_rfc3339()
            ),
            Self::Expired(reservation) => format!(
                "the solo reservation for {}#{} expired at {}",
                reservation.repo,
                reservation.pr,
                reservation.expires_at.to_rfc3339()
            ),
            Self::Unreadable { path, detail } => format!(
                "solo reservation {} is unreadable ({detail}); every admission into this \
                 repository's queue is paused until `shipyard merge-queue solo-release` \
                 removes it",
                path.display()
            ),
        }
    }

    /// The JSON the steward and `merge-queue status` report.
    #[must_use]
    pub fn report(&self) -> Value {
        match self {
            Self::Absent => serde_json::json!({"state": "absent"}),
            Self::Active(reservation) => serde_json::json!({
                "state": "active",
                "reservation": reservation,
                "detail": self.describe(),
            }),
            Self::Expired(reservation) => serde_json::json!({
                "state": "expired",
                "reservation": reservation,
                "detail": self.describe(),
            }),
            Self::Unreadable { path, detail } => serde_json::json!({
                "state": "unreadable",
                "path": path.display().to_string(),
                "error": detail,
                "detail": self.describe(),
            }),
        }
    }
}

/// `owner/name` split and checked, so it can name a file.
fn repo_key(repo: &str) -> Result<String, String> {
    let (owner, name) = repo
        .split_once('/')
        .filter(|(owner, name)| !owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .ok_or_else(|| format!("`{repo}` is not an owner/name repository"))?;
    let allowed = |part: &str| {
        part.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            && part != "."
            && part != ".."
    };
    if !allowed(owner) || !allowed(name) {
        return Err(format!("`{repo}` is not an owner/name repository"));
    }
    Ok(format!(
        "{}__{}",
        owner.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    ))
}

/// The reservation file for `repo`.
pub fn reservation_path(state_root: &Path, repo: &str) -> Result<PathBuf, String> {
    Ok(state_root
        .join(SOLO_DIR)
        .join(format!("{}.json", repo_key(repo)?)))
}

fn lock_path(state_root: &Path, repo: &str) -> Result<PathBuf, String> {
    Ok(state_root
        .join(SOLO_DIR)
        .join(format!("{}.lock", repo_key(repo)?)))
}

/// Read `repo`'s reservation as of `now`.
#[must_use]
pub fn read(state_root: &Path, repo: &str, now: DateTime<Utc>) -> SoloState {
    let path = match reservation_path(state_root, repo) {
        Ok(path) => path,
        Err(detail) => {
            return SoloState::Unreadable {
                path: state_root.join(SOLO_DIR),
                detail,
            };
        }
    };
    let unreadable = |detail: String| SoloState::Unreadable {
        path: path.clone(),
        detail,
    };
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return SoloState::Absent,
        Err(error) => return unreadable(error.to_string()),
    };
    if !metadata.file_type().is_file() {
        return unreadable("not a regular file".to_owned());
    }
    if let Err(detail) = private_to_this_user(&metadata) {
        return unreadable(detail);
    }
    let mut text = String::new();
    if let Err(error) = open_no_follow(&path).and_then(|mut file| file.read_to_string(&mut text)) {
        return unreadable(error.to_string());
    }
    let reservation = match serde_json::from_str::<Reservation>(&text) {
        Ok(reservation) => reservation,
        Err(error) => return unreadable(format!("not a reservation record: {error}")),
    };
    if let Err(detail) = reservation.validate(repo) {
        return unreadable(detail);
    }
    if now >= reservation.expires_at {
        SoloState::Expired(reservation)
    } else {
        SoloState::Active(reservation)
    }
}

/// Every reservation file under the state root, for `merge-queue status`.
#[must_use]
pub fn list(state_root: &Path, now: DateTime<Utc>) -> Vec<(String, SoloState)> {
    let Ok(entries) = fs::read_dir(state_root.join(SOLO_DIR)) else {
        return Vec::new();
    };
    let mut found = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let key = name.strip_suffix(".json")?.to_owned();
            let (owner, repo) = key.split_once("__")?;
            Some(format!("{owner}/{repo}"))
        })
        .collect::<Vec<_>>();
    found.sort();
    found
        .into_iter()
        .map(|repo| {
            let state = read(state_root, &repo, now);
            (repo, state)
        })
        .collect()
}

#[cfg(unix)]
fn private_to_this_user(metadata: &fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;
    let uid = nix::unistd::geteuid().as_raw();
    if metadata.uid() != uid {
        return Err("owned by another user".to_owned());
    }
    if metadata.mode() & 0o077 != 0 {
        return Err("readable or writable by other users".to_owned());
    }
    Ok(())
}

#[cfg(not(unix))]
fn private_to_this_user(_metadata: &fs::Metadata) -> Result<(), String> {
    Ok(())
}

fn open_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits());
    }
    options.open(path)
}

fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("cannot restrict {}: {error}", dir.display()))?;
    }
    Ok(())
}

/// Exclusive per-repository lock for every write to a reservation.
struct RepoLock(File);

impl Drop for RepoLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

fn lock_repo(state_root: &Path, repo: &str) -> Result<RepoLock, String> {
    ensure_private_dir(&state_root.join(SOLO_DIR))?;
    let path = lock_path(state_root, repo)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    file.lock_exclusive()
        .map_err(|error| format!("cannot lock {}: {error}", path.display()))?;
    Ok(RepoLock(file))
}

fn write_record(path: &Path, reservation: &Reservation) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "reservation path has no parent".to_owned())?;
    let text = serde_json::to_string_pretty(reservation)
        .map_err(|error| format!("cannot encode the reservation: {error}"))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".solo-")
        .tempfile_in(parent)
        .map_err(|error| format!("cannot stage the reservation: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot restrict the reservation: {error}"))?;
    }
    temp.write_all(text.as_bytes())
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|error| format!("cannot write the reservation: {error}"))?;
    temp.persist(path)
        .map_err(|error| format!("cannot install the reservation: {}", error.error))?;
    Ok(())
}

fn audit(state_root: &Path, event: &str, repo: &str, fields: Value) -> Result<(), String> {
    let path = state_root.join(SOLO_AUDIT_FILE);
    let mut record = serde_json::json!({
        "at": Utc::now().to_rfc3339(),
        "event": event,
        "repo": repo,
    });
    if let (Some(record), Value::Object(fields)) = (record.as_object_mut(), fields) {
        record.extend(fields);
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    writeln!(file, "{record}")
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("cannot record the reservation audit: {error}"))
}

/// Install `reservation`. Refuses while another pull request holds a live
/// reservation on the repository, or while its file is unreadable.
pub fn reserve(state_root: &Path, reservation: &Reservation) -> Result<PathBuf, String> {
    let repo = reservation.repo.clone();
    let _lock = lock_repo(state_root, &repo)?;
    match read(state_root, &repo, reservation.created_at) {
        SoloState::Active(existing) if existing.pr != reservation.pr => {
            return Err(format!(
                "{repo}#{} already holds the queue alone until {}; release it first",
                existing.pr,
                existing.expires_at.to_rfc3339()
            ));
        }
        state @ SoloState::Unreadable { .. } => {
            return Err(format!(
                "{}; release it before making a new one",
                state.describe()
            ));
        }
        _ => {}
    }
    let path = reservation_path(state_root, &repo)?;
    write_record(&path, reservation)?;
    audit(
        state_root,
        "reserved",
        &repo,
        serde_json::to_value(reservation).unwrap_or(Value::Null),
    )?;
    Ok(path)
}

/// Remove `repo`'s reservation whatever its state, including an unreadable
/// one. Returns whether there was anything to remove.
pub fn release(state_root: &Path, repo: &str, why: &str) -> Result<bool, String> {
    let _lock = lock_repo(state_root, repo)?;
    let path = reservation_path(state_root, repo)?;
    let prior = read(state_root, repo, Utc::now());
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("cannot remove {}: {error}", path.display())),
    }
    audit(
        state_root,
        "released",
        repo,
        serde_json::json!({"why": why, "prior": prior.report()}),
    )?;
    Ok(true)
}

/// The reserved pull request as GitHub shows it now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveReserved {
    /// `OPEN`, `MERGED` or `CLOSED`.
    pub state: String,
    /// Current head.
    pub head: String,
    /// Whether it is in the merge queue.
    pub in_queue: bool,
}

impl LiveReserved {
    /// From a `PR_QUEUE_STATE_QUERY` answer; `None` when it names no pull
    /// request, so an unread state never ends a reservation.
    #[must_use]
    pub fn from_queue_state(response: &Value) -> Option<Self> {
        if response
            .get("errors")
            .is_some_and(|errors| !errors.is_null())
        {
            return None;
        }
        let pr = response.pointer("/data/repository/pullRequest")?;
        Some(Self {
            state: pr.get("state")?.as_str()?.to_owned(),
            head: pr.get("headRefOid")?.as_str()?.to_owned(),
            in_queue: pr.get("isInMergeQueue")?.as_bool()?,
        })
    }
}

/// What a steward pass does with an active reservation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reconcile {
    /// Leave it.
    Keep,
    /// The pull request is now in the queue: remember that.
    Latch,
    /// End it, for this reason.
    Clear(&'static str),
}

/// Decide one reservation from the live pull request (`None` when unread).
#[must_use]
pub fn reconcile_decision(
    reservation: &Reservation,
    live: Option<&LiveReserved>,
    now: DateTime<Utc>,
) -> Reconcile {
    if now >= reservation.expires_at {
        return Reconcile::Clear("expired");
    }
    let Some(live) = live else {
        return Reconcile::Keep;
    };
    match live.state.as_str() {
        "MERGED" => return Reconcile::Clear("merged"),
        "CLOSED" => return Reconcile::Clear("closed"),
        _ => {}
    }
    if !live.head.eq_ignore_ascii_case(&reservation.head) {
        return Reconcile::Clear("head_moved");
    }
    match (live.in_queue, reservation.seen_in_queue) {
        (true, false) => Reconcile::Latch,
        (false, true) => Reconcile::Clear("left_queue"),
        _ => Reconcile::Keep,
    }
}

/// Apply `decision` to the reservation it was made for, if the file still
/// holds that same reservation. Returns the state after.
pub fn apply_decision(
    state_root: &Path,
    reservation: &Reservation,
    decision: &Reconcile,
    now: DateTime<Utc>,
) -> Result<SoloState, String> {
    let repo = reservation.repo.clone();
    if *decision == Reconcile::Keep {
        return Ok(read(state_root, &repo, now));
    }
    let _lock = lock_repo(state_root, &repo)?;
    let current = match read(state_root, &repo, now) {
        SoloState::Active(current) | SoloState::Expired(current) => current,
        other => return Ok(other),
    };
    if !current.same_identity(reservation) {
        // Replaced since it was read: the next pass judges the new one.
        return Ok(read(state_root, &repo, now));
    }
    let path = reservation_path(state_root, &repo)?;
    match decision {
        Reconcile::Keep => {}
        Reconcile::Latch => {
            let mut latched = current;
            latched.seen_in_queue = true;
            write_record(&path, &latched)?;
            audit(
                state_root,
                "seen_in_queue",
                &repo,
                serde_json::json!({"pr": latched.pr, "head": latched.head}),
            )?;
        }
        Reconcile::Clear(why) => {
            fs::remove_file(&path)
                .or_else(|error| {
                    if error.kind() == io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })
                .map_err(|error| format!("cannot remove {}: {error}", path.display()))?;
            audit(
                state_root,
                "ended",
                &repo,
                serde_json::json!({"pr": current.pr, "head": current.head, "why": why}),
            )?;
        }
    }
    Ok(read(state_root, &repo, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const REPO: &str = "Generous-Corp/vellum";

    fn at(minute: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-14T10:00:00Z")
            .expect("time")
            .with_timezone(&Utc)
            + Duration::minutes(minute)
    }

    fn reservation(pr: u64) -> Reservation {
        Reservation::new(
            REPO,
            "main",
            pr,
            HEAD,
            SoloReason::Release,
            "release coalesce must land alone",
            "verdict comment 1 by shipyard-local[bot]",
            DEFAULT_TTL_MINUTES,
            at(0),
        )
        .expect("reservation")
    }

    fn live(state: &str, head: &str, in_queue: bool) -> LiveReserved {
        LiveReserved {
            state: state.to_owned(),
            head: head.to_owned(),
            in_queue,
        }
    }

    #[test]
    fn an_active_reservation_holds_every_other_pull_request_and_not_its_own() {
        let state = SoloState::Active(reservation(451));
        assert_eq!(
            state.admission_hold("main", 418).as_deref(),
            Some("held_solo_reservation:#451")
        );
        assert_eq!(state.admission_hold("main", 451), None);
        assert_eq!(state.admission_hold("release", 418), None);
        assert_eq!(
            SoloState::Expired(reservation(451)).admission_hold("main", 418),
            None
        );
        assert_eq!(SoloState::Absent.admission_hold("main", 418), None);
    }

    #[test]
    fn an_unreadable_reservation_holds_every_pull_request_including_the_named_one() {
        let temp = tempfile::tempdir().expect("temp");
        let path = reservation_path(temp.path(), REPO).expect("path");
        ensure_private_dir(path.parent().expect("dir")).expect("dir");
        fs::write(&path, "{not json").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
        }
        let state = read(temp.path(), REPO, at(1));
        assert!(matches!(state, SoloState::Unreadable { .. }), "{state:?}");
        assert_eq!(
            state.admission_hold("main", 451).as_deref(),
            Some("held_solo_reservation_unreadable")
        );
        assert_eq!(
            state.admission_hold("main", 418).as_deref(),
            Some("held_solo_reservation_unreadable")
        );
        // A new reservation cannot paper over it; only a release removes it.
        assert!(reserve(temp.path(), &reservation(451)).is_err());
        assert!(release(temp.path(), REPO, "operator removed a corrupt file").expect("release"));
        assert_eq!(read(temp.path(), REPO, at(1)), SoloState::Absent);
    }

    #[cfg(unix)]
    #[test]
    fn a_reservation_others_can_write_or_a_link_is_unreadable() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().expect("temp");
        let path = reserve(temp.path(), &reservation(451)).expect("reserve");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("mode");
        assert!(matches!(
            read(temp.path(), REPO, at(1)),
            SoloState::Unreadable { .. }
        ));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
        let real = temp.path().join("elsewhere.json");
        fs::rename(&path, &real).expect("move");
        std::os::unix::fs::symlink(&real, &path).expect("link");
        assert!(matches!(
            read(temp.path(), REPO, at(1)),
            SoloState::Unreadable { .. }
        ));
    }

    #[test]
    fn a_reservation_round_trips_and_is_private() {
        let temp = tempfile::tempdir().expect("temp");
        let path = reserve(temp.path(), &reservation(451)).expect("reserve");
        assert_eq!(
            read(temp.path(), "generous-corp/VELLUM", at(1)),
            SoloState::Active(reservation(451))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Another pull request cannot take the queue while this one holds it.
        assert!(reserve(temp.path(), &reservation(452)).is_err());
        // The same one may refresh it.
        assert!(reserve(temp.path(), &reservation(451)).is_ok());
    }

    #[test]
    fn a_reservation_expires_after_its_ttl_and_the_steward_ends_it() {
        let temp = tempfile::tempdir().expect("temp");
        reserve(temp.path(), &reservation(451)).expect("reserve");
        let reserved = reservation(451);
        let before = at(DEFAULT_TTL_MINUTES - 1);
        let after = at(DEFAULT_TTL_MINUTES);
        assert!(matches!(
            read(temp.path(), REPO, before),
            SoloState::Active(_)
        ));
        let expired = read(temp.path(), REPO, after);
        assert!(matches!(expired, SoloState::Expired(_)));
        assert_eq!(expired.admission_hold("main", 418), None);
        let open = live("OPEN", HEAD, true);
        assert_eq!(
            reconcile_decision(&reserved, Some(&open), after),
            Reconcile::Clear("expired")
        );
        // Expiry needs no read of GitHub.
        assert_eq!(
            reconcile_decision(&reserved, None, after),
            Reconcile::Clear("expired")
        );
        let state = apply_decision(temp.path(), &reserved, &Reconcile::Clear("expired"), after)
            .expect("apply");
        assert_eq!(state, SoloState::Absent);
    }

    #[test]
    fn a_reservation_ends_when_its_pull_request_merges_closes_or_moves() {
        let reserved = reservation(451);
        let now = at(5);
        assert_eq!(
            reconcile_decision(&reserved, Some(&live("MERGED", HEAD, false)), now),
            Reconcile::Clear("merged")
        );
        assert_eq!(
            reconcile_decision(&reserved, Some(&live("CLOSED", HEAD, false)), now),
            Reconcile::Clear("closed")
        );
        assert_eq!(
            reconcile_decision(&reserved, Some(&live("OPEN", &"b".repeat(40), false)), now),
            Reconcile::Clear("head_moved")
        );
        // An unread pull request keeps it: absence is not an ending.
        assert_eq!(reconcile_decision(&reserved, None, now), Reconcile::Keep);
    }

    #[test]
    fn a_reservation_ends_when_its_pull_request_leaves_the_queue_only_after_joining_it() {
        let temp = tempfile::tempdir().expect("temp");
        let mut reserved = reservation(451);
        reserve(temp.path(), &reserved).expect("reserve");
        let now = at(5);
        // Not queued yet: the steward is still to enqueue it.
        assert_eq!(
            reconcile_decision(&reserved, Some(&live("OPEN", HEAD, false)), now),
            Reconcile::Keep
        );
        assert_eq!(
            reconcile_decision(&reserved, Some(&live("OPEN", HEAD, true)), now),
            Reconcile::Latch
        );
        let state = apply_decision(temp.path(), &reserved, &Reconcile::Latch, now).expect("latch");
        let SoloState::Active(latched) = state else {
            panic!("latched reservation stays active: {state:?}");
        };
        assert!(latched.seen_in_queue);
        reserved = latched;
        assert_eq!(
            reconcile_decision(&reserved, Some(&live("OPEN", HEAD, false)), now),
            Reconcile::Clear("left_queue")
        );
        let state = apply_decision(temp.path(), &reserved, &Reconcile::Clear("left_queue"), now)
            .expect("clear");
        assert_eq!(state, SoloState::Absent);
        let audit = fs::read_to_string(temp.path().join(SOLO_AUDIT_FILE)).expect("audit");
        assert!(audit.contains("\"reserved\""), "{audit}");
        assert!(audit.contains("\"seen_in_queue\""), "{audit}");
        assert!(audit.contains("left_queue"), "{audit}");
    }

    #[test]
    fn a_decision_never_ends_a_reservation_that_replaced_the_one_it_judged() {
        let temp = tempfile::tempdir().expect("temp");
        let old = reservation(451);
        reserve(temp.path(), &old).expect("reserve");
        let mut fresh = reservation(451);
        fresh.created_at = at(2);
        fresh.expires_at = at(2 + DEFAULT_TTL_MINUTES);
        reserve(temp.path(), &fresh).expect("refresh");
        let state =
            apply_decision(temp.path(), &old, &Reconcile::Clear("merged"), at(3)).expect("apply");
        assert_eq!(state, SoloState::Active(fresh));
    }

    #[test]
    fn a_reservation_is_bounded_and_named() {
        assert!(SoloReason::parse("reorder").is_err());
        assert_eq!(SoloReason::parse("release"), Ok(SoloReason::Release));
        assert!(
            Reservation::new(
                REPO,
                "main",
                1,
                HEAD,
                SoloReason::Release,
                "n",
                "a",
                MAX_TTL_MINUTES + 1,
                at(0)
            )
            .is_err()
        );
        assert!(
            Reservation::new(
                REPO,
                "main",
                1,
                "abc",
                SoloReason::Release,
                "n",
                "a",
                10,
                at(0)
            )
            .is_err()
        );
        assert!(reservation_path(Path::new("/s"), "../x").is_err());
        assert!(reservation_path(Path::new("/s"), "a/b/c").is_err());
    }

    /// The `ghapp` removal guard finds the reservation without asking
    /// Shipyard, so its default directory must be this one.
    #[test]
    fn the_removal_guard_reads_reservations_where_shipyard_writes_them() {
        use crate::identity::RuntimeMode;
        use crate::paths::RuntimePaths;
        use crate::platform::Platform;
        let guard = include_str!("../scripts/ghapp_queue_removal_guard.py");
        let home = Path::new("/h");
        let mac = RuntimePaths::for_platform(Platform::MacOs, home, RuntimeMode::Shipyard);
        let linux = RuntimePaths::for_platform(Platform::Linux, home, RuntimeMode::Shipyard);
        assert_eq!(
            mac.state_dir,
            Path::new("/h/Library/Application Support/shipyard")
        );
        assert_eq!(linux.state_dir, Path::new("/h/.local/state/shipyard"));
        assert!(guard.contains(r#"SOLO_STATE_STEM = "shipyard""#));
        let (first, second) = SOLO_DIR.split_once('/').expect("two parts");
        assert!(guard.contains(&format!(r#"SOLO_SUBDIR = ("{first}", "{second}")"#)));
        assert!(guard.contains(&format!(r#"SOLO_KIND = "{KIND}""#)));
        assert!(guard.contains(&format!("SOLO_SCHEMA = {SCHEMA}")));
    }

    #[test]
    fn only_admission_actions_are_paused() {
        assert!(is_admission_mutation(
            crate::merge_queue_control::ARM_ACTION
        ));
        assert!(is_admission_mutation("runner steward enqueue pull request"));
        assert!(!is_admission_mutation(
            "runner steward dequeue a merge group that can never finish"
        ));
        assert!(!is_admission_mutation("runner steward rerun failed run 1"));
    }
}
