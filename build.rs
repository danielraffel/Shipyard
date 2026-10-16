use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    // Watch checkout contents and worktree-specific Git state. Never accept a
    // caller-supplied SHA: metadata identifies the clean tree being compiled.
    println!("cargo:rerun-if-changed=.");
    for name in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git(&root, &["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(reference) = git(&root, &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&root, &["rev-parse", "--git-path", &reference]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let head = git(&root, &["rev-parse", "HEAD"]);
    let clean = git(
        &root,
        &["status", "--porcelain", "--untracked-files=normal"],
    )
    .is_some_and(|status| status.is_empty());
    let source = head
        .filter(|head| clean && head.len() == 40 && head.bytes().all(|b| b.is_ascii_hexdigit()));
    // Explicitly override ambient build variables even when provenance cannot
    // be established (source archives and dirty checkouts remain unavailable).
    println!(
        "cargo:rustc-env=SHIPYARD_SOURCE_HEAD={}",
        source.as_deref().unwrap_or("unavailable")
    );
}
