//! Helpers shared by the integration-test crates.
//!
//! Integration tests cannot reach the library's `crate::test_support`, so the
//! one helper they need is repeated here with the same contract: see
//! `test_support::write_executable_script` in `src/lib.rs` for why a script
//! written by this process and then executed races the rest of the suite on
//! Linux (`ETXTBSY`).

#![allow(dead_code)]

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// Write `contents` to `path` with `mode` from a child process, so no thread
/// of this test process ever holds a writable descriptor on the file, and
/// return once it is exec-ready.
#[cfg(unix)]
pub fn write_executable_with_mode(path: &Path, contents: &[u8], mode: u32) {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"cat > "$1" && chmod "$2" "$1""#)
        .arg("shipyard-write-executable")
        .arg(path)
        .arg(format!("{mode:o}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn writer for {}: {error}", path.display()));
    let mut stdin = child
        .stdin
        .take()
        .unwrap_or_else(|| panic!("writer stdin for {}", path.display()));
    let written = stdin.write_all(contents);
    drop(stdin);
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("wait for writer of {}: {error}", path.display()));
    assert!(
        output.status.success(),
        "writing {} failed: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    written.unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

/// [`write_executable_with_mode`] with mode `0o755`.
#[cfg(unix)]
pub fn write_executable(path: &Path, contents: &str) {
    write_executable_with_mode(path, contents.as_bytes(), 0o755);
}
