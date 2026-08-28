//! `SQLite` opening, inspection, and legacy snapshot persistence.

use super::lifecycle::record_event;
use super::{
    Connection, DATABASE_NAME, Duration, ImportCandidate, ImportReport, LedgerStatus,
    LifecycleState, OpenFlags, OptionalExtension, Path, PathBuf, SCHEMA_VERSION,
    TransactionBehavior, Utc, WorkLedger, WorkLedgerError, WorkLedgerResult, configure_durable,
    count, count_where, create_database_file_no_follow, fs, import_report, importer,
    load_ledger_incarnation, migrate, opaque_path_ref, params, protect_database_file,
    protect_ledger_directory, schema_version, synchronous_name, validate_candidate,
    validate_protected_storage, verify_integrity, verify_ledger_incarnation,
    verify_supported_schema,
};

impl WorkLedger {
    /// Return the canonical database path without creating it.
    #[must_use]
    pub fn path_at(state_dir: &Path) -> PathBuf {
        state_dir.join("work-ledger").join(DATABASE_NAME)
    }

    /// Create or open the ledger and apply supported migrations.
    pub fn open(state_dir: &Path) -> WorkLedgerResult<Self> {
        let dir = state_dir.join("work-ledger");
        reject_symlink_if_present(state_dir, &dir, "ledger directory")?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&dir)?;
        crate::writer_domain_lease::ensure_protected_dir_all(&dir)?;
        let path = dir.join(DATABASE_NAME);
        validate_ledger_path(state_dir, &path, false)?;
        let existing = path.exists();
        if existing {
            validate_protected_storage(&dir, &path)?;
            let connection = connect_read_only_raw(&path)?;
            let version = schema_version(&connection)?;
            if !(0..=SCHEMA_VERSION).contains(&version) {
                return Err(WorkLedgerError::UnsupportedSchema(version));
            }
        } else {
            protect_ledger_directory(&dir)?;
            create_database_file_no_follow(&path)?;
            protect_database_file(&path)?;
        }
        let mut connection = connect_read_write_raw(&path)?;
        configure_durable(&connection)?;
        migrate(&mut connection)?;
        verify_integrity(&connection)?;
        let ledger_incarnation_ref = load_ledger_incarnation(&connection)?;
        let ledger = Self {
            path,
            ledger_incarnation_ref,
        };
        Ok(ledger)
    }

    /// Open an existing ledger without creating or migrating it.
    pub fn open_existing(state_dir: &Path) -> WorkLedgerResult<Option<Self>> {
        let dir = state_dir.join("work-ledger");
        reject_symlink_if_present(state_dir, &dir, "ledger directory")?;
        let path = dir.join(DATABASE_NAME);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        validate_ledger_path(state_dir, &path, true)?;
        validate_protected_storage(&dir, &path)?;
        let connection = connect_read_only_raw(&path)?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let ledger = Self {
            path,
            ledger_incarnation_ref: load_ledger_incarnation(&connection)?,
        };
        Ok(Some(ledger))
    }

    /// Database path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Opaque identity of the exact database lifetime opened by this handle.
    #[must_use]
    pub fn ledger_incarnation_ref(&self) -> &str {
        &self.ledger_incarnation_ref
    }

    /// Return redacted operational status.
    pub fn status(&self) -> WorkLedgerResult<LedgerStatus> {
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        let integrity = verify_integrity(&connection)?;
        Ok(LedgerStatus {
            exists: true,
            schema_version: schema_version(&connection)?,
            journal_mode: connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?,
            synchronous: synchronous_name(&connection)?,
            foreign_keys: connection.query_row("PRAGMA foreign_keys", [], |row| {
                row.get::<_, i64>(0).map(|value| {
                    if value == 1 {
                        "enabled".to_owned()
                    } else {
                        "disabled".to_owned()
                    }
                })
            })?,
            integrity,
            work_items: count(&connection, "work_items")?,
            pending_wakes: count_where(&connection, "outbox", "state", "pending")?,
            uncertain_wakes: count_where(&connection, "outbox", "state", "uncertain")?,
            imports: count(&connection, "imports")?,
            activation_enabled: false,
            dispatch_enabled: false,
        })
    }

    /// Import selected legacy projections idempotently under the writer domain.
    pub(crate) fn import_candidates(
        &self,
        candidates: &[ImportCandidate],
    ) -> WorkLedgerResult<ImportReport> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let mut connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();
        let mut inserted = 0;
        let mut updated = 0;
        for candidate in candidates {
            validate_candidate(candidate)?;
            let existing_digest: Option<String> = transaction
                .query_row(
                    "SELECT source_digest FROM work_items WHERE id = ?1",
                    [&candidate.work_id],
                    |row| row.get(0),
                )
                .optional()?;
            match existing_digest.as_deref() {
                None => {
                    transaction.execute(
                        "INSERT INTO work_items (
                   id, kind, repo, pr, head_sha, base_ref, goal_id, goal_generation,
                   lane, role, owner_id, owner_generation, terminal_adapter,
                   agent_adapter, provider_adapter, coordinator_route_ref, repair_route_ref,
                   pr_truth, acceptance_truth, continuation_truth, phase,
                   work_generation, source_digest, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                           ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, 1, ?22, ?23, ?23)",
                        candidate_params!(candidate, &now),
                    )?;
                    inserted += 1;
                }
                Some(digest) if digest == candidate.content_digest => {}
                Some(_) => {
                    let changed = transaction.execute(
                        "UPDATE work_items SET
                           kind = ?2, repo = ?3, pr = ?4, head_sha = ?5, base_ref = ?6,
                           goal_id = ?7, goal_generation = ?8, lane = ?9, role = ?10,
                           owner_id = ?11, owner_generation = ?12, terminal_adapter = ?13,
                           agent_adapter = ?14, provider_adapter = ?15,
                           coordinator_route_ref = ?16, repair_route_ref = ?17,
                           pr_truth = ?18, acceptance_truth = ?19,
                           continuation_truth = ?20, phase = ?21, source_digest = ?22,
                           updated_at = ?23
                         WHERE id = ?1 AND work_generation = 1
                           AND NOT EXISTS (
                             SELECT 1 FROM outbox WHERE work_item_id = work_items.id
                           )
                           AND NOT EXISTS (
                             SELECT 1 FROM continuation_contracts
                             WHERE work_item_id = work_items.id
                           )
                           AND NOT EXISTS (
                             SELECT 1 FROM route_records WHERE work_item_id = work_items.id
                           )",
                        candidate_params!(candidate, &now),
                    )?;
                    if changed != 1 {
                        return Err(WorkLedgerError::Refused(
                            "legacy refresh conflicts with native lifecycle, route, or wake state"
                                .to_owned(),
                        ));
                    }
                    updated += 1;
                }
            }
            transaction.execute(
                "INSERT OR IGNORE INTO imports
                 (source_ref, content_digest, work_item_id, imported_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    candidate.source_ref,
                    candidate.content_digest,
                    candidate.work_id,
                    now,
                ],
            )?;
            record_event(
                &transaction,
                &self.ledger_incarnation_ref,
                None,
                &candidate.work_id,
                1,
                candidate.owner_generation,
                "legacy_import",
                None,
                LifecycleState::ShadowImported,
                &candidate.content_digest,
                &now,
            )?;
        }
        transaction.commit()?;
        Ok(import_report(candidates, true, inserted, updated))
    }

    /// Classify an import through a read-only connection without creating storage.
    pub(crate) fn plan_candidates(
        &self,
        candidates: &[ImportCandidate],
    ) -> WorkLedgerResult<ImportReport> {
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let mut inserted = 0;
        let mut updated = 0;
        for candidate in candidates {
            validate_candidate(candidate)?;
            let existing: Option<(String, u64, bool)> = connection
                .query_row(
                    "SELECT source_digest, work_generation,
                            EXISTS(SELECT 1 FROM outbox WHERE work_item_id = work_items.id)
                            OR EXISTS(SELECT 1 FROM continuation_contracts
                                      WHERE work_item_id = work_items.id)
                            OR EXISTS(SELECT 1 FROM route_records
                                      WHERE work_item_id = work_items.id)
                     FROM work_items WHERE id = ?1",
                    [&candidate.work_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            match existing {
                None => inserted += 1,
                Some((digest, _, _)) if digest == candidate.content_digest => {}
                Some((_, 1, false)) => updated += 1,
                Some(_) => {
                    return Err(WorkLedgerError::Refused(
                        "legacy refresh conflicts with native lifecycle, route, or wake state"
                            .to_owned(),
                    ));
                }
            }
        }
        Ok(import_report(candidates, false, inserted, updated))
    }

    #[cfg(test)]
    pub(super) fn import(&self, candidates: &[ImportCandidate]) -> WorkLedgerResult<ImportReport> {
        self.import_candidates(candidates)
    }

    #[cfg(test)]
    pub(super) fn plan_import(
        &self,
        candidates: &[ImportCandidate],
    ) -> WorkLedgerResult<ImportReport> {
        self.plan_candidates(candidates)
    }

    /// Register one complete route under exact work, owner, and revision fences.
    #[allow(dead_code)] // Trusted adapter installation is activated after shadow cutover.
    pub(super) fn connect_read_write(&self) -> WorkLedgerResult<Connection> {
        let connection = connect_read_write_raw(&self.path)?;
        #[cfg(not(test))]
        verify_ledger_incarnation(&connection, &self.ledger_incarnation_ref)?;
        #[cfg(test)]
        if schema_version(&connection)? == SCHEMA_VERSION {
            verify_ledger_incarnation(&connection, &self.ledger_incarnation_ref)?;
        }
        Ok(connection)
    }

    pub(super) fn connect_read_only(&self) -> WorkLedgerResult<Connection> {
        let connection = connect_read_only_raw(&self.path)?;
        #[cfg(not(test))]
        verify_ledger_incarnation(&connection, &self.ledger_incarnation_ref)?;
        #[cfg(test)]
        if schema_version(&connection)? == SCHEMA_VERSION {
            verify_ledger_incarnation(&connection, &self.ledger_incarnation_ref)?;
        }
        Ok(connection)
    }
}

fn connect_read_write_raw(path: &Path) -> WorkLedgerResult<Connection> {
    let sqlite_path = sqlite_path_with_pinned_final_component(path)?;
    let connection = Connection::open_with_flags(
        sqlite_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    Ok(connection)
}

fn connect_read_only_raw(path: &Path) -> WorkLedgerResult<Connection> {
    let sqlite_path = sqlite_path_with_pinned_final_component(path)?;
    let connection = Connection::open_with_flags(
        sqlite_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA synchronous = FULL;",
    )?;
    Ok(connection)
}

fn sqlite_path_with_pinned_final_component(path: &Path) -> WorkLedgerResult<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
    let name = path
        .file_name()
        .ok_or_else(|| WorkLedgerError::Refused("database has no file name".to_owned()))?;
    // Resolve platform aliases such as macOS /var -> /private/var, but keep the
    // database entry itself unresolved so SQLITE_OPEN_NOFOLLOW still fences a
    // final-component replacement.
    Ok(fs::canonicalize(parent)?.join(name))
}

fn reject_symlink_if_present(state_dir: &Path, path: &Path, label: &str) -> WorkLedgerResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(WorkLedgerError::Refused(format!(
                "{label} {} is a symlink",
                opaque_path_ref(state_dir, path, None)
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_ledger_path(
    state_dir: &Path,
    path: &Path,
    require_database: bool,
) -> WorkLedgerResult<()> {
    let directory = path
        .parent()
        .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
    let directory_metadata = fs::symlink_metadata(directory)?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(WorkLedgerError::Refused(format!(
            "ledger directory {} is not a regular directory",
            opaque_path_ref(state_dir, directory, None)
        )));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
            WorkLedgerError::Refused("ledger database is not a regular file".to_owned()),
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !require_database => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Scan and atomically apply one immutable legacy snapshot.
///
/// The exclusive writer-domain barrier spans source discovery, database open,
/// and transaction commit so no production writer can invalidate the selected
/// snapshot between those boundaries.
pub fn apply_legacy_snapshot(state_dir: &Path) -> WorkLedgerResult<ImportReport> {
    let _snapshot_barrier =
        crate::writer_domain_lease::acquire_exclusive_for_protected_path(state_dir)?;
    let candidates = importer::scan_legacy(state_dir)?;
    let ledger = WorkLedger::open(state_dir)?;
    ledger.import_candidates(&candidates)
}

/// Plan a legacy snapshot without creating or mutating ledger storage.
pub fn plan_legacy_snapshot(state_dir: &Path) -> WorkLedgerResult<ImportReport> {
    let candidates = importer::scan_legacy(state_dir)?;
    WorkLedger::open_existing(state_dir)?.map_or_else(
        || Ok(importer::dry_run_report(&candidates)),
        |ledger| ledger.plan_candidates(&candidates),
    )
}
