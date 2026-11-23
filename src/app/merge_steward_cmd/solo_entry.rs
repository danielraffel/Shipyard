//! Solo-entry admission: land one named branch's pull request with nothing
//! ahead of it in the merge queue.
//!
//! A repository whose release tag gate tags only a commit whose tail is all
//! evidence (Vellum's observatory reconcile) needs that reconcile to merge
//! alone: GitHub's merge queue batches up to five entries, and an entry
//! enqueued earlier always merges first. So, per repository, the steward host's
//! machine-global config can name *solo branches*:
//!
//! ```toml
//! [merge_steward.solo_entry."Generous-Corp/vellum"]
//! branches = ["automation/pulp-observatory-coalesced"]
//! version_file = "CMakeLists.txt"   # where the release request's version lives
//! tag_prefix = "v"                  # default
//! window_minutes = 120              # default
//! ```
//!
//! The mode acts only inside a *release window*: the base's version (the
//! `project(... VERSION x.y.z)` of a `CMakeLists.txt`, otherwise the first
//! `x.y.z` in the file) is newer than every other `<prefix>x.y.z` tag, and its
//! own tag is missing or its GitHub release is missing or a draft. An open
//! window holds nothing by itself.
//!
//! A *hold* engages on the first pass that sees an admission-ready solo pull
//! request in the window: open, not a draft, authored by the fleet App from a
//! branch of this repository, and either queued or one the steward would
//! enqueue with an approved exact head. The hold is for that one pull request.
//! Once engaged it lasts while that pull request is open, ready or not (the
//! reconcile is rebuilt on the new base tip and reviewed again), and it ends
//! when:
//!
//! * that pull request merges or closes, read from GitHub (a census that
//!   misses it for one pass ends nothing). The next reconcile, even when it
//!   is already open, starts its own attempt;
//! * the hold has run `window_minutes` (the clock starts when the hold
//!   engages, never when the window opens). Past that with its entry still
//!   in the merge queue the hold stays, so the backlog is not admitted into
//!   its push, until the queue merges or removes it and for at most one more
//!   `window_minutes`;
//! * the solo head cannot be enqueued: it left the queue unmerged, the gate
//!   admitted it on [`MAX_REFUSED_PASSES`] passes and the enqueue path
//!   refused it by name (a central HOLD or a failed read is not a refusal),
//!   or the steward removed its own entry [`MAX_OWN_DEQUEUES`] times;
//! * the release state could not be read for more than
//!   [`MAX_READ_FAILURES`] passes in a row (fail open). Up to that many
//!   failed reads keep the last known window, so one 5xx never drops a hold.
//!
//! A hold that ended on a head does not engage again for that head. A new
//! head, or another solo pull request, starts the next attempt, at most
//! [`MAX_ATTEMPTS`] per version; an attempt that ends in a merge counts, so
//! the attempts go to the first reconciles after a version bump. Every
//! engage, release and expiry is in the repository report's `solo_entry` line.
//!
//! While a hold is engaged:
//!
//! * nothing else is admitted for the repository: the steward's own enqueue
//!   (every caller of [`super::pr_mutations::mutate_pr_with_recovery`]) and the
//!   native-arm backstop both hold. The one exemption is the pull request the
//!   base-poison path fast-tracks as the fix for a confirmed red base
//!   ([`poison_fix_exempt`]): without it a red base and a solo hold would
//!   block each other;
//! * auto-merge requests the steward's own backstop armed (recorded in the
//!   ledger when it arms) are disabled, so GitHub cannot enqueue them behind
//!   the solo entry; the backstop arms them again once the hold ends;
//! * the solo pull request is enqueued only when the base's merge queue is
//!   empty and its head contains the base tip (`behind_by == 0`), both read
//!   again under the mutation guard just before the enqueue. A stale head is
//!   never enqueued and never pushed to: its owner rebuilds it;
//! * right after the enqueue, and on every pass while it is queued, the queue
//!   is read again; when any entry is ahead of it, the steward dequeues only
//!   its own solo entry ([`super::unapproved_admission::dequeue_under_guard`],
//!   so the removal never counts as an ejection) and retries next pass. A
//!   queued solo entry whose head is behind the base (another path enqueued
//!   it) is removed the same way. An
//!   entry behind it is reported as `solo_entry_shared_batch_risk` and never
//!   touched: the steward never dequeues another pull request.
//!
//! Every mutation goes through the merge-queue mutation guard, so the central
//! HOLD, the mutation machine and the pass budget all apply.
//! `runner steward-admit-ready` never admits a solo pull request (only the
//! tick runs the checks above), and skips the whole repository while a hold
//! is engaged.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

use super::{
    GitHubActions, MutationApplyContext, MutationControl, ObservedPr, RepoObservation,
    StewardDecision, StewardLedger, StewardPolicy, acquire_pr_mutation_guard, attempts_for,
    classify_pr, merge_queue_snapshot,
};

/// The machine-global table, keyed by `owner/name`.
pub(super) const CONFIG_KEY: &str = "merge_steward.solo_entry";
/// How long one hold lasts, from the pass that engaged it.
pub(super) const DEFAULT_WINDOW_MINUTES: i64 = 120;
/// Hold attempts per release version.
pub(super) const MAX_ATTEMPTS: u32 = 3;
/// Failed release reads in a row that keep the last known window.
pub(super) const MAX_READ_FAILURES: u32 = 3;
/// Passes on which the gate admitted the solo head and it was not enqueued
/// before the hold lets go of that head.
pub(super) const MAX_REFUSED_PASSES: u32 = 3;
/// The steward's own removals of its solo entry within one attempt.
pub(super) const MAX_OWN_DEQUEUES: u32 = 3;
/// How far one pass moves the deadline of a hold whose entry is still in the
/// merge queue past its window (two ticks).
const QUEUED_EXTENSION_MINUTES: i64 = 10;
/// Audit action recorded when the steward disables an auto-merge request its
/// own backstop armed.
const DISARM_AUDIT: &str = "solo_entry_disarmed_own_arm";
/// The tag prefix before `x.y.z`.
pub(super) const DEFAULT_TAG_PREFIX: &str = "v";
/// Audit action recorded with the steward's own removal of its solo entry.
const DEQUEUE_AUDIT: &str = "solo_entry_dequeued_behind";

/// One repository's solo-entry setting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepoSetting {
    /// Head branches whose pull request must merge alone.
    pub(crate) branches: Vec<String>,
    /// Base-branch file holding the release request's version.
    pub(crate) version_file: String,
    /// Prefix of a release tag (`v` for `v1.2.3`).
    pub(crate) tag_prefix: String,
    /// How long one hold lasts once it engages.
    pub(crate) window_minutes: i64,
}

/// `[merge_steward.solo_entry]`, read once per steward pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Settings {
    /// By lowercase `owner/name`: the setting, or why it is malformed.
    repos: BTreeMap<String, Result<RepoSetting, String>>,
    /// The whole table is malformed (or the config could not be read).
    error: Option<String>,
}

