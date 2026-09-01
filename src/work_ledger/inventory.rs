//! Bounded zero-write inventory of locally tracked workstreams.

use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{
    LedgerStatus, WorkLedger, WorkLedgerError, WorkLedgerResult, count, count_where,
    is_canonical_repo_slug, synchronous_name, validate_opaque_ref, validate_workstream_handle,
    verify_integrity, verify_open_lineage, verify_supported_schema,
};

/// Maximum number of local work records returned by one inventory call.
pub const MAX_LOCAL_WORK_INVENTORY_ITEMS: usize = 256;

const MAX_LEDGER_DATABASE_BYTES: u64 = 128 * 1024 * 1024;
pub(super) const MAX_SQLITE_SIDECAR_BYTES: u64 = 4 * 1024 * 1024;
const LEGACY_INVENTORY_SCHEMA_VERSION: i64 = 11;

#[derive(Clone, Copy)]
enum InventorySchema {
    Current,
    LegacyV11,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ImmutableInventoryInspection {
    pub(super) schema_version: i64,
    pub(super) inventory: LocalWorkInventory,
}

/// A bounded local inventory response.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalWorkInventory {
    /// SHA-256 of the exact immutable database bytes inspected for this response.
    pub snapshot_sha256: Option<String>,
    /// False when more rows exist than this response can safely carry.
    pub complete: bool,
    /// True when at least one deterministically ordered row was omitted.
    pub truncated: bool,
    /// Hard response bound applied to `items`.
    pub limit: usize,
    /// Deterministically ordered local work identities.
    pub items: Vec<LocalWorkInventoryItem>,
}

/// Immutable identity and current local custody state for one workstream.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalWorkInventoryItem {
    /// Ledger-minted local ownership root; never a caller-selected Linear issue ID.
    pub root_uuid: Option<String>,
    /// Provider host that issued `repository_id`, absent only for migrated legacy rows.
    pub repository_provider: Option<String>,
    /// Provider-scoped immutable repository identity, absent only for migrated legacy rows.
    pub repository_id: Option<String>,
    /// Current canonical lowercase `owner/repository` routing/display coordinate.
    pub repository: String,
    /// Repository-scoped pull-request number.
    pub pull_request: u64,
    /// Exact immutable pull-request head currently bound to this work.
    pub exact_head: String,
    /// Current local work-ledger lifecycle state.
    pub state: String,
    /// Durable external workstream identity.
    pub workstream_handle: String,
    /// Opaque local work-item custody identity.
    pub work_item_id: String,
    /// Current work generation.
    pub work_generation: u64,
    /// Opaque steward identity, when one was recorded.
    pub owner_id: Option<String>,
    /// Current steward generation.
    pub owner_generation: u64,
    /// Opaque agent-ownership identity for the current generations, when present.
    pub ownership_id: Option<String>,
    /// Exact agent-ownership state paired with `ownership_id`, when present.
    pub ownership_state: Option<String>,
    /// Work generation bound into `ownership_id`, when present.
    pub ownership_work_generation: Option<u64>,
    /// Steward generation bound into `ownership_id`, when present.
    pub ownership_owner_generation: Option<u64>,
}

/// Read a bounded local inventory without creating storage, taking a writer-domain
/// lease, reconciling protected objects, or opening the database read-write.
pub fn local_work_inventory(state_dir: &Path) -> WorkLedgerResult<LocalWorkInventory> {
    Ok(local_work_inventory_inspection(state_dir)?.inventory)
}

pub(super) fn local_work_inventory_inspection(
    state_dir: &Path,
) -> WorkLedgerResult<ImmutableInventoryInspection> {
    local_work_inventory_inspection_with_preopen_hook(state_dir, || Ok(()))
}

pub(super) fn database_effective_schema_version(state_dir: &Path) -> WorkLedgerResult<Option<i64>> {
    let path = WorkLedger::path_at(state_dir);
    let Some((database_before, mut database)) = open_regular_file_for_schema_probe(&path)? else {
        return Ok(None);
    };
    let mut database_header = [0_u8; 64];
    database.read_exact(&mut database_header)?;
    let main_schema = database_schema_from_header(&database_header)?;
    if main_schema != LEGACY_INVENTORY_SCHEMA_VERSION {
        return Ok(Some(main_schema));
    }
    let wal_path = sqlite_sidecar(&path, "-wal");
    let Some((wal_before, mut wal)) = open_regular_file_for_schema_probe(&wal_path)? else {
        return Ok(Some(main_schema));
    };
    if wal_before.len == 0 {
        return Ok(Some(main_schema));
    }
    if wal_before.len > MAX_SQLITE_SIDECAR_BYTES {
        return Err(WorkLedgerError::Refused(format!(
            "work ledger WAL exceeds the bounded schema-probe limit of {MAX_SQLITE_SIDECAR_BYTES} bytes"
        )));
    }
    let database_page_size = database_page_size(&database_header)?;
    let schema =
        committed_schema_from_wal(&mut wal, wal_before.len, main_schema, database_page_size)?;
    if regular_file_metadata(&wal_path)?.as_ref() != Some(&wal_before)
        || regular_file_metadata(&path)?.as_ref() != Some(&database_before)
    {
        return Err(WorkLedgerError::Refused(
            "work ledger changed while determining its effective schema".to_owned(),
        ));
    }
    Ok(Some(schema))
}

fn committed_schema_from_wal(
    wal: &mut fs::File,
    wal_len: u64,
    main_schema: i64,
    database_page_size: u64,
) -> WorkLedgerResult<i64> {
    const WAL_HEADER_BYTES: u64 = 32;
    const WAL_HEADER_LEN: usize = 32;
    const FRAME_HEADER_BYTES: u64 = 24;
    const FRAME_HEADER_LEN: usize = 24;
    if wal_len < WAL_HEADER_BYTES {
        return Err(WorkLedgerError::Refused(
            "work ledger WAL header is truncated".to_owned(),
        ));
    }
    let mut header = [0_u8; WAL_HEADER_LEN];
    wal.read_exact(&mut header)?;
    let magic = be_u32(&header, 0)?;
    if !matches!(magic, 0x377f_0682 | 0x377f_0683) || be_u32(&header, 4)? != 3_007_000 {
        return Err(WorkLedgerError::Refused(
            "work ledger WAL header is invalid".to_owned(),
        ));
    }
    let checksum_big_endian = magic == 0x377f_0683;
    let mut checksum = wal_checksum(&header[..24], checksum_big_endian, [0, 0])?;
    if checksum != [be_u32(&header, 24)?, be_u32(&header, 28)?] {
        return Err(WorkLedgerError::Refused(
            "work ledger WAL header checksum is invalid".to_owned(),
        ));
    }
    let page_size = match be_u32(&header, 8)? {
        1 => 65_536_u64,
        value if value.is_power_of_two() && (512..=65_536).contains(&value) => u64::from(value),
        _ => {
            return Err(WorkLedgerError::Refused(
                "work ledger WAL page size is invalid".to_owned(),
            ));
        }
    };
    if page_size != database_page_size {
        return Err(WorkLedgerError::Refused(
            "work ledger WAL page size disagrees with its database".to_owned(),
        ));
    }
    let frame_size = FRAME_HEADER_BYTES
        .checked_add(page_size)
        .ok_or_else(|| WorkLedgerError::Refused("work ledger WAL size overflowed".to_owned()))?;
    let salt = &header[16..24];
    let mut schema = main_schema;
    let mut transaction_schema = None;
    let mut offset = WAL_HEADER_BYTES;
    let mut frame_header = [0_u8; FRAME_HEADER_LEN];
    let page_capacity = usize::try_from(page_size)
        .map_err(|_| WorkLedgerError::Refused("work ledger WAL page is too large".to_owned()))?;
    let mut page = vec![0_u8; page_capacity];
    while offset
        .checked_add(frame_size)
        .is_some_and(|end| end <= wal_len)
    {
        wal.seek(SeekFrom::Start(offset))?;
        wal.read_exact(&mut frame_header)?;
        // SQLite may reset and reuse a WAL without truncating its old physical
        // tail. A salt or rolling-checksum disagreement terminates the valid
        // current-generation prefix; bytes after that boundary are stale, not
        // part of the logical WAL.
        if &frame_header[8..16] != salt {
            break;
        }
        let page_number = be_u32(&frame_header, 0)?;
        if page_number == 0 || page_number == u32::MAX {
            return Err(WorkLedgerError::Refused(
                "work ledger WAL frame page number is invalid".to_owned(),
            ));
        }
        wal.read_exact(&mut page)?;
        checksum = wal_checksum(&frame_header[..8], checksum_big_endian, checksum)?;
        checksum = wal_checksum(&page, checksum_big_endian, checksum)?;
        if checksum != [be_u32(&frame_header, 16)?, be_u32(&frame_header, 20)?] {
            break;
        }
        if page_number == 1 {
            validate_database_page_header(&page, page_size)?;
            transaction_schema = Some(database_schema_from_header(&page)?);
        }
        let commit_database_pages = be_u32(&frame_header, 4)?;
        if commit_database_pages == u32::MAX {
            return Err(WorkLedgerError::Refused(
                "work ledger WAL commit database size is invalid".to_owned(),
            ));
        }
        if commit_database_pages != 0
            && let Some(committed) = transaction_schema.take()
        {
            schema = committed;
        }
        offset += frame_size;
    }
    // A partial final frame is likewise outside SQLite's valid frame prefix.
    Ok(schema)
}

