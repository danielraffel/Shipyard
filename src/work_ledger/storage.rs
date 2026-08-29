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
        pending_projection_intents: 0,
        quarantined_projection_intents: 0,
        imports: 0,
        protected_objects: 0,
        provider_deliveries: 0,
        agent_ownership: 0,
        activation_epochs: 0,
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
    let mut version = schema_version(connection)?;
    if version > SCHEMA_VERSION {
        return Err(WorkLedgerError::UnsupportedSchema(version));
    }
    verify_open_lineage(connection, version)?;
    if version == 1 {
        migrate_v1_to_v2(connection)?;
        version = 2;
    }
    if version == 2 {
        migrate_v2_to_v3(connection)?;
        version = 3;
    }
    if version == 3 {
        migrate_v3_to_v4(connection)?;
        version = 4;
    }
    if version == 4 {
        migrate_v4_to_v5(connection)?;
        version = 5;
    }
    if version == 5 {
        migrate_v5_to_v6(connection)?;
        version = 6;
    }
    if version == 6 {
        migrate_main_v6_to_v8(connection)?;
        version = 8;
    }
    if version == 8 {
        migrate_v8_to_v9(connection)?;
        version = 9;
    }
    if version == 9 {
        migrate_v9_to_v10(connection)?;
        version = 10;
    }
    if version == 10 {
        migrate_v10_to_v11(connection)?;
        return Ok(());
    }
    if version == SCHEMA_VERSION {
        return verify_schema_identity(connection);
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
           state TEXT NOT NULL CHECK(state IN ('pending', 'claimed', 'delivered',
                                               'acknowledged', 'uncertain', 'failed')),
           route_ref TEXT NOT NULL,
           profile_ref TEXT,
           payload_digest TEXT NOT NULL,
           transport_receipt_digest TEXT,
           provider_delivery_id TEXT UNIQUE
             REFERENCES provider_deliveries(delivery_id) DEFERRABLE INITIALLY DEFERRED,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           acknowledged_at TEXT,
           CHECK(state NOT IN ('delivered', 'acknowledged')
                 OR (provider_delivery_id IS NOT NULL
                     AND profile_ref IS NOT NULL AND length(profile_ref) = 78
                     AND substr(profile_ref, 1, 14) = 'opaque:sha256:'
                     AND substr(profile_ref, 15) NOT GLOB '*[^0-9a-f]*'
                     AND transport_receipt_digest IS NOT NULL
                     AND length(transport_receipt_digest) = 64
                     AND transport_receipt_digest NOT GLOB '*[^0-9a-f]*'
                     AND ((state = 'delivered' AND acknowledged_at IS NULL)
                          OR (state = 'acknowledged' AND acknowledged_at IS NOT NULL))))
         );
         CREATE TABLE wake_attempts (
           wake_id TEXT NOT NULL REFERENCES outbox(wake_id) ON DELETE RESTRICT,
           attempt INTEGER NOT NULL CHECK(attempt > 0),
           state TEXT NOT NULL CHECK(state IN ('claimed', 'delivered', 'acknowledged',
                                               'retry', 'uncertain', 'failed')),
           adapter_id TEXT NOT NULL,
           idempotent INTEGER NOT NULL CHECK(idempotent IN (0, 1)),
           outcome_digest TEXT,
           started_at TEXT NOT NULL,
           finished_at TEXT,
           CHECK(state != 'delivered'
                 OR (outcome_digest IS NOT NULL AND length(outcome_digest) = 64
                     AND outcome_digest NOT GLOB '*[^0-9a-f]*'
                     AND finished_at IS NOT NULL)),
           PRIMARY KEY(wake_id, attempt)
         );
         CREATE TABLE wake_claim_epochs (
           wake_id TEXT NOT NULL,
           attempt INTEGER NOT NULL,
           epoch INTEGER NOT NULL CHECK(epoch > 0),
           owner_ref TEXT NOT NULL,
           kind TEXT NOT NULL CHECK(kind IN ('claim', 'recovery')),
           acquired_at TEXT NOT NULL,
           PRIMARY KEY(wake_id, attempt, epoch),
           FOREIGN KEY(wake_id, attempt) REFERENCES wake_attempts(wake_id, attempt)
             ON DELETE RESTRICT
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
         CREATE INDEX wake_attempts_recovery ON wake_attempts(state, started_at, wake_id);
         CREATE INDEX wake_claim_epoch_owner ON wake_claim_epochs(owner_ref, epoch);
         CREATE TABLE protected_objects (
           object_ref TEXT PRIMARY KEY
             CHECK(length(object_ref) = 67 AND substr(object_ref, 1, 3) = 'po_'
                   AND substr(object_ref, 4) NOT GLOB '*[^0-9a-f]*'),
           work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
           kind TEXT NOT NULL CHECK(kind IN ('launch_profile', 'provider_request',
                                             'provider_receipt', 'agent_receipt')),
           profile_ref TEXT,
           storage_name TEXT NOT NULL UNIQUE
             CHECK(length(storage_name) = 76
                   AND substr(storage_name, 1, 7) = 'object-'
                   AND substr(storage_name, 8, 64) NOT GLOB '*[^0-9a-f]*'
                   AND substr(storage_name, 72, 5) = '.blob'),
           content_digest TEXT NOT NULL
             CHECK(length(content_digest) = 64
                   AND content_digest NOT GLOB '*[^0-9a-f]*'),
           byte_length INTEGER NOT NULL CHECK(byte_length >= 0 AND byte_length <= 1048576),
           created_at TEXT NOT NULL CHECK(length(created_at) >= 20),
           CHECK((kind = 'launch_profile' AND profile_ref IS NOT NULL
                  AND length(profile_ref) = 78
                  AND substr(profile_ref, 1, 14) = 'opaque:sha256:'
                  AND substr(profile_ref, 15) NOT GLOB '*[^0-9a-f]*')
                 OR (kind != 'launch_profile' AND profile_ref IS NULL)),
           UNIQUE(work_item_id, profile_ref)
         );
         CREATE TABLE activation_epochs (
           activation_id TEXT PRIMARY KEY
             CHECK(length(activation_id) = 67 AND substr(activation_id, 1, 3) = 'ae_'
                   AND substr(activation_id, 4) NOT GLOB '*[^0-9a-f]*'),
           work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
           work_generation INTEGER NOT NULL CHECK(work_generation > 0),
           owner_generation INTEGER NOT NULL CHECK(owner_generation > 0),
           epoch INTEGER NOT NULL CHECK(epoch > 0),
           owner_ref TEXT NOT NULL CHECK(length(owner_ref) BETWEEN 65 AND 128),
           state TEXT NOT NULL CHECK(state IN ('active', 'released', 'superseded')),
           acquired_at TEXT NOT NULL CHECK(length(acquired_at) >= 20),
           released_at TEXT,
           CHECK((state = 'active' AND released_at IS NULL)
                 OR (state != 'active' AND released_at IS NOT NULL)),
           UNIQUE(work_item_id, work_generation, owner_generation, epoch)
         );
         CREATE TABLE provider_deliveries (
           delivery_id TEXT PRIMARY KEY
             CHECK(length(delivery_id) = 67 AND substr(delivery_id, 1, 3) = 'pd_'
                   AND substr(delivery_id, 4) NOT GLOB '*[^0-9a-f]*'),
           wake_id TEXT NOT NULL,
           attempt INTEGER NOT NULL CHECK(attempt > 0),
           activation_id TEXT NOT NULL REFERENCES activation_epochs(activation_id) ON DELETE RESTRICT,
           provider_id TEXT NOT NULL CHECK(length(provider_id) BETWEEN 1 AND 512),
           adapter_id TEXT NOT NULL CHECK(length(adapter_id) BETWEEN 1 AND 512),
           idempotency_key TEXT NOT NULL UNIQUE CHECK(length(idempotency_key) BETWEEN 1 AND 512),
           request_object_ref TEXT NOT NULL REFERENCES protected_objects(object_ref) ON DELETE RESTRICT,
           receipt_object_ref TEXT REFERENCES protected_objects(object_ref) ON DELETE RESTRICT,
           state TEXT NOT NULL CHECK(state IN ('prepared', 'launched', 'delivered',
                                               'retry', 'uncertain', 'failed')),
           created_at TEXT NOT NULL CHECK(length(created_at) >= 20),
           updated_at TEXT NOT NULL CHECK(length(updated_at) >= 20),
           delivered_at TEXT,
           CHECK((state = 'delivered' AND receipt_object_ref IS NOT NULL
                  AND delivered_at IS NOT NULL)
                 OR (state != 'delivered' AND delivered_at IS NULL)),
           UNIQUE(wake_id, attempt),
           FOREIGN KEY(wake_id, attempt) REFERENCES wake_attempts(wake_id, attempt)
             ON DELETE RESTRICT
         );
         CREATE TABLE agent_ownership (
           ownership_id TEXT PRIMARY KEY
             CHECK(length(ownership_id) = 67 AND substr(ownership_id, 1, 3) = 'ao_'
                   AND substr(ownership_id, 4) NOT GLOB '*[^0-9a-f]*'),
           work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
           work_generation INTEGER NOT NULL CHECK(work_generation > 0),
           owner_generation INTEGER NOT NULL CHECK(owner_generation > 0),
           delivery_id TEXT NOT NULL UNIQUE REFERENCES provider_deliveries(delivery_id) ON DELETE RESTRICT,
           launch_profile_object_ref TEXT NOT NULL REFERENCES protected_objects(object_ref) ON DELETE RESTRICT,
           context_receipt_object_ref TEXT REFERENCES protected_objects(object_ref) ON DELETE RESTRICT,
           state TEXT NOT NULL CHECK(state IN ('pending', 'acknowledged', 'returned',
                                               'uncertain', 'failed')),
           context_receipt_digest TEXT,
           created_at TEXT NOT NULL CHECK(length(created_at) >= 20),
           updated_at TEXT NOT NULL CHECK(length(updated_at) >= 20),
           acknowledged_at TEXT,
           returned_at TEXT,
           CHECK((state IN ('acknowledged', 'returned')
                  AND context_receipt_digest IS NOT NULL
                  AND length(context_receipt_digest) = 64
                  AND context_receipt_digest NOT GLOB '*[^0-9a-f]*'
                  AND context_receipt_object_ref IS NOT NULL
                  AND acknowledged_at IS NOT NULL)
                 OR (state NOT IN ('acknowledged', 'returned')
                     AND context_receipt_digest IS NULL
                     AND context_receipt_object_ref IS NULL
                     AND acknowledged_at IS NULL)),
           CHECK((state = 'returned' AND returned_at IS NOT NULL)
                 OR (state != 'returned' AND returned_at IS NULL)),
           UNIQUE(work_item_id, work_generation, owner_generation)
         );
         CREATE INDEX protected_objects_work_kind
           ON protected_objects(work_item_id, kind, created_at);
         CREATE TRIGGER protected_object_capacity
         BEFORE INSERT ON protected_objects
         WHEN (SELECT COUNT(*) FROM protected_objects) >= 4096
           OR (SELECT COALESCE(SUM(byte_length), 0) FROM protected_objects)
                + NEW.byte_length > 16777216
         BEGIN SELECT RAISE(ABORT, 'protected object store capacity exceeded'); END;
         CREATE TRIGGER protected_object_immutable
         BEFORE UPDATE ON protected_objects
         BEGIN SELECT RAISE(ABORT, 'protected object metadata is immutable'); END;
         CREATE TRIGGER protected_object_no_delete
         BEFORE DELETE ON protected_objects
         BEGIN SELECT RAISE(ABORT, 'protected objects cannot be deleted'); END;
         CREATE INDEX activation_epochs_active
           ON activation_epochs(state, acquired_at, work_item_id);
         CREATE UNIQUE INDEX activation_epochs_one_active
           ON activation_epochs(work_item_id) WHERE state = 'active';
         CREATE TRIGGER activation_epoch_identity_immutable
         BEFORE UPDATE OF activation_id, work_item_id, work_generation, owner_generation, epoch,
                          owner_ref, acquired_at ON activation_epochs
         BEGIN SELECT RAISE(ABORT, 'activation epoch identity is immutable'); END;
         CREATE TRIGGER activation_epoch_no_delete
         BEFORE DELETE ON activation_epochs
         BEGIN SELECT RAISE(ABORT, 'activation epochs cannot be deleted'); END;
         CREATE TRIGGER activation_epoch_release_fence
         BEFORE UPDATE OF state, released_at ON activation_epochs
         WHEN (OLD.state = 'active' AND NEW.state = 'active'
               AND OLD.released_at IS NOT NEW.released_at)
           OR (OLD.state != 'active'
               AND (OLD.state != NEW.state OR OLD.released_at IS NOT NEW.released_at))
           OR (OLD.state != NEW.state
               AND (OLD.state != 'active' OR NEW.state NOT IN ('released', 'superseded')))
           OR (OLD.state = 'active' AND NEW.state != 'active' AND EXISTS (
               SELECT 1 FROM provider_deliveries delivery
               WHERE delivery.activation_id = OLD.activation_id
                 AND delivery.state IN ('prepared', 'launched')
           ))
         BEGIN SELECT RAISE(ABORT, 'activation epoch transition is unsafe'); END;
         CREATE INDEX provider_deliveries_state
           ON provider_deliveries(state, updated_at, wake_id);
         CREATE INDEX agent_ownership_state
           ON agent_ownership(state, updated_at, work_item_id);
         CREATE TRIGGER provider_delivery_insert_fence
         BEFORE INSERT ON provider_deliveries
         WHEN NOT EXISTS (
           SELECT 1 FROM wake_attempts attempt
           JOIN outbox wake ON wake.wake_id = attempt.wake_id
           JOIN activation_epochs activation
             ON activation.activation_id = NEW.activation_id
           WHERE attempt.wake_id = NEW.wake_id AND attempt.attempt = NEW.attempt
             AND attempt.adapter_id = NEW.adapter_id
             AND activation.state = 'active'
             AND activation.work_item_id = wake.work_item_id
             AND activation.work_generation = wake.work_generation
             AND activation.owner_generation = wake.owner_generation
         )
         BEGIN SELECT RAISE(ABORT, 'provider delivery authority mismatch'); END;
         CREATE TRIGGER provider_delivery_identity_immutable
         BEFORE UPDATE OF delivery_id, wake_id, attempt, activation_id, provider_id, adapter_id,
                          idempotency_key, request_object_ref ON provider_deliveries
         BEGIN SELECT RAISE(ABORT, 'provider delivery identity is immutable'); END;
         CREATE TRIGGER provider_delivery_no_delete
         BEFORE DELETE ON provider_deliveries
         BEGIN SELECT RAISE(ABORT, 'provider deliveries cannot be deleted'); END;
         CREATE TRIGGER provider_delivery_state_fence_insert
         BEFORE INSERT ON provider_deliveries
         WHEN NOT EXISTS (
           SELECT 1 FROM wake_attempts attempt
           JOIN outbox wake ON wake.wake_id = attempt.wake_id
           JOIN protected_objects request ON request.object_ref = NEW.request_object_ref
           JOIN activation_epochs activation ON activation.activation_id = NEW.activation_id
           LEFT JOIN protected_objects receipt ON receipt.object_ref = NEW.receipt_object_ref
           WHERE attempt.wake_id = NEW.wake_id AND attempt.attempt = NEW.attempt
             AND attempt.adapter_id = NEW.adapter_id
             AND request.kind = 'provider_request'
             AND request.work_item_id = wake.work_item_id
             AND activation.work_item_id = wake.work_item_id
             AND activation.work_generation = wake.work_generation
             AND activation.owner_generation = wake.owner_generation
             AND ((NEW.state IN ('prepared', 'launched') AND activation.state = 'active'
                   AND attempt.state = 'claimed')
                  OR (NEW.state = 'delivered' AND attempt.state = 'delivered'
                      AND receipt.kind = 'provider_receipt'
                      AND receipt.work_item_id = wake.work_item_id
                      AND receipt.content_digest = attempt.outcome_digest)
                  OR (NEW.state IN ('retry', 'uncertain', 'failed')
                      AND attempt.state = NEW.state))
         )
         BEGIN SELECT RAISE(ABORT, 'provider delivery state mismatch'); END;
         CREATE TRIGGER provider_delivery_state_fence_update
         BEFORE UPDATE OF state, receipt_object_ref, delivered_at ON provider_deliveries
         WHEN NOT EXISTS (
           SELECT 1 FROM wake_attempts attempt
           JOIN outbox wake ON wake.wake_id = attempt.wake_id
           JOIN protected_objects request ON request.object_ref = NEW.request_object_ref
           JOIN activation_epochs activation ON activation.activation_id = NEW.activation_id
           LEFT JOIN protected_objects receipt ON receipt.object_ref = NEW.receipt_object_ref
           WHERE attempt.wake_id = NEW.wake_id AND attempt.attempt = NEW.attempt
             AND attempt.adapter_id = NEW.adapter_id
             AND request.kind = 'provider_request'
             AND request.work_item_id = wake.work_item_id
             AND activation.work_item_id = wake.work_item_id
             AND activation.work_generation = wake.work_generation
             AND activation.owner_generation = wake.owner_generation
             AND ((NEW.state IN ('prepared', 'launched') AND activation.state = 'active'
                   AND attempt.state = 'claimed')
                  OR (NEW.state = 'delivered' AND attempt.state = 'delivered'
                      AND receipt.kind = 'provider_receipt'
                      AND receipt.work_item_id = wake.work_item_id
                      AND receipt.content_digest = attempt.outcome_digest)
                  OR (NEW.state IN ('retry', 'uncertain', 'failed')
                      AND attempt.state = NEW.state))
         )
         BEGIN SELECT RAISE(ABORT, 'provider delivery state mismatch'); END;
         CREATE TRIGGER agent_ownership_insert_fence
         BEFORE INSERT ON agent_ownership
         WHEN NOT EXISTS (
           SELECT 1 FROM provider_deliveries delivery
           JOIN outbox wake ON wake.wake_id = delivery.wake_id
           JOIN protected_objects profile
             ON profile.object_ref = NEW.launch_profile_object_ref
           WHERE delivery.delivery_id = NEW.delivery_id
             AND delivery.state = 'delivered'
             AND wake.work_item_id = NEW.work_item_id
             AND wake.work_generation = NEW.work_generation
             AND wake.owner_generation = NEW.owner_generation
             AND profile.work_item_id = NEW.work_item_id
             AND profile.kind = 'launch_profile'
             AND profile.profile_ref = wake.profile_ref
             AND profile.content_digest = wake.payload_digest
             AND (NEW.state NOT IN ('acknowledged', 'returned') OR EXISTS (
               SELECT 1 FROM protected_objects receipt
               WHERE receipt.object_ref = NEW.context_receipt_object_ref
                 AND receipt.work_item_id = NEW.work_item_id
                 AND receipt.kind = 'agent_receipt'
                 AND receipt.content_digest = NEW.context_receipt_digest
             ))
         )
         BEGIN SELECT RAISE(ABORT, 'agent ownership authority mismatch'); END;
         CREATE TRIGGER agent_ownership_identity_immutable
         BEFORE UPDATE OF ownership_id, work_item_id, work_generation, owner_generation,
                          delivery_id, launch_profile_object_ref, created_at ON agent_ownership
         BEGIN SELECT RAISE(ABORT, 'agent ownership identity is immutable'); END;
         CREATE TRIGGER agent_ownership_no_delete
         BEFORE DELETE ON agent_ownership
         BEGIN SELECT RAISE(ABORT, 'agent ownership cannot be deleted'); END;
         CREATE TRIGGER agent_ownership_context_fence
         BEFORE UPDATE OF state, context_receipt_digest, context_receipt_object_ref,
                          acknowledged_at, returned_at ON agent_ownership
         WHEN NEW.state IN ('acknowledged', 'returned') AND NOT EXISTS (
           SELECT 1 FROM protected_objects receipt
           WHERE receipt.object_ref = NEW.context_receipt_object_ref
             AND receipt.work_item_id = NEW.work_item_id
             AND receipt.kind = 'agent_receipt'
             AND receipt.content_digest = NEW.context_receipt_digest
         )
         BEGIN SELECT RAISE(ABORT, 'agent ownership receipt mismatch'); END;
         CREATE TRIGGER agent_ownership_state_fence
         BEFORE UPDATE OF state ON agent_ownership
         WHEN (OLD.state = 'pending' AND NEW.state NOT IN ('pending', 'acknowledged',
                                                           'uncertain', 'failed'))
            OR (OLD.state = 'acknowledged' AND NEW.state NOT IN ('acknowledged', 'returned'))
            OR (OLD.state = 'uncertain'
                AND NEW.state NOT IN ('uncertain', 'acknowledged', 'failed'))
            OR (OLD.state IN ('returned', 'failed') AND NEW.state != OLD.state)
         BEGIN SELECT RAISE(ABORT, 'agent ownership transition is not monotonic'); END;
         CREATE TRIGGER agent_ownership_receipt_immutable
         BEFORE UPDATE OF context_receipt_object_ref, context_receipt_digest,
                          acknowledged_at ON agent_ownership
         WHEN OLD.state IN ('acknowledged', 'returned')
           AND (OLD.context_receipt_object_ref IS NOT NEW.context_receipt_object_ref
                OR OLD.context_receipt_digest IS NOT NEW.context_receipt_digest
                OR OLD.acknowledged_at IS NOT NEW.acknowledged_at)
         BEGIN SELECT RAISE(ABORT, 'agent ownership receipt is immutable'); END;
         CREATE TRIGGER outbox_acknowledged_fence
         BEFORE UPDATE OF state ON outbox
         WHEN NEW.state = 'acknowledged' AND NOT EXISTS (
           SELECT 1 FROM agent_ownership ownership
           WHERE ownership.delivery_id = NEW.provider_delivery_id
             AND ownership.state IN ('acknowledged', 'returned')
             AND ownership.work_item_id = NEW.work_item_id
         )
         BEGIN SELECT RAISE(ABORT, 'wake acknowledgement lacks agent ownership'); END;
         CREATE TRIGGER outbox_acknowledged_insert_fence
         BEFORE INSERT ON outbox WHEN NEW.state = 'acknowledged'
         BEGIN SELECT RAISE(ABORT, 'wake cannot be created acknowledged'); END;
         CREATE TRIGGER wake_attempt_acknowledged_insert_fence
         BEFORE INSERT ON wake_attempts WHEN NEW.state = 'acknowledged'
         BEGIN SELECT RAISE(ABORT, 'wake attempt cannot be created acknowledged'); END;
         CREATE TRIGGER wake_attempt_acknowledged_update_fence
         BEFORE UPDATE OF state ON wake_attempts
         WHEN NEW.state = 'acknowledged' AND NOT EXISTS (
           SELECT 1 FROM provider_deliveries delivery
           JOIN agent_ownership ownership ON ownership.delivery_id = delivery.delivery_id
           WHERE delivery.wake_id = NEW.wake_id AND delivery.attempt = NEW.attempt
             AND delivery.state = 'delivered'
             AND ownership.state IN ('acknowledged', 'returned')
         )
         BEGIN SELECT RAISE(ABORT, 'wake attempt acknowledgement lacks agent ownership'); END;
         CREATE TABLE provider_delivery_observations (
           observation_id TEXT PRIMARY KEY
             CHECK(length(observation_id) = 67 AND substr(observation_id, 1, 3) = 'ro_'
                   AND substr(observation_id, 4) NOT GLOB '*[^0-9a-f]*'),
           delivery_id TEXT NOT NULL REFERENCES provider_deliveries(delivery_id) ON DELETE RESTRICT,
           sequence INTEGER NOT NULL CHECK(sequence > 0),
           work_generation INTEGER NOT NULL CHECK(work_generation > 0),
           owner_generation INTEGER NOT NULL CHECK(owner_generation > 0),
           from_state TEXT NOT NULL CHECK(from_state IN ('prepared', 'launched', 'uncertain')),
           to_state TEXT NOT NULL CHECK(to_state IN ('delivered', 'retry', 'uncertain', 'failed')),
           receipt_object_ref TEXT NOT NULL REFERENCES protected_objects(object_ref) ON DELETE RESTRICT,
           outcome_digest TEXT NOT NULL
             CHECK(length(outcome_digest) = 64 AND outcome_digest NOT GLOB '*[^0-9a-f]*'),
           observed_at TEXT NOT NULL CHECK(length(observed_at) >= 20),
           UNIQUE(delivery_id, sequence)
         );
         CREATE INDEX provider_delivery_observations_delivery
           ON provider_delivery_observations(delivery_id, sequence);
         CREATE TRIGGER provider_delivery_observation_immutable
         BEFORE UPDATE ON provider_delivery_observations
         BEGIN SELECT RAISE(ABORT, 'provider delivery observations are immutable'); END;
         CREATE TRIGGER provider_delivery_observation_no_delete
         BEFORE DELETE ON provider_delivery_observations
         BEGIN SELECT RAISE(ABORT, 'provider delivery observations cannot be deleted'); END;
         CREATE TRIGGER provider_delivery_observation_insert_fence
         BEFORE INSERT ON provider_delivery_observations
         WHEN NOT EXISTS (
           SELECT 1 FROM provider_deliveries delivery
           JOIN outbox wake ON wake.wake_id = delivery.wake_id
           JOIN protected_objects receipt ON receipt.object_ref = NEW.receipt_object_ref
           WHERE delivery.delivery_id = NEW.delivery_id
             AND delivery.state = NEW.from_state
             AND ((NEW.from_state IN ('prepared', 'launched') AND wake.state = 'claimed')
                  OR (NEW.from_state = 'uncertain' AND wake.state = 'uncertain'))
             AND wake.work_generation = NEW.work_generation
             AND wake.owner_generation = NEW.owner_generation
             AND receipt.kind = 'provider_receipt'
             AND receipt.work_item_id = wake.work_item_id
             AND receipt.content_digest = NEW.outcome_digest
             AND NEW.sequence = coalesce((
               SELECT max(previous.sequence) + 1
                 FROM provider_delivery_observations previous
                WHERE previous.delivery_id = NEW.delivery_id
             ), 1)
         )
         BEGIN SELECT RAISE(ABORT, 'provider delivery observation authority mismatch'); END;",
    )?;
    install_schema_identity(&transaction)?;
    install_custody_schema(&transaction)?;
    install_custody_successor_schema(&transaction)?;
    install_projection_intent_schema(&transaction)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn schema_object_exists(
    connection: &Connection,
    object_type: &str,
    name: &str,
) -> WorkLedgerResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM sqlite_schema WHERE type = ?1 AND name = ?2
         )",
        [object_type, name],
        |row| row.get(0),
    )?)
}

