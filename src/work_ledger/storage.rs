//! `SQLite` durability, schema, and inspection helpers.

use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;

use rusqlite::{Connection, TransactionBehavior};

use super::{
    LedgerStatus, SCHEMA_VERSION, WorkLedgerError, WorkLedgerResult, random_opaque_ref,
    validate_opaque_ref,
};

/// Status for a ledger that has not been created yet.
#[must_use]
pub fn absent_status() -> LedgerStatus {
    LedgerStatus {
        exists: false,
        schema_version: 0,
        journal_mode: "absent".to_owned(),
        synchronous: "absent".to_owned(),
        foreign_keys: "absent".to_owned(),
        integrity: "not_created".to_owned(),
        work_items: 0,
        pending_wakes: 0,
        uncertain_wakes: 0,
        imports: 0,
        activation_enabled: false,
        dispatch_enabled: false,
    }
}

pub(super) fn configure_durable(connection: &Connection) -> WorkLedgerResult<()> {
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA wal_autocheckpoint = 1000;
         PRAGMA secure_delete = ON;",
    )?;
    Ok(())
}

pub(super) fn create_database_file_no_follow(path: &Path) -> WorkLedgerResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(WorkLedgerError::Refused(
                "ledger database is not a regular file".to_owned(),
            ));
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(WorkLedgerError::Refused(
                    "ledger database is not a regular file".to_owned(),
                ));
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
pub(super) fn protect_ledger_directory(path: &Path) -> WorkLedgerResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(unix)]
pub(super) fn validate_protected_storage(
    directory: &Path,
    database: &Path,
) -> WorkLedgerResult<()> {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    let directory_mode = fs::symlink_metadata(directory)?.permissions().mode() & 0o777;
    if directory_mode != 0o700 {
        return Err(WorkLedgerError::Refused(
            "ledger directory permissions are not 0700".to_owned(),
        ));
    }
    for suffix in ["", "-wal", "-shm"] {
        let path = if suffix.is_empty() {
            database.to_path_buf()
        } else {
            let mut name = OsString::from(database.as_os_str());
            name.push(suffix);
            Path::new(&name).to_path_buf()
        };
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            return Err(WorkLedgerError::Refused(
                "ledger database or sidecar permissions are not protected".to_owned(),
            ));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // Keep one fallible storage API across platforms.
pub(super) fn validate_protected_storage(
    _directory: &Path,
    _database: &Path,
) -> WorkLedgerResult<()> {
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // Keep one fallible storage API across platforms.
pub(super) fn protect_ledger_directory(_path: &Path) -> WorkLedgerResult<()> {
    Ok(())
}

#[allow(clippy::too_many_lines)] // One atomic v1 DDL transaction is easier to audit intact.
pub(super) fn migrate(connection: &mut Connection) -> WorkLedgerResult<()> {
    let version = schema_version(connection)?;
    if version > SCHEMA_VERSION {
        return Err(WorkLedgerError::UnsupportedSchema(version));
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if version == 5 {
        return migrate_v5_to_v6(connection);
    }
    if version == 4 {
        return migrate_v4_to_current(connection);
    }
    let ledger_incarnation_ref = random_opaque_ref("ledger")?;
    if version == 1 {
        return migrate_v1_to_v4(connection, &ledger_incarnation_ref);
    }
    if version == 2 {
        return migrate_v2_to_v4(connection, &ledger_incarnation_ref);
    }
    if version == 3 {
        return migrate_v3_to_v4(connection, &ledger_incarnation_ref);
    }
    if version != 0 {
        return Err(WorkLedgerError::UnsupportedSchema(version));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    transaction.execute_batch(
        "CREATE TABLE work_items (
           id TEXT PRIMARY KEY,
           kind TEXT NOT NULL,
           repo TEXT,
           pr INTEGER,
           head_sha TEXT,
           base_ref TEXT,
           goal_id TEXT,
           goal_generation INTEGER NOT NULL CHECK(goal_generation > 0),
           lane TEXT,
           role TEXT NOT NULL CHECK(role IN ('root', 'coordinator', 'child')),
           owner_id TEXT,
           owner_generation INTEGER NOT NULL CHECK(owner_generation > 0),
           terminal_adapter TEXT,
           agent_adapter TEXT,
           provider_adapter TEXT,
           coordinator_route_ref TEXT,
           repair_route_ref TEXT,
           pr_truth TEXT NOT NULL CHECK(pr_truth IN ('pending', 'succeeded', 'failed', 'unknown')),
           acceptance_truth TEXT NOT NULL CHECK(acceptance_truth IN ('pending', 'succeeded', 'failed', 'unknown')),
           continuation_truth TEXT NOT NULL CHECK(continuation_truth IN ('pending', 'succeeded', 'failed', 'unknown')),
           phase TEXT NOT NULL,
           work_generation INTEGER NOT NULL CHECK(work_generation > 0),
           source_digest TEXT NOT NULL,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL
         );
         CREATE TABLE continuation_contracts (
           work_item_id TEXT PRIMARY KEY REFERENCES work_items(id) ON DELETE RESTRICT,
           success_contract_digest TEXT NOT NULL,
           success_route_ref TEXT,
           success_state TEXT NOT NULL CHECK(success_state IN ('pending', 'acknowledged', 'completed', 'failed')),
           failure_contract_digest TEXT NOT NULL,
           failure_route_ref TEXT,
           failure_state TEXT NOT NULL CHECK(failure_state IN ('pending', 'acknowledged', 'completed', 'failed')),
           revision INTEGER NOT NULL CHECK(revision > 0),
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL
         );
         CREATE TABLE route_records (
           route_ref TEXT PRIMARY KEY,
           work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
           head_sha TEXT NOT NULL,
           work_generation INTEGER NOT NULL CHECK(work_generation > 0),
           owner_ref TEXT NOT NULL,
           owner_generation INTEGER NOT NULL CHECK(owner_generation > 0),
           revision INTEGER NOT NULL CHECK(revision > 0),
           origin_machine_ref TEXT NOT NULL,
           terminal_kind TEXT NOT NULL,
           agent_kind TEXT NOT NULL,
           provider_kind TEXT NOT NULL,
           payload_json BLOB NOT NULL,
           payload_digest TEXT NOT NULL,
           integrity_hash TEXT NOT NULL,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           UNIQUE(work_item_id, owner_generation, revision)
         );
         CREATE TABLE adapter_registry (
           registry_ref TEXT PRIMARY KEY,
           axis TEXT NOT NULL CHECK(axis IN ('terminal', 'agent', 'provider')),
           name TEXT NOT NULL,
           generation INTEGER NOT NULL CHECK(generation > 0),
           revision INTEGER NOT NULL CHECK(revision > 0),
           implementation_digest TEXT NOT NULL,
           configuration_digest TEXT NOT NULL,
           capabilities_digest TEXT NOT NULL,
           state TEXT NOT NULL CHECK(state IN ('active', 'retired')),
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           UNIQUE(axis, name, generation, revision)
         );
         CREATE TABLE events (
           event_id TEXT PRIMARY KEY,
           work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
           work_generation INTEGER NOT NULL,
           owner_generation INTEGER NOT NULL,
           kind TEXT NOT NULL,
           from_state TEXT,
           to_state TEXT NOT NULL,
           payload_digest TEXT NOT NULL,
           created_at TEXT NOT NULL
         );
         CREATE TABLE outbox (
           wake_id TEXT PRIMARY KEY,
           work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
           work_generation INTEGER NOT NULL,
           owner_generation INTEGER NOT NULL,
           state TEXT NOT NULL CHECK(state IN ('pending', 'claimed', 'delivery_started', 'acknowledged', 'uncertain', 'failed')),
           route_ref TEXT NOT NULL,
           payload_digest TEXT NOT NULL,
           claim_id TEXT,
           claimant_ref TEXT,
           claim_attempt INTEGER NOT NULL DEFAULT 0 CHECK(claim_attempt >= 0),
           claim_identity_digest TEXT,
           claim_payload_json BLOB,
           claimed_at TEXT,
           lease_expires_at TEXT,
           delivery_started_at TEXT,
           receipt_kind TEXT CHECK(receipt_kind IN ('accepted', 'definitive_pre_delivery_failure', 'reconciled_not_delivered', 'uncertain')),
           receipt_digest TEXT,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           completed_at TEXT,
           CHECK(
             (state = 'pending'
               AND claim_id IS NULL AND claimant_ref IS NULL
               AND claim_identity_digest IS NULL AND claim_payload_json IS NULL
               AND claimed_at IS NULL AND lease_expires_at IS NULL
               AND delivery_started_at IS NULL
               AND receipt_kind IS NULL AND receipt_digest IS NULL
               AND completed_at IS NULL)
             OR
             (state = 'claimed'
               AND claim_id IS NOT NULL AND claimant_ref IS NOT NULL
               AND claim_attempt > 0 AND claim_identity_digest IS NOT NULL
               AND claim_payload_json IS NOT NULL
               AND claimed_at IS NOT NULL AND lease_expires_at IS NOT NULL
               AND delivery_started_at IS NULL
               AND receipt_kind IS NULL AND receipt_digest IS NULL
               AND completed_at IS NULL)
             OR
             (state = 'delivery_started'
               AND claim_id IS NOT NULL AND claimant_ref IS NOT NULL
               AND claim_attempt > 0 AND claim_identity_digest IS NOT NULL
               AND claim_payload_json IS NOT NULL
               AND claimed_at IS NOT NULL AND lease_expires_at IS NOT NULL
               AND delivery_started_at IS NOT NULL
               AND receipt_kind IS NULL AND receipt_digest IS NULL
               AND completed_at IS NULL)
             OR
             (state = 'acknowledged'
               AND claim_id IS NOT NULL AND claimant_ref IS NOT NULL
               AND claim_attempt > 0 AND claim_identity_digest IS NOT NULL
               AND claim_payload_json IS NOT NULL
               AND claimed_at IS NOT NULL AND lease_expires_at IS NOT NULL
               AND delivery_started_at IS NOT NULL
               AND receipt_kind IS NOT NULL AND receipt_kind = 'accepted'
               AND receipt_digest IS NOT NULL AND completed_at IS NOT NULL)
             OR
             (state = 'uncertain'
               AND claim_id IS NOT NULL AND claimant_ref IS NOT NULL
               AND claim_attempt > 0 AND claim_identity_digest IS NOT NULL
               AND claim_payload_json IS NOT NULL
               AND claimed_at IS NOT NULL AND lease_expires_at IS NOT NULL
               AND delivery_started_at IS NOT NULL
               AND receipt_kind IS NOT NULL AND receipt_kind = 'uncertain'
               AND receipt_digest IS NOT NULL AND completed_at IS NOT NULL)
             OR
             (state = 'failed'
               AND claim_id IS NOT NULL AND claimant_ref IS NOT NULL
               AND claim_attempt > 0 AND claim_identity_digest IS NOT NULL
               AND claim_payload_json IS NOT NULL
               AND claimed_at IS NOT NULL AND lease_expires_at IS NOT NULL
               AND receipt_kind IS NOT NULL
               AND ((delivery_started_at IS NULL
                     AND receipt_kind = 'definitive_pre_delivery_failure')
                    OR (delivery_started_at IS NOT NULL
                        AND receipt_kind = 'reconciled_not_delivered'))
               AND receipt_digest IS NOT NULL AND completed_at IS NOT NULL)
           )
         );
         CREATE TABLE imports (
           source_ref TEXT NOT NULL,
           content_digest TEXT NOT NULL,
           work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
           imported_at TEXT NOT NULL,
           PRIMARY KEY(source_ref, content_digest)
         );
         CREATE TABLE repo_policies (
           repo TEXT PRIMARY KEY,
           primary_platform TEXT NOT NULL,
           compatibility_mode TEXT NOT NULL CHECK(compatibility_mode IN ('independent', 'blocking')),
           compatibility_lanes_json TEXT NOT NULL,
           blocking_rule TEXT NOT NULL CHECK(blocking_rule IN ('declared_dependency_or_shared_integrity', 'all')),
           declared_dependency_lanes_json TEXT NOT NULL,
           revision INTEGER NOT NULL CHECK(revision > 0),
           updated_at TEXT NOT NULL
         );
         CREATE INDEX work_items_nonterminal ON work_items(phase, updated_at, id);
         CREATE INDEX outbox_delivery ON outbox(state, created_at, wake_id);
         PRAGMA user_version = 3;",
    )?;
    upgrade_v3_incarnation(&transaction, &ledger_incarnation_ref)?;
    upgrade_v4_clock(&transaction)?;
    upgrade_v5_route_changes(&transaction)?;
    verify_migration_foreign_keys(&transaction)?;
    transaction.commit()?;
    Ok(())
}

/// Upgrade the inert v2 intent outbox into the typed delivery state machine.
/// v2 had no legal consumer, so a non-pending row has no claim provenance that
/// can be inferred safely and must be reconciled explicitly before upgrading.
fn migrate_v2_to_v4(
    connection: &mut Connection,
    ledger_incarnation_ref: &str,
) -> WorkLedgerResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    upgrade_v2_outbox(&transaction)?;
    upgrade_v3_incarnation(&transaction, ledger_incarnation_ref)?;
    upgrade_v4_clock(&transaction)?;
    upgrade_v5_route_changes(&transaction)?;
    verify_migration_foreign_keys(&transaction)?;
    transaction.commit()?;
    Ok(())
}

#[allow(clippy::too_many_lines)] // Keep the table rebuild auditable as one DDL unit.
fn upgrade_v2_outbox(transaction: &rusqlite::Transaction<'_>) -> WorkLedgerResult<()> {
    let non_pending: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM outbox WHERE state != 'pending'",
        [],
        |row| row.get(0),
    )?;
    if non_pending != 0 {
        return Err(WorkLedgerError::Refused(
            "schema v2 contains non-pending wakes without exact claim provenance; explicit outbox reconciliation is required before v3 migration"
                .to_owned(),
        ));
    }
    transaction.execute_batch(
        "ALTER TABLE outbox RENAME TO outbox_v2;
         CREATE TABLE outbox (
           wake_id TEXT PRIMARY KEY,
           work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
           work_generation INTEGER NOT NULL,
           owner_generation INTEGER NOT NULL,
           state TEXT NOT NULL CHECK(state IN ('pending', 'claimed', 'delivery_started', 'acknowledged', 'uncertain', 'failed')),
           route_ref TEXT NOT NULL,
           payload_digest TEXT NOT NULL,
           claim_id TEXT,
           claimant_ref TEXT,
           claim_attempt INTEGER NOT NULL DEFAULT 0 CHECK(claim_attempt >= 0),
           claim_identity_digest TEXT,
           claim_payload_json BLOB,
           claimed_at TEXT,
           lease_expires_at TEXT,
           delivery_started_at TEXT,
           receipt_kind TEXT CHECK(receipt_kind IN ('accepted', 'definitive_pre_delivery_failure', 'reconciled_not_delivered', 'uncertain')),
           receipt_digest TEXT,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           completed_at TEXT,
           CHECK(
             (state = 'pending'
               AND claim_id IS NULL AND claimant_ref IS NULL
               AND claim_identity_digest IS NULL AND claim_payload_json IS NULL
               AND claimed_at IS NULL AND lease_expires_at IS NULL
               AND delivery_started_at IS NULL
               AND receipt_kind IS NULL AND receipt_digest IS NULL
               AND completed_at IS NULL)
             OR
             (state = 'claimed'
               AND claim_id IS NOT NULL AND claimant_ref IS NOT NULL
               AND claim_attempt > 0 AND claim_identity_digest IS NOT NULL
               AND claim_payload_json IS NOT NULL
               AND claimed_at IS NOT NULL AND lease_expires_at IS NOT NULL
               AND delivery_started_at IS NULL
               AND receipt_kind IS NULL AND receipt_digest IS NULL
               AND completed_at IS NULL)
             OR
             (state = 'delivery_started'
               AND claim_id IS NOT NULL AND claimant_ref IS NOT NULL
               AND claim_attempt > 0 AND claim_identity_digest IS NOT NULL
               AND claim_payload_json IS NOT NULL
               AND claimed_at IS NOT NULL AND lease_expires_at IS NOT NULL
               AND delivery_started_at IS NOT NULL
               AND receipt_kind IS NULL AND receipt_digest IS NULL
               AND completed_at IS NULL)
             OR
             (state = 'acknowledged'
               AND claim_id IS NOT NULL AND claimant_ref IS NOT NULL
               AND claim_attempt > 0 AND claim_identity_digest IS NOT NULL
               AND claim_payload_json IS NOT NULL
               AND claimed_at IS NOT NULL AND lease_expires_at IS NOT NULL
               AND delivery_started_at IS NOT NULL
               AND receipt_kind IS NOT NULL AND receipt_kind = 'accepted'
               AND receipt_digest IS NOT NULL AND completed_at IS NOT NULL)
             OR
             (state = 'uncertain'
               AND claim_id IS NOT NULL AND claimant_ref IS NOT NULL
               AND claim_attempt > 0 AND claim_identity_digest IS NOT NULL
               AND claim_payload_json IS NOT NULL
               AND claimed_at IS NOT NULL AND lease_expires_at IS NOT NULL
               AND delivery_started_at IS NOT NULL
               AND receipt_kind IS NOT NULL AND receipt_kind = 'uncertain'
               AND receipt_digest IS NOT NULL AND completed_at IS NOT NULL)
             OR
             (state = 'failed'
               AND claim_id IS NOT NULL AND claimant_ref IS NOT NULL
               AND claim_attempt > 0 AND claim_identity_digest IS NOT NULL
               AND claim_payload_json IS NOT NULL
               AND claimed_at IS NOT NULL AND lease_expires_at IS NOT NULL
               AND receipt_kind IS NOT NULL
               AND ((delivery_started_at IS NULL
                     AND receipt_kind = 'definitive_pre_delivery_failure')
                    OR (delivery_started_at IS NOT NULL
                        AND receipt_kind = 'reconciled_not_delivered'))
               AND receipt_digest IS NOT NULL AND completed_at IS NOT NULL)
           )
         );
         INSERT INTO outbox
           (wake_id, work_item_id, work_generation, owner_generation, state,
            route_ref, payload_digest, created_at, updated_at)
         SELECT wake_id, work_item_id, work_generation, owner_generation, state,
                route_ref, payload_digest, created_at, updated_at
           FROM outbox_v2;
         DROP TABLE outbox_v2;
         CREATE INDEX outbox_delivery ON outbox(state, created_at, wake_id);
         PRAGMA user_version = 3;",
    )?;
    Ok(())
}

/// Upgrade both v1 tables under one exclusive transaction. Committing v2 before
/// rebuilding the outbox would leave a partially upgraded database after a
/// crash or second-stage failure.
fn migrate_v1_to_v4(
    connection: &mut Connection,
    ledger_incarnation_ref: &str,
) -> WorkLedgerResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    upgrade_v1_registry(&transaction)?;
    upgrade_v2_outbox(&transaction)?;
    upgrade_v3_incarnation(&transaction, ledger_incarnation_ref)?;
    upgrade_v4_clock(&transaction)?;
    upgrade_v5_route_changes(&transaction)?;
    verify_migration_foreign_keys(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v3_to_v4(
    connection: &mut Connection,
    ledger_incarnation_ref: &str,
) -> WorkLedgerResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    upgrade_v3_incarnation(&transaction, ledger_incarnation_ref)?;
    upgrade_v4_clock(&transaction)?;
    upgrade_v5_route_changes(&transaction)?;
    verify_migration_foreign_keys(&transaction)?;
    transaction.commit()?;
    Ok(())
}

/// Bind all durable delivery evidence to one database incarnation. Active v3
/// claims cannot be upgraded because their dispatcher process identity was not
/// recorded and must never be guessed during migration.
fn upgrade_v3_incarnation(
    transaction: &rusqlite::Transaction<'_>,
    ledger_incarnation_ref: &str,
) -> WorkLedgerResult<()> {
    validate_opaque_ref("ledger_incarnation_ref", ledger_incarnation_ref, "ledger")?;
    let active: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM outbox WHERE state != 'pending'",
        [],
        |row| row.get(0),
    )?;
    if active != 0 {
        return Err(WorkLedgerError::Refused(
            "schema v3 contains active wakes without ledger incarnation or dispatcher epoch provenance; explicit outbox reconciliation is required before v4 migration"
                .to_owned(),
        ));
    }
    transaction.execute_batch(
        "CREATE TABLE ledger_metadata (
           singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
           ledger_incarnation_ref TEXT NOT NULL UNIQUE
         );
         ALTER TABLE events ADD COLUMN ledger_incarnation_ref TEXT NOT NULL DEFAULT '';
         ALTER TABLE events ADD COLUMN dispatcher_epoch_ref TEXT;
         ALTER TABLE outbox ADD COLUMN ledger_incarnation_ref TEXT NOT NULL DEFAULT '';
         ALTER TABLE outbox ADD COLUMN dispatcher_epoch_ref TEXT;
         ALTER TABLE outbox ADD COLUMN delivery_start_digest TEXT;",
    )?;
    transaction.execute(
        "INSERT INTO ledger_metadata (singleton, ledger_incarnation_ref) VALUES (1, ?1)",
        [ledger_incarnation_ref],
    )?;
    transaction.execute(
        "UPDATE events SET ledger_incarnation_ref = ?1",
        [ledger_incarnation_ref],
    )?;
    transaction.execute(
        "UPDATE outbox SET ledger_incarnation_ref = ?1",
        [ledger_incarnation_ref],
    )?;
    transaction.execute_batch(
        "CREATE TRIGGER ledger_incarnation_update
         BEFORE UPDATE ON ledger_metadata
         BEGIN SELECT RAISE(ABORT, 'ledger incarnation is immutable'); END;
         CREATE TRIGGER ledger_incarnation_delete
         BEFORE DELETE ON ledger_metadata
         BEGIN SELECT RAISE(ABORT, 'ledger incarnation is immutable'); END;
         CREATE TRIGGER events_incarnation_insert
         BEFORE INSERT ON events
         WHEN NEW.ledger_incarnation_ref !=
                (SELECT ledger_incarnation_ref FROM ledger_metadata WHERE singleton = 1)
         BEGIN SELECT RAISE(ABORT, 'event ledger incarnation mismatch'); END;
         CREATE TRIGGER events_incarnation_update
         BEFORE UPDATE ON events
         WHEN NEW.ledger_incarnation_ref != OLD.ledger_incarnation_ref
           OR ifnull(NEW.dispatcher_epoch_ref, '') != ifnull(OLD.dispatcher_epoch_ref, '')
         BEGIN SELECT RAISE(ABORT, 'event incarnation fields are immutable'); END;
         CREATE TRIGGER outbox_incarnation_insert
         BEFORE INSERT ON outbox
         WHEN NEW.ledger_incarnation_ref !=
                (SELECT ledger_incarnation_ref FROM ledger_metadata WHERE singleton = 1)
              OR NEW.dispatcher_epoch_ref IS NOT NULL
              OR NEW.delivery_start_digest IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'pending wake incarnation shape is invalid'); END;
         CREATE TRIGGER outbox_incarnation_update
         BEFORE UPDATE ON outbox
         WHEN NEW.ledger_incarnation_ref != OLD.ledger_incarnation_ref
              OR NEW.ledger_incarnation_ref !=
                 (SELECT ledger_incarnation_ref FROM ledger_metadata WHERE singleton = 1)
              OR (OLD.state != 'pending' AND NEW.state != 'pending' AND
                  ifnull(NEW.dispatcher_epoch_ref, '') !=
                  ifnull(OLD.dispatcher_epoch_ref, ''))
              OR (OLD.delivery_start_digest IS NOT NULL AND
                  ifnull(NEW.delivery_start_digest, '') != OLD.delivery_start_digest)
              OR (NEW.state = 'pending' AND
                  (NEW.dispatcher_epoch_ref IS NOT NULL OR NEW.delivery_start_digest IS NOT NULL))
              OR (NEW.state = 'claimed' AND
                  (NEW.dispatcher_epoch_ref IS NULL OR NEW.delivery_start_digest IS NOT NULL))
              OR (NEW.state IN ('delivery_started', 'acknowledged', 'uncertain') AND
                  (NEW.dispatcher_epoch_ref IS NULL OR NEW.delivery_start_digest IS NULL))
              OR (NEW.state = 'failed' AND
                  (NEW.dispatcher_epoch_ref IS NULL OR
                   ((NEW.delivery_started_at IS NULL) != (NEW.delivery_start_digest IS NULL))))
         BEGIN SELECT RAISE(ABORT, 'outbox incarnation shape is invalid'); END;",
    )?;
    transaction.pragma_update(None, "user_version", 4)?;
    Ok(())
}

