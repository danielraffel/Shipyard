//! The shadow ledger for base-poison rules, and the promotion gate it feeds.
//!
//! Every base-poison rule starts in shadow: it reports what it would do and
//! changes nothing. This ledger records each would-fire episode per rule and
//! context, labels it once it ends, and computes whether the rule has earned
//! enforcement ([`gate`]). The rules measured:
//!
//! * `streak`: the detector's verdict (a context failing in consecutive merge
//!   groups that carried different pull requests).
//! * `strict`: `streak`, and every group in it failed with the same first
//!   error line.
//!
//! An episode is labelled by a **PR-free control** (Chromium CQ's "without
//! patch" retry): when a context starts failing merge groups, the steward
//! dispatches the workflow that produces it (`[base_health.control_workflows]`)
//! once on the base branch and reads that run's verdict for the context. The
//! control failing too means the base is broken (a true fire); the control
//! passing means the groups' pull requests were (a false fire). A context
//! with no control workflow, or a control that never reports within
//! [`CONTROL_TIMEOUT_HOURS`], stays unlabelled and counts toward no gate.
//! A false fire costs every pull request it held, for as long as it held
//! them; a true fire saves one gate per held entry.
//!
//! A rule may move from shadow to enforcing only when all of these hold over
//! at least [`MIN_EVENTS`] ended episodes and [`MIN_DAYS`] days, whichever is
//! later: precision at least [`MIN_PRECISION_DELAY`] (rules that delay or
//! remove work), false-fire cost below true-fire savings, and a one-line
//! config flips it back (every rule here is a `[base_health]` key).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::base_poison::RepoPoison;

/// Open episodes, rewritten each pass.
const OPEN_FILE: &str = "base-health/shadow-open.json";
/// Ended episodes, one JSON line each.
const ENDED_FILE: &str = "base-health/shadow-ended.jsonl";
/// Ended episodes a gate needs.
pub(super) const MIN_EVENTS: usize = 10;
/// Days of record a gate needs.
pub(super) const MIN_DAYS: i64 = 14;
/// Precision a rule that delays or removes work needs.
pub(super) const MIN_PRECISION_DELAY: f64 = 0.9;
/// Gate minutes a true fire saves per held entry (one avoided group).
const GATE_MINUTES: f64 = 40.0;
/// How long a control run may take before its episode is left unlabelled.
pub(super) const CONTROL_TIMEOUT_HOURS: i64 = 6;

/// One PR-free control run: the context's workflow dispatched on the base.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct ControlRun {
    /// The workflow file dispatched.
    pub(super) workflow: String,
    /// The base tip it ran on (the branch head when dispatched).
    pub(super) base_sha: String,
    /// RFC 3339.
    pub(super) dispatched_at: String,
    /// The GitHub Actions run this dispatch created, as the dispatch response
    /// reported it (`workflow_run_id`).
    /// Check runs are accepted only when their check suite points at this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) workflow_run_id: Option<u64>,
    /// `success`, `failure`, or `unlabelled`, once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) conclusion: Option<String>,
    /// Why it could not be dispatched or read, if it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

/// A count as a float, saturating (counts here are small).
fn count<T: TryInto<u32>>(value: T) -> f64 {
    f64::from(value.try_into().unwrap_or(u32::MAX))
}

/// One would-fire episode of one rule on one context.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(super) struct Episode {
    /// `owner/repo@base`.
    pub(super) key: String,
    /// The required context.
    pub(super) context: String,
    /// `streak` or `strict`.
    pub(super) rule: String,
    /// RFC 3339.
    pub(super) started_at: String,
    /// RFC 3339 of the last pass that saw it firing.
    pub(super) last_seen_at: String,
    /// The longest streak seen.
    pub(super) max_streak: usize,
    /// Pull requests it really held or paused at any point.
    pub(super) held: BTreeSet<u64>,
    /// Pull requests it would have held or paused (dry-run) at any point.
    #[serde(default)]
    pub(super) would_hold: BTreeSet<u64>,
    /// The control run's label: `true` when the context failed on the
    /// base without the groups' pull requests. `None` while unlabelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) corroborated: Option<bool>,
    /// RFC 3339 once the rule stopped firing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) ended_at: Option<String>,
    /// The PR-free control for this episode's context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) control: Option<ControlRun>,
}