fn validate_database_page_header(page: &[u8], expected_page_size: u64) -> WorkLedgerResult<()> {
    if database_page_size(page)? != expected_page_size
        || page.get(18) != Some(&2)
        || page.get(19) != Some(&2)
        || page.get(21..24) != Some(&[64, 32, 32])
    {
        return Err(WorkLedgerError::Refused(
            "work ledger WAL page-one header is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn database_page_size(header: &[u8]) -> WorkLedgerResult<u64> {
    let encoded = u16::from_be_bytes(
        header
            .get(16..18)
            .ok_or_else(|| WorkLedgerError::Refused("database header is truncated".to_owned()))?
            .try_into()
            .map_err(|_| WorkLedgerError::Refused("database header is truncated".to_owned()))?,
    );
    match encoded {
        1 => Ok(65_536),
        value if value.is_power_of_two() && (512..=32_768).contains(&value) => Ok(u64::from(value)),
        _ => Err(WorkLedgerError::Refused(
            "work ledger database page size is invalid".to_owned(),
        )),
    }
}

fn wal_checksum(
    bytes: &[u8],
    big_endian: bool,
    mut checksum: [u32; 2],
) -> WorkLedgerResult<[u32; 2]> {
    if bytes.len() < 8 || !bytes.len().is_multiple_of(8) {
        return Err(WorkLedgerError::Refused(
            "work ledger WAL checksum input is invalid".to_owned(),
        ));
    }
    for words in bytes.as_chunks::<8>().0 {
        let first = wal_checksum_word(&words[..4], big_endian)?;
        let second = wal_checksum_word(&words[4..], big_endian)?;
        checksum[0] = checksum[0].wrapping_add(first).wrapping_add(checksum[1]);
        checksum[1] = checksum[1].wrapping_add(second).wrapping_add(checksum[0]);
    }
    Ok(checksum)
}

fn wal_checksum_word(bytes: &[u8], big_endian: bool) -> WorkLedgerResult<u32> {
    let word: [u8; 4] = bytes
        .try_into()
        .map_err(|_| WorkLedgerError::Refused("work ledger WAL word is truncated".to_owned()))?;
    Ok(if big_endian {
        u32::from_be_bytes(word)
    } else {
        u32::from_le_bytes(word)
    })
}

fn be_u32(bytes: &[u8], offset: usize) -> WorkLedgerResult<u32> {
    Ok(u32::from_be_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(|| WorkLedgerError::Refused("work ledger WAL is truncated".to_owned()))?
            .try_into()
            .map_err(|_| WorkLedgerError::Refused("work ledger WAL is truncated".to_owned()))?,
    ))
}

#[derive(Debug, Eq, PartialEq)]
struct SchemaProbeFileMetadata {
    len: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn open_regular_file_for_schema_probe(
    path: &Path,
) -> WorkLedgerResult<Option<(SchemaProbeFileMetadata, fs::File)>> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt as _;

    let Some(before) = regular_file_metadata(path)? else {
        return Ok(None);
    };
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(nix::libc::O_NOFOLLOW);
    let file = options.open(path)?;
    if metadata_identity(&file.metadata()?)? != before {
        return Err(WorkLedgerError::Refused(
            "work ledger sidecar changed while opening".to_owned(),
        ));
    }
    Ok(Some((before, file)))
}

fn regular_file_metadata(path: &Path) -> WorkLedgerResult<Option<SchemaProbeFileMetadata>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(WorkLedgerError::Refused(
            "work ledger sidecar is not a regular file".to_owned(),
        ));
    }
    Ok(Some(metadata_identity(&metadata)?))
}