fn migrate_v4_to_current(connection: &mut Connection) -> WorkLedgerResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    upgrade_v4_clock(&transaction)?;
    upgrade_v5_route_changes(&transaction)?;
    verify_migration_foreign_keys(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v5_to_v6(connection: &mut Connection) -> WorkLedgerResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    upgrade_v5_route_changes(&transaction)?;
    verify_migration_foreign_keys(&transaction)?;
    transaction.commit()?;
    Ok(())
}

pub(super) const CLOCK_TIMESTAMP_TABLES: &[(&str, &[(&str, bool)])] = &[
    (
        "work_items",
        &[("created_at", false), ("updated_at", false)],
    ),
    (
        "continuation_contracts",
        &[("created_at", false), ("updated_at", false)],
    ),
    (
        "route_records",
        &[("created_at", false), ("updated_at", false)],
    ),
    (
        "adapter_registry",
        &[("created_at", false), ("updated_at", false)],
    ),
    ("events", &[("created_at", false)]),
    (
        "outbox",
        &[
            ("created_at", false),
            ("updated_at", false),
            ("claimed_at", true),
            ("delivery_started_at", true),
            ("completed_at", true),
        ],
    ),
    ("imports", &[("imported_at", false)]),
    ("repo_policies", &[("updated_at", false)]),
    (
        "route_changes",
        &[
            ("created_at", false),
            ("updated_at", false),
            ("delivery_started_at", true),
            ("completed_at", true),
        ],
    ),
];

