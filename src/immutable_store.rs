//! Private, no-follow, crash-consistent immutable byte publication.
//!
//! This is the shared filesystem authority for compact controller receipts.
//! Callers retain schema validation and logical-key ownership; this module
//! supplies only private storage, bounded reads, byte-identical replay, and
//! no-overwrite publication.

use std::fmt::{Display, Formatter};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

use fs2::FileExt;

use crate::parallel_proof::StoreWriteOutcome;

const MAX_DIRECTORY_ENTRIES: usize = 4_096;
#[cfg(unix)]
static PENDING_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(crate) enum ImmutableStoreError {
    InvalidRoot,
    UnsafePath(PathBuf),
    LimitExceeded { max: usize, found: usize },
    Missing(String),
    Conflict(String),
    Io(std::io::Error),
}

impl Display for ImmutableStoreError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRoot => formatter.write_str("immutable store root is invalid"),
            Self::UnsafePath(path) => {
                write!(
                    formatter,
                    "immutable store path is unsafe: {}",
                    path.display()
                )
            }
            Self::LimitExceeded { max, found } => {
                write!(formatter, "immutable record exceeds {max} bytes: {found}")
            }
            Self::Missing(key) => write!(formatter, "immutable record is missing: {key}"),
            Self::Conflict(key) => write!(formatter, "immutable record conflicts: {key}"),
            Self::Io(error) => Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for ImmutableStoreError {}

impl From<std::io::Error> for ImmutableStoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ImmutableByteStore {
    root: PathBuf,
    #[cfg(unix)]
    directory: Arc<File>,
    max_record_bytes: usize,
}

impl ImmutableByteStore {
    pub(crate) fn open(
        root: impl Into<PathBuf>,
        max_record_bytes: usize,
    ) -> Result<Self, ImmutableStoreError> {
        let root = root.into();
        if max_record_bytes == 0
            || root.file_name().is_none()
            || root
                .components()
                .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
        {
            return Err(ImmutableStoreError::InvalidRoot);
        }
        let parent = root
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        validate_real_directory(parent, false)?;
        let creating = fs::symlink_metadata(&root)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        let migrating = !creating && needs_private_migration(&root)?;
        let _create_lease = if creating || migrating {
            crate::writer_domain_lease::acquire_for_protected_path(&root)?
        } else {
            None
        };
        create_private_directory(&root, parent)?;
        if migrating {
            migrate_private_directory(&root, parent)?;
        }
        validate_real_directory(&root, true)?;
        #[cfg(unix)]
        let directory = Arc::new(open_directory_nofollow(&root)?);
        #[cfg(unix)]
        validate_private_directory_file(&root, &directory)?;
        #[cfg(not(unix))]
        sync_directory(&root)?;
        let store = Self {
            root,
            #[cfg(unix)]
            directory,
            max_record_bytes,
        };
        if store.has_pending_records()? {
            let _lease = crate::writer_domain_lease::acquire_for_protected_path(&store.root)?;
            store.reconcile_pending_records()?;
        }
        Ok(store)
    }

