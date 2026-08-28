//! Immutable protected objects referenced by the canonical work ledger.

use std::collections::BTreeMap;
use std::io::{Read, Write};

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::route::OpaqueRef;
use super::{
    Utc, WorkLedger, WorkLedgerError, WorkLedgerResult, configure_durable, digest, opaque_ref,
    validate_digest, verify_integrity, verify_supported_schema,
};

const PROTECTED_OBJECT_DIRECTORY: &str = "protected-objects";
const MAX_PROTECTED_OBJECT_BYTES: usize = 1_048_576;
const MAX_PROTECTED_OBJECT_ROWS: usize = 4_096;
const MAX_PROTECTED_OBJECT_TOTAL_BYTES: u64 = 16 * 1_048_576;

/// Closed object families accepted by the protected store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProtectedObjectKind {
    LaunchProfile,
    ProviderRequest,
    ProviderReceipt,
    AgentReceipt,
}

impl ProtectedObjectKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::LaunchProfile => "launch_profile",
            Self::ProviderRequest => "provider_request",
            Self::ProviderReceipt => "provider_receipt",
            Self::AgentReceipt => "agent_receipt",
        }
    }
}

/// Immutable metadata for one protected object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProtectedObjectRecord {
    pub(crate) object_ref: String,
    pub(crate) work_item_id: String,
    pub(crate) kind: String,
    pub(crate) profile_ref: Option<String>,
    pub(crate) content_digest: String,
    pub(crate) byte_length: u64,
}