const CLOCK_GUARD_TRIGGERS: &[(&str, &str)] = &[
    (
        "ledger_clock_no_delete",
        "CREATE TRIGGER ledger_clock_no_delete
         BEFORE DELETE ON ledger_clock
         BEGIN SELECT RAISE(ABORT, 'ledger clock is durable'); END",
    ),
    (
        "ledger_clock_no_second_insert",
        "CREATE TRIGGER ledger_clock_no_second_insert
         BEFORE INSERT ON ledger_clock
         WHEN EXISTS (SELECT 1 FROM ledger_clock)
         BEGIN SELECT RAISE(ABORT, 'ledger clock is a singleton'); END",
    ),
    (
        "ledger_clock_guard_update",
        "CREATE TRIGGER ledger_clock_guard_update
         BEFORE UPDATE ON ledger_clock
         WHEN NEW.singleton != OLD.singleton
           OR NEW.writer_revision < OLD.writer_revision
           OR NEW.floor_revision < OLD.floor_revision
           OR (NEW.writer_revision - OLD.writer_revision) !=
              (NEW.floor_revision - OLD.floor_revision)
           OR (NEW.observed_floor IS NOT OLD.observed_floor AND
               NEW.writer_revision = OLD.writer_revision)
           OR (OLD.observed_floor IS NOT NULL AND
               (NEW.observed_floor IS NULL OR NEW.observed_floor < OLD.observed_floor))
         BEGIN SELECT RAISE(ABORT, 'ledger clock update is invalid'); END",
    ),
];

