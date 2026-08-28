//! Ledger-owned process-monotonic time with restart regression detection.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rusqlite::Connection;

#[cfg(test)]
use super::WorkLedger;
use super::{WorkLedgerError, WorkLedgerResult};

#[derive(Clone, Debug)]
pub(super) struct LedgerClock {
    state: Arc<Mutex<ClockState>>,
}

#[cfg(test)]
impl WorkLedger {
    pub(super) fn set_manual_time(&self, wall: DateTime<Utc>) -> WorkLedgerResult<()> {
        self.clock.set_manual_wall(wall)
    }
}

#[derive(Debug)]
struct ClockState {
    durable_floor: Option<DateTime<Utc>>,
    last_observed: Option<DateTime<Utc>>,
    restart_regression_floor: Option<DateTime<Utc>>,
    #[cfg(test)]
    manual_wall: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug)]
struct DurableClockRecord {
    observed_floor: Option<DateTime<Utc>>,
    writer_revision: i64,
    floor_revision: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LedgerTime {
    pub(super) timestamp: DateTime<Utc>,
    pub(super) durable_wall_regressed: bool,
    pub(super) restart_wall_regressed: bool,
}

impl LedgerClock {
    pub(super) fn open(connection: &Connection) -> WorkLedgerResult<Self> {
        let record = load_durable_clock(connection)?;
        let durable_floor = reconcile_floor(connection, record)?;
        let wall = Utc::now();
        Ok(Self {
            state: Arc::new(Mutex::new(ClockState {
                durable_floor,
                last_observed: None,
                restart_regression_floor: durable_floor.filter(|floor| wall < *floor),
                #[cfg(test)]
                manual_wall: None,
            })),
        })
    }

    /// Observe and reserve one timestamp inside the caller's write transaction.
    pub(super) fn observe(&self, transaction: &Connection) -> WorkLedgerResult<LedgerTime> {
        let record = load_durable_clock(transaction)?;
        let durable_floor = reconcile_floor(transaction, record)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| WorkLedgerError::Refused("ledger clock lock is poisoned".to_owned()))?;
        state.durable_floor = match (state.durable_floor, durable_floor) {
            (Some(current), Some(durable)) => Some(current.max(durable)),
            (current, None) => current,
            (None, durable) => durable,
        };
        #[cfg(test)]
        let wall = state.manual_wall.unwrap_or_else(Utc::now);
        #[cfg(not(test))]
        let wall = Utc::now();
        let floor = match (state.last_observed, state.durable_floor) {
            (Some(process), Some(durable)) => Some(process.max(durable)),
            (Some(process), None) => Some(process),
            (None, Some(durable)) => Some(durable),
            (None, None) => None,
        };
        if state
            .restart_regression_floor
            .is_some_and(|restart_floor| wall >= restart_floor)
        {
            state.restart_regression_floor = None;
        }
        let restart_wall_regressed = state.restart_regression_floor.is_some();
        let durable_wall_regressed = state
            .durable_floor
            .is_some_and(|durable_floor| wall < durable_floor);
        let timestamp = floor.map_or(wall, |floor| wall.max(floor));
        state.last_observed = Some(timestamp);
        Ok(LedgerTime {
            timestamp,
            durable_wall_regressed,
            restart_wall_regressed,
        })
    }

    /// Verify that every timestamp writer in this transaction advanced the
    /// authoritative floor and its paired revision atomically.
    pub(super) fn finalize(transaction: &Connection) -> WorkLedgerResult<()> {
        let complete: bool = transaction.query_row(
            "SELECT writer_revision = floor_revision
             FROM ledger_clock WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        if !complete {
            return Err(WorkLedgerError::Refused(
                "ledger clock floor revision is not transactionally complete".to_owned(),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn set_manual_wall(&self, wall: DateTime<Utc>) -> WorkLedgerResult<()> {
        self.state
            .lock()
            .map_err(|_| WorkLedgerError::Refused("ledger clock lock is poisoned".to_owned()))?
            .manual_wall = Some(wall);
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn load_durable_floor(
    connection: &Connection,
) -> WorkLedgerResult<Option<DateTime<Utc>>> {
    Ok(load_durable_clock(connection)?.observed_floor)
}

fn load_durable_clock(connection: &Connection) -> WorkLedgerResult<DurableClockRecord> {
    let (value, writer_revision, floor_revision): (Option<String>, i64, i64) = connection
        .query_row(
            "SELECT observed_floor, writer_revision, floor_revision
             FROM ledger_clock WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    let observed_floor = value
        .map(|value| {
            DateTime::parse_from_rfc3339(&value)
                .map(|value| value.with_timezone(&Utc))
                .map_err(|_| WorkLedgerError::Refused("ledger clock floor is invalid".to_owned()))
        })
        .transpose()?;
    if floor_revision > writer_revision || floor_revision < 0 {
        return Err(WorkLedgerError::Refused(
            "ledger clock revisions are invalid".to_owned(),
        ));
    }
    Ok(DurableClockRecord {
        observed_floor,
        writer_revision,
        floor_revision,
    })
}

fn reconcile_floor(
    _connection: &Connection,
    record: DurableClockRecord,
) -> WorkLedgerResult<Option<DateTime<Utc>>> {
    if record.writer_revision != record.floor_revision {
        return Err(WorkLedgerError::Refused(
            "ledger clock floor revision is not transactionally complete".to_owned(),
        ));
    }
    Ok(record.observed_floor)
}

/// Derive the initial v5 floor from every timestamp-writing v4 surface.
pub(super) fn derive_legacy_floor(
    connection: &Connection,
) -> WorkLedgerResult<Option<DateTime<Utc>>> {
    // Lease expiry is a future deadline, not an observation; including it would
    // manufacture rollback whenever a process restarted during a live lease.
    let mut statement = connection.prepare(
        "SELECT observed_at FROM (
           SELECT created_at AS observed_at FROM work_items
           UNION ALL SELECT updated_at FROM work_items
           UNION ALL SELECT created_at FROM continuation_contracts
           UNION ALL SELECT updated_at FROM continuation_contracts
           UNION ALL SELECT created_at FROM route_records
           UNION ALL SELECT updated_at FROM route_records
           UNION ALL SELECT created_at FROM adapter_registry
           UNION ALL SELECT updated_at FROM adapter_registry
           UNION ALL SELECT created_at FROM events
           UNION ALL SELECT created_at FROM outbox
           UNION ALL SELECT updated_at FROM outbox
           UNION ALL SELECT claimed_at FROM outbox WHERE claimed_at IS NOT NULL
           UNION ALL SELECT delivery_started_at FROM outbox WHERE delivery_started_at IS NOT NULL
           UNION ALL SELECT completed_at FROM outbox WHERE completed_at IS NOT NULL
           UNION ALL SELECT imported_at FROM imports
           UNION ALL SELECT updated_at FROM repo_policies
         )",
    )?;
    let mut rows = statement.query([])?;
    let mut floor: Option<DateTime<Utc>> = None;
    while let Some(row) = rows.next()? {
        let value: String = row.get(0)?;
        let observed = DateTime::parse_from_rfc3339(&value)
            .map_err(|_| {
                WorkLedgerError::Refused("ledger contains an invalid observed timestamp".to_owned())
            })?
            .with_timezone(&Utc);
        floor = Some(floor.map_or(observed, |current| current.max(observed)));
    }
    Ok(floor)
}
