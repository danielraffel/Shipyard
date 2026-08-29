//! Transactional installation of release-bound GitHub App support scripts.

use std::path::Path;

use super::release_authority::ReleaseAuthority;
use crate::executor::ssh::shlex_quote;

pub(super) const BEFORE_HELPER_SHA_PREFIX: &str = "SHIPYARD_FLEET_BEFORE_AUTH_HELPER_SHA256=";
pub(super) const BEFORE_HELPER_MODE_PREFIX: &str = "SHIPYARD_FLEET_BEFORE_AUTH_HELPER_MODE=";
pub(super) const BEFORE_WRAPPER_SHA_PREFIX: &str = "SHIPYARD_FLEET_BEFORE_AUTH_WRAPPER_SHA256=";
pub(super) const BEFORE_WRAPPER_MODE_PREFIX: &str = "SHIPYARD_FLEET_BEFORE_AUTH_WRAPPER_MODE=";
pub(super) const AFTER_HELPER_SHA_PREFIX: &str = "SHIPYARD_FLEET_AFTER_AUTH_HELPER_SHA256=";
pub(super) const AFTER_HELPER_MODE_PREFIX: &str = "SHIPYARD_FLEET_AFTER_AUTH_HELPER_MODE=";
pub(super) const AFTER_WRAPPER_SHA_PREFIX: &str = "SHIPYARD_FLEET_AFTER_AUTH_WRAPPER_SHA256=";
pub(super) const AFTER_WRAPPER_MODE_PREFIX: &str = "SHIPYARD_FLEET_AFTER_AUTH_WRAPPER_MODE=";
const WRAPPER_DEFAULT_HELPER: &str = ".config/shipyard/bin/shipyard-github-app-token";

pub(super) fn source_urls(authority: &ReleaseAuthority) -> (String, String) {
    let base = format!(
        "https://raw.githubusercontent.com/{}/{}",
        authority.repository, authority.commit_oid
    );
    (
        format!("{base}/{}", authority.auth_helper.path),
        format!("{base}/{}", authority.auth_wrapper.path),
    )
}

/// The frozen wrapper intentionally has one environment-independent helper
/// location. Fleet rollout preserves GitHub auth and daemon configuration, so
/// it must refuse a path that the installed wrapper would not subsequently use.
pub(super) fn wrapper_helper_contract_probe(helper: &Path) -> String {
    let helper = shlex_quote(&helper.display().to_string());
    format!(
        "test {helper} = \"$HOME/{WRAPPER_DEFAULT_HELPER}\"; case \"${{SHIPYARD_GITHUB_APP_TOKEN_HELPER:-}}\" in ''|{helper}) ;; *) exit 1 ;; esac"
    )
}

pub(super) fn probe(helper: &Path, wrapper: &Path, phase: &str) -> String {
    let helper = shlex_quote(&helper.display().to_string());
    let wrapper = shlex_quote(&wrapper.display().to_string());
    format!(
        "if [ -e {helper} ] || [ -L {helper} ]; then test -f {helper}; test ! -L {helper}; {phase}_auth_helper_sha256=\"$(/usr/bin/shasum -a 256 {helper} | /usr/bin/awk '{{print $1}}')\"; {phase}_auth_helper_mode=\"$(/usr/bin/stat -f '%Lp' {helper})\"; else {phase}_auth_helper_sha256=absent; {phase}_auth_helper_mode=absent; fi\n\
         if [ -e {wrapper} ] || [ -L {wrapper} ]; then test -f {wrapper}; test ! -L {wrapper}; {phase}_auth_wrapper_sha256=\"$(/usr/bin/shasum -a 256 {wrapper} | /usr/bin/awk '{{print $1}}')\"; {phase}_auth_wrapper_mode=\"$(/usr/bin/stat -f '%Lp' {wrapper})\"; else {phase}_auth_wrapper_sha256=absent; {phase}_auth_wrapper_mode=absent; fi"
    )
}

