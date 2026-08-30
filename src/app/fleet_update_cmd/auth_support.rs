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
/// authority. The journal makes the four- or five-target transaction
/// recoverable after abrupt process death; ordinary errors roll back before
/// returning.
#[allow(clippy::too_many_arguments)]
pub(super) fn install_transaction(
    helper: &Path,
    wrapper: &Path,
    binary: &Path,
    companion: &Path,
    companion_required: bool,
    resolver_required: bool,
    helper_source: &str,
    wrapper_source: &str,
    binary_install_command: &str,
    post_commit_command: &str,
    mode: &str,
    global_dir: &Path,
    state_dir: &Path,
    probe_repo: &str,
    authority: &ReleaseAuthority,
    fail_after_helper_for_test: bool,
) -> String {
    let helper = shlex_quote(&helper.display().to_string());
    let wrapper = shlex_quote(&wrapper.display().to_string());
    let binary = shlex_quote(&binary.display().to_string());
    let companion = shlex_quote(&companion.display().to_string());
    let companion_required = if companion_required { "1" } else { "0" };
    let resolver_required = if resolver_required { "1" } else { "0" };
    let context_json = shlex_quote(
        &serde_json::json!({
            "schema_version": 1,
            "mode": mode,
            "global_dir": global_dir.display().to_string(),
        })
        .to_string(),
    );
    let mode = shlex_quote(mode);
    let global_dir = shlex_quote(&global_dir.display().to_string());
    let state_dir = shlex_quote(&state_dir.display().to_string());
    let probe_repo = shlex_quote(probe_repo);
    let authority_id = shlex_quote(&authority.identity_sha256);
    let helper_digest = shlex_quote(&authority.auth_helper.sha256);
    let wrapper_digest = shlex_quote(&authority.auth_wrapper.sha256);
    let post_commit_command = if post_commit_command.trim().is_empty() {
        ":"
    } else {
        post_commit_command
    };
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
auth_resolver_required={resolver_required}
if [ "$auth_resolver_required" = 1 ]; then auth_context="$auth_wrapper.shipyard-context.json"; else auth_context=; fi
auth_mode={mode}
auth_global_dir={global_dir}
auth_state_dir={state_dir}
auth_probe_repo={probe_repo}
auth_context_json={context_json}
auth_authority={authority_id}
auth_helper_digest={helper_digest}
auth_wrapper_digest={wrapper_digest}
auth_helper_source={helper_source}
auth_wrapper_source={wrapper_source}
auth_journal="$auth_state_dir/fleet-auth-support.transaction"
auth_lock="$auth_state_dir/fleet-auth-support.lock"
auth_guard="$auth_state_dir/fleet-auth-support.guard"

auth_safe_target() {{
  auth_target="$1"
  case "$auth_target" in "$HOME"/*) ;; *) return 1 ;; esac
  auth_parent="$(/usr/bin/dirname "$auth_target")"
  auth_cursor="$auth_parent"
  while [ "$auth_cursor" != "$HOME" ]; do
    test -d "$auth_cursor"
    test ! -L "$auth_cursor"
    test "$(/usr/bin/stat -f '%u' "$auth_cursor")" = "$(/usr/bin/id -u)"
    auth_permissions="$(/usr/bin/stat -f '%Lp' "$auth_cursor")"
    test $((8#$auth_permissions & 8#22)) -eq 0
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
  if [ "$auth_resolver_required" = 1 ]; then
    /usr/bin/printf '%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n' \
      "$1" "$auth_authority" "$auth_helper" "$auth_wrapper" "$auth_binary" \
      "$auth_companion" "$auth_context" "$auth_helper_digest" "$auth_wrapper_digest" \
      "$auth_context_digest" "$auth_companion_required" > "$auth_phase_tmp"
  else
    /usr/bin/printf '%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n' \
      "$1" "$auth_authority" "$auth_helper" "$auth_wrapper" "$auth_binary" \
      "$auth_companion" "$auth_helper_digest" "$auth_wrapper_digest" \
      "$auth_companion_required" > "$auth_phase_tmp"
  fi
  /bin/chmod 600 "$auth_phase_tmp"
  /bin/mv -f "$auth_phase_tmp" "$auth_journal"
}}

auth_cleanup_markers() {{
  auth_cleanup_helper="$1"
  auth_cleanup_wrapper="$2"
  auth_cleanup_binary="$3"
  auth_cleanup_companion="$4"
  auth_cleanup_context="$5"
  /bin/rm -f "$auth_cleanup_helper.shipyard-rollback" "$auth_cleanup_helper.shipyard-was-absent"
  /bin/rm -f "$auth_cleanup_wrapper.shipyard-rollback" "$auth_cleanup_wrapper.shipyard-was-absent"
  /bin/rm -f "$auth_cleanup_binary.shipyard-rollback" "$auth_cleanup_binary.shipyard-was-absent"
  /bin/rm -f "$auth_cleanup_companion.shipyard-rollback" "$auth_cleanup_companion.shipyard-was-absent"
  /bin/rm -f "$auth_cleanup_binary.shipyard-rollback.tmp" "$auth_cleanup_companion.shipyard-rollback.tmp"
  if [ -n "$auth_cleanup_context" ]; then /bin/rm -f "$auth_cleanup_context.shipyard-rollback" "$auth_cleanup_context.shipyard-was-absent"; fi
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
  auth_journal_lines="$(/usr/bin/wc -l < "$auth_journal" | /usr/bin/tr -d ' ')"
  case "$auth_journal_lines" in 9|11) ;; *) return 1 ;; esac
  auth_phase="$(/usr/bin/sed -n '1p' "$auth_journal")"
  auth_recovery_authority="$(/usr/bin/sed -n '2p' "$auth_journal")"
  auth_recovery_helper="$(/usr/bin/sed -n '3p' "$auth_journal")"
  auth_recovery_wrapper="$(/usr/bin/sed -n '4p' "$auth_journal")"
  auth_recovery_binary="$(/usr/bin/sed -n '5p' "$auth_journal")"
  auth_recovery_companion="$(/usr/bin/sed -n '6p' "$auth_journal")"
  if [ "$auth_journal_lines" = 11 ]; then
    auth_recovery_context="$(/usr/bin/sed -n '7p' "$auth_journal")"
    auth_recovery_helper_digest="$(/usr/bin/sed -n '8p' "$auth_journal")"
    auth_recovery_wrapper_digest="$(/usr/bin/sed -n '9p' "$auth_journal")"
    auth_recovery_context_digest="$(/usr/bin/sed -n '10p' "$auth_journal")"
    auth_recovery_companion_required="$(/usr/bin/sed -n '11p' "$auth_journal")"
  else
    auth_recovery_context=
    auth_recovery_context_digest=
    auth_recovery_helper_digest="$(/usr/bin/sed -n '7p' "$auth_journal")"
    auth_recovery_wrapper_digest="$(/usr/bin/sed -n '8p' "$auth_journal")"
    auth_recovery_companion_required="$(/usr/bin/sed -n '9p' "$auth_journal")"
  fi
  test -n "$auth_recovery_authority"
  test "$auth_recovery_helper" != "$auth_recovery_wrapper"
  auth_safe_target "$auth_recovery_helper"
  auth_safe_target "$auth_recovery_wrapper"
  auth_safe_target "$auth_recovery_binary"
  auth_safe_target "$auth_recovery_companion"
  if [ -n "$auth_recovery_context" ]; then auth_safe_target "$auth_recovery_context"; fi
  case "$auth_recovery_companion_required" in 0|1) ;; *) return 1 ;; esac
  if [ "$auth_phase" = committed ]; then
    test "$(/usr/bin/shasum -a 256 "$auth_recovery_helper" | /usr/bin/awk '{{print $1}}')" = "$auth_recovery_helper_digest"
    test "$(/usr/bin/shasum -a 256 "$auth_recovery_wrapper" | /usr/bin/awk '{{print $1}}')" = "$auth_recovery_wrapper_digest"
    test -x "$auth_recovery_binary"
    if [ "$auth_recovery_companion_required" = 1 ]; then test -x "$auth_recovery_companion"; fi
    if [ -n "$auth_recovery_context" ]; then
      test "$(/usr/bin/shasum -a 256 "$auth_recovery_context" | /usr/bin/awk '{{print $1}}')" = "$auth_recovery_context_digest"
      test "$(/usr/bin/stat -f '%Lp' "$auth_recovery_context")" = 600
    fi
  else
    case "$auth_phase" in preparing|prepared|helper-installed|auth-installed|context-installed) ;; *) return 1 ;; esac
    if [ -n "$auth_recovery_context" ]; then auth_restore_one "$auth_recovery_context"; fi
    auth_restore_one "$auth_recovery_companion"
    auth_restore_one "$auth_recovery_binary"
    auth_restore_one "$auth_recovery_wrapper"
    auth_restore_one "$auth_recovery_helper"
  fi
  auth_cleanup_markers "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" "$auth_recovery_companion" "$auth_recovery_context"
}}

test -d "$auth_state_dir"
test ! -L "$auth_state_dir"
test "$(/usr/bin/stat -f '%u' "$auth_state_dir")" = "$(/usr/bin/id -u)"
auth_state_mode="$(/usr/bin/stat -f '%Lp' "$auth_state_dir")"
test $((8#$auth_state_mode & 8#22)) -eq 0
if [ ! -e "$auth_guard" ] && [ ! -L "$auth_guard" ]; then
  auth_prior_umask="$(umask)"
  umask 077
  : >> "$auth_guard"
  umask "$auth_prior_umask"
fi
test -f "$auth_guard"
test ! -L "$auth_guard"
test "$(/usr/bin/stat -f '%u' "$auth_guard")" = "$(/usr/bin/id -u)"
test "$(/usr/bin/stat -f '%Lp' "$auth_guard")" = 600
exec 9<>"$auth_guard"
if ! /usr/bin/lockf -s -t 0 9; then exec 9>&-; exit 1; fi
if ! /bin/mkdir "$auth_lock" 2>/dev/null; then
  test -d "$auth_lock"
  test ! -L "$auth_lock"
  test "$(/usr/bin/stat -f '%u' "$auth_lock")" = "$(/usr/bin/id -u)"
  auth_legacy_pid_file="$auth_lock/pid"
  test -f "$auth_legacy_pid_file"
  test ! -L "$auth_legacy_pid_file"
  test "$(/usr/bin/stat -f '%u' "$auth_legacy_pid_file")" = "$(/usr/bin/id -u)"
  test "$(/usr/bin/stat -f '%Lp' "$auth_legacy_pid_file")" = 600
  auth_legacy_pid="$(/bin/cat "$auth_legacy_pid_file")"
  case "$auth_legacy_pid" in ''|*[!0-9]*) exit 1 ;; esac
  if /bin/kill -0 "$auth_legacy_pid" 2>/dev/null; then exit 1; fi
  /bin/rm "$auth_legacy_pid_file"
  /bin/rmdir "$auth_lock"
  /bin/mkdir "$auth_lock"
fi
if ! /usr/bin/printf '%s\n' "$$" > "$auth_lock/pid"; then
  /bin/rmdir "$auth_lock"
  exec 9>&-
  exit 1
fi
if ! /bin/chmod 600 "$auth_lock/pid"; then
  /bin/rm -f "$auth_lock/pid"
  /bin/rmdir "$auth_lock"
  exec 9>&-
  exit 1
fi
auth_helper_tmp=
auth_wrapper_tmp=
auth_context_tmp=
auth_release_lock() {{
  auth_release_pid="$auth_lock/pid"
  test -f "$auth_release_pid"
  test ! -L "$auth_release_pid"
  test "$(/usr/bin/stat -f '%u' "$auth_release_pid")" = "$(/usr/bin/id -u)"
  test "$(/usr/bin/stat -f '%Lp' "$auth_release_pid")" = 600
  test "$(/bin/cat "$auth_release_pid")" = "$$"
  /bin/rm "$auth_release_pid"
  /bin/rmdir "$auth_lock"
  exec 9>&-
}}
auth_release_on_error() {{ auth_status=$?; trap - ERR INT TERM; if [ -n "$auth_helper_tmp" ]; then /bin/rm -f "$auth_helper_tmp"; fi; if [ -n "$auth_wrapper_tmp" ]; then /bin/rm -f "$auth_wrapper_tmp"; fi; if [ -n "$auth_context_tmp" ]; then /bin/rm -f "$auth_context_tmp"; fi; auth_release_lock; exit "$auth_status"; }}
trap auth_release_on_error ERR INT TERM

auth_safe_target "$auth_helper"
auth_safe_target "$auth_wrapper"
auth_safe_target "$auth_binary"
auth_safe_target "$auth_companion"
if [ "$auth_resolver_required" = 1 ]; then auth_safe_target "$auth_context"; fi
auth_recovery_needed=0
if [ -e "$auth_journal" ] || [ -L "$auth_journal" ]; then auth_recovery_needed=1; fi
case "$auth_recovery_needed" in 1) auth_recover ;; esac
for auth_target in "$auth_helper" "$auth_wrapper" "$auth_binary" "$auth_companion"; do
  test ! -e "$auth_target.shipyard-rollback"
  test ! -L "$auth_target.shipyard-rollback"
  test ! -e "$auth_target.shipyard-was-absent"
  test ! -L "$auth_target.shipyard-was-absent"
done
test ! -e "$auth_binary.shipyard-rollback.tmp"
test ! -L "$auth_binary.shipyard-rollback.tmp"
test ! -e "$auth_companion.shipyard-rollback.tmp"
test ! -L "$auth_companion.shipyard-rollback.tmp"
if [ "$auth_resolver_required" = 1 ]; then
  test ! -e "$auth_context.shipyard-rollback"
  test ! -L "$auth_context.shipyard-rollback"
  test ! -e "$auth_context.shipyard-was-absent"
  test ! -L "$auth_context.shipyard-was-absent"
fi

auth_helper_tmp="$(/usr/bin/mktemp "$(/usr/bin/dirname "$auth_helper")/.shipyard-auth-helper.XXXXXX")"
auth_wrapper_tmp="$(/usr/bin/mktemp "$(/usr/bin/dirname "$auth_wrapper")/.shipyard-auth-wrapper.XXXXXX")"
/bin/cp "$auth_helper_source" "$auth_helper_tmp"
/bin/cp "$auth_wrapper_source" "$auth_wrapper_tmp"
/bin/chmod 700 "$auth_helper_tmp" "$auth_wrapper_tmp"
auth_context_digest=
auth_context_tmp=
if [ "$auth_resolver_required" = 1 ]; then
  auth_context_tmp="$(/usr/bin/mktemp "$(/usr/bin/dirname "$auth_context")/.shipyard-auth-context.XXXXXX")"
  /usr/bin/printf '%s\n' "$auth_context_json" > "$auth_context_tmp"
  /bin/chmod 600 "$auth_context_tmp"
  auth_context_digest="$(/usr/bin/shasum -a 256 "$auth_context_tmp" | /usr/bin/awk '{{print $1}}')"
fi
test "$(/usr/bin/shasum -a 256 "$auth_helper_tmp" | /usr/bin/awk '{{print $1}}')" = "$auth_helper_digest"
test "$(/usr/bin/shasum -a 256 "$auth_wrapper_tmp" | /usr/bin/awk '{{print $1}}')" = "$auth_wrapper_digest"
auth_write_phase preparing

auth_rollback_on_error() {{
  auth_status=$?
  trap - ERR INT TERM
  if [ "$auth_resolver_required" = 1 ]; then auth_restore_one "$auth_context"; fi
  auth_restore_one "$auth_companion"
  auth_restore_one "$auth_binary"
  auth_restore_one "$auth_wrapper"
  auth_restore_one "$auth_helper"
  auth_cleanup_markers "$auth_helper" "$auth_wrapper" "$auth_binary" "$auth_companion" "$auth_context"
  /bin/rm -f "$auth_helper_tmp" "$auth_wrapper_tmp" "$auth_context_tmp"
  auth_release_lock
  exit "$auth_status"
}}
trap auth_rollback_on_error ERR INT TERM

if [ -e "$auth_helper" ]; then /bin/mv "$auth_helper" "$auth_helper.shipyard-rollback"; else /usr/bin/touch "$auth_helper.shipyard-was-absent"; /bin/chmod 600 "$auth_helper.shipyard-was-absent"; fi
if [ -e "$auth_wrapper" ]; then /bin/mv "$auth_wrapper" "$auth_wrapper.shipyard-rollback"; else /usr/bin/touch "$auth_wrapper.shipyard-was-absent"; /bin/chmod 600 "$auth_wrapper.shipyard-was-absent"; fi
if [ -e "$auth_binary" ]; then /bin/cp -p "$auth_binary" "$auth_binary.shipyard-rollback.tmp"; /bin/mv "$auth_binary.shipyard-rollback.tmp" "$auth_binary.shipyard-rollback"; else /usr/bin/touch "$auth_binary.shipyard-was-absent"; /bin/chmod 600 "$auth_binary.shipyard-was-absent"; fi
if [ -e "$auth_companion" ]; then /bin/cp -p "$auth_companion" "$auth_companion.shipyard-rollback.tmp"; /bin/mv "$auth_companion.shipyard-rollback.tmp" "$auth_companion.shipyard-rollback"; else /usr/bin/touch "$auth_companion.shipyard-was-absent"; /bin/chmod 600 "$auth_companion.shipyard-was-absent"; fi
if [ "$auth_resolver_required" = 1 ]; then
  if [ -e "$auth_context" ]; then /bin/mv "$auth_context" "$auth_context.shipyard-rollback"; else /usr/bin/touch "$auth_context.shipyard-was-absent"; /bin/chmod 600 "$auth_context.shipyard-was-absent"; fi
fi
auth_write_phase prepared
/bin/mv "$auth_helper_tmp" "$auth_helper"
auth_write_phase helper-installed
{injected_failure}
/bin/mv "$auth_wrapper_tmp" "$auth_wrapper"
auth_write_phase auth-installed
{binary_install_command}
if [ "$auth_resolver_required" = 1 ]; then
  /bin/mv "$auth_context_tmp" "$auth_context"
  auth_write_phase context-installed
fi
test "$(/usr/bin/shasum -a 256 "$auth_helper" | /usr/bin/awk '{{print $1}}')" = "$auth_helper_digest"
test "$(/usr/bin/shasum -a 256 "$auth_wrapper" | /usr/bin/awk '{{print $1}}')" = "$auth_wrapper_digest"
test "$(/usr/bin/stat -f '%Lp' "$auth_helper")" = 700
test "$(/usr/bin/stat -f '%Lp' "$auth_wrapper")" = 700
test -x "$auth_binary"
if [ "$auth_companion_required" = 1 ]; then test -x "$auth_companion"; fi
if [ "$auth_resolver_required" = 1 ]; then
  test "$(/usr/bin/shasum -a 256 "$auth_context" | /usr/bin/awk '{{print $1}}')" = "$auth_context_digest"
  test "$(/usr/bin/stat -f '%Lp' "$auth_context")" = 600
  "$auth_binary" --mode "$auth_mode" --global-dir "$auth_global_dir" auth helper-argv --wrapper "$auth_wrapper" --repo "$auth_probe_repo" >/dev/null
fi
auth_write_phase committed
trap - ERR INT TERM
auth_cleanup_markers "$auth_helper" "$auth_wrapper" "$auth_binary" "$auth_companion" "$auth_context"
auth_post_commit_on_error() {{ auth_status=$?; trap - ERR INT TERM; auth_release_lock; exit "$auth_status"; }}
trap auth_post_commit_on_error ERR INT TERM
{post_commit_command}
trap - ERR INT TERM
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
        run_with_probe(
            root,
            helper,
            wrapper,
            helper_source,
            wrapper_source,
            authority,
            fail_after_helper,
            "v0.129.0",
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_with_probe(
        root: &tempfile::TempDir,
        helper: &Path,
        wrapper: &Path,
        helper_source: &Path,
        wrapper_source: &Path,
        authority: &ReleaseAuthority,
        fail_after_helper: bool,
        target: &str,
        resolver_succeeds: bool,
    ) -> std::process::ExitStatus {
        run_with_probe_and_post_commit(
            root,
            helper,
            wrapper,
            helper_source,
            wrapper_source,
            authority,
            fail_after_helper,
            target,
            resolver_succeeds,
            ":",
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_with_probe_and_post_commit(
        root: &tempfile::TempDir,
        helper: &Path,
        wrapper: &Path,
        helper_source: &Path,
        wrapper_source: &Path,
        authority: &ReleaseAuthority,
        fail_after_helper: bool,
        target: &str,
        resolver_succeeds: bool,
        post_commit_command: &str,
    ) -> std::process::ExitStatus {
        let resolver_required = crate::app::fleet_update_cmd::tag_supports_auth_resolver(target);
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
        let expected_global_dir = shlex_quote(&state.display().to_string());
        let expected_wrapper = shlex_quote(&wrapper.display().to_string());
        let installed_binary_lines = if resolver_required {
            vec![
                "#!/bin/sh".to_owned(),
                "test \"$#\" = 10".to_owned(),
                "test \"$1\" = --mode".to_owned(),
                "test \"$2\" = shipyard".to_owned(),
                "test \"$3\" = --global-dir".to_owned(),
                format!("test \"$4\" = {expected_global_dir}"),
                "test \"$5\" = auth".to_owned(),
                "test \"$6\" = helper-argv".to_owned(),
                "test \"$7\" = --wrapper".to_owned(),
                format!("test \"$8\" = {expected_wrapper}"),
                "test \"$9\" = --repo".to_owned(),
                "test \"${10}\" = danielraffel/Shipyard".to_owned(),
                format!("exit {}", if resolver_succeeds { 0 } else { 71 }),
            ]
        } else {
            vec!["#!/bin/sh".to_owned(), "exit 86".to_owned()]
        };
        let installed_binary_lines = installed_binary_lines
            .iter()
            .map(|line| shlex_quote(line))
            .collect::<Vec<_>>()
            .join(" ");
        let installed_binary = format!(
            "/usr/bin/printf '%s\\n' {installed_binary_lines} > {}; /bin/chmod 700 {}",
            shlex_quote(&binary.display().to_string()),
            shlex_quote(&binary.display().to_string()),
        );
        let script = install_transaction(
            helper,
            wrapper,
            &binary,
            &companion,
            true,
            resolver_required,
            &shlex_quote(&helper_source.display().to_string()),
            &shlex_quote(&wrapper_source.display().to_string()),
            &installed_binary,
            post_commit_command,
            "shipyard",
            &state,
            &state,
            "danielraffel/Shipyard",
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
        let context = wrapper.with_file_name("ghapp.shipyard-context.json");
        let context_value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&context).expect("resolver context"))
                .expect("typed resolver context");
        assert_eq!(
            context_value,
            serde_json::json!({
                "schema_version": 1,
                "mode": "shipyard",
                "global_dir": root.path().join("Library/Application Support/shipyard"),
            })
        );
        assert_eq!(
            std::fs::metadata(&context)
                .expect("context metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
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
    fn v0_128_target_retains_four_target_transaction_without_resolver_probe() {
        let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();

        assert!(
            run_with_probe(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false,
                "v0.128.9",
                false,
            )
            .success(),
            "legacy target must not execute the installed binary's failing resolver path",
        );
        assert!(
            !wrapper
                .with_file_name("ghapp.shipyard-context.json")
                .exists()
        );
        assert!(
            !root
                .path()
                .join("Library/Application Support/shipyard/fleet-auth-support.transaction")
                .exists()
        );
    }

    #[test]
    fn non_file_existing_lock_refuses_without_mutation_or_reclamation() {
        let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
        let state = root.path().join("Library/Application Support/shipyard");
        let lock = state.join("fleet-auth-support.lock");
        std::fs::create_dir(&lock).expect("non-file lock");

        assert!(
            !run(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false,
            )
            .success()
        );
        assert!(lock.is_dir());
        assert!(!helper.exists());
        assert!(!wrapper.exists());
        assert!(
            !wrapper
                .with_file_name("ghapp.shipyard-context.json")
                .exists()
        );
        assert!(!state.join("fleet-auth-support.transaction").exists());
        assert!(
            !std::fs::read_dir(&state)
                .expect("state entries")
                .any(|entry| {
                    entry
                        .expect("state entry")
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".fleet-auth-support.lock.")
                })
        );
    }

    #[test]
    fn malformed_symlinked_and_live_legacy_pid_locks_are_preserved() {
        for scenario in ["malformed", "symlink", "live"] {
            let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
            let state = root.path().join("Library/Application Support/shipyard");
            let lock = state.join("fleet-auth-support.lock");
            let pid = lock.join("pid");
            std::fs::create_dir(&lock).expect("legacy lock");
            match scenario {
                "malformed" => std::fs::write(&pid, b"not-a-pid\n").expect("malformed pid"),
                "symlink" => {
                    let target = root.path().join("foreign-pid");
                    std::fs::write(&target, b"99999999\n").expect("foreign pid target");
                    symlink(&target, &pid).expect("pid symlink");
                }
                "live" => {
                    std::fs::write(&pid, format!("{}\n", std::process::id())).expect("live pid");
                }
                _ => unreachable!(),
            }
            if scenario != "symlink" {
                std::fs::set_permissions(&pid, std::fs::Permissions::from_mode(0o600))
                    .expect("pid mode");
            }

            assert!(
                !run(
                    &root,
                    &helper,
                    &wrapper,
                    &helper_source,
                    &wrapper_source,
                    &authority,
                    false,
                )
                .success(),
                "{scenario} legacy pid must refuse"
            );
            assert!(lock.is_dir(), "{scenario} lock must be preserved");
            assert!(!helper.exists());
            assert!(!wrapper.exists());
        }
    }

    #[test]
    fn invalid_advisory_guard_types_and_mode_refuse_before_artifact_mutation() {
        for scenario in ["directory", "symlink", "mode"] {
            let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
            let state = root.path().join("Library/Application Support/shipyard");
            let guard = state.join("fleet-auth-support.guard");
            match scenario {
                "directory" => std::fs::create_dir(&guard).expect("guard directory"),
                "symlink" => {
                    let target = root.path().join("foreign-guard");
                    std::fs::write(&target, b"").expect("foreign guard");
                    symlink(&target, &guard).expect("guard symlink");
                }
                "mode" => {
                    std::fs::write(&guard, b"").expect("guard file");
                    std::fs::set_permissions(&guard, std::fs::Permissions::from_mode(0o644))
                        .expect("guard mode");
                }
                _ => unreachable!(),
            }

            assert!(
                !run(
                    &root,
                    &helper,
                    &wrapper,
                    &helper_source,
                    &wrapper_source,
                    &authority,
                    false,
                )
                .success(),
                "{scenario} guard must refuse"
            );
            assert!(!helper.exists());
            assert!(!wrapper.exists());
            assert!(!state.join("fleet-auth-support.lock").exists());
        }
    }

    #[test]
    fn dead_legacy_directory_lock_is_reclaimed_under_advisory_guard() {
        let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
        let state = root.path().join("Library/Application Support/shipyard");
        let lock = state.join("fleet-auth-support.lock");
        let pid = lock.join("pid");
        std::fs::create_dir(&lock).expect("legacy lock directory");
        std::fs::write(&pid, b"99999999\n").expect("dead legacy pid");
        std::fs::set_permissions(&pid, std::fs::Permissions::from_mode(0o600))
            .expect("legacy pid mode");

        assert!(
            run(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false,
            )
            .success()
        );
        assert!(!lock.exists());
        let guard = state.join("fleet-auth-support.guard");
        assert_eq!(
            std::fs::metadata(&guard)
                .expect("advisory lock carrier")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn active_advisory_lock_refuses_concurrent_transaction_then_releases() {
        let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
        let state = root.path().join("Library/Application Support/shipyard");
        let lock = state.join("fleet-auth-support.guard");
        let acquired = root.path().join("lock-acquired");
        std::fs::write(&lock, b"").expect("lock carrier");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600))
            .expect("lock carrier mode");
        let mut holder = Command::new("/bin/bash")
            .args([
                "-c",
                "exec 9<>\"$1\"; /usr/bin/lockf -s -t 0 9 || exit 1; /usr/bin/touch \"$2\"; exec /bin/sleep 30",
                "holder",
            ])
            .arg(&lock)
            .arg(&acquired)
            .spawn()
            .expect("lock holder");
        for _ in 0..200 {
            if acquired.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(acquired.exists(), "holder did not acquire lock");

        assert!(
            !run(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false,
            )
            .success()
        );
        assert!(!helper.exists());
        assert!(!wrapper.exists());
        holder.kill().expect("stop holder");
        holder.wait().expect("reap holder");

        assert!(
            run(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false,
            )
            .success()
        );
    }

    #[test]
    fn detached_post_commit_child_does_not_inherit_advisory_guard() {
        let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
        assert!(
            run_with_probe_and_post_commit(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false,
                "v0.129.0",
                true,
                "/bin/sh -c '/bin/sleep 2 &' 9>&-",
            )
            .success()
        );
        assert!(
            run(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false,
            )
            .success(),
            "a detached post-commit child must not retain the advisory guard"
        );
    }

    #[test]
    fn foreign_replacement_of_legacy_pid_is_preserved_at_release() {
        let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
        assert!(
            !run_with_probe_and_post_commit(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false,
                "v0.129.0",
                true,
                "/usr/bin/printf '%s\\n' 99999999 > \"$auth_lock/pid\"; /bin/chmod 600 \"$auth_lock/pid\"",
            )
            .success()
        );
        let pid = root
            .path()
            .join("Library/Application Support/shipyard/fleet-auth-support.lock/pid");
        assert_eq!(
            std::fs::read_to_string(&pid).expect("foreign pid"),
            "99999999\n"
        );
        assert_eq!(
            std::fs::read(&helper).expect("committed helper"),
            b"new helper\n"
        );
        assert_eq!(
            std::fs::read(&wrapper).expect("committed wrapper"),
            b"new wrapper\n"
        );
    }

    #[test]
    fn resolver_failure_skips_refresh_and_refresh_failure_releases_both_lock_layers() {
        let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
        let refreshed = root.path().join("refresh-ran");
        let touch_refresh = format!(
            "/usr/bin/touch {}",
            shlex_quote(&refreshed.display().to_string())
        );
        assert!(
            !run_with_probe_and_post_commit(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false,
                "v0.129.0",
                false,
                &touch_refresh,
            )
            .success()
        );
        assert!(!refreshed.exists(), "failed resolver must not refresh");

        assert!(
            !run_with_probe_and_post_commit(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false,
                "v0.129.0",
                true,
                "/usr/bin/false",
            )
            .success()
        );
        let state = root.path().join("Library/Application Support/shipyard");
        assert!(!state.join("fleet-auth-support.lock").exists());
        assert!(state.join("fleet-auth-support.guard").is_file());
        assert_eq!(
            std::fs::read(&helper).expect("committed helper"),
            b"new helper\n"
        );
        assert_eq!(
            std::fs::read(&wrapper).expect("committed wrapper"),
            b"new wrapper\n"
        );
        assert!(
            run(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false,
            )
            .success(),
            "refresh failure must release the advisory guard"
        );
    }

    #[test]
    fn v0_128_recovers_nine_line_journal_and_partial_atomic_backups() {
        let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
        let state = root.path().join("Library/Application Support/shipyard");
        let binary = root.path().join(".local/bin/shipyard");
        let companion = root.path().join(".local/bin/shipyard-workstream-provider");
        for (path, contents) in [
            (&helper, b"old helper\n".as_slice()),
            (&wrapper, b"old wrapper\n".as_slice()),
            (&binary, b"old binary\n".as_slice()),
            (&companion, b"old companion\n".as_slice()),
        ] {
            std::fs::write(path, contents).expect("old artifact");
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .expect("old mode");
        }
        for path in [&binary, &companion] {
            std::fs::write(
                format!("{}.shipyard-rollback.tmp", path.display()),
                b"partial backup",
            )
            .expect("interrupted backup");
        }
        let journal = state.join("fleet-auth-support.transaction");
        let journal_contents = format!(
            "preparing\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n1\n",
            "f".repeat(64),
            helper.display(),
            wrapper.display(),
            binary.display(),
            companion.display(),
            digest(b"old helper\n"),
            digest(b"old wrapper\n"),
        );
        assert_eq!(journal_contents.lines().count(), 9);
        std::fs::write(&journal, journal_contents).expect("legacy journal");

        assert!(
            !run_with_probe(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                true,
                "v0.128.9",
                false,
            )
            .success()
        );
        for (path, expected) in [
            (&helper, b"old helper\n".as_slice()),
            (&wrapper, b"old wrapper\n".as_slice()),
            (&binary, b"old binary\n".as_slice()),
            (&companion, b"old companion\n".as_slice()),
        ] {
            assert_eq!(std::fs::read(path).expect("restored artifact"), expected);
        }
        assert!(!journal.exists());
        assert!(!state.join("fleet-auth-support.lock").exists());
        let lock = state.join("fleet-auth-support.guard");
        assert!(lock.is_file());
        assert_eq!(
            std::fs::metadata(&lock)
                .expect("lock metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(
            !std::path::Path::new(&format!("{}.shipyard-rollback.tmp", binary.display())).exists()
        );
        assert!(
            !std::path::Path::new(&format!("{}.shipyard-rollback.tmp", companion.display()))
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
    fn post_install_resolver_failure_rolls_back_all_installed_artifacts() {
        let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
        let binary = root.path().join(".local/bin/shipyard");
        let companion = root.path().join(".local/bin/shipyard-workstream-provider");
        let context = wrapper.with_file_name("ghapp.shipyard-context.json");
        for (path, contents) in [
            (&helper, b"old helper\n".as_slice()),
            (&wrapper, b"old wrapper\n".as_slice()),
            (&binary, b"old binary\n".as_slice()),
            (&companion, b"old companion\n".as_slice()),
        ] {
            std::fs::write(path, contents).expect("old artifact");
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .expect("old mode");
        }
        std::fs::write(&context, b"old context\n").expect("old context");
        std::fs::set_permissions(&context, std::fs::Permissions::from_mode(0o600))
            .expect("context mode");

        assert!(
            !run_with_probe(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                false,
                "v0.129.0",
                false,
            )
            .success()
        );
        assert_eq!(std::fs::read(&helper).expect("helper"), b"old helper\n");
        assert_eq!(std::fs::read(&wrapper).expect("wrapper"), b"old wrapper\n");
        assert_eq!(std::fs::read(&binary).expect("binary"), b"old binary\n");
        assert_eq!(std::fs::read(&context).expect("context"), b"old context\n");
        assert_eq!(
            std::fs::read(&companion).expect("companion"),
            b"old companion\n"
        );
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
        std::fs::create_dir(&lock).expect("stale legacy lock");
        std::fs::write(lock.join("pid"), b"99999999\n").expect("stale legacy pid");
        std::fs::set_permissions(lock.join("pid"), std::fs::Permissions::from_mode(0o600))
            .expect("stale pid mode");

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
    fn next_release_rolls_back_an_interrupted_context_install() {
        let (root, helper, wrapper, helper_source, wrapper_source, authority) = fixture();
        let state = root.path().join("Library/Application Support/shipyard");
        let binary = root.path().join(".local/bin/shipyard");
        let companion = root.path().join(".local/bin/shipyard-workstream-provider");
        let context = wrapper.with_file_name("ghapp.shipyard-context.json");
        for (path, current, old, mode) in [
            (
                &helper,
                b"new helper\n".as_slice(),
                b"old helper\n".as_slice(),
                0o700,
            ),
            (
                &wrapper,
                b"new wrapper\n".as_slice(),
                b"old wrapper\n".as_slice(),
                0o700,
            ),
            (
                &binary,
                b"new binary\n".as_slice(),
                b"old binary\n".as_slice(),
                0o700,
            ),
            (
                &companion,
                b"new companion\n".as_slice(),
                b"old companion\n".as_slice(),
                0o700,
            ),
            (
                &context,
                b"new context\n".as_slice(),
                b"old context\n".as_slice(),
                0o600,
            ),
        ] {
            std::fs::write(path, current).expect("interrupted artifact");
            std::fs::write(
                std::path::PathBuf::from(format!("{}.shipyard-rollback", path.display())),
                old,
            )
            .expect("rollback artifact");
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .expect("artifact mode");
        }
        std::fs::write(
            state.join("fleet-auth-support.transaction"),
            format!(
                "context-installed\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n1\n",
                "f".repeat(64),
                helper.display(),
                wrapper.display(),
                binary.display(),
                companion.display(),
                context.display(),
                digest(b"new helper\n"),
                digest(b"new wrapper\n"),
                digest(b"new context\n"),
            ),
        )
        .expect("current journal");

        assert!(
            !run(
                &root,
                &helper,
                &wrapper,
                &helper_source,
                &wrapper_source,
                &authority,
                true,
            )
            .success()
        );
        for (path, expected) in [
            (&helper, b"old helper\n".as_slice()),
            (&wrapper, b"old wrapper\n".as_slice()),
            (&binary, b"old binary\n".as_slice()),
            (&companion, b"old companion\n".as_slice()),
            (&context, b"old context\n".as_slice()),
        ] {
            assert_eq!(std::fs::read(path).expect("restored artifact"), expected);
        }
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