impl Settings {
    /// The settings in the machine-global config at `global_dir`. Never
    /// fails: an unreadable config is an error every configured pass reports,
    /// and admission then runs as without the setting.
    pub(super) fn from_machine_global(global_dir: &Path) -> Self {
        match crate::config::LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf()) {
            Ok(config) => Self::from_value(config.get(CONFIG_KEY)),
            Err(error) => Self {
                repos: BTreeMap::new(),
                error: Some(format!("{CONFIG_KEY} unreadable: {error}")),
            },
        }
    }

    /// The settings `value` (the `merge_steward.solo_entry` table) names.
    pub(super) fn from_value(value: Option<&toml::Value>) -> Self {
        let Some(value) = value else {
            return Self::default();
        };
        let Some(table) = value.as_table() else {
            return Self {
                repos: BTreeMap::new(),
                error: Some(format!("{CONFIG_KEY} must be a table keyed by owner/name")),
            };
        };
        let repos = table
            .iter()
            .map(|(repo, value)| (repo.to_ascii_lowercase(), parse_repo(repo, value)))
            .collect();
        Self { repos, error: None }
    }

    /// Whether `repo` has a well-formed setting.
    pub(super) fn configured(&self, repo: &str) -> bool {
        matches!(self.for_repo(repo), Some(Ok(_)))
    }

    /// Whether `pr` can be `repo`'s solo pull request ([`candidate`]) under a
    /// well-formed setting. `runner steward-admit-ready` leaves such a pull
    /// request to the tick, which alone runs the solo checks.
    pub(super) fn solo_candidate(&self, repo: &str, pr: &ObservedPr) -> bool {
        matches!(self.for_repo(repo), Some(Ok(setting)) if candidate(&setting.branches, pr))
    }

    /// `repo`'s setting: `None` when it has none (today's behaviour).
    fn for_repo(&self, repo: &str) -> Option<Result<&RepoSetting, String>> {
        if let Some(error) = &self.error {
            return Some(Err(error.clone()));
        }
        self.repos
            .get(&repo.to_ascii_lowercase())
            .map(|setting| setting.as_ref().map_err(Clone::clone))
    }
}

fn parse_repo(repo: &str, value: &toml::Value) -> Result<RepoSetting, String> {
    let at = format!("{CONFIG_KEY}.\"{repo}\"");
    let table = value
        .as_table()
        .ok_or_else(|| format!("{at} must be a table"))?;
    let branches = table
        .get("branches")
        .and_then(toml::Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::trim)
                        .filter(|branch| !branch.is_empty())
                        .map(str::to_owned)
                })
                .collect::<Option<Vec<_>>>()
        })
        .filter(|branches| !branches.is_empty())
        .ok_or_else(|| format!("{at}.branches must be a non-empty list of branch names"))?;
    let version_file = table
        .get("version_file")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|file| !file.is_empty() && !file.starts_with('/') && !file.contains(".."))
        .ok_or_else(|| format!("{at}.version_file must name a file in the repository"))?
        .to_owned();
    let tag_prefix = match table.get("tag_prefix") {
        None => DEFAULT_TAG_PREFIX.to_owned(),
        Some(value) => value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("{at}.tag_prefix must be a string"))?,
    };
    let window_minutes = match table.get("window_minutes") {
        None => DEFAULT_WINDOW_MINUTES,
        Some(value) => value
            .as_integer()
            .filter(|minutes| (1..=24 * 60).contains(minutes))
            .ok_or_else(|| format!("{at}.window_minutes must be 1 to 1440"))?,
    };
    Ok(RepoSetting {
        branches,
        version_file,
        tag_prefix,
        window_minutes,
    })
}

/// One repository's release window, kept in the steward ledger.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Window {
    /// The version whose release the window waits for.
    pub(super) version: String,
    /// When a pass first saw the release request.
    pub(super) opened_at: DateTime<Utc>,
    /// The hold engaged now, when one is.
    #[serde(default)]
    pub(super) attempt: Option<Attempt>,
    /// Holds engaged for `version` so far, at most [`MAX_ATTEMPTS`].
    #[serde(default)]
    pub(super) attempts: u32,
    /// `<pr>@<head>` of every head a hold let go of. Only a new head, or
    /// another pull request, engages a hold again.
    #[serde(default)]
    pub(super) released: BTreeSet<String>,
    /// Release reads that failed in a row.
    #[serde(default)]
    pub(super) read_failures: u32,
}

/// One engaged hold.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Attempt {
    /// The solo pull request the hold is for.
    pub(super) solo: u64,
    /// Its head when a pass last saw it.
    pub(super) head: String,
    /// When the hold engaged.
    pub(super) engaged_at: DateTime<Utc>,
    /// When the hold lets go whatever happened.
    pub(super) deadline: DateTime<Utc>,
    /// Passes on which the gate admitted `head` and it was not enqueued.
    #[serde(default)]
    pub(super) refused: u32,
    /// Whether a pass saw `head` in the merge queue.
    #[serde(default)]
    pub(super) queued: bool,
    /// The steward's own removals of the solo entry in this attempt.
    #[serde(default)]
    pub(super) own_dequeues: u32,
}

impl Window {
    /// A window for `version`, first seen at `now`, holding nothing.
    pub(super) fn new(version: &str, now: DateTime<Utc>) -> Self {
        Self {
            version: version.to_owned(),
            opened_at: now,
            attempt: None,
            attempts: 0,
            released: BTreeSet::new(),
            read_failures: 0,
        }
    }

    /// Whether a hold already let go of `pr` at `head`.
    pub(super) fn is_released(&self, pr: u64, head: &str) -> bool {
        self.released.contains(&released_key(pr, head))
    }
}

fn released_key(pr: u64, head: &str) -> String {
    format!("{pr}@{head}")
}

/// Whether `attempt`'s hold is over at `now`: past its deadline, or the
/// clock reads earlier than the moment it engaged (a clock that moved back
/// must never extend a hold).
pub(super) fn attempt_expired(attempt: &Attempt, now: DateTime<Utc>) -> bool {
    now >= attempt.deadline || now < attempt.engaged_at
}

/// The ledger key of `repo`'s window on `base`.
pub(super) fn window_key(repo: &str, base: &str) -> String {
    format!("{}@{base}", repo.to_ascii_lowercase())
}

/// Whether a solo-entry hold is engaged for `repo` on `base` at `now`, as
/// the last tick recorded it. `runner steward-admit-ready` leaves such a
/// repository to the tick; an open window with no hold does not stop it.
pub(super) fn hold_engaged(
    ledger: &StewardLedger,
    repo: &str,
    base: &str,
    now: DateTime<Utc>,
) -> bool {
    ledger
        .solo_entry_windows
        .get(&window_key(repo, base))
        .and_then(|window| window.attempt.as_ref())
        .is_some_and(|attempt| !attempt_expired(attempt, now))
}