fn table_has_column(connection: &Connection, table: &str, column: &str) -> WorkLedgerResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        [table, column],
        |row| row.get(0),
    )?)
}

pub(super) fn verify_open_lineage(connection: &Connection, version: i64) -> WorkLedgerResult<()> {
    if !(0..=SCHEMA_VERSION).contains(&version) {
        return Err(WorkLedgerError::UnsupportedSchema(version));
    }
    if version < 3 {
        return Ok(());
    }
    let donor_sentinel = ["ledger_metadata", "ledger_clock", "route_changes"]
        .iter()
        .map(|name| schema_object_exists(connection, "table", name))
        .collect::<WorkLedgerResult<Vec<_>>>()?
        .into_iter()
        .any(|exists| exists);
    if donor_sentinel {
        return Err(WorkLedgerError::ForeignSchemaLineage {
            version,
            lineage: "route-change donor",
        });
    }
    if version == SCHEMA_VERSION {
        return verify_schema_identity(connection);
    }
    if version == 8 || version == 9 || version == 10 {
        return verify_schema_identity(connection);
    }
    if version == 7 {
        return Err(WorkLedgerError::Refused(
            "unmarked schema v7 is not a supported provider-continuation ledger".to_owned(),
        ));
    }
    let required_tables = match version {
        3 => &["wake_attempts"][..],
        4 => &["wake_attempts", "wake_claim_epochs"][..],
        5 => &[
            "wake_attempts",
            "wake_claim_epochs",
            "protected_objects",
            "activation_epochs",
            "provider_deliveries",
            "agent_ownership",
        ][..],
        6 => &[
            "wake_attempts",
            "wake_claim_epochs",
            "protected_objects",
            "activation_epochs",
            "provider_deliveries",
            "provider_delivery_observations",
            "agent_ownership",
        ][..],
        _ => {
            return Err(WorkLedgerError::UnsupportedSchema(version));
        }
    };
    let expected_main_shape = required_tables
        .iter()
        .map(|name| schema_object_exists(connection, "table", name))
        .collect::<WorkLedgerResult<Vec<_>>>()?
        .into_iter()
        .all(|exists| exists)
        && !table_has_column(connection, "outbox", "ledger_incarnation_ref")?
        && (version < 5
            || (table_has_column(connection, "outbox", "profile_ref")?
                && table_has_column(connection, "outbox", "provider_delivery_id")?));
    if !expected_main_shape || schema_object_exists(connection, "table", "ledger_schema_identity")?
    {
        return Err(WorkLedgerError::Refused(format!(
            "schema v{version} lineage is ambiguous or altered; refusing migration"
        )));
    }
    Ok(())
}

