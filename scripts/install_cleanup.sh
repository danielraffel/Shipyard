#!/usr/bin/env bash
# Install the daily bounded-retention launchd agent on a macOS Shipyard host.
# The default is a plan; --install is the only mutating mode.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
TEMPLATE="$HERE/launchd/com.danielraffel.shipyard.cleanup.plist.template"
WRAPPER="$HERE/scripts/run_cleanup.sh"
LABEL="com.danielraffel.shipyard.cleanup"
SHIPYARD="$(command -v shipyard 2>/dev/null || true)"
INTERVAL=86400
OLDER_THAN_HOURS=6
INSTALL=0
UNINSTALL=0
LAUNCHCTL="${SHIPYARD_LAUNCHCTL_BIN:-/bin/launchctl}"
PLUTIL="${SHIPYARD_PLUTIL_BIN:-/usr/bin/plutil}"
STATE_DIR="$HOME/Library/Application Support/shipyard"
STATUS="$STATE_DIR/cleanup/last-run.json"
INSTALLED_WRAPPER="$STATE_DIR/cleanup/run_cleanup.sh"
VALIDATION_TMP_BASE="${SHIPYARD_VALIDATION_TMP_BASE:-}"
AGENT_PATH="/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

usage() {
  cat <<'USAGE'
usage: install_cleanup.sh [--shipyard ABSOLUTE_PATH] [--interval SECONDS]
       [--older-than-hours N] [--install|--uninstall]

Prints a plan by default. --install loads a per-user launchd agent that runs
retention and validation-TMP cleanup daily and records its last result.
--uninstall removes the agent, its installed wrapper, and its last receipt.
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --shipyard) SHIPYARD="${2:-}"; shift 2 ;;
    --interval) INTERVAL="${2:-}"; shift 2 ;;
    --older-than-hours) OLDER_THAN_HOURS="${2:-}"; shift 2 ;;
    --install) INSTALL=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[ "$INSTALL" -eq 0 ] || [ "$UNINSTALL" -eq 0 ] || {
  echo "--install and --uninstall are mutually exclusive" >&2; exit 2;
}

for value in "$INTERVAL" "$OLDER_THAN_HOURS"; do
  case "$value" in ''|*[!0-9]*) echo "numeric options must be positive integers" >&2; exit 2 ;; esac