/// `x.y.z` from `text`, as numbers.
fn semver(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// The release version `content` (the file `file`) names: a
/// `CMakeLists.txt`'s `project(... VERSION x.y.z ...)`, otherwise the first
/// `x.y.z` in it.
pub(super) fn file_version(file: &str, content: &str) -> Option<(u64, u64, u64)> {
    let pattern = if file.rsplit('/').next() == Some("CMakeLists.txt") {
        r"(?is)\bproject\s*\([^)]*?\bVERSION\s+([0-9]+\.[0-9]+\.[0-9]+)\b"
    } else {
        r"\b([0-9]+\.[0-9]+\.[0-9]+)\b"
    };
    let captured = regex::Regex::new(pattern)
        .ok()?
        .captures(content)?
        .get(1)?
        .as_str()
        .to_owned();
    semver(&captured)
}

/// Where `version` stands against `tags` (full refs or tag names).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TagState {
    /// A `<prefix>x.y.z` tag at or above it exists besides its own, or it is
    /// older than a tag: no release request.
    NotPending,
    /// Newer than every other tag and not tagged itself.
    Untagged,
    /// Newer than every other tag and tagged: pending until its release is
    /// published.
    Tagged,
}

/// `version` against the `<prefix>x.y.z` tags in `tags`.
pub(super) fn tag_state(version: (u64, u64, u64), tags: &[String], prefix: &str) -> TagState {
    let tagged = tags
        .iter()
        .filter_map(|tag| {
            let name = tag.strip_prefix("refs/tags/").unwrap_or(tag);
            semver(name.strip_prefix(prefix)?)
        })
        .collect::<Vec<_>>();
    if tagged.iter().any(|tag| *tag > version) {
        TagState::NotPending
    } else if tagged.contains(&version) {
        TagState::Tagged
    } else {
        TagState::Untagged
    }
}

/// Whether the release a `releases/tags/<tag>` read describes is published.
/// `read` is the release's `.draft` (`true` or `false`), or `gh`'s error. A
/// 404 is a release that does not exist yet; any other error is a failed
/// read, never "unpublished".
pub(super) fn release_published(tag: &str, read: Result<&str, &str>) -> Result<bool, String> {
    match read.map(str::trim) {
        Ok("false") => Ok(true),
        Ok("true") => Ok(false),
        Ok(other) => Err(format!("release {tag} unreadable: draft is {other:?}")),
        Err(error) if crate::repo_variable::is_not_found(error) => Ok(false),
        Err(error) => Err(format!("release {tag} unreadable: {error}")),
    }
}

/// The version a pending release request names on `base`, or `None`.
fn read_pending_release(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    setting: &RepoSetting,
) -> Result<Option<String>, String> {
    let content = actions
        .run_gh(&[
            "api".to_owned(),
            "-H".to_owned(),
            "Accept: application/vnd.github.raw+json".to_owned(),
            format!("repos/{repo}/contents/{}?ref={base}", setting.version_file),
        ])
        .map_err(|error| format!("{} on {base} unreadable: {error}", setting.version_file))?;
    let version = file_version(&setting.version_file, &content)
        .ok_or_else(|| format!("{} on {base} names no x.y.z version", setting.version_file))?;
    let tags = actions
        .run_gh(&[
            "api".to_owned(),
            "--paginate".to_owned(),
            format!("repos/{repo}/git/matching-refs/tags/{}", setting.tag_prefix),
            "--jq".to_owned(),
            ".[].ref".to_owned(),
        ])
        .map_err(|error| format!("tags unreadable: {error}"))?;
    let tags = tags
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let (major, minor, patch) = version;
    let tag = format!("{}{major}.{minor}.{patch}", setting.tag_prefix);
    match tag_state(version, &tags, &setting.tag_prefix) {
        TagState::NotPending => Ok(None),
        TagState::Untagged => Ok(Some(tag)),
        TagState::Tagged => {
            let read = actions
                .run_gh(&[
                    "api".to_owned(),
                    format!("repos/{repo}/releases/tags/{tag}"),
                    "--jq".to_owned(),
                    ".draft".to_owned(),
                ])
                .map_err(|error| error.to_string());
            let published = release_published(&tag, read.as_deref().map_err(String::as_str))?;
            Ok((!published).then_some(tag))
        }
    }
}

/// Whether `head` contains the tip of `base` (`behind_by == 0`).
fn head_up_to_date(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    head: &str,
) -> Result<bool, String> {
    let value = actions
        .run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/compare/{base}...{head}"),
            "--jq".to_owned(),
            ".behind_by".to_owned(),
        ])
        .map_err(|error| format!("solo head compare unreadable: {error}"))?;
    behind_by_zero(&value)
}

/// Whether a compare read's `.behind_by` says the head is up to date.
/// Anything but a number is a failed read.
pub(super) fn behind_by_zero(value: &str) -> Result<bool, String> {
    value
        .trim()
        .parse::<u64>()
        .map(|behind| behind == 0)
        .map_err(|error| format!("solo head compare unreadable: behind_by {value:?}: {error}"))
}

/// Where the solo pull request stands against the base's merge queue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Phase {
    /// The hold is engaged but the solo pull request is not admission-ready
    /// (rebuilt and not approved yet, red, or held).
    NotReady,
    /// Not queued; this many entries are in the queue, so it waits.
    Waiting { ahead: usize },
    /// Not queued, the queue is empty and its head contains the base tip:
    /// only it may be enqueued.
    Admit,
    /// The queue is empty, but the solo head is behind the base tip.
    Stale,
    /// Queued with nothing ahead of it.
    Queued,
    /// Queued behind `ahead` entries: the steward removes its own entry.
    Behind { ahead: usize },
    /// The queue could not be read: nothing is admitted this pass.
    QueueUnreadable,
    /// The solo head could not be compared with the base: nothing is
    /// admitted this pass.
    BaseUnreadable,
}

/// The repository's solo-entry gate for one pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) enum Gate {
    /// No setting for the repository: admission exactly as without the mode.
    #[default]
    Off,
    /// Configured, but nothing is held this pass; the line says why.
    Inactive { line: String },
    /// An engaged hold: the solo pull request holds every other admission.
    Hold {
        solo: u64,
        phase: Phase,
        line: String,
    },
}

impl Gate {
    /// The repository report's one reason line.
    pub(super) fn line(&self) -> Option<String> {
        match self {
            Self::Off => None,
            Self::Inactive { line } | Self::Hold { line, .. } => Some(line.clone()),
        }
    }

    /// The token that holds `pr`'s enqueue this pass, or `None` when it may
    /// be admitted as usual.
    pub(super) fn enqueue_hold(&self, pr: u64) -> Option<String> {
        let Self::Hold { solo, phase, .. } = self else {
            return None;
        };
        if pr != *solo {
            return Some(match phase {
                Phase::NotReady => format!("held_for_solo_attempt:#{solo}"),
                _ => format!("held_solo_entry:#{solo}"),
            });
        }
        match phase {
            Phase::Admit => None,
            Phase::NotReady => Some("solo_entry_waiting:not_ready".to_owned()),
            Phase::Stale => Some("solo_entry_waiting:behind_base".to_owned()),
            Phase::Waiting { ahead } | Phase::Behind { ahead } => {
                Some(format!("solo_entry_waiting:{ahead}_ahead"))
            }
            Phase::Queued => Some("solo_entry_queued".to_owned()),
            Phase::QueueUnreadable => Some("solo_entry_queue_unreadable".to_owned()),
            Phase::BaseUnreadable => Some("solo_entry_base_unreadable".to_owned()),
        }
    }