const SCHEMA_IDENTITY_OBJECTS: &[(&str, &str, &str)] = &[
    (
        "table",
        "ledger_schema_identity",
        "CREATE TABLE ledger_schema_identity (
           singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
           lineage TEXT NOT NULL CHECK(lineage = 'provider-continuation'),
           lineage_revision INTEGER NOT NULL CHECK(lineage_revision = 1)
         )",
    ),
    (
        "trigger",
        "ledger_schema_identity_immutable",
        "CREATE TRIGGER ledger_schema_identity_immutable
         BEFORE UPDATE ON ledger_schema_identity
         BEGIN SELECT RAISE(ABORT, 'ledger schema identity is immutable'); END",
    ),
    (
        "trigger",
        "ledger_schema_identity_no_delete",
        "CREATE TRIGGER ledger_schema_identity_no_delete
         BEFORE DELETE ON ledger_schema_identity
         BEGIN SELECT RAISE(ABORT, 'ledger schema identity is immutable'); END",
    ),
    (
        "trigger",
        "ledger_schema_identity_no_second_insert",
        "CREATE TRIGGER ledger_schema_identity_no_second_insert
         BEFORE INSERT ON ledger_schema_identity
         WHEN EXISTS (SELECT 1 FROM ledger_schema_identity)
         BEGIN SELECT RAISE(ABORT, 'ledger schema identity is a singleton'); END",
    ),
];

fn install_schema_identity(transaction: &rusqlite::Transaction<'_>) -> WorkLedgerResult<()> {
    for object_type in ["table", "trigger"] {
        for (kind, _, sql) in SCHEMA_IDENTITY_OBJECTS {
            if *kind == object_type {
                transaction.execute_batch(&format!("{sql};"))?;
            }
        }
        if object_type == "table" {
            transaction.execute(
                "INSERT INTO ledger_schema_identity
                 (singleton, lineage, lineage_revision)
                 VALUES (1, 'provider-continuation', 1)",
                [],
            )?;
        }
    }
    Ok(())
}