/// Generate the macOS transaction. Both source files must already exist in a
/// private staging directory and have been checked against the frozen release
/// authority. The journal makes the two-rename transaction recoverable after
/// abrupt process death; ordinary errors roll back before returning.
#[allow(clippy::too_many_arguments)]
pub(super) fn install_transaction(
    helper: &Path,
    wrapper: &Path,
    binary: &Path,
    companion: &Path,
    companion_required: bool,
    helper_source: &str,
    wrapper_source: &str,
    binary_install_command: &str,
    state_dir: &Path,
    authority: &ReleaseAuthority,
    fail_after_helper_for_test: bool,
) -> String {
    let helper = shlex_quote(&helper.display().to_string());
    let wrapper = shlex_quote(&wrapper.display().to_string());
    let binary = shlex_quote(&binary.display().to_string());
    let companion = shlex_quote(&companion.display().to_string());
    let companion_required = if companion_required { "1" } else { "0" };
    let state_dir = shlex_quote(&state_dir.display().to_string());
    let authority_id = shlex_quote(&authority.identity_sha256);
    let helper_digest = shlex_quote(&authority.auth_helper.sha256);
    let wrapper_digest = shlex_quote(&authority.auth_wrapper.sha256);
    // Source arguments are internally generated shell expressions, never
    // configuration or user input. External values are quoted before here.
    let injected_failure = if fail_after_helper_for_test {
        "/usr/bin/false"
    } else {
        ":"
    };
    format!(
        r#"
auth_helper={helper}
auth_wrapper={wrapper}
auth_binary={binary}
auth_companion={companion}
auth_companion_required={companion_required}
auth_state_dir={state_dir}
auth_authority={authority_id}
auth_helper_digest={helper_digest}
auth_wrapper_digest={wrapper_digest}
auth_helper_source={helper_source}
auth_wrapper_source={wrapper_source}
auth_journal="$auth_state_dir/fleet-auth-support.transaction"
auth_lock="$auth_state_dir/fleet-auth-support.lock"

auth_safe_target() {{
  auth_target="$1"
  case "$auth_target" in "$HOME"/*) ;; *) return 1 ;; esac
  auth_parent="$(/usr/bin/dirname "$auth_target")"
  auth_cursor="$auth_parent"
  while [ "$auth_cursor" != "$HOME" ]; do
    test -d "$auth_cursor"
    test ! -L "$auth_cursor"
    test "$(/usr/bin/stat -f '%u' "$auth_cursor")" = "$(/usr/bin/id -u)"
    auth_mode="$(/usr/bin/stat -f '%Lp' "$auth_cursor")"
    test $((8#$auth_mode & 8#22)) -eq 0
    auth_next="$(/usr/bin/dirname "$auth_cursor")"
    test "$auth_next" != "$auth_cursor"
    auth_cursor="$auth_next"
  done
  test -d "$HOME"
  test ! -L "$HOME"
  test "$(/usr/bin/stat -f '%u' "$HOME")" = "$(/usr/bin/id -u)"
  if [ -e "$auth_target" ] || [ -L "$auth_target" ]; then
    test -f "$auth_target"
    test ! -L "$auth_target"
    test "$(/usr/bin/stat -f '%u' "$auth_target")" = "$(/usr/bin/id -u)"
  fi
}}

auth_write_phase() {{
  auth_phase_tmp="$(/usr/bin/mktemp "$auth_state_dir/.fleet-auth-support.phase.XXXXXX")"
  /usr/bin/printf '%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n' \
    "$1" "$auth_authority" "$auth_helper" "$auth_wrapper" "$auth_binary" \
    "$auth_companion" "$auth_helper_digest" "$auth_wrapper_digest" \
    "$auth_companion_required" > "$auth_phase_tmp"
  /bin/chmod 600 "$auth_phase_tmp"
  /bin/mv -f "$auth_phase_tmp" "$auth_journal"
}}

auth_cleanup_markers() {{
  auth_cleanup_helper="$1"
  auth_cleanup_wrapper="$2"
  auth_cleanup_binary="$3"
  auth_cleanup_companion="$4"
  /bin/rm -f "$auth_cleanup_helper.shipyard-rollback" "$auth_cleanup_helper.shipyard-was-absent"
  /bin/rm -f "$auth_cleanup_wrapper.shipyard-rollback" "$auth_cleanup_wrapper.shipyard-was-absent"
  /bin/rm -f "$auth_cleanup_binary.shipyard-rollback" "$auth_cleanup_binary.shipyard-was-absent"
  /bin/rm -f "$auth_cleanup_companion.shipyard-rollback" "$auth_cleanup_companion.shipyard-was-absent"
  /bin/rm -f "$auth_journal"
}}

auth_restore_one() {{
  auth_target="$1"
  if [ -f "$auth_target.shipyard-rollback" ] && [ ! -L "$auth_target.shipyard-rollback" ]; then
    /bin/rm -f "$auth_target"
    /bin/mv "$auth_target.shipyard-rollback" "$auth_target"
  elif [ -f "$auth_target.shipyard-was-absent" ] && [ ! -L "$auth_target.shipyard-was-absent" ]; then
    /bin/rm -f "$auth_target"
  fi
}}

auth_recover() {{
  test -f "$auth_journal"
  test ! -L "$auth_journal"
  test "$(/usr/bin/wc -l < "$auth_journal" | /usr/bin/tr -d ' ')" = 9
  auth_phase="$(/usr/bin/sed -n '1p' "$auth_journal")"
  auth_recovery_authority="$(/usr/bin/sed -n '2p' "$auth_journal")"
  auth_recovery_helper="$(/usr/bin/sed -n '3p' "$auth_journal")"
  auth_recovery_wrapper="$(/usr/bin/sed -n '4p' "$auth_journal")"
  auth_recovery_binary="$(/usr/bin/sed -n '5p' "$auth_journal")"
  auth_recovery_companion="$(/usr/bin/sed -n '6p' "$auth_journal")"
  auth_recovery_helper_digest="$(/usr/bin/sed -n '7p' "$auth_journal")"
  auth_recovery_wrapper_digest="$(/usr/bin/sed -n '8p' "$auth_journal")"
  auth_recovery_companion_required="$(/usr/bin/sed -n '9p' "$auth_journal")"
  test -n "$auth_recovery_authority"
  test "$auth_recovery_helper" != "$auth_recovery_wrapper"
  auth_safe_target "$auth_recovery_helper"
  auth_safe_target "$auth_recovery_wrapper"
  auth_safe_target "$auth_recovery_binary"
  auth_safe_target "$auth_recovery_companion"
  case "$auth_recovery_companion_required" in 0|1) ;; *) return 1 ;; esac
  if [ "$auth_phase" = committed ]; then
    test "$(/usr/bin/shasum -a 256 "$auth_recovery_helper" | /usr/bin/awk '{{print $1}}')" = "$auth_recovery_helper_digest"
    test "$(/usr/bin/shasum -a 256 "$auth_recovery_wrapper" | /usr/bin/awk '{{print $1}}')" = "$auth_recovery_wrapper_digest"
    test -x "$auth_recovery_binary"
    if [ "$auth_recovery_companion_required" = 1 ]; then test -x "$auth_recovery_companion"; fi
  else
    case "$auth_phase" in preparing|prepared|helper-installed|auth-installed) ;; *) return 1 ;; esac
    auth_restore_one "$auth_recovery_companion"
    auth_restore_one "$auth_recovery_binary"
    auth_restore_one "$auth_recovery_wrapper"
    auth_restore_one "$auth_recovery_helper"
  fi
  auth_cleanup_markers "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" "$auth_recovery_companion"
}}

test -d "$auth_state_dir"
test ! -L "$auth_state_dir"
test "$(/usr/bin/stat -f '%u' "$auth_state_dir")" = "$(/usr/bin/id -u)"
auth_state_mode="$(/usr/bin/stat -f '%Lp' "$auth_state_dir")"
test $((8#$auth_state_mode & 8#22)) -eq 0
if ! /bin/mkdir "$auth_lock" 2>/dev/null; then
  test -d "$auth_lock"
  test ! -L "$auth_lock"
  test "$(/usr/bin/stat -f '%u' "$auth_lock")" = "$(/usr/bin/id -u)"
  if [ -e "$auth_lock/pid" ] || [ -L "$auth_lock/pid" ]; then
    test -f "$auth_lock/pid"
    test ! -L "$auth_lock/pid"
    auth_lock_pid="$(/bin/cat "$auth_lock/pid")"
    case "$auth_lock_pid" in ''|*[!0-9]*) exit 1 ;; esac
    if /bin/kill -0 "$auth_lock_pid" 2>/dev/null; then exit 1; fi
    /bin/rm "$auth_lock/pid"
  fi
  /bin/rmdir "$auth_lock"
  /bin/mkdir "$auth_lock"
fi
if ! /usr/bin/printf '%s\n' "$$" > "$auth_lock/pid"; then
  /bin/rmdir "$auth_lock"
  exit 1
fi
if ! /bin/chmod 600 "$auth_lock/pid"; then
  /bin/rm -f "$auth_lock/pid"
  /bin/rmdir "$auth_lock"
  exit 1
fi
auth_helper_tmp=
auth_wrapper_tmp=
auth_release_lock() {{ /bin/rm -f "$auth_lock/pid"; /bin/rmdir "$auth_lock"; }}
auth_release_on_error() {{ auth_status=$?; trap - ERR INT TERM; if [ -n "$auth_helper_tmp" ]; then /bin/rm -f "$auth_helper_tmp"; fi; if [ -n "$auth_wrapper_tmp" ]; then /bin/rm -f "$auth_wrapper_tmp"; fi; auth_release_lock; exit "$auth_status"; }}
trap auth_release_on_error ERR INT TERM

auth_safe_target "$auth_helper"
auth_safe_target "$auth_wrapper"
auth_safe_target "$auth_binary"
auth_safe_target "$auth_companion"
auth_recovery_needed=0
if [ -e "$auth_journal" ] || [ -L "$auth_journal" ]; then auth_recovery_needed=1; fi
case "$auth_recovery_needed" in 1) auth_recover ;; esac
for auth_target in "$auth_helper" "$auth_wrapper" "$auth_binary" "$auth_companion"; do
  test ! -e "$auth_target.shipyard-rollback"
  test ! -L "$auth_target.shipyard-rollback"
  test ! -e "$auth_target.shipyard-was-absent"
  test ! -L "$auth_target.shipyard-was-absent"
done

auth_helper_tmp="$(/usr/bin/mktemp "$(/usr/bin/dirname "$auth_helper")/.shipyard-auth-helper.XXXXXX")"
auth_wrapper_tmp="$(/usr/bin/mktemp "$(/usr/bin/dirname "$auth_wrapper")/.shipyard-auth-wrapper.XXXXXX")"
/bin/cp "$auth_helper_source" "$auth_helper_tmp"
/bin/cp "$auth_wrapper_source" "$auth_wrapper_tmp"
/bin/chmod 700 "$auth_helper_tmp" "$auth_wrapper_tmp"
test "$(/usr/bin/shasum -a 256 "$auth_helper_tmp" | /usr/bin/awk '{{print $1}}')" = "$auth_helper_digest"
test "$(/usr/bin/shasum -a 256 "$auth_wrapper_tmp" | /usr/bin/awk '{{print $1}}')" = "$auth_wrapper_digest"
auth_write_phase preparing

auth_rollback_on_error() {{
  auth_status=$?
  trap - ERR INT TERM
  auth_restore_one "$auth_wrapper"
  auth_restore_one "$auth_helper"
  auth_restore_one "$auth_companion"
  auth_restore_one "$auth_binary"
  auth_cleanup_markers "$auth_helper" "$auth_wrapper" "$auth_binary" "$auth_companion"
  /bin/rm -f "$auth_helper_tmp" "$auth_wrapper_tmp"
  auth_release_lock
  exit "$auth_status"
}}
trap auth_rollback_on_error ERR INT TERM

if [ -e "$auth_helper" ]; then /bin/mv "$auth_helper" "$auth_helper.shipyard-rollback"; else /usr/bin/touch "$auth_helper.shipyard-was-absent"; /bin/chmod 600 "$auth_helper.shipyard-was-absent"; fi
if [ -e "$auth_wrapper" ]; then /bin/mv "$auth_wrapper" "$auth_wrapper.shipyard-rollback"; else /usr/bin/touch "$auth_wrapper.shipyard-was-absent"; /bin/chmod 600 "$auth_wrapper.shipyard-was-absent"; fi
if [ -e "$auth_binary" ]; then /bin/cp -p "$auth_binary" "$auth_binary.shipyard-rollback"; else /usr/bin/touch "$auth_binary.shipyard-was-absent"; /bin/chmod 600 "$auth_binary.shipyard-was-absent"; fi
if [ -e "$auth_companion" ]; then /bin/cp -p "$auth_companion" "$auth_companion.shipyard-rollback"; else /usr/bin/touch "$auth_companion.shipyard-was-absent"; /bin/chmod 600 "$auth_companion.shipyard-was-absent"; fi
auth_write_phase prepared
/bin/mv "$auth_helper_tmp" "$auth_helper"
auth_write_phase helper-installed
{injected_failure}
/bin/mv "$auth_wrapper_tmp" "$auth_wrapper"
auth_write_phase auth-installed
{binary_install_command}
test "$(/usr/bin/shasum -a 256 "$auth_helper" | /usr/bin/awk '{{print $1}}')" = "$auth_helper_digest"
test "$(/usr/bin/shasum -a 256 "$auth_wrapper" | /usr/bin/awk '{{print $1}}')" = "$auth_wrapper_digest"
test "$(/usr/bin/stat -f '%Lp' "$auth_helper")" = 700
test "$(/usr/bin/stat -f '%Lp' "$auth_wrapper")" = 700
test -x "$auth_binary"
if [ "$auth_companion_required" = 1 ]; then test -x "$auth_companion"; fi
auth_write_phase committed
trap - ERR INT TERM
auth_cleanup_markers "$auth_helper" "$auth_wrapper" "$auth_binary" "$auth_companion"
auth_release_lock
"#
    )
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Command;

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::app::fleet_update_cmd::test_release_authority;

    fn digest(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn fixture() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        ReleaseAuthority,
    ) {
        let root = tempfile::tempdir().expect("root");
        let bin = root.path().join(".local/bin");
        let helper_dir = root.path().join(".config/shipyard/bin");
        let state = root.path().join("Library/Application Support/shipyard");
        std::fs::create_dir_all(&bin).expect("bin");
        std::fs::create_dir_all(&helper_dir).expect("helper dir");
        std::fs::create_dir_all(&state).expect("state");
        let helper = helper_dir.join("shipyard-github-app-token");
        let wrapper = bin.join("ghapp");
        let helper_source = root.path().join("new-helper");
        let wrapper_source = root.path().join("new-wrapper");
        std::fs::write(&helper_source, b"new helper\n").expect("helper source");
        std::fs::write(&wrapper_source, b"new wrapper\n").expect("wrapper source");
        let mut authority = test_release_authority("v0.127.0");
        authority.auth_helper.sha256 = digest(b"new helper\n");
        authority.auth_wrapper.sha256 = digest(b"new wrapper\n");
        (
            root,
            helper,
            wrapper,
            helper_source,
            wrapper_source,
            authority,
        )
    }

    fn run(
        root: &tempfile::TempDir,
        helper: &Path,
        wrapper: &Path,
        helper_source: &Path,
        wrapper_source: &Path,
        authority: &ReleaseAuthority,
        fail_after_helper: bool,
    ) -> std::process::ExitStatus {
        let state = root.path().join("Library/Application Support/shipyard");
        let binary = root.path().join(".local/bin/shipyard");
        let companion = root.path().join(".local/bin/shipyard-workstream-provider");
        for path in [&binary, &companion] {
            if !path.exists() {
                std::fs::write(path, b"#!/bin/sh\nexit 0\n").expect("binary fixture");
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                    .expect("binary mode");
            }
        }
        let script = install_transaction(
            helper,
            wrapper,
            &binary,
            &companion,
            true,
            &shlex_quote(&helper_source.display().to_string()),
            &shlex_quote(&wrapper_source.display().to_string()),
            ":",
            &state,
            authority,
            fail_after_helper,
        );
        Command::new("/bin/bash")
            .args(["-c", &format!("set -Eeuo pipefail\n{script}")])
            .env("HOME", root.path())
            .status()
            .expect("transaction")
    }

    #[test]
    fn legacy_pair_is_migrated_helper_first_to_exact_private_files() {
        let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
        std::fs::write(&helper, b"legacy fixed installation helper\n").expect("old helper");
        std::fs::write(&wrapper, b"legacy wrapper\n").expect("old wrapper");
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).expect("mode");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("mode");

        assert!(
            run(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false
            )
            .success()
        );
        assert_eq!(std::fs::read(&helper).expect("helper"), b"new helper\n");
        assert_eq!(std::fs::read(&wrapper).expect("wrapper"), b"new wrapper\n");
        assert_eq!(
            std::fs::metadata(&helper)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&wrapper)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(
            !root
                .path()
                .join("Library/Application Support/shipyard/fleet-auth-support.transaction")
                .exists()
        );
    }

    #[test]
    fn partial_install_failure_rolls_back_both_legacy_files() {
        let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
        std::fs::write(&helper, b"old helper\n").expect("old helper");
        std::fs::write(&wrapper, b"old wrapper\n").expect("old wrapper");
        assert!(
            !run(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                true
            )
            .success()
        );
        assert_eq!(std::fs::read(&helper).expect("helper"), b"old helper\n");
        assert_eq!(std::fs::read(&wrapper).expect("wrapper"), b"old wrapper\n");
    }

    #[test]
    fn next_release_recovers_an_interrupted_prior_release_before_installing() {
        let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
        let state = root.path().join("Library/Application Support/shipyard");
        let binary = root.path().join(".local/bin/shipyard");
        let companion = root.path().join(".local/bin/shipyard-workstream-provider");
        std::fs::write(helper.with_extension("shipyard-rollback"), b"old helper\n")
            .expect("helper rollback");
        std::fs::write(
            wrapper.with_extension("shipyard-rollback"),
            b"old wrapper\n",
        )
        .expect("wrapper rollback");
        std::fs::write(&helper, b"interrupted prior-release helper\n").expect("interrupted helper");
        for (path, current, old) in [
            (
                &binary,
                b"interrupted binary\n".as_slice(),
                b"old binary\n".as_slice(),
            ),
            (
                &companion,
                b"interrupted companion\n".as_slice(),
                b"old companion\n".as_slice(),
            ),
        ] {
            std::fs::write(path, current).expect("interrupted binary pair");
            let rollback = path.with_extension("shipyard-rollback");
            std::fs::write(&rollback, old).expect("binary rollback");
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .expect("binary mode");
            std::fs::set_permissions(&rollback, std::fs::Permissions::from_mode(0o700))
                .expect("rollback mode");
        }
        let journal = state.join("fleet-auth-support.transaction");
        std::fs::write(
            &journal,
            format!(
                "auth-installed\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n1\n",
                "f".repeat(64),
                helper.display(),
                wrapper.display(),
                binary.display(),
                companion.display(),
                digest(b"interrupted prior-release helper\n"),
                digest(b"interrupted prior-release wrapper\n")
            ),
        )
        .expect("prior journal");
        let lock = state.join("fleet-auth-support.lock");
        std::fs::create_dir(&lock).expect("stale lock");
        std::fs::write(lock.join("pid"), b"99999999\n").expect("stale pid");

        assert!(
            !run(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                true
            )
            .success()
        );
        assert_eq!(std::fs::read(&helper).expect("helper"), b"old helper\n");
        assert_eq!(std::fs::read(&wrapper).expect("wrapper"), b"old wrapper\n");
        assert_eq!(std::fs::read(&binary).expect("binary"), b"old binary\n");
        assert_eq!(
            std::fs::read(&companion).expect("companion"),
            b"old companion\n"
        );
        assert!(!journal.exists());

        assert!(
            run(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false
            )
            .success()
        );
        assert_eq!(std::fs::read(&helper).expect("helper"), b"new helper\n");
        assert_eq!(std::fs::read(&wrapper).expect("wrapper"), b"new wrapper\n");
    }

    #[test]
    fn tampered_source_and_symlink_target_fail_before_mutation() {
        let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
        std::fs::write(&helper_source, b"tampered\n").expect("tamper");
        assert!(
            !run(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false
            )
            .success()
        );
        assert!(!helper.exists());
        assert!(!wrapper.exists());

        std::fs::write(&helper_source, b"new helper\n").expect("restore source");
        let real = root.path().join("real-helper");
        std::fs::write(&real, b"legacy\n").expect("real");
        symlink(&real, &helper).expect("symlink");
        assert!(
            !run(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false
            )
            .success()
        );
        assert_eq!(std::fs::read_link(&helper).expect("link"), real);
    }
}
