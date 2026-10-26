//! Rollup batching: land several approved pull requests as one tested merge.
//!
//! ## Why
//!
//! A merge queue that requires every entry's checks builds each entry's own
//! cumulative group, so N pull requests still cost N full runs. A rollup is
//! the one shape that costs one: a branch that merges each member's exact
//! approved head onto the base, tested and landed as a single pull request.
//!
//! ## The shape
//!
//! Branch `shipyard-train/rollup-<n>` is the base commit `B` plus one merge
//! commit per member, oldest first: `M_i = merge(M_{i-1}, H_i)` with
//! `M_0 = B`, whose second parent is member `i`'s exact approved head `H_i`.
//! Nothing else is committed. The rollup pull request carries a notice
//! comment (`rollup-of: #n@H_i` per member) for people; [`accept`] never reads
//! it. Every member stays revertable on its own with `git revert -m 1 M_i`.
//!
//! ## What the guard accepts
//!
//! A rollup's head has no verdict of its own. The guard
//! ([`crate::head_approval`] and `scripts/ghapp_queue_arm_guard.py`, which
//! share this rule) lands it only when [`accept`] proves, from the base's
//! config and GitHub alone:
//!
//! * (a) each `H_i` is the current head of an open member pull request on
//!   the same base whose own exact-head approval counts (with
//!   `require_counted_approval`, the default, a carried approval does not),
//!   with no later HOLD;
//! * (b) the first-parent chain from the head back to `B` is exactly `k`
//!   (at most `max_members`) two-parent merges whose second parents are the
//!   `H_i`, and each merge changes exactly the member's files to the member's
//!   blobs (a replay: an added commit or an edit inside a merge refuses);
//! * (c) the members change disjoint files, every file of every member has
//!   the member's approved blob and mode at the rollup head, every other file
//!   is as at `B`, and the base touched none of a member's files between the
//!   member's merge base and `B`, nor between `B` and the base's tip;
//! * (d) no member changes a path in `exclude_paths` or in the guard's carry
//!   exclusions ([`crate::head_approval::DEFAULT_CARRY_EXCLUDED_PATHS`] or the
//!   base's `carry_excluded_paths`), and no two members change files matching
//!   the same `hot_files` glob.
//!
//! This is a bounded multi-source extension of the merge-only carry: each
//! member's approval carries to the rollup head because every file it
//! changes keeps the approved blob and mode, and nothing else changed. What
//! no rule can prove is that two separately reviewed changes are right
//! together; the rollup's own full run is what tests that.
//!
//! ## Configuration
//!
//! Off unless the base's `.shipyard/config.toml` turns it on:
//!
//! ```toml
//! [train]
//! enabled = true
//! mode = "rollup"           # "off" (default) | "burst" | "rollup"
//! landing = "merge-queue"   # or "direct"
//! max_members = 4           # 1..=4
//! exclude_paths = []        # globs no member may change
//! hot_files = []            # globs no two members may both change
//! require_counted_approval = true
//! ```
//!
//! `SHIPYARD_TRAIN=off` (or `0`/`false`, optionally `:owner/repo`) and a
//! `train.pause` file in the guards directory stop it on a host.
//!
//! Both landings need the merge method MERGE ([`merge_method_refusal`]):
//! squash or rebase would flatten the member merges into one commit. With
//! `landing = "direct"`, Shipyard lands the rollup itself ([`land_direct`])
//! only after the base's required checks pass on the rollup head, and only
//! while the base is still at `B`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::head_approval::{self, Approval, HeadGate, RunGh};
use crate::required_check_policy::encode_path_segment;

/// The rollup branch prefix; [`accept`] judges only these heads.
pub const BRANCH_PREFIX: &str = "shipyard-train/rollup-";
/// The base config table.
pub const TABLE: &str = "train";
/// No rollup has more members than this, whatever the config says.
pub const MEMBER_CAP: usize = 4;
/// The host kill switch: `off`, `0` or `false`, optionally `:<owner/repo>`.
pub const KILL_ENV: &str = "SHIPYARD_TRAIN";
/// The pause file in the guards directory.
pub const PAUSE_FILE: &str = "train.pause";
/// The notice-comment line naming one member.
pub const NOTICE_PREFIX: &str = "rollup-of:";
/// The direct-landing record, in Shipyard's state directory.
pub const LANDINGS_LOG: &str = "rollup-landings.jsonl";

/// `[train] mode`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// No batching.
    #[default]
    Off,
    /// Burst admission only; no rollup.
    Burst,
    /// Rollups.
    Rollup,
}

/// `[train] landing`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Landing {
    /// Through the base's merge queue, enqueued by the merge steward.
    #[default]
    MergeQueue,
    /// Shipyard merges the rollup pull request itself.
    Direct,
}

/// The base's `[train]` table.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Config {
    /// `enabled`.
    pub enabled: bool,
    /// `mode`.
    pub mode: Mode,
    /// `landing`.
    pub landing: Landing,
    /// `max_members`, at most [`MEMBER_CAP`].
    pub max_members: usize,
    /// `exclude_paths`: globs no member may change. Empty by default.
    pub exclude_paths: Vec<String>,
    /// `hot_files`: globs no two members may both change. Empty by default.
    pub hot_files: Vec<String>,
    /// `require_counted_approval`: a member's approval must name its exact
    /// head; a carried one does not count.
    pub require_counted_approval: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: Mode::Off,
            landing: Landing::MergeQueue,
            max_members: MEMBER_CAP,
            exclude_paths: Vec::new(),
            hot_files: Vec::new(),
            require_counted_approval: true,
        }
    }
}