fn migrate_main_v6_to_v8(connection: &mut Connection) -> WorkLedgerResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    if schema_version(&transaction)? != 6 {
        return Err(WorkLedgerError::Refused(
            "schema version changed while acquiring the migration fence".to_owned(),
        ));
    }
    verify_open_lineage(&transaction, 6)?;
    validate_relational_integrity(&transaction)?;
    install_schema_identity(&transaction)?;
    transaction.pragma_update(None, "user_version", 8)?;
    verify_schema_identity(&transaction)?;
    validate_relational_integrity(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v8_to_v9(connection: &mut Connection) -> WorkLedgerResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    if schema_version(&transaction)? != 8 {
        return Err(WorkLedgerError::Refused(
            "schema version changed while acquiring the custody migration fence".to_owned(),
        ));
    }
    verify_open_lineage(&transaction, 8)?;
    validate_relational_integrity(&transaction)?;
    install_custody_schema(&transaction)?;
    transaction.pragma_update(None, "user_version", 9)?;
    verify_schema_identity(&transaction)?;
    validate_relational_integrity(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v9_to_v10(connection: &mut Connection) -> WorkLedgerResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    if schema_version(&transaction)? != 9 {
        return Err(WorkLedgerError::Refused(
            "schema version changed while acquiring the custody successor migration fence"
                .to_owned(),
        ));
    }
    verify_open_lineage(&transaction, 9)?;
    validate_relational_integrity(&transaction)?;
    install_custody_successor_schema(&transaction)?;
    transaction.pragma_update(None, "user_version", 10)?;
    verify_schema_identity(&transaction)?;
    validate_relational_integrity(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v10_to_v11(connection: &mut Connection) -> WorkLedgerResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    if schema_version(&transaction)? != 10 {
        return Err(WorkLedgerError::Refused(
            "schema version changed while acquiring the projection intent migration fence"
                .to_owned(),
        ));
    }
    verify_open_lineage(&transaction, 10)?;
    validate_relational_integrity(&transaction)?;
    install_projection_intent_schema(&transaction)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    verify_schema_identity(&transaction)?;
    validate_relational_integrity(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn install_projection_intent_schema(
    transaction: &rusqlite::Transaction<'_>,
) -> WorkLedgerResult<()> {
    transaction.execute_batch(
        "CREATE TABLE workstream_projection_bindings (
           work_item_id TEXT PRIMARY KEY REFERENCES work_items(id) ON DELETE RESTRICT,
           workstream_handle TEXT NOT NULL CHECK(length(workstream_handle) BETWEEN 1 AND 128),
           plan_sha256 TEXT NOT NULL
             CHECK(length(plan_sha256) = 64 AND plan_sha256 NOT GLOB '*[^0-9a-f]*'),
           root_revision INTEGER NOT NULL CHECK(root_revision >= 0),
           issue_revision INTEGER NOT NULL CHECK(issue_revision >= 0),
           projection_revision INTEGER NOT NULL CHECK(projection_revision > 0),
           material_event_revision INTEGER NOT NULL CHECK(material_event_revision >= 0),
           repository TEXT NOT NULL CHECK(length(repository) BETWEEN 3 AND 255),
           exact_head TEXT NOT NULL
             CHECK(length(exact_head) = 40 AND exact_head NOT GLOB '*[^0-9a-f]*'),
           created_at TEXT NOT NULL CHECK(length(created_at) >= 20),
           UNIQUE(workstream_handle, repository, exact_head)
         );
         CREATE TRIGGER workstream_projection_binding_identity_immutable
         BEFORE UPDATE OF work_item_id, workstream_handle, plan_sha256, root_revision,
                          issue_revision, projection_revision, material_event_revision,
                          repository, created_at
         ON workstream_projection_bindings
         BEGIN SELECT RAISE(ABORT, 'workstream projection binding identity is immutable'); END;
         CREATE TRIGGER workstream_projection_binding_no_delete
         BEFORE DELETE ON workstream_projection_bindings
         BEGIN SELECT RAISE(ABORT, 'workstream projection binding cannot be deleted'); END;
         CREATE TABLE projection_intents (
           intent_id TEXT PRIMARY KEY
             CHECK(length(intent_id) = 64 AND intent_id NOT GLOB '*[^0-9a-f]*'),
           work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
           workstream_handle TEXT NOT NULL CHECK(length(workstream_handle) BETWEEN 1 AND 128),
           sequence INTEGER NOT NULL CHECK(sequence > 0),
           kind TEXT NOT NULL CHECK(kind IN ('handoff', 'waiting', 'actionable', 'new_head',
                                              'merge', 'configured_closure')),
           source_revision TEXT NOT NULL
             CHECK(length(source_revision) = 64 AND source_revision NOT GLOB '*[^0-9a-f]*'),
           exact_head TEXT
             CHECK(exact_head IS NULL OR
                   (length(exact_head) = 40 AND exact_head NOT GLOB '*[^0-9a-f]*')),
           receipt_snapshot BLOB NOT NULL CHECK(length(receipt_snapshot) BETWEEN 2 AND 1048576),
           receipt_sha256 TEXT NOT NULL
             CHECK(length(receipt_sha256) = 64 AND receipt_sha256 NOT GLOB '*[^0-9a-f]*'),
           transition_id TEXT NOT NULL UNIQUE
             CHECK(length(transition_id) = 64 AND transition_id NOT GLOB '*[^0-9a-f]*'),
           supersedes_transition_id TEXT
             CHECK(supersedes_transition_id IS NULL OR
                   (length(supersedes_transition_id) = 64 AND
                    supersedes_transition_id NOT GLOB '*[^0-9a-f]*')),
           state TEXT NOT NULL CHECK(state IN ('pending', 'projected', 'quarantined')),
           attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts >= 0),
           retry_at_unix_ms INTEGER NOT NULL DEFAULT 0 CHECK(retry_at_unix_ms >= 0),
           failure_class TEXT,
           created_at TEXT NOT NULL CHECK(length(created_at) >= 20),
           updated_at TEXT NOT NULL CHECK(length(updated_at) >= 20),
           UNIQUE(workstream_handle, sequence),
           CHECK(length(receipt_sha256) = 64)
         );
         CREATE INDEX projection_intents_drain
           ON projection_intents(state, retry_at_unix_ms, workstream_handle, sequence);
         CREATE TRIGGER projection_intent_identity_immutable
         BEFORE UPDATE OF intent_id, work_item_id, workstream_handle, sequence, kind,
                          source_revision, exact_head, receipt_snapshot, receipt_sha256,
                          transition_id, supersedes_transition_id, created_at
         ON projection_intents
         BEGIN SELECT RAISE(ABORT, 'projection intent identity is immutable'); END;
         CREATE TRIGGER projection_intent_no_delete
         BEFORE DELETE ON projection_intents
         BEGIN SELECT RAISE(ABORT, 'projection intent cannot be deleted'); END;",
    )?;
    Ok(())
}

fn install_custody_successor_schema(
    transaction: &rusqlite::Transaction<'_>,
) -> WorkLedgerResult<()> {
    transaction.execute_batch(
        "CREATE TABLE custody_successor_rebinds (
           rebind_id TEXT NOT NULL,
           message_id TEXT NOT NULL,
           side TEXT NOT NULL CHECK(side IN ('sender', 'receiver')),
           rebind_json BLOB NOT NULL,
           rebind_digest TEXT NOT NULL
             CHECK(length(rebind_digest) = 64 AND rebind_digest NOT GLOB '*[^0-9a-f]*'),
           authority_epoch INTEGER NOT NULL CHECK(authority_epoch > 0),
           state TEXT NOT NULL CHECK(state IN ('prepared', 'committed', 'acknowledged')),
           receipt_json BLOB,
           receipt_digest TEXT,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           PRIMARY KEY(rebind_id, side),
           UNIQUE(message_id, side, rebind_digest),
           UNIQUE(message_id, side, authority_epoch),
           CHECK((receipt_json IS NULL AND receipt_digest IS NULL) OR
                 (receipt_json IS NOT NULL AND length(receipt_digest) = 64 AND
                  receipt_digest NOT GLOB '*[^0-9a-f]*'))
         );
         CREATE INDEX custody_successor_message
           ON custody_successor_rebinds(message_id, side, state, updated_at);
         CREATE TRIGGER custody_successor_identity_immutable
         BEFORE UPDATE OF message_id, side, rebind_json, rebind_digest, authority_epoch
         ON custody_successor_rebinds
         BEGIN SELECT RAISE(ABORT, 'custody successor identity is immutable'); END;
         CREATE TRIGGER custody_successor_receipt_immutable
         BEFORE UPDATE OF receipt_json, receipt_digest ON custody_successor_rebinds
         WHEN OLD.receipt_digest IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'custody successor receipt is immutable'); END;
         CREATE TRIGGER custody_successor_no_delete
         BEFORE DELETE ON custody_successor_rebinds
         BEGIN SELECT RAISE(ABORT, 'custody successor rebinds cannot be deleted'); END;
         CREATE TABLE custody_processed_acknowledgements (
           receipt_digest TEXT PRIMARY KEY
             CHECK(length(receipt_digest) = 64 AND receipt_digest NOT GLOB '*[^0-9a-f]*'),
           message_id TEXT NOT NULL REFERENCES custody_inbox(message_id) ON DELETE RESTRICT,
           source_machine_ref TEXT NOT NULL,
           created_at TEXT NOT NULL,
           UNIQUE(message_id)
         );
         CREATE TRIGGER custody_processed_ack_immutable
         BEFORE UPDATE ON custody_processed_acknowledgements
         BEGIN SELECT RAISE(ABORT, 'custody processed acknowledgements are immutable'); END;
         CREATE TRIGGER custody_processed_ack_no_delete
         BEFORE DELETE ON custody_processed_acknowledgements
         BEGIN SELECT RAISE(ABORT, 'custody processed acknowledgements cannot be deleted'); END;",
    )?;
    Ok(())
}