fn metadata_identity(metadata: &fs::Metadata) -> WorkLedgerResult<SchemaProbeFileMetadata> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(SchemaProbeFileMetadata {
            len: metadata.len(),
            modified: metadata.modified()?,
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    Ok(SchemaProbeFileMetadata {
        len: metadata.len(),
        modified: metadata.modified()?,
    })
}

pub(super) fn database_schema_from_header(header: &[u8]) -> WorkLedgerResult<i64> {
    if header.len() < 64 || &header[..16] != b"SQLite format 3\0" {
        return Err(WorkLedgerError::Refused(
            "work ledger database header is invalid".to_owned(),
        ));
    }
    Ok(i64::from(u32::from_be_bytes(
        header[60..64]
            .try_into()
            .map_err(|_| WorkLedgerError::Refused("schema header is invalid".to_owned()))?,
    )))
}

pub(super) fn immutable_schema11_query<T>(
    state_dir: &Path,
    query: impl FnOnce(&Connection) -> WorkLedgerResult<T>,
) -> WorkLedgerResult<(String, T)> {
    let path = WorkLedger::path_at(state_dir);
    let directory = path
        .parent()
        .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
    let source_before = storage_snapshot(directory, &path)?.ok_or_else(|| {
        WorkLedgerError::Refused("schema-v11 query requires an existing ledger".to_owned())
    })?;
    validate_storage_snapshot(&source_before)?;
    let connection = connect_immutable(&path)?;
    validate_database_resources(&connection)?;
    if super::schema_version(&connection)? != LEGACY_INVENTORY_SCHEMA_VERSION {
        return Err(WorkLedgerError::Refused(
            "schema-v11 query observed a different schema".to_owned(),
        ));
    }
    verify_open_lineage(&connection, LEGACY_INVENTORY_SCHEMA_VERSION)?;
    verify_legacy_inventory_schema(&connection)?;
    validate_inventory_rows_sql_side(&connection, InventorySchema::LegacyV11)?;
    verify_integrity(&connection)?;
    let result = query(&connection)?;
    if storage_snapshot(directory, &path)?.as_ref() != Some(&source_before) {
        return Err(WorkLedgerError::Refused(
            "work ledger changed during zero-write schema-v11 query".to_owned(),
        ));
    }
    Ok((source_before.database.sha256, result))
}

#[cfg(all(test, unix))]
fn local_work_inventory_with_preopen_hook(
    state_dir: &Path,
    preopen_hook: impl FnOnce() -> WorkLedgerResult<()>,
) -> WorkLedgerResult<LocalWorkInventory> {
    Ok(local_work_inventory_inspection_with_preopen_hook(state_dir, preopen_hook)?.inventory)
}

fn local_work_inventory_inspection_with_preopen_hook(
    state_dir: &Path,
    preopen_hook: impl FnOnce() -> WorkLedgerResult<()>,
) -> WorkLedgerResult<ImmutableInventoryInspection> {
    let path = WorkLedger::path_at(state_dir);
    let directory = path
        .parent()
        .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
    let Some(source_before) = storage_snapshot(directory, &path)? else {
        if storage_snapshot(directory, &path)?.is_some() {
            return Err(WorkLedgerError::Refused(
                "work ledger appeared during zero-write inventory".to_owned(),
            ));
        }
        return Ok(ImmutableInventoryInspection {
            schema_version: 0,
            inventory: empty_inventory(),
        });
    };
    validate_storage_snapshot(&source_before)?;
    preopen_hook()?;

    let connection = connect_immutable(&path)?;
    validate_database_resources(&connection)?;
    let schema = verify_inventory_schema(&connection)?;
    validate_inventory_rows_sql_side(&connection, schema)?;
    verify_integrity(&connection)?;

    let inventory =
        materialize_inventory(&connection, source_before.database.sha256.clone(), schema)?;
    if storage_snapshot(directory, &path)?.as_ref() != Some(&source_before) {
        return Err(WorkLedgerError::Refused(
            "work ledger changed during zero-write inventory".to_owned(),
        ));
    }
    Ok(ImmutableInventoryInspection {
        schema_version: match schema {
            InventorySchema::Current => super::SCHEMA_VERSION,
            InventorySchema::LegacyV11 => LEGACY_INVENTORY_SCHEMA_VERSION,
        },
        inventory,
    })
}

/// Read schema-v11 status through the same immutable snapshot boundary used by
/// inventory. Current schemas deliberately continue through `WorkLedger` so
/// protected-object reconciliation retains its existing ownership.
pub(crate) fn immutable_legacy_status(state_dir: &Path) -> WorkLedgerResult<Option<LedgerStatus>> {
    if database_effective_schema_version(state_dir)? != Some(LEGACY_INVENTORY_SCHEMA_VERSION) {
        return Ok(None);
    }
    let path = WorkLedger::path_at(state_dir);
    let directory = path
        .parent()
        .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
    let Some(source_before) = storage_snapshot(directory, &path)? else {
        return Ok(None);
    };
    validate_storage_snapshot(&source_before)?;
    let connection = connect_immutable(&path)?;
    validate_database_resources(&connection)?;
    if super::schema_version(&connection)? != LEGACY_INVENTORY_SCHEMA_VERSION {
        return Ok(None);
    }
    verify_open_lineage(&connection, LEGACY_INVENTORY_SCHEMA_VERSION)?;
    verify_legacy_inventory_schema(&connection)?;
    validate_inventory_rows_sql_side(&connection, InventorySchema::LegacyV11)?;
    let integrity = verify_integrity(&connection)?;
    let snapshot_ledger = WorkLedger { path: path.clone() };
    snapshot_ledger.verify_protected_object_storage(&connection)?;
    let status = LedgerStatus {
        exists: true,
        schema_version: LEGACY_INVENTORY_SCHEMA_VERSION,
        journal_mode: connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?,
        synchronous: synchronous_name(&connection)?,
        foreign_keys: "enabled".to_owned(),
        integrity,
        work_items: count(&connection, "work_items")?,
        pending_wakes: count_where(&connection, "outbox", "state", "pending")?,
        uncertain_wakes: count_where(&connection, "outbox", "state", "uncertain")?,
        pending_projection_intents: count_where(
            &connection,
            "projection_intents",
            "state",
            "pending",
        )?,
        quarantined_projection_intents: count_where(
            &connection,
            "projection_intents",
            "state",
            "quarantined",
        )?,
        imports: count(&connection, "imports")?,
        protected_objects: count(&connection, "protected_objects")?,
        provider_deliveries: count(&connection, "provider_deliveries")?,
        agent_ownership: count(&connection, "agent_ownership")?,
        activation_epochs: count(&connection, "activation_epochs")?,
        activation_enabled: false,
        dispatch_enabled: false,
    };
    if storage_snapshot(directory, &path)?.as_ref() != Some(&source_before) {
        return Err(WorkLedgerError::Refused(
            "work ledger changed during zero-write status".to_owned(),
        ));
    }
    Ok(Some(status))
}

/// Run a bounded query against the same immutable, race-checked snapshot used
/// by local inventory. The closure cannot obtain a writable connection.
pub(super) fn immutable_ledger_query<T>(
    state_dir: &Path,
    query: impl FnOnce(&Connection, &str) -> WorkLedgerResult<T>,
) -> WorkLedgerResult<Option<T>> {
    let path = WorkLedger::path_at(state_dir);
    let directory = path
        .parent()
        .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
    let Some(source_before) = storage_snapshot(directory, &path)? else {
        if storage_snapshot(directory, &path)?.is_some() {
            return Err(WorkLedgerError::Refused(
                "work ledger appeared during zero-write query".to_owned(),
            ));
        }
        return Ok(None);
    };
    validate_storage_snapshot(&source_before)?;
    let connection = connect_immutable(&path)?;
    validate_database_resources(&connection)?;
    verify_supported_schema(&connection)?;
    verify_integrity(&connection)?;
    let result = query(&connection, &source_before.database.sha256)?;
    if storage_snapshot(directory, &path)?.as_ref() != Some(&source_before) {
        return Err(WorkLedgerError::Refused(
            "work ledger changed during zero-write query".to_owned(),
        ));
    }
    Ok(Some(result))
}

pub(super) fn inventory_from_connection(
    connection: &Connection,
    snapshot_sha256: String,
) -> WorkLedgerResult<LocalWorkInventory> {
    validate_inventory_rows_sql_side(connection, InventorySchema::Current)?;
    materialize_inventory(connection, snapshot_sha256, InventorySchema::Current)
}

fn materialize_inventory(
    connection: &Connection,
    snapshot_sha256: String,
    schema: InventorySchema,
) -> WorkLedgerResult<LocalWorkInventory> {
    let mut selected = load_inventory_rows(connection, schema)?;
    let truncated = selected.len() > MAX_LOCAL_WORK_INVENTORY_ITEMS;
    selected.truncate(MAX_LOCAL_WORK_INVENTORY_ITEMS);
    let mut items = Vec::with_capacity(selected.len());
    let mut has_legacy_repository_identity = false;
    for (item, work_repository, work_head) in selected {
        validate_item(&item, work_repository.as_deref(), work_head.as_deref())?;
        has_legacy_repository_identity |= item.repository_provider.is_none();
        items.push(item);
    }
    Ok(LocalWorkInventory {
        snapshot_sha256: Some(snapshot_sha256),
        complete: !truncated && !has_legacy_repository_identity,
        truncated,
        limit: MAX_LOCAL_WORK_INVENTORY_ITEMS,
        items,
    })
}

#[cfg(any(unix, test))]
pub(super) fn validate_remote_inventory(inventory: &LocalWorkInventory) -> WorkLedgerResult<()> {
    let has_legacy_repository_identity = inventory
        .items
        .iter()
        .any(|item| item.repository_provider.is_none() || item.repository_id.is_none());
    let expected_complete = !inventory.truncated && !has_legacy_repository_identity;
    if inventory.limit != MAX_LOCAL_WORK_INVENTORY_ITEMS
        || inventory.items.len() > inventory.limit
        || (inventory.truncated && inventory.items.len() != inventory.limit)
        || inventory.complete != expected_complete
        || (inventory.snapshot_sha256.is_none()
            && (!inventory.items.is_empty() || !inventory.complete || inventory.truncated))
    {
        return Err(WorkLedgerError::Refused(
            "remote inventory bounds or completeness are contradictory".to_owned(),
        ));
    }
    if let Some(snapshot) = &inventory.snapshot_sha256
        && (snapshot.len() != 64
            || !snapshot
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    {
        return Err(WorkLedgerError::Refused(
            "remote inventory snapshot identity is invalid".to_owned(),
        ));
    }
    let mut previous: Option<(&str, u64, &str, &str, &str)> = None;
    for item in &inventory.items {
        validate_item(item, Some(&item.repository), Some(&item.exact_head))?;
        let key = (
            item.repository.as_str(),
            item.pull_request,
            item.exact_head.as_str(),
            item.workstream_handle.as_str(),
            item.work_item_id.as_str(),
        );
        if previous.is_some_and(|prior| prior >= key) {
            return Err(WorkLedgerError::Refused(
                "remote inventory ordering or item identity is contradictory".to_owned(),
            ));
        }
        previous = Some(key);
    }
    Ok(())
}

type InventoryRow = (LocalWorkInventoryItem, Option<String>, Option<String>);

fn verify_inventory_schema(connection: &Connection) -> WorkLedgerResult<InventorySchema> {
    let version = super::schema_version(connection)?;
    match version {
        super::SCHEMA_VERSION => {
            verify_supported_schema(connection)?;
            Ok(InventorySchema::Current)
        }
        LEGACY_INVENTORY_SCHEMA_VERSION => {
            verify_open_lineage(connection, version)?;
            verify_legacy_inventory_schema(connection)?;
            Ok(InventorySchema::LegacyV11)
        }
        _ => Err(WorkLedgerError::UnsupportedSchema(version)),
    }
}

pub(super) fn verify_legacy_inventory_schema(connection: &Connection) -> WorkLedgerResult<()> {
    // Exact sqlite_schema object sets produced by Shipyard v0.139.1 schema v11.
    // This is intentionally the complete schema-v11 manifest, not only the
    // tables read directly by inventory. Publication migrates the whole ledger
    // and then trusts policy, route, protected-object, continuation, import,
    // adapter, delivery, and custody invariants. Every table, automatic/named
    // index, constraint, and trigger therefore has to be authenticated first.
    let actual_tables = connection
        .prepare("SELECT DISTINCT tbl_name FROM sqlite_schema ORDER BY tbl_name")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let expected_tables = LEGACY_V11_SCHEMA_MANIFEST
        .iter()
        .map(|(table, _)| (*table).to_owned())
        .collect::<Vec<_>>();
    if actual_tables != expected_tables {
        return Err(WorkLedgerError::Refused(
            "schema v11 object inventory is missing, altered, or contains foreign objects"
                .to_owned(),
        ));
    }
    for (table, expected) in LEGACY_V11_SCHEMA_MANIFEST {
        let actual = schema_objects_digest(connection, table)?;
        let allowed = if *table == "workstream_projection_bindings" {
            matches!(
                actual.as_str(),
                "93ad7a2dfb12e804a7589013803975e5f54a73eb413f5c999cfc24d34f512b52"
                    | "c2e1484a34333c08343243eadc317c20b93172872be09fbf7fcc5820413ef2c8"
            )
        } else {
            actual == *expected
        };
        if !allowed {
            return Err(WorkLedgerError::Refused(format!(
                "schema v11 object set for {table} is missing or altered"
            )));
        }
    }
    Ok(())
}

const LEGACY_V11_SCHEMA_MANIFEST: &[(&str, &str)] = &[
    (
        "activation_epochs",
        "5ed25cec7bdfd4913ce44eb39e9849978311d34f60e5f6d163552ec3b7a148fd",
    ),
    (
        "adapter_registry",
        "59764fdec7b777347b94e1125a793857905cc710c7ed78180d1dbd9a3a61aab5",
    ),
    (
        "agent_ownership",
        "0935492203f50b3bd0ab767c5e376f4dfe5b94a264195fa1506c63c34738e30d",
    ),
    (
        "continuation_contracts",
        "9f6976425cbb06a36ffdc52cd8788b7c3e4c6502d578c014e6bbc4b4d35333ca",
    ),
    (
        "custody_controls",
        "a75a41e35ce99ac0cffd26661ea222e0aab47cc0aef82c55f568c3ce2103e5b0",
    ),
    (
        "custody_effects",
        "2c5b10b4f0c30dad20d612a2ba7b51749192d5d119325c273e967a3f79b5294b",
    ),
    (
        "custody_events",
        "96ff74461dd2c2e12b75e0938906b67154adc4858e89e7ca26245bee91b7038b",
    ),
    (
        "custody_inbox",
        "3d27f6ee672f9be9a350f1ae3326aea24d7138ab8ba28657eb4b62ee361a3663",
    ),
    (
        "custody_inbox_claims",
        "2f2b5d0eb9e7b15410474aca234541682ed4ef361d2301b6444d9aad388786f1",
    ),
    (
        "custody_outbox",
        "58776108fc676eda7fd76284e322a6fdd06906c7b6d9d86967f7cac21a47f99b",
    ),
    (
        "custody_processed_acknowledgements",
        "044d3f50e64c18a49e5e86d47e4e598293d789f6c920b3e95797db573faa80b4",
    ),
    (
        "custody_rebinds",
        "be92bb09004ef840165d2de5e72dab6e525c8066521fba180757e2cd9ab3585b",
    ),
    (
        "custody_sender_claims",
        "61a0f8b2d3690a0de3281040e33d553fb8e87e06b05fa7b5d090944b752c49ac",
    ),
    (
        "custody_successor_rebinds",
        "dcaf9f13d20c9f0f0b9ce3ff283576a1a5d5841bc1f08871eb2ef3a10b1ce79c",
    ),
    (
        "events",
        "4ccc356df6153ad9bcf6cce804d8de5ebedd76d63226adc270fc092068481690",
    ),
    (
        "imports",
        "b104e8365a7d17bd0ce8257c510da9af43f8aec349af34f8945add2738025c3c",
    ),
    (
        "ledger_schema_identity",
        "72257bf40fb374dd91eadeac9ee60cb24bf847d49daa542bf5025b9f6694cda7",
    ),
    (
        "outbox",
        "d2d1d289a7c5a2b6433dcb7be9e78d866bd0c121287ec670a1103a5445b19565",
    ),
    (
        "projection_intents",
        "e053347b4d391f4d51271fc6b33a4ee78b3262b56ab4676902bc2d66a739aca7",
    ),
    (
        "protected_objects",
        "858ac7c24cf017c101f8f5318c4ecf354a839a4469ec6458cd0d02fbb80c272b",
    ),
    (
        "provider_deliveries",
        "9aa46de98feef623687cb2eef94c868e4cec11c3ce2ce9482b2d09071662e8a7",
    ),
    (
        "provider_delivery_observations",
        "4bc7270639d1ec95527c2caf53a4425a118f2177776122110f72a7afc9177924",
    ),
    (
        "repo_policies",
        "04b3cd4a480d6287b0231e048a89eb111af7cc787209503cafc57c60837a5696",
    ),
    (
        "route_records",
        "2c8951a5085ab7ae42e6a27786831d0e06d695ff6e5d2db1016843b70ad6b9e3",
    ),
    (
        "wake_attempts",
        "a553d438eb15c398bd01dd855343b6d29e309bd6ef0440cc3f97f240d1915177",
    ),
    (
        "wake_claim_epochs",
        "233ef3dbe2c386df3ea51f958e0df090c5b5bee0a08bea2cab6c99e914d1e78f",
    ),
    (
        "work_items",
        "834d9274d49924d2ffc48b3f52be876af508876ac36bdb44e5238651eb2c9ce9",
    ),
    // The exact production representation is separately allowlisted above.
    (
        "workstream_projection_bindings",
        "c2e1484a34333c08343243eadc317c20b93172872be09fbf7fcc5820413ef2c8",
    ),
];

pub(super) fn verify_legacy_custody_migration_schema(
    connection: &Connection,
) -> WorkLedgerResult<()> {
    for (table, expected) in [
        (
            "custody_successor_rebinds",
            "dcaf9f13d20c9f0f0b9ce3ff283576a1a5d5841bc1f08871eb2ef3a10b1ce79c",
        ),
        (
            "custody_processed_acknowledgements",
            "044d3f50e64c18a49e5e86d47e4e598293d789f6c920b3e95797db573faa80b4",
        ),
    ] {
        if schema_objects_digest(connection, table)? != expected {
            return Err(WorkLedgerError::Refused(format!(
                "legacy custody table {table} is missing or altered"
            )));
        }
    }
    Ok(())
}

fn schema_objects_digest(connection: &Connection, table: &str) -> WorkLedgerResult<String> {
    let mut statement = connection.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_schema
          WHERE tbl_name = ?1 ORDER BY type, name",
    )?;
    let objects = statement
        .query_map([table], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut hasher = Sha256::new();
    hasher.update(b"shipyard.work-ledger.inventory-schema.v11\0");
    for (kind, name, owner, sql) in objects {
        for value in [kind, name, owner] {
            let length = u64::try_from(value.len()).map_err(|_| {
                WorkLedgerError::Refused("schema object identity is too large".to_owned())
            })?;
            hasher.update(length.to_le_bytes());
            hasher.update(value.as_bytes());
        }
        match sql {
            Some(sql) => {
                hasher.update([1]);
                let length = u64::try_from(sql.len()).map_err(|_| {
                    WorkLedgerError::Refused("schema object SQL is too large".to_owned())
                })?;
                hasher.update(length.to_le_bytes());
                hasher.update(sql.as_bytes());
            }
            None => hasher.update([0]),
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

fn load_inventory_rows(
    connection: &Connection,
    schema: InventorySchema,
) -> WorkLedgerResult<Vec<InventoryRow>> {
    let repository_identity = match schema {
        InventorySchema::Current => {
            "binding.repository_provider, binding.repository_id, root.root_uuid"
        }
        InventorySchema::LegacyV11 => "NULL, NULL, NULL",
    };
    let root_join = match schema {
        InventorySchema::Current => "LEFT JOIN ownership_roots root ON root.work_item_id = work.id",
        InventorySchema::LegacyV11 => "",
    };
    let mut statement = connection.prepare(&format!(
        "SELECT {repository_identity},
                binding.repository, work.pr, binding.exact_head, work.phase,
                binding.workstream_handle, work.id, work.work_generation,
                work.owner_id, work.owner_generation, ownership.ownership_id,
                ownership.state, ownership.work_generation,
                ownership.owner_generation, work.repo, work.head_sha
           FROM workstream_projection_bindings binding
           JOIN work_items work ON work.id = binding.work_item_id
           {root_join}
           LEFT JOIN agent_ownership ownership
             ON ownership.work_item_id = work.id
            AND ownership.work_generation = work.work_generation
            AND ownership.owner_generation = work.owner_generation
          ORDER BY binding.repository, work.pr, binding.exact_head,
                   binding.workstream_handle, work.id
          LIMIT ?1"
    ))?;
    let limit = i64::try_from(MAX_LOCAL_WORK_INVENTORY_ITEMS + 1)
        .map_err(|_| WorkLedgerError::Refused("inventory limit is invalid".to_owned()))?;
    let rows = statement.query_map([limit], |row| {
        Ok((
            LocalWorkInventoryItem {
                repository_provider: row.get(0)?,
                repository_id: row.get(1)?,
                root_uuid: row.get(2)?,
                repository: row.get(3)?,
                pull_request: row.get(4)?,
                exact_head: row.get(5)?,
                state: row.get(6)?,
                workstream_handle: row.get(7)?,
                work_item_id: row.get(8)?,
                work_generation: row.get(9)?,
                owner_id: row.get(10)?,
                owner_generation: row.get(11)?,
                ownership_id: row.get(12)?,
                ownership_state: row.get(13)?,
                ownership_work_generation: row.get(14)?,
                ownership_owner_generation: row.get(15)?,
            },
            row.get::<_, Option<String>>(16)?,
            row.get::<_, Option<String>>(17)?,
        ))
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

#[derive(Debug, Eq, PartialEq)]
struct StorageSnapshot {
    directory: DirectoryIdentity,
    database: FileIdentity,
    wal: Option<FileIdentity>,
    shared_memory: Option<FileIdentity>,
    rollback_journal: Option<FileIdentity>,
}

#[derive(Debug, Eq, PartialEq)]
struct DirectoryIdentity {
    modified: std::time::SystemTime,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct FileIdentity {
    len: u64,
    modified: std::time::SystemTime,
    sha256: String,
    header: Vec<u8>,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn storage_snapshot(
    directory: &Path,
    database: &Path,
) -> WorkLedgerResult<Option<StorageSnapshot>> {
    let Some(directory_identity) = directory_identity(directory)? else {
        return Ok(None);
    };
    let database_identity = file_identity(database, MAX_LEDGER_DATABASE_BYTES)?;
    let wal = file_identity(&sqlite_sidecar(database, "-wal"), MAX_SQLITE_SIDECAR_BYTES)?;
    let shared_memory = file_identity(&sqlite_sidecar(database, "-shm"), MAX_SQLITE_SIDECAR_BYTES)?;
    let rollback_journal = file_identity(
        &sqlite_sidecar(database, "-journal"),
        MAX_SQLITE_SIDECAR_BYTES,
    )?;
    let Some(database_identity) = database_identity else {
        if wal.is_some() || shared_memory.is_some() || rollback_journal.is_some() {
            return Err(WorkLedgerError::Refused(
                "zero-write inventory found orphan SQLite sidecars without a database".to_owned(),
            ));
        }
        return Ok(None);
    };
    Ok(Some(StorageSnapshot {
        directory: directory_identity,
        database: database_identity,
        wal,
        shared_memory,
        rollback_journal,
    }))
}

fn directory_identity(path: &Path) -> WorkLedgerResult<Option<DirectoryIdentity>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkLedgerError::Refused(
            "ledger directory is not a regular directory".to_owned(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let mode = metadata.mode() & 0o777;
        if mode != 0o700 {
            return Err(WorkLedgerError::Refused(
                "ledger directory permissions are not 0700".to_owned(),
            ));
        }
        Ok(Some(DirectoryIdentity {
            modified: metadata.modified()?,
            mode,
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            device: metadata.dev(),
            inode: metadata.ino(),
        }))
    }
    #[cfg(not(unix))]
    {
        Ok(Some(DirectoryIdentity {
            modified: metadata.modified()?,
        }))
    }
}

fn file_identity(path: &Path, max_bytes: u64) -> WorkLedgerResult<Option<FileIdentity>> {
    let path_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(WorkLedgerError::Refused(
            "ledger database or sidecar is not a regular file".to_owned(),
        ));
    }
    if path_metadata.len() > max_bytes {
        return Err(WorkLedgerError::Refused(
            "zero-write inventory storage exceeds its resource bound".to_owned(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
        if path_metadata.mode() & 0o777 != 0o600 {
            return Err(WorkLedgerError::Refused(
                "ledger database or sidecar permissions are not protected".to_owned(),
            ));
        }
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(nix::libc::O_NOFOLLOW);
        let mut file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.dev() != path_metadata.dev()
            || metadata.ino() != path_metadata.ino()
            || metadata.len() != path_metadata.len()
            || metadata.mtime() != path_metadata.mtime()
            || metadata.mtime_nsec() != path_metadata.mtime_nsec()
            || metadata.ctime() != path_metadata.ctime()
            || metadata.ctime_nsec() != path_metadata.ctime_nsec()
        {
            return Err(WorkLedgerError::Refused(
                "work ledger changed while taking the pre-open snapshot".to_owned(),
            ));
        }
        let (sha256, header) = digest_bounded_file(&mut file, max_bytes)?;
        Ok(Some(FileIdentity {
            len: metadata.len(),
            modified: metadata.modified()?,
            sha256,
            header,
            mode: metadata.mode() & 0o777,
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            device: metadata.dev(),
            inode: metadata.ino(),
        }))
    }
    #[cfg(not(unix))]
    {
        let mut file = OpenOptions::new().read(true).open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.len() != path_metadata.len()
            || metadata.modified()? != path_metadata.modified()?
        {
            return Err(WorkLedgerError::Refused(
                "work ledger changed while taking the pre-open snapshot".to_owned(),
            ));
        }
        let (sha256, header) = digest_bounded_file(&mut file, max_bytes)?;
        Ok(Some(FileIdentity {
            len: metadata.len(),
            modified: metadata.modified()?,
            sha256,
            header,
        }))
    }
}

fn digest_bounded_file(file: &mut fs::File, max_bytes: u64) -> WorkLedgerResult<(String, Vec<u8>)> {
    let mut digest = Sha256::new();
    let mut header = Vec::with_capacity(100);
    let mut total = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.checked_add(read as u64).ok_or_else(|| {
            WorkLedgerError::Refused("zero-write inventory storage size overflowed".to_owned())
        })?;
        if total > max_bytes {
            return Err(WorkLedgerError::Refused(
                "zero-write inventory storage exceeds its resource bound".to_owned(),
            ));
        }
        if header.len() < 100 {
            let take = (100 - header.len()).min(read);
            header.extend_from_slice(&buffer[..take]);
        }
        digest.update(&buffer[..read]);
    }
    Ok((hex::encode(digest.finalize()), header))
}

fn empty_inventory() -> LocalWorkInventory {
    LocalWorkInventory {
        snapshot_sha256: None,
        complete: true,
        truncated: false,
        limit: MAX_LOCAL_WORK_INVENTORY_ITEMS,
        items: Vec::new(),
    }
}

fn validate_storage_snapshot(snapshot: &StorageSnapshot) -> WorkLedgerResult<()> {
    if snapshot.wal.is_some() != snapshot.shared_memory.is_some() {
        return Err(WorkLedgerError::Refused(
            "zero-write inventory found an orphan WAL/shared-memory sidecar".to_owned(),
        ));
    }
    if snapshot.wal.as_ref().is_some_and(|wal| wal.len > 0) {
        return Err(WorkLedgerError::Refused(
            "zero-write inventory cannot inspect an uncheckpointed WAL".to_owned(),
        ));
    }
    if snapshot.rollback_journal.is_some() {
        return Err(WorkLedgerError::Refused(
            "zero-write inventory cannot inspect rollback-journal storage".to_owned(),
        ));
    }
    let header = &snapshot.database.header;
    if header.len() < 20
        || &header[..16] != b"SQLite format 3\0"
        || header[18] != 2
        || header[19] != 2
    {
        return Err(WorkLedgerError::Refused(
            "zero-write inventory requires a WAL-format work ledger".to_owned(),
        ));
    }
    Ok(())
}

fn connect_immutable(database: &Path) -> WorkLedgerResult<Connection> {
    let parent = database
        .parent()
        .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
    let name = database
        .file_name()
        .ok_or_else(|| WorkLedgerError::Refused("database has no file name".to_owned()))?;
    let pinned = fs::canonicalize(parent)?.join(name);
    let path = pinned.to_str().ok_or_else(|| {
        WorkLedgerError::Refused("ledger database path is not valid UTF-8".to_owned())
    })?;
    let normalized = normalize_sqlite_uri_path(path)?;
    let mut uri = String::from("file:");
    for byte in normalized.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'.' | b'_' | b'-') {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut uri, "%{byte:02X}")
                .map_err(|_| WorkLedgerError::Refused("ledger URI is invalid".to_owned()))?;
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    let connection = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA query_only = ON;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(connection)
}

#[cfg_attr(not(windows), allow(clippy::unnecessary_wraps))]
fn normalize_sqlite_uri_path(path: &str) -> WorkLedgerResult<String> {
    let normalized = path.replace('\\', "/");
    #[cfg(windows)]
    {
        normalize_windows_sqlite_uri_path(&normalized)
    }
    #[cfg(not(windows))]
    Ok(normalized)
}

#[cfg(any(windows, test))]
fn normalize_windows_sqlite_uri_path(path: &str) -> WorkLedgerResult<String> {
    if path.starts_with("//?/UNC/") {
        return Err(WorkLedgerError::Refused(
            "zero-write inventory does not support a UNC ledger path".to_owned(),
        ));
    }
    let path = path.strip_prefix("//?/").unwrap_or(path);
    if path.starts_with("//") {
        return Err(WorkLedgerError::Refused(
            "zero-write inventory does not support a UNC ledger path".to_owned(),
        ));
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        Ok(format!("/{path}"))
    } else {
        Err(WorkLedgerError::Refused(
            "zero-write inventory Windows ledger path is not absolute".to_owned(),
        ))
    }
}

fn validate_database_resources(connection: &Connection) -> WorkLedgerResult<()> {
    let page_count: u64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let page_size: u64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let database_bytes = page_count.checked_mul(page_size).ok_or_else(|| {
        WorkLedgerError::Refused("zero-write inventory database size overflowed".to_owned())
    })?;
    if page_size == 0 || database_bytes > MAX_LEDGER_DATABASE_BYTES {
        return Err(WorkLedgerError::Refused(
            "zero-write inventory database exceeds its resource bound".to_owned(),
        ));
    }
    Ok(())
}

fn validate_inventory_rows_sql_side(
    connection: &Connection,
    schema: InventorySchema,
) -> WorkLedgerResult<()> {
    let unbound: bool = connection.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM work_items work
           LEFT JOIN workstream_projection_bindings binding
             ON binding.work_item_id = work.id
          WHERE binding.work_item_id IS NULL
         )",
        [],
        |row| row.get(0),
    )?;
    if unbound {
        return Err(WorkLedgerError::Refused(
            "zero-write inventory found an unbound local work row".to_owned(),
        ));
    }
    let repository_identity_validation = match schema {
        InventorySchema::Current => {
            "OR ((binding.repository_provider IS NULL)
                   != (binding.repository_id IS NULL))
               OR (binding.repository_provider IS NOT NULL AND
                   (typeof(binding.repository_provider) != 'text'
                    OR length(CAST(binding.repository_provider AS BLOB)) NOT BETWEEN 3 AND 64
                    OR typeof(binding.repository_id) != 'text'
                    OR length(CAST(binding.repository_id AS BLOB)) NOT BETWEEN 1 AND 512))"
        }
        InventorySchema::LegacyV11 => "",
    };
    let invalid: bool = connection.query_row(
        &format!(
            "SELECT EXISTS(
           SELECT 1
             FROM workstream_projection_bindings binding
             JOIN work_items work ON work.id = binding.work_item_id
             LEFT JOIN agent_ownership ownership
               ON ownership.work_item_id = work.id
              AND ownership.work_generation = work.work_generation
              AND ownership.owner_generation = work.owner_generation
            WHERE typeof(binding.repository) != 'text'
               OR length(CAST(binding.repository AS BLOB)) NOT BETWEEN 3 AND 255
               {repository_identity_validation}
               OR typeof(binding.exact_head) != 'text'
               OR length(CAST(binding.exact_head AS BLOB)) != 40
               OR typeof(binding.workstream_handle) != 'text'
               OR length(CAST(binding.workstream_handle AS BLOB)) NOT BETWEEN 3 AND 128
               OR typeof(work.id) != 'text'
               OR length(CAST(work.id AS BLOB)) NOT BETWEEN 4 AND 128
               OR typeof(work.phase) != 'text'
               OR length(CAST(work.phase AS BLOB)) NOT BETWEEN 1 AND 32
               OR typeof(work.pr) != 'integer' OR work.pr <= 0
               OR typeof(work.work_generation) != 'integer' OR work.work_generation <= 0
               OR typeof(work.owner_generation) != 'integer' OR work.owner_generation <= 0
               OR (work.owner_id IS NOT NULL AND
                   (typeof(work.owner_id) != 'text'
                    OR length(CAST(work.owner_id AS BLOB)) NOT BETWEEN 7 AND 128))
               OR typeof(work.repo) != 'text'
               OR length(CAST(work.repo AS BLOB)) NOT BETWEEN 3 AND 255
               OR typeof(work.head_sha) != 'text'
               OR length(CAST(work.head_sha AS BLOB)) != 40
               OR (ownership.ownership_id IS NOT NULL AND
                   (typeof(ownership.ownership_id) != 'text'
                    OR length(CAST(ownership.ownership_id AS BLOB)) NOT BETWEEN 4 AND 128
                    OR typeof(ownership.state) != 'text'
                    OR length(CAST(ownership.state AS BLOB)) NOT BETWEEN 1 AND 16
                    OR typeof(ownership.work_generation) != 'integer'
                    OR ownership.work_generation <= 0
                    OR typeof(ownership.owner_generation) != 'integer'
                    OR ownership.owner_generation <= 0))
         )"
        ),
        [],
        |row| row.get(0),
    )?;
    if invalid {
        return Err(WorkLedgerError::Refused(
            "zero-write inventory row exceeds its SQL-side identity bound".to_owned(),
        ));
    }
    Ok(())
}

fn sqlite_sidecar(database: &Path, suffix: &str) -> std::path::PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push(suffix);
    path.into()
}

fn validate_item(
    item: &LocalWorkInventoryItem,
    work_repository: Option<&str>,
    work_head: Option<&str>,
) -> WorkLedgerResult<()> {
    let exact_head = item.exact_head.len() == 40
        && item
            .exact_head
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    let repository_identity_valid = match (&item.repository_provider, &item.repository_id) {
        (None, None) => true,
        (Some(provider), Some(repository_id)) => {
            is_canonical_provider(provider) && is_canonical_identity_token(repository_id, 512)
        }
        _ => false,
    };
    if !repository_identity_valid
        || !is_canonical_repo_slug(&item.repository)
        || item.pull_request == 0
        || !exact_head
        || work_repository != Some(item.repository.as_str())
        || work_head != Some(item.exact_head.as_str())
    {
        return Err(WorkLedgerError::Refused(
            "inventory item has contradictory repository, PR, or exact-head identity".to_owned(),
        ));
    }
    if !matches!(
        item.state.as_str(),
        "shadow_imported"
            | "published"
            | "ready"
            | "managed"
            | "waiting"
            | "actionable"
            | "dispatching"
            | "agent_owned_repair"
            | "returned"
            | "terminal"
    ) {
        return Err(WorkLedgerError::Refused(
            "inventory item has an unsupported lifecycle state".to_owned(),
        ));
    }
    if validate_workstream_handle(&item.workstream_handle).is_err()
        || item.work_generation == 0
        || item.owner_generation == 0
    {
        return Err(WorkLedgerError::Refused(
            "inventory item has incomplete workstream custody".to_owned(),
        ));
    }
    validate_opaque_ref("inventory work item", &item.work_item_id, "wi")?;
    if let Some(owner_id) = &item.owner_id {
        validate_opaque_ref("inventory owner", owner_id, "owner")?;
    }
    match (
        &item.ownership_id,
        &item.ownership_state,
        item.ownership_work_generation,
        item.ownership_owner_generation,
    ) {
        (None, None, None, None) => {}
        (Some(ownership_id), Some(state), Some(work_generation), Some(owner_generation)) => {
            validate_opaque_ref("inventory agent ownership", ownership_id, "ao")?;
            if work_generation == 0
                || owner_generation == 0
                || work_generation != item.work_generation
                || owner_generation != item.owner_generation
                || item.owner_id.is_none()
            {
                return Err(WorkLedgerError::Refused(
                    "inventory agent ownership generation is invalid".to_owned(),
                ));
            }
            if !matches!(
                state.as_str(),
                "pending" | "acknowledged" | "returned" | "uncertain" | "failed"
            ) {
                return Err(WorkLedgerError::Refused(
                    "inventory agent ownership state is invalid".to_owned(),
                ));
            }
        }
        _ => {
            return Err(WorkLedgerError::Refused(
                "inventory agent ownership identity is incomplete".to_owned(),
            ));
        }
    }
    Ok(())
}

fn is_canonical_provider(value: &str) -> bool {
    (3..=64).contains(&value.len())
        && value == value.to_ascii_lowercase()
        && value.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn is_canonical_identity_token(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'/' | b'\\'))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::time::SystemTime;

    use super::*;
    use crate::work_ledger::{ImportCandidate, LifecycleState, digest, opaque_ref};

    #[test]
    fn sqlite_uri_path_removes_windows_verbatim_authority_prefix() {
        assert_eq!(
            normalize_windows_sqlite_uri_path("//?/D:/state/work-items.sqlite3")
                .expect("verbatim drive path"),
            "/D:/state/work-items.sqlite3"
        );
        assert!(
            normalize_windows_sqlite_uri_path("//?/UNC/server/share/work-items.sqlite3")
                .unwrap_err()
                .to_string()
                .contains("UNC ledger path")
        );
    }

    fn candidate(repository: &str, pull_request: u64, head: &str, label: &str) -> ImportCandidate {
        ImportCandidate {
            work_id: opaque_ref("wi", label),
            kind: "terminal_handoff".to_owned(),
            repo: Some(repository.to_owned()),
            pr: Some(pull_request),
            head_sha: Some(head.to_owned()),
            base_ref: Some("main".to_owned()),
            goal_id: Some(opaque_ref("goal", label)),
            goal_generation: 1,
            lane: Some("fresh_agent_continuation".to_owned()),
            role: "root".to_owned(),
            owner_id: Some(opaque_ref("owner", label)),
            owner_generation: 1,
            terminal_adapter: Some("session_host".to_owned()),
            agent_adapter: Some("codex".to_owned()),
            provider_adapter: Some("cmux".to_owned()),
            coordinator_route_ref: None,
            repair_route_ref: Some(opaque_ref("route", label)),
            pr_truth: "unknown".to_owned(),
            acceptance_truth: "unknown".to_owned(),
            continuation_truth: "pending".to_owned(),
            phase: LifecycleState::ShadowImported.as_str().to_owned(),
            source_ref: opaque_ref("src", label),
            content_digest: digest(label.as_bytes()),
            source_updated_at: None,
        }
    }

    fn seed(ledger: &WorkLedger, repository: &str, pull_request: u64, label: &str) {
        let head = digest(label.as_bytes())[..40].to_owned();
        let candidate = candidate(repository, pull_request, &head, label);
        let work_id = candidate.work_id.clone();
        let workstream_handle = format!("GEN-{pull_request}");
        ledger.import(&[candidate]).expect("import work item");
        ledger
            .bind_workstream_projection(
                &work_id,
                &workstream_handle,
                &digest(format!("plan:{label}").as_bytes()),
                1,
                1,
                1,
                1,
                "github.com",
                &format!("R_{label}"),
                repository,
                &head,
            )
            .expect("bind workstream");
    }

    fn seed_production_v11(
        ledger: &WorkLedger,
        repository: &str,
        pull_request: u64,
        handle: &str,
        label: &str,
    ) {
        let head = digest(label.as_bytes())[..40].to_owned();
        let imported = candidate(repository, pull_request, &head, label);
        let work_id = imported.work_id.clone();
        ledger.import(&[imported]).expect("v11 work item");
        let connection = ledger.connect_read_write().expect("v11 fixture connection");
        super::super::storage::reconstruct_authentic_v11_schema_for_test(&connection)
            .expect("authentic production v11 schema");
        connection
            .execute(
                "INSERT INTO workstream_projection_bindings
                 (work_item_id, workstream_handle, plan_sha256, root_revision, issue_revision,
                  projection_revision, material_event_revision, repository, exact_head, created_at)
                 VALUES (?1, ?2, ?3, 1, 1, 1, 1, ?4, ?5, '2026-08-31T00:00:00Z')",
                rusqlite::params![work_id, handle, digest(b"v11 plan"), repository, head],
            )
            .expect("v11 binding");
    }

    fn replace_v11_binding_schema(
        connection: &Connection,
        work_item_column: &str,
        handle_column: &str,
        unique_constraint: &str,
    ) {
        connection
            .execute_batch(&format!(
                "DROP TABLE workstream_projection_bindings;
                 CREATE TABLE workstream_projection_bindings (
                   {work_item_column},
                   {handle_column},
                   plan_sha256 TEXT NOT NULL
                     CHECK(length(plan_sha256) = 64 AND plan_sha256 NOT GLOB '*[^0-9a-f]*'),
                   root_revision INTEGER NOT NULL CHECK(root_revision >= 0),
                   issue_revision INTEGER NOT NULL CHECK(issue_revision >= 0),
                   projection_revision INTEGER NOT NULL CHECK(projection_revision > 0),
                   material_event_revision INTEGER NOT NULL CHECK(material_event_revision >= 0),
                   repository TEXT NOT NULL CHECK(length(repository) BETWEEN 3 AND 255),
                   exact_head TEXT NOT NULL
                     CHECK(length(exact_head) = 40 AND exact_head NOT GLOB '*[^0-9a-f]*'),
                   created_at TEXT NOT NULL CHECK(length(created_at) >= 20)
                   {unique_constraint}
                 );
                 CREATE TRIGGER workstream_projection_binding_identity_immutable
                 BEFORE UPDATE OF work_item_id, workstream_handle, plan_sha256, root_revision,
                                  issue_revision, projection_revision, material_event_revision,
                                  repository, created_at
                 ON workstream_projection_bindings
                 BEGIN SELECT RAISE(ABORT, 'workstream projection binding identity is immutable'); END;
                 CREATE TRIGGER workstream_projection_binding_no_delete
                 BEFORE DELETE ON workstream_projection_bindings
                 BEGIN SELECT RAISE(ABORT, 'workstream projection binding cannot be deleted'); END;"
            ))
            .expect("replace v11 binding schema");
    }

    fn assert_v11_schema_mutation_refuses(label: &str, mutation: impl FnOnce(&Connection)) {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        seed_production_v11(&ledger, "alpha/repo", 7, "GEN-7", label);
        let connection = ledger.connect_read_write().expect("fixture connection");
        mutation(&connection);
        drop(connection);
        drop(ledger);
        #[cfg(unix)]
        let before = snapshot(state.path());

        let error = local_work_inventory(state.path()).expect_err("schema drift must refuse");

        let message = error.to_string();
        assert!(
            message.contains("missing or altered")
                || message.contains("object inventory is missing, altered, or contains foreign")
        );
        #[cfg(unix)]
        assert_eq!(snapshot(state.path()), before);
    }

    #[test]
    fn gate_0b_1_live_v11_projection_representation_is_exactly_allowlisted() {
        assert_eq!(LEGACY_V11_SCHEMA_MANIFEST.len(), 28);
        assert_eq!(
            LEGACY_V11_SCHEMA_MANIFEST
                .iter()
                .find(|(table, _)| *table == "projection_intents"),
            Some(&(
                "projection_intents",
                "e053347b4d391f4d51271fc6b33a4ee78b3262b56ab4676902bc2d66a739aca7"
            ))
        );
        assert_eq!(
            LEGACY_V11_SCHEMA_MANIFEST.last(),
            Some(&(
                "workstream_projection_bindings",
                "c2e1484a34333c08343243eadc317c20b93172872be09fbf7fcc5820413ef2c8"
            ))
        );
    }

    #[test]
    fn production_v11_inventory_is_truthful_and_zero_write() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        seed_production_v11(&ledger, "alpha/repo", 7, "GEN-7", "production-v11");
        drop(ledger);
        #[cfg(unix)]
        let before = snapshot(state.path());

        let inventory = local_work_inventory(state.path()).expect("v11 inventory");

        #[cfg(unix)]
        assert_eq!(snapshot(state.path()), before);
        assert!(!inventory.complete);
        assert!(!inventory.truncated);
        assert_eq!(inventory.items.len(), 1);
        assert_eq!(inventory.items[0].repository_provider, None);
        assert_eq!(inventory.items[0].repository_id, None);
        assert_eq!(inventory.items[0].repository, "alpha/repo");
        assert_eq!(inventory.items[0].pull_request, 7);
        assert_eq!(inventory.items[0].workstream_handle, "GEN-7");
    }

    #[cfg(unix)]
    #[test]
    fn production_v11_status_is_immutable_and_supported() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        seed_production_v11(&ledger, "alpha/repo", 7, "GEN-7", "status-v11");
        drop(ledger);
        let before = snapshot(state.path());

        let status = immutable_legacy_status(state.path())
            .expect("v11 status")
            .expect("present v11 status");

        assert_eq!(status.schema_version, 11);
        assert_eq!(status.work_items, 1);
        assert_eq!(status.integrity, "ok");
        assert_eq!(snapshot(state.path()), before);
    }

    #[test]
    fn v11_inventory_refuses_lineage_and_column_drift_zero_write() {
        let bad_lineage = tempfile::tempdir().expect("bad lineage state");
        let ledger = WorkLedger::open(bad_lineage.path()).expect("ledger");
        seed_production_v11(&ledger, "alpha/repo", 7, "GEN-7", "bad-lineage");
        let connection = ledger.connect_read_write().expect("fixture connection");
        connection
            .execute_batch("DROP TRIGGER ledger_schema_identity_immutable;")
            .expect("alter lineage");
        drop(connection);
        drop(ledger);
        #[cfg(unix)]
        let before = snapshot(bad_lineage.path());
        let error = local_work_inventory(bad_lineage.path()).expect_err("lineage must refuse");
        assert!(error.to_string().contains("identity"));
        #[cfg(unix)]
        assert_eq!(snapshot(bad_lineage.path()), before);

        let column_drift = tempfile::tempdir().expect("column drift state");
        let ledger = WorkLedger::open(column_drift.path()).expect("ledger");
        seed_production_v11(&ledger, "alpha/repo", 7, "GEN-7", "column-drift");
        let connection = ledger.connect_read_write().expect("fixture connection");
        connection
            .execute_batch("ALTER TABLE work_items ADD COLUMN surprise TEXT;")
            .expect("alter columns");
        drop(connection);
        drop(ledger);
        #[cfg(unix)]
        let before = snapshot(column_drift.path());
        let error = local_work_inventory(column_drift.path()).expect_err("drift must refuse");
        assert!(
            error
                .to_string()
                .contains("work_items is missing or altered")
        );
        #[cfg(unix)]
        assert_eq!(snapshot(column_drift.path()), before);
    }

    #[test]
    fn v11_inventory_refuses_same_column_ddl_and_object_drift_zero_write() {
        const WORK_ITEM_WITH_FK: &str =
            "work_item_id TEXT PRIMARY KEY REFERENCES work_items(id) ON DELETE RESTRICT";
        const WORK_ITEM_WITHOUT_FK: &str = "work_item_id TEXT PRIMARY KEY";
        const HANDLE_WITH_CHECK: &str = "workstream_handle TEXT NOT NULL \
CHECK(length(workstream_handle) BETWEEN 1 AND 128)";
        const HANDLE_WITHOUT_CHECK: &str = "workstream_handle TEXT NOT NULL";
        const UNIQUE: &str = ", UNIQUE(workstream_handle, repository, exact_head)";

        assert_v11_schema_mutation_refuses("removed-fk", |connection| {
            replace_v11_binding_schema(connection, WORK_ITEM_WITHOUT_FK, HANDLE_WITH_CHECK, UNIQUE);
        });
        assert_v11_schema_mutation_refuses("removed-check", |connection| {
            replace_v11_binding_schema(connection, WORK_ITEM_WITH_FK, HANDLE_WITHOUT_CHECK, UNIQUE);
        });
        assert_v11_schema_mutation_refuses("removed-unique", |connection| {
            replace_v11_binding_schema(connection, WORK_ITEM_WITH_FK, HANDLE_WITH_CHECK, "");
        });
        assert_v11_schema_mutation_refuses("removed-trigger", |connection| {
            connection
                .execute_batch("DROP TRIGGER workstream_projection_binding_no_delete;")
                .expect("remove trigger");
        });
        assert_v11_schema_mutation_refuses("altered-trigger", |connection| {
            connection
                .execute_batch(
                    "DROP TRIGGER workstream_projection_binding_identity_immutable;
                     CREATE TRIGGER workstream_projection_binding_identity_immutable
                     BEFORE UPDATE ON workstream_projection_bindings
                     BEGIN SELECT RAISE(ABORT, 'altered'); END;",
                )
                .expect("alter trigger");
        });
        assert_v11_schema_mutation_refuses("altered-index", |connection| {
            connection
                .execute_batch(
                    "DROP INDEX work_items_nonterminal;
                     CREATE INDEX work_items_nonterminal ON work_items(phase, id);",
                )
                .expect("alter index");
        });
        assert_v11_schema_mutation_refuses("projection-constraint-drift", |connection| {
            connection
                .execute_batch(
                    "PRAGMA writable_schema = ON;
                     UPDATE sqlite_schema
                        SET sql = replace(
                          sql,
                          'CHECK(length(workstream_handle) BETWEEN 1 AND 128)',
                          'CHECK(length(workstream_handle) BETWEEN 5 AND 128)'
                        )
                      WHERE type = 'table' AND name = 'projection_intents';
                     PRAGMA writable_schema = OFF;",
                )
                .expect("alter projection constraint");
        });
        assert_v11_schema_mutation_refuses("projection-index-drift", |connection| {
            connection
                .execute_batch(
                    "DROP INDEX projection_intents_drain;
                     CREATE INDEX projection_intents_drain
                       ON projection_intents(state, workstream_handle, sequence);",
                )
                .expect("alter projection index");
        });
        assert_v11_schema_mutation_refuses("projection-identity-trigger-drift", |connection| {
            connection
                .execute_batch("DROP TRIGGER projection_intent_identity_immutable;")
                .expect("remove projection identity trigger");
        });
        assert_v11_schema_mutation_refuses("projection-delete-trigger-drift", |connection| {
            connection
                .execute_batch("DROP TRIGGER projection_intent_no_delete;")
                .expect("remove projection delete trigger");
        });
        assert_v11_schema_mutation_refuses("policy-constraint-drift", |connection| {
            connection
                .execute_batch("ALTER TABLE repo_policies ADD COLUMN unauthenticated_policy TEXT;")
                .expect("alter policy schema");
        });
        assert_v11_schema_mutation_refuses("route-index-drift", |connection| {
            connection
                .execute_batch("CREATE INDEX unauthenticated_route ON route_records(work_item_id);")
                .expect("alter route schema");
        });
        assert_v11_schema_mutation_refuses("foreign-schema-object", |connection| {
            connection
                .execute_batch("CREATE TABLE unauthenticated_table(value TEXT);")
                .expect("add foreign table");
        });
    }

    #[test]
    fn inventory_is_repository_scoped_and_deterministically_sorted() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        seed(&ledger, "zeta/repo", 7, "GEN-7-Z");
        seed(&ledger, "alpha/repo", 7, "GEN-7-A");

        let inventory = local_work_inventory(state.path()).expect("inventory");

        assert!(inventory.complete);
        assert!(!inventory.truncated);
        assert_eq!(inventory.limit, MAX_LOCAL_WORK_INVENTORY_ITEMS);
        assert_eq!(inventory.items.len(), 2);
        assert_eq!(inventory.items[0].repository, "alpha/repo");
        assert_eq!(inventory.items[1].repository, "zeta/repo");
        assert_eq!(inventory.items[0].pull_request, 7);
        assert_eq!(inventory.items[1].pull_request, 7);
        assert_ne!(inventory.items[0].exact_head, inventory.items[1].exact_head);
        assert_ne!(
            inventory.items[0].work_item_id,
            inventory.items[1].work_item_id
        );
    }

    #[test]
    fn remote_inventory_completeness_is_recomputed_from_bounded_items() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        seed(&ledger, "owner/repo", 8, "remote-completeness");
        let inventory = local_work_inventory(state.path()).expect("inventory");
        validate_remote_inventory(&inventory).expect("canonical complete inventory");

        let mut short_truncation = inventory.clone();
        short_truncation.complete = false;
        short_truncation.truncated = true;
        assert!(validate_remote_inventory(&short_truncation).is_err());

        let mut false_partial = inventory.clone();
        false_partial.complete = false;
        assert!(validate_remote_inventory(&false_partial).is_err());

        let mut legacy_complete = inventory.clone();
        legacy_complete.items[0].repository_provider = None;
        legacy_complete.items[0].repository_id = None;
        assert!(validate_remote_inventory(&legacy_complete).is_err());

        let mut noncanonical_empty = empty_inventory();
        noncanonical_empty.complete = false;
        assert!(validate_remote_inventory(&noncanonical_empty).is_err());
    }

    #[test]
    fn absent_inventory_is_empty_and_creates_nothing() {
        let parent = tempfile::tempdir().expect("parent");
        let state = parent.path().join("absent-state");
        let inventory = local_work_inventory(&state).expect("empty inventory");
        assert!(inventory.complete);
        assert!(!inventory.truncated);
        assert!(inventory.items.is_empty());
        assert!(!state.exists());
    }

    #[cfg(unix)]
    #[test]
    fn empty_inventory_refuses_symlinked_directory_and_orphan_sidecars() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let state = tempfile::tempdir().expect("state");
        let outside = tempfile::tempdir().expect("outside");
        symlink(outside.path(), state.path().join("work-ledger")).expect("directory symlink");
        let symlink_error = local_work_inventory(state.path()).expect_err("symlink must refuse");
        assert!(symlink_error.to_string().contains("regular directory"));

        fs::remove_file(state.path().join("work-ledger")).expect("remove symlink");
        let directory = state.path().join("work-ledger");
        fs::create_dir(&directory).expect("ledger directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("protect directory");
        let wal = directory.join("work-items.sqlite3-wal");
        fs::write(&wal, b"orphan WAL").expect("orphan WAL");
        fs::set_permissions(&wal, fs::Permissions::from_mode(0o600)).expect("protect WAL");
        let before = snapshot(state.path());

        let orphan_error = local_work_inventory(state.path()).expect_err("orphan must refuse");

        assert!(orphan_error.to_string().contains("orphan SQLite sidecars"));
        assert_eq!(snapshot(state.path()), before);
    }

    #[test]
    fn inventory_refuses_unbound_work_instead_of_reporting_complete() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        ledger
            .import(&[candidate("alpha/repo", 7, &"a".repeat(40), "unbound")])
            .expect("unbound import");

        let error = local_work_inventory(state.path()).expect_err("unbound work must refuse");

        assert!(error.to_string().contains("unbound local work row"));
    }

    #[test]
    fn migrated_legacy_binding_has_unknown_immutable_identity_and_is_incomplete() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        seed_production_v11(&ledger, "alpha/repo", 7, "GEN-7", "legacy-v11");
        drop(ledger);

        WorkLedger::open(state.path()).expect("migrate v11");
        let inventory = local_work_inventory(state.path()).expect("legacy inventory");

        assert!(!inventory.complete);
        assert_eq!(inventory.items.len(), 1);
        assert_eq!(inventory.items[0].repository_provider, None);
        assert_eq!(inventory.items[0].repository_id, None);
    }

    #[test]
    fn inventory_refuses_sql_side_oversize_and_control_handle_before_rendering() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        seed(&ledger, "alpha/repo", 7, "GEN-7");
        let connection = ledger.connect_read_write().expect("fixture connection");
        connection
            .execute_batch(
                "DROP TRIGGER workstream_projection_binding_identity_immutable;
                 PRAGMA ignore_check_constraints = ON;",
            )
            .expect("permit planted corruption");
        connection
            .execute(
                "UPDATE workstream_projection_bindings SET workstream_handle = ?1",
                ["x".repeat(4096)],
            )
            .expect("oversize handle");
        drop(connection);
        let oversize = local_work_inventory(state.path()).expect_err("oversize must refuse");
        assert!(oversize.to_string().contains("SQL-side identity bound"));

        let connection = ledger.connect_read_write().expect("fixture connection");
        connection
            .execute_batch("PRAGMA ignore_check_constraints = ON;")
            .expect("permit planted control corruption");
        connection
            .execute(
                "UPDATE workstream_projection_bindings SET workstream_handle = 'GEN-7' || char(10) || 'spoof'",
                [],
            )
            .expect("control handle");
        drop(connection);
        let control = local_work_inventory(state.path()).expect_err("control must refuse");
        assert!(
            control
                .to_string()
                .contains("invalid canonical workstream handle")
        );
    }

    #[test]
    fn inventory_never_projects_historical_agent_ownership_as_current_custody() {
        let connection = Connection::open_in_memory().expect("fixture database");
        connection
            .execute_batch(
                "CREATE TABLE work_items (
                   id TEXT PRIMARY KEY, repo TEXT, pr INTEGER, head_sha TEXT, phase TEXT,
                   work_generation INTEGER, owner_id TEXT, owner_generation INTEGER
                 );
                 CREATE TABLE workstream_projection_bindings (
                   work_item_id TEXT PRIMARY KEY, repository_provider TEXT, repository_id TEXT,
                   repository TEXT, exact_head TEXT, workstream_handle TEXT
                 );
                 CREATE TABLE agent_ownership (
                   ownership_id TEXT PRIMARY KEY, work_item_id TEXT, state TEXT,
                   work_generation INTEGER, owner_generation INTEGER
                 );
                 CREATE TABLE ownership_roots (
                   root_uuid TEXT PRIMARY KEY
                     CHECK(length(root_uuid) = 36
                           AND substr(root_uuid, 9, 1) = '-'
                           AND substr(root_uuid, 14, 1) = '-'
                           AND substr(root_uuid, 19, 1) = '-'
                           AND substr(root_uuid, 24, 1) = '-'
                           AND replace(root_uuid, '-', '') NOT GLOB '*[^0-9a-f]*'),
                   work_item_id TEXT NOT NULL UNIQUE REFERENCES work_items(id) ON DELETE RESTRICT,
                   created_at TEXT NOT NULL CHECK(length(created_at) >= 20)
                 );",
            )
            .expect("fixture schema");
        let work_id = opaque_ref("wi", "generation-aware inventory");
        let owner_id = opaque_ref("owner", "generation-aware inventory");
        let ownership_id = opaque_ref("ao", "historical ownership");
        let head = "a".repeat(40);
        connection
            .execute(
                "INSERT INTO work_items VALUES (?1, 'alpha/repo', 7, ?2, 'managed', 2, ?3, 1)",
                rusqlite::params![work_id, head, owner_id],
            )
            .expect("current work");
        connection
            .execute(
                "INSERT INTO workstream_projection_bindings
                 VALUES (?1, 'github.com', 'R_test', 'alpha/repo', ?2, 'GEN-7')",
                rusqlite::params![work_id, head],
            )
            .expect("binding");
        connection
            .execute(
                "INSERT INTO agent_ownership VALUES (?1, ?2, 'acknowledged', 1, 1)",
                rusqlite::params![ownership_id, work_id],
            )
            .expect("historical ownership");
        connection
            .execute(
                "INSERT INTO ownership_roots VALUES
                 ('00000000-0000-0000-0000-000000000001', ?1, '2026-08-31T00:00:00Z')",
                [&work_id],
            )
            .expect("current ownership root");

        let rows =
            load_inventory_rows(&connection, InventorySchema::Current).expect("inventory rows");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.work_generation, 2);
        assert_eq!(rows[0].0.ownership_id, None);
        assert_eq!(rows[0].0.ownership_state, None);
        assert_eq!(rows[0].0.ownership_work_generation, None);
        assert_eq!(rows[0].0.ownership_owner_generation, None);
    }

    #[test]
    fn inventory_is_explicitly_bounded_and_marks_truncation() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        let mut candidates = Vec::new();
        let mut bindings = Vec::new();
        for index in 0..=MAX_LOCAL_WORK_INVENTORY_ITEMS {
            let label = format!("inventory-{index:04}");
            let handle = format!("GEN-{}", index + 1);
            let head = digest(label.as_bytes())[..40].to_owned();
            let candidate = candidate("alpha/repo", (index + 1) as u64, &head, &label);
            bindings.push((candidate.work_id.clone(), handle, head));
            candidates.push(candidate);
        }
        ledger.import(&candidates).expect("bulk import");
        let mut connection = ledger.connect_read_write().expect("test connection");
        let transaction = connection.transaction().expect("binding transaction");
        for (work_id, handle, head) in &bindings {
            transaction
                .execute(
                    "INSERT INTO workstream_projection_bindings
                     (work_item_id, workstream_handle, plan_sha256, root_revision,
                      issue_revision, projection_revision, material_event_revision,
                      repository_provider, repository_id, repository, exact_head, created_at)
                     VALUES (?1, ?2, ?3, 1, 1, 1, 1, 'github.com', ?2,
                             'alpha/repo', ?4,
                             '2026-08-31T00:00:00Z')",
                    rusqlite::params![work_id, handle, digest(handle.as_bytes()), head],
                )
                .expect("insert binding");
        }
        transaction.commit().expect("commit bindings");
        drop(connection);

        let inventory = local_work_inventory(state.path()).expect("bounded inventory");

        assert!(!inventory.complete);
        assert!(inventory.truncated);
        assert_eq!(inventory.limit, MAX_LOCAL_WORK_INVENTORY_ITEMS);
        assert_eq!(inventory.items.len(), MAX_LOCAL_WORK_INVENTORY_ITEMS);
        assert_eq!(inventory.items[0].pull_request, 1);
        assert_eq!(inventory.items.last().unwrap().pull_request, 256);
    }

    #[test]
    fn inventory_refuses_newer_schema_and_identity_mismatch() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        seed(&ledger, "alpha/repo", 7, "GEN-7");
        let connection = ledger.connect_read_write().expect("test connection");
        connection
            .execute("UPDATE work_items SET repo = 'other/repo'", [])
            .expect("plant identity contradiction");
        drop(connection);
        let error = local_work_inventory(state.path()).expect_err("identity must refuse");
        assert!(error.to_string().contains("contradictory repository"));

        let connection = ledger.connect_read_write().expect("test connection");
        connection
            .pragma_update(None, "user_version", super::super::SCHEMA_VERSION + 1)
            .expect("plant newer schema");
        drop(connection);
        #[cfg(unix)]
        let before = snapshot(state.path());
        assert!(matches!(
            local_work_inventory(state.path()),
            Err(WorkLedgerError::UnsupportedSchema(_))
        ));
        #[cfg(unix)]
        assert_eq!(snapshot(state.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn inventory_refuses_corrupt_storage_without_touching_it() {
        use std::os::unix::fs::PermissionsExt;

        let state = tempfile::tempdir().expect("state");
        let directory = state.path().join("work-ledger");
        fs::create_dir(&directory).expect("ledger directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("protect directory");
        let database = directory.join("work-items.sqlite3");
        fs::write(&database, b"not a sqlite database").expect("corrupt fixture");
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600))
            .expect("protect database");
        let before = snapshot(state.path());

        assert!(local_work_inventory(state.path()).is_err());

        assert_eq!(snapshot(state.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn inventory_refuses_database_above_the_preallocation_resource_ceiling() {
        use std::os::unix::fs::PermissionsExt;

        let state = tempfile::tempdir().expect("state");
        let directory = state.path().join("work-ledger");
        fs::create_dir(&directory).expect("ledger directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("protect directory");
        let database = directory.join("work-items.sqlite3");
        let file = fs::File::create(&database).expect("database fixture");
        file.set_len(MAX_LEDGER_DATABASE_BYTES + 1)
            .expect("oversize sparse database");
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600))
            .expect("protect database");

        let error = local_work_inventory(state.path()).expect_err("oversize must refuse");

        assert!(error.to_string().contains("resource bound"));
    }

    #[cfg(unix)]
    #[test]
    fn inventory_preserves_entire_state_tree_while_writer_domain_is_exclusively_held() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        seed(&ledger, "alpha/repo", 7, "GEN-7");
        let writer = crate::writer_domain_lease::acquire_exclusive_for_protected_path(state.path())
            .expect("exclusive writer-domain evidence");
        let before = snapshot(state.path());

        let inventory = local_work_inventory(state.path()).expect("lock-free inventory");

        let after = snapshot(state.path());
        assert_eq!(inventory.items.len(), 1);
        assert_eq!(after, before);
        drop(writer);
    }

    #[cfg(unix)]
    #[test]
    fn inventory_refuses_live_wal_without_touching_wal_or_shared_memory() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        seed(&ledger, "alpha/repo", 7, "GEN-7");
        let connection = ledger.connect_read_write().expect("live WAL connection");
        connection
            .execute_batch("PRAGMA wal_autocheckpoint = 0;")
            .expect("disable checkpoint");
        connection
            .execute("UPDATE work_items SET phase = 'managed'", [])
            .expect("commit live WAL row");
        let database = WorkLedger::path_at(state.path());
        assert!(sqlite_sidecar(&database, "-wal").exists());
        assert!(sqlite_sidecar(&database, "-shm").exists());
        let before = snapshot(state.path());

        assert_eq!(
            immutable_legacy_status(state.path()).expect("current status routing"),
            None
        );
        assert_eq!(snapshot(state.path()), before);

        let error = local_work_inventory(state.path()).expect_err("live WAL must refuse");

        let after = snapshot(state.path());
        assert!(error.to_string().contains("uncheckpointed WAL"));
        assert_eq!(after, before);
        drop(connection);
    }

    #[cfg(unix)]
    #[test]
    fn inventory_refuses_wal_created_in_the_preopen_race_window() {
        use std::cell::RefCell;

        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        seed(&ledger, "alpha/repo", 7, "GEN-7");
        let writer = RefCell::new(None);

        let error = local_work_inventory_with_preopen_hook(state.path(), || {
            let connection = ledger.connect_read_write()?;
            connection.execute_batch("PRAGMA wal_autocheckpoint = 0;")?;
            connection.execute("UPDATE work_items SET phase = 'managed'", [])?;
            *writer.borrow_mut() = Some(connection);
            Ok(())
        })
        .expect_err("WAL created after the snapshot must refuse");

        assert!(
            error
                .to_string()
                .contains("changed during zero-write inventory")
        );
        assert!(writer.borrow().is_some());
    }

    #[cfg(unix)]
    fn snapshot(root: &Path) -> BTreeMap<String, (String, u32, u64, SystemTime)> {
        use sha2::{Digest as _, Sha256};
        use std::os::unix::fs::MetadataExt;

        fn visit(
            root: &Path,
            path: &Path,
            snapshot: &mut BTreeMap<String, (String, u32, u64, SystemTime)>,
        ) {
            let mut entries = fs::read_dir(path)
                .expect("read state tree")
                .collect::<Result<Vec<_>, _>>()
                .expect("state entries");
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let entry_path = entry.path();
                let metadata = fs::symlink_metadata(&entry_path).expect("entry metadata");
                let relative = entry_path
                    .strip_prefix(root)
                    .expect("relative entry")
                    .to_string_lossy()
                    .into_owned();
                let content_digest = if metadata.is_file() {
                    hex::encode(Sha256::digest(fs::read(&entry_path).expect("entry bytes")))
                } else {
                    String::new()
                };
                snapshot.insert(
                    relative,
                    (
                        content_digest,
                        metadata.mode(),
                        metadata.len(),
                        metadata.modified().expect("mtime"),
                    ),
                );
                if metadata.is_dir() {
                    visit(root, &entry_path, snapshot);
                }
            }
        }

        let mut result = BTreeMap::new();
        visit(root, root, &mut result);
        result
    }
}
