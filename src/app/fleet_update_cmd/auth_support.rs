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
pub(super) const BEFORE_HELPER_TARGET_PREFIX: &str = "SHIPYARD_FLEET_BEFORE_AUTH_HELPER_TARGET=";
pub(super) const BEFORE_WRAPPER_TARGET_PREFIX: &str = "SHIPYARD_FLEET_BEFORE_AUTH_WRAPPER_TARGET=";
pub(super) const AFTER_HELPER_TARGET_PREFIX: &str = "SHIPYARD_FLEET_AFTER_AUTH_HELPER_TARGET=";
pub(super) const AFTER_WRAPPER_TARGET_PREFIX: &str = "SHIPYARD_FLEET_AFTER_AUTH_WRAPPER_TARGET=";
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

pub(super) fn source_urls(authority: &ReleaseAuthority) -> (String, String, String) {
    let base = format!(
        "https://raw.githubusercontent.com/{}/{}",
        authority.repository, authority.commit_oid
    );
    (
        format!("{base}/{}", authority.auth_helper.path),
        format!("{base}/{}", authority.auth_wrapper.path),
        format!("{base}/{}", authority.pr_close_guard.path),
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
        "if [ -e {helper} ] || [ -L {helper} ]; then test -f {helper}; {phase}_auth_helper_sha256=\"$(/usr/bin/shasum -a 256 {helper} | /usr/bin/awk '{{print $1}}')\"; {phase}_auth_helper_mode=\"$(/usr/bin/stat -L -f '%Lp' {helper})\"; if [ -L {helper} ]; then {phase}_auth_helper_target=\"$(/usr/bin/readlink {helper})\"; else {phase}_auth_helper_target=direct; fi; else {phase}_auth_helper_sha256=absent; {phase}_auth_helper_mode=absent; {phase}_auth_helper_target=absent; fi\n\
         if [ -e {wrapper} ] || [ -L {wrapper} ]; then test -f {wrapper}; if [ -L {wrapper} ]; then {phase}_auth_wrapper_target=\"$(/usr/bin/readlink {wrapper})\"; {phase}_auth_wrapper_sha256=\"$(/usr/bin/shasum -a 256 {wrapper} | /usr/bin/awk '{{print $1}}')\"; {phase}_auth_wrapper_mode=\"$(/usr/bin/stat -L -f '%Lp' {wrapper})\"; elif [ -L {wrapper}.shipyard-generation ]; then {phase}_auth_wrapper_target=\"$(/usr/bin/readlink {wrapper}.shipyard-generation)\"; test -f \"${{{phase}_auth_wrapper_target}}\"; test ! -L \"${{{phase}_auth_wrapper_target}}\"; {phase}_auth_wrapper_sha256=\"$(/usr/bin/shasum -a 256 \"${{{phase}_auth_wrapper_target}}\" | /usr/bin/awk '{{print $1}}')\"; {phase}_auth_wrapper_mode=\"$(/usr/bin/stat -f '%Lp' \"${{{phase}_auth_wrapper_target}}\")\"; else {phase}_auth_wrapper_target=direct; {phase}_auth_wrapper_sha256=\"$(/usr/bin/shasum -a 256 {wrapper} | /usr/bin/awk '{{print $1}}')\"; {phase}_auth_wrapper_mode=\"$(/usr/bin/stat -f '%Lp' {wrapper})\"; fi; else {phase}_auth_wrapper_sha256=absent; {phase}_auth_wrapper_mode=absent; {phase}_auth_wrapper_target=absent; fi"
    )
}

