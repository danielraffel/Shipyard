//! Validation for security-sensitive direct native executables.

use std::env;
use std::ffi::OsStr;
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) enum NativeExecutableError {
    Resolve(std::io::Error),
    Metadata(std::io::Error),
    NotFile,
    #[cfg(unix)]
    NotExecutable,
    Open(std::io::Error),
    Header(std::io::Error),
    Wrapper,
}

impl Display for NativeExecutableError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resolve(error) => write!(f, "could not resolve path: {error}"),
            Self::Metadata(error) => write!(f, "could not read metadata: {error}"),
            Self::NotFile => write!(f, "path is not a regular file"),
            #[cfg(unix)]
            Self::NotExecutable => write!(f, "file is not executable"),
            Self::Open(error) => write!(f, "could not open executable: {error}"),
            Self::Header(error) => write!(f, "could not read executable header: {error}"),
            Self::Wrapper => write!(f, "path is a script or wrapper, not a native executable"),
        }
    }
}

pub(crate) fn validate_native_executable(path: &Path) -> Result<PathBuf, NativeExecutableError> {
    let canonical = path
        .canonicalize()
        .map_err(NativeExecutableError::Resolve)?;
    let metadata = canonical
        .metadata()
        .map_err(NativeExecutableError::Metadata)?;
    if !metadata.is_file() {
        return Err(NativeExecutableError::NotFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(NativeExecutableError::NotExecutable);
        }
    }

    let mut file = File::open(&canonical).map_err(NativeExecutableError::Open)?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)
        .map_err(NativeExecutableError::Header)?;
    if !is_native_executable_magic(magic) {
        return Err(NativeExecutableError::Wrapper);
    }
    Ok(canonical)
}

pub(crate) fn resolve_native_executable_from_path(
    executable: &str,
    path: Option<&OsStr>,
) -> Option<PathBuf> {
    path.and_then(|path| {
        env::split_paths(path)
            .map(|directory| directory.join(executable))
            .find_map(|candidate| validate_native_executable(&candidate).ok())
    })
}

fn is_native_executable_magic(magic: [u8; 4]) -> bool {
    magic == [0x7f, b'E', b'L', b'F']
        || magic[..2] == *b"MZ"
        || matches!(
            magic,
            [0xfe, 0xed, 0xfa, 0xce | 0xcf]
                | [0xce | 0xcf, 0xfa, 0xed, 0xfe]
                | [0xca, 0xfe, 0xba, 0xbe | 0xbf]
                | [0xbe | 0xbf, 0xba, 0xfe, 0xca]
        )
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    fn write_executable(path: &Path, contents: &str) {
        std::fs::write(path, contents).expect("write script");
        let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod script");
    }

    #[test]
    fn path_resolution_skips_script_shim_for_native_executable() {
        let temp = TempDir::new().expect("tempdir");
        let shim_dir = temp.path().join("shim");
        let native_dir = temp.path().join("native");
        std::fs::create_dir_all(&shim_dir).expect("shim dir");
        std::fs::create_dir_all(&native_dir).expect("native dir");
        write_executable(
            &shim_dir.join("gh"),
            "#!/bin/sh\nprintf 'ghapp wrapper must not run' >&2\nexit 91\n",
        );
        let native = native_dir.join("gh");
        std::os::unix::fs::symlink("/bin/echo", &native).expect("native fixture");
        let path = env::join_paths([shim_dir, native_dir]).expect("PATH fixture");

        let resolved = resolve_native_executable_from_path("gh", Some(&path))
            .expect("later native executable");

        assert_eq!(resolved, native.canonicalize().expect("canonical native"));
    }

    #[test]
    fn validation_rejects_executable_script_wrapper() {
        let temp = TempDir::new().expect("tempdir");
        let wrapper = temp.path().join("codex");
        write_executable(&wrapper, "#!/bin/sh\nexit 0\n");

        assert!(matches!(
            validate_native_executable(&wrapper),
            Err(NativeExecutableError::Wrapper)
        ));
    }
}