    /// Whether the native-arm backstop may run for the repository. Arming
    /// hands the enqueue to GitHub at a time nothing controls, so it never
    /// runs while a solo pull request holds admission.
    pub(super) fn backstop_allowed(&self) -> bool {
        !matches!(self, Self::Hold { .. })
    }

    /// The solo pull request this gate admits now, when it admits one.
    fn admits(&self, pr: u64) -> bool {
        matches!(self, Self::Hold { solo, phase: Phase::Admit, .. } if *solo == pr)
    }
}

/// Whether `pr` is the pull request the base-poison path fast-tracks as the
/// fix for a confirmed red base. Such a fix passes the solo hold: the solo
/// pull request cannot merge onto a red base, and without this the hold and
/// the base-poison hold would block each other until the hold expired.
pub(super) fn poison_fix_exempt(poison: Option<&super::base_poison::RepoPoison>, pr: u64) -> bool {
    matches!(
        super::base_poison::admission(poison, pr),
        super::base_poison::Admission::FastTrack { .. }
    )
}

/// The token that holds `pr`'s enqueue under `gate`, or `None`. The solo
/// pull request's own wait is never exempt; any other row is exempt only as
/// the base fix ([`poison_fix_exempt`]).
pub(super) fn admission_hold(
    gate: Option<&Gate>,
    poison: Option<&super::base_poison::RepoPoison>,
    pr: u64,
) -> Option<String> {
    let gate = gate?;
    let token = gate.enqueue_hold(pr)?;
    let own = matches!(gate, Gate::Hold { solo, .. } if *solo == pr);
    (own || !poison_fix_exempt(poison, pr)).then_some(token)
}

/// Whether `mutation` (comma-separated tokens) says the pull request was
/// enqueued.
pub(super) fn was_enqueued(mutation: &str) -> bool {
    mutation
        .split(',')
        .any(|part| matches!(part, "enqueued" | "enqueued_priority_restored"))
}

/// Whether `pr` can be a solo pull request at all: on a solo branch, open
/// (it is in the census), not a draft, and authored by the fleet App. A
/// branch name alone is not an identity: anyone can push a branch of that
/// name, from a fork too.
pub(super) fn candidate(branches: &[String], pr: &ObservedPr) -> bool {
    branches.contains(&pr.fact.head_branch)
        && !pr.fact.draft
        && crate::merge_steward::is_implicitly_managed(&pr.fact)
}

/// Whether candidate `pr` is admission-ready: queued, or one the steward
/// would enqueue with an approved exact head.
pub(super) fn ready(
    pr: &ObservedPr,
    decision: &StewardDecision,
    approved: &dyn Fn(&ObservedPr) -> bool,
) -> bool {
    match decision {
        StewardDecision::Queued { .. } => true,
        StewardDecision::ArmMergeQueue => approved(pr),
        _ => false,
    }
}

/// The phase of solo pull request `solo` against `queue` (pull request to
/// position, all from one snapshot). "Ahead" is a strictly lower position.
/// An empty queue is [`Phase::Admit`] only once the caller has also read the
/// head as up to date.
pub(super) fn phase(solo: u64, queue: Result<&BTreeMap<u64, u64>, &String>) -> Phase {
    let Ok(positions) = queue else {
        return Phase::QueueUnreadable;
    };
    match positions.get(&solo) {
        Some(mine) => match positions.values().filter(|other| *other < mine).count() {
            0 => Phase::Queued,
            ahead => Phase::Behind { ahead },
        },
        None if positions.is_empty() => Phase::Admit,
        None => Phase::Waiting {
            ahead: positions.len(),
        },
    }
}

fn queue_positions(
    actions: &GitHubActions,
    observation: &RepoObservation,
) -> Result<BTreeMap<u64, u64>, String> {
    match merge_queue_snapshot(actions, &observation.repo, &observation.base)? {
        (true, positions, _, _) => Ok(positions),
        (false, ..) => Err(format!("{} reports no merge queue", observation.base)),
    }
}

/// What [`observe`] reads from, and does to, GitHub.
pub(super) trait Reads {
    /// The version a pending release request names, or `None`.
    fn pending_release(&self, setting: &RepoSetting) -> Result<Option<String>, String>;
    /// Whether `pr`'s exact head is approved by the steward's admission gate.
    fn approved(&self, pr: &ObservedPr, ledger: &StewardLedger) -> bool;
    /// Whether `pr`'s head branch lives in this repository (not a fork).
    fn same_repo(&self, pr: &ObservedPr) -> Result<bool, String>;
    /// The base's merge queue: pull request to position.
    fn queue(&self) -> Result<BTreeMap<u64, u64>, String>;
    /// Whether `pr`'s head contains the current base tip.
    fn up_to_date(&self, pr: &ObservedPr) -> Result<bool, String>;
    /// The state of pull request `number`, read from GitHub: `open`,
    /// `closed` or `merged`.
    fn pr_state(&self, number: u64) -> Result<String, String>;
    /// Remove `pr`'s own queue entry under the mutation guard.
    fn dequeue(&self, pr: &ObservedPr, ledger: &mut StewardLedger) -> Result<(), String>;
    /// Disable the auto-merge request the steward armed on `pr`, under the
    /// mutation guard.
    fn disarm(&self, pr: &ObservedPr, ledger: &mut StewardLedger) -> Result<(), String>;
}

/// [`Reads`] against GitHub.
pub(super) struct GitHubReads<'a> {
    pub(super) actions: &'a GitHubActions,
    pub(super) observation: &'a RepoObservation,
    pub(super) control: Option<&'a MutationControl>,
    pub(super) ledger_path: &'a Path,
}

impl GitHubReads<'_> {
    fn context(&self) -> Result<MutationApplyContext<'_>, String> {
        let control = self
            .control
            .ok_or_else(|| "a solo-entry mutation needs mutation control".to_owned())?;
        Ok(MutationApplyContext {
            actions: self.actions,
            observation: self.observation,
            ledger_path: self.ledger_path,
            mutation_control: control,
            approval_blockers: std::cell::RefCell::default(),
            carried_commits: std::cell::RefCell::default(),
            changed_files: std::cell::RefCell::default(),
            ejection_guards: crate::merge_steward::EjectionGuards::default(),
            base_poison: None,
            defect_fix: None,
            solo_entry: None,
        })
    }
}