/// Generate the macOS transaction. Both source files must already exist in a
/// private staging directory and have been checked against the frozen release
/// authority. The journal makes the compatibility-projection transaction
/// recoverable after abrupt process death; ordinary errors roll back before
/// returning.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // Keep publication and recovery ordering in one auditable shell transaction.
pub(super) fn install_transaction(
    helper: &Path,
    wrapper: &Path,
    binary: &Path,
    companion: &Path,
    companion_required: bool,
    resolver_required: bool,
    helper_source: &str,
    wrapper_source: &str,
    close_guard_source: &str,
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
            "schema_version": 2,
            "mode": mode,
            "global_dir": global_dir.display().to_string(),
            "authority_identity": authority.identity_sha256,
            "generation_id": "__SHIPYARD_AUTH_GENERATION__",
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
    let close_guard_digest = shlex_quote(&authority.pr_close_guard.sha256);
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
auth_close_guard="$HOME/.config/shipyard/guards/pr-close-guard"
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
case "$auth_authority" in ''|*[!0-9a-f]*) exit 1 ;; esac
test "${{#auth_authority}}" = 64
auth_generation_root="$HOME/.local/share/shipyard/auth-generations"
auth_generation=
auth_generation_id=
auth_helper_digest={helper_digest}
auth_wrapper_digest={wrapper_digest}
auth_close_guard_digest={close_guard_digest}
auth_helper_source={helper_source}
auth_wrapper_source={wrapper_source}
auth_close_guard_source={close_guard_source}
auth_selector="$auth_wrapper.shipyard-generation"
auth_journal="$auth_state_dir/fleet-auth-support.transaction"
auth_lock="$auth_state_dir/fleet-auth-support.lock"
auth_guard="$auth_state_dir/fleet-auth-support.guard"
auth_original_selector_kind=
auth_original_selector_identity=
auth_original_authority=absent
auth_original_manifest_digest=absent
auth_original_backup_digest=pending
auth_target_generation_id=
auth_target_wrapper_target=
auth_target_manifest_digest=
auth_anchor_id=absent
auth_anchor_wrapper_target=absent
auth_anchor_manifest_digest=absent

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
  if [ -L "$auth_target" ]; then
    auth_link_target="$(/usr/bin/readlink "$auth_target")"
    auth_link_member="$(/usr/bin/basename "$auth_target")"
    if [ "$auth_target" = "$auth_selector" ]; then auth_link_member=ghapp; fi
    case "$auth_link_target" in "$auth_generation_root"/*/"$auth_link_member") ;; *) return 1 ;; esac
    auth_link_tail="${{auth_link_target#"$auth_generation_root"/}}"
    auth_link_id="${{auth_link_tail%%/*}}"
    test "${{#auth_link_id}}" = 64
    case "$auth_link_id" in ''|*[!0-9a-f]*) return 1 ;; esac
    test "$auth_link_tail" = "$auth_link_id/$auth_link_member"
    test -f "$auth_link_target"
    test ! -L "$auth_link_target"
    test "$(/usr/bin/stat -f '%u' "$auth_link_target")" = "$(/usr/bin/id -u)"
  elif [ -e "$auth_target" ]; then
    test -f "$auth_target"
    test "$(/usr/bin/stat -f '%u' "$auth_target")" = "$(/usr/bin/id -u)"
  fi
}}

auth_write_phase() {{
  auth_phase_tmp="$(/usr/bin/mktemp "$auth_state_dir/.fleet-auth-support.phase.XXXXXX")"
  /usr/bin/printf '%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n' \
    'shipyard-fleet-auth-v4' "$1" "$auth_authority" "$auth_helper" "$auth_wrapper" \
    "$auth_binary" "$auth_companion" "$auth_context" "$auth_close_guard" \
    "$auth_helper_digest" "$auth_wrapper_digest" "$auth_close_guard_digest" \
    "$auth_context_digest" "$auth_companion_required" \
    "$auth_resolver_required" "$auth_original_selector_kind" \
    "$auth_original_selector_identity" "$auth_target_generation_id" \
    "$auth_target_wrapper_target" "$auth_target_manifest_digest" "$auth_anchor_id" \
    "$auth_anchor_wrapper_target" "$auth_anchor_manifest_digest" \
    "$auth_original_manifest_digest" "$auth_original_backup_digest" \
    "$auth_original_authority" > "$auth_phase_tmp"
  /bin/chmod 600 "$auth_phase_tmp"
  /bin/mv -f "$auth_phase_tmp" "$auth_journal"
}}

auth_publish_link() {{
  auth_target="$1"
  auth_source="$2"
  auth_link_stage="$(/usr/bin/mktemp "$auth_target.shipyard-link.XXXXXX")"
  /bin/rm "$auth_link_stage"
  /bin/ln -s "$auth_source" "$auth_link_stage"
  /bin/mv -f "$auth_link_stage" "$auth_target"
}}

auth_publish_file() {{
  auth_target="$1"
  auth_source="$2"
  auth_file_stage="$(/usr/bin/mktemp "$auth_target.shipyard-file.XXXXXX")"
  /bin/cp "$auth_source" "$auth_file_stage"
  /bin/chmod 700 "$auth_file_stage"
  /bin/mv -f "$auth_file_stage" "$auth_target"
}}

auth_publish_generation_selection() {{
  auth_selection_wrapper="$1"
  auth_selection_target="$2"
  auth_selection_stable=0
  if [ -f "$auth_selection_wrapper" ] && [ ! -L "$auth_selection_wrapper" ] && \
     [ "$(/usr/bin/grep -c '^# Shipyard-Stable-Public-Trampoline-Contract: stable-selector-v1$' "$auth_selection_wrapper" || true)" = 1 ]; then
    auth_selection_stable=1
  fi
  if [ "$auth_selection_stable" = 1 ]; then
    auth_publish_link "$auth_selection_wrapper.shipyard-generation" "$auth_selection_target"
  else
    auth_publish_link "$auth_selection_wrapper" "$auth_selection_target"
  fi
}}

auth_selector_identity() {{
  auth_selector_path="$1"
  if [ -L "$auth_selector_path" ]; then
    auth_selector_target="$(/usr/bin/readlink "$auth_selector_path")"
    case "$auth_selector_target" in "$auth_generation_root"/*/ghapp) ;; *) return 1 ;; esac
    auth_selector_tail="${{auth_selector_target#"$auth_generation_root"/}}"
    auth_selector_id="${{auth_selector_tail%%/*}}"
    test "${{#auth_selector_id}}" = 64
    case "$auth_selector_id" in ''|*[!0-9a-f]*) return 1 ;; esac
    test "$auth_selector_tail" = "$auth_selector_id/ghapp"
    /usr/bin/printf 'generation:%s\n' "$auth_selector_target"
  elif [ -f "$auth_selector_path" ] && [ ! -L "$auth_selector_path" ] && \
       [ "$(/usr/bin/grep -c '^# Shipyard-Stable-Public-Trampoline-Contract: stable-selector-v1$' "$auth_selector_path" || true)" = 1 ] && \
       [ -L "$auth_selector_path.shipyard-generation" ]; then
    auth_selector_target="$(/usr/bin/readlink "$auth_selector_path.shipyard-generation")"
    case "$auth_selector_target" in "$auth_generation_root"/*/ghapp) ;; *) return 1 ;; esac
    auth_selector_tail="${{auth_selector_target#"$auth_generation_root"/}}"
    auth_selector_id="${{auth_selector_tail%%/*}}"
    test "${{#auth_selector_id}}" = 64
    case "$auth_selector_id" in ''|*[!0-9a-f]*) return 1 ;; esac
    test "$auth_selector_tail" = "$auth_selector_id/ghapp"
    /usr/bin/printf 'generation:%s\n' "$auth_selector_target"
  elif [ -e "$auth_selector_path.shipyard-generation" ] || [ -L "$auth_selector_path.shipyard-generation" ]; then
    return 1
  elif [ -f "$auth_selector_path" ]; then
    test ! -L "$auth_selector_path"
    /usr/bin/printf 'direct:%s\n' "$(/usr/bin/shasum -a 256 "$auth_selector_path" | /usr/bin/awk '{{print $1}}')"
  elif [ ! -e "$auth_selector_path" ]; then
    /usr/bin/printf 'absent\n'
  else
    return 1
  fi
}}

auth_manifest_value() {{
  auth_manifest="$1"
  auth_key="$2"
  /usr/bin/awk -F= -v key="$auth_key" '
    $1 == key {{ if (seen) exit 2; value=substr($0, length(key)+2); seen=1 }}
    END {{ if (!seen || value == "") exit 1; print value }}
  ' "$auth_manifest"
}}

auth_validate_recovery_generation() {{
  auth_generation_id_check="$1"
  auth_generation_wrapper_check="$2"
  auth_generation_manifest_digest_check="$3"
  auth_generation_kind_check="$4"
  auth_generation_authority_check="$5"
  auth_generation_guard_required_check="$6"
  case "$auth_generation_guard_required_check" in 0|1) ;; *) return 1 ;; esac
  case "$auth_generation_id_check" in ''|*[!0-9a-f]*) return 1 ;; esac
  test "${{#auth_generation_id_check}}" = 64
  test "$auth_generation_wrapper_check" = "$auth_generation_root/$auth_generation_id_check/ghapp"
  auth_generation_dir_check="${{auth_generation_wrapper_check%/ghapp}}"
  test -d "$auth_generation_dir_check"
  test ! -L "$auth_generation_dir_check"
  test "$(/usr/bin/stat -f '%u' "$auth_generation_dir_check")" = "$(/usr/bin/id -u)"
  test "$(/usr/bin/stat -f '%Lp' "$auth_generation_dir_check")" = 700
  auth_generation_manifest_check="$auth_generation_dir_check/generation.manifest"
  auth_generation_seed_check="$auth_generation_dir_check/generation.seed"
  for auth_private_member in "$auth_generation_manifest_check" "$auth_generation_seed_check"; do
    test -f "$auth_private_member"; test ! -L "$auth_private_member"
    test "$(/usr/bin/stat -f '%u' "$auth_private_member")" = "$(/usr/bin/id -u)"
    test "$(/usr/bin/stat -f '%Lp' "$auth_private_member")" = 600
  done
  test "$(/usr/bin/shasum -a 256 "$auth_generation_manifest_check" | /usr/bin/awk '{{print $1}}')" = "$auth_generation_manifest_digest_check"
  test "$(/usr/bin/shasum -a 256 "$auth_generation_seed_check" | /usr/bin/awk '{{print $1}}')" = "$auth_generation_id_check"
  test "$(auth_manifest_value "$auth_generation_manifest_check" generation_id)" = "$auth_generation_id_check"
  auth_generation_contract_check="$(auth_manifest_value "$auth_generation_manifest_check" generation_contract)"
  if [ "$auth_generation_guard_required_check" = 1 ]; then
    test "$auth_generation_contract_check" = auth-selector-v2
  else
    case "$auth_generation_contract_check" in auth-selector-v1|auth-selector-v2) ;; *) return 1 ;; esac
  fi
  test "$(auth_manifest_value "$auth_generation_manifest_check" authority_identity)" = "$auth_generation_authority_check"
  if [ "$auth_generation_kind_check" = legacy-anchor ]; then
    test "$(auth_manifest_value "$auth_generation_manifest_check" generation_kind)" = legacy-anchor
  else
    if /usr/bin/grep -q '^generation_kind=' "$auth_generation_manifest_check"; then return 1; fi
  fi
  for auth_member_spec in \
    'shipyard-github-app-token:helper_sha256:700' \
    'ghapp:wrapper_sha256:700' \
    'shipyard:binary_sha256:700'; do
    auth_member_name="${{auth_member_spec%%:*}}"
    auth_member_rest="${{auth_member_spec#*:}}"
    auth_member_key="${{auth_member_rest%%:*}}"
    auth_member_mode="${{auth_member_rest##*:}}"
    auth_member_path="$auth_generation_dir_check/$auth_member_name"
    test -f "$auth_member_path"; test ! -L "$auth_member_path"
    test "$(/usr/bin/stat -f '%u' "$auth_member_path")" = "$(/usr/bin/id -u)"
    test "$(/usr/bin/stat -f '%Lp' "$auth_member_path")" = "$auth_member_mode"
    test "$(/usr/bin/shasum -a 256 "$auth_member_path" | /usr/bin/awk '{{print $1}}')" = "$(auth_manifest_value "$auth_generation_manifest_check" "$auth_member_key")"
  done
  if /usr/bin/grep -q '^public_trampoline_sha256=' "$auth_generation_manifest_check"; then
    auth_trampoline_path_check="$auth_generation_dir_check/ghapp.public-trampoline"
    test -f "$auth_trampoline_path_check"; test ! -L "$auth_trampoline_path_check"
    test "$(/usr/bin/stat -f '%u' "$auth_trampoline_path_check")" = "$(/usr/bin/id -u)"
    test "$(auth_manifest_value "$auth_generation_manifest_check" public_trampoline_mode)" = 700
    test "$(/usr/bin/stat -f '%Lp' "$auth_trampoline_path_check")" = 700
    test "$(/usr/bin/shasum -a 256 "$auth_trampoline_path_check" | /usr/bin/awk '{{print $1}}')" = "$(auth_manifest_value "$auth_generation_manifest_check" public_trampoline_sha256)"
  else
    test "$auth_generation_guard_required_check" = 0
    test ! -e "$auth_generation_dir_check/ghapp.public-trampoline"
    test ! -L "$auth_generation_dir_check/ghapp.public-trampoline"
  fi
  if /usr/bin/grep -q '^close_guard_sha256=' "$auth_generation_manifest_check"; then
    auth_close_guard_path_check="$auth_generation_dir_check/pr-close-guard"
    test -f "$auth_close_guard_path_check"; test ! -L "$auth_close_guard_path_check"
    test "$(/usr/bin/stat -f '%u' "$auth_close_guard_path_check")" = "$(/usr/bin/id -u)"
    test "$(/usr/bin/stat -f '%Lp' "$auth_close_guard_path_check")" = "$(auth_manifest_value "$auth_generation_manifest_check" close_guard_mode)"
    test "$(auth_manifest_value "$auth_generation_manifest_check" close_guard_mode)" = 700
    test "$(/usr/bin/shasum -a 256 "$auth_close_guard_path_check" | /usr/bin/awk '{{print $1}}')" = "$(auth_manifest_value "$auth_generation_manifest_check" close_guard_sha256)"
    if [ "$auth_generation_guard_required_check" = 1 ]; then
      test "$(/usr/bin/grep -c '^# Shipyard-Sibling-Close-Guard-Contract: sibling-close-guard-v1$' "$auth_generation_wrapper_check")" = 1
    fi
  else
    test "$auth_generation_guard_required_check" = 0
    # Existing generations and v2 journals can legitimately predate the guard.
    test ! -e "$auth_generation_dir_check/pr-close-guard"
    test ! -L "$auth_generation_dir_check/pr-close-guard"
  fi
  auth_companion_sha_check="$(auth_manifest_value "$auth_generation_manifest_check" companion_sha256)"
  if [ "$auth_recovery_companion_required" = 1 ]; then
    auth_companion_path_check="$auth_generation_dir_check/shipyard-workstream-provider"
    test -f "$auth_companion_path_check"; test ! -L "$auth_companion_path_check"; test -x "$auth_companion_path_check"
    test "$(/usr/bin/stat -f '%u' "$auth_companion_path_check")" = "$(/usr/bin/id -u)"
    test "$(/usr/bin/stat -f '%Lp' "$auth_companion_path_check")" = 700
    test "$(/usr/bin/shasum -a 256 "$auth_companion_path_check" | /usr/bin/awk '{{print $1}}')" = "$auth_companion_sha_check"
  else
    test "$auth_companion_sha_check" = absent
    test ! -e "$auth_generation_dir_check/shipyard-workstream-provider"
    test ! -L "$auth_generation_dir_check/shipyard-workstream-provider"
  fi
  auth_context_sha_check="$(auth_manifest_value "$auth_generation_manifest_check" context_sha256)"
  if [ "$auth_recovery_resolver_required" = 1 ]; then
    auth_context_path_check="$auth_generation_dir_check/ghapp.shipyard-context.json"
    test -f "$auth_context_path_check"; test ! -L "$auth_context_path_check"
    test "$(/usr/bin/stat -f '%u' "$auth_context_path_check")" = "$(/usr/bin/id -u)"
    test "$(/usr/bin/stat -f '%Lp' "$auth_context_path_check")" = 600
    test "$(/usr/bin/shasum -a 256 "$auth_context_path_check" | /usr/bin/awk '{{print $1}}')" = "$auth_context_sha_check"
    /usr/bin/grep -q "\"generation_id\":\"$auth_generation_id_check\"" "$auth_context_path_check"
  else
    test "$auth_context_sha_check" = absent
    test ! -e "$auth_generation_dir_check/ghapp.shipyard-context.json"
    test ! -L "$auth_generation_dir_check/ghapp.shipyard-context.json"
  fi
}}

auth_cleanup_markers() {{
  auth_cleanup_helper="$1"
  auth_cleanup_wrapper="$2"
  auth_cleanup_binary="$3"
  auth_cleanup_companion="$4"
  auth_cleanup_context="$5"
  auth_cleanup_close_guard="${{6:-}}"
  auth_cleanup_selector="$auth_cleanup_wrapper.shipyard-generation"
  /bin/rm -f "$auth_cleanup_helper.shipyard-rollback" "$auth_cleanup_helper.shipyard-was-absent"
  /bin/rm -f "$auth_cleanup_wrapper.shipyard-rollback" "$auth_cleanup_wrapper.shipyard-was-absent"
  /bin/rm -f "$auth_cleanup_binary.shipyard-rollback" "$auth_cleanup_binary.shipyard-was-absent"
  /bin/rm -f "$auth_cleanup_companion.shipyard-rollback" "$auth_cleanup_companion.shipyard-was-absent"
  /bin/rm -f "$auth_cleanup_helper.shipyard-rollback.tmp" "$auth_cleanup_wrapper.shipyard-rollback.tmp"
  /bin/rm -f "$auth_cleanup_binary.shipyard-rollback.tmp" "$auth_cleanup_companion.shipyard-rollback.tmp"
  /bin/rm -f "$auth_cleanup_selector.shipyard-rollback" "$auth_cleanup_selector.shipyard-was-absent" "$auth_cleanup_selector.shipyard-rollback.tmp"
  if [ -n "$auth_cleanup_context" ]; then /bin/rm -f "$auth_cleanup_context.shipyard-rollback" "$auth_cleanup_context.shipyard-was-absent" "$auth_cleanup_context.shipyard-rollback.tmp"; fi
  if [ -n "$auth_cleanup_close_guard" ]; then /bin/rm -f "$auth_cleanup_close_guard.shipyard-rollback" "$auth_cleanup_close_guard.shipyard-was-absent" "$auth_cleanup_close_guard.shipyard-rollback.tmp"; fi
  /bin/rm -f "$auth_journal"
}}

auth_restore_one() {{
  auth_target="$1"
  if [ -e "$auth_target.shipyard-rollback" ] || [ -L "$auth_target.shipyard-rollback" ]; then
    auth_restore_tmp="$auth_target.shipyard-rollback.tmp"
    if [ -L "$auth_target.shipyard-rollback" ]; then
      /bin/cp -P "$auth_target.shipyard-rollback" "$auth_restore_tmp"
    else
      /bin/cp -p "$auth_target.shipyard-rollback" "$auth_restore_tmp"
    fi
    /bin/mv -f "$auth_restore_tmp" "$auth_target"
  elif [ -f "$auth_target.shipyard-was-absent" ] && [ ! -L "$auth_target.shipyard-was-absent" ]; then
    /bin/rm -f "$auth_target"
  fi
}}

auth_restore_transaction() {{
  auth_restore_helper="$1"
  auth_restore_wrapper="$2"
  auth_restore_binary="$3"
  auth_restore_companion="$4"
  auth_restore_context="$5"
  auth_restore_close_guard="${{6:-}}"
  auth_restore_selector="$auth_restore_wrapper.shipyard-generation"
  # Only a sibling-guard generation is independent of compatibility
  # projections and safe to restore first. A direct wrapper or guardless v2
  # generation must remain behind the immutable anchor until the public guard
  # and the rest of its projections have been restored.
  auth_restore_wrapper_first=0
  auth_restore_public_stable=0
  if [ -L "$auth_restore_selector.shipyard-rollback" ] && \
     [ -f "$auth_restore_wrapper.shipyard-rollback" ] && [ ! -L "$auth_restore_wrapper.shipyard-rollback" ] && \
     [ "$(/usr/bin/grep -c '^# Shipyard-Stable-Public-Trampoline-Contract: stable-selector-v1$' "$auth_restore_wrapper.shipyard-rollback" || true)" = 1 ]; then
    auth_restore_public_stable=1
  fi
  auth_restore_selector_source="$auth_restore_wrapper.shipyard-rollback"
  if [ -L "$auth_restore_selector.shipyard-rollback" ]; then
    auth_restore_selector_source="$auth_restore_selector.shipyard-rollback"
  fi
  if [ -L "$auth_restore_selector_source" ]; then
    auth_restore_wrapper_target="$(/usr/bin/readlink "$auth_restore_selector_source")"
    case "$auth_restore_wrapper_target" in "$auth_generation_root"/*/ghapp) ;; *) return 1 ;; esac
    if [ "$(/usr/bin/grep -c '^# Shipyard-Sibling-Close-Guard-Contract: sibling-close-guard-v1$' "$auth_restore_wrapper_target")" = 1 ]; then
      auth_restore_wrapper_first=1
    fi
  fi
  if [ "$auth_restore_wrapper_first" = 1 ]; then
    if [ "$auth_restore_public_stable" = 0 ]; then auth_restore_one "$auth_restore_wrapper"; fi
    auth_restore_one "$auth_restore_selector"
  fi
  if [ -n "$auth_restore_context" ]; then auth_restore_one "$auth_restore_context"; fi
  if [ -n "$auth_restore_close_guard" ]; then auth_restore_one "$auth_restore_close_guard"; fi
  auth_restore_one "$auth_restore_companion"
  auth_restore_one "$auth_restore_binary"
  auth_restore_one "$auth_restore_helper"
  if [ "$auth_restore_wrapper_first" = 0 ]; then
    auth_restore_one "$auth_restore_selector"
    if [ "$auth_restore_public_stable" = 0 ]; then auth_restore_one "$auth_restore_wrapper"; fi
  fi
}}

