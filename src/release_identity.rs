//! The code-signing identity every Shipyard release binary carries on macOS.
//!
//! macOS privacy consent (TCC) follows a binary's designated requirement. Every
//! release is signed with Developer ID by one team under the identifier
//! `shipyard`, so a new release keeps the grants the old one had. A binary
//! that does not satisfy the requirement, such as a local `cargo build` or an
//! installer's ad-hoc re-sign, is a new identity to macOS and raises a privacy
//! prompt the first time it touches a protected location. Nothing on an
//! unattended host can answer that prompt, so the work behind it stalls.
//!
//! The fleet install transaction refuses to activate such a binary, and doctor
//! and fleet status report the active binary against the same requirement.

use std::path::Path;
use std::process::{Command, Stdio};

/// The Developer ID team that signs every Shipyard release.
pub const RELEASE_TEAM: &str = "95CX6P84C4";

/// The codesign identifier of every Shipyard release binary.
pub const RELEASE_IDENTIFIER: &str = "shipyard";

/// The designated requirement of a Shipyard release: Apple-anchored Developer
/// ID Application certificate, identifier `shipyard`, team [`RELEASE_TEAM`].
/// This is the requirement `codesign -d -r-` prints for a release binary.
pub const RELEASE_REQUIREMENT: &str = "identifier \"shipyard\" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] and certificate leaf[field.1.2.840.113635.100.6.1.13] and certificate leaf[subject.OU] = \"95CX6P84C4\"";

/// The system codesign. Always the absolute path, never a `PATH` lookup.
pub const CODESIGN: &str = "/usr/bin/codesign";

/// The arguments that check `binary` against [`RELEASE_REQUIREMENT`]: a strict
/// signature verification and the requirement in one codesign call.
#[must_use]
pub fn verify_args(binary: &str) -> Vec<String> {
    vec![
        "--verify".to_owned(),
        "--strict".to_owned(),
        format!("-R={RELEASE_REQUIREMENT}"),
        binary.to_owned(),
    ]
}

/// What the check found for one binary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReleaseIdentity {
    /// Signed with Developer ID by the release team as `shipyard`.
    Release,
    /// Present, but the signature or the requirement check failed.
    NotRelease(String),
    /// codesign itself could not be run.
    Unchecked(String),
}

/// Check `binary` with the system codesign.
#[must_use]
pub fn check(binary: &Path) -> ReleaseIdentity {
    check_with(CODESIGN, binary)
}

/// Check `binary` with the given codesign program (tests pass a fake).
#[must_use]
pub fn check_with(codesign: &str, binary: &Path) -> ReleaseIdentity {
    let output = Command::new(codesign)
        .args(verify_args(&binary.display().to_string()))
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => ReleaseIdentity::Release,
        Ok(output) => {
            let detail = String::from_utf8_lossy(&output.stderr)
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("codesign rejected the binary")
                .chars()
                .take(200)
                .collect();
            ReleaseIdentity::NotRelease(detail)
        }
        Err(error) => ReleaseIdentity::Unchecked(format!("{codesign}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirement_names_the_release_identifier_and_team() {
        assert!(RELEASE_REQUIREMENT.starts_with(&format!("identifier \"{RELEASE_IDENTIFIER}\" ")));
        assert!(RELEASE_REQUIREMENT.ends_with(&format!("[subject.OU] = \"{RELEASE_TEAM}\"")));
        assert_eq!(
            verify_args("/x/shipyard"),
            [
                "--verify",
                "--strict",
                &format!("-R={RELEASE_REQUIREMENT}"),
                "/x/shipyard"
            ]
        );
    }

    #[test]
    fn a_fake_codesign_decides_the_verdict() {
        let dir = tempfile::tempdir().expect("dir");
        let binary = dir.path().join("shipyard");
        std::fs::write(&binary, b"x").expect("binary");
        let accept = dir.path().join("accept");
        crate::test_support::write_executable_script_with_mode(
            &accept,
            "#!/bin/sh\ntest \"$1\" = --verify && test \"$2\" = --strict && exit 0\nexit 9\n",
            0o700,
        );
        let refuse = dir.path().join("refuse");
        crate::test_support::write_executable_script_with_mode(
            &refuse,
            "#!/bin/sh\necho 'test-requirement: code failed to satisfy specified code requirement(s)' >&2\nexit 3\n",
            0o700,
        );
        assert_eq!(
            check_with(&accept.display().to_string(), &binary),
            ReleaseIdentity::Release
        );
        assert_eq!(
            check_with(&refuse.display().to_string(), &binary),
            ReleaseIdentity::NotRelease(
                "test-requirement: code failed to satisfy specified code requirement(s)".to_owned()
            )
        );
        assert!(matches!(
            check_with("/nonexistent/codesign", &binary),
            ReleaseIdentity::Unchecked(_)
        ));
    }

    /// The real codesign: an ad-hoc signed binary is not the release.
    #[cfg(target_os = "macos")]
    #[test]
    fn real_codesign_rejects_an_ad_hoc_binary() {
        let dir = tempfile::tempdir().expect("dir");
        let binary = dir.path().join("shipyard");
        std::fs::copy("/usr/bin/true", &binary).expect("copy");
        let signed = Command::new(CODESIGN)
            .args(["--force", "--sign", "-"])
            .arg(&binary)
            .output()
            .expect("codesign");
        assert!(signed.status.success(), "ad-hoc sign: {signed:?}");
        assert!(matches!(check(&binary), ReleaseIdentity::NotRelease(_)));
    }
}
