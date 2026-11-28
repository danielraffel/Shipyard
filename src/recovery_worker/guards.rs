//! Worker-side guards the store can answer on its own: a durable attempt
//! ledger for the per-hour and per-pull-request caps, and the claim fence
//! that keeps a second request from touching a pull request already claimed.

use super::archive::persist_json;
use super::{DateTime, RecoveryError, RecoveryResult, RecoveryStatus, RecoveryStore, Utc, fs};
use serde::{Deserialize, Serialize};

const LEDGER_FILE: &str = "attempt-ledger";
const LEDGER_SCHEMA: u32 = 1;
const PR_WINDOW_HOURS: i64 = 24;

/// Conservative upper bounds on model attempts, from trusted policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptCaps {
    /// Attempts across every repository in any rolling hour.
    pub per_hour: u32,
    /// Attempts on one pull request, whatever its head, in a rolling day.
    pub per_pr_per_day: u32,
}

/// Whether one more attempt may start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptAdmission {
    /// Within both caps.
    Admitted,
    /// The rolling-hour cap is spent.
    HourCapReached {
        /// Attempts counted in the last hour.
        used: u32,
        /// The configured cap.
        cap: u32,
    },
    /// The pull request's rolling-day cap is spent.
    PrCapReached {
        /// Attempts counted on this pull request in the last day.
        used: u32,
        /// The configured cap.
        cap: u32,
    },
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    schema_version: u32,
    entries: Vec<LedgerEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LedgerEntry {
    repo: String,
    pr: u64,
    at: DateTime<Utc>,
}

impl RecoveryStore {
    /// Check the attempt caps for `repo`/`pr` and, when `record` is set and
    /// the attempt is admitted, count it in the same locked step. An
    /// unreadable ledger is an error: the caller defers rather than guess.
    pub fn admit_attempt(
        &self,
        repo: &str,
        pr: u64,
        now: DateTime<Utc>,
        caps: AttemptCaps,
        record: bool,
    ) -> RecoveryResult<AttemptAdmission> {
        let _lock = self.lock()?;
        let mut ledger = self.read_ledger()?;
        let day_ago = now - chrono::Duration::hours(PR_WINDOW_HOURS);
        ledger.entries.retain(|entry| entry.at > day_ago);
        let hour_ago = now - chrono::Duration::hours(1);
        let used_hour = count(ledger.entries.iter().filter(|entry| entry.at > hour_ago));
        if used_hour >= caps.per_hour {
            return Ok(AttemptAdmission::HourCapReached {
                used: used_hour,
                cap: caps.per_hour,
            });
        }
        let used_pr = count(
            ledger
                .entries
                .iter()
                .filter(|entry| entry.pr == pr && entry.repo.eq_ignore_ascii_case(repo)),
        );
        if used_pr >= caps.per_pr_per_day {
            return Ok(AttemptAdmission::PrCapReached {
                used: used_pr,
                cap: caps.per_pr_per_day,
            });
        }
        if record {
            ledger.entries.push(LedgerEntry {
                repo: repo.to_ascii_lowercase(),
                pr,
                at: now,
            });
            persist_json(&self.root.join(LEDGER_FILE), &ledger)?;
        }
        Ok(AttemptAdmission::Admitted)
    }

    fn read_ledger(&self) -> RecoveryResult<Ledger> {
        let payload = match fs::read(self.root.join(LEDGER_FILE)) {
            Ok(payload) => payload,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Ledger {
                    schema_version: LEDGER_SCHEMA,
                    entries: Vec::new(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let ledger = serde_json::from_slice::<Ledger>(&payload)?;
        if ledger.schema_version != LEDGER_SCHEMA {
            return Err(RecoveryError::SchemaVersion {
                surface: "attempt ledger",
                observed: ledger.schema_version,
            });
        }
        Ok(ledger)
    }

    /// The id of a claimed (running, or claimed but not yet saved) request on
    /// this pull request other than `except_id`, if any. A pull request with
    /// an active claim is never touched by a second request.
    pub fn active_claim_on_pull(
        &self,
        repo: &str,
        pr: u64,
        except_id: &str,
    ) -> RecoveryResult<Option<String>> {
        let _lock = self.lock()?;
        for id in self.record_ids_unlocked()? {
            if id == except_id {
                continue;
            }
            let Some(record) = self.load_unlocked(&id)? else {
                continue;
            };
            if record.receipt.status == RecoveryStatus::Running
                && record.request.pr == pr
                && record.request.repo.eq_ignore_ascii_case(repo)
            {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }
}

fn count<'a>(entries: impl Iterator<Item = &'a LedgerEntry>) -> u32 {
    u32::try_from(entries.count()).unwrap_or(u32::MAX)
}