impl Config {
    /// Read `[train]` from a parsed base config; an absent table is off.
    ///
    /// # Errors
    ///
    /// A value of the wrong type or out of range: a config that cannot be
    /// read never turns rollups on.
    pub fn from_table(table: &toml::Table) -> Result<Self, String> {
        let Some(section) = table.get(TABLE) else {
            return Ok(Self::default());
        };
        let section = section
            .as_table()
            .ok_or_else(|| format!("[{TABLE}] is not a table"))?;
        let mut config = Self::default();
        let text = |key: &str| -> Result<Option<&str>, String> {
            section
                .get(key)
                .map(|value| {
                    value
                        .as_str()
                        .ok_or_else(|| format!("[{TABLE}] {key} is not a string"))
                })
                .transpose()
        };
        let flag = |key: &str, default: bool| -> Result<bool, String> {
            section.get(key).map_or(Ok(default), |value| {
                value
                    .as_bool()
                    .ok_or_else(|| format!("[{TABLE}] {key} is not true or false"))
            })
        };
        let globs = |key: &str| -> Result<Vec<String>, String> {
            section.get(key).map_or(Ok(Vec::new()), |value| {
                value
                    .as_array()
                    .and_then(|items| {
                        items
                            .iter()
                            .map(|item| item.as_str().map(str::to_owned))
                            .collect::<Option<Vec<_>>>()
                    })
                    .ok_or_else(|| format!("[{TABLE}] {key} is not a list of strings"))
            })
        };
        config.enabled = flag("enabled", false)?;
        config.require_counted_approval = flag("require_counted_approval", true)?;
        config.mode = match text("mode")? {
            None | Some("off") => Mode::Off,
            Some("burst") => Mode::Burst,
            Some("rollup") => Mode::Rollup,
            Some(other) => {
                return Err(format!(
                    "[{TABLE}] mode {other:?} is not off, burst or rollup"
                ));
            }
        };
        config.landing = match text("landing")? {
            None | Some("merge-queue") => Landing::MergeQueue,
            Some("direct") => Landing::Direct,
            Some(other) => {
                return Err(format!(
                    "[{TABLE}] landing {other:?} is not merge-queue or direct"
                ));
            }
        };
        if let Some(value) = section.get("max_members") {
            let members = value
                .as_integer()
                .and_then(|count| usize::try_from(count).ok())
                .filter(|count| (1..=MEMBER_CAP).contains(count))
                .ok_or_else(|| {
                    format!("[{TABLE}] max_members must be a whole number from 1 to {MEMBER_CAP}")
                })?;
            config.max_members = members;
        }
        config.exclude_paths = globs("exclude_paths")?;
        config.hot_files = globs("hot_files")?;
        Ok(config)
    }

    /// [`Self::from_table`] from the base config's text; `None` (no file) is
    /// off.
    ///
    /// # Errors
    ///
    /// When the text does not parse or a value is invalid.
    pub fn from_text(text: Option<&str>) -> Result<Self, String> {
        let Some(text) = text else {
            return Ok(Self::default());
        };
        let table = text
            .parse::<toml::Table>()
            .map_err(|error| format!("{} does not parse: {error}", head_approval::CONFIG_PATH))?;
        Self::from_table(&table)
    }

    /// Whether this base takes rollups.
    #[must_use]
    pub fn rolls_up(&self) -> bool {
        self.enabled && self.mode == Mode::Rollup
    }
}

/// Why this host's train is stopped for `repo`, or `None`: the kill switch
/// `kill` ([`KILL_ENV`]'s value) or a [`PAUSE_FILE`] in `guards_dir`.
#[must_use]
pub fn stopped(repo: &str, kill: Option<&str>, guards_dir: Option<&Path>) -> Option<String> {
    if let Some(value) = kill.map(str::trim).filter(|value| !value.is_empty()) {
        let (switch, scope) = value.split_once(':').unwrap_or((value, ""));
        let off = ["off", "0", "false"]
            .iter()
            .any(|word| switch.trim().eq_ignore_ascii_case(word));
        if off && (scope.trim().is_empty() || scope.trim().eq_ignore_ascii_case(repo)) {
            return Some(format!("{KILL_ENV}={value} stops the train on this host"));
        }
    }
    let pause = guards_dir?.join(PAUSE_FILE);
    pause
        .exists()
        .then(|| format!("the train is paused ({})", pause.display()))
}

/// [`stopped`] from this process's environment and guards directory.
#[must_use]
pub fn host_stopped(repo: &str) -> Option<String> {
    stopped(
        repo,
        std::env::var(KILL_ENV).ok().as_deref(),
        head_approval::guards_dir().as_deref(),
    )
}

/// One member of an accepted rollup.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Member {
    /// The member pull request.
    pub pr: u64,
    /// Its exact approved head, the merge's second parent.
    pub head: String,
    /// The rollup's merge commit of that head.
    pub merge: String,
    /// The files it changes (rename sources included).
    pub files: Vec<String>,
    /// Its approval.
    pub approval: Option<Approval>,
}

