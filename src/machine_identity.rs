//! Stable per-install machine identity used by multi-host state.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const MACHINE_ID_FILE: &str = "machine-id";
const MACHINE_ID_PREFIX: &str = "sy_node_";

/// Return the stable machine-id path for a Shipyard state directory.
#[must_use]
pub fn machine_id_path(state_dir: &Path) -> PathBuf {
    state_dir.join(MACHINE_ID_FILE)
}

/// Load or create the stable machine id for this Shipyard install.
pub fn get_or_create_machine_id(state_dir: &Path) -> io::Result<String> {
    if let Some(existing) = existing_machine_id(state_dir)? {
        return Ok(existing);
    }
    let machine_id = generate_machine_id()?;
    let path = machine_id_path(state_dir);
    write_machine_id(&path, &machine_id)?;
    Ok(machine_id)
}

/// Load the existing stable machine id without creating one.
pub fn existing_machine_id(state_dir: &Path) -> io::Result<Option<String>> {
    read_machine_id(&machine_id_path(state_dir))
}

fn read_machine_id(path: &Path) -> io::Result<Option<String>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let trimmed = raw.trim();
    if is_valid_machine_id(trimmed) {
        Ok(Some(trimmed.to_owned()))
    } else {
        Ok(None)
    }
}

fn write_machine_id(path: &Path, machine_id: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, format!("{machine_id}\n"))?;
    fs::rename(tmp, path)
}

fn generate_machine_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    crate::random::fill_bytes(&mut bytes)?;
    Ok(format!("{MACHINE_ID_PREFIX}{}", hex::encode(bytes)))
}

fn is_valid_machine_id(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix(MACHINE_ID_PREFIX) else {
        return false;
    };
    suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::{get_or_create_machine_id, machine_id_path};

    #[test]
    fn machine_id_is_stable_and_persisted() {
        let temp = tempfile::tempdir().expect("tempdir");

        let first = get_or_create_machine_id(temp.path()).expect("first");
        let second = get_or_create_machine_id(temp.path()).expect("second");

        assert_eq!(first, second);
        assert!(first.starts_with("sy_node_"));
        assert_eq!(first.len(), "sy_node_".len() + 32);
        assert_eq!(
            std::fs::read_to_string(machine_id_path(temp.path()))
                .expect("machine-id")
                .trim(),
            first
        );
    }

    #[test]
    fn invalid_machine_id_file_is_replaced() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = machine_id_path(temp.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        std::fs::write(&path, "not-valid\n").expect("write");

        let machine_id = get_or_create_machine_id(temp.path()).expect("machine id");

        assert!(machine_id.starts_with("sy_node_"));
        assert_eq!(
            std::fs::read_to_string(path).expect("machine-id").trim(),
            machine_id
        );
    }
}