impl WorkLedger {
    /// Remove only safe, unpublished temporary objects while the caller owns
    /// the work-ledger writer domain. Final object files are never removed.
    pub(super) fn reconcile_protected_object_storage(&self) -> WorkLedgerResult<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        if let Some(directory) = try_open_object_directory(parent, false)? {
            reconcile_pending_objects(&directory)?;
            verify_directory_binding(parent, &directory)?;
        }
        Ok(())
    }

    /// Verify that every registered object has one exact protected file and
    /// that the protected directory contains no unregistered or pending file.
    pub(super) fn verify_protected_object_storage(
        &self,
        connection: &rusqlite::Connection,
    ) -> WorkLedgerResult<()> {
        let mut statement = connection.prepare(
            "SELECT object_ref, work_item_id, kind, profile_ref, storage_name,
                    content_digest, byte_length
               FROM protected_objects ORDER BY object_ref",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(4)?,
                ProtectedObjectRecord {
                    object_ref: row.get(0)?,
                    work_item_id: row.get(1)?,
                    kind: row.get(2)?,
                    profile_ref: row.get(3)?,
                    content_digest: row.get(5)?,
                    byte_length: row.get(6)?,
                },
            ))
        })?;
        let mut expected = rows.collect::<Result<BTreeMap<_, _>, _>>()?;
        let total_bytes = expected
            .values()
            .try_fold(0_u64, |total, record| total.checked_add(record.byte_length))
            .ok_or_else(|| {
                WorkLedgerError::Refused("protected object integrity size overflow".to_owned())
            })?;
        if expected.len() > MAX_PROTECTED_OBJECT_ROWS
            || total_bytes > MAX_PROTECTED_OBJECT_TOTAL_BYTES
        {
            return Err(WorkLedgerError::Refused(
                "protected object integrity scan exceeds its bound".to_owned(),
            ));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        scan_object_directory(parent, &mut expected)?;
        if !expected.is_empty() {
            return Err(WorkLedgerError::Refused(
                "registered protected object file is missing".to_owned(),
            ));
        }
        Ok(())
    }

    /// Persist an immutable bounded object and its exact metadata.
    ///
    /// The expected digest is caller-reviewed authority. An exact replay is a
    /// no-op; a metadata or byte collision fails closed.
    pub(crate) fn put_protected_object(
        &self,
        work_item_id: &str,
        kind: ProtectedObjectKind,
        profile_ref: Option<&str>,
        expected_digest: &str,
        bytes: &[u8],
    ) -> WorkLedgerResult<ProtectedObjectRecord> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        self.put_protected_object_with_writer_domain(
            work_item_id,
            kind,
            profile_ref,
            expected_digest,
            bytes,
        )
    }

    /// Persist while the caller already owns this ledger's writer domain.
    pub(super) fn put_protected_object_with_writer_domain(
        &self,
        work_item_id: &str,
        kind: ProtectedObjectKind,
        profile_ref: Option<&str>,
        expected_digest: &str,
        bytes: &[u8],
    ) -> WorkLedgerResult<ProtectedObjectRecord> {
        validate_digest("protected object digest", expected_digest)?;
        if bytes.len() > MAX_PROTECTED_OBJECT_BYTES {
            return Err(WorkLedgerError::Refused(
                "protected object exceeds the byte limit".to_owned(),
            ));
        }
        if digest(bytes) != expected_digest {
            return Err(WorkLedgerError::Refused(
                "protected object digest does not match its bytes".to_owned(),
            ));
        }
        validate_profile_ref(kind, profile_ref)?;
        let object_ref = derive_object_ref(
            work_item_id,
            kind,
            profile_ref,
            expected_digest,
            bytes.len(),
        );
        let storage_name = storage_name(&object_ref)?;
        let record = ProtectedObjectRecord {
            object_ref: object_ref.clone(),
            work_item_id: work_item_id.to_owned(),
            kind: kind.as_str().to_owned(),
            profile_ref: profile_ref.map(ToOwned::to_owned),
            content_digest: expected_digest.to_owned(),
            byte_length: bytes.len() as u64,
        };
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let directory = open_object_directory(parent, true)?;
        reconcile_pending_objects(&directory)?;
        let mut connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let existing = protected_object_record(&connection, &object_ref)?;
        if let Some(existing) = existing {
            if existing != record {
                return Err(WorkLedgerError::Refused(
                    "protected object identity collides with different metadata".to_owned(),
                ));
            }
            let observed = read_object_from_directory(&directory, &storage_name, &record)?;
            if observed != bytes {
                return Err(WorkLedgerError::Refused(
                    "protected object replay bytes differ".to_owned(),
                ));
            }
            self.verify_protected_object_storage(&connection)?;
            return Ok(existing);
        }
        let work_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_items WHERE id = ?1)",
            [work_item_id],
            |row| row.get(0),
        )?;
        if !work_exists {
            return Err(WorkLedgerError::Refused(
                "protected object work item does not exist".to_owned(),
            ));
        }
        if let Some(profile_ref) = profile_ref {
            let collision: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM protected_objects
                                WHERE work_item_id = ?1 AND profile_ref = ?2)",
                params![work_item_id, profile_ref],
                |row| row.get(0),
            )?;
            if collision {
                return Err(WorkLedgerError::Refused(
                    "launch profile already binds a different protected object".to_owned(),
                ));
            }
        }
        let current_count: u64 =
            connection.query_row("SELECT COUNT(*) FROM protected_objects", [], |row| {
                row.get(0)
            })?;
        let current_bytes: u64 = connection.query_row(
            "SELECT COALESCE(SUM(byte_length), 0) FROM protected_objects",
            [],
            |row| row.get(0),
        )?;
        if current_count >= MAX_PROTECTED_OBJECT_ROWS as u64
            || current_bytes
                .checked_add(record.byte_length)
                .is_none_or(|total| total > MAX_PROTECTED_OBJECT_TOTAL_BYTES)
        {
            return Err(WorkLedgerError::Refused(
                "protected object store exceeds its aggregate bound".to_owned(),
            ));
        }
        write_object_to_directory(&directory, &storage_name, &record, bytes)?;
        verify_directory_binding(parent, &directory)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO protected_objects
             (object_ref, work_item_id, kind, profile_ref, storage_name,
              content_digest, byte_length, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                record.object_ref,
                record.work_item_id,
                record.kind,
                record.profile_ref,
                storage_name,
                record.content_digest,
                record.byte_length,
                Utc::now().to_rfc3339(),
            ],
        )?;
        transaction.commit()?;
        verify_directory_binding(parent, &directory)?;
        let observed = read_object_from_directory(&directory, &storage_name, &record)?;
        if observed != bytes {
            return Err(WorkLedgerError::Refused(
                "committed protected object bytes differ".to_owned(),
            ));
        }
        let committed = protected_object_record(&connection, &object_ref)?.ok_or_else(|| {
            WorkLedgerError::Refused("protected object was not visible after commit".to_owned())
        })?;
        self.verify_protected_object_storage(&connection)?;
        Ok(committed)
    }

    /// Open and verify one immutable object through a pinned no-follow file.
    pub(crate) fn open_protected_object(
        &self,
        object_ref: &str,
    ) -> WorkLedgerResult<(ProtectedObjectRecord, Vec<u8>)> {
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let record = protected_object_record(&connection, object_ref)?.ok_or_else(|| {
            WorkLedgerError::Refused("protected object is not registered".to_owned())
        })?;
        let storage_name = storage_name(&record.object_ref)?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let bytes = read_object_file(parent, &storage_name, &record)?;
        Ok((record, bytes))
    }
}

