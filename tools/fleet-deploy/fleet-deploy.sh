#!/bin/bash
# Deploy merged fleet scripts to this host.
#
# Units:
#   tick         Shipyard tools/steward-tick/shipyard-steward-tick.sh -> FLEET_DEPLOY_TICK_DEST
#   deploy       Shipyard tools/fleet-deploy/fleet-deploy.sh          -> FLEET_DEPLOY_SELF_DEST
#   pass-runner  fleet-handbook scripts/pass-runner, through its own scripts/install-pass-runner.sh
#   launcher     a staged, signed ShipyardLauncher.app      -> FLEET_DEPLOY_LAUNCHER_DEST,
#                then activated once the owner's Full Disk Access grant is in place
#
# These scripts used to be copied to a host by hand, so a merged fix could sit
# undeployed with nothing noticing (2026-11-12: m3 ran a steward tick from
# 11-09 and a reviewer runner missing three merged fixes). The steward tick
# calls this at its start; it acts at most every FLEET_DEPLOY_INTERVAL_S.
#
# Each unit installs only when its source on main differs from what is
# installed, only after it validates, atomically (temp file and mv, so a
# running script keeps reading the old file), with a dated backup. A unit that
# fails validation or install is left as it was and logged as refused on
# every run until main is fixed, so drift is visible in the tick summary.
# This script never fails its caller and exits 0.
#
# Off switch: SHIPYARD_FLEET_DEPLOY=off.
set -uo pipefail
[ "${SHIPYARD_FLEET_DEPLOY:-on}" = off ] && exit 0

root="${FLEET_DEPLOY_ROOT:-$HOME/.local/share/shipyard-steward/deploy}"
log="${FLEET_DEPLOY_LOG:-$HOME/Library/Logs/shipyard-steward-tick/summary.log}"
interval="${FLEET_DEPLOY_INTERVAL_S:-600}"
shipyard_remote="${FLEET_DEPLOY_SHIPYARD_REMOTE:-git@github.com:danielraffel/Shipyard.git}"
handbook_remote="${FLEET_DEPLOY_HANDBOOK_REMOTE:-git@github.com:danielraffel/fleet-handbook.git}"
tick_dest="${FLEET_DEPLOY_TICK_DEST:-$HOME/.local/libexec/shipyard-steward-tick.sh}"
self_dest="${FLEET_DEPLOY_SELF_DEST:-$HOME/.local/libexec/shipyard-fleet-deploy.sh}"
keep_backups="${FLEET_DEPLOY_KEEP_BACKUPS:-5}"
launcher_staged="${FLEET_DEPLOY_LAUNCHER_STAGED:-$HOME/.local/share/shipyard/launcher/staged/ShipyardLauncher.app}"
launcher_dest="${FLEET_DEPLOY_LAUNCHER_DEST:-$HOME/.local/libexec/ShipyardLauncher.app}"
export GIT_SSH_COMMAND="${GIT_SSH_COMMAND:-ssh -o BatchMode=yes -o ConnectTimeout=15}"
stamp=$(date -u +%Y%m%dT%H%M%SZ)

say() { echo "$stamp fleet-deploy $*" >> "$log" 2>/dev/null || true; }

mkdir -p "$root" 2>/dev/null || exit 0
now=$(date +%s)
last=$(cat "$root/last-run" 2>/dev/null || echo 0)
case "$last" in ''|*[!0-9]*) last=0 ;; esac
[ $((now - last)) -lt "$interval" ] && exit 0
mkdir "$root/lock" 2>/dev/null || exit 0
trap 'rmdir "$root/lock" 2>/dev/null' EXIT
echo "$now" > "$root/last-run"

# sync_clone NAME REMOTE: a private clone at origin/main, detached. Prints HEAD.
sync_clone() {
  local dir="$root/$1"
  if [ ! -d "$dir/.git" ]; then
    git clone -q "$2" "$dir" >/dev/null 2>&1 || return 1
  fi
  git -C "$dir" fetch -q origin main >/dev/null 2>&1 || return 1
  git -C "$dir" checkout -q --detach --force origin/main >/dev/null 2>&1 || return 1
  git -C "$dir" rev-parse --short=12 HEAD
}