auth_backup_record() {{
  auth_backup_label="$1"
  auth_backup_target="$2"
  if [ -z "$auth_backup_target" ]; then
    /usr/bin/printf '%s=unmanaged\n' "$auth_backup_label"
  elif [ -L "$auth_backup_target.shipyard-rollback" ]; then
    /usr/bin/printf '%s=symlink:%s\n' "$auth_backup_label" "$(/usr/bin/readlink "$auth_backup_target.shipyard-rollback")"
  elif [ -f "$auth_backup_target.shipyard-rollback" ] && [ ! -L "$auth_backup_target.shipyard-rollback" ]; then
    test "$(/usr/bin/stat -f '%u' "$auth_backup_target.shipyard-rollback")" = "$(/usr/bin/id -u)"
    /usr/bin/printf '%s=file:%s:%s\n' "$auth_backup_label" \
      "$(/usr/bin/shasum -a 256 "$auth_backup_target.shipyard-rollback" | /usr/bin/awk '{{print $1}}')" \
      "$(/usr/bin/stat -f '%Lp' "$auth_backup_target.shipyard-rollback")"
  elif [ -f "$auth_backup_target.shipyard-was-absent" ] && [ ! -L "$auth_backup_target.shipyard-was-absent" ]; then
    test "$(/usr/bin/stat -f '%u' "$auth_backup_target.shipyard-was-absent")" = "$(/usr/bin/id -u)"
    test "$(/usr/bin/stat -f '%Lp' "$auth_backup_target.shipyard-was-absent")" = 600
    /usr/bin/printf '%s=absent\n' "$auth_backup_label"
  else
    return 1
  fi
}}

auth_backup_cohort_digest_legacy() {{
  {{
    auth_backup_record helper "$1"
    auth_backup_record wrapper "$2"
    auth_backup_record binary "$3"
    auth_backup_record companion "$4"
    auth_backup_record context "$5"
    if [ -n "${{6:-}}" ]; then auth_backup_record close_guard "$6"; fi
  }} | /usr/bin/shasum -a 256 | /usr/bin/awk '{{print $1}}'
}}

auth_backup_cohort_digest() {{
  {{
    auth_backup_record helper "$1"
    auth_backup_record wrapper "$2"
    auth_backup_record binary "$3"
    auth_backup_record companion "$4"
    auth_backup_record context "$5"
    if [ -n "${{6:-}}" ]; then auth_backup_record close_guard "$6"; fi
    auth_backup_record selector "$2.shipyard-generation"
  }} | /usr/bin/shasum -a 256 | /usr/bin/awk '{{print $1}}'
}}

auth_validate_recovery_prior() {{
  test "$auth_recovery_original_backup" != pending
  if [ "$auth_recovery_selector_backed" = 1 ]; then
    auth_recovery_backup_digest="$(auth_backup_cohort_digest "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" "$auth_recovery_companion" "$auth_recovery_context" "$auth_recovery_close_guard")"
  else
    auth_recovery_backup_digest="$(auth_backup_cohort_digest_legacy "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" "$auth_recovery_companion" "$auth_recovery_context" "$auth_recovery_close_guard")"
  fi
  test "$auth_recovery_backup_digest" = "$auth_recovery_original_backup"
  if [ "$auth_recovery_original_kind" = generation ]; then
    auth_validate_recovery_generation "$auth_recovery_original_id" "$auth_recovery_original_target" "$auth_recovery_original_manifest" normal "$auth_recovery_original_authority" 0
  fi
}}

auth_write_recovery_phase() {{
  auth_recovery_phase_tmp="$(/usr/bin/mktemp "$auth_state_dir/.fleet-auth-support.recovery-phase.XXXXXX")"
  if [ "$auth_recovery_schema" = shipyard-fleet-auth-v2 ]; then
    /usr/bin/printf '%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n' \
      'shipyard-fleet-auth-v2' "$1" "$auth_recovery_authority" \
      "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" \
      "$auth_recovery_companion" "$auth_recovery_context" \
      "$auth_recovery_helper_digest" "$auth_recovery_wrapper_digest" \
      "$auth_recovery_context_digest" "$auth_recovery_companion_required" \
      "$auth_recovery_resolver_required" "$auth_recovery_original_kind" \
      "$auth_recovery_original_identity" "$auth_recovery_target_id" \
      "$auth_recovery_target_wrapper" "$auth_recovery_target_manifest" \
      "$auth_recovery_anchor_id" "$auth_recovery_anchor_wrapper" \
      "$auth_recovery_anchor_manifest" "$auth_recovery_original_manifest" \
      "$auth_recovery_original_backup" "$auth_recovery_original_authority" > "$auth_recovery_phase_tmp"
  else
    /usr/bin/printf '%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n' \
      "$auth_recovery_schema" "$1" "$auth_recovery_authority" \
      "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" \
      "$auth_recovery_companion" "$auth_recovery_context" "$auth_recovery_close_guard" \
      "$auth_recovery_helper_digest" "$auth_recovery_wrapper_digest" \
      "$auth_recovery_close_guard_digest" "$auth_recovery_context_digest" "$auth_recovery_companion_required" \
      "$auth_recovery_resolver_required" "$auth_recovery_original_kind" \
      "$auth_recovery_original_identity" "$auth_recovery_target_id" \
      "$auth_recovery_target_wrapper" "$auth_recovery_target_manifest" \
      "$auth_recovery_anchor_id" "$auth_recovery_anchor_wrapper" \
      "$auth_recovery_anchor_manifest" "$auth_recovery_original_manifest" \
      "$auth_recovery_original_backup" "$auth_recovery_original_authority" > "$auth_recovery_phase_tmp"
  fi
  /bin/chmod 600 "$auth_recovery_phase_tmp"
  /bin/mv -f "$auth_recovery_phase_tmp" "$auth_journal"
  auth_recovery_phase="$1"
}}