/// An accepted rollup.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Rollup {
    /// The rollup pull request.
    pub pr: u64,
    /// Its head, the last merge.
    pub head: String,
    /// The base branch.
    pub base: String,
    /// `B`: the base commit the rollup was formed on.
    pub base_sha: String,
    /// The base branch's tip when read.
    pub base_tip: String,
    /// The members, oldest first.
    pub members: Vec<Member>,
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn gh_json(run_gh: RunGh<'_>, args: &[String]) -> Result<Value, String> {
    serde_json::from_str(run_gh(args)?.trim()).map_err(|error| format!("invalid JSON: {error}"))
}

fn api(path: String, jq: &str) -> Vec<String> {
    vec!["api".to_owned(), path, "--jq".to_owned(), jq.to_owned()]
}

/// The base config's text, `None` when the base has none.
///
/// # Errors
///
/// When it exists but cannot be read.
pub fn base_config_text(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
) -> Result<Option<String>, String> {
    match run_gh(&[
        "api".to_owned(),
        "-H".to_owned(),
        "Accept: application/vnd.github.raw+json".to_owned(),
        format!(
            "repos/{repo}/contents/{}?ref={base}",
            head_approval::CONFIG_PATH
        ),
    ]) {
        Ok(text) => Ok(Some(text)),
        Err(detail) if detail.contains("404") || detail.contains("Not Found") => Ok(None),
        Err(detail) => Err(format!(
            "{} on {base} unreadable: {detail}",
            head_approval::CONFIG_PATH
        )),
    }
}

/// The commit `sha`'s parents.
fn parents(run_gh: RunGh<'_>, repo: &str, sha: &str) -> Result<Vec<String>, String> {
    let value = gh_json(
        run_gh,
        &api(format!("repos/{repo}/commits/{sha}"), "[.parents[].sha]"),
    )?;
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .ok_or_else(|| format!("commit {} lists no parents", short(sha)))
}

/// The base branch's tip.
///
/// # Errors
///
/// When it cannot be read.
pub fn base_tip(run_gh: RunGh<'_>, repo: &str, base: &str) -> Result<String, String> {
    let tip = run_gh(&api(
        format!("repos/{repo}/git/ref/heads/{base}"),
        ".object.sha",
    ))?;
    let tip = tip.trim().to_ascii_lowercase();
    if tip.len() != 40 {
        return Err(format!("the tip of {base} is unreadable"));
    }
    Ok(tip)
}

/// `(merge, member head)` from `B` up to `head`, oldest first: the
/// first-parent chain must be at most `max` two-parent merges and reach
/// `base_sha` exactly.
///
/// # Errors
///
/// Why the chain is not a rollup.
pub fn chain(
    run_gh: RunGh<'_>,
    repo: &str,
    head: &str,
    base_sha: &str,
    max: usize,
) -> Result<Vec<(String, String)>, String> {
    let mut links = Vec::new();
    let mut sha = head.to_ascii_lowercase();
    while sha != base_sha.to_ascii_lowercase() {
        if links.len() == max {
            return Err(format!(
                "the first-parent chain from {} does not reach the base commit {} within {max} \
                 merges",
                short(head),
                short(base_sha)
            ));
        }
        let parents = parents(run_gh, repo, &sha)?;
        let [first, member] = parents.as_slice() else {
            return Err(format!(
                "commit {} on the rollup's first-parent chain is not a two-parent merge: a \
                 rollup is only merges of member heads",
                short(&sha)
            ));
        };
        links.push((sha.clone(), member.clone()));
        sha.clone_from(first);
    }
    if links.is_empty() {
        return Err("the rollup merges no member".to_owned());
    }
    links.reverse();
    Ok(links)
}

/// One member pull request: the single open pull request on `base` whose head
/// is `head`, other than the rollup `rollup`.
fn member_pr(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    head: &str,
    rollup: u64,
) -> Result<u64, String> {
    let value = gh_json(
        run_gh,
        &api(
            format!("repos/{repo}/commits/{head}/pulls"),
            "[.[] | {number, state, sha: .head.sha, base: .base.ref}]",
        ),
    )?;
    let open: Vec<u64> = value
        .as_array()
        .into_iter()
        .flatten()
        .filter(|pull| {
            pull.get("state").and_then(Value::as_str) == Some("open")
                && pull
                    .get("sha")
                    .and_then(Value::as_str)
                    .is_some_and(|sha| sha.eq_ignore_ascii_case(head))
                && pull.get("base").and_then(Value::as_str) == Some(base)
        })
        .filter_map(|pull| pull.get("number").and_then(Value::as_u64))
        .filter(|number| *number != rollup)
        .collect();
    match open.as_slice() {
        [number] => Ok(*number),
        [] => Err(format!(
            "{} is not the current head of an open pull request on {base}",
            short(head)
        )),
        _ => Err(format!(
            "{} is the head of more than one open pull request ({open:?})",
            short(head)
        )),
    }
}