fn clock_timestamp_trigger(
    table: &str,
    event: &str,
    column: &str,
    nullable: bool,
) -> (String, String) {
    let name = format!("ledger_clock_{table}_{event}_{column}");
    let when = if nullable {
        format!(" WHEN NEW.{column} IS NOT NULL")
    } else {
        String::new()
    };
    let operation = if event == "update" {
        format!("UPDATE OF {column}")
    } else {
        "INSERT".to_owned()
    };
    let normalized = format!(
        "CASE WHEN substr(NEW.{column}, -1) = 'Z'
           THEN substr(NEW.{column}, 1, length(NEW.{column}) - 1) || '+00:00'
           ELSE NEW.{column} END"
    );
    let normalized_length = format!("length({normalized})");
    let fraction = format!("substr({normalized}, 21, {normalized_length} - 26)");
    let sql = format!(
        "CREATE TRIGGER {name}
         AFTER {operation} ON {table}{when}
         BEGIN
           SELECT CASE WHEN strftime('%Y-%m-%dT%H:%M:%S', NEW.{column}) IS NULL
               OR strftime('%Y-%m-%dT%H:%M:%S', NEW.{column}) != substr(NEW.{column}, 1, 19)
               OR date(substr(NEW.{column}, 1, 10)) != substr(NEW.{column}, 1, 10)
               OR substr(NEW.{column}, 12, 2) GLOB '*[^0-9]*'
               OR CAST(substr(NEW.{column}, 12, 2) AS INTEGER) > 23
               OR substr(NEW.{column}, 15, 2) GLOB '*[^0-9]*'
               OR CAST(substr(NEW.{column}, 15, 2) AS INTEGER) > 59
               OR substr(NEW.{column}, 18, 2) GLOB '*[^0-9]*'
               OR CAST(substr(NEW.{column}, 18, 2) AS INTEGER) > 59
               OR (substr(NEW.{column}, -1) != 'Z'
                   AND substr(NEW.{column}, -6) != '+00:00')
               OR ({normalized_length} != 25 AND
                   ({normalized_length} < 27 OR {normalized_length} > 35))
               OR ({normalized_length} > 25 AND
                   (substr({normalized}, 20, 1) != '.'
                    OR {fraction} = ''
                    OR {fraction} GLOB '*[^0-9]*'))
             THEN RAISE(ABORT, 'invalid observed timestamp') END;
           SELECT CASE WHEN
             (SELECT writer_revision FROM ledger_clock WHERE singleton = 1)
               = 9223372036854775807
             THEN RAISE(ABORT, 'ledger clock revision is exhausted') END;
           UPDATE ledger_clock
           SET observed_floor = CASE
                 WHEN observed_floor IS NULL OR observed_floor < {normalized}
                 THEN {normalized} ELSE observed_floor END,
               writer_revision = writer_revision + 1,
               floor_revision = floor_revision + 1
           WHERE singleton = 1;
         END"
    );
    (name, sql)
}