fn validate_profile_ref(
    kind: ProtectedObjectKind,
    profile_ref: Option<&str>,
) -> WorkLedgerResult<()> {
    match (kind, profile_ref) {
        (ProtectedObjectKind::LaunchProfile, Some(value)) => OpaqueRef::parse(value.to_owned())
            .map(|_| ())
            .map_err(|_| WorkLedgerError::Refused("invalid launch profile reference".to_owned())),
        (ProtectedObjectKind::LaunchProfile, None) => Err(WorkLedgerError::Refused(
            "launch profile object requires its route profile reference".to_owned(),
        )),
        (_, None) => Ok(()),
        (_, Some(_)) => Err(WorkLedgerError::Refused(
            "only launch profile objects may carry a profile reference".to_owned(),
        )),
    }
}

pub(super) fn derive_object_ref(
    work_item_id: &str,
    kind: ProtectedObjectKind,
    profile_ref: Option<&str>,
    content_digest: &str,
    byte_length: usize,
) -> String {
    opaque_ref(
        "po",
        &format!(
            "shipyard-protected-object-v1\0{work_item_id}\n{}\n{}\n{content_digest}\n{byte_length}",
            kind.as_str(),
            profile_ref.unwrap_or("")
        ),
    )
}

pub(super) fn storage_name(object_ref: &str) -> WorkLedgerResult<String> {
    let digest = object_ref.strip_prefix("po_").ok_or_else(|| {
        WorkLedgerError::Refused("protected object reference is malformed".to_owned())
    })?;
    validate_digest("protected object reference", digest)?;
    Ok(format!("object-{digest}.blob"))
}

fn protected_object_record(
    connection: &rusqlite::Connection,
    object_ref: &str,
) -> WorkLedgerResult<Option<ProtectedObjectRecord>> {
    Ok(connection
        .query_row(
            "SELECT object_ref, work_item_id, kind, profile_ref,
                    content_digest, byte_length
               FROM protected_objects WHERE object_ref = ?1",
            [object_ref],
            |row| {
                Ok(ProtectedObjectRecord {
                    object_ref: row.get(0)?,
                    work_item_id: row.get(1)?,
                    kind: row.get(2)?,
                    profile_ref: row.get(3)?,
                    content_digest: row.get(4)?,
                    byte_length: row.get(5)?,
                })
            },
        )
        .optional()?)
}

#[cfg(unix)]
fn try_open_object_directory(
    parent: &std::path::Path,
    create: bool,
) -> WorkLedgerResult<Option<std::fs::File>> {
    use rustix::fs::{Mode, OFlags, fchmod, mkdirat, open, openat};
    use std::os::unix::fs::PermissionsExt;

    let parent_directory = std::fs::File::from(
        open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let created = create
        && match mkdirat(&parent_directory, PROTECTED_OBJECT_DIRECTORY, Mode::RWXU) {
            Ok(()) => true,
            Err(rustix::io::Errno::EXIST) => false,
            Err(error) => return Err(std::io::Error::from(error).into()),
        };
    let directory = match openat(
        &parent_directory,
        PROTECTED_OBJECT_DIRECTORY,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(directory) => std::fs::File::from(directory),
        Err(rustix::io::Errno::NOENT) if !create => return Ok(None),
        Err(error) => return Err(std::io::Error::from(error).into()),
    };
    if created {
        fchmod(&directory, Mode::RWXU).map_err(std::io::Error::from)?;
        parent_directory.sync_all()?;
        directory.sync_all()?;
    }
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(WorkLedgerError::Refused(
            "protected object directory is not a 0700 directory".to_owned(),
        ));
    }
    Ok(Some(directory))
}

#[cfg(unix)]
fn open_object_directory(
    parent: &std::path::Path,
    create: bool,
) -> WorkLedgerResult<std::fs::File> {
    try_open_object_directory(parent, create)?
        .ok_or_else(|| WorkLedgerError::Refused("protected object directory is missing".to_owned()))
}

#[cfg(not(unix))]
fn try_open_object_directory(
    parent: &std::path::Path,
    create: bool,
) -> WorkLedgerResult<Option<std::fs::File>> {
    match std::fs::symlink_metadata(parent.join(PROTECTED_OBJECT_DIRECTORY)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(WorkLedgerError::Refused(
                "protected objects require no-follow file descriptors on this platform".to_owned(),
            ))
        }
        Err(error) => Err(error.into()),
        Ok(_) => Err(WorkLedgerError::Refused(
            "protected objects require no-follow file descriptors on this platform".to_owned(),
        )),
    }
}