/// Whether one member's gate lets it into a rollup.
fn member_refusal(pr: u64, gate: &HeadGate, counted: bool) -> Option<String> {
    match gate {
        HeadGate::Approved {
            approval: Approval::Carried { .. },
        } if counted => Some(format!(
            "member #{pr}'s approval is carried from an earlier head; a rollup member needs a \
             verdict naming its exact head (require_counted_approval)"
        )),
        HeadGate::Approved {
            approval: Approval::Rollup { .. },
        } => Some(format!("member #{pr} is itself a rollup")),
        HeadGate::NotRequired | HeadGate::Approved { .. } => None,
        other => Some(format!("member #{pr}: {}", other.explain())),
    }
}

type Files = BTreeMap<String, (String, String, bool)>;

fn touched(files: &Files, sources: &BTreeMap<String, String>) -> BTreeSet<String> {
    files.keys().chain(sources.values()).cloned().collect()
}

fn same_change(
    what: &str,
    expected: &Files,
    expected_sources: &BTreeMap<String, String>,
    found: &Files,
    found_sources: &BTreeMap<String, String>,
) -> Option<String> {
    if let Some(name) = expected
        .keys()
        .collect::<BTreeSet<_>>()
        .symmetric_difference(&found.keys().collect())
        .next()
    {
        return Some(format!(
            "{what} changes {name}, which the member does not, or misses it"
        ));
    }
    for (name, (status, sha, _)) in expected {
        let (found_status, found_sha, _) = &found[name];
        if status != found_status || !sha.eq_ignore_ascii_case(found_sha) {
            return Some(format!(
                "{what} leaves {name} {found_status} at blob {}, not the member's approved \
                 {status} at {}",
                short(found_sha),
                short(sha)
            ));
        }
        if expected_sources.get(name) != found_sources.get(name) {
            return Some(format!("{what} renames {name} from somewhere else"));
        }
    }
    None
}

/// Accept the rollup pull request `pr` at `head`, or say why not. See the
/// module documentation for the rule; `member_gate` judges one member's
/// exact head (its pull request, head), and `carry_excluded` are the base's
/// carry exclusions.
///
/// # Errors
///
/// Why `head` does not land as a rollup.
#[allow(clippy::too_many_lines)]
pub fn accept(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    head: &str,
    config: &Config,
    carry_excluded: &[String],
    member_gate: &dyn Fn(u64, &str) -> Result<HeadGate, String>,
) -> Result<Rollup, String> {
    if !config.rolls_up() {
        return Err(format!(
            "the base does not take rollups ([{TABLE}] enabled = true, mode = \"rollup\")"
        ));
    }
    if let Some(why) = host_stopped(repo) {
        return Err(why);
    }
    let pull = gh_json(
        run_gh,
        &api(
            format!("repos/{repo}/pulls/{pr}"),
            "{ref: .head.ref, sha: .head.sha, base: .base.ref, state: .state, \
             head_repo: .head.repo.full_name}",
        ),
    )?;
    let field = |key: &str| pull.get(key).and_then(Value::as_str).unwrap_or_default();
    if !field("ref").starts_with(BRANCH_PREFIX) {
        return Err(format!(
            "PR #{pr}'s branch is not a {BRANCH_PREFIX}* rollup"
        ));
    }
    if !field("head_repo").eq_ignore_ascii_case(repo) {
        return Err(format!("PR #{pr}'s rollup branch is not in {repo}"));
    }
    if field("state") != "open" || !field("sha").eq_ignore_ascii_case(head) {
        return Err(format!(
            "PR #{pr} is not open at {}; read it again",
            short(head)
        ));
    }
    let base = field("base").to_owned();
    let (union, base_sha, union_sources) =
        head_approval::compare_files(run_gh, repo, &format!("{base}...{head}"))?;
    let base_sha = base_sha.to_ascii_lowercase();
    let tip = base_tip(run_gh, repo, &base)?;
    let links = chain(run_gh, repo, head, &base_sha, config.max_members)?;
    let mut members: Vec<Member> = Vec::new();
    let mut seen: BTreeMap<String, u64> = BTreeMap::new();
    let mut expected_union = Files::new();
    let mut expected_sources = BTreeMap::new();
    let mut previous = base_sha.clone();
    for (merge, member_head) in &links {
        let number = member_pr(run_gh, repo, &base, member_head, pr)?;
        if members.iter().any(|member| member.pr == number) {
            return Err(format!("member #{number} is merged more than once"));
        }
        let gate = member_gate(number, member_head)?;
        if let Some(why) = member_refusal(number, &gate, config.require_counted_approval) {
            return Err(why);
        }
        let (own, own_base, own_sources) =
            head_approval::compare_files(run_gh, repo, &format!("{base_sha}...{member_head}"))?;
        let files = touched(&own, &own_sources);
        for name in &files {
            if let Some(other) = seen.insert(name.clone(), number) {
                return Err(format!(
                    "members #{other} and #{number} both change {name}; rollup members change \
                     disjoint files"
                ));
            }
        }
        if !own_base.eq_ignore_ascii_case(&base_sha) {
            let (moved, _, moved_sources) =
                head_approval::compare_files(run_gh, repo, &format!("{own_base}...{base_sha}"))?;
            if let Some(name) = touched(&moved, &moved_sources).intersection(&files).next() {
                return Err(format!(
                    "the base changed {name} after member #{number}'s merge base {}; the member \
                     must be updated and approved again before it joins a rollup",
                    short(&own_base)
                ));
            }
        }
        let (step, _, step_sources) =
            head_approval::compare_files(run_gh, repo, &format!("{previous}...{merge}"))?;
        if let Some(why) = same_change(
            &format!("the rollup's merge {} of #{number}", short(merge)),
            &own,
            &own_sources,
            &step,
            &step_sources,
        ) {
            return Err(why);
        }
        expected_union.extend(own.clone());
        expected_sources.extend(own_sources);
        members.push(Member {
            pr: number,
            head: member_head.clone(),
            merge: merge.clone(),
            files: files.into_iter().collect(),
            approval: match gate {
                HeadGate::Approved { approval } => Some(approval),
                _ => None,
            },
        });
        previous.clone_from(merge);
    }
    if let Some(why) = same_change(
        &format!("the rollup head {}", short(head)),
        &expected_union,
        &expected_sources,
        &union,
        &union_sources,
    ) {
        return Err(why);
    }
    let all: Vec<&str> = seen.keys().map(String::as_str).collect();
    let excluded: Vec<String> = config
        .exclude_paths
        .iter()
        .chain(carry_excluded)
        .cloned()
        .collect();
    if let Some(why) = head_approval::carry_excluded(all.iter().copied(), &excluded) {
        return Err(format!("a rollup member cannot land this way: {why}"));
    }
    for pattern in &config.hot_files {
        let hot: BTreeSet<u64> = seen
            .iter()
            .filter(|(name, _)| head_approval::glob_match(pattern, name))
            .map(|(_, member)| *member)
            .collect();
        if hot.len() > 1 {
            return Err(format!(
                "members {hot:?} each change a hot file ({pattern}); at most one member of a \
                 rollup may"
            ));
        }
    }
    let present: Vec<&str> = union
        .iter()
        .filter(|(_, (status, _, _))| status != "removed")
        .map(|(name, _)| name.as_str())
        .collect();
    let head_modes = head_approval::tree_modes(run_gh, repo, head, &present)?;
    for member in &members {
        let names: Vec<&str> = member
            .files
            .iter()
            .map(String::as_str)
            .filter(|name| present.contains(name))
            .collect();
        let modes = head_approval::tree_modes(run_gh, repo, &member.head, &names)?;
        for name in names {
            if head_modes.get(name) != modes.get(name) {
                return Err(format!(
                    "{name} has mode {:?} at the rollup head and {:?} at member #{}'s approved \
                     head",
                    head_modes.get(name),
                    modes.get(name),
                    member.pr
                ));
            }
        }
    }
    if tip != base_sha {
        let (moved, _, moved_sources) =
            head_approval::compare_files(run_gh, repo, &format!("{base_sha}...{tip}"))?;
        if let Some(name) = touched(&moved, &moved_sources)
            .iter()
            .find(|name| seen.contains_key(*name))
        {
            return Err(format!(
                "the base changed {name}, which a member changes, after the rollup was formed \
                 on {}; form it again",
                short(&base_sha)
            ));
        }
    }
    Ok(Rollup {
        pr,
        head: head.to_ascii_lowercase(),
        base,
        base_sha,
        base_tip: tip,
        members,
    })
}