impl Reads for GitHubReads<'_> {
    fn pending_release(&self, setting: &RepoSetting) -> Result<Option<String>, String> {
        read_pending_release(
            self.actions,
            &self.observation.repo,
            &self.observation.base,
            setting,
        )
    }

    fn approved(&self, pr: &ObservedPr, ledger: &StewardLedger) -> bool {
        super::pr_mutations::read_head_gate_for_report(self.actions, self.observation, pr, ledger)
            .is_ok_and(|(gate, _, _)| gate.allows())
    }

    fn same_repo(&self, pr: &ObservedPr) -> Result<bool, String> {
        let repo = &self.observation.repo;
        self.actions
            .run_gh(&[
                "api".to_owned(),
                format!("repos/{repo}/pulls/{}", pr.fact.number),
                "--jq".to_owned(),
                ".head.repo.full_name".to_owned(),
            ])
            .map(|head_repo| head_repo.trim().eq_ignore_ascii_case(repo))
            .map_err(|error| format!("head repository unreadable: {error}"))
    }

    fn queue(&self) -> Result<BTreeMap<u64, u64>, String> {
        queue_positions(self.actions, self.observation)
    }

    fn up_to_date(&self, pr: &ObservedPr) -> Result<bool, String> {
        head_up_to_date(
            self.actions,
            &self.observation.repo,
            &self.observation.base,
            &pr.fact.head_sha,
        )
    }

    fn pr_state(&self, number: u64) -> Result<String, String> {
        self.actions
            .run_gh(&[
                "api".to_owned(),
                format!("repos/{}/pulls/{number}", self.observation.repo),
                "--jq".to_owned(),
                r#"if .merged then "merged" else .state end"#.to_owned(),
            ])
            .map(|state| state.trim().to_owned())
            .map_err(|error| error.to_string())
            .and_then(|state| match state.as_str() {
                "open" | "closed" | "merged" => Ok(state),
                other => Err(format!("unexpected pull request state {other:?}")),
            })
    }

    fn dequeue(&self, pr: &ObservedPr, ledger: &mut StewardLedger) -> Result<(), String> {
        dequeue_with_context(&self.context()?, pr, ledger)
    }

    fn disarm(&self, pr: &ObservedPr, ledger: &mut StewardLedger) -> Result<(), String> {
        let context = self.context()?;
        let guard = acquire_pr_mutation_guard(
            context.mutation_control,
            context.observation,
            pr,
            "runner steward disable its own auto-merge arm during a solo-entry hold",
        )?;
        // The census is minutes old: read the pull request again under the
        // guard, and disable only a request still armed on the head the
        // steward armed, with no queue entry.
        let live = super::native_arm::read_queue_state(
            self.actions,
            &self.observation.repo,
            pr.fact.number,
        )
        .and_then(|queue| {
            let live = queue
                .pointer("/data/repository/pullRequest")
                .ok_or_else(|| "pull request not in the response".to_owned())?;
            let head = live.get("headRefOid").and_then(serde_json::Value::as_str);
            let armed = live
                .get("autoMergeRequest")
                .is_some_and(|request| !request.is_null());
            let queued = live.get("isInMergeQueue") != Some(&serde_json::Value::Bool(false));
            if head != Some(pr.fact.head_sha.as_str()) {
                Err("its head moved since the census".to_owned())
            } else if !armed {
                Err("it is no longer armed".to_owned())
            } else if queued {
                Err("it is in the merge queue now".to_owned())
            } else {
                Ok(())
            }
        });
        if let Err(reason) = live {
            let audit = guard.finish("solo_entry_disarm_skipped").err();
            return Err(audit.map_or(format!("left alone: {reason}"), |audit| {
                format!("left alone: {reason}; mutation audit also failed: {audit}")
            }));
        }
        match super::exact_head_merge::disarm_under_guard(&context, pr, ledger, DISARM_AUDIT) {
            Ok(()) => guard.finish("solo_entry_disarmed_own_arm"),
            Err(error) => {
                let audit = guard.finish("solo_entry_disarm_failed").err();
                Err(audit.map_or(error.clone(), |audit| {
                    format!("{error}; mutation audit also failed: {audit}")
                }))
            }
        }
    }
}

/// The inputs [`observe`] needs besides GitHub.
pub(super) struct ObserveInput<'a> {
    pub(super) settings: &'a Settings,
    pub(super) observation: &'a RepoObservation,
    pub(super) policy: &'a StewardPolicy,
    pub(super) apply: bool,
    pub(super) deadline: Option<std::time::Instant>,
    pub(super) now: DateTime<Utc>,
}

/// This pass's gate for one repository, read before any admission. Reads
/// nothing for a repository with no setting. Under `apply` it keeps the
/// window in the ledger, removes the solo entry when it is queued behind
/// another, and disables the auto-merge requests the steward itself armed.
pub(super) fn observe(
    reads: &dyn Reads,
    input: &ObserveInput<'_>,
    ledger: &mut StewardLedger,
) -> Gate {
    let observation = input.observation;
    let repo = &observation.repo;
    let key = window_key(repo, &observation.base);
    let setting = match input.settings.for_repo(repo) {
        None => {
            // A removed setting is a rollback: forget everything the mode
            // kept, so `runner steward-admit-ready` stops deferring the
            // repository.
            if input.apply {
                ledger.solo_entry_windows.remove(&key);
                ledger.solo_entry_native_arms.remove(&key);
            }
            return Gate::Off;
        }
        Some(Err(error)) => {
            return Gate::Inactive {
                line: format!("solo entry: setting refused ({error}); normal admission"),
            };
        }
        Some(Ok(setting)) => setting,
    };
    if !observation.merge_queue {
        return Gate::Inactive {
            line: format!(
                "solo entry: {} has no merge queue on {}; normal admission",
                repo, observation.base
            ),
        };
    }
    let prior = ledger.solo_entry_windows.get(&key).cloned();
    let (gate, window) = pass(reads, input, setting, prior, ledger);
    if input.apply {
        match window {
            Some(window) => {
                ledger.solo_entry_windows.insert(key, window);
            }
            None => {
                ledger.solo_entry_windows.remove(&key);
            }
        }
    }
    gate
}

fn inactive(line: &str) -> Gate {
    Gate::Inactive {
        line: format!("solo entry: {line}"),
    }
}

