//! Honour `defect-fix` queue removals: a head removed for a defect is not
//! put back.
//!
//! The ghapp queue-removal guard appends every removal it allows to
//! `$XDG_STATE_HOME/shipyard/queue-removals.jsonl` (default
//! `~/.local/state/shipyard/queue-removals.jsonl`): one JSON line with `at`,
//! `reason`, `note`, `host`, `session` and `targets` (`repo`, `pr`, `head`).
//! On 2026-11-13 the coordinator removed Generous-Corp/vellum#406 at head
//! `ef8a1db2` with reason `defect-fix` at 17:05:03Z; 40 seconds later the
//! steward enqueued the same head again. It was doomed the same way, and the
//! queue lock kept the ready fix from being pushed.
//!
//! Each pass reads the ledger once. A `defect-fix` removal of head X of a
//! pull request refuses every steward admission of X (enqueue, exact-head
//! merge, and the native-arm backstop) until the head changes, reported as
//! `blocked_on` `defect_fix_removed:<reason>` with the repair lane as the
//! next actor. A new head is admitted as usual. The steward's own
//! `dequeue_doomed_groups` act appends its removal to the same ledger
//! ([`append`]), so it is honoured the same way.
//!
//! **An unreadable ledger.** A missing file is no record: nothing was ever
//! removed on this host. A file that cannot be read is logged in the pass
//! report and never stalls the whole queue. While the error is fresh (under
//! [`FRESH_ERROR_MINUTES`] since the last good read) the steward keeps
//! honouring every record it read last time (cached in the steward ledger)
//! and fails closed only for heads it cannot judge: a head whose latest queue
//! removal happened after that last good read, which an unseen record could
//! cover ([`unjudgeable`]; one queue-state read for the pull request, made
//! only while the error is fresh). After that the error is only logged and the cache is still
//! honoured. Malformed lines are skipped and counted, never an error.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The removal reason this module honours.
pub(super) const DEFECT_FIX: &str = "defect-fix";
/// How long after the last good read an unreadable ledger still fails closed
/// for heads it cannot judge.
pub(super) const FRESH_ERROR_MINUTES: i64 = 30;

/// One `defect-fix` removal of one head.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct Removal {
    pub(super) repo: String,
    pub(super) pr: u64,
    /// Lowercase head.
    pub(super) head: String,
    pub(super) reason: String,
    pub(super) at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) host: Option<String>,
}

/// What the steward ledger keeps between passes.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct Cache {
    /// RFC 3339 of the last pass that read the ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) last_good_at: Option<String>,
    /// Every `defect-fix` removal that read found.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) removals: Vec<Removal>,
}

/// This pass's reading.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(super) struct Reading {
    pub(super) removals: Vec<Removal>,
    /// Malformed lines skipped.
    pub(super) skipped: usize,
    /// Why the file could not be read, when it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
    /// The last good read, when the file could not be read this pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) last_good_at: Option<DateTime<Utc>>,
    /// The error is fresh: heads that cannot be judged fail closed.
    pub(super) fresh_error: bool,
}

/// The ledger the ghapp queue-removal guard writes.
#[must_use]
pub(super) fn ledger_path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map_or_else(
            || {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default()
                    .join(".local")
                    .join("state")
            },
            PathBuf::from,
        );
    state.join("shipyard").join("queue-removals.jsonl")
}

/// Parse the ledger text: every `defect-fix` target, and how many lines
/// were skipped.
#[must_use]
pub(super) fn parse(text: &str) -> (Vec<Removal>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            skipped += 1;
            continue;
        };
        let text = |value: &Value, key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|text| !text.is_empty())
        };
        let (Some(reason), Some(at)) = (text(&value, "reason"), text(&value, "at")) else {
            skipped += 1;
            continue;
        };
        if reason != DEFECT_FIX {
            continue;
        }
        for target in value
            .get("targets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(repo), Some(pr), Some(head)) = (
                text(target, "repo"),
                target.get("pr").and_then(Value::as_u64),
                text(target, "head"),
            ) else {
                skipped += 1;
                continue;
            };
            out.push(Removal {
                repo,
                pr,
                head: head.to_ascii_lowercase(),
                reason: reason.clone(),
                at: at.clone(),
                note: text(&value, "note"),
                host: text(&value, "host"),
            });
        }
    }
    (out, skipped)
}

