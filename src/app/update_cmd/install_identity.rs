//! `shipyard update` installs only the Developer ID release on macOS.
//!
//! The update runs the target tag's own install.sh. An installer with the
//! identity check refuses a binary that is not the Developer ID release
//! (identifier, an Apple-anchored Developer ID Application certificate, team
//! 95CX6P84C4) before it ever runs it, and rolls back if the installed copy
//! fails the check after the swap. An older installer has no such check and
//! writes in place, so nothing outside it can undo what it installed. On
//! macOS the update therefore refuses a target whose installer lacks the
//! check, before running any of it. macOS privacy grants follow the release
//! identity; any other binary is a new identity that raises prompts nobody
//! answers on an unattended host.

use crate::app::CliFailure;

/// The repository owner's developer mode for installing a local build. Never
/// set by agents or automation (docs/install.md); the refusal does not name it.
const UNSIGNED_DEV_ENV: &str = "SHIPYARD_INSTALL_ALLOW_UNSIGNED_DEV";

/// The definition every installer with the identity check carries.
const IDENTITY_CHECK_MARKER: &str = "\nrequire_release_identity() {\n";

/// Whether this update may only run an installer that checks the identity:
/// on macOS, outside the owner's developer mode.
pub(super) fn installer_must_check_identity() -> bool {
    cfg!(target_os = "macos") && std::env::var_os(UNSIGNED_DEV_ENV).is_none_or(|v| v != "1")
}

/// Refuse an installer that predates the identity check.
pub(super) fn require_identity_checking_installer(
    installer: &[u8],
    target_tag: &str,
) -> Result<(), CliFailure> {
    let text = String::from_utf8_lossy(installer);
    if text.contains(IDENTITY_CHECK_MARKER) {
        return Ok(());
    }
    Err(CliFailure::new(
        1,
        format!(
            "refused to update to {target_tag}: its installer predates the Developer ID check, \
             so it could install a binary macOS treats as a new privacy identity. Update to a \
             release whose installer checks the identity; the installed Shipyard is unchanged \
             and the installer was not executed."
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_installer_with_the_check_is_accepted() {
        let installer =
            std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh")).expect("install.sh");
        assert!(require_identity_checking_installer(&installer, "v1.0.0").is_ok());
    }

    #[test]
    fn an_installer_without_the_check_is_refused_without_naming_a_way_around() {
        let error = require_identity_checking_installer(
            b"#!/bin/sh\nprepare_macos_binary() {\n}\n",
            "v0.440.0",
        )
        .expect_err("refused");
        let message = &error.message;
        assert!(
            message.contains("refused to update to v0.440.0"),
            "{message}"
        );
        assert!(message.contains("installer was not executed"), "{message}");
        assert!(!message.contains("ALLOW_UNSIGNED"), "{message}");
        assert!(!message.contains("developer mode"), "{message}");
        // A mention of the name is not the definition.
        assert!(
            require_identity_checking_installer(b"# see require_release_identity()\n", "v1")
                .is_err()
        );
    }
}