/// The rollup acceptance the guard runs on a head no verdict approves:
/// `Ok(None)` when `pr` is not a rollup of a base that takes them, so the
/// gate stands; `Err` with why a rollup does not land.
///
/// # Errors
///
/// Why the rollup does not land, or what could not be read.
pub fn guard_accept(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    head: &str,
    policy: &head_approval::ApprovalPolicy,
    member_gate: &dyn Fn(u64, &str) -> Result<HeadGate, String>,
) -> Result<Option<Rollup>, String> {
    let branch = run_gh(&api(
        format!("repos/{repo}/pulls/{pr}"),
        "[.head.ref, .base.ref] | join(\"\\n\")",
    ))?;
    let mut lines = branch.lines();
    let (Some(branch), Some(base)) = (lines.next(), lines.next()) else {
        return Err(format!("PR #{pr}'s branches are unreadable"));
    };
    if !branch.starts_with(BRANCH_PREFIX) {
        return Ok(None);
    }
    let config = Config::from_text(base_config_text(run_gh, repo, base)?.as_deref())?;
    if !config.rolls_up() {
        return Ok(None);
    }
    accept(
        run_gh,
        repo,
        pr,
        head,
        &config,
        &policy.carry_excluded_paths(),
        member_gate,
    )
    .map(Some)
}