/// Read the ledger at `path` and update the steward's cache.
pub(super) fn read(path: &Path, cache: &mut Cache, now: DateTime<Utc>) -> Reading {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
        Err(error) => {
            let last_good_at = cache
                .last_good_at
                .as_deref()
                .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.with_timezone(&Utc));
            return Reading {
                removals: cache.removals.clone(),
                skipped: 0,
                error: Some(format!("{}: {error}", path.display())),
                fresh_error: last_good_at
                    .is_some_and(|at| now - at < Duration::minutes(FRESH_ERROR_MINUTES)),
                last_good_at,
            };
        }
    };
    let (removals, skipped) = parse(&text.unwrap_or_default());
    cache.last_good_at = Some(now.to_rfc3339());
    cache.removals.clone_from(&removals);
    Reading {
        removals,
        skipped,
        ..Reading::default()
    }
}

impl Reading {
    /// The `defect-fix` removal that blocks this exact head, if any.
    pub(super) fn blocking(&self, repo: &str, pr: u64, head: &str) -> Option<&Removal> {
        self.removals.iter().rev().find(|removal| {
            removal.repo.eq_ignore_ascii_case(repo)
                && removal.pr == pr
                && removal.head.eq_ignore_ascii_case(head)
        })
    }

    /// Report lines.
    pub(super) fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(error) = &self.error {
            lines.push(format!(
                "queue-removals ledger unreadable ({error}); {}",
                if self.fresh_error {
                    "honouring the last good read and holding heads removed since it"
                } else {
                    "honouring the last good read only"
                }
            ));
        }
        if self.skipped > 0 {
            lines.push(format!(
                "queue-removals ledger: {} malformed line(s) skipped",
                self.skipped
            ));
        }
        lines
    }
}

/// The refusal for an admission of `head`: `(token, detail)`.
pub(super) fn refusal(removal: &Removal) -> (String, String) {
    (
        format!("defect_fix_removed:{}", removal.reason),
        format!(
            "head {} was removed from the merge queue for a defect at {}{}{}; it is not put back until the head changes",
            removal.head.get(..12).unwrap_or(&removal.head),
            removal.at,
            removal
                .host
                .as_deref()
                .map_or_else(String::new, |host| format!(" from {host}")),
            removal
                .note
                .as_deref()
                .map_or_else(String::new, |note| format!(": {note}"))
        ),
    )
}

/// Whether, under a fresh read error, the head's latest queue removal in
/// `queue` (a queue-state timeline) happened after the last good read, so
/// an unseen record could cover it.
pub(super) fn unjudgeable(reading: &Reading, queue: &Value, head: &str) -> bool {
    if !reading.fresh_error {
        return false;
    }
    let Some(last_good) = reading.last_good_at else {
        return false;
    };
    // Every non-merge removal of the head, manual ones included.
    crate::head_approval::head_ejections(queue, head)
        .iter()
        .filter_map(|ejection| ejection.at.as_deref())
        .filter_map(|at| DateTime::parse_from_rfc3339(at).ok())
        .any(|at| at.with_timezone(&Utc) > last_good)
}

/// Append the steward's own `defect-fix` removal, in the guard's format.
pub(super) fn append(
    path: &Path,
    repo: &str,
    pr: u64,
    head: &str,
    note: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let line = json!({
        "at": now.to_rfc3339(),
        "reason": DEFECT_FIX,
        "note": note,
        "host": "shipyard-merge-steward",
        "fix_pr": null,
        "targets": [{"repo": repo, "pr": pr, "head": head}],
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    writeln!(file, "{line}").map_err(|error| error.to_string())
}