#[cfg(unix)]
fn verify_directory_binding(
    parent: &std::path::Path,
    directory: &std::fs::File,
) -> WorkLedgerResult<()> {
    use rustix::fs::{Mode, OFlags, open, openat};
    use std::os::unix::fs::MetadataExt;

    let parent_directory = std::fs::File::from(
        open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let rebound = std::fs::File::from(
        openat(
            &parent_directory,
            PROTECTED_OBJECT_DIRECTORY,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let pinned = directory.metadata()?;
    let observed = rebound.metadata()?;
    if pinned.dev() != observed.dev() || pinned.ino() != observed.ino() {
        return Err(WorkLedgerError::Refused(
            "protected object directory binding changed".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_object_file(
    file: &std::fs::File,
    record: &ProtectedObjectRecord,
) -> WorkLedgerResult<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() != record.byte_length
        || metadata.len() > MAX_PROTECTED_OBJECT_BYTES as u64
    {
        return Err(WorkLedgerError::Refused(
            "protected object file metadata is invalid".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn read_object_file(
    parent: &std::path::Path,
    name: &str,
    record: &ProtectedObjectRecord,
) -> WorkLedgerResult<Vec<u8>> {
    let directory = open_object_directory(parent, false)?;
    read_object_from_directory(&directory, name, record)
}

#[cfg(unix)]
fn read_object_from_directory(
    directory: &std::fs::File,
    name: &str,
    record: &ProtectedObjectRecord,
) -> WorkLedgerResult<Vec<u8>> {
    use rustix::fs::{Mode, OFlags, openat};

    let mut file = std::fs::File::from(
        openat(
            directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    validate_object_file(&file, record)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_PROTECTED_OBJECT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != record.byte_length || digest(&bytes) != record.content_digest {
        return Err(WorkLedgerError::Refused(
            "protected object bytes do not match registered metadata".to_owned(),
        ));
    }
    validate_object_file(&file, record)?;
    Ok(bytes)
}

#[cfg(unix)]
fn reconcile_pending_objects(directory: &std::fs::File) -> WorkLedgerResult<()> {
    use rustix::fs::{AtFlags, Mode, OFlags, openat, unlinkat};
    let entries = rustix::fs::Dir::read_from(directory).map_err(std::io::Error::from)?;
    for entry in entries {
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry.file_name().to_bytes();
        if !name.starts_with(b".pending-") {
            continue;
        }
        let name = std::str::from_utf8(name).map_err(|_| {
            WorkLedgerError::Refused("pending protected object name is not UTF-8".to_owned())
        })?;
        let file = std::fs::File::from(
            openat(
                directory,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        );
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.permissions().mode() & 0o777 != 0o600
            || metadata.nlink() != 1
            || metadata.len() > MAX_PROTECTED_OBJECT_BYTES as u64
        {
            return Err(WorkLedgerError::Refused(
                "pending protected object is unsafe to reconcile".to_owned(),
            ));
        }
        unlinkat(directory, name, AtFlags::empty()).map_err(std::io::Error::from)?;
    }
    directory.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn scan_object_directory(
    parent: &std::path::Path,
    expected: &mut BTreeMap<String, ProtectedObjectRecord>,
) -> WorkLedgerResult<()> {
    let Some(directory) = try_open_object_directory(parent, false)? else {
        return Ok(());
    };
    let mut names = rustix::fs::Dir::read_from(&directory)
        .map_err(std::io::Error::from)?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_bytes().to_vec())
                .map_err(std::io::Error::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    names.retain(|name| name.as_slice() != b"." && name.as_slice() != b"..");
    if names.len() > MAX_PROTECTED_OBJECT_ROWS {
        return Err(WorkLedgerError::Refused(
            "protected object directory exceeds its entry bound".to_owned(),
        ));
    }
    names.sort();
    for name in names {
        let name = std::str::from_utf8(&name).map_err(|_| {
            WorkLedgerError::Refused("protected object filename is not UTF-8".to_owned())
        })?;
        let Some(record) = expected.remove(name) else {
            return Err(WorkLedgerError::Refused(
                "protected object directory contains an unregistered entry".to_owned(),
            ));
        };
        read_object_from_directory(&directory, name, &record)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn scan_object_directory(
    _parent: &std::path::Path,
    expected: &mut BTreeMap<String, ProtectedObjectRecord>,
) -> WorkLedgerResult<()> {
    if expected.is_empty() {
        Ok(())
    } else {
        Err(WorkLedgerError::Refused(
            "protected objects require no-follow file descriptors on this platform".to_owned(),
        ))
    }
}

#[cfg(all(
    unix,
    any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "redox"
    )
))]
fn write_object_to_directory(
    directory: &std::fs::File,
    name: &str,
    record: &ProtectedObjectRecord,
    bytes: &[u8],
) -> WorkLedgerResult<()> {
    use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, fchmod, openat, renameat_with, unlinkat};

    let pending_name = format!(
        ".pending-{}",
        digest(
            format!(
                "{}:{}:{:?}:{}",
                std::process::id(),
                Utc::now().to_rfc3339(),
                std::thread::current().id(),
                name
            )
            .as_bytes()
        )
    );
    let opened = openat(
        &directory,
        &pending_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    );
    match opened {
        Ok(file) => {
            let result = (|| {
                let mut file = std::fs::File::from(file);
                fchmod(&file, Mode::RUSR | Mode::WUSR).map_err(std::io::Error::from)?;
                file.write_all(bytes)?;
                file.sync_all()?;
                validate_object_file(&file, record)?;
                match renameat_with(
                    &directory,
                    &pending_name,
                    &directory,
                    name,
                    RenameFlags::NOREPLACE,
                ) {
                    Ok(()) => {
                        directory.sync_all()?;
                        let observed = read_object_from_directory(directory, name, record)?;
                        if observed == bytes {
                            Ok(())
                        } else {
                            Err(WorkLedgerError::Refused(
                                "published protected object bytes differ".to_owned(),
                            ))
                        }
                    }
                    Err(rustix::io::Errno::EXIST) => {
                        unlinkat(directory, &pending_name, AtFlags::empty())
                            .map_err(std::io::Error::from)?;
                        directory.sync_all()?;
                        let observed = read_object_from_directory(directory, name, record)?;
                        if observed == bytes {
                            Ok(())
                        } else {
                            Err(WorkLedgerError::Refused(
                                "protected object filename collides with different bytes"
                                    .to_owned(),
                            ))
                        }
                    }
                    Err(error) => Err(std::io::Error::from(error).into()),
                }
            })();
            if result.is_err() {
                let _ = unlinkat(directory, &pending_name, AtFlags::empty());
                let _ = directory.sync_all();
            }
            result
        }
        Err(error) => Err(std::io::Error::from(error).into()),
    }
}

#[cfg(all(
    unix,
    not(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "redox"
    ))
))]
fn write_object_to_directory(
    _directory: &std::fs::File,
    _name: &str,
    _record: &ProtectedObjectRecord,
    _bytes: &[u8],
) -> WorkLedgerResult<()> {
    Err(WorkLedgerError::Refused(
        "protected objects require atomic no-replace publication on this platform".to_owned(),
    ))
}

#[cfg(not(unix))]
fn read_object_file(
    _parent: &std::path::Path,
    _name: &str,
    _record: &ProtectedObjectRecord,
) -> WorkLedgerResult<Vec<u8>> {
    Err(WorkLedgerError::Refused(
        "protected objects require no-follow file descriptors on this platform".to_owned(),
    ))
}

#[cfg(not(unix))]
fn open_object_directory(
    _parent: &std::path::Path,
    _create: bool,
) -> WorkLedgerResult<std::fs::File> {
    Err(WorkLedgerError::Refused(
        "protected objects require no-follow file descriptors on this platform".to_owned(),
    ))
}

#[cfg(not(unix))]
fn read_object_from_directory(
    _directory: &std::fs::File,
    _name: &str,
    _record: &ProtectedObjectRecord,
) -> WorkLedgerResult<Vec<u8>> {
    Err(WorkLedgerError::Refused(
        "protected objects require no-follow file descriptors on this platform".to_owned(),
    ))
}

#[cfg(not(unix))]
fn reconcile_pending_objects(_directory: &std::fs::File) -> WorkLedgerResult<()> {
    Err(WorkLedgerError::Refused(
        "protected objects require no-follow file descriptors on this platform".to_owned(),
    ))
}

#[cfg(not(unix))]
fn verify_directory_binding(
    _parent: &std::path::Path,
    _directory: &std::fs::File,
) -> WorkLedgerResult<()> {
    Err(WorkLedgerError::Refused(
        "protected objects require no-follow file descriptors on this platform".to_owned(),
    ))
}

#[cfg(not(unix))]
fn write_object_to_directory(
    _directory: &std::fs::File,
    _name: &str,
    _record: &ProtectedObjectRecord,
    _bytes: &[u8],
) -> WorkLedgerResult<()> {
    Err(WorkLedgerError::Refused(
        "protected objects require no-follow file descriptors on this platform".to_owned(),
    ))
}