auth_recover_generation_transaction() {{
  auth_recovery_disposition="${{1:-auto}}"
  case "$auth_recovery_disposition" in auto|rollback|rollforward) ;; *) return 1 ;; esac
  auth_recovery_schema="$(/usr/bin/sed -n '1p' "$auth_journal")"
  auth_recovery_phase="$(/usr/bin/sed -n '2p' "$auth_journal")"
  auth_recovery_authority="$(/usr/bin/sed -n '3p' "$auth_journal")"
  auth_recovery_helper="$(/usr/bin/sed -n '4p' "$auth_journal")"
  auth_recovery_wrapper="$(/usr/bin/sed -n '5p' "$auth_journal")"
  auth_recovery_binary="$(/usr/bin/sed -n '6p' "$auth_journal")"
  auth_recovery_companion="$(/usr/bin/sed -n '7p' "$auth_journal")"
  auth_recovery_context="$(/usr/bin/sed -n '8p' "$auth_journal")"
  auth_recovery_selector_backed=0
  if [ "$auth_recovery_schema" = shipyard-fleet-auth-v3 ] || [ "$auth_recovery_schema" = shipyard-fleet-auth-v4 ]; then
    test "$(/usr/bin/wc -l < "$auth_journal" | /usr/bin/tr -d ' ')" = 26
    auth_recovery_close_guard="$(/usr/bin/sed -n '9p' "$auth_journal")"
    auth_recovery_helper_digest="$(/usr/bin/sed -n '10p' "$auth_journal")"
    auth_recovery_wrapper_digest="$(/usr/bin/sed -n '11p' "$auth_journal")"
    auth_recovery_close_guard_digest="$(/usr/bin/sed -n '12p' "$auth_journal")"
    auth_recovery_context_digest="$(/usr/bin/sed -n '13p' "$auth_journal")"
    auth_recovery_offset=2
    auth_recovery_guard_required=1
    if [ "$auth_recovery_schema" = shipyard-fleet-auth-v4 ]; then auth_recovery_selector_backed=1; fi
  else
    test "$auth_recovery_schema" = shipyard-fleet-auth-v2
    test "$(/usr/bin/wc -l < "$auth_journal" | /usr/bin/tr -d ' ')" = 24
    auth_recovery_close_guard=
    auth_recovery_close_guard_digest=absent
    auth_recovery_helper_digest="$(/usr/bin/sed -n '9p' "$auth_journal")"
    auth_recovery_wrapper_digest="$(/usr/bin/sed -n '10p' "$auth_journal")"
    auth_recovery_context_digest="$(/usr/bin/sed -n '11p' "$auth_journal")"
    auth_recovery_offset=0
    auth_recovery_guard_required=0
  fi
  auth_recovery_companion_required="$(/usr/bin/sed -n "$((12 + auth_recovery_offset))p" "$auth_journal")"
  auth_recovery_resolver_required="$(/usr/bin/sed -n "$((13 + auth_recovery_offset))p" "$auth_journal")"
  auth_recovery_original_kind="$(/usr/bin/sed -n "$((14 + auth_recovery_offset))p" "$auth_journal")"
  auth_recovery_original_identity="$(/usr/bin/sed -n "$((15 + auth_recovery_offset))p" "$auth_journal")"
  auth_recovery_target_id="$(/usr/bin/sed -n "$((16 + auth_recovery_offset))p" "$auth_journal")"
  auth_recovery_target_wrapper="$(/usr/bin/sed -n "$((17 + auth_recovery_offset))p" "$auth_journal")"
  auth_recovery_target_manifest="$(/usr/bin/sed -n "$((18 + auth_recovery_offset))p" "$auth_journal")"
  auth_recovery_anchor_id="$(/usr/bin/sed -n "$((19 + auth_recovery_offset))p" "$auth_journal")"
  auth_recovery_anchor_wrapper="$(/usr/bin/sed -n "$((20 + auth_recovery_offset))p" "$auth_journal")"
  auth_recovery_anchor_manifest="$(/usr/bin/sed -n "$((21 + auth_recovery_offset))p" "$auth_journal")"
  auth_recovery_original_manifest="$(/usr/bin/sed -n "$((22 + auth_recovery_offset))p" "$auth_journal")"
  auth_recovery_original_backup="$(/usr/bin/sed -n "$((23 + auth_recovery_offset))p" "$auth_journal")"
  auth_recovery_original_authority="$(/usr/bin/sed -n "$((24 + auth_recovery_offset))p" "$auth_journal")"
  case "$auth_recovery_phase" in preparing|prepared|generation-installed|anchor-select-intent|anchor-selected|projections-publish-intent|projections-published|target-select-intent|target-selected|validation-intent|validated|committed|rollback-intent|rollback-complete|rollforward-intent) ;; *) return 1 ;; esac
  case "$auth_recovery_companion_required:$auth_recovery_resolver_required" in 0:0|0:1|1:0|1:1) ;; *) return 1 ;; esac
  case "$auth_recovery_original_kind" in absent|direct|generation) ;; *) return 1 ;; esac
  case "$auth_recovery_authority" in ''|*[!0-9a-f]*) return 1 ;; esac
  test "${{#auth_recovery_authority}}" = 64
  for auth_recovery_hex in "$auth_recovery_target_id" "$auth_recovery_target_manifest"; do
    case "$auth_recovery_hex" in ''|*[!0-9a-f]*) return 1 ;; esac
    test "${{#auth_recovery_hex}}" = 64
  done
  test "$auth_recovery_target_wrapper" = "$auth_generation_root/$auth_recovery_target_id/ghapp"
  case "$auth_recovery_anchor_id:$auth_recovery_anchor_wrapper:$auth_recovery_anchor_manifest" in
    absent:absent:absent) ;;
    *)
      for auth_recovery_hex in "$auth_recovery_anchor_id" "$auth_recovery_anchor_manifest"; do
        case "$auth_recovery_hex" in ''|*[!0-9a-f]*) return 1 ;; esac
        test "${{#auth_recovery_hex}}" = 64
      done
      test "$auth_recovery_anchor_wrapper" = "$auth_generation_root/$auth_recovery_anchor_id/ghapp"
      ;;
  esac
  case "$auth_recovery_original_kind:$auth_recovery_original_identity" in
    absent:absent)
      test "$auth_recovery_original_manifest" = absent
      test "$auth_recovery_original_authority" = absent
      ;;
    direct:direct:*)
      test "$auth_recovery_original_manifest" = absent
      test "$auth_recovery_original_authority" = absent
      auth_recovery_original_digest="${{auth_recovery_original_identity#direct:}}"
      case "$auth_recovery_original_digest" in ''|*[!0-9a-f]*) return 1 ;; esac
      test "${{#auth_recovery_original_digest}}" = 64
      ;;
    generation:generation:*)
      auth_recovery_original_target="${{auth_recovery_original_identity#generation:}}"
      case "$auth_recovery_original_target" in "$auth_generation_root"/*/ghapp) ;; *) return 1 ;; esac
      auth_recovery_original_tail="${{auth_recovery_original_target#"$auth_generation_root"/}}"
      auth_recovery_original_id="${{auth_recovery_original_tail%%/*}}"
      case "$auth_recovery_original_id" in ''|*[!0-9a-f]*) return 1 ;; esac
      test "${{#auth_recovery_original_id}}" = 64
      test "$auth_recovery_original_tail" = "$auth_recovery_original_id/ghapp"
      case "$auth_recovery_original_manifest" in ''|*[!0-9a-f]*) return 1 ;; esac
      test "${{#auth_recovery_original_manifest}}" = 64
      case "$auth_recovery_original_authority" in ''|*[!0-9a-f]*) return 1 ;; esac
      test "${{#auth_recovery_original_authority}}" = 64
      ;;
    *) return 1 ;;
  esac
  case "$auth_recovery_original_backup" in
    pending) test "$auth_recovery_phase" = preparing ;;
    ''|*[!0-9a-f]*) return 1 ;;
    *) test "${{#auth_recovery_original_backup}}" = 64 ;;
  esac
  auth_safe_target "$auth_recovery_helper"
  auth_safe_target "$auth_recovery_wrapper"
  auth_safe_target "$auth_recovery_binary"
  auth_safe_target "$auth_recovery_companion"
  if [ "$auth_recovery_schema" = shipyard-fleet-auth-v3 ] || [ "$auth_recovery_schema" = shipyard-fleet-auth-v4 ]; then
    test "$auth_recovery_close_guard" = "$HOME/.config/shipyard/guards/pr-close-guard"
    case "$auth_recovery_close_guard_digest" in ''|*[!0-9a-f]*) return 1 ;; esac
    test "${{#auth_recovery_close_guard_digest}}" = 64
    auth_safe_target "$auth_recovery_close_guard"
  fi
  if [ "$auth_recovery_resolver_required" = 1 ]; then auth_safe_target "$auth_recovery_context"; else test -z "$auth_recovery_context"; fi
  # This observation can intentionally fail at the first-install selector-only
  # checkpoint. Suppress the inherited ERR trap inside the substitution so the
  # recovery owner retains its transaction lock while classifying that state.
  auth_recovery_live_selector="$(auth_selector_identity "$auth_recovery_wrapper" || true)"
  if [ -n "$auth_recovery_live_selector" ]; then
    :
  elif [ "$auth_recovery_selector_backed" = 1 ] && \
       [ "$auth_recovery_phase" = anchor-select-intent ] && \
       [ "$auth_recovery_original_kind" = direct ] && \
       [ -L "$auth_recovery_wrapper.shipyard-generation" ]; then
    # First migration publishes the anchor selector before replacing the
    # legacy direct wrapper with the stable trampoline. Admit exactly that
    # journaled selector-only boundary so recovery can restore the direct
    # cohort rather than stranding the transaction.
    auth_recovery_transition_target="$(/usr/bin/readlink "$auth_recovery_wrapper.shipyard-generation")"
    test "$auth_recovery_transition_target" = "$auth_recovery_anchor_wrapper"
    test -f "$auth_recovery_wrapper"; test ! -L "$auth_recovery_wrapper"
    test "direct:$(/usr/bin/shasum -a 256 "$auth_recovery_wrapper" | /usr/bin/awk '{{print $1}}')" = "$auth_recovery_original_identity"
    auth_recovery_live_selector="generation:$auth_recovery_transition_target"
  elif [ "$auth_recovery_selector_backed" = 1 ] && \
       [ "$auth_recovery_phase" = target-select-intent ] && \
       [ -L "$auth_recovery_wrapper.shipyard-generation" ]; then
    # The selector is published before the stable public trampoline so direct
    # readers never observe a trampoline without a target. A clean first
    # install can therefore be interrupted with only the authenticated target
    # selector live; recognize exactly that journaled boundary for rollback.
    auth_recovery_transition_target="$(/usr/bin/readlink "$auth_recovery_wrapper.shipyard-generation")"
    test "$auth_recovery_transition_target" = "$auth_recovery_target_wrapper"
    case "$auth_recovery_original_kind" in
      absent) test ! -e "$auth_recovery_wrapper"; test ! -L "$auth_recovery_wrapper" ;;
      direct)
        test -f "$auth_recovery_wrapper"; test ! -L "$auth_recovery_wrapper"
        test "direct:$(/usr/bin/shasum -a 256 "$auth_recovery_wrapper" | /usr/bin/awk '{{print $1}}')" = "$auth_recovery_original_identity"
        ;;
      *) return 1 ;;
    esac
    auth_recovery_live_selector="generation:$auth_recovery_transition_target"
  else
    return 1
  fi
  if [ "$auth_recovery_phase" = rollback-complete ]; then
    test "$auth_recovery_live_selector" = "$auth_recovery_original_identity"
    auth_cleanup_markers "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" "$auth_recovery_companion" "$auth_recovery_context" "$auth_recovery_close_guard"
    return 0
  fi
  if [ "$auth_recovery_phase" = preparing ]; then
    test "$auth_recovery_live_selector" = "$auth_recovery_original_identity"
    auth_cleanup_markers "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" "$auth_recovery_companion" "$auth_recovery_context" "$auth_recovery_close_guard"
    return 0
  fi
  if [ "$auth_recovery_disposition" = auto ]; then
    case "$auth_recovery_phase" in
      validated|committed|rollforward-intent) auth_recovery_disposition=rollforward ;;
      *) auth_recovery_disposition=rollback ;;
    esac
  fi
  auth_recovery_target_selector="generation:$auth_recovery_target_wrapper"
  if [ "$auth_recovery_live_selector" = "$auth_recovery_target_selector" ]; then
    auth_validate_recovery_generation "$auth_recovery_target_id" "$auth_recovery_target_wrapper" "$auth_recovery_target_manifest" normal "$auth_recovery_authority" "$auth_recovery_guard_required"
    if [ "$auth_recovery_disposition" = rollforward ]; then
      if [ "$auth_recovery_phase" != rollforward-intent ]; then auth_write_recovery_phase rollforward-intent; fi
      if [ -n "$auth_recovery_close_guard" ]; then auth_publish_link "$auth_recovery_close_guard" "${{auth_recovery_target_wrapper%/ghapp}}/pr-close-guard"; fi
      auth_publish_link "$auth_recovery_helper" "${{auth_recovery_target_wrapper%/ghapp}}/shipyard-github-app-token"
      auth_publish_link "$auth_recovery_binary" "${{auth_recovery_target_wrapper%/ghapp}}/shipyard"
      if [ "$auth_recovery_companion_required" = 1 ]; then auth_publish_link "$auth_recovery_companion" "${{auth_recovery_target_wrapper%/ghapp}}/shipyard-workstream-provider"; fi
      if [ "$auth_recovery_resolver_required" = 1 ]; then auth_publish_link "$auth_recovery_context" "${{auth_recovery_target_wrapper%/ghapp}}/ghapp.shipyard-context.json"; fi
      test "$(auth_selector_identity "$auth_recovery_wrapper")" = "$auth_recovery_target_selector"
      auth_cleanup_markers "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" "$auth_recovery_companion" "$auth_recovery_context" "$auth_recovery_close_guard"
      return 0
    fi
    if [ "$auth_recovery_phase" != rollback-intent ]; then auth_write_recovery_phase rollback-intent; fi
    auth_validate_recovery_prior
    if [ "$auth_recovery_anchor_id" != absent ]; then
      auth_validate_recovery_generation "$auth_recovery_anchor_id" "$auth_recovery_anchor_wrapper" "$auth_recovery_anchor_manifest" legacy-anchor "$auth_recovery_authority" "$auth_recovery_guard_required"
      auth_publish_generation_selection "$auth_recovery_wrapper" "$auth_recovery_anchor_wrapper"
      auth_recovery_live_selector="generation:$auth_recovery_anchor_wrapper"
    elif [ "$auth_recovery_original_kind" = direct ]; then
      return 1
    else
      auth_restore_transaction "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" "$auth_recovery_companion" "$auth_recovery_context" "$auth_recovery_close_guard"
      test "$(auth_selector_identity "$auth_recovery_wrapper")" = "$auth_recovery_original_identity"
      auth_write_recovery_phase rollback-complete
      auth_cleanup_markers "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" "$auth_recovery_companion" "$auth_recovery_context" "$auth_recovery_close_guard"
      return 0
    fi
  fi
  if [ "$auth_recovery_disposition" = rollforward ]; then return 1; fi
  if [ "$auth_recovery_phase" != rollback-intent ]; then auth_write_recovery_phase rollback-intent; fi
  if [ "$auth_recovery_anchor_id" != absent ] && [ "$auth_recovery_live_selector" = "generation:$auth_recovery_anchor_wrapper" ]; then
    auth_validate_recovery_generation "$auth_recovery_anchor_id" "$auth_recovery_anchor_wrapper" "$auth_recovery_anchor_manifest" legacy-anchor "$auth_recovery_authority" "$auth_recovery_guard_required"
  elif [ "$auth_recovery_live_selector" != "$auth_recovery_original_identity" ]; then
    return 1
  fi
  auth_validate_recovery_prior
  auth_restore_transaction "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" "$auth_recovery_companion" "$auth_recovery_context" "$auth_recovery_close_guard"
  test "$(auth_selector_identity "$auth_recovery_wrapper")" = "$auth_recovery_original_identity"
  auth_write_recovery_phase rollback-complete
  auth_cleanup_markers "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" "$auth_recovery_companion" "$auth_recovery_context" "$auth_recovery_close_guard"
}}

auth_recover() {{
  test -f "$auth_journal"
  test ! -L "$auth_journal"
  case "$(/usr/bin/sed -n '1p' "$auth_journal")" in
  shipyard-fleet-auth-v2|shipyard-fleet-auth-v3|shipyard-fleet-auth-v4)
    auth_recover_generation_transaction
    return
    ;;
  esac
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
      test "$(/usr/bin/stat -L -f '%Lp' "$auth_recovery_context")" = 600
    fi
  else
    case "$auth_phase" in preparing|prepared|generation-installed|anchor-installed|barrier-installed|wrapper-installed|projections-installed|helper-installed|auth-installed|context-installed) ;; *) return 1 ;; esac
    if [ "$auth_journal_lines" = 9 ] && [ "$auth_phase" = preparing ]; then
      # Pre-v0.131 clients copied binary rollback files directly after writing
      # `preparing`. A crash could leave partial copies while both live binaries
      # were still intact, so only the already-moved helper pair is restored.
      auth_restore_one "$auth_recovery_helper"
      auth_restore_one "$auth_recovery_wrapper"
    else
      case "$auth_phase" in
        generation-installed|anchor-installed|barrier-installed|projections-installed|wrapper-installed)
          auth_restore_transaction "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" "$auth_recovery_companion" "$auth_recovery_context"
          ;;
        *)
          if [ -n "$auth_recovery_context" ]; then auth_restore_one "$auth_recovery_context"; fi
          auth_restore_one "$auth_recovery_companion"
          auth_restore_one "$auth_recovery_binary"
          auth_restore_one "$auth_recovery_helper"
          auth_restore_one "$auth_recovery_wrapper"
          ;;
      esac
    fi
  fi
  auth_cleanup_markers "$auth_recovery_helper" "$auth_recovery_wrapper" "$auth_recovery_binary" "$auth_recovery_companion" "$auth_recovery_context"
}}

test -d "$auth_state_dir"
test ! -L "$auth_state_dir"
test "$(/usr/bin/stat -f '%u' "$auth_state_dir")" = "$(/usr/bin/id -u)"
auth_state_mode="$(/usr/bin/stat -f '%Lp' "$auth_state_dir")"
test $((8#$auth_state_mode & 8#22)) -eq 0
{lock_acquisition}
auth_generation_stage=
auth_anchor_stage=
auth_generation_created=0
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
auth_release_after_failure() {{ auth_status="$1"; trap - ERR INT TERM; if [ -n "$auth_generation_stage" ] && [ -d "$auth_generation_stage" ] && [ ! -L "$auth_generation_stage" ]; then /bin/rm -rf "$auth_generation_stage"; fi; if [ -n "$auth_anchor_stage" ] && [ -d "$auth_anchor_stage" ] && [ ! -L "$auth_anchor_stage" ]; then /bin/rm -rf "$auth_anchor_stage"; fi; auth_release_lock; exit "$auth_status"; }}
auth_release_on_error() {{ auth_release_after_failure "$?"; }}
trap auth_release_on_error ERR
trap 'auth_release_after_failure 130' INT
trap 'auth_release_after_failure 143' TERM

auth_close_guard_dir="$(/usr/bin/dirname "$auth_close_guard")"
test "$auth_close_guard_dir" = "$HOME/.config/shipyard/guards"
if [ ! -e "$auth_close_guard_dir" ] && [ ! -L "$auth_close_guard_dir" ]; then
  /bin/mkdir "$auth_close_guard_dir"
  /bin/chmod 700 "$auth_close_guard_dir"
fi
test -d "$auth_close_guard_dir"; test ! -L "$auth_close_guard_dir"
test "$(/usr/bin/stat -f '%u' "$auth_close_guard_dir")" = "$(/usr/bin/id -u)"
auth_close_guard_dir_mode="$(/usr/bin/stat -f '%Lp' "$auth_close_guard_dir")"
test $((8#$auth_close_guard_dir_mode & 8#22)) -eq 0
auth_safe_target "$auth_helper"
auth_safe_target "$auth_wrapper"
auth_safe_target "$auth_selector"
auth_safe_target "$auth_binary"
auth_safe_target "$auth_companion"
auth_safe_target "$auth_close_guard"
if [ "$auth_resolver_required" = 1 ]; then auth_safe_target "$auth_context"; fi
auth_recovery_needed=0
if [ -e "$auth_journal" ] || [ -L "$auth_journal" ]; then auth_recovery_needed=1; fi
case "$auth_recovery_needed" in 1) auth_recover ;; esac
auth_original_selector_identity="$(auth_selector_identity "$auth_wrapper")" || exit 1
auth_previous_wrapper_needs_anchor=0
auth_public_trampoline_active=0
if [ -f "$auth_wrapper" ] && [ ! -L "$auth_wrapper" ] && [ -L "$auth_selector" ]; then
  test "$(/usr/bin/grep -c '^# Shipyard-Stable-Public-Trampoline-Contract: stable-selector-v1$' "$auth_wrapper")" = 1
  test "$(/usr/bin/stat -f '%Lp' "$auth_wrapper")" = 700
  auth_public_trampoline_active=1
fi
case "$auth_original_selector_identity" in
  absent) auth_original_selector_kind=absent ;;
  direct:*) auth_original_selector_kind=direct; auth_previous_wrapper_needs_anchor=1 ;;
  generation:*)
    auth_original_selector_kind=generation
    auth_original_wrapper_target="${{auth_original_selector_identity#generation:}}"
    auth_original_generation_tail="${{auth_original_wrapper_target#"$auth_generation_root"/}}"
    auth_original_generation_id="${{auth_original_generation_tail%%/*}}"
    auth_original_manifest="$auth_generation_root/$auth_original_generation_id/generation.manifest"
    auth_original_manifest_digest="$(/usr/bin/shasum -a 256 "$auth_original_manifest" | /usr/bin/awk '{{print $1}}')"
    auth_original_authority="$(auth_manifest_value "$auth_original_manifest" authority_identity)"
    case "$auth_original_authority" in ''|*[!0-9a-f]*) exit 1 ;; esac
    test "${{#auth_original_authority}}" = 64
    auth_recovery_companion_required="$auth_companion_required"
    auth_recovery_resolver_required="$auth_resolver_required"
    auth_validate_recovery_generation "$auth_original_generation_id" "$auth_original_wrapper_target" "$auth_original_manifest_digest" normal "$auth_original_authority" 0
    if ! /usr/bin/grep -q '^close_guard_sha256=' "$auth_original_manifest" || \
       [ "$(/usr/bin/grep -c '^# Shipyard-Sibling-Close-Guard-Contract: sibling-close-guard-v1$' "$auth_original_wrapper_target")" != 1 ]; then
      auth_previous_wrapper_needs_anchor=1
    fi
    ;;
  *) exit 1 ;;
esac
for auth_target in "$auth_helper" "$auth_wrapper" "$auth_selector" "$auth_binary" "$auth_companion"; do
  test ! -e "$auth_target.shipyard-rollback"
  test ! -L "$auth_target.shipyard-rollback"
  test ! -e "$auth_target.shipyard-was-absent"
  test ! -L "$auth_target.shipyard-was-absent"
done
for auth_target in "$auth_close_guard"; do
  test ! -e "$auth_target.shipyard-rollback"
  test ! -L "$auth_target.shipyard-rollback"
  test ! -e "$auth_target.shipyard-was-absent"
  test ! -L "$auth_target.shipyard-was-absent"
  test ! -e "$auth_target.shipyard-rollback.tmp"
  test ! -L "$auth_target.shipyard-rollback.tmp"
done
test ! -e "$auth_binary.shipyard-rollback.tmp"
test ! -L "$auth_binary.shipyard-rollback.tmp"
test ! -e "$auth_companion.shipyard-rollback.tmp"
test ! -L "$auth_companion.shipyard-rollback.tmp"
test ! -e "$auth_helper.shipyard-rollback.tmp"
test ! -L "$auth_helper.shipyard-rollback.tmp"
test ! -e "$auth_wrapper.shipyard-rollback.tmp"
test ! -L "$auth_wrapper.shipyard-rollback.tmp"
test ! -e "$auth_selector.shipyard-rollback.tmp"
test ! -L "$auth_selector.shipyard-rollback.tmp"
if [ "$auth_resolver_required" = 1 ]; then
  test ! -e "$auth_context.shipyard-rollback"
  test ! -L "$auth_context.shipyard-rollback"
  test ! -e "$auth_context.shipyard-was-absent"
  test ! -L "$auth_context.shipyard-was-absent"
  test ! -e "$auth_context.shipyard-rollback.tmp"
  test ! -L "$auth_context.shipyard-rollback.tmp"
fi

for auth_generation_ancestor in "$HOME" "$HOME/.local"; do
  test -d "$auth_generation_ancestor"
  test ! -L "$auth_generation_ancestor"
  test "$(/usr/bin/stat -f '%u' "$auth_generation_ancestor")" = "$(/usr/bin/id -u)"
  auth_generation_ancestor_mode="$(/usr/bin/stat -f '%Lp' "$auth_generation_ancestor")"
  test $((8#$auth_generation_ancestor_mode & 8#22)) -eq 0
done
auth_generation_share="$HOME/.local/share"
if [ ! -e "$auth_generation_share" ] && [ ! -L "$auth_generation_share" ]; then
  /bin/mkdir "$auth_generation_share"
  /bin/chmod 755 "$auth_generation_share"
fi
test -d "$auth_generation_share"; test ! -L "$auth_generation_share"
test "$(/usr/bin/stat -f '%u' "$auth_generation_share")" = "$(/usr/bin/id -u)"
auth_generation_share_mode="$(/usr/bin/stat -f '%Lp' "$auth_generation_share")"
test $((8#$auth_generation_share_mode & 8#22)) -eq 0
auth_generation_private_root="$HOME/.local/share/shipyard"
for auth_private_root in "$auth_generation_private_root" "$auth_generation_root"; do
  if [ ! -e "$auth_private_root" ] && [ ! -L "$auth_private_root" ]; then
    /bin/mkdir "$auth_private_root"
    /bin/chmod 700 "$auth_private_root"
  fi
  test -d "$auth_private_root"; test ! -L "$auth_private_root"
  test "$(/usr/bin/stat -f '%u' "$auth_private_root")" = "$(/usr/bin/id -u)"
  test "$(/usr/bin/stat -f '%Lp' "$auth_private_root")" = 700
done
auth_generation_stage="$(/usr/bin/mktemp -d "$auth_generation_root/.stage.$auth_authority.XXXXXX")"
/bin/chmod 700 "$auth_generation_stage"
/bin/cp "$auth_helper_source" "$auth_generation_stage/shipyard-github-app-token"
/bin/cp "$auth_wrapper_source" "$auth_generation_stage/ghapp"
/bin/cp "$auth_close_guard_source" "$auth_generation_stage/pr-close-guard"
/bin/chmod 700 "$auth_generation_stage/shipyard-github-app-token" "$auth_generation_stage/ghapp" "$auth_generation_stage/pr-close-guard"
test "$(/usr/bin/grep -c '^# Shipyard-Auth-Generation-Contract: auth-selector-v2$' "$auth_generation_stage/ghapp")" = 1
auth_legacy_contract_count="$(/usr/bin/grep -c '^# Shipyard-Auth-Generation-Contract: auth-selector-v1$' "$auth_generation_stage/ghapp" || true)"
test "$auth_legacy_contract_count" = 0
test "$(/usr/bin/grep -c '^# Shipyard-Sibling-Close-Guard-Contract: sibling-close-guard-v1$' "$auth_generation_stage/ghapp")" = 1
test "$(/usr/bin/grep -c '^# Shipyard-Stable-Public-Trampoline-Contract: stable-selector-v1$' "$auth_generation_stage/ghapp")" = 1
/usr/bin/awk '{{ print }} /^# Shipyard-Stable-Public-Trampoline-END$/ {{ found=1; exit }} END {{ if (!found) exit 1 }}' "$auth_generation_stage/ghapp" > "$auth_generation_stage/ghapp.public-trampoline"
/usr/bin/printf '%s\n' 'echo "ghapp: stable public trampoline fell through" >&2' 'exit 1' >> "$auth_generation_stage/ghapp.public-trampoline"
/bin/chmod 700 "$auth_generation_stage/ghapp.public-trampoline"
{binary_install_command}
/bin/chmod 700 "$auth_generation_stage/shipyard"
if [ "$auth_companion_required" = 1 ]; then /bin/chmod 700 "$auth_generation_stage/shipyard-workstream-provider"; fi
auth_stage_helper_digest="$(/usr/bin/shasum -a 256 "$auth_generation_stage/shipyard-github-app-token" | /usr/bin/awk '{{print $1}}')"
auth_stage_wrapper_digest="$(/usr/bin/shasum -a 256 "$auth_generation_stage/ghapp" | /usr/bin/awk '{{print $1}}')"
auth_stage_trampoline_digest="$(/usr/bin/shasum -a 256 "$auth_generation_stage/ghapp.public-trampoline" | /usr/bin/awk '{{print $1}}')"
auth_stage_close_guard_digest="$(/usr/bin/shasum -a 256 "$auth_generation_stage/pr-close-guard" | /usr/bin/awk '{{print $1}}')"
auth_stage_binary_digest="$(/usr/bin/shasum -a 256 "$auth_generation_stage/shipyard" | /usr/bin/awk '{{print $1}}')"
test "$auth_stage_helper_digest" = "$auth_helper_digest"
test "$auth_stage_wrapper_digest" = "$auth_wrapper_digest"
test "$auth_stage_close_guard_digest" = "$auth_close_guard_digest"
if [ "$auth_public_trampoline_active" = 1 ]; then
  test "$(/usr/bin/shasum -a 256 "$auth_wrapper" | /usr/bin/awk '{{print $1}}')" = "$auth_stage_trampoline_digest"
fi
test -x "$auth_generation_stage/shipyard"
if [ "$auth_companion_required" = 1 ]; then
  test -x "$auth_generation_stage/shipyard-workstream-provider"
  auth_stage_companion_digest="$(/usr/bin/shasum -a 256 "$auth_generation_stage/shipyard-workstream-provider" | /usr/bin/awk '{{print $1}}')"
else
  auth_stage_companion_digest=absent
fi
auth_context_template_digest="$(/usr/bin/printf '%s' "$auth_context_json" | /usr/bin/shasum -a 256 | /usr/bin/awk '{{print $1}}')"
auth_generation_seed="$auth_generation_stage/generation.seed"
  /usr/bin/printf '%s\n' \
  'schema_version=1' \
  'generation_contract=auth-selector-v2' \
  "authority_identity=$auth_authority" \
  "helper_sha256=$auth_stage_helper_digest" \
  "wrapper_sha256=$auth_stage_wrapper_digest" \
  "public_trampoline_sha256=$auth_stage_trampoline_digest" \
  "close_guard_sha256=$auth_stage_close_guard_digest" \
  "binary_sha256=$auth_stage_binary_digest" \
  "companion_sha256=$auth_stage_companion_digest" \
  "context_template_sha256=$auth_context_template_digest" > "$auth_generation_seed"
/bin/chmod 600 "$auth_generation_seed"
auth_generation_id="$(/usr/bin/shasum -a 256 "$auth_generation_seed" | /usr/bin/awk '{{print $1}}')"
case "$auth_generation_id" in ''|*[!0-9a-f]*) exit 1 ;; esac
test "${{#auth_generation_id}}" = 64
auth_generation="$auth_generation_root/$auth_generation_id"
auth_context_digest=absent
if [ "$auth_resolver_required" = 1 ]; then
  /usr/bin/printf '%s\n' "${{auth_context_json/__SHIPYARD_AUTH_GENERATION__/$auth_generation_id}}" > "$auth_generation_stage/ghapp.shipyard-context.json"
  /bin/chmod 600 "$auth_generation_stage/ghapp.shipyard-context.json"
  auth_context_digest="$(/usr/bin/shasum -a 256 "$auth_generation_stage/ghapp.shipyard-context.json" | /usr/bin/awk '{{print $1}}')"
fi
auth_generation_manifest="$auth_generation_stage/generation.manifest"
/usr/bin/printf '%s\n' \
  'schema_version=1' \
  'generation_contract=auth-selector-v2' \
  "generation_id=$auth_generation_id" \
  "authority_identity=$auth_authority" \
  "helper_sha256=$auth_stage_helper_digest" \
  'helper_mode=700' \
  "wrapper_sha256=$auth_stage_wrapper_digest" \
  'wrapper_mode=700' \
  "public_trampoline_sha256=$auth_stage_trampoline_digest" \
  'public_trampoline_mode=700' \
  "close_guard_sha256=$auth_stage_close_guard_digest" \
  'close_guard_mode=700' \
  "binary_sha256=$auth_stage_binary_digest" \
  'binary_mode=700' \
  "companion_sha256=$auth_stage_companion_digest" \
  "context_sha256=$auth_context_digest" \
  "context_template_sha256=$auth_context_template_digest" > "$auth_generation_manifest"
/bin/chmod 600 "$auth_generation_manifest"
auth_target_generation_id="$auth_generation_id"
auth_target_wrapper_target="$auth_generation/ghapp"
auth_target_manifest_digest="$(/usr/bin/shasum -a 256 "$auth_generation_manifest" | /usr/bin/awk '{{print $1}}')"
auth_write_phase preparing

auth_backup_one() {{
  auth_target="$1"
  if [ -e "$auth_target" ] || [ -L "$auth_target" ]; then
    auth_backup_tmp="$auth_target.shipyard-rollback.tmp"
    if [ -L "$auth_target" ]; then /bin/cp -P "$auth_target" "$auth_backup_tmp"; else /bin/cp -p "$auth_target" "$auth_backup_tmp"; fi
    /bin/mv "$auth_backup_tmp" "$auth_target.shipyard-rollback"
  else
    /usr/bin/touch "$auth_target.shipyard-was-absent"
    /bin/chmod 600 "$auth_target.shipyard-was-absent"
  fi
}}

auth_rollback_after_failure() {{
  auth_status="$1"
  trap - ERR INT TERM
  if [ -f "$auth_journal" ]; then
    case "$(/usr/bin/sed -n '1p' "$auth_journal")" in
      shipyard-fleet-auth-v2|shipyard-fleet-auth-v3|shipyard-fleet-auth-v4) auth_recover_generation_transaction rollback ;;
      *) auth_restore_transaction "$auth_helper" "$auth_wrapper" "$auth_binary" "$auth_companion" "$auth_context" "$auth_close_guard"; auth_cleanup_markers "$auth_helper" "$auth_wrapper" "$auth_binary" "$auth_companion" "$auth_context" "$auth_close_guard" ;;
    esac
  else
    auth_restore_transaction "$auth_helper" "$auth_wrapper" "$auth_binary" "$auth_companion" "$auth_context" "$auth_close_guard"
    auth_cleanup_markers "$auth_helper" "$auth_wrapper" "$auth_binary" "$auth_companion" "$auth_context" "$auth_close_guard"
  fi
  if [ -n "${{auth_generation_stage:-}}" ] && [ -d "$auth_generation_stage" ] && [ ! -L "$auth_generation_stage" ]; then /bin/rm -rf "$auth_generation_stage"; fi
  # Never remove a published generation here. A reader may already have
  # resolved its wrapper before the selector is rolled back.
  auth_release_lock
  exit "$auth_status"
}}
auth_rollback_on_error() {{ auth_rollback_after_failure "$?"; }}
trap auth_rollback_on_error ERR
trap 'auth_rollback_after_failure 130' INT
trap 'auth_rollback_after_failure 143' TERM

auth_backup_one "$auth_helper"
auth_backup_one "$auth_wrapper"
auth_backup_one "$auth_selector"
auth_backup_one "$auth_binary"
auth_backup_one "$auth_companion"
if [ "$auth_resolver_required" = 1 ]; then auth_backup_one "$auth_context"; fi
auth_backup_one "$auth_close_guard"
auth_original_backup_digest="$(auth_backup_cohort_digest "$auth_helper" "$auth_wrapper" "$auth_binary" "$auth_companion" "$auth_context" "$auth_close_guard")"
case "$auth_original_backup_digest" in ''|*[!0-9a-f]*) exit 1 ;; esac
test "${{#auth_original_backup_digest}}" = 64
auth_write_phase prepared
if [ -e "$auth_generation" ] || [ -L "$auth_generation" ]; then
  test -d "$auth_generation"
  test ! -L "$auth_generation"
  test "$(/usr/bin/stat -f '%u' "$auth_generation")" = "$(/usr/bin/id -u)"
  test "$(/usr/bin/stat -f '%Lp' "$auth_generation")" = 700
  /usr/bin/cmp -s "$auth_generation/generation.seed" "$auth_generation_stage/generation.seed"
  /usr/bin/cmp -s "$auth_generation/generation.manifest" "$auth_generation_stage/generation.manifest"
  for auth_member in shipyard-github-app-token ghapp ghapp.public-trampoline pr-close-guard shipyard generation.seed generation.manifest; do
    test -f "$auth_generation/$auth_member"
    test ! -L "$auth_generation/$auth_member"
    test "$(/usr/bin/stat -f '%u' "$auth_generation/$auth_member")" = "$(/usr/bin/id -u)"
    case "$auth_member" in generation.seed|generation.manifest) auth_member_mode=600 ;; *) auth_member_mode=700 ;; esac
    test "$(/usr/bin/stat -f '%Lp' "$auth_generation/$auth_member")" = "$auth_member_mode"
    /usr/bin/cmp -s "$auth_generation/$auth_member" "$auth_generation_stage/$auth_member"
  done
  if [ "$auth_companion_required" = 1 ]; then
    test -f "$auth_generation/shipyard-workstream-provider"
    test ! -L "$auth_generation/shipyard-workstream-provider"
    test "$(/usr/bin/stat -f '%u' "$auth_generation/shipyard-workstream-provider")" = "$(/usr/bin/id -u)"
    test "$(/usr/bin/stat -f '%Lp' "$auth_generation/shipyard-workstream-provider")" = 700
    /usr/bin/cmp -s "$auth_generation/shipyard-workstream-provider" "$auth_generation_stage/shipyard-workstream-provider"
  else
    test ! -e "$auth_generation/shipyard-workstream-provider"
    test ! -L "$auth_generation/shipyard-workstream-provider"
  fi
  if [ "$auth_resolver_required" = 1 ]; then
    test -f "$auth_generation/ghapp.shipyard-context.json"
    test ! -L "$auth_generation/ghapp.shipyard-context.json"
    test "$(/usr/bin/stat -f '%u' "$auth_generation/ghapp.shipyard-context.json")" = "$(/usr/bin/id -u)"
    test "$(/usr/bin/stat -f '%Lp' "$auth_generation/ghapp.shipyard-context.json")" = 600
    /usr/bin/cmp -s "$auth_generation/ghapp.shipyard-context.json" "$auth_generation_stage/ghapp.shipyard-context.json"
  else
    test ! -e "$auth_generation/ghapp.shipyard-context.json"
    test ! -L "$auth_generation/ghapp.shipyard-context.json"
  fi
  /bin/rm -rf "$auth_generation_stage"
else
  /bin/mv "$auth_generation_stage" "$auth_generation"
  auth_generation_created=1
fi
auth_generation_stage=
auth_write_phase generation-installed
{injected_failure}
if [ "$auth_previous_wrapper_needs_anchor" = 1 ]; then
  # Direct wrappers and pre-sibling-guard generations can read mutable public
  # projections. Select a release-matched immutable bridge before enumerating
  # those readers, then drain the finite old cohort before projections move.
  auth_anchor_stage="$(/usr/bin/mktemp -d "$auth_generation_root/.anchor.$auth_authority.XXXXXX")"
  /bin/chmod 700 "$auth_anchor_stage"
  /bin/cp "$auth_generation/ghapp" "$auth_anchor_stage/ghapp"
  /bin/cp "$auth_generation/ghapp.public-trampoline" "$auth_anchor_stage/ghapp.public-trampoline"
  /bin/cp "$auth_generation/pr-close-guard" "$auth_anchor_stage/pr-close-guard"
  /bin/cp "$auth_generation/shipyard" "$auth_anchor_stage/shipyard"
  /bin/cp "$auth_helper" "$auth_anchor_stage/shipyard-github-app-token"
  /bin/chmod 700 "$auth_anchor_stage/ghapp" "$auth_anchor_stage/ghapp.public-trampoline" "$auth_anchor_stage/pr-close-guard" "$auth_anchor_stage/shipyard" "$auth_anchor_stage/shipyard-github-app-token"
  if [ "$auth_companion_required" = 1 ]; then
    /bin/cp "$auth_generation/shipyard-workstream-provider" "$auth_anchor_stage/shipyard-workstream-provider"
    /bin/chmod 700 "$auth_anchor_stage/shipyard-workstream-provider"
  fi
  auth_anchor_helper_digest="$(/usr/bin/shasum -a 256 "$auth_anchor_stage/shipyard-github-app-token" | /usr/bin/awk '{{print $1}}')"
  auth_anchor_seed="$auth_anchor_stage/generation.seed"
  /usr/bin/printf '%s\n' \
    'schema_version=1' \
    'generation_contract=auth-selector-v2' \
    'generation_kind=legacy-anchor' \
    "authority_identity=$auth_authority" \
    "helper_sha256=$auth_anchor_helper_digest" \
    "wrapper_sha256=$auth_stage_wrapper_digest" \
    "public_trampoline_sha256=$auth_stage_trampoline_digest" \
    "close_guard_sha256=$auth_stage_close_guard_digest" \
    "binary_sha256=$auth_stage_binary_digest" \
    "companion_sha256=$auth_stage_companion_digest" \
    "context_template_sha256=$auth_context_template_digest" > "$auth_anchor_seed"
  /bin/chmod 600 "$auth_anchor_seed"
  auth_anchor_id="$(/usr/bin/shasum -a 256 "$auth_anchor_seed" | /usr/bin/awk '{{print $1}}')"
  case "$auth_anchor_id" in ''|*[!0-9a-f]*) exit 1 ;; esac
  test "${{#auth_anchor_id}}" = 64
  auth_anchor="$auth_generation_root/$auth_anchor_id"
  auth_anchor_context_digest=absent
  if [ "$auth_resolver_required" = 1 ]; then
    /usr/bin/printf '%s\n' "${{auth_context_json/__SHIPYARD_AUTH_GENERATION__/$auth_anchor_id}}" > "$auth_anchor_stage/ghapp.shipyard-context.json"
    /bin/chmod 600 "$auth_anchor_stage/ghapp.shipyard-context.json"
    auth_anchor_context_digest="$(/usr/bin/shasum -a 256 "$auth_anchor_stage/ghapp.shipyard-context.json" | /usr/bin/awk '{{print $1}}')"
  fi
  /usr/bin/printf '%s\n' \
    'schema_version=1' \
    'generation_contract=auth-selector-v2' \
    'generation_kind=legacy-anchor' \
    "generation_id=$auth_anchor_id" \
    "authority_identity=$auth_authority" \
    "helper_sha256=$auth_anchor_helper_digest" \
    'helper_mode=700' \
    "wrapper_sha256=$auth_stage_wrapper_digest" \
    'wrapper_mode=700' \
    "public_trampoline_sha256=$auth_stage_trampoline_digest" \
    'public_trampoline_mode=700' \
    "close_guard_sha256=$auth_stage_close_guard_digest" \
    'close_guard_mode=700' \
    "binary_sha256=$auth_stage_binary_digest" \
    'binary_mode=700' \
    "companion_sha256=$auth_stage_companion_digest" \
    "context_sha256=$auth_anchor_context_digest" \
    "context_template_sha256=$auth_context_template_digest" > "$auth_anchor_stage/generation.manifest"
  /bin/chmod 600 "$auth_anchor_stage/generation.manifest"
  auth_anchor_wrapper_target="$auth_anchor/ghapp"
  auth_anchor_manifest_digest="$(/usr/bin/shasum -a 256 "$auth_anchor_stage/generation.manifest" | /usr/bin/awk '{{print $1}}')"
  if [ -e "$auth_anchor" ] || [ -L "$auth_anchor" ]; then
    test -d "$auth_anchor"
    test ! -L "$auth_anchor"
    test "$(/usr/bin/stat -f '%u' "$auth_anchor")" = "$(/usr/bin/id -u)"
    test "$(/usr/bin/stat -f '%Lp' "$auth_anchor")" = 700
    /usr/bin/cmp -s "$auth_anchor/generation.seed" "$auth_anchor_stage/generation.seed"
    /usr/bin/cmp -s "$auth_anchor/generation.manifest" "$auth_anchor_stage/generation.manifest"
    for auth_member in generation.seed generation.manifest; do
      test -f "$auth_anchor/$auth_member"
      test ! -L "$auth_anchor/$auth_member"
      test "$(/usr/bin/stat -f '%u' "$auth_anchor/$auth_member")" = "$(/usr/bin/id -u)"
      test "$(/usr/bin/stat -f '%Lp' "$auth_anchor/$auth_member")" = 600
    done
    for auth_member in shipyard-github-app-token ghapp ghapp.public-trampoline pr-close-guard shipyard; do
      test -f "$auth_anchor/$auth_member"
      test ! -L "$auth_anchor/$auth_member"
      test "$(/usr/bin/stat -f '%u' "$auth_anchor/$auth_member")" = "$(/usr/bin/id -u)"
      test "$(/usr/bin/stat -f '%Lp' "$auth_anchor/$auth_member")" = 700
      /usr/bin/cmp -s "$auth_anchor/$auth_member" "$auth_anchor_stage/$auth_member"
    done
    if [ "$auth_companion_required" = 1 ]; then
      test -f "$auth_anchor/shipyard-workstream-provider"
      test ! -L "$auth_anchor/shipyard-workstream-provider"
      test "$(/usr/bin/stat -f '%u' "$auth_anchor/shipyard-workstream-provider")" = "$(/usr/bin/id -u)"
      test "$(/usr/bin/stat -f '%Lp' "$auth_anchor/shipyard-workstream-provider")" = 700
      /usr/bin/cmp -s "$auth_anchor/shipyard-workstream-provider" "$auth_anchor_stage/shipyard-workstream-provider"
    else
      test ! -e "$auth_anchor/shipyard-workstream-provider"
      test ! -L "$auth_anchor/shipyard-workstream-provider"
    fi
    if [ "$auth_resolver_required" = 1 ]; then
      test -f "$auth_anchor/ghapp.shipyard-context.json"
      test ! -L "$auth_anchor/ghapp.shipyard-context.json"
      test "$(/usr/bin/stat -f '%u' "$auth_anchor/ghapp.shipyard-context.json")" = "$(/usr/bin/id -u)"
      test "$(/usr/bin/stat -f '%Lp' "$auth_anchor/ghapp.shipyard-context.json")" = 600
      /usr/bin/cmp -s "$auth_anchor/ghapp.shipyard-context.json" "$auth_anchor_stage/ghapp.shipyard-context.json"
    else
      test ! -e "$auth_anchor/ghapp.shipyard-context.json"
      test ! -L "$auth_anchor/ghapp.shipyard-context.json"
    fi
    /bin/rm -rf "$auth_anchor_stage"
  else
    /bin/mv "$auth_anchor_stage" "$auth_anchor"
  fi
  auth_anchor_stage=
  auth_write_phase anchor-select-intent
  # Publish the immutable selector before the stable regular-file trampoline.
  # A legacy reader may already have opened the direct wrapper while this
  # transaction replaces its pathname. Keeping the public entrypoint regular
  # avoids making that reader's opened pathname fail the wrapper's no-symlink
  # identity check; new readers immediately route through the anchor.
  auth_publish_link "$auth_selector" "$auth_anchor/ghapp"
  auth_publish_file "$auth_wrapper" "$auth_generation/ghapp.public-trampoline"
  auth_public_trampoline_active=1
  auth_write_phase anchor-selected
  auth_old_reader_cohort=
  # A direct-wrapper exec can cross the atomic selector rename before it is
  # visible to one ps snapshot. Observe a bounded post-selector quiescence
  # window and fence the union of exact PID/start identities before projections
  # move. New readers resolve the immutable anchor and cannot join this cohort.
  for auth_observation_round in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    auth_old_reader_pids="$(/bin/ps -axo pid=,command= | /usr/bin/awk -v wrapper="$auth_wrapper" '$2 == "/bin/bash" && $3 == wrapper {{print $1}}')"
    for auth_reader_pid in $auth_old_reader_pids; do
      case "$auth_reader_pid" in ''|*[!0-9]*) exit 1 ;; esac
      auth_reader_started="$(/bin/ps -p "$auth_reader_pid" -o lstart= 2>/dev/null || true)"
      if [ -n "$auth_reader_started" ]; then
        auth_reader_start_digest="$(/usr/bin/printf '%s' "$auth_reader_started" | /usr/bin/shasum -a 256 | /usr/bin/awk '{{print $1}}')"
        auth_reader_identity="$auth_reader_pid:$auth_reader_start_digest"
        case " $auth_old_reader_cohort " in
          *" $auth_reader_identity "*) ;;
          *) auth_old_reader_cohort="$auth_old_reader_cohort $auth_reader_identity" ;;
        esac
      fi
    done
    /bin/sleep 0.05
  done
  auth_reader_deadline=$(( $(/bin/date +%s) + 30 ))
  while [ -n "${{auth_old_reader_cohort# }}" ]; do
    auth_reader_remaining=
    for auth_reader_identity in $auth_old_reader_cohort; do
      auth_reader_pid="${{auth_reader_identity%%:*}}"
      auth_reader_expected_start="${{auth_reader_identity#*:}}"
      case "$auth_reader_pid:$auth_reader_expected_start" in *[!0-9a-f:]*) exit 1 ;; esac
      test "${{#auth_reader_expected_start}}" = 64
      if /bin/kill -0 "$auth_reader_pid" 2>/dev/null; then
        auth_reader_started="$(/bin/ps -p "$auth_reader_pid" -o lstart= 2>/dev/null || true)"
        if [ -n "$auth_reader_started" ]; then
          auth_reader_current_start="$(/usr/bin/printf '%s' "$auth_reader_started" | /usr/bin/shasum -a 256 | /usr/bin/awk '{{print $1}}')"
          if [ "$auth_reader_current_start" = "$auth_reader_expected_start" ]; then
            auth_reader_remaining="$auth_reader_remaining $auth_reader_identity"
          fi
        fi
      fi
    done
    auth_old_reader_cohort="$auth_reader_remaining"
    test "$(/bin/date +%s)" -lt "$auth_reader_deadline"
    /bin/sleep 0.05
  done
fi
auth_write_phase projections-publish-intent
auth_publish_link "$auth_close_guard" "$auth_generation/pr-close-guard"
auth_publish_link "$auth_helper" "$auth_generation/shipyard-github-app-token"
auth_publish_link "$auth_binary" "$auth_generation/shipyard"
if [ "$auth_companion_required" = 1 ]; then auth_publish_link "$auth_companion" "$auth_generation/shipyard-workstream-provider"; fi
if [ "$auth_resolver_required" = 1 ]; then
  auth_publish_link "$auth_context" "$auth_generation/ghapp.shipyard-context.json"
fi
auth_write_phase projections-published
auth_write_phase target-select-intent
auth_publish_link "$auth_selector" "$auth_generation/ghapp"
if [ "$auth_public_trampoline_active" = 0 ]; then auth_publish_file "$auth_wrapper" "$auth_generation/ghapp.public-trampoline"; fi
auth_write_phase target-selected
auth_write_phase validation-intent
test "$(/usr/bin/shasum -a 256 "$auth_helper" | /usr/bin/awk '{{print $1}}')" = "$auth_helper_digest"
test "$(/usr/bin/shasum -a 256 "$(/usr/bin/readlink "$auth_selector")" | /usr/bin/awk '{{print $1}}')" = "$auth_wrapper_digest"
test "$(/usr/bin/shasum -a 256 "$auth_wrapper" | /usr/bin/awk '{{print $1}}')" = "$auth_stage_trampoline_digest"
test "$(/usr/bin/shasum -a 256 "$auth_close_guard" | /usr/bin/awk '{{print $1}}')" = "$auth_close_guard_digest"
test "$(/usr/bin/stat -L -f '%Lp' "$auth_helper")" = 700
test "$(/usr/bin/stat -L -f '%Lp' "$auth_wrapper")" = 700
test "$(/usr/bin/stat -L -f '%Lp' "$auth_close_guard")" = 700
test -x "$auth_binary"
if [ "$auth_companion_required" = 1 ]; then test -x "$auth_companion"; fi
if [ "$auth_resolver_required" = 1 ]; then
  test "$(/usr/bin/shasum -a 256 "$auth_context" | /usr/bin/awk '{{print $1}}')" = "$auth_context_digest"
  test "$(/usr/bin/stat -L -f '%Lp' "$auth_context")" = 600
  "$auth_binary" --mode "$auth_mode" --global-dir "$auth_global_dir" auth helper-argv --wrapper "$auth_wrapper" --repo "$auth_probe_repo" >/dev/null
fi
auth_write_phase validated
auth_write_phase committed
auth_generation_created=0
trap - ERR INT TERM
auth_cleanup_markers "$auth_helper" "$auth_wrapper" "$auth_binary" "$auth_companion" "$auth_context" "$auth_close_guard"
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