impl Episode {
    /// Every pull request it touched, really or in shadow: the gate's cost
    /// side is the same question either way.
    fn affected(&self) -> usize {
        self.held.union(&self.would_hold).count()
    }

    fn minutes(&self) -> f64 {
        let at = |text: &str| DateTime::parse_from_rfc3339(text).map(|at| at.with_timezone(&Utc));
        match (
            at(&self.started_at),
            at(self.ended_at.as_deref().unwrap_or(&self.last_seen_at)),
        ) {
            (Ok(start), Ok(end)) => count((end - start).num_seconds().max(0)) / 60.0,
            _ => 0.0,
        }
    }
}

/// Which rules fire this pass, by `(context, rule)`, with the streak length.
pub(super) fn firing(poison: &RepoPoison) -> BTreeMap<(String, String), usize> {
    let mut out = BTreeMap::new();
    for context in poison.health.poisoned() {
        let streak = context.groups.len();
        out.insert((context.context.clone(), "streak".to_owned()), streak);
        if context.same_first_error == Some(true) {
            out.insert((context.context.clone(), "strict".to_owned()), streak);
        }
    }
    out
}

/// Advance the open episodes with this pass's reading: extend the ones still
/// firing, open new ones, and mark the rest ended. An ended episode stays
/// open until its control labels it ([`label`]) and is then returned by
/// [`settle`].
pub(super) fn advance(
    open: &mut Vec<Episode>,
    key: &str,
    firing: &BTreeMap<(String, String), usize>,
    held: &BTreeSet<u64>,
    would: &BTreeSet<u64>,
    now: DateTime<Utc>,
) {
    let stamp = now.to_rfc3339();
    for episode in open.iter_mut().filter(|episode| episode.key == key) {
        if episode.ended_at.is_some() {
            continue;
        }
        if let Some(streak) = firing.get(&(episode.context.clone(), episode.rule.clone())) {
            episode.last_seen_at.clone_from(&stamp);
            episode.max_streak = episode.max_streak.max(*streak);
            episode.held.extend(held);
            episode.would_hold.extend(would);
        } else {
            episode.ended_at = Some(stamp.clone());
        }
    }
    for ((context, rule), streak) in firing {
        if !open.iter().any(|episode| {
            episode.key == key
                && episode.ended_at.is_none()
                && &episode.context == context
                && &episode.rule == rule
        }) {
            open.push(Episode {
                key: key.to_owned(),
                context: context.clone(),
                rule: rule.clone(),
                started_at: stamp.clone(),
                last_seen_at: stamp.clone(),
                max_streak: *streak,
                held: held.clone(),
                would_hold: would.clone(),
                corroborated: None,
                ended_at: None,
                control: None,
            });
        }
    }
}

