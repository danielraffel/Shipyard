//! Bind producer metadata to the clean Git source used for this build.

use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command.args(args).current_dir(root);
    // A caller's Git context must not redirect the provenance lookup.
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    let output = command.output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

// Cargo treats a missing watched path as changed on every invocation. Watch
// its nearest existing ancestor so creation is detected without forced rebuilds.
fn watch_git_path(root: &Path, path: &str) {
    let mut watched = root.join(path);
    while !watched.exists() {
        if !watched.pop() {
            return;
        }
    }
    println!("cargo:rerun-if-changed={}", watched.display());
}

fn main() {
    let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    // Watch source inputs and Git state, never target/ or the package root:
    // compiler output must not cause perpetual build-script invalidation.
    for path in [
        "src",
        "tests",
        "examples",
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    if let Some(files) = git(&root, &["ls-files"]) {
        for path in files.lines() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    for name in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git(&root, &["rev-parse", "--git-path", name]) {
            watch_git_path(&root, &path);
        }
    }
    if let Some(reference) = git(&root, &["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(&root, &["rev-parse", "--git-path", &reference])
    {
        watch_git_path(&root, &path);
    }
    // An archive nested inside another checkout must not inherit its identity.
    let own_checkout = git(&root, &["rev-parse", "--show-toplevel"])
        .and_then(|path| std::fs::canonicalize(path).ok())
        .zip(std::fs::canonicalize(&root).ok())
        .is_some_and(|(top, manifest)| top == manifest);
    let head = git(&root, &["rev-parse", "HEAD"]);
    let clean = git(
        &root,
        // Status is read during concurrent builds; never take Git's optional
        // index lock just to decide whether provenance can be authenticated.
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=normal",
        ],
    )
    .is_some_and(|status| status.is_empty());
    // Ignored Rust inputs can alter compilation without appearing in status.
    let ignored_inputs = git(
        &root,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--",
            "src",
            "tests",
            "examples",
        ],
    )
    .is_some_and(|files| !files.is_empty());
    let source = head.filter(|head| {
        own_checkout
            && clean
            && !ignored_inputs
            && head.len() == 40
            && head.bytes().all(|b| b.is_ascii_hexdigit())
    });
    // This identifies checkout source, not dependency/toolchain authenticity.
    // Override ambient assertions; archives and dirty source are unavailable.
    println!(
        "cargo:rustc-env=SHIPYARD_SOURCE_HEAD={}",
        source.as_deref().unwrap_or("unavailable")
    );
}