prune_backups() {
  local dest=$1 n=0 f
  for f in $(ls -1t "$dest".bak-* 2>/dev/null); do
    n=$((n + 1))
    [ "$n" -gt "$keep_backups" ] && rm -f "$f"
  done
}

# install_file UNIT SRC DEST REV: bash -n, then atomic replace with a backup.
install_file() {
  local unit=$1 src=$2 dest=$3 rev=$4 tmp
  if [ ! -f "$src" ]; then say "unit=$unit refused rev=$rev reason=source_missing"; return 0; fi
  if [ -f "$dest" ] && cmp -s "$src" "$dest"; then return 0; fi
  if ! bash -n "$src" 2>/dev/null; then say "unit=$unit refused rev=$rev reason=syntax"; return 0; fi
  mkdir -p "$(dirname "$dest")" 2>/dev/null || { say "unit=$unit refused rev=$rev reason=dest_dir"; return 0; }
  tmp="$dest.tmp.$$"
  if ! cp "$src" "$tmp" || ! chmod 0755 "$tmp"; then
    rm -f "$tmp"; say "unit=$unit refused rev=$rev reason=copy"; return 0
  fi
  [ -f "$dest" ] && cp -p "$dest" "$dest.bak-$stamp" 2>/dev/null
  if ! mv -f "$tmp" "$dest"; then
    rm -f "$tmp"; say "unit=$unit refused rev=$rev reason=install"; return 0
  fi
  prune_backups "$dest"
  say "unit=$unit installed rev=$rev dest=$dest"
}

# bounded SECONDS CMD...: CMD's stdout and exit status; killed and exit 124
# after SECONDS, so a hung launcher or check can never hold the deploy lock.
bounded() {
  /usr/bin/python3 -c '
import subprocess, sys
try:
    run = subprocess.run(sys.argv[2:], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                         stdin=subprocess.DEVNULL, timeout=float(sys.argv[1]))
except subprocess.TimeoutExpired:
    sys.exit(124)
except OSError:
    sys.exit(126)
sys.stdout.buffer.write(run.stdout)
sys.exit(run.returncode)
' "$@"
}
launcher_timeout="${FLEET_DEPLOY_LAUNCHER_TIMEOUT_S:-45}"
launcher_floor="$root/launcher.version-floor"

# launcher_version BUNDLE: its integer CFBundleVersion, or nothing.
launcher_version() {
  bounded "$launcher_timeout" /usr/bin/python3 "$root/Shipyard/tools/shipyard-launcher/shipyard-launcher-check" --bundle-version "$1"
}

# say_once UNIT LINE: log a standing state (nothing staged, already current, a
# refusal) once when it changes, not on every run.
say_once() {
  local file="$root/$1.state"
  [ "$(cat "$file" 2>/dev/null)" = "$2" ] && return 0
  printf '%s\n' "$2" > "$file"
  say "$2"
}

