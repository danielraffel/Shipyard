//! `SQLite` durability, schema, and inspection helpers.

use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;

use rusqlite::{Connection, TransactionBehavior};

use super::{LedgerStatus, SCHEMA_VERSION, WorkLedgerError, WorkLedgerResult};

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
    if version == 1 {
        return migrate_v1_to_v2(connection);
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
           state TEXT NOT NULL CHECK(state IN ('pending', 'claimed', 'acknowledged', 'uncertain', 'failed')),
           route_ref TEXT NOT NULL,
           payload_digest TEXT NOT NULL,
           transport_receipt_digest TEXT,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           acknowledged_at TEXT
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
         PRAGMA user_version = 2;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Rebuild the only v1 table whose closed constraint changed. `SQLite` cannot
/// alter a `CHECK` constraint in place, so the entire copy occurs atomically.
fn migrate_v1_to_v2(connection: &mut Connection) -> WorkLedgerResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
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
    let foreign_key_violation: i64 =
        transaction.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_key_violation != 0 {
        return Err(WorkLedgerError::Refused(
            "work ledger migration would violate foreign keys".to_owned(),
        ));
    }
    transaction.commit()?;
    Ok(())
}

pub(super) fn verify_supported_schema(connection: &Connection) -> WorkLedgerResult<()> {
    let version = schema_version(connection)?;
    if version != SCHEMA_VERSION {
        return Err(WorkLedgerError::UnsupportedSchema(version));
    }
    Ok(())
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
