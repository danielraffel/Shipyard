#!/usr/bin/env bash
# Install a producer-side or remote-observer merge-receipt handoff supervisor.
#
# The default is a dry-run. --install copies the wrapper to the managed
# Shipyard directory, renders and validates a per-user LaunchAgent, and loads
# it. Both modes are read-only with respect to GitHub and merge authorization.
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
BASE_BRANCH="main"
INTERVAL=900
SOURCE_HOST=""
SOURCE_SHIPYARD=""
SOURCE_STATE_DIR=""
INSTALL=0
LAUNCHCTL="${SHIPYARD_LAUNCHCTL_BIN:-/bin/launchctl}"
PLUTIL="${SHIPYARD_PLUTIL_BIN:-/usr/bin/plutil}"

usage() {
  echo "usage: $0 --repo OWNER/REPO [--base-branch BRANCH] [--shipyard ABS] [--python ABS] [--state-dir ABS] [--source-host HOST --source-shipyard ABS --source-state-dir ABS] [--file ABS] [--health-file ABS] [--interval SECONDS] [--install]"
}
while [ "$#" -gt 0 ]; do
  case "$1" in
    --repo) REPO="${2:-}"; shift 2;; --shipyard) SHIPYARD="${2:-}"; shift 2;;
    --python) PYTHON="${2:-}"; shift 2;; --state-dir) STATE_DIR="${2:-}"; shift 2;;
    --source-host) SOURCE_HOST="${2:-}"; shift 2;; --source-shipyard) SOURCE_SHIPYARD="${2:-}"; shift 2;;
    --source-state-dir) SOURCE_STATE_DIR="${2:-}"; shift 2;;
    --file) FILE="${2:-}"; shift 2;; --health-file) HEALTH_FILE="${2:-}"; shift 2;; --base-branch) BASE_BRANCH="${2:-}"; shift 2;;
    --interval) INTERVAL="${2:-}"; shift 2;; --install) INSTALL=1; shift;;
    -h|--help) usage; exit 0;; *) echo "unknown argument: $1" >&2; usage >&2; exit 2;;
  esac