# install_launcher REV: ShipyardLauncher.app (tools/shipyard-launcher/README.md).
# It is built, signed and notarized on the maintainer's Mac, never here, and
# staged on this host by hand. A staged bundle is installed only when
# shipyard-launcher-check --verify-bundle accepts it: Developer ID signed by
# the pinned team, with the launcher's bundle identifier, and
# notarized. The copy is verified again before it replaces the installed
# bundle, and the previous bundle is kept as <dest>.bak-<stamp>. Installing
# alone never routes anything: activate_launcher below activates only once the
# owner's Full Disk Access grant is in place.
install_launcher() {
  local rev=$1 check="$root/Shipyard/tools/shipyard-launcher/shipyard-launcher-check" tmp out
  [ "$(uname -s)" = Darwin ] || return 0
  if [ ! -d "$launcher_staged" ]; then
    say_once launcher "unit=launcher absent staged=$launcher_staged"
    return 0
  fi
  if [ -d "$launcher_dest" ] && diff -rq "$launcher_staged" "$launcher_dest" >/dev/null 2>&1; then
    say_once launcher "unit=launcher current dest=$launcher_dest"
    return 0
  fi
  if ! out=$(bounded 180 /usr/bin/python3 "$check" --verify-bundle "$launcher_staged"); then
    say_once launcher "unit=launcher refused rev=$rev reason=$(printf '%s' "$out" | head -c 200 | tr '\n' ' ')"
    return 0
  fi
  # Never downgrade: the staged CFBundleVersion must be at least the installed
  # one and the highest this host ever installed (launcher.version-floor).
  local staged_v installed_v floor
  staged_v=$(launcher_version "$launcher_staged") || staged_v=""
  floor=$(cat "$launcher_floor" 2>/dev/null || echo 0)
  case "$floor" in ''|*[!0-9]*) floor=0 ;; esac
  if [ -d "$launcher_dest" ]; then
    installed_v=$(launcher_version "$launcher_dest") || installed_v=""
    case "$installed_v" in ''|*[!0-9]*) ;; *) [ "$installed_v" -gt "$floor" ] && floor=$installed_v ;; esac
  fi
  case "$staged_v" in
    ''|*[!0-9]*) say_once launcher "unit=launcher refused rev=$rev reason=staged_version_unreadable"; return 0 ;;
  esac
  if [ "$staged_v" -lt "$floor" ]; then
    say_once launcher "unit=launcher refused rev=$rev reason=downgrade staged=$staged_v installed_floor=$floor"
    return 0
  fi
  tmp="$launcher_dest.tmp.$$"
  if ! mkdir -p "$(dirname "$launcher_dest")" || ! ditto "$launcher_staged" "$tmp" \
    || ! bounded 180 /usr/bin/python3 "$check" --verify-bundle "$tmp" >/dev/null 2>&1; then
    rm -rf "$tmp"; say "unit=launcher refused rev=$rev reason=copy"; return 0
  fi
  if [ -d "$launcher_dest" ] && ! mv "$launcher_dest" "$launcher_dest.bak-$stamp"; then
    rm -rf "$tmp"; say "unit=launcher refused rev=$rev reason=backup"; return 0
  fi
  if ! mv "$tmp" "$launcher_dest"; then
    [ -d "$launcher_dest.bak-$stamp" ] && mv "$launcher_dest.bak-$stamp" "$launcher_dest"
    rm -rf "$tmp"; say "unit=launcher refused rev=$rev reason=install"; return 0
  fi
  local n=0 old
  for old in $(ls -1dt "$launcher_dest".bak-* 2>/dev/null); do
    n=$((n + 1))
    [ "$n" -gt "$keep_backups" ] && rm -rf "$old"
  done
  printf '%s\n' "$staged_v" > "$launcher_floor"
  say "unit=launcher installed rev=$rev version=$staged_v dest=$launcher_dest"
  printf '%s\n' "unit=launcher current dest=$launcher_dest" > "$root/launcher.state"
}

