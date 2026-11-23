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
//! `x.y.z` in the file) is newer than every `<prefix>x.y.z` tag and has no tag
//! of its own. The window opens the first pass that sees it, closes when the
//! tag exists, and expires `window_minutes` after it opened (one log line), so
//! a reconcile that never lands cannot freeze the repository.
//!
//! Inside a window, while a pull request whose head is a solo branch is open,
//! not a draft, and admission-ready (the steward would enqueue it and its
//! exact head is approved, or it is already queued):
//!
//! * nothing else is admitted for the repository: the steward's own enqueue
//!   (every caller of [`super::pr_mutations::mutate_pr_with_recovery`]) and the
//!   native-arm backstop both hold;
//! * the solo pull request is enqueued only when the base's merge queue is
//!   empty, read again under the mutation guard just before the enqueue;
//! * right after the enqueue, and on every pass while it is queued, the queue
//!   is read again; when any entry is ahead of it, the steward dequeues only
//!   its own solo entry ([`super::unapproved_admission::dequeue_under_guard`],
//!   so the removal never counts as an ejection) and retries next pass. It
//!   never dequeues another pull request.
//!
//! A solo pull request that is not admission-ready (red, held, unapproved,
//! draft, or not the steward's to enqueue) changes nothing. Every mutation
//! goes through the merge-queue mutation guard, so the central HOLD, the
//! mutation machine and the pass budget all apply. `runner steward-admit-ready`
//! skips a repository whose window is open, leaving it to the tick.

use std::collections::BTreeMap;
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
/// How long a release window holds admission before it lets go.
pub(super) const DEFAULT_WINDOW_MINUTES: i64 = 120;
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
    /// How long a release window holds admission.
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
    /// The version whose tag the window waits for.
    pub(super) version: String,
    /// When a pass first saw the release request.
    pub(super) opened_at: DateTime<Utc>,
    /// When the window lets go whatever happened.
    pub(super) deadline: DateTime<Utc>,
    /// Set once the deadline passed; the window stays closed for `version`.
    #[serde(default)]
    pub(super) expired: bool,
}

/// The ledger key of `repo`'s window on `base`.
pub(super) fn window_key(repo: &str, base: &str) -> String {
    format!("{}@{base}", repo.to_ascii_lowercase())
}

/// Whether `repo`'s release window on `base` is open at `now`, as the last
/// tick recorded it. `runner steward-admit-ready` leaves such a repository to the
/// tick.
pub(super) fn window_open(
    ledger: &StewardLedger,
    repo: &str,
    base: &str,
    now: DateTime<Utc>,
) -> bool {
    ledger
        .solo_entry_windows
        .get(&window_key(repo, base))
        .is_some_and(|window| !window.expired && now < window.deadline)
}

/// What the window does this pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WindowStatus {
    /// No release request is pending.
    NoRelease,
    /// Open, with this long left.
    Open {
        version: String,
        remaining_minutes: i64,
    },
    /// Past its deadline; `just_now` on the pass that closed it.
    Expired { version: String, just_now: bool },
}

/// Advance `state` for this pass's release reading `pending` (the version a
/// release request names, when one is pending).
pub(super) fn advance_window(
    state: Option<&Window>,
    pending: Option<&str>,
    window_minutes: i64,
    now: DateTime<Utc>,
) -> (Option<Window>, WindowStatus) {
    let Some(version) = pending else {
        return (None, WindowStatus::NoRelease);
    };
    let mut window = match state {
        Some(window) if window.version == version => window.clone(),
        _ => Window {
            version: version.to_owned(),
            opened_at: now,
            deadline: now + ChronoDuration::minutes(window_minutes),
            expired: false,
        },
    };
    if window.expired {
        return (
            Some(window),
            WindowStatus::Expired {
                version: version.to_owned(),
                just_now: false,
            },
        );
    }
    if now >= window.deadline || now < window.opened_at {
        window.expired = true;
        return (
            Some(window),
            WindowStatus::Expired {
                version: version.to_owned(),
                just_now: true,
            },
        );
    }
    let remaining_minutes = (window.deadline - now).num_minutes();
    (
        Some(window),
        WindowStatus::Open {
            version: version.to_owned(),
            remaining_minutes,
        },
    )
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

/// Whether `version` is a pending release request against `tags` (full refs
/// or tag names): newer than every `<prefix>x.y.z` tag and untagged itself.
pub(super) fn release_pending(version: (u64, u64, u64), tags: &[String], prefix: &str) -> bool {
    let tagged = tags
        .iter()
        .filter_map(|tag| {
            let name = tag.strip_prefix("refs/tags/").unwrap_or(tag);
            semver(name.strip_prefix(prefix)?)
        })
        .collect::<Vec<_>>();
    !tagged.contains(&version) && tagged.iter().all(|tag| version > *tag)
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
    Ok(release_pending(version, &tags, &setting.tag_prefix)
        .then(|| format!("{}{major}.{minor}.{patch}", setting.tag_prefix)))
}

/// Where the solo pull request stands against the base's merge queue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Phase {
    /// Not queued; this many entries are in the queue, so it waits.
    Waiting { ahead: usize },
    /// Not queued and the queue is empty: only it may be enqueued.
    Admit,
    /// Queued with nothing ahead of it.
    Queued,
    /// Queued behind `ahead` entries: the steward removes its own entry.
    Behind { ahead: usize },
    /// The queue could not be read: nothing is admitted this pass.
    QueueUnreadable,
}