/// Why the base cannot land a rollup by `landing` with a merge commit, or
/// `None`. Squash and rebase flatten the member merges into one commit, so
/// the rollup's history and per-member revert would be lost.
///
/// # Errors
///
/// When the repository or its rules cannot be read.
pub fn merge_method_refusal(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    landing: Landing,
) -> Result<Option<String>, String> {
    let allowed = run_gh(&api(format!("repos/{repo}"), ".allow_merge_commit"))?;
    if allowed.trim() != "true" {
        return Ok(Some(format!(
            "{repo} does not allow merge commits; a rollup lands only with the merge method \
             MERGE (squash or rebase would flatten its member merges)"
        )));
    }
    let rules = gh_json(
        run_gh,
        &api(
            format!("repos/{repo}/rules/branches/{}", encode_path_segment(base)),
            "[.[] | {type, parameters}]",
        ),
    )?;
    let rules = rules.as_array().cloned().unwrap_or_default();
    let queue = rules
        .iter()
        .find(|rule| rule.get("type").and_then(Value::as_str) == Some("merge_queue"));
    match (landing, queue) {
        (Landing::MergeQueue, None) => Ok(Some(format!(
            "{base} has no merge queue; set [{TABLE}] landing = \"direct\" or add one"
        ))),
        (Landing::MergeQueue, Some(rule)) => {
            let method = rule
                .pointer("/parameters/merge_method")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            Ok((!method.eq_ignore_ascii_case("MERGE")).then(|| {
                format!(
                    "{base}'s merge queue merges with {method}; a rollup needs MERGE (squash or \
                     rebase would flatten its member merges)"
                )
            }))
        }
        (Landing::Direct, Some(_)) => Ok(Some(format!(
            "{base} requires its merge queue, so Shipyard cannot merge a rollup directly; set \
             [{TABLE}] landing = \"merge-queue\""
        ))),
        (Landing::Direct, None) => Ok(rules
            .iter()
            .filter(|rule| rule.get("type").and_then(Value::as_str) == Some("pull_request"))
            .find_map(|rule| {
                let methods = rule
                    .pointer("/parameters/allowed_merge_methods")?
                    .as_array()?;
                (!methods.iter().any(|method| {
                    method
                        .as_str()
                        .is_some_and(|m| m.eq_ignore_ascii_case("merge"))
                }))
                .then(|| {
                    format!("{base}'s rules do not allow the merge method MERGE; a rollup needs it")
                })
            })),
    }
}

/// The base's required check names (rulesets and branch protection).
///
/// # Errors
///
/// When the rules cannot be read.
pub fn required_checks(run_gh: RunGh<'_>, repo: &str, base: &str) -> Result<Vec<String>, String> {
    let mut names = BTreeSet::new();
    let rules = gh_json(
        run_gh,
        &api(
            format!("repos/{repo}/rules/branches/{}", encode_path_segment(base)),
            "[.[] | select(.type == \"required_status_checks\") | \
             .parameters.required_status_checks[].context]",
        ),
    )?;
    names.extend(
        rules
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned),
    );
    match run_gh(&api(
        format!(
            "repos/{repo}/branches/{}/protection/required_status_checks",
            encode_path_segment(base)
        ),
        "[.contexts[]]",
    )) {
        Ok(raw) => {
            let value: Value = serde_json::from_str(raw.trim())
                .map_err(|error| format!("invalid JSON: {error}"))?;
            names.extend(
                value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
        }
        Err(detail) if detail.contains("404") || detail.contains("Not Found") => {}
        Err(detail) => {
            return Err(format!(
                "{base}'s branch protection is unreadable: {detail}"
            ));
        }
    }
    Ok(names.into_iter().collect())
}

/// Why the required checks have not all passed on `head`, or `None`.
///
/// # Errors
///
/// When the checks cannot be read.
pub fn checks_refusal(
    run_gh: RunGh<'_>,
    repo: &str,
    head: &str,
    required: &[String],
) -> Result<Option<String>, String> {
    if required.is_empty() {
        return Ok(Some(
            "the base requires no checks, so nothing has tested the rollup; Shipyard lands a \
             rollup directly only after the base's required checks pass on its head"
                .to_owned(),
        ));
    }
    let runs = gh_json(
        run_gh,
        &api(
            format!("repos/{repo}/commits/{head}/check-runs?per_page=100"),
            "[.check_runs[] | {name, status, conclusion}]",
        ),
    )?;
    let statuses = gh_json(
        run_gh,
        &api(
            format!("repos/{repo}/commits/{head}/status"),
            "[.statuses[] | {context, state}]",
        ),
    )?;
    for name in required {
        let run_passed = runs.as_array().into_iter().flatten().any(|run| {
            run.get("name").and_then(Value::as_str) == Some(name)
                && run.get("status").and_then(Value::as_str) == Some("completed")
                && matches!(
                    run.get("conclusion").and_then(Value::as_str),
                    Some("success" | "neutral" | "skipped")
                )
        });
        let status_passed = statuses.as_array().into_iter().flatten().any(|status| {
            status.get("context").and_then(Value::as_str) == Some(name)
                && status.get("state").and_then(Value::as_str) == Some("success")
        });
        if !run_passed && !status_passed {
            return Ok(Some(format!(
                "required check {name} has not passed on the rollup head {}",
                short(head)
            )));
        }
    }
    Ok(None)
}

/// The tested-merge record of a direct landing: the base commit and the
/// rollup head the checks ran on, which is exactly what lands.
#[must_use]
pub fn tested_record(repo: &str, rollup: &Rollup) -> Value {
    serde_json::json!({
        "at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "repo": repo,
        "pr": rollup.pr,
        "base": rollup.base,
        "merged_base": rollup.base_sha,
        "tested_sha": rollup.head,
        "members": rollup.members.iter().map(|member| serde_json::json!({
            "pr": member.pr, "head": member.head, "merge": member.merge,
        })).collect::<Vec<_>>(),
    })
}