# activate_launcher: when the installed launcher is not active, try
# shipyard-launcher-check --activate. It writes the activation record only
# when the bundle verifies and the launcher already holds Full Disk Access
# (the owner's one-time grant); checking the grant never prompts. So a new or
# reinstalled bundle (a new cdhash) becomes active without anyone running a
# command, and a host without the grant just logs that it waits for it.
# SHIPYARD_LAUNCHER_AUTO_ACTIVATE=off turns it off.
activate_launcher() {
  local check="$root/Shipyard/tools/shipyard-launcher/shipyard-launcher-check"
  local exe="$launcher_dest/Contents/MacOS/shipyard-launcher" out why
  [ "$(uname -s)" = Darwin ] || return 0
  [ "${SHIPYARD_LAUNCHER_AUTO_ACTIVATE:-on}" = off ] && return 0
  [ -x "$exe" ] || return 0
  # Never activate a bundle older than the highest version this host installed.
  local installed_v floor
  installed_v=$(launcher_version "$launcher_dest") || installed_v=""
  floor=$(cat "$launcher_floor" 2>/dev/null || echo 0)
  case "$floor" in ''|*[!0-9]*) floor=0 ;; esac
  case "$installed_v" in
    ''|*[!0-9]*) say_once launcher-activation "unit=launcher-activation waiting reason=installed_version_unreadable"; return 0 ;;
  esac
  if [ "$installed_v" -lt "$floor" ]; then
    say_once launcher-activation "unit=launcher-activation waiting reason=downgraded installed=$installed_v floor=$floor"
    return 0
  fi
  if [ "$(bounded "$launcher_timeout" "$exe" --launcher-active | /usr/bin/python3 -c '
import json, sys
try:
    print(json.load(sys.stdin).get("active") is True)
except (ValueError, AttributeError):
    print(False)
' 2>/dev/null)" = True ]; then
    say_once launcher-activation "unit=launcher-activation active"
    return 0
  fi
  if out=$(bounded 180 /usr/bin/python3 "$check" --activate --json); then
    say "unit=launcher-activation activated"
    printf '%s\n' "unit=launcher-activation active" > "$root/launcher-activation.state"
    return 0
  fi
  why=$(printf '%s' "$out" | /usr/bin/python3 -c '
import json, sys
try:
    report = json.load(sys.stdin)
except ValueError:
    print("unreadable"); sys.exit(0)
failed = [name + "=" + str(check.get("state", "fail")) for name, check in report.get("checks", {}).items() if not check.get("ok")]
print(",".join(failed) or report.get("activation", "unknown"))
' 2>/dev/null)
  say_once launcher-activation "unit=launcher-activation waiting reason=${why:-unknown}"
}

# bind_tick: record the SHA-256 of the installed steward tick in the launcher's
# activation record, so the launcher runs it under its identity. The check
# tool binds only when the installed file is byte-identical to the tick on
# main that this script installs; any other content stays unbound, and the
# launcher then hands the tick to plain /bin/bash (launcher-script-mismatch).
bind_tick() {
  local check="$root/Shipyard/tools/shipyard-launcher/shipyard-launcher-check" out
  [ "$(uname -s)" = Darwin ] || return 0
  [ -f "$HOME/.local/share/shipyard/launcher/activation.json" ] || return 0
  if out=$(bounded 60 /usr/bin/python3 "$check" --bind-script \
      "steward-tick=$root/Shipyard/tools/steward-tick/shipyard-steward-tick.sh"); then
    case "$out" in
      bound*) say "unit=launcher-bind $out" ;;
      *) say_once launcher-bind "unit=launcher-bind current" ;;
    esac
  else
    say_once launcher-bind "unit=launcher-bind refused reason=installed tick differs from main or no record"
  fi
}

if rev=$(sync_clone Shipyard "$shipyard_remote"); then
  install_file tick "$root/Shipyard/tools/steward-tick/shipyard-steward-tick.sh" "$tick_dest" "$rev"
  install_file deploy "$root/Shipyard/tools/fleet-deploy/fleet-deploy.sh" "$self_dest" "$rev"
  install_launcher "$rev"
  activate_launcher
  bind_tick
else
  say "unit=tick refused reason=fetch_failed"
fi

if rev=$(sync_clone fleet-handbook "$handbook_remote"); then
  installed=$(cat "$root/pass-runner.rev" 2>/dev/null || echo none)
  if [ "$rev" != "$installed" ]; then
    out="$root/pass-runner-install-$stamp.log"
    if (cd "$root/fleet-handbook" && bash scripts/install-pass-runner.sh) >"$out" 2>&1; then
      echo "$rev" > "$root/pass-runner.rev"
      say "unit=pass-runner installed rev=$rev from=$installed"
    else
      say "unit=pass-runner refused rev=$rev rc=$? installed=$installed log=$out"
    fi
    for f in $(ls -1t "$root"/pass-runner-install-*.log 2>/dev/null | tail -n +21); do rm -f "$f"; done
  fi
else
  say "unit=pass-runner refused reason=fetch_failed"
fi
exit 0