/// Percent-encode a query value (contexts carry spaces and `&`).
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Dispatch a control for each new episode whose context has a control
/// workflow (one per context per episode start, shared by its rules), and
/// read the verdict of every control still waiting. Calls `run_gh` only for
/// episodes that need it.
pub(super) fn label(
    open: &mut [Episode],
    key: &str,
    repo: &str,
    base: &str,
    controls: &BTreeMap<String, String>,
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    now: DateTime<Utc>,
) {
    // A control belongs to one episode start.  The streak and strict rules
    // opened by the same pass share it, but a later episode for the same
    // context must dispatch its own control.
    let mut dispatched: BTreeMap<(String, String), ControlRun> = open
        .iter()
        .filter(|episode| episode.key == key)
        .filter_map(|episode| {
            Some((
                (episode.context.clone(), episode.started_at.clone()),
                episode.control.clone()?,
            ))
        })
        .filter(|(_, control)| control.conclusion.is_none())
        .collect();
    for episode in open.iter_mut().filter(|episode| episode.key == key) {
        if episode.control.is_some() {
            continue;
        }
        let Some(workflow) = controls.get(&episode.context) else {
            continue;
        };
        let control_key = (episode.context.clone(), episode.started_at.clone());
        if let Some(shared) = dispatched.get(&control_key) {
            episode.control = Some(shared.clone());
            continue;
        }
        let mut control = ControlRun {
            workflow: workflow.clone(),
            base_sha: String::new(),
            dispatched_at: String::new(),
            workflow_run_id: None,
            conclusion: None,
            error: None,
        };
        let tip = run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/commits/{base}"),
            "--jq".to_owned(),
            ".sha".to_owned(),
        ]);
        match tip {
            Ok(sha) if sha.trim().len() == 40 => {
                sha.trim().clone_into(&mut control.base_sha);
                // The dispatch boundary is after the base-tip lookup.  A
                // prior run that started just before this request is not the
                // control for this episode.
                control.dispatched_at = now.to_rfc3339();
                // The dispatch response names the exact run it created
                // (`workflow_run_id`).  That ID is the only correlation we
                // trust: a concurrent or manual run of the same workflow on
                // the same base is never selected by searching.  A response
                // without it leaves the episode unlabelled.
                match run_gh(&[
                    "api".to_owned(),
                    "--method".to_owned(),
                    "POST".to_owned(),
                    format!("repos/{repo}/actions/workflows/{workflow}/dispatches"),
                    "-f".to_owned(),
                    format!("ref={base}"),
                ]) {
                    Ok(body) => {
                        if let Some(id) = dispatched_run_id(&body) {
                            control.workflow_run_id = Some(id);
                        } else {
                            control.error = Some(format!(
                                "dispatch returned no workflow_run_id: {:?}",
                                body.trim()
                            ));
                            control.conclusion = Some("unlabelled".to_owned());
                        }
                    }
                    Err(error) => {
                        control.error = Some(error);
                        control.conclusion = Some("unlabelled".to_owned());
                    }
                }
            }
            Ok(other) => {
                control.error = Some(format!("base tip unreadable: {other:?}"));
                control.conclusion = Some("unlabelled".to_owned());
            }
            Err(error) => {
                control.error = Some(error);
                control.conclusion = Some("unlabelled".to_owned());
            }
        }
        dispatched.insert(control_key, control.clone());
        episode.control = Some(control);
    }
    // Read each waiting control once per episode start.
    let mut verdicts: BTreeMap<(String, String), Option<String>> = BTreeMap::new();
    for episode in open.iter_mut().filter(|episode| episode.key == key) {
        let Some(control) = episode.control.as_mut() else {
            continue;
        };
        if control.conclusion.is_none() {
            let verdict_key = (episode.context.clone(), episode.started_at.clone());
            let verdict = verdicts
                .entry(verdict_key)
                .or_insert_with(|| control_verdict(control, &episode.context, repo, run_gh, now))
                .clone();
            control.conclusion = verdict;
        }
        episode.corroborated = match control.conclusion.as_deref() {
            Some("failure") => Some(true),
            Some("success") => Some(false),
            _ => None,
        };
    }
}

/// The run ID a `workflow_dispatch` POST reported, if any.
fn dispatched_run_id(body: &str) -> Option<u64> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("workflow_run_id")
        .and_then(Value::as_u64)
}