/// Land an accepted rollup directly: record what was tested, re-read the
/// base and refuse on any move, then merge the pull request with its head
/// bound and the merge method MERGE. Returns the merge commit.
///
/// The merge goes through `run_gh`, so the guard judges it like any other
/// landing. Nothing here pushes to the base.
///
/// # Errors
///
/// Why it did not land.
pub fn land_direct(
    run_gh: RunGh<'_>,
    repo: &str,
    rollup: &Rollup,
    record: &mut dyn FnMut(&Value) -> Result<(), String>,
) -> Result<String, String> {
    record(&tested_record(repo, rollup))?;
    let tip = base_tip(run_gh, repo, &rollup.base)?;
    if tip != rollup.base_sha {
        return Err(format!(
            "{} moved from {} to {} since the rollup was formed and tested; form it again and \
             run its checks again",
            rollup.base,
            short(&rollup.base_sha),
            short(&tip)
        ));
    }
    let merged = gh_json(
        run_gh,
        &[
            "api".to_owned(),
            "-X".to_owned(),
            "PUT".to_owned(),
            format!("repos/{repo}/pulls/{}/merge", rollup.pr),
            "-f".to_owned(),
            format!("sha={}", rollup.head),
            "-f".to_owned(),
            "merge_method=merge".to_owned(),
        ],
    )?;
    if merged.get("merged").and_then(Value::as_bool) != Some(true) {
        return Err(format!("GitHub did not merge PR #{}: {merged}", rollup.pr));
    }
    merged
        .get("sha")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "GitHub's merge answer names no commit".to_owned())
}

/// Close each member GitHub did not mark merged, saying which merge commit
/// landed it. Returns the members closed.
///
/// # Errors
///
/// When a member cannot be read or closed.
pub fn close_landed_members(
    run_gh: RunGh<'_>,
    repo: &str,
    rollup: &Rollup,
) -> Result<Vec<u64>, String> {
    let mut closed = Vec::new();
    for member in &rollup.members {
        let state = gh_json(
            run_gh,
            &api(
                format!("repos/{repo}/pulls/{}", member.pr),
                "{state, merged}",
            ),
        )?;
        if state.get("merged").and_then(Value::as_bool) == Some(true)
            || state.get("state").and_then(Value::as_str) != Some("open")
        {
            continue;
        }
        run_gh(&[
            "api".to_owned(),
            "-X".to_owned(),
            "POST".to_owned(),
            format!("repos/{repo}/issues/{}/comments", member.pr),
            "-f".to_owned(),
            format!(
                "body=Landed via rollup #{} (merge commit {} of head {}).",
                rollup.pr, member.merge, member.head
            ),
        ])?;
        run_gh(&[
            "api".to_owned(),
            "-X".to_owned(),
            "PATCH".to_owned(),
            format!("repos/{repo}/pulls/{}", member.pr),
            "-f".to_owned(),
            "state=closed".to_owned(),
        ])?;
        closed.push(member.pr);
    }
    Ok(closed)
}

/// The notice comment on a rollup pull request. People read it; the guard
/// never does.
#[must_use]
pub fn notice(members: &[(u64, String)]) -> String {
    let mut body = String::from(
        "Rollup formed by Shipyard. Each member is merged at its exact approved head; the \
         guard checks that from the commits, not from this comment.\n\n",
    );
    for (pr, head) in members {
        let _ = writeln!(body, "{NOTICE_PREFIX} #{pr}@{head}");
    }
    body
}

/// A pull request that might join a rollup.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Candidate {
    /// Its number; lower is older.
    pub pr: u64,
    /// Its head.
    pub head: String,
    /// The files it changes.
    pub files: Vec<String>,
    /// Why it cannot join, or `None` when it is ready.
    pub blocked: Option<String>,
}

/// Which candidates form the next rollup: the oldest ready ones, each
/// changing files no earlier choice changes, none in an excluded path, no
/// two in the same hot glob, at most `max_members`. Returns the chosen
/// numbers and, for each candidate left out, why.
#[must_use]
pub fn form(
    candidates: &[Candidate],
    config: &Config,
    carry_excluded: &[String],
) -> (Vec<u64>, Vec<(u64, String)>) {
    let mut sorted: Vec<&Candidate> = candidates.iter().collect();
    sorted.sort_by_key(|candidate| candidate.pr);
    let excluded: Vec<String> = config
        .exclude_paths
        .iter()
        .chain(carry_excluded)
        .cloned()
        .collect();
    let mut chosen: Vec<&Candidate> = Vec::new();
    let mut skipped = Vec::new();
    for candidate in sorted {
        let why = candidate.blocked.clone().or_else(|| {
            if chosen.len() == config.max_members {
                return Some(format!(
                    "the rollup is full ({} members)",
                    config.max_members
                ));
            }
            head_approval::carry_excluded(candidate.files.iter().map(String::as_str), &excluded)
                .or_else(|| {
                    chosen.iter().find_map(|other| {
                        candidate
                            .files
                            .iter()
                            .find(|name| other.files.contains(name))
                            .map(|name| format!("#{} also changes {name}", other.pr))
                    })
                })
                .or_else(|| {
                    config.hot_files.iter().find_map(|pattern| {
                        let hot = |files: &[String]| {
                            files
                                .iter()
                                .any(|name| head_approval::glob_match(pattern, name))
                        };
                        if !hot(&candidate.files) {
                            return None;
                        }
                        chosen.iter().find(|other| hot(&other.files)).map(|other| {
                            format!("#{} already changes a hot file ({pattern})", other.pr)
                        })
                    })
                })
        });
        match why {
            Some(why) => skipped.push((candidate.pr, why)),
            None => chosen.push(candidate),
        }
    }
    (
        chosen.iter().map(|candidate| candidate.pr).collect(),
        skipped,
    )
}

