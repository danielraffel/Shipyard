#!/usr/bin/env bash
# Install the producer-side merge-receipt handoff supervisor.
#
# The default is a dry-run. --install copies the wrapper to the managed
# Shipyard directory, renders and validates a per-user LaunchAgent, and loads
# it. The wrapper calls only Shipyard's local, repository-scoped export; it
# never queries GitHub or creates a receipt.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
TEMPLATE="$HERE/launchd/com.danielraffel.shipyard.merge-receipt-handoff.plist.template"
LABEL="com.danielraffel.shipyard.merge-receipt-handoff"
PYTHON="$(command -v python3 2>/dev/null || true)"
SHIPYARD="$(command -v shipyard 2>/dev/null || true)"
SOURCE_SCRIPT="$HERE/scripts/merge_receipt_handoff.py"
MANAGED_DIR="$HOME/Library/Application Support/shipyard"
SCRIPT="$MANAGED_DIR/merge_receipt_handoff.py"
STATE_DIR="$MANAGED_DIR"
REPO=""
FILE="$MANAGED_DIR/merge-receipts.jsonl"
HEALTH_FILE="$MANAGED_DIR/merge-receipts.health.json"
INTERVAL=900
INSTALL=0
LAUNCHCTL="${SHIPYARD_LAUNCHCTL_BIN:-/bin/launchctl}"
PLUTIL="${SHIPYARD_PLUTIL_BIN:-/usr/bin/plutil}"

usage() {
  echo "usage: $0 --repo OWNER/REPO [--shipyard ABS] [--python ABS] [--state-dir ABS] [--file ABS] [--health-file ABS] [--interval SECONDS] [--install]"
}
while [ "$#" -gt 0 ]; do
  case "$1" in
    --repo) REPO="${2:-}"; shift 2;; --shipyard) SHIPYARD="${2:-}"; shift 2;;
    --python) PYTHON="${2:-}"; shift 2;; --state-dir) STATE_DIR="${2:-}"; shift 2;;
    --file) FILE="${2:-}"; shift 2;; --health-file) HEALTH_FILE="${2:-}"; shift 2;;
    --interval) INTERVAL="${2:-}"; shift 2;; --install) INSTALL=1; shift;;
    -h|--help) usage; exit 0;; *) echo "unknown argument: $1" >&2; usage >&2; exit 2;;
  esac
done
[ -n "$REPO" ] || { echo "--repo is required" >&2; usage >&2; exit 2; }
case "$REPO" in */*/*|/*|*/|*\ *|"") echo "--repo must be OWNER/REPO" >&2; exit 2;; */*) ;; *) echo "--repo must be OWNER/REPO" >&2; exit 2;; esac
case "$INTERVAL" in ''|*[!0-9]*) echo "--interval must be a positive integer" >&2; exit 2;; esac
[ "$INTERVAL" -ge 300 ] || { echo "--interval must be at least 300 seconds" >&2; exit 2; }
case "$PYTHON" in /*) ;; *) echo "--python must be an absolute path" >&2; exit 2;; esac
case "$SHIPYARD" in /*) ;; *) echo "--shipyard must be an absolute path" >&2; exit 2;; esac
for path in "$PYTHON" "$SHIPYARD" "$SOURCE_SCRIPT" "$TEMPLATE"; do
  [ -f "$path" ] || [ -x "$path" ] || { echo "missing required path: $path" >&2; exit 2; }
done
[ -x "$SHIPYARD" ] || { echo "Shipyard executable is unavailable: $SHIPYARD" >&2; exit 2; }
"$PYTHON" "$SOURCE_SCRIPT" --help >/dev/null
for path in "$STATE_DIR" "$FILE" "$HEALTH_FILE"; do case "$path" in /*) ;; *) echo "paths must be absolute: $path" >&2; exit 2;; esac; done
PLIST_DIR="$HOME/Library/LaunchAgents"; PLIST="$PLIST_DIR/$LABEL.plist"; LOG="$HOME/Library/Logs/shipyard-merge-receipt-handoff.log"
echo "Shipyard merge-receipt handoff install plan:"; echo "  repo=$REPO interval=${INTERVAL}s"; echo "  shipyard=$SHIPYARD"; echo "  state_dir=$STATE_DIR"; echo "  handoff=$FILE"; echo "  health=$HEALTH_FILE"; echo "  launch_agent=$PLIST"; echo "  action=$( [ "$INSTALL" = 1 ] && echo install || echo dry-run )"
[ "$INSTALL" = 1 ] || exit 0
umask 077
mkdir -p "$PLIST_DIR" "$HOME/Library/Logs" "$MANAGED_DIR" "$(dirname "$FILE")" "$(dirname "$HEALTH_FILE")"
STAGED="$(mktemp "$PLIST_DIR/.$LABEL.XXXXXX")"; STAGED_SCRIPT="$(mktemp "$MANAGED_DIR/.merge_receipt_handoff.py.XXXXXX")"; trap 'rm -f "$STAGED" "$STAGED_SCRIPT"' EXIT
cp "$SOURCE_SCRIPT" "$STAGED_SCRIPT"; chmod 0755 "$STAGED_SCRIPT"; mv -f "$STAGED_SCRIPT" "$SCRIPT"
escape() { printf '%s' "$1" | sed -e 's/[&|\\]/\\&/g'; }
sed -e "s|@PYTHON@|$(escape "$PYTHON")|g" -e "s|@SCRIPT@|$(escape "$SCRIPT")|g" \
    -e "s|@SHIPYARD@|$(escape "$SHIPYARD")|g" -e "s|@STATE_DIR@|$(escape "$STATE_DIR")|g" \
    -e "s|@REPO@|$(escape "$REPO")|g" -e "s|@FILE@|$(escape "$FILE")|g" \
    -e "s|@HEALTH_FILE@|$(escape "$HEALTH_FILE")|g" -e "s|@INTERVAL@|$INTERVAL|g" \
    -e "s|@HOME@|$(escape "$HOME")|g" -e "s|@LOG@|$(escape "$LOG")|g" "$TEMPLATE" > "$STAGED"
if grep -q '@[A-Z_]*@' "$STAGED"; then echo "template left an unrendered placeholder" >&2; exit 1; fi
"$PLUTIL" -lint "$STAGED" >/dev/null; chmod 644 "$STAGED"; mv -f "$STAGED" "$PLIST"; trap - EXIT
DOMAIN="gui/$(id -u)"; "$LAUNCHCTL" bootout "$DOMAIN/$LABEL" >/dev/null 2>&1 || true; "$LAUNCHCTL" bootstrap "$DOMAIN" "$PLIST"
echo "installed and loaded $LABEL"