fn expected_clock_triggers() -> Vec<(String, String)> {
    let mut expected = CLOCK_GUARD_TRIGGERS
        .iter()
        .map(|(name, sql)| ((*name).to_owned(), (*sql).to_owned()))
        .collect::<Vec<_>>();
    for (table, columns) in CLOCK_TIMESTAMP_TABLES {
        for event in ["insert", "update"] {
            for (column, nullable) in *columns {
                expected.push(clock_timestamp_trigger(table, event, column, *nullable));
            }
        }
    }
    expected.sort_by(|left, right| left.0.cmp(&right.0));
    expected
}

fn upgrade_v4_clock(transaction: &rusqlite::Transaction<'_>) -> WorkLedgerResult<()> {
    let floor = super::clock::derive_legacy_floor(transaction)?.map(|value| value.to_rfc3339());
    transaction.execute_batch(
        "CREATE TABLE ledger_clock (
           singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
           observed_floor TEXT,
           writer_revision INTEGER NOT NULL CHECK(writer_revision >= 0),
           floor_revision INTEGER NOT NULL CHECK(floor_revision >= 0 AND floor_revision <= writer_revision)
         );",
    )?;
    for (name, sql) in expected_clock_triggers() {
        if name.starts_with("ledger_clock_route_changes_") {
            continue;
        }
        transaction.execute_batch(&format!("{sql};"))?;
    }
    transaction.execute(
        "INSERT INTO ledger_clock
         (singleton, observed_floor, writer_revision, floor_revision)
         VALUES (1, ?1, 0, 0)",
        [floor],
    )?;
    transaction.pragma_update(None, "user_version", 5)?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn upgrade_v5_route_changes(transaction: &rusqlite::Transaction<'_>) -> WorkLedgerResult<()> {
    transaction.execute_batch(
        "CREATE TABLE route_changes (
           change_id TEXT PRIMARY KEY,
           ledger_incarnation_ref TEXT NOT NULL,
           work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
           head_sha TEXT NOT NULL,
           kind TEXT NOT NULL CHECK(kind IN ('same_session_rebind', 'fresh_owner_transfer')),
           state TEXT NOT NULL CHECK(state IN ('prepared', 'delivery_started', 'applied', 'uncertain', 'failed')),
           source_work_generation INTEGER NOT NULL CHECK(source_work_generation > 0),
           source_owner_ref TEXT NOT NULL,
           source_owner_generation INTEGER NOT NULL CHECK(source_owner_generation > 0),
           source_route_ref TEXT NOT NULL REFERENCES route_records(route_ref) ON DELETE RESTRICT,
           intermediate_work_generation INTEGER NOT NULL CHECK(intermediate_work_generation > 0),
           target_work_generation INTEGER NOT NULL CHECK(target_work_generation > 0),
           target_owner_ref TEXT NOT NULL,
           target_owner_generation INTEGER NOT NULL CHECK(target_owner_generation > 0),
           target_route_ref TEXT NOT NULL,
           recovery_route_ref TEXT,
           dead_session_evidence_digest TEXT,
           checkpoint_digest TEXT,
           claim_integrity TEXT NOT NULL,
           delivery_started_at TEXT,
           adapter_evidence_digest TEXT,
           start_integrity TEXT,
           receipt_kind TEXT CHECK(receipt_kind IN ('accepted', 'definitive_not_delivered', 'uncertain')),
           receipt_evidence_digest TEXT,
           receipt_digest TEXT,
           change_integrity TEXT NOT NULL,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           completed_at TEXT,
           UNIQUE(work_item_id, source_work_generation, source_owner_generation),
           CHECK(source_route_ref != target_route_ref),
           CHECK(
             (kind = 'same_session_rebind'
               AND intermediate_work_generation = source_work_generation
               AND target_work_generation = source_work_generation + 1
               AND target_owner_ref = source_owner_ref
               AND target_owner_generation = source_owner_generation
               AND dead_session_evidence_digest IS NULL AND checkpoint_digest IS NULL
               AND recovery_route_ref IS NULL
               AND delivery_started_at IS NULL AND start_integrity IS NULL
               AND state IN ('prepared', 'applied'))
             OR
             (kind = 'fresh_owner_transfer'
               AND intermediate_work_generation = source_work_generation + 1
               AND target_work_generation = source_work_generation + 2
               AND target_owner_generation = source_owner_generation + 1
               AND dead_session_evidence_digest IS NOT NULL
               AND checkpoint_digest IS NOT NULL
               AND recovery_route_ref IS NOT NULL)
           ),
           CHECK(
             (state = 'prepared'
               AND receipt_kind IS NULL AND receipt_evidence_digest IS NULL
               AND receipt_digest IS NULL AND completed_at IS NULL)
             OR
             (state = 'delivery_started'
               AND kind = 'fresh_owner_transfer'
               AND delivery_started_at IS NOT NULL
               AND adapter_evidence_digest IS NOT NULL AND start_integrity IS NOT NULL
               AND receipt_kind IS NULL AND receipt_evidence_digest IS NULL
               AND receipt_digest IS NULL AND completed_at IS NULL)
             OR
             (state = 'applied'
               AND receipt_kind = 'accepted'
               AND receipt_evidence_digest IS NOT NULL AND receipt_digest IS NOT NULL
               AND (kind = 'same_session_rebind'
                    OR (kind = 'fresh_owner_transfer'
                        AND delivery_started_at IS NOT NULL
                        AND adapter_evidence_digest IS NOT NULL
                        AND start_integrity IS NOT NULL))
               AND completed_at IS NOT NULL)
             OR
             (state = 'uncertain'
               AND kind = 'fresh_owner_transfer'
               AND delivery_started_at IS NOT NULL
               AND adapter_evidence_digest IS NOT NULL AND start_integrity IS NOT NULL
               AND receipt_kind = 'uncertain'
               AND receipt_evidence_digest IS NOT NULL AND receipt_digest IS NOT NULL
               AND completed_at IS NOT NULL)
             OR
             (state = 'failed'
               AND kind = 'fresh_owner_transfer'
               AND delivery_started_at IS NOT NULL
               AND adapter_evidence_digest IS NOT NULL AND start_integrity IS NOT NULL
               AND receipt_kind = 'definitive_not_delivered'
               AND receipt_evidence_digest IS NOT NULL AND receipt_digest IS NOT NULL
               AND completed_at IS NOT NULL)
           )
         );
         CREATE INDEX route_changes_state
           ON route_changes(state, updated_at, change_id);",
    )?;
    for (name, sql) in expected_clock_triggers() {
        if name.starts_with("ledger_clock_route_changes_") {
            transaction.execute_batch(&format!("{sql};"))?;
        }
    }
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn verify_clock_schema(connection: &Connection) -> WorkLedgerResult<()> {
    let mut statement = connection.prepare(
        "SELECT name, sql FROM sqlite_schema
         WHERE type = 'trigger' AND name LIKE 'ledger_clock_%'
         ORDER BY name",
    )?;
    let actual = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if actual != expected_clock_triggers() {
        return Err(WorkLedgerError::Refused(
            "ledger clock trigger schema is incomplete or altered".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn load_ledger_incarnation(connection: &Connection) -> WorkLedgerResult<String> {
    verify_supported_schema(connection)?;
    let mut statement = connection
        .prepare("SELECT ledger_incarnation_ref FROM ledger_metadata WHERE singleton = 1")?;
    let mut rows = statement.query([])?;
    let value: String = rows
        .next()?
        .ok_or_else(|| WorkLedgerError::Refused("ledger incarnation is missing".to_owned()))?
        .get(0)?;
    if rows.next()?.is_some() {
        return Err(WorkLedgerError::Refused(
            "ledger incarnation is not a singleton".to_owned(),
        ));
    }
    validate_opaque_ref("ledger_incarnation_ref", &value, "ledger")?;
    Ok(value)
}

pub(super) fn verify_ledger_incarnation(
    connection: &Connection,
    expected: &str,
) -> WorkLedgerResult<()> {
    if load_ledger_incarnation(connection)? != expected {
        return Err(WorkLedgerError::Refused(
            "ledger incarnation changed after open".to_owned(),
        ));
    }
    Ok(())
}

/// Rebuild the v1 registry whose closed axis constraint changed.
fn upgrade_v1_registry(transaction: &rusqlite::Transaction<'_>) -> WorkLedgerResult<()> {
    let route_count: i64 =
        transaction.query_row("SELECT COUNT(*) FROM route_records", [], |row| row.get(0))?;
    if route_count != 0 {
        return Err(WorkLedgerError::Refused(
            "schema v1 contains route records whose exact agent adapter binding cannot be inferred; explicit route reconciliation is required before v2 migration"
                .to_owned(),
        ));
    }
    transaction.execute_batch(
        "ALTER TABLE adapter_registry RENAME TO adapter_registry_v1;
         CREATE TABLE adapter_registry (
           registry_ref TEXT PRIMARY KEY,
           axis TEXT NOT NULL CHECK(axis IN ('terminal', 'agent', 'provider')),
           name TEXT NOT NULL,
           generation INTEGER NOT NULL CHECK(generation > 0),
           revision INTEGER NOT NULL CHECK(revision > 0),
           implementation_digest TEXT NOT NULL,
           configuration_digest TEXT NOT NULL,
           capabilities_digest TEXT NOT NULL,
           state TEXT NOT NULL CHECK(state IN ('active', 'retired')),
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           UNIQUE(axis, name, generation, revision)
         );
         INSERT INTO adapter_registry
           (registry_ref, axis, name, generation, revision,
            implementation_digest, configuration_digest, capabilities_digest,
            state, created_at, updated_at)
         SELECT registry_ref, axis, name, generation, revision,
                implementation_digest, configuration_digest, capabilities_digest,
                state, created_at, updated_at
           FROM adapter_registry_v1;
         DROP TABLE adapter_registry_v1;
         PRAGMA user_version = 2;",
    )?;
    Ok(())
}

fn verify_migration_foreign_keys(transaction: &rusqlite::Transaction<'_>) -> WorkLedgerResult<()> {
    let foreign_key_violation: i64 =
        transaction.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_key_violation != 0 {
        return Err(WorkLedgerError::Refused(
            "work ledger migration would violate foreign keys".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn verify_supported_schema(connection: &Connection) -> WorkLedgerResult<()> {
    let version = schema_version(connection)?;
    if version != SCHEMA_VERSION {
        return Err(WorkLedgerError::UnsupportedSchema(version));
    }
    verify_clock_schema(connection)
}

pub(super) fn schema_version(connection: &Connection) -> WorkLedgerResult<i64> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

pub(super) fn verify_integrity(connection: &Connection) -> WorkLedgerResult<String> {
    let verdict: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if verdict != "ok" {
        return Err(WorkLedgerError::Refused(format!(
            "integrity check returned {verdict}"
        )));
    }
    Ok(verdict)
}

pub(super) fn synchronous_name(connection: &Connection) -> WorkLedgerResult<String> {
    let value: i64 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    match value {
        0 => Ok("off".to_owned()),
        1 => Ok("normal".to_owned()),
        2 => Ok("full".to_owned()),
        3 => Ok("extra".to_owned()),
        other => Err(WorkLedgerError::Refused(format!(
            "unsupported synchronous mode {other}"
        ))),
    }
}

pub(super) fn count(connection: &Connection, table: &str) -> WorkLedgerResult<u64> {
    let sql = match table {
        "work_items" => "SELECT COUNT(*) FROM work_items",
        "imports" => "SELECT COUNT(*) FROM imports",
        _ => {
            return Err(WorkLedgerError::Refused(
                "unsupported count table".to_owned(),
            ));
        }
    };
    Ok(connection.query_row(sql, [], |row| row.get(0))?)
}

pub(super) fn count_where(
    connection: &Connection,
    table: &str,
    column: &str,
    value: &str,
) -> WorkLedgerResult<u64> {
    if table != "outbox" || column != "state" {
        return Err(WorkLedgerError::Refused(
            "unsupported filtered count".to_owned(),
        ));
    }
    Ok(connection.query_row(
        "SELECT COUNT(*) FROM outbox WHERE state = ?1",
        [value],
        |row| row.get(0),
    )?)
}

#[cfg(unix)]
pub(super) fn protect_database_file(path: &Path) -> WorkLedgerResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // Keep one fallible storage API across platforms.
pub(super) fn protect_database_file(_path: &Path) -> WorkLedgerResult<()> {
    Ok(())
}