#[allow(clippy::too_many_lines)] // Keep one atomic, auditable schema installation transaction.
fn install_custody_schema(transaction: &rusqlite::Transaction<'_>) -> WorkLedgerResult<()> {
    transaction.execute_batch(
        "CREATE TABLE custody_outbox (
           message_id TEXT PRIMARY KEY
             CHECK(length(message_id) = 67 AND substr(message_id, 1, 3) = 'wm_'
                   AND substr(message_id, 4) NOT GLOB '*[^0-9a-f]*'),
           wake_id TEXT NOT NULL REFERENCES outbox(wake_id) ON DELETE RESTRICT,
           identity_json BLOB NOT NULL,
           identity_digest TEXT NOT NULL
             CHECK(length(identity_digest) = 64 AND identity_digest NOT GLOB '*[^0-9a-f]*'),
           state TEXT NOT NULL CHECK(state IN ('pending', 'claimed', 'custody_accepted',
                                               'processed', 'cancelled', 'superseded')),
           active_rebind_epoch INTEGER NOT NULL CHECK(active_rebind_epoch > 0),
           custody_receipt_digest TEXT,
           custody_transfer_digest TEXT,
           processed_receipt_digest TEXT,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           UNIQUE(wake_id, message_id)
         );
         CREATE TABLE custody_rebinds (
           message_id TEXT NOT NULL REFERENCES custody_outbox(message_id) ON DELETE RESTRICT,
           epoch INTEGER NOT NULL CHECK(epoch > 0),
           target_machine_ref TEXT NOT NULL,
           target_incarnation_ref TEXT NOT NULL,
           target_route_ref TEXT NOT NULL,
           terminal_adapter TEXT NOT NULL,
           authority_digest TEXT NOT NULL
             CHECK(length(authority_digest) = 64 AND authority_digest NOT GLOB '*[^0-9a-f]*'),
           created_at TEXT NOT NULL,
           PRIMARY KEY(message_id, epoch)
         );
         CREATE TABLE custody_sender_claims (
           message_id TEXT NOT NULL REFERENCES custody_outbox(message_id) ON DELETE RESTRICT,
           epoch INTEGER NOT NULL CHECK(epoch > 0),
           owner_ref TEXT NOT NULL,
           state TEXT NOT NULL CHECK(state IN ('active', 'released')),
           acquired_at TEXT NOT NULL,
           expires_at TEXT NOT NULL,
           released_at TEXT,
           PRIMARY KEY(message_id, epoch)
         );
         CREATE TABLE custody_inbox (
           message_id TEXT PRIMARY KEY,
           identity_json BLOB NOT NULL,
           identity_digest TEXT NOT NULL,
           rebind_epoch INTEGER NOT NULL CHECK(rebind_epoch > 0),
           target_machine_ref TEXT NOT NULL,
           target_incarnation_ref TEXT NOT NULL,
           target_route_ref TEXT NOT NULL,
           terminal_adapter TEXT NOT NULL,
           authority_digest TEXT NOT NULL,
           transfer_digest TEXT NOT NULL,
           transport_auth_digest TEXT NOT NULL,
           state TEXT NOT NULL CHECK(state IN ('received', 'processing', 'processed',
                                               'cancelled', 'superseded')),
           custody_receipt_digest TEXT NOT NULL,
           effect_digest TEXT,
           processed_receipt_digest TEXT,
           processed_receipt_json BLOB,
           received_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           processed_at TEXT,
           CHECK(length(identity_digest) = 64 AND identity_digest NOT GLOB '*[^0-9a-f]*'),
           CHECK(length(authority_digest) = 64 AND authority_digest NOT GLOB '*[^0-9a-f]*'),
           CHECK(length(transfer_digest) = 64 AND transfer_digest NOT GLOB '*[^0-9a-f]*'),
           CHECK(length(transport_auth_digest) = 64 AND transport_auth_digest NOT GLOB '*[^0-9a-f]*'),
           CHECK(length(custody_receipt_digest) = 64 AND custody_receipt_digest NOT GLOB '*[^0-9a-f]*'),
           CHECK(effect_digest IS NULL OR
                 (length(effect_digest) = 64 AND effect_digest NOT GLOB '*[^0-9a-f]*')),
           CHECK(processed_receipt_digest IS NULL OR
                 (length(processed_receipt_digest) = 64 AND
                  processed_receipt_digest NOT GLOB '*[^0-9a-f]*')),
           CHECK((state = 'processed' AND effect_digest IS NOT NULL AND
                  processed_receipt_digest IS NOT NULL AND
                  processed_receipt_json IS NOT NULL AND processed_at IS NOT NULL) OR
                 (state != 'processed' AND effect_digest IS NULL AND
                  processed_receipt_digest IS NULL AND
                  processed_receipt_json IS NULL AND processed_at IS NULL))
         );
         CREATE TABLE custody_inbox_claims (
           message_id TEXT NOT NULL REFERENCES custody_inbox(message_id) ON DELETE RESTRICT,
           epoch INTEGER NOT NULL CHECK(epoch > 0),
           owner_ref TEXT NOT NULL,
           state TEXT NOT NULL CHECK(state IN ('active', 'released')),
           acquired_at TEXT NOT NULL,
           expires_at TEXT NOT NULL,
           released_at TEXT,
           PRIMARY KEY(message_id, epoch)
         );
         CREATE TABLE custody_effects (
           message_id TEXT PRIMARY KEY REFERENCES custody_inbox(message_id) ON DELETE RESTRICT,
           effect_digest TEXT NOT NULL,
           applied_at TEXT NOT NULL
         );
         CREATE TABLE custody_controls (
           control_id TEXT PRIMARY KEY,
           message_id TEXT NOT NULL,
           identity_digest TEXT NOT NULL,
           kind TEXT NOT NULL CHECK(kind IN ('cancelled', 'superseded')),
           successor_message_id TEXT,
           expected_rebind_epoch INTEGER NOT NULL CHECK(expected_rebind_epoch > 0),
           workstream_revision INTEGER NOT NULL CHECK(workstream_revision > 0),
           authority_digest TEXT NOT NULL,
           control_digest TEXT NOT NULL UNIQUE,
           state TEXT NOT NULL CHECK(state IN ('pending', 'acknowledged')),
           receipt_digest TEXT,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL
         );
         CREATE TABLE custody_events (
           event_id TEXT PRIMARY KEY,
           message_id TEXT NOT NULL,
           side TEXT NOT NULL CHECK(side IN ('sender', 'receiver')),
           sequence INTEGER NOT NULL CHECK(sequence > 0),
           kind TEXT NOT NULL,
           evidence_digest TEXT NOT NULL,
           created_at TEXT NOT NULL,
           UNIQUE(message_id, side, sequence)
         );
         CREATE INDEX custody_outbox_state ON custody_outbox(state, updated_at, message_id);
         CREATE INDEX custody_inbox_state ON custody_inbox(state, updated_at, message_id);
         CREATE TRIGGER custody_outbox_identity_immutable
         BEFORE UPDATE OF wake_id, identity_json, identity_digest ON custody_outbox
         BEGIN SELECT RAISE(ABORT, 'custody outbox identity is immutable'); END;
         CREATE TRIGGER custody_rebind_immutable BEFORE UPDATE ON custody_rebinds
         BEGIN SELECT RAISE(ABORT, 'custody rebinds are immutable'); END;
         CREATE TRIGGER custody_rebind_no_delete BEFORE DELETE ON custody_rebinds
         BEGIN SELECT RAISE(ABORT, 'custody rebinds cannot be deleted'); END;
         CREATE TRIGGER custody_inbox_identity_immutable
         BEFORE UPDATE OF identity_json, identity_digest, rebind_epoch,
                          target_machine_ref, target_incarnation_ref, target_route_ref,
                          terminal_adapter, authority_digest, transfer_digest,
                          transport_auth_digest, custody_receipt_digest ON custody_inbox
         BEGIN SELECT RAISE(ABORT, 'custody inbox identity is immutable'); END;
         CREATE TRIGGER custody_inbox_processed_receipt_immutable
         BEFORE UPDATE OF effect_digest, processed_receipt_digest,
                          processed_receipt_json, processed_at ON custody_inbox
         WHEN OLD.processed_receipt_digest IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'custody processed receipt is immutable'); END;
         CREATE TRIGGER custody_effect_immutable BEFORE UPDATE ON custody_effects
         BEGIN SELECT RAISE(ABORT, 'custody effects are immutable'); END;
         CREATE TRIGGER custody_effect_no_delete BEFORE DELETE ON custody_effects
         BEGIN SELECT RAISE(ABORT, 'custody effects cannot be deleted'); END;
         CREATE TRIGGER custody_control_identity_immutable
         BEFORE UPDATE OF message_id, identity_digest, kind, successor_message_id,
                          expected_rebind_epoch, workstream_revision, authority_digest,
                          control_digest ON custody_controls
         BEGIN SELECT RAISE(ABORT, 'custody controls are immutable'); END;
         CREATE TRIGGER custody_control_no_delete BEFORE DELETE ON custody_controls
         BEGIN SELECT RAISE(ABORT, 'custody controls cannot be deleted'); END;
         CREATE TRIGGER custody_event_immutable BEFORE UPDATE ON custody_events
         BEGIN SELECT RAISE(ABORT, 'custody events are immutable'); END;
         CREATE TRIGGER custody_event_no_delete BEFORE DELETE ON custody_events
         BEGIN SELECT RAISE(ABORT, 'custody events cannot be deleted'); END;",
    )?;
    Ok(())
}