/// One pass over a configured repository: the gate, and the window to keep
/// (`None` forgets it).
#[allow(clippy::too_many_lines)] // One pass, in order: release, hold, readiness, queue.
fn pass(
    reads: &dyn Reads,
    input: &ObserveInput<'_>,
    setting: &RepoSetting,
    prior: Option<Window>,
    ledger: &mut StewardLedger,
) -> (Gate, Option<Window>) {
    let observation = input.observation;
    let repo = &observation.repo;
    let now = input.now;
    let branches = setting.branches.join(", ");
    // The release read. A failed read keeps the last known window for up to
    // `MAX_READ_FAILURES` passes, then fails open; with no window on record
    // it holds nothing.
    let mut read_note = String::new();
    let (pending, read_failures) = match reads.pending_release(setting) {
        Ok(pending) => (pending, 0),
        Err(error) => match prior.as_ref() {
            None => {
                return (
                    inactive(&format!(
                        "release state unknown ({error}); normal admission"
                    )),
                    None,
                );
            }
            Some(window) if window.read_failures >= MAX_READ_FAILURES => {
                let mut window = window.clone();
                let released = window.attempt.take().is_some();
                window.read_failures = window.read_failures.saturating_add(1);
                return (
                    inactive(&format!(
                        "release state unreadable for more than {MAX_READ_FAILURES} passes \
                         ({error}); {}normal admission",
                        if released {
                            "HOLD RELEASED (fail open); "
                        } else {
                            ""
                        }
                    )),
                    Some(window),
                );
            }
            Some(window) => {
                read_note = format!(
                    "; release state unreadable ({error}), last known window kept (failed read \
                     {} of {MAX_READ_FAILURES})",
                    window.read_failures + 1
                );
                (Some(window.version.clone()), window.read_failures + 1)
            }
        },
    };
    let Some(version) = pending else {
        let released = prior.is_some_and(|window| window.attempt.is_some());
        return (
            inactive(if released {
                "no release pending; hold released; normal admission"
            } else {
                "no release pending; normal admission"
            }),
            None,
        );
    };
    let mut window = match prior {
        Some(window) if window.version == version => window,
        _ => Window::new(&version, now),
    };
    window.read_failures = read_failures;
    // Solo pull requests a hold can be for: a head a hold already let go of
    // is not one.
    let candidates = observation
        .prs
        .iter()
        .filter(|pr| {
            candidate(&setting.branches, pr)
                && !window.is_released(pr.fact.number, &pr.fact.head_sha)
        })
        .collect::<Vec<_>>();
    let mut events = String::new();
    let mut note = String::new();
    let mut attempt = window.attempt.take();
    if let Some(engaged) = attempt.as_mut() {
        let listed = observation
            .prs
            .iter()
            .find(|pr| pr.fact.number == engaged.solo);
        let same_head = listed.filter(|pr| pr.fact.head_sha == engaged.head);
        if attempt_expired(engaged, now) {
            // Past the deadline with its entry in the merge queue: letting go
            // now would admit the backlog behind it, into its push. The queue
            // decides (it merges, or the queue removes it), and the hold
            // waits for that, for at most one more `window_minutes`.
            let hard_stop =
                engaged.engaged_at + ChronoDuration::minutes(2 * setting.window_minutes);
            let queued = same_head.is_some_and(|pr| pr.fact.queue_position.is_some());
            if queued && now >= engaged.engaged_at && now < hard_stop {
                engaged.deadline =
                    (now + ChronoDuration::minutes(QUEUED_EXTENSION_MINUTES)).min(hard_stop);
                "; past its window, HOLD EXTENDED while its entry is in the merge queue"
                    .clone_into(&mut events);
            } else {
                window
                    .released
                    .insert(released_key(engaged.solo, &engaged.head));
                return (
                    inactive(&format!(
                        "RELEASE WINDOW EXPIRED for {version}: the hold for #{} ran its {} min \
                         (attempt {} of {MAX_ATTEMPTS}); normal admission resumes{read_note}",
                        engaged.solo, setting.window_minutes, window.attempts
                    )),
                    Some(window),
                );
            }
        }
        // Seen queued, and now open at the same head with no queue entry:
        // the queue removed it. The same head is not enqueued again, so
        // holding for it would freeze the repository for nothing.
        if engaged.queued && same_head.is_some_and(|pr| pr.fact.queue_position.is_none()) {
            window
                .released
                .insert(released_key(engaged.solo, &engaged.head));
            return (
                inactive(&format!(
                    "HOLD RELEASED for {version}: #{} left the merge queue unmerged at {}; a new \
                     head may engage it again; normal admission{read_note}",
                    engaged.solo,
                    short(&engaged.head)
                )),
                Some(window),
            );
        }
        match listed {
            // Not among the open pull requests: the attempt ends when GitHub
            // says it merged or closed, whether or not the next reconcile is
            // already open (that one starts its own attempt). A census that
            // only missed it keeps the hold.
            None => match reads.pr_state(engaged.solo) {
                Ok(state) if state != "open" => {
                    return (
                        inactive(&format!(
                            "HOLD RELEASED for {version}: #{} {state}; normal admission{read_note}",
                            engaged.solo
                        )),
                        Some(window),
                    );
                }
                Ok(_) => {
                    note = format!(
                        "; #{} is open but not in this pass's census, hold kept",
                        engaged.solo
                    );
                }
                Err(error) => {
                    note = format!(
                        "; #{} is not in this pass's census and its state is unreadable \
                         ({error}), hold kept",
                        engaged.solo
                    );
                }
            },
            Some(pr)
                if !candidates
                    .iter()
                    .any(|other| other.fact.number == pr.fact.number) =>
            {
                return (
                    inactive(&format!(
                        "HOLD RELEASED for {version}: #{} is no longer a solo pull request (a \
                         draft, or back at a head already let go); normal admission{read_note}",
                        engaged.solo
                    )),
                    Some(window),
                );
            }
            Some(_) => {}
        }
    }
    let is_ready = |pr: &ObservedPr, ledger: &StewardLedger| {
        let decision = classify_pr(
            &pr.fact,
            input.policy,
            &attempts_for(ledger, repo, &pr.fact),
        );
        ready(pr, &decision, &|pr| reads.approved(pr, ledger))
    };
    // The hold is for one pull request. Another one on a solo branch is held
    // like any other row until this attempt ends.
    let (mut attempt, solo, solo_ready) = if let Some(engaged) = attempt {
        let solo = candidates
            .iter()
            .copied()
            .find(|pr| pr.fact.number == engaged.solo);
        let solo_ready = solo.is_some_and(|pr| is_ready(pr, ledger));
        (engaged, solo, solo_ready)
    } else {
        // An unreadable head repository is not a solo pull request.
        let ready_solo = candidates
            .iter()
            .copied()
            .filter(|pr| is_ready(pr, ledger) && reads.same_repo(pr).unwrap_or(false))
            .min_by_key(|pr| pr.fact.number);
        let Some(solo) = ready_solo else {
            return (
                inactive(&format!(
                    "release window for {version} open, but no admission-ready pull request \
                         on {branches}; normal admission{read_note}"
                )),
                Some(window),
            );
        };
        if window.attempts >= MAX_ATTEMPTS {
            return (
                inactive(&format!(
                    "all {MAX_ATTEMPTS} hold attempts for {version} are spent; #{} is \
                         admitted as any other pull request{read_note}",
                    solo.fact.number
                )),
                Some(window),
            );
        }
        window.attempts += 1;
        events = format!(
            "; HOLD ENGAGED (attempt {} of {MAX_ATTEMPTS}, at most {} min)",
            window.attempts, setting.window_minutes
        );
        (
            Attempt {
                solo: solo.fact.number,
                head: solo.fact.head_sha.clone(),
                engaged_at: now,
                deadline: now + ChronoDuration::minutes(setting.window_minutes),
                refused: 0,
                queued: false,
                own_dequeues: 0,
            },
            Some(solo),
            true,
        )
    };
    let number = attempt.solo;
    if let Some(solo) = solo
        && attempt.head != solo.fact.head_sha
    {
        // Rebuilt: the hold follows the new head, on the same clock. The new
        // head was never queued and never refused.
        attempt.head.clone_from(&solo.fact.head_sha);
        attempt.refused = 0;
        attempt.queued = false;
    }
    let mut phase = Phase::NotReady;
    if let Some(solo) = solo.filter(|_| solo_ready) {
        let positions = reads.queue();
        let total = positions.as_ref().map_or(0, BTreeMap::len);
        phase = self::phase(number, positions.as_ref());
        // Why the solo entry must leave the queue, when it must.
        let mut leave = None;
        match phase {
            Phase::Admit => match reads.up_to_date(solo) {
                Ok(true) => {}
                Ok(false) => phase = Phase::Stale,
                Err(error) => {
                    phase = Phase::BaseUnreadable;
                    note = format!(" ({error})");
                }
            },
            Phase::QueueUnreadable => {
                note = format!(" ({})", positions.as_ref().err().map_or("", String::as_str));
            }
            Phase::Queued => {
                attempt.queued = true;
                // An entry the tick's own admission did not create (a ready
                // event, a person, a native arm) may be stale: compare it.
                match reads.up_to_date(solo) {
                    Ok(false) => {
                        phase = Phase::Stale;
                        leave = Some("behind the base tip".to_owned());
                    }
                    Ok(true) => {}
                    Err(error) => note = format!("; base comparison unreadable ({error})"),
                }
                let behind = positions
                    .as_ref()
                    .map(|positions| {
                        positions
                            .keys()
                            .filter(|other| **other != number)
                            .map(|other| format!("#{other}"))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if phase == Phase::Queued && !behind.is_empty() {
                    let _ = write!(
                        note,
                        "; solo_entry_shared_batch_risk: {} queued behind it and may merge in \
                         its push (not the steward's entry, left alone)",
                        behind.join(", ")
                    );
                }
            }
            Phase::Behind { ahead } => {
                attempt.queued = true;
                leave = Some(format!("{ahead} ahead"));
            }
            Phase::NotReady | Phase::Waiting { .. } | Phase::Stale | Phase::BaseUnreadable => {}
        }
        if let Some(why) = leave {
            if !input.apply {
                note = format!("; would dequeue its own entry ({why})");
            } else if attempt.own_dequeues >= MAX_OWN_DEQUEUES {
                window.released.insert(released_key(number, &attempt.head));
                return (
                    inactive(&format!(
                        "HOLD RELEASED for {version}: #{number} is queued {why} after \
                         {MAX_OWN_DEQUEUES} own dequeues; its entry is left in the queue; normal \
                         admission{read_note}"
                    )),
                    Some(window),
                );
            } else if super::pass_budget::deadline_passed(input.deadline) {
                "; its own dequeue deferred: the pass time budget is spent".clone_into(&mut note);
            } else {
                match reads.dequeue(solo, ledger) {
                    Ok(()) => {
                        attempt.own_dequeues += 1;
                        attempt.queued = false;
                        note = format!("; dequeued its own entry ({why}), retry next pass");
                        if matches!(phase, Phase::Behind { .. }) {
                            phase = Phase::Waiting {
                                ahead: total.saturating_sub(1),
                            };
                        }
                    }
                    Err(error) => note = format!("; its own dequeue failed: {error}"),
                }
            }
        }
    }
    note.push_str(&disarm_own_arms(reads, input, number, ledger));
    let state = match &phase {
        Phase::NotReady => {
            "held_for_solo_attempt: it is not admission-ready yet (rebuilt, unapproved or red)"
                .to_owned()
        }
        Phase::Waiting { ahead } => format!("waiting, {ahead} queue entries ahead"),
        Phase::Admit => "queue empty and head up to date, admitting it alone".to_owned(),
        Phase::Stale => {
            "solo_entry_waiting:behind_base: its head is behind the base tip; waiting for its \
             owner to rebuild it"
                .to_owned()
        }
        Phase::Queued => "queued with nothing ahead".to_owned(),
        Phase::Behind { ahead } => format!("queued behind {ahead}"),
        Phase::QueueUnreadable => "merge queue unreadable; nothing admitted".to_owned(),
        Phase::BaseUnreadable => "base comparison unreadable; nothing admitted".to_owned(),
    };
    let line = format!(
        "solo entry: #{number} ({}) holds admission for {repo} for the {version} release (attempt \
         {} of {MAX_ATTEMPTS}, {} min of hold left): {state}{note}{events}{read_note}",
        solo.map_or(branches.as_str(), |solo| solo.fact.head_branch.as_str()),
        window.attempts,
        (attempt.deadline - now).num_minutes()
    );
    window.attempt = Some(attempt);
    (
        Gate::Hold {
            solo: number,
            phase,
            line,
        },
        Some(window),
    )
}

fn short(head: &str) -> &str {
    head.get(..12).unwrap_or(head)
}

/// While the hold is engaged: disable every auto-merge request the
/// steward's own backstop armed (recorded by [`record_native_arms`]) that is
/// still armed on the same head and not queued, so GitHub cannot enqueue it
/// behind the solo entry. A request anyone else armed is never touched. The
/// backstop arms them again, under its own checks, once the hold ends.
fn disarm_own_arms(
    reads: &dyn Reads,
    input: &ObserveInput<'_>,
    solo: u64,
    ledger: &mut StewardLedger,
) -> String {
    let observation = input.observation;
    let key = window_key(&observation.repo, &observation.base);
    let Some(armed) = ledger.solo_entry_native_arms.get(&key).cloned() else {
        return String::new();
    };
    let own = observation
        .prs
        .iter()
        .filter(|pr| {
            pr.fact.number != solo
                && pr.fact.auto_merge_active
                && pr.fact.queue_position.is_none()
                && armed.get(&pr.fact.number) == Some(&pr.fact.head_sha)
        })
        .collect::<Vec<_>>();
    let mut note = String::new();
    for pr in own {
        let number = pr.fact.number;
        if !input.apply {
            let _ = write!(
                note,
                "; would disable the auto-merge the steward armed on #{number}"
            );
        } else if super::pass_budget::deadline_passed(input.deadline) {
            let _ = write!(
                note,
                "; disabling its own arm on #{number} deferred: the pass time budget is spent"
            );
        } else {
            match reads.disarm(pr, ledger) {
                Ok(()) => {
                    // No longer the steward's arm: a request anyone arms on
                    // this head later is theirs. The backstop's own re-arm
                    // records it again.
                    if let Some(arms) = ledger.solo_entry_native_arms.get_mut(&key) {
                        arms.remove(&number);
                        if arms.is_empty() {
                            ledger.solo_entry_native_arms.remove(&key);
                        }
                    }
                    let _ = write!(
                        note,
                        "; disabled the auto-merge the steward armed on #{number} for the hold"
                    );
                }
                Err(error) => {
                    let _ = write!(
                        note,
                        "; could not disable its own arm on #{number}: {error}"
                    );
                }
            }
        }
    }
    note
}

/// Keep the ledger's record of auto-merge requests the backstop armed this
/// pass, for [`disarm_own_arms`]. Only for a repository with a setting, so a
/// repository without one keeps a byte-identical ledger. A record is dropped
/// once its pull request closed or its head moved.
pub(super) fn record_native_arms(
    settings: &Settings,
    observation: &RepoObservation,
    results: &[super::native_arm::NativeArmResult],
    apply: bool,
    ledger: &mut StewardLedger,
) {
    if !apply || !settings.configured(&observation.repo) {
        return;
    }
    let key = window_key(&observation.repo, &observation.base);
    let mut arms = ledger
        .solo_entry_native_arms
        .remove(&key)
        .unwrap_or_default();
    arms.retain(|number, head| {
        observation
            .prs
            .iter()
            .any(|pr| pr.fact.number == *number && pr.fact.head_sha == *head)
    });
    for result in results {
        if matches!(result.outcome.as_str(), "armed" | "rearmed_same_head") {
            arms.insert(result.number, result.head_sha.clone());
        }
    }
    if !arms.is_empty() {
        ledger.solo_entry_native_arms.insert(key, arms);
    }
}

/// After the pass's admissions: the repository report's `solo_entry` line.
/// Names every base fix that passed the hold, and counts a pass on which the
/// gate admitted the solo head and nothing enqueued it. After
/// [`MAX_REFUSED_PASSES`] such passes the hold lets go of that head: a head
/// the enqueue path refuses (ejected and unchanged, carried commits, a
/// code-owner wait, a defect-fix block) would otherwise hold every other
/// pull request until the hold expired. Only a refusal the enqueue path names
/// with a token counts: a deferral, the guarded re-read's own wait, and a row
/// with no token (a central HOLD, the wrong mutation machine, a failed read)
/// are not refusals.
pub(super) fn settle(
    gate: &Gate,
    observation: &RepoObservation,
    reports: &[super::PrReport],
    poison: Option<&super::base_poison::RepoPoison>,
    apply: bool,
    ledger: &mut StewardLedger,
) -> Option<String> {
    let mut line = gate.line()?;
    let Gate::Hold { solo, phase, .. } = gate else {
        return Some(line);
    };
    let exempt = observation
        .prs
        .iter()
        .map(|pr| pr.fact.number)
        .filter(|number| number != solo && poison_fix_exempt(poison, *number))
        .map(|number| format!("#{number}"))
        .collect::<Vec<_>>();
    if !exempt.is_empty() {
        let _ = write!(
            line,
            "; solo_entry_exempt_base_fix: {} passes the hold as the fix for the red base",
            exempt.join(", ")
        );
    }
    if !apply || *phase != Phase::Admit {
        return Some(line);
    }
    let Some(report) = reports.iter().find(|report| report.number == *solo) else {
        return Some(line);
    };
    let mutation = report.mutation.as_deref();
    let waited = super::pass_budget::is_deferred(report)
        || mutation.is_some_and(|mutation| {
            mutation
                .split(',')
                .any(|part| part.starts_with("solo_entry_"))
        })
        // No token: nothing judged this head. A central HOLD, the wrong
        // mutation machine and a failed read all end here; only a refusal
        // the enqueue path names counts.
        || mutation.is_none();
    let key = window_key(&observation.repo, &observation.base);
    let Some(window) = ledger.solo_entry_windows.get_mut(&key) else {
        return Some(line);
    };
    let Some(attempt) = window
        .attempt
        .as_mut()
        .filter(|attempt| attempt.solo == *solo)
    else {
        return Some(line);
    };
    if mutation.is_some_and(was_enqueued) {
        attempt.refused = 0;
    } else if !waited {
        attempt.refused += 1;
        let why = mutation.unwrap_or_default().to_owned();
        if attempt.refused >= MAX_REFUSED_PASSES {
            let released = released_key(attempt.solo, &attempt.head);
            window.released.insert(released);
            window.attempt = None;
            let _ = write!(
                line,
                "; HOLD RELEASED for this head: admitted and not enqueued on \
                 {MAX_REFUSED_PASSES} passes ({why}); a new head may engage it again"
            );
        } else {
            let _ = write!(
                line,
                "; admitted and not enqueued ({why}): pass {} of {MAX_REFUSED_PASSES} before the \
                 hold lets go of this head",
                attempt.refused
            );
        }
    }
    Some(line)
}

/// Remove `pr`'s own queue entry under the merge-queue mutation guard.
fn dequeue_with_context(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    ledger: &mut StewardLedger,
) -> Result<(), String> {
    let guard = acquire_pr_mutation_guard(
        context.mutation_control,
        context.observation,
        pr,
        "runner steward dequeue solo entry queued behind another",
    )?;
    match super::unapproved_admission::dequeue_under_guard(
        context,
        pr,
        &pr.fact.head_sha,
        ledger,
        DEQUEUE_AUDIT,
    ) {
        Ok(()) => guard.finish("dequeued_solo_entry"),
        Err(error) => {
            let audit = guard.finish("solo_entry_dequeue_failed").err();
            Err(audit.map_or(error.clone(), |audit| {
                format!("{error}; mutation audit also failed: {audit}")
            }))
        }
    }
}

/// Under the enqueue's mutation guard, just before the solo pull request is
/// enqueued: the queue and the base comparison read again. `Some` refuses
/// the enqueue (an entry appeared, the base moved, or either read failed).
pub(super) fn pre_enqueue_refusal(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
) -> Option<Result<String, String>> {
    if !context
        .solo_entry
        .is_some_and(|gate| gate.admits(pr.fact.number))
    {
        return None;
    }
    match queue_positions(context.actions, context.observation) {
        Ok(positions) if positions.is_empty() => {}
        Ok(positions) => {
            return Some(Ok(format!("solo_entry_waiting:{}_ahead", positions.len())));
        }
        Err(error) => {
            return Some(Err(format!(
                "solo entry: merge queue unreadable before enqueue: {error}"
            )));
        }
    }
    match head_up_to_date(
        context.actions,
        &context.observation.repo,
        &context.observation.base,
        &pr.fact.head_sha,
    ) {
        Ok(true) => None,
        Ok(false) => Some(Ok("solo_entry_waiting:behind_base".to_owned())),
        Err(error) => Some(Err(format!("solo entry: {error} before enqueue"))),
    }
}

/// Right after the solo pull request was enqueued: the queue read again, and
/// its own entry removed when anything is ahead of it. Returns the mutation
/// token to append, when there is one.
pub(super) fn verify_after_enqueue(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    if !context
        .solo_entry
        .is_some_and(|gate| gate.admits(pr.fact.number))
    {
        return (None, None);
    }
    // Unreadable: the next pass reads the queue again and acts then.
    let Ok(positions) = queue_positions(context.actions, context.observation) else {
        return (Some("solo_entry_recheck_unread".to_owned()), None);
    };
    match phase(pr.fact.number, Ok(&positions)) {
        Phase::Behind { ahead } => match dequeue_with_context(context, pr, ledger) {
            Ok(()) => {
                let key = window_key(&context.observation.repo, &context.observation.base);
                if let Some(attempt) = ledger
                    .solo_entry_windows
                    .get_mut(&key)
                    .and_then(|window| window.attempt.as_mut())
                {
                    attempt.own_dequeues += 1;
                    attempt.queued = false;
                }
                (Some(format!("dequeued_solo_entry:{ahead}_ahead")), None)
            }
            Err(error) => (
                Some(format!("solo_entry_behind:{ahead}_ahead")),
                Some(format!(
                    "solo entry queued behind {ahead}; own dequeue failed: {error}"
                )),
            ),
        },
        _ => (None, None),
    }
}
