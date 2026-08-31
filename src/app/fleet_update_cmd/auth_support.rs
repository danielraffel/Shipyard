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
const LOCK_ACQUISITION_SCRIPT: &str = r#"
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
if [ -e "$auth_lock" ] || [ -L "$auth_lock" ]; then
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
fi
auth_lock_stage_parent="$(/usr/bin/mktemp -d "$auth_state_dir/.fleet-auth-support-lock-stage.XXXXXX")"
/bin/chmod 700 "$auth_lock_stage_parent"
auth_lock_staging="$auth_lock_stage_parent/fleet-auth-support.lock"
/bin/mkdir "$auth_lock_staging"
/bin/chmod 700 "$auth_lock_staging"
auth_lock_staging_pid="$auth_lock_staging/pid"
if ! /usr/bin/printf '%s\n' "$$" > "$auth_lock_staging_pid"; then
  /bin/rm -f "$auth_lock_staging_pid"
  /bin/rmdir "$auth_lock_staging"
  /bin/rmdir "$auth_lock_stage_parent"
  exec 9>&-
  exit 1
fi
if ! /bin/chmod 600 "$auth_lock_staging_pid"; then
  /bin/rm -f "$auth_lock_staging_pid"
  /bin/rmdir "$auth_lock_staging"
  /bin/rmdir "$auth_lock_stage_parent"
  exec 9>&-
  exit 1
fi
auth_lock_staging_inode="$(/usr/bin/stat -f '%i' "$auth_lock_staging")"
# Pre-v0.131 clients do not acquire auth_guard and can still publish the legacy
# directory concurrently. Verify that the final inode is ours; a destination
# race must fail closed without deleting the old client's ownership.
if ! /bin/mv -n "$auth_lock_staging" "$auth_state_dir/"; then
  if [ -d "$auth_lock_staging" ] && [ ! -L "$auth_lock_staging" ]; then
    /bin/rm -f "$auth_lock_staging_pid"
    /bin/rmdir "$auth_lock_staging"
  fi
  /bin/rmdir "$auth_lock_stage_parent"
  exec 9>&-
  exit 1
fi
if [ -e "$auth_lock_staging" ] || [ -L "$auth_lock_staging" ]; then
  if [ -d "$auth_lock_staging" ] && [ ! -L "$auth_lock_staging" ]; then
    /bin/rm -f "$auth_lock_staging_pid"
    /bin/rmdir "$auth_lock_staging"
  fi
  /bin/rmdir "$auth_lock_stage_parent"
  exec 9>&-
  exit 1
fi
/bin/rmdir "$auth_lock_stage_parent"
auth_published_lock_inode="$(/usr/bin/stat -f '%i' "$auth_lock")"
if [ "$auth_published_lock_inode" != "$auth_lock_staging_inode" ]; then
  exec 9>&-
  exit 1
fi
auth_lock_stage_parent=
auth_lock_staging=
"#;

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
    mode: &str,
    global_dir: &Path,
    state_dir: &Path,
    probe_repo: &str,
    authority: &ReleaseAuthority,
    refresh_prefix: &str,
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
    let refresh_prefix = shlex_quote(refresh_prefix);
    let helper_digest = shlex_quote(&authority.auth_helper.sha256);
    let wrapper_digest = shlex_quote(&authority.auth_wrapper.sha256);
    let lock_acquisition = LOCK_ACQUISITION_SCRIPT;
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
auth_refresh_prefix={refresh_prefix}
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
    if [ "$auth_journal_lines" = 9 ] && [ "$auth_phase" = preparing ]; then
      # Pre-v0.131 clients copied binary rollback files directly after writing
      # `preparing`. A crash could leave partial copies while both live binaries
      # were still intact, so only the already-moved helper pair is restored.
      :
    else
      if [ -n "$auth_recovery_context" ]; then auth_restore_one "$auth_recovery_context"; fi
      auth_restore_one "$auth_recovery_companion"
      auth_restore_one "$auth_recovery_binary"
    fi
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
{lock_acquisition}
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
auth_release_after_failure() {{ auth_status="$1"; trap - ERR INT TERM; if [ -n "$auth_helper_tmp" ]; then /bin/rm -f "$auth_helper_tmp"; fi; if [ -n "$auth_wrapper_tmp" ]; then /bin/rm -f "$auth_wrapper_tmp"; fi; if [ -n "$auth_context_tmp" ]; then /bin/rm -f "$auth_context_tmp"; fi; auth_release_lock; exit "$auth_status"; }}
auth_release_on_error() {{ auth_release_after_failure "$?"; }}
trap auth_release_on_error ERR
trap 'auth_release_after_failure 130' INT
trap 'auth_release_after_failure 143' TERM

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

auth_rollback_after_failure() {{
  auth_status="$1"
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
auth_rollback_on_error() {{ auth_rollback_after_failure "$?"; }}
trap auth_rollback_on_error ERR
trap 'auth_rollback_after_failure 130' INT
trap 'auth_rollback_after_failure 143' TERM

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
auth_post_commit_after_failure() {{ auth_status="$1"; trap - ERR INT TERM; auth_release_lock; exit "$auth_status"; }}
auth_post_commit_on_error() {{ auth_post_commit_after_failure "$?"; }}
trap auth_post_commit_on_error ERR
trap 'auth_post_commit_after_failure 130' INT
trap 'auth_post_commit_after_failure 143' TERM
/usr/bin/printf '%s' "$auth_refresh_prefix"
"$auth_binary" --mode "$auth_mode" --global-dir "$auth_global_dir" --state-dir "$auth_state_dir" --json daemon refresh 9>&- | /usr/bin/tr -d '\n'
/usr/bin/printf '\n'
trap - ERR INT TERM
auth_release_lock
"#
    )
}

#[cfg(all(test, target_os = "macos"))]
mod tests;
