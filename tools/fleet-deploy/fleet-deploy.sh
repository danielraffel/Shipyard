#!/bin/bash
# Deploy merged fleet scripts to this host.
#
# Units:
#   tick         Shipyard tools/steward-tick/shipyard-steward-tick.sh -> FLEET_DEPLOY_TICK_DEST
#   deploy       Shipyard tools/fleet-deploy/fleet-deploy.sh          -> FLEET_DEPLOY_SELF_DEST
#   pass-runner  fleet-handbook scripts/pass-runner, through its own scripts/install-pass-runner.sh
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

if rev=$(sync_clone Shipyard "$shipyard_remote"); then
  install_file tick "$root/Shipyard/tools/steward-tick/shipyard-steward-tick.sh" "$tick_dest" "$rev"
  install_file deploy "$root/Shipyard/tools/fleet-deploy/fleet-deploy.sh" "$self_dest" "$rev"
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