/// The open pull requests on `base` that might join a rollup, oldest first,
/// at most `limit`: each judged by the review rule at its exact head
/// ([`head_approval::evaluate_member`]) and listed with the files it changes.
///
/// # Errors
///
/// When the pull requests cannot be listed.
pub fn candidates(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    policy: &head_approval::ApprovalPolicy,
    config: &Config,
    limit: usize,
) -> Result<Vec<Candidate>, String> {
    let pulls = gh_json(
        run_gh,
        &api(
            format!(
                "repos/{repo}/pulls?state=open&base={base}&sort=created&direction=asc&per_page=100"
            ),
            "[.[] | {number, sha: .head.sha, ref: .head.ref, draft}]",
        ),
    )?;
    let mut out = Vec::new();
    for pull in pulls.as_array().into_iter().flatten() {
        let (Some(pr), Some(head)) = (
            pull.get("number").and_then(Value::as_u64),
            pull.get("sha").and_then(Value::as_str),
        ) else {
            continue;
        };
        if pull
            .get("ref")
            .and_then(Value::as_str)
            .is_some_and(|branch| branch.starts_with(BRANCH_PREFIX))
        {
            continue;
        }
        if out.len() == limit {
            break;
        }
        let files = head_approval::compare_files(run_gh, repo, &format!("{base}...{head}"))
            .map(|(files, _, sources)| touched(&files, &sources).into_iter().collect());
        let blocked = if pull.get("draft").and_then(Value::as_bool) == Some(true) {
            Some("a draft".to_owned())
        } else {
            match head_approval::evaluate_member(run_gh, repo, pr, head, policy) {
                Ok(gate) => member_refusal(pr, &gate, config.require_counted_approval),
                Err(error) => Some(format!("its review cannot be read: {error}")),
            }
        }
        .or_else(|| {
            files
                .as_ref()
                .err()
                .map(|error| format!("its files cannot be read: {error}"))
        });
        out.push(Candidate {
            pr,
            head: head.to_ascii_lowercase(),
            files: files.unwrap_or_default(),
            blocked,
        });
    }
    Ok(out)
}

/// What to do after a rollup's checks finish.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "next", rename_all = "snake_case")]
pub enum Split {
    /// It passed: land it.
    Land,
    /// One member failed alone: eject it and form the rest again.
    Eject {
        /// The culprit.
        pr: u64,
    },
    /// Test each half as its own rollup.
    Halves {
        /// The older half.
        first: Vec<u64>,
        /// The newer half.
        second: Vec<u64>,
    },
    /// Both halves passed although the whole failed: the failure needs
    /// members from both halves, or is a flake. Quarantine the failing check
    /// and run the whole rollup once more; never land it on the halves'
    /// results.
    SharedCause,
}

/// The next bisect step for `members` after their rollup's run.
#[must_use]
pub fn split(members: &[u64], passed: bool) -> Split {
    if passed {
        return Split::Land;
    }
    if let [pr] = members {
        return Split::Eject { pr: *pr };
    }
    let (first, second) = members.split_at(members.len().div_ceil(2));
    Split::Halves {
        first: first.to_vec(),
        second: second.to_vec(),
    }
}

/// After both halves of a failed rollup ran: which half to bisect further,
/// or [`Split::SharedCause`] when neither failed.
#[must_use]
pub fn after_halves(
    first: &[u64],
    first_passed: bool,
    second: &[u64],
    second_passed: bool,
) -> Split {
    match (first_passed, second_passed) {
        (true, true) => Split::SharedCause,
        (false, _) => split(first, false),
        (true, false) => split(second, false),
    }
}

/// Form a rollup's commits in a local repository without touching its work
/// tree: for each member, `git merge-tree --write-tree` against the previous
/// commit and `git commit-tree` with the member's head as second parent.
/// `git` runs one git command in the repository and returns its stdout.
///
/// # Errors
///
/// A conflict, or a git failure.
pub fn build_commits(
    git: &dyn Fn(&[&str]) -> Result<String, String>,
    base_sha: &str,
    members: &[(u64, String)],
) -> Result<Vec<String>, String> {
    let mut previous = base_sha.to_owned();
    let mut merges = Vec::new();
    for (pr, head) in members {
        let tree = git(&[
            "merge-tree",
            "--write-tree",
            "--no-messages",
            &previous,
            head,
        ])
        .map_err(|error| format!("member #{pr} does not merge cleanly: {error}"))?;
        let tree = tree.lines().next().unwrap_or_default().trim().to_owned();
        if tree.len() != 40 {
            return Err(format!("member #{pr} does not merge cleanly"));
        }
        let message = format!("Merge #{pr} into the rollup\n\n{NOTICE_PREFIX} #{pr}@{head}\n");
        let merge = git(&[
            "commit-tree",
            &tree,
            "-p",
            &previous,
            "-p",
            head,
            "-m",
            &message,
        ])?
        .trim()
        .to_owned();
        merges.push(merge.clone());
        previous = merge;
    }
    Ok(merges)
}

#[cfg(test)]
mod tests;