/// The repository's solo-entry gate for one pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) enum Gate {
    /// No setting for the repository: admission exactly as without the mode.
    #[default]
    Off,
    /// Configured, but nothing is held this pass; the line says why.
    Inactive { line: String },
    /// A ready solo pull request holds every other admission.
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
            return Some(format!("held_solo_entry:#{solo}"));
        }
        match phase {
            Phase::Admit => None,
            Phase::Waiting { ahead } | Phase::Behind { ahead } => {
                Some(format!("solo_entry_waiting:{ahead}_ahead"))
            }
            Phase::Queued => Some("solo_entry_queued".to_owned()),
            Phase::QueueUnreadable => Some("solo_entry_queue_unreadable".to_owned()),
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

/// Whether `pr` is the steward's admission-ready solo pull request: a solo
/// branch, open (it is in the census), not a draft, and either queued or one
/// the steward would enqueue with an approved exact head.
pub(super) fn ready(
    branches: &[String],
    pr: &ObservedPr,
    decision: &StewardDecision,
    approved: &dyn Fn(&ObservedPr) -> bool,
) -> bool {
    branches.contains(&pr.fact.head_branch)
        && !pr.fact.draft
        && match decision {
            StewardDecision::Queued { .. } => true,
            StewardDecision::ArmMergeQueue => approved(pr),
            _ => false,
        }
}

/// The phase of solo pull request `solo` against `queue` (pull request to
/// position, all from one snapshot). "Ahead" is a strictly lower position.
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
    /// The base's merge queue: pull request to position.
    fn queue(&self) -> Result<BTreeMap<u64, u64>, String>;
    /// Remove `pr`'s own queue entry under the mutation guard.
    fn dequeue(&self, pr: &ObservedPr, ledger: &mut StewardLedger) -> Result<(), String>;
}