done
[ -n "$REPO" ] || { echo "--repo is required" >&2; usage >&2; exit 2; }
[ -n "$BASE_BRANCH" ] || { echo "--base-branch must not be empty" >&2; exit 2; }
case "$REPO" in */*/*|/*|*/|*\ *|"") echo "--repo must be OWNER/REPO" >&2; exit 2;; */*) ;; *) echo "--repo must be OWNER/REPO" >&2; exit 2;; esac
case "$INTERVAL" in ''|*[!0-9]*) echo "--interval must be a positive integer" >&2; exit 2;; esac
[ "$INTERVAL" -ge 300 ] || { echo "--interval must be at least 300 seconds" >&2; exit 2; }
case "$PYTHON" in /*) ;; *) echo "--python must be an absolute path" >&2; exit 2;; esac
if [ -n "$SOURCE_HOST" ]; then
  case "$SOURCE_HOST" in *[!A-Za-z0-9._-]*|'') echo "--source-host must be a plain fleet host name" >&2; exit 2;; esac
  case "$SOURCE_SHIPYARD" in /*) ;; *) echo "--source-shipyard must be an absolute path" >&2; exit 2;; esac
  case "$SOURCE_STATE_DIR" in /*) ;; *) echo "--source-state-dir must be an absolute path" >&2; exit 2;; esac
else
  case "$SHIPYARD" in /*) ;; *) echo "--shipyard must be an absolute path" >&2; exit 2;; esac
fi
for path in "$PYTHON" "$SOURCE_SCRIPT" "$TEMPLATE"; do
  [ -f "$path" ] || [ -x "$path" ] || { echo "missing required path: $path" >&2; exit 2; }
done
if [ -z "$SOURCE_HOST" ] && [ ! -x "$SHIPYARD" ]; then
  echo "Shipyard executable is unavailable: $SHIPYARD" >&2; exit 2
fi
"$PYTHON" "$SOURCE_SCRIPT" --help >/dev/null
for path in "$STATE_DIR" "$FILE" "$HEALTH_FILE"; do case "$path" in /*) ;; *) echo "paths must be absolute: $path" >&2; exit 2;; esac; done
PLIST_DIR="$HOME/Library/LaunchAgents"; PLIST="$PLIST_DIR/$LABEL.plist"; LOG="$HOME/Library/Logs/shipyard-merge-receipt-handoff.log"
MODE_ARGS=""
xml_escape() { printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' -e 's/"/\&quot;/g' -e "s/'/\&apos;/g"; }
if [ -n "$SOURCE_HOST" ]; then
  MODE_ARGS="<string>fetch</string><string>--source-host</string><string>$(xml_escape "$SOURCE_HOST")</string><string>--source-shipyard</string><string>$(xml_escape "$SOURCE_SHIPYARD")</string><string>--source-state-dir</string><string>$(xml_escape "$SOURCE_STATE_DIR")</string><string>--repo</string><string>$(xml_escape "$REPO")</string><string>--file</string><string>$(xml_escape "$FILE")</string><string>--base-branch</string><string>$(xml_escape "$BASE_BRANCH")</string><string>--health-file</string><string>$(xml_escape "$HEALTH_FILE")</string>"
else
  MODE_ARGS="<string>export</string><string>--shipyard</string><string>$(xml_escape "$SHIPYARD")</string><string>--state-dir</string><string>$(xml_escape "$STATE_DIR")</string><string>--repo</string><string>$(xml_escape "$REPO")</string><string>--file</string><string>$(xml_escape "$FILE")</string><string>--base-branch</string><string>$(xml_escape "$BASE_BRANCH")</string><string>--health-file</string><string>$(xml_escape "$HEALTH_FILE")</string>"
fi
echo "Shipyard merge-receipt handoff install plan:"; echo "  mode=$( [ -n "$SOURCE_HOST" ] && echo fetch || echo export )"; echo "  repo=$REPO base_branch=$BASE_BRANCH interval=${INTERVAL}s"; echo "  shipyard=$SHIPYARD"; echo "  source_host=$SOURCE_HOST"; echo "  state_dir=$STATE_DIR"; echo "  source_state_dir=$SOURCE_STATE_DIR"; echo "  handoff=$FILE"; echo "  health=$HEALTH_FILE"; echo "  launch_agent=$PLIST"; echo "  action=$( [ "$INSTALL" = 1 ] && echo install || echo dry-run )"
[ "$INSTALL" = 1 ] || exit 0
umask 077
mkdir -p "$PLIST_DIR" "$HOME/Library/Logs" "$MANAGED_DIR" "$(dirname "$FILE")" "$(dirname "$HEALTH_FILE")"
STAGED="$(mktemp "$PLIST_DIR/.$LABEL.XXXXXX")"; STAGED_SCRIPT="$(mktemp "$MANAGED_DIR/.merge_receipt_handoff.py.XXXXXX")"; trap 'rm -f "$STAGED" "$STAGED_SCRIPT"' EXIT
cp "$SOURCE_SCRIPT" "$STAGED_SCRIPT"; chmod 0755 "$STAGED_SCRIPT"; mv -f "$STAGED_SCRIPT" "$SCRIPT"
escape() { printf '%s' "$1" | sed -e 's/[&|\\]/\\&/g'; }
sed -e "s|@PYTHON@|$(escape "$PYTHON")|g" -e "s|@SCRIPT@|$(escape "$SCRIPT")|g" \
    -e "s|@MODE_ARGS@|$(escape "$MODE_ARGS")|g" -e "s|@INTERVAL@|$INTERVAL|g" \
    -e "s|@HOME@|$(escape "$HOME")|g" -e "s|@LOG@|$(escape "$LOG")|g" "$TEMPLATE" > "$STAGED"
if grep -q '@[A-Z_]*@' "$STAGED"; then echo "template left an unrendered placeholder" >&2; exit 1; fi
"$PLUTIL" -lint "$STAGED" >/dev/null; chmod 644 "$STAGED"; mv -f "$STAGED" "$PLIST"; trap - EXIT
DOMAIN="gui/$(id -u)"; "$LAUNCHCTL" bootout "$DOMAIN/$LABEL" >/dev/null 2>&1 || true; "$LAUNCHCTL" bootstrap "$DOMAIN" "$PLIST"
echo "installed and loaded $LABEL"
