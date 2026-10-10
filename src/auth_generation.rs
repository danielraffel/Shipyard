//! Which release the installed `ghapp` wrapper comes from.
//!
//! `~/.local/bin/ghapp` is a stable trampoline. It executes the wrapper of the
//! auth *generation* named by the selector symlink
//! `~/.local/bin/ghapp.shipyard-generation`, and each generation carries the
//! release-matched Shipyard binary it was built with. Only
//! `shipyard runner fleet-update` publishes a generation and moves the
//! selector; `shipyard update` replaces the CLI and never touches either. A
//! host updated with `shipyard update` alone therefore keeps running the old
//! wrapper while every version check of the CLI reads current, which is how
//! a released wrapper fix stayed off a host that believed it had it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

/// Selector symlink naming the live generation's wrapper, under `~/.local/bin`.
pub const SELECTOR: &str = "ghapp.shipyard-generation";

/// The directory of the live auth generation, when this host has one.
///
/// Only a selector pointing at `<home>/.local/share/shipyard/auth-generations/
/// <64-hex id>/ghapp` counts; anything else is not a generation this code can
/// speak for.
#[must_use]
pub fn selected_generation(home: &Path) -> Option<PathBuf> {
    let target = std::fs::read_link(home.join(".local/bin").join(SELECTOR)).ok()?;
    let root = home.join(".local/share/shipyard/auth-generations");
    let relative = target.strip_prefix(&root).ok()?;
    let mut parts = relative.components();
    let id = parts.next()?.as_os_str().to_str()?;
    let member = parts.next()?.as_os_str().to_str()?;
    let well_formed = parts.next().is_none()
        && member == "ghapp"
        && id.len() == 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    well_formed.then(|| root.join(id))
}

/// The Shipyard version bundled in `generation`, read from its binary.
#[must_use]
pub fn generation_version(generation: &Path) -> Option<String> {
    let mut child = Command::new(generation.join("shipyard"))
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if child.wait_timeout(Duration::from_secs(10)).ok()?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let output = child.wait_with_output().ok()?;
    parse_version(&String::from_utf8_lossy(&output.stdout))
}

/// `0.223.0` from `shipyard 0.223.0`.
#[must_use]
pub fn parse_version(text: &str) -> Option<String> {
    let version = text.trim().strip_prefix("shipyard ")?.trim();
    (!version.is_empty()).then(|| version.to_owned())
}

/// The command that installs the wrapper generation for `version`.
#[must_use]
pub fn fleet_update_command(version: &str) -> String {
    format!(
        "shipyard runner fleet-update --to v{} --all-hosts --apply",
        version.trim_start_matches('v')
    )
}

/// A warning when the live wrapper generation is from another release than
/// `cli_version`; `None` when they agree.
#[must_use]
pub fn lag_warning(cli_version: &str, generation_version: &str) -> Option<String> {
    let cli = cli_version.trim_start_matches('v');
    let generation = generation_version.trim_start_matches('v');
    (cli != generation).then(|| {
        format!(
            "WARNING: the ghapp wrapper in use is from Shipyard {generation}, but the CLI is {cli}. \
             `shipyard update` does not install the ghapp wrapper; fixes to it are not live until \
             `{}` runs on the controller (review the plan first without --apply).",
            fleet_update_command(cli)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    const ID: &str = "75f602aa0abfc8e8cccca632702bfadf1e1cb1dcd64883ac46c083299b686c28";

    #[cfg(unix)]
    fn home_with_generation(version: &str) -> tempfile::TempDir {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().expect("home");
        let generation = home
            .path()
            .join(".local/share/shipyard/auth-generations")
            .join(ID);
        std::fs::create_dir_all(&generation).expect("generation");
        let binary = generation.join("shipyard");
        crate::test_support::write_executable_script_with_mode(
            &binary,
            &format!("#!/bin/sh\necho 'shipyard {version}'\n"),
            0o700,
        );
        std::fs::create_dir_all(home.path().join(".local/bin")).expect("bin");
        symlink(
            generation.join("ghapp"),
            home.path().join(".local/bin").join(SELECTOR),
        )
        .expect("selector");
        home
    }

    #[cfg(unix)]
    #[test]
    fn the_live_generation_and_its_release_are_read_from_the_selector() {
        let home = home_with_generation("0.217.0");
        let generation = selected_generation(home.path()).expect("selected");
        assert!(generation.ends_with(ID));
        assert_eq!(generation_version(&generation).as_deref(), Some("0.217.0"));
    }

    #[cfg(unix)]
    #[test]
    fn a_selector_outside_the_generation_store_names_nothing() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".local/bin")).expect("bin");
        symlink(
            "/tmp/elsewhere/ghapp",
            home.path().join(".local/bin").join(SELECTOR),
        )
        .expect("selector");
        assert_eq!(selected_generation(home.path()), None);

        let short = tempfile::tempdir().expect("short id");
        std::fs::create_dir_all(short.path().join(".local/bin")).expect("bin");
        symlink(
            short
                .path()
                .join(".local/share/shipyard/auth-generations/abc/ghapp"),
            short.path().join(".local/bin").join(SELECTOR),
        )
        .expect("selector");
        assert_eq!(
            selected_generation(short.path()),
            None,
            "not a generation id"
        );

        let empty = tempfile::tempdir().expect("empty");
        assert_eq!(selected_generation(empty.path()), None);
    }

    #[test]
    fn a_lagging_generation_warns_with_the_exact_install_command() {
        let warning = lag_warning("v0.224.0", "0.217.0").expect("lagging");
        assert!(warning.contains("Shipyard 0.217.0"), "{warning}");
        assert!(warning.contains("the CLI is 0.224.0"), "{warning}");
        assert!(
            warning.contains("`shipyard runner fleet-update --to v0.224.0 --all-hosts --apply`"),
            "{warning}"
        );
        assert!(
            warning.contains("does not install the ghapp wrapper"),
            "{warning}"
        );
        assert_eq!(lag_warning("v0.224.0", "0.224.0"), None);
        assert_eq!(
            parse_version("shipyard 0.224.0\n").as_deref(),
            Some("0.224.0")
        );
        assert_eq!(parse_version("ghapp: nope"), None);
    }
}