/// [`Reads`] against GitHub.
pub(super) struct GitHubReads<'a> {
    pub(super) actions: &'a GitHubActions,
    pub(super) observation: &'a RepoObservation,
    pub(super) control: Option<&'a MutationControl>,
    pub(super) ledger_path: &'a Path,
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

    fn queue(&self) -> Result<BTreeMap<u64, u64>, String> {
        queue_positions(self.actions, self.observation)
    }

    fn dequeue(&self, pr: &ObservedPr, ledger: &mut StewardLedger) -> Result<(), String> {
        let control = self
            .control
            .ok_or_else(|| "a dequeue needs mutation control".to_owned())?;
        dequeue_own(
            self.actions,
            self.observation,
            control,
            self.ledger_path,
            pr,
            ledger,
        )
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
/// window in the ledger and, when the solo entry is queued behind another,
/// removes the solo entry.
#[allow(clippy::too_many_lines)] // One pass, in order: setting, window, readiness, queue.
pub(super) fn observe(
    reads: &dyn Reads,
    input: &ObserveInput<'_>,
    ledger: &mut StewardLedger,
) -> Gate {
    let observation = input.observation;
    let repo = &observation.repo;
    let setting = match input.settings.for_repo(repo) {
        None => {
            // A removed setting is a rollback: forget its window too, so
            // `runner steward-admit-ready` stops deferring the repository.
            if input.apply {
                ledger
                    .solo_entry_windows
                    .remove(&window_key(repo, &observation.base));
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
    let pending = match reads.pending_release(setting) {
        Ok(pending) => pending,
        Err(error) => {
            return Gate::Inactive {
                line: format!("solo entry: release state unknown ({error}); normal admission"),
            };
        }
    };
    let key = window_key(repo, &observation.base);
    let (window, status) = advance_window(
        ledger.solo_entry_windows.get(&key),
        pending.as_deref(),
        setting.window_minutes,
        input.now,
    );
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
    let (version, remaining) = match status {
        WindowStatus::NoRelease => {
            return Gate::Inactive {
                line: "solo entry: no release pending; normal admission".to_owned(),
            };
        }
        WindowStatus::Expired { version, just_now } => {
            return Gate::Inactive {
                line: if just_now {
                    format!(
                        "solo entry: RELEASE WINDOW EXPIRED for {version} after {} min; normal \
                         admission resumes",
                        setting.window_minutes
                    )
                } else {
                    format!("solo entry: release window for {version} expired; normal admission")
                },
            };
        }
        WindowStatus::Open {
            version,
            remaining_minutes,
        } => (version, remaining_minutes),
    };
    let solo = observation
        .prs
        .iter()
        .filter(|pr| {
            let decision = classify_pr(
                &pr.fact,
                input.policy,
                &attempts_for(ledger, repo, &pr.fact),
            );
            ready(&setting.branches, pr, &decision, &|pr| {
                reads.approved(pr, ledger)
            })
        })
        .min_by_key(|pr| pr.fact.number);
    let Some(solo) = solo else {
        return Gate::Inactive {
            line: format!(
                "solo entry: release window for {version} open ({remaining} min left), but no \
                 admission-ready pull request on {}; normal admission",
                setting.branches.join(", ")
            ),
        };
    };
    let number = solo.fact.number;
    let positions = reads.queue();
    let mut phase = phase(number, positions.as_ref());
    let mut note = String::new();
    if let Phase::Behind { ahead } = phase {
        let total = positions.as_ref().map_or(0, BTreeMap::len);
        if !input.apply {
            note = format!("; would dequeue its own entry ({ahead} ahead)");
        } else if super::pass_budget::deadline_passed(input.deadline) {
            "; its own dequeue deferred: the pass time budget is spent".clone_into(&mut note);
        } else {
            match reads.dequeue(solo, ledger) {
                Ok(()) => {
                    note = format!("; dequeued its own entry ({ahead} ahead), retry next pass");
                    phase = Phase::Waiting {
                        ahead: total.saturating_sub(1),
                    };
                }
                Err(error) => note = format!("; its own dequeue failed: {error}"),
            }
        }
    }
    let state = match &phase {
        Phase::Waiting { ahead } => format!("waiting, {ahead} queue entries ahead"),
        Phase::Admit => "queue empty, admitting it alone".to_owned(),
        Phase::Queued => "queued with nothing ahead".to_owned(),
        Phase::Behind { ahead } => format!("queued behind {ahead}"),
        Phase::QueueUnreadable => format!(
            "merge queue unreadable ({}); nothing admitted",
            positions.as_ref().err().map_or("", String::as_str)
        ),
    };
    Gate::Hold {
        solo: number,
        phase,
        line: format!(
            "solo entry: #{number} ({}) holds admission for {repo} during the {version} release \
             window ({remaining} min left): {state}{note}",
            solo.fact.head_branch
        ),
    }
}

fn dequeue_own(
    actions: &GitHubActions,
    observation: &RepoObservation,
    control: &MutationControl,
    ledger_path: &Path,
    pr: &ObservedPr,
    ledger: &mut StewardLedger,
) -> Result<(), String> {
    let context = MutationApplyContext {
        actions,
        observation,
        ledger_path,
        mutation_control: control,
        approval_blockers: std::cell::RefCell::default(),
        carried_commits: std::cell::RefCell::default(),
        changed_files: std::cell::RefCell::default(),
        ejection_guards: crate::merge_steward::EjectionGuards::default(),
        base_poison: None,
        defect_fix: None,
        solo_entry: None,
    };
    dequeue_with_context(&context, pr, ledger)
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
/// enqueued: the queue read again. `Some` refuses the enqueue (an entry
/// appeared, or the queue cannot be read).
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
        Ok(positions) if positions.is_empty() => None,
        Ok(positions) => Some(Ok(format!("solo_entry_waiting:{}_ahead", positions.len()))),
        Err(error) => Some(Err(format!(
            "solo entry: merge queue unreadable before enqueue: {error}"
        ))),
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
            Ok(()) => (Some(format!("dequeued_solo_entry:{ahead}_ahead")), None),
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