fn verify_schema_identity(connection: &Connection) -> WorkLedgerResult<()> {
    let mut statement = connection.prepare(
        "SELECT type, name, sql FROM sqlite_schema
         WHERE name = 'ledger_schema_identity'
            OR name LIKE 'ledger_schema_identity_%'
         ORDER BY type, name",
    )?;
    let actual = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut expected = SCHEMA_IDENTITY_OBJECTS
        .iter()
        .map(|(kind, name, sql)| ((*kind).to_owned(), (*name).to_owned(), (*sql).to_owned()))
        .collect::<Vec<_>>();
    expected.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
    if actual != expected {
        let version = schema_version(connection)?;
        let donor_v7 = version == 7
            && schema_object_exists(connection, "table", "ledger_clock")?
            && schema_object_exists(connection, "table", "route_changes")?
            && schema_object_exists(connection, "table", "protected_objects")?
            && table_has_column(connection, "outbox", "ledger_incarnation_ref")?;
        let reason = if donor_v7 {
            "schema v7 belongs to the protected-object donor lineage; explicit state reconciliation is required"
        } else {
            "work ledger schema identity is missing or altered"
        };
        return Err(WorkLedgerError::Refused(reason.to_owned()));
    }
    let identity: (String, i64) = connection.query_row(
        "SELECT lineage, lineage_revision FROM ledger_schema_identity WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if identity != ("provider-continuation".to_owned(), 1) {
        return Err(WorkLedgerError::Refused(
            "work ledger schema identity row is missing or altered".to_owned(),
        ));
    }
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

/// Add the append-only delivery attempt fence without changing any v2 route or
/// wake identity. Existing pending wakes remain pending; an old `claimed` wake
/// is deliberately not guessed safe and will reconcile as uncertain unless its
/// provider proves idempotent delivery.
fn migrate_v2_to_v3(connection: &mut Connection) -> WorkLedgerResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    transaction.execute_batch(
        "CREATE TABLE wake_attempts (
           wake_id TEXT NOT NULL REFERENCES outbox(wake_id) ON DELETE RESTRICT,
           attempt INTEGER NOT NULL CHECK(attempt > 0),
           state TEXT NOT NULL CHECK(state IN ('claimed', 'acknowledged', 'retry', 'uncertain', 'failed')),
           adapter_id TEXT NOT NULL,
           idempotent INTEGER NOT NULL CHECK(idempotent IN (0, 1)),
           outcome_digest TEXT,
           started_at TEXT NOT NULL,
           finished_at TEXT,
           PRIMARY KEY(wake_id, attempt)
         );
         CREATE INDEX wake_attempts_recovery
           ON wake_attempts(state, started_at, wake_id);
         PRAGMA user_version = 3;",
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

/// Add append-only consumer ownership epochs. The host-local exclusive
/// consumer lock proves liveness; these rows distinguish an original claim
/// from a later crash-recovery owner and fence stale finalizers.
fn migrate_v3_to_v4(connection: &mut Connection) -> WorkLedgerResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    transaction.execute_batch(
        "CREATE TABLE wake_claim_epochs (
           wake_id TEXT NOT NULL,
           attempt INTEGER NOT NULL,
           epoch INTEGER NOT NULL CHECK(epoch > 0),
           owner_ref TEXT NOT NULL,
           kind TEXT NOT NULL CHECK(kind IN ('claim', 'recovery')),
           acquired_at TEXT NOT NULL,
           PRIMARY KEY(wake_id, attempt, epoch),
           FOREIGN KEY(wake_id, attempt) REFERENCES wake_attempts(wake_id, attempt)
             ON DELETE RESTRICT
         );
         CREATE INDEX wake_claim_epoch_owner ON wake_claim_epochs(owner_ref, epoch);
         PRAGMA user_version = 4;",
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

/// Add immutable protected objects and split provider delivery from resumed
/// agent ownership. Historical v4 `acknowledged` rows remain byte-for-byte
/// representable; new provider acceptance uses `delivered` and is fenced by an
/// exact delivery row and receipt object.
#[allow(clippy::too_many_lines)] // One audited transaction owns the cyclic FK rebuild.
fn migrate_v4_to_v5(connection: &mut Connection) -> WorkLedgerResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    transaction.execute_batch(
        "PRAGMA defer_foreign_keys = ON;
         ALTER TABLE outbox RENAME TO outbox_v4;
         ALTER TABLE wake_attempts RENAME TO wake_attempts_v4;
         ALTER TABLE wake_claim_epochs RENAME TO wake_claim_epochs_v4;
         CREATE TABLE protected_objects (
           object_ref TEXT PRIMARY KEY
             CHECK(length(object_ref) = 67 AND substr(object_ref, 1, 3) = 'po_'
                   AND substr(object_ref, 4) NOT GLOB '*[^0-9a-f]*'),
           work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
           kind TEXT NOT NULL CHECK(kind IN ('launch_profile', 'provider_request',
                                             'provider_receipt', 'agent_receipt')),
           profile_ref TEXT,
           storage_name TEXT NOT NULL UNIQUE
             CHECK(length(storage_name) = 76
                   AND substr(storage_name, 1, 7) = 'object-'
                   AND substr(storage_name, 8, 64) NOT GLOB '*[^0-9a-f]*'
                   AND substr(storage_name, 72, 5) = '.blob'),
           content_digest TEXT NOT NULL
             CHECK(length(content_digest) = 64
                   AND content_digest NOT GLOB '*[^0-9a-f]*'),
           byte_length INTEGER NOT NULL CHECK(byte_length >= 0 AND byte_length <= 1048576),
           created_at TEXT NOT NULL CHECK(length(created_at) >= 20),
           CHECK((kind = 'launch_profile' AND profile_ref IS NOT NULL
                  AND length(profile_ref) = 78
                  AND substr(profile_ref, 1, 14) = 'opaque:sha256:'
                  AND substr(profile_ref, 15) NOT GLOB '*[^0-9a-f]*')
                 OR (kind != 'launch_profile' AND profile_ref IS NULL)),
           UNIQUE(work_item_id, profile_ref)
         );
         CREATE TABLE activation_epochs (
           activation_id TEXT PRIMARY KEY
             CHECK(length(activation_id) = 67 AND substr(activation_id, 1, 3) = 'ae_'
                   AND substr(activation_id, 4) NOT GLOB '*[^0-9a-f]*'),
           work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
           work_generation INTEGER NOT NULL CHECK(work_generation > 0),
           owner_generation INTEGER NOT NULL CHECK(owner_generation > 0),
           epoch INTEGER NOT NULL CHECK(epoch > 0),
           owner_ref TEXT NOT NULL CHECK(length(owner_ref) BETWEEN 65 AND 128),
           state TEXT NOT NULL CHECK(state IN ('active', 'released', 'superseded')),
           acquired_at TEXT NOT NULL CHECK(length(acquired_at) >= 20),
           released_at TEXT,
           CHECK((state = 'active' AND released_at IS NULL)
                 OR (state != 'active' AND released_at IS NOT NULL)),
           UNIQUE(work_item_id, work_generation, owner_generation, epoch)
         );
         CREATE TABLE outbox (
           wake_id TEXT PRIMARY KEY,
           work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
           work_generation INTEGER NOT NULL,
           owner_generation INTEGER NOT NULL,
           state TEXT NOT NULL CHECK(state IN ('pending', 'claimed', 'delivered',
                                               'acknowledged', 'uncertain', 'failed')),
           route_ref TEXT NOT NULL,
           profile_ref TEXT,
           payload_digest TEXT NOT NULL,
           transport_receipt_digest TEXT,
           provider_delivery_id TEXT UNIQUE
             REFERENCES provider_deliveries(delivery_id) DEFERRABLE INITIALLY DEFERRED,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           acknowledged_at TEXT,
           CHECK(state NOT IN ('delivered', 'acknowledged')
                 OR (provider_delivery_id IS NOT NULL
                     AND profile_ref IS NOT NULL AND length(profile_ref) = 78
                     AND substr(profile_ref, 1, 14) = 'opaque:sha256:'
                     AND substr(profile_ref, 15) NOT GLOB '*[^0-9a-f]*'
                     AND transport_receipt_digest IS NOT NULL
                     AND length(transport_receipt_digest) = 64
                     AND transport_receipt_digest NOT GLOB '*[^0-9a-f]*'
                     AND ((state = 'delivered' AND acknowledged_at IS NULL)
                          OR (state = 'acknowledged' AND acknowledged_at IS NOT NULL))))
         );
         CREATE TABLE wake_attempts (
           wake_id TEXT NOT NULL REFERENCES outbox(wake_id) ON DELETE RESTRICT,
           attempt INTEGER NOT NULL CHECK(attempt > 0),
           state TEXT NOT NULL CHECK(state IN ('claimed', 'delivered', 'acknowledged',
                                               'retry', 'uncertain', 'failed')),
           adapter_id TEXT NOT NULL,
           idempotent INTEGER NOT NULL CHECK(idempotent IN (0, 1)),
           outcome_digest TEXT,
           started_at TEXT NOT NULL,
           finished_at TEXT,
           CHECK(state != 'delivered'
                 OR (outcome_digest IS NOT NULL AND length(outcome_digest) = 64
                     AND outcome_digest NOT GLOB '*[^0-9a-f]*'
                     AND finished_at IS NOT NULL)),
           PRIMARY KEY(wake_id, attempt)
         );
         CREATE TABLE wake_claim_epochs (
           wake_id TEXT NOT NULL,
           attempt INTEGER NOT NULL,
           epoch INTEGER NOT NULL CHECK(epoch > 0),
           owner_ref TEXT NOT NULL,
           kind TEXT NOT NULL CHECK(kind IN ('claim', 'recovery')),
           acquired_at TEXT NOT NULL,
           PRIMARY KEY(wake_id, attempt, epoch),
           FOREIGN KEY(wake_id, attempt) REFERENCES wake_attempts(wake_id, attempt)
             ON DELETE RESTRICT
         );
         CREATE TABLE provider_deliveries (
           delivery_id TEXT PRIMARY KEY
             CHECK(length(delivery_id) = 67 AND substr(delivery_id, 1, 3) = 'pd_'
                   AND substr(delivery_id, 4) NOT GLOB '*[^0-9a-f]*'),
           wake_id TEXT NOT NULL,
           attempt INTEGER NOT NULL CHECK(attempt > 0),
           activation_id TEXT NOT NULL REFERENCES activation_epochs(activation_id) ON DELETE RESTRICT,
           provider_id TEXT NOT NULL CHECK(length(provider_id) BETWEEN 1 AND 512),
           adapter_id TEXT NOT NULL CHECK(length(adapter_id) BETWEEN 1 AND 512),
           idempotency_key TEXT NOT NULL UNIQUE CHECK(length(idempotency_key) BETWEEN 1 AND 512),
           request_object_ref TEXT NOT NULL REFERENCES protected_objects(object_ref) ON DELETE RESTRICT,
           receipt_object_ref TEXT REFERENCES protected_objects(object_ref) ON DELETE RESTRICT,
           state TEXT NOT NULL CHECK(state IN ('prepared', 'launched', 'delivered',
                                               'retry', 'uncertain', 'failed')),
           created_at TEXT NOT NULL CHECK(length(created_at) >= 20),
           updated_at TEXT NOT NULL CHECK(length(updated_at) >= 20),
           delivered_at TEXT,
           CHECK((state = 'delivered' AND receipt_object_ref IS NOT NULL
                  AND delivered_at IS NOT NULL)
                 OR (state != 'delivered' AND delivered_at IS NULL)),
           UNIQUE(wake_id, attempt),
           FOREIGN KEY(wake_id, attempt) REFERENCES wake_attempts(wake_id, attempt)
             ON DELETE RESTRICT
         );
         CREATE TABLE agent_ownership (
           ownership_id TEXT PRIMARY KEY
             CHECK(length(ownership_id) = 67 AND substr(ownership_id, 1, 3) = 'ao_'
                   AND substr(ownership_id, 4) NOT GLOB '*[^0-9a-f]*'),
           work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
           work_generation INTEGER NOT NULL CHECK(work_generation > 0),
           owner_generation INTEGER NOT NULL CHECK(owner_generation > 0),
           delivery_id TEXT NOT NULL UNIQUE REFERENCES provider_deliveries(delivery_id) ON DELETE RESTRICT,
           launch_profile_object_ref TEXT NOT NULL REFERENCES protected_objects(object_ref) ON DELETE RESTRICT,
           context_receipt_object_ref TEXT REFERENCES protected_objects(object_ref) ON DELETE RESTRICT,
           state TEXT NOT NULL CHECK(state IN ('pending', 'acknowledged', 'returned',
                                               'uncertain', 'failed')),
           context_receipt_digest TEXT,
           created_at TEXT NOT NULL CHECK(length(created_at) >= 20),
           updated_at TEXT NOT NULL CHECK(length(updated_at) >= 20),
           acknowledged_at TEXT,
           returned_at TEXT,
           CHECK((state IN ('acknowledged', 'returned')
                  AND context_receipt_digest IS NOT NULL
                  AND length(context_receipt_digest) = 64
                  AND context_receipt_digest NOT GLOB '*[^0-9a-f]*'
                  AND context_receipt_object_ref IS NOT NULL
                  AND acknowledged_at IS NOT NULL)
                 OR (state NOT IN ('acknowledged', 'returned')
                     AND context_receipt_digest IS NULL
                     AND context_receipt_object_ref IS NULL
                     AND acknowledged_at IS NULL)),
           CHECK((state = 'returned' AND returned_at IS NOT NULL)
                 OR (state != 'returned' AND returned_at IS NULL)),
           UNIQUE(work_item_id, work_generation, owner_generation)
         );
         INSERT INTO outbox
           (wake_id, work_item_id, work_generation, owner_generation, state,
            route_ref, profile_ref, payload_digest, transport_receipt_digest,
            provider_delivery_id, created_at, updated_at, acknowledged_at)
         SELECT wake_id, work_item_id, work_generation, owner_generation,
                CASE WHEN state = 'acknowledged' THEN 'uncertain' ELSE state END,
                route_ref, NULL, payload_digest, transport_receipt_digest,
                NULL, created_at, updated_at, acknowledged_at
           FROM outbox_v4;
         INSERT INTO wake_attempts
           (wake_id, attempt, state, adapter_id, idempotent, outcome_digest,
            started_at, finished_at)
         SELECT wake_id, attempt,
                CASE WHEN state = 'acknowledged' THEN 'uncertain' ELSE state END,
                adapter_id, idempotent, outcome_digest,
                started_at, finished_at FROM wake_attempts_v4;
         INSERT INTO wake_claim_epochs
           (wake_id, attempt, epoch, owner_ref, kind, acquired_at)
         SELECT wake_id, attempt, epoch, owner_ref, kind, acquired_at
           FROM wake_claim_epochs_v4;
         DROP TABLE wake_claim_epochs_v4;
         DROP TABLE wake_attempts_v4;
         DROP TABLE outbox_v4;
         CREATE INDEX outbox_delivery ON outbox(state, created_at, wake_id);
         CREATE INDEX wake_attempts_recovery ON wake_attempts(state, started_at, wake_id);
         CREATE INDEX wake_claim_epoch_owner ON wake_claim_epochs(owner_ref, epoch);
         CREATE INDEX protected_objects_work_kind
           ON protected_objects(work_item_id, kind, created_at);
         CREATE TRIGGER protected_object_capacity
         BEFORE INSERT ON protected_objects
         WHEN (SELECT COUNT(*) FROM protected_objects) >= 4096
           OR (SELECT COALESCE(SUM(byte_length), 0) FROM protected_objects)
                + NEW.byte_length > 16777216
         BEGIN SELECT RAISE(ABORT, 'protected object store capacity exceeded'); END;
         CREATE TRIGGER protected_object_immutable
         BEFORE UPDATE ON protected_objects
         BEGIN SELECT RAISE(ABORT, 'protected object metadata is immutable'); END;
         CREATE TRIGGER protected_object_no_delete
         BEFORE DELETE ON protected_objects
         BEGIN SELECT RAISE(ABORT, 'protected objects cannot be deleted'); END;
         CREATE INDEX activation_epochs_active
           ON activation_epochs(state, acquired_at, work_item_id);
         CREATE UNIQUE INDEX activation_epochs_one_active
           ON activation_epochs(work_item_id) WHERE state = 'active';
         CREATE TRIGGER activation_epoch_identity_immutable
         BEFORE UPDATE OF activation_id, work_item_id, work_generation, owner_generation, epoch,
                          owner_ref, acquired_at ON activation_epochs
         BEGIN SELECT RAISE(ABORT, 'activation epoch identity is immutable'); END;
         CREATE TRIGGER activation_epoch_no_delete
         BEFORE DELETE ON activation_epochs
         BEGIN SELECT RAISE(ABORT, 'activation epochs cannot be deleted'); END;
         CREATE TRIGGER activation_epoch_release_fence
         BEFORE UPDATE OF state, released_at ON activation_epochs
         WHEN (OLD.state = 'active' AND NEW.state = 'active'
               AND OLD.released_at IS NOT NEW.released_at)
           OR (OLD.state != 'active'
               AND (OLD.state != NEW.state OR OLD.released_at IS NOT NEW.released_at))
           OR (OLD.state != NEW.state
               AND (OLD.state != 'active' OR NEW.state NOT IN ('released', 'superseded')))
           OR (OLD.state = 'active' AND NEW.state != 'active' AND EXISTS (
               SELECT 1 FROM provider_deliveries delivery
               WHERE delivery.activation_id = OLD.activation_id
                 AND delivery.state IN ('prepared', 'launched')
           ))
         BEGIN SELECT RAISE(ABORT, 'activation epoch transition is unsafe'); END;
         CREATE INDEX provider_deliveries_state
           ON provider_deliveries(state, updated_at, wake_id);
         CREATE INDEX agent_ownership_state
           ON agent_ownership(state, updated_at, work_item_id);
         CREATE TRIGGER provider_delivery_insert_fence
         BEFORE INSERT ON provider_deliveries
         WHEN NOT EXISTS (
           SELECT 1 FROM wake_attempts attempt
           JOIN outbox wake ON wake.wake_id = attempt.wake_id
           JOIN activation_epochs activation
             ON activation.activation_id = NEW.activation_id
           WHERE attempt.wake_id = NEW.wake_id AND attempt.attempt = NEW.attempt
             AND attempt.adapter_id = NEW.adapter_id
             AND activation.state = 'active'
             AND activation.work_item_id = wake.work_item_id
             AND activation.work_generation = wake.work_generation
             AND activation.owner_generation = wake.owner_generation
         )
         BEGIN SELECT RAISE(ABORT, 'provider delivery authority mismatch'); END;
         CREATE TRIGGER provider_delivery_identity_immutable
         BEFORE UPDATE OF delivery_id, wake_id, attempt, activation_id, provider_id, adapter_id,
                          idempotency_key, request_object_ref ON provider_deliveries
         BEGIN SELECT RAISE(ABORT, 'provider delivery identity is immutable'); END;
         CREATE TRIGGER provider_delivery_no_delete
         BEFORE DELETE ON provider_deliveries
         BEGIN SELECT RAISE(ABORT, 'provider deliveries cannot be deleted'); END;
         CREATE TRIGGER provider_delivery_state_fence_insert
         BEFORE INSERT ON provider_deliveries
         WHEN NOT EXISTS (
           SELECT 1 FROM wake_attempts attempt
           JOIN outbox wake ON wake.wake_id = attempt.wake_id
           JOIN protected_objects request ON request.object_ref = NEW.request_object_ref
           JOIN activation_epochs activation ON activation.activation_id = NEW.activation_id
           LEFT JOIN protected_objects receipt ON receipt.object_ref = NEW.receipt_object_ref
           WHERE attempt.wake_id = NEW.wake_id AND attempt.attempt = NEW.attempt
             AND attempt.adapter_id = NEW.adapter_id
             AND request.kind = 'provider_request'
             AND request.work_item_id = wake.work_item_id
             AND activation.work_item_id = wake.work_item_id
             AND activation.work_generation = wake.work_generation
             AND activation.owner_generation = wake.owner_generation
             AND ((NEW.state IN ('prepared', 'launched') AND activation.state = 'active'
                   AND attempt.state = 'claimed')
                  OR (NEW.state = 'delivered' AND attempt.state = 'delivered'
                      AND receipt.kind = 'provider_receipt'
                      AND receipt.work_item_id = wake.work_item_id
                      AND receipt.content_digest = attempt.outcome_digest)
                  OR (NEW.state IN ('retry', 'uncertain', 'failed')
                      AND attempt.state = NEW.state))
         )
         BEGIN SELECT RAISE(ABORT, 'provider delivery state mismatch'); END;
         CREATE TRIGGER provider_delivery_state_fence_update
         BEFORE UPDATE OF state, receipt_object_ref, delivered_at ON provider_deliveries
         WHEN NOT EXISTS (
           SELECT 1 FROM wake_attempts attempt
           JOIN outbox wake ON wake.wake_id = attempt.wake_id
           JOIN protected_objects request ON request.object_ref = NEW.request_object_ref
           JOIN activation_epochs activation ON activation.activation_id = NEW.activation_id
           LEFT JOIN protected_objects receipt ON receipt.object_ref = NEW.receipt_object_ref
           WHERE attempt.wake_id = NEW.wake_id AND attempt.attempt = NEW.attempt
             AND attempt.adapter_id = NEW.adapter_id
             AND request.kind = 'provider_request'
             AND request.work_item_id = wake.work_item_id
             AND activation.work_item_id = wake.work_item_id
             AND activation.work_generation = wake.work_generation
             AND activation.owner_generation = wake.owner_generation
             AND ((NEW.state IN ('prepared', 'launched') AND activation.state = 'active'
                   AND attempt.state = 'claimed')
                  OR (NEW.state = 'delivered' AND attempt.state = 'delivered'
                      AND receipt.kind = 'provider_receipt'
                      AND receipt.work_item_id = wake.work_item_id
                      AND receipt.content_digest = attempt.outcome_digest)
                  OR (NEW.state IN ('retry', 'uncertain', 'failed')
                      AND attempt.state = NEW.state))
         )
         BEGIN SELECT RAISE(ABORT, 'provider delivery state mismatch'); END;
         CREATE TRIGGER agent_ownership_insert_fence
         BEFORE INSERT ON agent_ownership
         WHEN NOT EXISTS (
           SELECT 1 FROM provider_deliveries delivery
           JOIN outbox wake ON wake.wake_id = delivery.wake_id
           JOIN protected_objects profile
             ON profile.object_ref = NEW.launch_profile_object_ref
           WHERE delivery.delivery_id = NEW.delivery_id
             AND delivery.state = 'delivered'
             AND wake.work_item_id = NEW.work_item_id
             AND wake.work_generation = NEW.work_generation
             AND wake.owner_generation = NEW.owner_generation
             AND profile.work_item_id = NEW.work_item_id
             AND profile.kind = 'launch_profile'
             AND profile.profile_ref = wake.profile_ref
             AND profile.content_digest = wake.payload_digest
             AND (NEW.state NOT IN ('acknowledged', 'returned') OR EXISTS (
               SELECT 1 FROM protected_objects receipt
               WHERE receipt.object_ref = NEW.context_receipt_object_ref
                 AND receipt.work_item_id = NEW.work_item_id
                 AND receipt.kind = 'agent_receipt'
                 AND receipt.content_digest = NEW.context_receipt_digest
             ))
         )
         BEGIN SELECT RAISE(ABORT, 'agent ownership authority mismatch'); END;
         CREATE TRIGGER agent_ownership_identity_immutable
         BEFORE UPDATE OF ownership_id, work_item_id, work_generation, owner_generation,
                          delivery_id, launch_profile_object_ref, created_at ON agent_ownership
         BEGIN SELECT RAISE(ABORT, 'agent ownership identity is immutable'); END;
         CREATE TRIGGER agent_ownership_no_delete
         BEFORE DELETE ON agent_ownership
         BEGIN SELECT RAISE(ABORT, 'agent ownership cannot be deleted'); END;
         CREATE TRIGGER agent_ownership_context_fence
         BEFORE UPDATE OF state, context_receipt_digest, context_receipt_object_ref,
                          acknowledged_at, returned_at ON agent_ownership
         WHEN NEW.state IN ('acknowledged', 'returned') AND NOT EXISTS (
           SELECT 1 FROM protected_objects receipt
           WHERE receipt.object_ref = NEW.context_receipt_object_ref
             AND receipt.work_item_id = NEW.work_item_id
             AND receipt.kind = 'agent_receipt'
             AND receipt.content_digest = NEW.context_receipt_digest
         )
         BEGIN SELECT RAISE(ABORT, 'agent ownership receipt mismatch'); END;
         CREATE TRIGGER agent_ownership_state_fence
         BEFORE UPDATE OF state ON agent_ownership
         WHEN (OLD.state = 'pending' AND NEW.state NOT IN ('pending', 'acknowledged',
                                                           'uncertain', 'failed'))
            OR (OLD.state = 'acknowledged' AND NEW.state NOT IN ('acknowledged', 'returned'))
            OR (OLD.state = 'uncertain'
                AND NEW.state NOT IN ('uncertain', 'acknowledged', 'failed'))
            OR (OLD.state IN ('returned', 'failed') AND NEW.state != OLD.state)
         BEGIN SELECT RAISE(ABORT, 'agent ownership transition is not monotonic'); END;
         CREATE TRIGGER agent_ownership_receipt_immutable
         BEFORE UPDATE OF context_receipt_object_ref, context_receipt_digest,
                          acknowledged_at ON agent_ownership
         WHEN OLD.state IN ('acknowledged', 'returned')
           AND (OLD.context_receipt_object_ref IS NOT NEW.context_receipt_object_ref
                OR OLD.context_receipt_digest IS NOT NEW.context_receipt_digest
                OR OLD.acknowledged_at IS NOT NEW.acknowledged_at)
         BEGIN SELECT RAISE(ABORT, 'agent ownership receipt is immutable'); END;
         CREATE TRIGGER outbox_acknowledged_fence
         BEFORE UPDATE OF state ON outbox
         WHEN NEW.state = 'acknowledged' AND NOT EXISTS (
           SELECT 1 FROM agent_ownership ownership
           WHERE ownership.delivery_id = NEW.provider_delivery_id
             AND ownership.state IN ('acknowledged', 'returned')
             AND ownership.work_item_id = NEW.work_item_id
         )
         BEGIN SELECT RAISE(ABORT, 'wake acknowledgement lacks agent ownership'); END;
         CREATE TRIGGER outbox_acknowledged_insert_fence
         BEFORE INSERT ON outbox WHEN NEW.state = 'acknowledged'
         BEGIN SELECT RAISE(ABORT, 'wake cannot be created acknowledged'); END;
         CREATE TRIGGER wake_attempt_acknowledged_insert_fence
         BEFORE INSERT ON wake_attempts WHEN NEW.state = 'acknowledged'
         BEGIN SELECT RAISE(ABORT, 'wake attempt cannot be created acknowledged'); END;
         CREATE TRIGGER wake_attempt_acknowledged_update_fence
         BEFORE UPDATE OF state ON wake_attempts
         WHEN NEW.state = 'acknowledged' AND NOT EXISTS (
           SELECT 1 FROM provider_deliveries delivery
           JOIN agent_ownership ownership ON ownership.delivery_id = delivery.delivery_id
           WHERE delivery.wake_id = NEW.wake_id AND delivery.attempt = NEW.attempt
             AND delivery.state = 'delivered'
             AND ownership.state IN ('acknowledged', 'returned')
         )
         BEGIN SELECT RAISE(ABORT, 'wake attempt acknowledgement lacks agent ownership'); END;
         PRAGMA user_version = 5;",
    )?;
    validate_relational_integrity(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v5_to_v6(connection: &mut Connection) -> WorkLedgerResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    transaction.execute_batch(
        "CREATE TABLE provider_delivery_observations (
           observation_id TEXT PRIMARY KEY
             CHECK(length(observation_id) = 67 AND substr(observation_id, 1, 3) = 'ro_'
                   AND substr(observation_id, 4) NOT GLOB '*[^0-9a-f]*'),
           delivery_id TEXT NOT NULL REFERENCES provider_deliveries(delivery_id) ON DELETE RESTRICT,
           sequence INTEGER NOT NULL CHECK(sequence > 0),
           work_generation INTEGER NOT NULL CHECK(work_generation > 0),
           owner_generation INTEGER NOT NULL CHECK(owner_generation > 0),
           from_state TEXT NOT NULL CHECK(from_state IN ('prepared', 'launched', 'uncertain')),
           to_state TEXT NOT NULL CHECK(to_state IN ('delivered', 'retry', 'uncertain', 'failed')),
           receipt_object_ref TEXT NOT NULL REFERENCES protected_objects(object_ref) ON DELETE RESTRICT,
           outcome_digest TEXT NOT NULL
             CHECK(length(outcome_digest) = 64 AND outcome_digest NOT GLOB '*[^0-9a-f]*'),
           observed_at TEXT NOT NULL CHECK(length(observed_at) >= 20),
           UNIQUE(delivery_id, sequence)
         );
         CREATE INDEX provider_delivery_observations_delivery
           ON provider_delivery_observations(delivery_id, sequence);
         CREATE TRIGGER provider_delivery_observation_immutable
         BEFORE UPDATE ON provider_delivery_observations
         BEGIN SELECT RAISE(ABORT, 'provider delivery observations are immutable'); END;
         CREATE TRIGGER provider_delivery_observation_no_delete
         BEFORE DELETE ON provider_delivery_observations
         BEGIN SELECT RAISE(ABORT, 'provider delivery observations cannot be deleted'); END;
         CREATE TRIGGER provider_delivery_observation_insert_fence
         BEFORE INSERT ON provider_delivery_observations
         WHEN NOT EXISTS (
           SELECT 1 FROM provider_deliveries delivery
           JOIN outbox wake ON wake.wake_id = delivery.wake_id
           JOIN protected_objects receipt ON receipt.object_ref = NEW.receipt_object_ref
           WHERE delivery.delivery_id = NEW.delivery_id
             AND delivery.state = NEW.from_state
             AND ((NEW.from_state IN ('prepared', 'launched') AND wake.state = 'claimed')
                  OR (NEW.from_state = 'uncertain' AND wake.state = 'uncertain'))
             AND wake.work_generation = NEW.work_generation
             AND wake.owner_generation = NEW.owner_generation
             AND receipt.kind = 'provider_receipt'
             AND receipt.work_item_id = wake.work_item_id
             AND receipt.content_digest = NEW.outcome_digest
             AND NEW.sequence = coalesce((
               SELECT max(previous.sequence) + 1
                 FROM provider_delivery_observations previous
                WHERE previous.delivery_id = NEW.delivery_id
             ), 1)
         )
         BEGIN SELECT RAISE(ABORT, 'provider delivery observation authority mismatch'); END;
         PRAGMA user_version = 6;",
    )?;
    validate_relational_integrity(&transaction)?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn verify_supported_schema(connection: &Connection) -> WorkLedgerResult<()> {
    let version = schema_version(connection)?;
    if version != SCHEMA_VERSION {
        return Err(WorkLedgerError::UnsupportedSchema(version));
    }
    verify_schema_identity(connection)
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
    verify_schema_identity(connection)?;
    validate_relational_integrity(connection)?;
    Ok(verdict)
}

#[allow(clippy::too_many_lines)]
fn validate_relational_integrity(connection: &Connection) -> WorkLedgerResult<()> {
    let foreign_key_violations: i64 =
        connection.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_key_violations != 0 {
        return Err(WorkLedgerError::Refused(
            "work ledger contains foreign-key violations".to_owned(),
        ));
    }
    if schema_version(connection)? < 5 {
        return Ok(());
    }
    if schema_version(connection)? >= 9 {
        super::durable_custody::validate_persisted_custody(connection)?;
    }
    let invalid_object_names: i64 = connection.query_row(
        "SELECT COUNT(*) FROM protected_objects
          WHERE storage_name != 'object-' || substr(object_ref, 4) || '.blob'",
        [],
        |row| row.get(0),
    )?;
    if invalid_object_names != 0 {
        return Err(WorkLedgerError::Refused(
            "protected object filename is not derived from its identity".to_owned(),
        ));
    }
    let invalid_deliveries: i64 = connection.query_row(
        "SELECT COUNT(*)
           FROM outbox o
           LEFT JOIN provider_deliveries d ON d.delivery_id = o.provider_delivery_id
           LEFT JOIN protected_objects receipt ON receipt.object_ref = d.receipt_object_ref
           LEFT JOIN protected_objects profile
             ON profile.work_item_id = o.work_item_id AND profile.profile_ref = o.profile_ref
          WHERE o.state = 'delivered'
            AND (d.delivery_id IS NULL OR d.wake_id != o.wake_id
                 OR d.state != 'delivered' OR receipt.kind != 'provider_receipt'
                 OR receipt.work_item_id != o.work_item_id
                 OR profile.kind != 'launch_profile'
                 OR profile.content_digest != o.payload_digest
                 OR o.transport_receipt_digest != receipt.content_digest
                 OR NOT EXISTS (
                     SELECT 1 FROM wake_attempts attempt
                      WHERE attempt.wake_id = o.wake_id
                        AND attempt.attempt = d.attempt
                        AND attempt.state = 'delivered'
                        AND attempt.outcome_digest = receipt.content_digest
                 ))",
        [],
        |row| row.get(0),
    )?;
    if invalid_deliveries != 0 {
        return Err(WorkLedgerError::Refused(
            "delivered wake lacks its exact provider delivery and receipt".to_owned(),
        ));
    }
    let invalid_provider_bindings: i64 = connection.query_row(
        "SELECT COUNT(*)
           FROM provider_deliveries delivery
           JOIN outbox wake ON wake.wake_id = delivery.wake_id
           JOIN protected_objects request
             ON request.object_ref = delivery.request_object_ref
           JOIN activation_epochs activation
             ON activation.activation_id = delivery.activation_id
          WHERE request.kind != 'provider_request'
             OR request.work_item_id != wake.work_item_id
             OR delivery.adapter_id != (
                 SELECT attempt.adapter_id FROM wake_attempts attempt
                  WHERE attempt.wake_id = delivery.wake_id
                    AND attempt.attempt = delivery.attempt
             )
             OR (delivery.state IN ('prepared', 'launched')
                 AND activation.state != 'active')
             OR (delivery.state IN ('prepared', 'launched') AND NOT EXISTS (
                 SELECT 1 FROM wake_attempts attempt
                  WHERE attempt.wake_id = delivery.wake_id
                    AND attempt.attempt = delivery.attempt
                    AND attempt.state = 'claimed'
             ))
             OR (delivery.state IN ('retry', 'uncertain', 'failed') AND NOT EXISTS (
                 SELECT 1 FROM wake_attempts attempt
                  WHERE attempt.wake_id = delivery.wake_id
                    AND attempt.attempt = delivery.attempt
                    AND attempt.state = delivery.state
             ))
             OR activation.work_item_id != wake.work_item_id
             OR activation.work_generation != wake.work_generation
             OR activation.owner_generation != wake.owner_generation",
        [],
        |row| row.get(0),
    )?;
    if invalid_provider_bindings != 0 {
        return Err(WorkLedgerError::Refused(
            "provider delivery is not bound to its exact work and request".to_owned(),
        ));
    }
    let invalid_ownership: i64 = connection.query_row(
        "SELECT COUNT(*)
           FROM agent_ownership ownership
           JOIN provider_deliveries delivery
             ON delivery.delivery_id = ownership.delivery_id
           JOIN outbox wake ON wake.wake_id = delivery.wake_id
           JOIN protected_objects profile
             ON profile.object_ref = ownership.launch_profile_object_ref
           LEFT JOIN protected_objects context_receipt
             ON context_receipt.object_ref = ownership.context_receipt_object_ref
          WHERE delivery.state != 'delivered'
             OR profile.kind != 'launch_profile'
             OR profile.work_item_id != ownership.work_item_id
             OR profile.profile_ref != wake.profile_ref
             OR profile.content_digest != wake.payload_digest
             OR wake.work_item_id != ownership.work_item_id
             OR wake.work_generation != ownership.work_generation
             OR wake.owner_generation != ownership.owner_generation
             OR (ownership.state IN ('acknowledged', 'returned')
                 AND (context_receipt.kind != 'agent_receipt'
                      OR context_receipt.work_item_id != ownership.work_item_id
                      OR context_receipt.content_digest != ownership.context_receipt_digest))",
        [],
        |row| row.get(0),
    )?;
    if invalid_ownership != 0 {
        return Err(WorkLedgerError::Refused(
            "agent ownership is not bound to a delivered launch profile".to_owned(),
        ));
    }
    if schema_version(connection)? >= 6 {
        let invalid_observations: i64 = connection.query_row(
            "SELECT COUNT(*)
               FROM provider_delivery_observations observation
               JOIN provider_deliveries delivery
                 ON delivery.delivery_id = observation.delivery_id
               JOIN outbox wake ON wake.wake_id = delivery.wake_id
               JOIN protected_objects receipt
                 ON receipt.object_ref = observation.receipt_object_ref
              WHERE observation.work_generation != wake.work_generation
                 OR observation.owner_generation != wake.owner_generation
                 OR receipt.kind != 'provider_receipt'
                 OR receipt.work_item_id != wake.work_item_id
                 OR receipt.content_digest != observation.outcome_digest
                 OR observation.sequence != (
                   SELECT COUNT(*) FROM provider_delivery_observations prior
                    WHERE prior.delivery_id = observation.delivery_id
                      AND prior.sequence <= observation.sequence
                 )",
            [],
            |row| row.get(0),
        )?;
        if invalid_observations != 0 {
            return Err(WorkLedgerError::Refused(
                "provider delivery observation history is not exact".to_owned(),
            ));
        }
    }
    Ok(())
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
        "protected_objects" => "SELECT COUNT(*) FROM protected_objects",
        "provider_deliveries" => "SELECT COUNT(*) FROM provider_deliveries",
        "agent_ownership" => "SELECT COUNT(*) FROM agent_ownership",
        "activation_epochs" => "SELECT COUNT(*) FROM activation_epochs",
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
    let sql = match (table, column) {
        ("outbox", "state") => "SELECT COUNT(*) FROM outbox WHERE state = ?1",
        ("projection_intents", "state") => {
            "SELECT COUNT(*) FROM projection_intents WHERE state = ?1"
        }
        _ => {
            return Err(WorkLedgerError::Refused(
                "unsupported filtered count".to_owned(),
            ));
        }
    };
    Ok(connection.query_row(sql, [value], |row| row.get(0))?)
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