/// The control's verdict for `context`, `None` while it is still running.
fn control_verdict(
    control: &ControlRun,
    context: &str,
    repo: &str,
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    now: DateTime<Utc>,
) -> Option<String> {
    let since = DateTime::parse_from_rfc3339(&control.dispatched_at)
        .ok()?
        .with_timezone(&Utc);
    if now - since > chrono::Duration::hours(CONTROL_TIMEOUT_HOURS) {
        return Some("unlabelled".to_owned());
    }
    // Only the run the dispatch itself reported is the control.  A control
    // with no reported run ID (a dispatch response without one, or a ledger
    // entry written before the ID was recorded) is never matched by search.
    let Some(workflow_run_id) = control.workflow_run_id else {
        return Some("unlabelled".to_owned());
    };
    let raw = run_gh(&[
        "api".to_owned(),
        format!(
            "repos/{repo}/commits/{}/check-runs?check_name={}&per_page=20",
            control.base_sha,
            encode(context)
        ),
    ])
    .ok()?;
    let value = serde_json::from_str::<Value>(&raw).ok()?;
    value
        .get("check_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|run| run.get("status").and_then(Value::as_str) == Some("completed"))
        .filter(|run| {
            // Only a run started at or after the dispatch boundary.
            run.get("started_at")
                .and_then(Value::as_str)
                .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                .is_some_and(|at| at.with_timezone(&Utc) >= since)
        })
        .filter(|run| {
            run.get("check_suite")
                .and_then(|suite| suite.get("workflow_run"))
                .and_then(|workflow_run| workflow_run.get("id"))
                .and_then(Value::as_u64)
                == Some(workflow_run_id)
        })
        .filter_map(|run| run.get("conclusion").and_then(Value::as_str))
        .map(|conclusion| match conclusion {
            "success" => "success",
            "failure" => "failure",
            _ => "unlabelled",
        })
        .next()
        .map(str::to_owned)
}

/// Take the episodes that ended and need nothing more: labelled, or with no
/// control to wait for.
pub(super) fn settle(open: &mut Vec<Episode>, key: &str) -> Vec<Episode> {
    let mut done = Vec::new();
    open.retain(|episode| {
        let finished = episode.key == key
            && episode.ended_at.is_some()
            && episode
                .control
                .as_ref()
                .is_none_or(|control| control.conclusion.is_some());
        if finished {
            done.push(episode.clone());
        }
        !finished
    });
    done
}

/// The promotion gate for one rule over its ended episodes.
pub(super) fn gate(rule: &str, ended: &[Episode], now: DateTime<Utc>) -> Value {
    let unlabelled = ended
        .iter()
        .filter(|episode| episode.rule == rule && episode.corroborated.is_none())
        .count();
    // Only a control-labelled episode is evidence either way.
    let episodes = ended
        .iter()
        .filter(|episode| episode.rule == rule && episode.corroborated.is_some())
        .collect::<Vec<_>>();
    let fires = episodes.len();
    let true_fires = episodes
        .iter()
        .filter(|episode| episode.corroborated == Some(true))
        .count();
    let first = episodes
        .iter()
        .filter_map(|episode| DateTime::parse_from_rfc3339(&episode.started_at).ok())
        .map(|at| at.with_timezone(&Utc))
        .min();
    let days = first.map_or(0, |first| (now - first).num_days());
    let precision = (fires > 0).then(|| count(true_fires) / count(fires));
    let false_minutes: f64 = episodes
        .iter()
        .filter(|episode| episode.corroborated == Some(false))
        .map(|episode| episode.minutes() * count(episode.affected().max(1)))
        .sum();
    let true_minutes: f64 = episodes
        .iter()
        .filter(|episode| episode.corroborated == Some(true))
        .map(|episode| GATE_MINUTES * count(episode.affected()))
        .sum();
    let enough = fires >= MIN_EVENTS && days >= MIN_DAYS;
    let eligible = enough
        && precision.is_some_and(|precision| precision >= MIN_PRECISION_DELAY)
        && false_minutes < true_minutes;
    json!({
        "rule": rule,
        "fires": fires,
        "unlabelled": unlabelled,
        "true_fires": true_fires,
        "days": days,
        "precision": precision,
        "false_fire_pr_minutes": false_minutes.round(),
        "true_fire_saved_pr_minutes": true_minutes.round(),
        "eligible_to_enforce": eligible,
        "needs": format!(
            ">= {MIN_EVENTS} control-labelled episodes and >= {MIN_DAYS} days, precision >= {MIN_PRECISION_DELAY}, false-fire cost below savings"
        ),
    })
}

fn read_open(state_dir: &Path) -> Vec<Episode> {
    fs::read_to_string(state_dir.join(OPEN_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn read_ended(state_dir: &Path) -> Vec<Episode> {
    fs::read_to_string(state_dir.join(ENDED_FILE))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Record this pass for `repo@base` and report the gate per rule. A write
/// failure is reported, never fatal: the ledger is measurement only.
#[allow(clippy::too_many_arguments)] // One pass's facts and its one reader.
pub(super) fn record_pass(
    state_dir: &Path,
    repo: &str,
    base: &str,
    poison: Option<&RepoPoison>,
    held: &BTreeSet<u64>,
    would: &BTreeSet<u64>,
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    now: DateTime<Utc>,
) -> Value {
    let key = format!("{}@{base}", repo.to_ascii_lowercase());
    let firing = poison.map(firing).unwrap_or_default();
    // An unread history neither extends nor ends an episode.
    if poison.is_some_and(|poison| {
        matches!(
            poison.health,
            crate::base_health::merge_group::MergeGroupHealth::Unknown { .. }
        )
    }) {
        return json!({"skipped": "merge groups unread"});
    }
    let mut open = read_open(state_dir);
    advance(&mut open, &key, &firing, held, would, now);
    if let Some(poison) = poison {
        label(
            &mut open,
            &key,
            repo,
            base,
            &poison.policy.control_workflows,
            run_gh,
            now,
        );
    }
    let ended = settle(&mut open, &key);
    let mut error = None;
    if let Some(parent) = state_dir.join(OPEN_FILE).parent()
        && let Err(failure) = fs::create_dir_all(parent)
    {
        error = Some(failure.to_string());
    }
    let tmp = state_dir.join(OPEN_FILE).with_extension("json.tmp");
    if let Err(failure) = fs::write(&tmp, serde_json::to_string(&open).unwrap_or_default())
        .and_then(|()| fs::rename(&tmp, state_dir.join(OPEN_FILE)))
    {
        error = Some(failure.to_string());
    }
    if !ended.is_empty() {
        match fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(state_dir.join(ENDED_FILE))
        {
            Ok(mut file) => {
                for episode in &ended {
                    let _ = writeln!(
                        file,
                        "{}",
                        serde_json::to_string(episode).unwrap_or_default()
                    );
                }
            }
            Err(failure) => error = Some(failure.to_string()),
        }
    }
    let history = read_ended(state_dir)
        .into_iter()
        .filter(|episode| episode.key == key)
        .collect::<Vec<_>>();
    let gates = ["streak", "strict"]
        .iter()
        .map(|rule| gate(rule, &history, now))
        .collect::<Vec<_>>();
    json!({
        "open": open.iter().filter(|episode| episode.key == key).collect::<Vec<_>>(),
        "ended_this_pass": ended,
        "gate": gates,
        "error": error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 11, 0, minute, 0)
            .single()
            .expect("test time")
    }

    fn one_episode(
        run_gh: &dyn Fn(&[String]) -> Result<String, String>,
        now: DateTime<Utc>,
        open: &mut Vec<Episode>,
    ) {
        let key = "owner/repo@main";
        let controls = BTreeMap::from([("required".to_owned(), "control.yml".to_owned())]);
        if open.is_empty() {
            let firing = BTreeMap::from([(("required".to_owned(), "streak".to_owned()), 2)]);
            advance(open, key, &firing, &BTreeSet::new(), &BTreeSet::new(), now);
        }
        label(open, key, "owner/repo", "main", &controls, run_gh, now);
    }

    /// An unrelated same-workflow run on the same base is visible (and
    /// already failed) before the dispatched run appears.  Only the run the
    /// dispatch reported may label the episode.
    #[test]
    fn an_unrelated_same_workflow_run_never_labels_the_episode() {
        let base_sha = "a".repeat(40);
        let ours_done = std::cell::Cell::new(false);
        let run_gh = |argv: &[String]| -> Result<String, String> {
            let line = argv.join(" ");
            if line.ends_with("--jq .sha") {
                return Ok(base_sha.clone());
            }
            if line.contains("/dispatches") {
                return Ok(r#"{"workflow_run_id":43,"run_url":"u","html_url":"h"}"#.to_owned());
            }
            if line.contains("/actions/workflows/control.yml/runs?") {
                return Ok(format!(
                    r#"{{"workflow_runs":[{{"id":41,"head_sha":"{base_sha}","created_at":"2026-11-11T00:00:30Z"}}]}}"#
                ));
            }
            if line.contains("/check-runs?") {
                let mut runs = vec![serde_json::json!({
                    "status": "completed", "conclusion": "failure",
                    "started_at": "2026-11-11T00:00:30Z",
                    "check_suite": {"workflow_run": {"id": 41}},
                })];
                if ours_done.get() {
                    runs.push(serde_json::json!({
                        "status": "completed", "conclusion": "success",
                        "started_at": "2026-11-11T00:01:00Z",
                        "check_suite": {"workflow_run": {"id": 43}},
                    }));
                }
                return Ok(serde_json::json!({ "check_runs": runs }).to_string());
            }
            Err(format!("unexpected call: {line}"))
        };
        let mut open = Vec::new();
        one_episode(&run_gh, at(0), &mut open);
        let control = open[0].control.clone().expect("control");
        assert_eq!(control.workflow_run_id, Some(43));
        assert_eq!(control.conclusion, None, "unrelated run 41 must not label");
        assert_eq!(open[0].corroborated, None);
        ours_done.set(true);
        one_episode(&run_gh, at(5), &mut open);
        assert_eq!(open[0].corroborated, Some(false));
    }

    /// A dispatch whose response names no run cannot be correlated, so the
    /// episode is unlabelled and no check run is read for it.
    #[test]
    fn a_dispatch_without_a_run_id_is_left_unlabelled() {
        let base_sha = "b".repeat(40);
        let run_gh = |argv: &[String]| -> Result<String, String> {
            let line = argv.join(" ");
            if line.ends_with("--jq .sha") {
                return Ok(base_sha.clone());
            }
            if line.contains("/dispatches") {
                return Ok(String::new());
            }
            Err(format!(
                "an uncorrelated dispatch must not be searched: {line}"
            ))
        };
        let mut open = Vec::new();
        one_episode(&run_gh, at(0), &mut open);
        let control = open[0].control.clone().expect("control");
        assert_eq!(control.workflow_run_id, None);
        assert_eq!(control.conclusion.as_deref(), Some("unlabelled"));
        assert_eq!(open[0].corroborated, None);
    }

    /// A pending control recorded before run IDs were captured is never
    /// resolved by searching for a run; it becomes unlabelled.
    #[test]
    fn a_legacy_pending_control_without_a_run_id_is_unlabelled() {
        let run_gh = |argv: &[String]| -> Result<String, String> {
            Err(format!("no lookup expected: {}", argv.join(" ")))
        };
        let mut control = ControlRun {
            workflow: "control.yml".to_owned(),
            base_sha: "c".repeat(40),
            dispatched_at: at(0).to_rfc3339(),
            workflow_run_id: None,
            conclusion: None,
            error: None,
        };
        let verdict = control_verdict(&control, "required", "owner/repo", &run_gh, at(5));
        assert_eq!(verdict.as_deref(), Some("unlabelled"));
        control.conclusion = verdict;
        assert!(control.conclusion.is_some());
    }
}