done
[ "$INTERVAL" -ge 900 ] || { echo "--interval must be at least 900 seconds" >&2; exit 2; }
[ "$OLDER_THAN_HOURS" -ge 1 ] || { echo "--older-than-hours must be at least 1" >&2; exit 2; }
if [ "$UNINSTALL" -eq 0 ]; then
  case "$SHIPYARD" in /*) ;; *) echo "--shipyard must be an absolute path (got '$SHIPYARD')" >&2; exit 2 ;; esac
  [ -x "$SHIPYARD" ] || { echo "Shipyard executable is unavailable: $SHIPYARD" >&2; exit 2; }
  [ -x "$WRAPPER" ] || { echo "cleanup wrapper is unavailable: $WRAPPER" >&2; exit 2; }
fi
[ -f "$TEMPLATE" ] || { echo "installer must run from a complete Shipyard checkout" >&2; exit 2; }
if [ "$UNINSTALL" -eq 0 ] && ! "$SHIPYARD" cleanup --help >/dev/null 2>&1; then
  echo "$SHIPYARD has no cleanup command; update it before installing the agent" >&2
  exit 2
fi
if [ "$UNINSTALL" -eq 0 ] && ! "$SHIPYARD" cleanup --help 2>&1 | grep -q -- '--owned-only'; then
  echo "$SHIPYARD lacks the conservative --owned-only validation-TMP mode; update it before installing the agent" >&2
  exit 2
fi
if [ "$UNINSTALL" -eq 0 ] && ! PATH="$AGENT_PATH" command -v python3 >/dev/null 2>&1; then
  echo "python3 is required for cleanup previews and receipts" >&2
  exit 2
fi

PLIST_DIR="$HOME/Library/LaunchAgents"
PLIST="$PLIST_DIR/$LABEL.plist"
LOG="$HOME/Library/Logs/shipyard-cleanup.log"
if [ "$UNINSTALL" -eq 1 ]; then
  [ "$(uname -s)" = "Darwin" ] || { echo "cleanup launchd agent is macOS-only" >&2; exit 2; }
  DOMAIN="gui/$(id -u)"
  "$LAUNCHCTL" bootout "$DOMAIN/$LABEL" >/dev/null 2>&1 || true
  if "$LAUNCHCTL" print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    echo "cannot uninstall $LABEL: launchd still reports it loaded" >&2
    exit 1
  fi
  rm -f "$PLIST" "$INSTALLED_WRAPPER" "$STATUS" "${STATUS%.json}.in-progress" \
    "$STATE_DIR/cleanup-preview.json" "$STATE_DIR/cleanup-validation-preview.json"
  echo "  action=uninstalled $LABEL"
  exit 0
fi
echo "Shipyard cleanup install plan:"
echo "  shipyard=$SHIPYARD"
echo "  interval=${INTERVAL}s validation_tmp_older_than=${OLDER_THAN_HOURS}h"
echo "  launch_agent=$PLIST"
echo "  status=$STATUS"
echo "  log=$LOG"
[ "$INSTALL" = 1 ] || { echo "  action=dry-run (pass --install to apply)"; exit 0; }
[ "$(uname -s)" = "Darwin" ] || { echo "cleanup launchd agent is macOS-only" >&2; exit 2; }
[ -x "$LAUNCHCTL" ] || { echo "launchctl is unavailable: $LAUNCHCTL" >&2; exit 2; }
umask 077
mkdir -p "$PLIST_DIR" "$HOME/Library/Logs" "$(dirname "$STATUS")"

# Show the same bounded plan the agent will apply. A refusal is printed but does
# not block installation; the first scheduled run records it for `doctor`.
set +e
(cd / && /usr/bin/env -i HOME="$HOME" PATH="$AGENT_PATH" SHIPYARD_VALIDATION_TMP_BASE="$VALIDATION_TMP_BASE" \
  "$SHIPYARD" --state-dir "$STATE_DIR" --json cleanup --dry-run) >"$STATE_DIR/cleanup-preview.json" 2>&1
preview_exit=$?
(cd / && /usr/bin/env -i HOME="$HOME" PATH="$AGENT_PATH" SHIPYARD_VALIDATION_TMP_BASE="$VALIDATION_TMP_BASE" \
  "$SHIPYARD" --state-dir "$STATE_DIR" --json cleanup --validation-tmp --dry-run \
  --older-than-hours "$OLDER_THAN_HOURS" --owned-only) >"$STATE_DIR/cleanup-validation-preview.json" 2>&1
validation_preview_exit=$?
set -e
preview_summary() {
  local label="$1" path="$2" exit_code="$3"
  if [ "$exit_code" -eq 0 ] && [ -s "$path" ]; then
    if summary="$(python3 - "$path" "$label" <<'PY'
import json, sys
path, label = sys.argv[1:]
try:
    data = json.load(open(path, encoding="utf-8"))
except Exception:
    raise SystemExit(1)
if label == "cleanup":
    print(f"count={data.get('count', len(data.get('items', [])))} bytes={data.get('total_bytes', 0)}")
else:
    print(f"count={len(data.get('candidates', []))} bytes={data.get('reclaimable_bytes', 0)}")
PY
)"; then
      echo "  preview=$label exit=$exit_code $summary"
      return
    fi
  fi
  echo "  preview=$label exit=$exit_code (details: $path)"
}
preview_summary cleanup "$STATE_DIR/cleanup-preview.json" "$preview_exit"
preview_summary validation-tmp "$STATE_DIR/cleanup-validation-preview.json" "$validation_preview_exit"

escape() { printf '%s' "$1" | sed -e 's/[&|\\]/\\&/g'; }
if printf '%s' "$WRAPPER$SHIPYARD$STATE_DIR$STATUS$LOG$HOME$VALIDATION_TMP_BASE" | grep -q '[&<>\"]'; then
  echo "paths containing XML metacharacters are not supported by this installer" >&2
  exit 2
fi
WRAPPER_STAGED="$(mktemp "$(dirname "$INSTALLED_WRAPPER")/.run_cleanup.XXXXXX")"
STAGED="$(mktemp "$PLIST_DIR/.$LABEL.XXXXXX")"
trap 'rm -f "$STAGED" "$WRAPPER_STAGED"' EXIT
cp "$WRAPPER" "$WRAPPER_STAGED"
chmod 700 "$WRAPPER_STAGED"
sed -e "s|@WRAPPER@|$(escape "$INSTALLED_WRAPPER")|g" \
    -e "s|@SHIPYARD@|$(escape "$SHIPYARD")|g" \
    -e "s|@STATE_DIR@|$(escape "$STATE_DIR")|g" \
    -e "s|@STATUS@|$(escape "$STATUS")|g" \
    -e "s|@VALIDATION_TMP_BASE@|$(escape "$VALIDATION_TMP_BASE")|g" \
    -e "s|@OLDER_THAN_HOURS@|$OLDER_THAN_HOURS|g" \
    -e "s|@INTERVAL@|$INTERVAL|g" \
    -e "s|@LOG@|$(escape "$LOG")|g" \
    -e "s|@HOME@|$(escape "$HOME")|g" \
    "$TEMPLATE" > "$STAGED"
if grep -q '@[A-Z_]*@' "$STAGED"; then
  echo "template left an unrendered placeholder" >&2
  exit 1
fi
"$PLUTIL" -lint "$STAGED" >/dev/null
chmod 644 "$STAGED"
DOMAIN="gui/$(id -u)"
"$LAUNCHCTL" bootout "$DOMAIN/$LABEL" >/dev/null 2>&1 || true
mv -f "$WRAPPER_STAGED" "$INSTALLED_WRAPPER"
mv -f "$STAGED" "$PLIST"
trap - EXIT
"$LAUNCHCTL" bootstrap "$DOMAIN" "$PLIST"
echo "  action=installed and loaded $LABEL"
