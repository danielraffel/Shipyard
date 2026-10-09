//! Replace a file so that a crash, a kill, or a power loss leaves either the
//! old contents or the new ones, complete, never a torn or empty file.
//!
//! A rename alone is atomic in the namespace but not durable: without a sync
//! the new file's data, and the rename itself, can still be in the page cache
//! when the host goes down, and the file reads back empty or old after reboot.
//! So the temporary file is synced before the rename and its directory after.

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::Path;

use serde::Serialize;

#[cfg(test)]
thread_local! {
    /// Syncs issued on this thread, so a test can prove a save was durable.
    static SYNCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Syncs this thread has issued through this module, for tests.
#[cfg(test)]
pub(crate) fn syncs_on_this_thread() -> usize {
    SYNCS.with(std::cell::Cell::get)
}

fn sync(file: &File) -> io::Result<()> {
    #[cfg(test)]
    SYNCS.with(|count| count.set(count.get() + 1));
    file.sync_all()
}

/// Sync the directory holding `path`, making a rename or create in it durable.
/// A no-op where std offers no directory handle that can be synced.
///
/// # Errors
///
/// When the directory cannot be opened or synced.
pub fn sync_parent_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        sync(&File::open(parent)?)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Replace `path` with `bytes` durably, creating its directory if needed.
///
/// Callers that write protected state still take their writer-domain lease
/// first; this function only makes the write itself crash-safe.
///
/// # Errors
///
/// When the directory, the temporary file, the write, a sync, or the rename
/// fails. On error `path` still holds its previous contents.
pub fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} has no parent directory", path.display()),
            )
        })?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    sync(temp.as_file())?;
    temp.persist(path).map_err(|error| error.error)?;
    sync_parent_directory(path)
}

/// [`replace`] with `value` as pretty-printed JSON, plus a trailing newline
/// when `newline` is set.
///
/// # Errors
///
/// When `value` does not serialize, or as [`replace`].
pub fn replace_json<T: Serialize + ?Sized>(
    path: &Path,
    value: &T,
    newline: bool,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if newline {
        bytes.push(b'\n');
    }
    replace(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_writes_complete_bytes_and_syncs_the_file_and_its_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("nested/dir/state.json");
        let before = syncs_on_this_thread();
        replace(&path, b"first").expect("create");
        assert_eq!(fs::read(&path).expect("read"), b"first");
        #[cfg(unix)]
        assert_eq!(
            syncs_on_this_thread() - before,
            2,
            "the file, then its directory"
        );
        replace(&path, b"second, longer").expect("replace");
        assert_eq!(fs::read(&path).expect("read"), b"second, longer");
        let leftovers: Vec<_> = fs::read_dir(path.parent().expect("parent"))
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(
            leftovers,
            ["state.json"],
            "no temporary file is left behind"
        );
    }

    #[test]
    fn replace_json_matches_serde_pretty_bytes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("value.json");
        let value = serde_json::json!({"b": 1, "a": [true]});
        replace_json(&path, &value, false).expect("write");
        assert_eq!(
            fs::read(&path).expect("read"),
            serde_json::to_vec_pretty(&value).expect("bytes")
        );
        replace_json(&path, &value, true).expect("write");
        assert!(fs::read_to_string(&path).expect("read").ends_with("}\n"));
    }

    #[test]
    fn a_failed_replace_keeps_the_previous_contents() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("state.json");
        replace(&path, b"kept").expect("write");
        // A directory where the file should be makes the rename fail.
        let blocked = temp.path().join("blocked");
        fs::create_dir_all(blocked.join("child")).expect("dir");
        assert!(replace(&blocked, b"x").is_err());
        assert_eq!(fs::read(&path).expect("read"), b"kept");
        assert!(replace(Path::new("relative-without-parent"), b"x").is_err());
    }
}