    pub(crate) fn put(
        &self,
        logical_key: &str,
        bytes: &[u8],
    ) -> Result<StoreWriteOutcome, ImmutableStoreError> {
        if bytes.len() > self.max_record_bytes {
            return Err(ImmutableStoreError::LimitExceeded {
                max: self.max_record_bytes,
                found: bytes.len(),
            });
        }
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&self.root)?;
        self.verify_directory_binding()?;
        let destination_name = Self::record_name(logical_key, "json");
        let lock_name = Self::record_name(logical_key, "lock");
        let lock = self.open_lock(&lock_name)?;
        ensure_private_lock_file(&self.root.join(&lock_name), &lock)?;
        lock.lock_exclusive()?;
        let result = (|| {
            if self.contains_name(&destination_name)? {
                return if self.read_name(&destination_name)? == bytes {
                    Ok(StoreWriteOutcome::AlreadyPresent)
                } else {
                    Err(ImmutableStoreError::Conflict(logical_key.to_owned()))
                };
            }
            self.publish_noreplace(&destination_name, bytes, logical_key)
        })();
        FileExt::unlock(&lock)?;
        self.verify_directory_binding()?;
        result
    }

    pub(crate) fn load(&self, logical_key: &str) -> Result<Vec<u8>, ImmutableStoreError> {
        self.verify_directory_binding()?;
        let name = Self::record_name(logical_key, "json");
        if !self.contains_name(&name)? {
            return Err(ImmutableStoreError::Missing(logical_key.to_owned()));
        }
        let bytes = self.read_name(&name)?;
        self.verify_directory_binding()?;
        Ok(bytes)
    }

    pub(crate) fn contains(&self, logical_key: &str) -> Result<bool, ImmutableStoreError> {
        self.verify_directory_binding()?;
        self.contains_name(&Self::record_name(logical_key, "json"))
    }

    fn record_name(logical_key: &str, extension: &str) -> String {
        let digest = crate::parallel_proof::Sha256Digest::of_bytes(logical_key.as_bytes());
        format!("{}.{extension}", digest.as_str())
    }

    #[cfg(unix)]
    fn pending_names(&self) -> Result<Vec<String>, ImmutableStoreError> {
        let entries = rustix::fs::Dir::read_from(&*self.directory).map_err(std::io::Error::from)?;
        let mut pending = Vec::new();
        let mut count = 0_usize;
        for entry in entries {
            let entry = entry.map_err(std::io::Error::from)?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            if bytes.starts_with(b".pending-") {
                count += 1;
                if count > MAX_DIRECTORY_ENTRIES {
                    return Err(ImmutableStoreError::LimitExceeded {
                        max: MAX_DIRECTORY_ENTRIES,
                        found: count,
                    });
                }
                pending.push(
                    std::str::from_utf8(bytes)
                        .map_err(|_| ImmutableStoreError::UnsafePath(self.root.clone()))?
                        .to_owned(),
                );
            }
        }
        Ok(pending)
    }

    #[cfg(unix)]
    fn has_pending_records(&self) -> Result<bool, ImmutableStoreError> {
        Ok(!self.pending_names()?.is_empty())
    }

    #[cfg(not(unix))]
    fn has_pending_records(&self) -> Result<bool, ImmutableStoreError> {
        Ok(false)
    }

    #[cfg(unix)]
    fn reconcile_pending_records(&self) -> Result<(), ImmutableStoreError> {
        use rustix::fs::{AtFlags, unlinkat};
        for name in self.pending_names()? {
            let file = self.open_readonly(&name)?;
            validate_private_regular_file(&self.root.join(&name), &file)?;
            if file.metadata()?.len() > self.max_record_bytes as u64 {
                return Err(ImmutableStoreError::UnsafePath(self.root.join(name)));
            }
            unlinkat(&*self.directory, &name, AtFlags::empty()).map_err(std::io::Error::from)?;
        }
        self.directory.sync_all()?;
        self.verify_directory_binding()
    }

    fn read_name(&self, name: &str) -> Result<Vec<u8>, ImmutableStoreError> {
        let path = self.root.join(name);
        let file = self.open_readonly(name)?;
        validate_private_regular_file(&path, &file)?;
        let metadata = file.metadata()?;
        if metadata.len() > self.max_record_bytes as u64 {
            return Err(ImmutableStoreError::LimitExceeded {
                max: self.max_record_bytes,
                found: usize::try_from(metadata.len()).unwrap_or(usize::MAX),
            });
        }
        let mut bytes = Vec::new();
        file.take(self.max_record_bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > self.max_record_bytes {
            return Err(ImmutableStoreError::LimitExceeded {
                max: self.max_record_bytes,
                found: bytes.len(),
            });
        }
        Ok(bytes)
    }

    #[cfg(unix)]
    fn open_readonly(&self, name: &str) -> Result<File, ImmutableStoreError> {
        use rustix::fs::{Mode, OFlags, openat};
        Ok(File::from(
            openat(
                &*self.directory,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        ))
    }

    #[cfg(not(unix))]
    fn open_readonly(&self, name: &str) -> Result<File, ImmutableStoreError> {
        open_readonly_nofollow(&self.root.join(name))
    }

    #[cfg(unix)]
    fn open_lock(&self, name: &str) -> Result<File, ImmutableStoreError> {
        use rustix::fs::{Mode, OFlags, openat};
        Ok(File::from(
            openat(
                &*self.directory,
                name,
                OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(std::io::Error::from)?,
        ))
    }

    #[cfg(not(unix))]
    fn open_lock(&self, name: &str) -> Result<File, ImmutableStoreError> {
        open_lock_nofollow(&self.root.join(name))
    }

    #[cfg(unix)]
    fn contains_name(&self, name: &str) -> Result<bool, ImmutableStoreError> {
        use rustix::fs::{AtFlags, statat};
        match statat(&*self.directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat)
                if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                    == rustix::fs::FileType::RegularFile =>
            {
                Ok(true)
            }
            Ok(_) => Err(ImmutableStoreError::UnsafePath(self.root.join(name))),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(error) => Err(std::io::Error::from(error).into()),
        }
    }

    #[cfg(not(unix))]
    fn contains_name(&self, name: &str) -> Result<bool, ImmutableStoreError> {
        let path = self.root.join(name);
        reject_non_regular_if_present(&path)?;
        path_exists_nofollow(&path)
    }

    #[cfg(unix)]
    fn publish_noreplace(
        &self,
        destination_name: &str,
        bytes: &[u8],
        logical_key: &str,
    ) -> Result<StoreWriteOutcome, ImmutableStoreError> {
        use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, openat, renameat_with, unlinkat};

        let pending_name = format!(
            ".pending-{}-{}",
            crate::parallel_proof::Sha256Digest::of_bytes(
                format!(
                    "{}:{:?}:{destination_name}",
                    std::process::id(),
                    std::thread::current().id()
                )
                .as_bytes()
            )
            .as_str(),
            PENDING_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        );
        let mut pending = File::from(
            openat(
                &*self.directory,
                &pending_name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(std::io::Error::from)?,
        );
        let result = (|| {
            pending.write_all(bytes)?;
            pending.sync_all()?;
            validate_private_regular_file(&self.root.join(&pending_name), &pending)?;
            match renameat_with(
                &*self.directory,
                &pending_name,
                &*self.directory,
                destination_name,
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => {
                    self.directory.sync_all()?;
                    if self.read_name(destination_name)? != bytes {
                        return Err(ImmutableStoreError::UnsafePath(
                            self.root.join(destination_name),
                        ));
                    }
                    Ok(StoreWriteOutcome::Created)
                }
                Err(rustix::io::Errno::EXIST) => {
                    unlinkat(&*self.directory, &pending_name, AtFlags::empty())
                        .map_err(std::io::Error::from)?;
                    self.directory.sync_all()?;
                    if self.read_name(destination_name)? == bytes {
                        Ok(StoreWriteOutcome::AlreadyPresent)
                    } else {
                        Err(ImmutableStoreError::Conflict(logical_key.to_owned()))
                    }
                }
                Err(error) => Err(std::io::Error::from(error).into()),
            }
        })();
        if result.is_err() {
            let _ = unlinkat(&*self.directory, &pending_name, AtFlags::empty());
            let _ = self.directory.sync_all();
        }
        result
    }

    #[cfg(not(unix))]
    fn publish_noreplace(
        &self,
        destination_name: &str,
        bytes: &[u8],
        logical_key: &str,
    ) -> Result<StoreWriteOutcome, ImmutableStoreError> {
        let destination = self.root.join(destination_name);
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        temporary.write_all(bytes)?;
        temporary.as_file_mut().sync_all()?;
        match temporary.persist_noclobber(&destination) {
            Ok(file) => {
                set_private_file_permissions(&destination)?;
                validate_private_regular_file(&destination, &file)?;
                sync_directory(&self.root)?;
                Ok(StoreWriteOutcome::Created)
            }
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if self.read_name(destination_name)? == bytes {
                    Ok(StoreWriteOutcome::AlreadyPresent)
                } else {
                    Err(ImmutableStoreError::Conflict(logical_key.to_owned()))
                }
            }
            Err(error) => Err(error.error.into()),
        }
    }

    #[cfg(unix)]
    fn verify_directory_binding(&self) -> Result<(), ImmutableStoreError> {
        use std::os::unix::fs::MetadataExt as _;
        let rebound = open_directory_nofollow(&self.root)?;
        let pinned = self.directory.metadata()?;
        let observed = rebound.metadata()?;
        if pinned.dev() != observed.dev() || pinned.ino() != observed.ino() {
            return Err(ImmutableStoreError::UnsafePath(self.root.clone()));
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn verify_directory_binding(&self) -> Result<(), ImmutableStoreError> {
        validate_real_directory(&self.root, true)
    }
}

#[cfg(not(unix))]
fn path_exists_nofollow(path: &Path) -> Result<bool, ImmutableStoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn reject_non_regular_if_present(path: &Path) -> Result<(), ImmutableStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(ImmutableStoreError::UnsafePath(path.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_real_directory(path: &Path, private: bool) -> Result<(), ImmutableStoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(ImmutableStoreError::UnsafePath(path.to_path_buf()));
    }
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.permissions().mode() & 0o777 != 0o700
            || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        {
            return Err(ImmutableStoreError::UnsafePath(path.to_path_buf()));
        }
    }
    Ok(())
}

fn create_private_directory(root: &Path, parent: &Path) -> Result<(), ImmutableStoreError> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, mkdirat, open};
        let parent_directory = File::from(
            open(
                parent,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        );
        let name = root.file_name().ok_or(ImmutableStoreError::InvalidRoot)?;
        match mkdirat(&parent_directory, name, Mode::RWXU) {
            Ok(()) => parent_directory.sync_all()?,
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
        Ok(())
    }
    #[cfg(not(unix))]
    match fs::create_dir(root) {
        Ok(()) => {
            set_private_directory_permissions(root)?;
            sync_directory(parent)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    #[cfg(not(unix))]
    Ok(())
}

#[cfg(unix)]
fn needs_private_migration(root: &Path) -> Result<bool, ImmutableStoreError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
    {
        return Err(ImmutableStoreError::UnsafePath(root.to_path_buf()));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode == 0o700 {
        return Ok(false);
    }
    if mode & 0o022 != 0 {
        return Err(ImmutableStoreError::UnsafePath(root.to_path_buf()));
    }
    Ok(true)
}

#[cfg(not(unix))]
fn needs_private_migration(_root: &Path) -> Result<bool, ImmutableStoreError> {
    Ok(false)
}

#[cfg(unix)]
fn migrate_private_directory(root: &Path, parent: &Path) -> Result<(), ImmutableStoreError> {
    use rustix::fs::{Mode, fchmod};
    let directory = open_directory_nofollow(root)?;
    fchmod(&directory, Mode::RWXU).map_err(std::io::Error::from)?;
    directory.sync_all()?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn migrate_private_directory(_root: &Path, _parent: &Path) -> Result<(), ImmutableStoreError> {
    Ok(())
}

#[cfg(unix)]
fn open_directory_nofollow(path: &Path) -> Result<File, ImmutableStoreError> {
    use rustix::fs::{Mode, OFlags, open};
    Ok(File::from(
        open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    ))
}

#[cfg(unix)]
fn validate_private_directory_file(
    path: &Path,
    directory: &File,
) -> Result<(), ImmutableStoreError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let metadata = directory.metadata()?;
    if !metadata.is_dir()
        || metadata.permissions().mode() & 0o777 != 0o700
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
    {
        return Err(ImmutableStoreError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn open_readonly_nofollow(path: &Path) -> Result<File, ImmutableStoreError> {
    Ok(File::open(path)?)
}

#[cfg(not(unix))]
fn open_lock_nofollow(path: &Path) -> Result<File, ImmutableStoreError> {
    Ok(fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?)
}

fn validate_private_regular_file(path: &Path, file: &File) -> Result<(), ImmutableStoreError> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(ImmutableStoreError::UnsafePath(path.to_path_buf()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let bound = fs::symlink_metadata(path)?;
        if metadata.permissions().mode() & 0o777 != 0o600
            || metadata.nlink() != 1
            || metadata.uid() != nix::unistd::Uid::effective().as_raw()
            || bound.dev() != metadata.dev()
            || bound.ino() != metadata.ino()
        {
            return Err(ImmutableStoreError::UnsafePath(path.to_path_buf()));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_private_lock_file(path: &Path, file: &File) -> Result<(), ImmutableStoreError> {
    use rustix::fs::{Mode, fchmod};
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let metadata = file.metadata()?;
    let bound = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || bound.dev() != metadata.dev()
        || bound.ino() != metadata.ino()
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(ImmutableStoreError::UnsafePath(path.to_path_buf()));
    }
    if metadata.permissions().mode() & 0o777 != 0o600 {
        fchmod(file, Mode::RUSR | Mode::WUSR).map_err(std::io::Error::from)?;
        file.sync_all()?;
    }
    validate_private_regular_file(path, file)
}

#[cfg(not(unix))]
fn ensure_private_lock_file(path: &Path, file: &File) -> Result<(), ImmutableStoreError> {
    validate_private_regular_file(path, file)
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), ImmutableStoreError> {
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), ImmutableStoreError> {
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
fn sync_directory(path: &Path) -> Result<(), ImmutableStoreError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> Result<(), ImmutableStoreError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immutable_replay_survives_reopen_and_conflicts_fail() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("records");
        let first = ImmutableByteStore::open(&root, 32).unwrap();
        assert_eq!(
            first.put("key", b"one").unwrap(),
            StoreWriteOutcome::Created
        );
        drop(first);
        let second = ImmutableByteStore::open(&root, 32).unwrap();
        assert_eq!(second.load("key").unwrap(), b"one");
        assert_eq!(
            second.put("key", b"one").unwrap(),
            StoreWriteOutcome::AlreadyPresent
        );
        assert!(matches!(
            second.put("key", b"two"),
            Err(ImmutableStoreError::Conflict(key)) if key == "key"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_non_private_modes_fail_closed() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let linked = parent.path().join("linked");
        symlink(outside.path(), &linked).unwrap();
        assert!(matches!(
            ImmutableByteStore::open(&linked, 32),
            Err(ImmutableStoreError::UnsafePath(_))
        ));

        let root = parent.path().join("records");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o770)).unwrap();
        assert!(matches!(
            ImmutableByteStore::open(&root, 32),
            Err(ImmutableStoreError::UnsafePath(_))
        ));

        let legacy = parent.path().join("legacy-records");
        fs::create_dir(&legacy).unwrap();
        fs::set_permissions(&legacy, fs::Permissions::from_mode(0o755)).unwrap();
        let legacy_lock = legacy.join(ImmutableByteStore::record_name("legacy", "lock"));
        fs::write(&legacy_lock, b"").unwrap();
        fs::set_permissions(&legacy_lock, fs::Permissions::from_mode(0o644)).unwrap();
        let migrated = ImmutableByteStore::open(&legacy, 32).unwrap();
        migrated.put("legacy", b"bytes").unwrap();
        assert_eq!(
            fs::metadata(&legacy).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(legacy_lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn replaced_directory_symlinked_record_and_public_file_fail_closed() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("records");
        let store = ImmutableByteStore::open(&root, 32).unwrap();
        let destination = root.join(ImmutableByteStore::record_name("linked", "json"));
        symlink(parent.path().join("outside"), &destination).unwrap();
        assert!(matches!(
            store.put("linked", b"bytes"),
            Err(ImmutableStoreError::UnsafePath(_))
        ));
        fs::remove_file(destination).unwrap();

        store.put("public", b"bytes").unwrap();
        let public = root.join(ImmutableByteStore::record_name("public", "json"));
        fs::set_permissions(&public, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            store.load("public"),
            Err(ImmutableStoreError::UnsafePath(_))
        ));

        let moved = parent.path().join("moved");
        fs::rename(&root, &moved).unwrap();
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(
            store.put("after-rebind", b"bytes"),
            Err(ImmutableStoreError::UnsafePath(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn restart_reconciles_private_pending_files_and_refuses_unsafe_pending_entries() {
        use rustix::fs::{Mode, OFlags, openat};
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("records");
        let store = ImmutableByteStore::open(&root, 32).unwrap();
        let mut pending = File::from(
            openat(
                &*store.directory,
                ".pending-crash",
                OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW,
                Mode::RUSR | Mode::WUSR,
            )
            .unwrap(),
        );
        pending.write_all(b"partial").unwrap();
        pending.sync_all().unwrap();
        drop(pending);
        drop(store);

        ImmutableByteStore::open(&root, 32).unwrap();
        assert!(!root.join(".pending-crash").exists());

        symlink(parent.path().join("outside"), root.join(".pending-unsafe")).unwrap();
        assert!(matches!(
            ImmutableByteStore::open(&root, 32),
            Err(ImmutableStoreError::Io(_) | ImmutableStoreError::UnsafePath(_))
        ));
    }
}
