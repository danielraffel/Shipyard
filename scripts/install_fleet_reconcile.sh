#!/usr/bin/env bash
# Install the fleet-reconcile launchd agent on the rollout controller.
#
# Prints the plan by default and changes nothing. --install renders the
# template, validates it with plutil, writes it atomically, and (re)loads the
# agent with launchctl bootout/bootstrap. The agent runs
# `shipyard --json runner fleet-reconcile --apply` every 15 minutes.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
TEMPLATE="$HERE/launchd/com.danielraffel.shipyard.fleet-reconcile.plist.template"
LABEL="com.danielraffel.shipyard.fleet-reconcile"
SHIPYARD="$(command -v shipyard 2>/dev/null || true)"
INTERVAL=900
SOAK_MINUTES=30
RETRY_HOURS=6
APPLY=0
LAUNCHCTL="${SHIPYARD_LAUNCHCTL_BIN:-/bin/launchctl}"
PLUTIL="${SHIPYARD_PLUTIL_BIN:-/usr/bin/plutil}"

usage() {
  cat <<'USAGE'
usage: install_fleet_reconcile.sh [--shipyard ABSOLUTE_PATH] [--interval SECONDS]
       [--soak-minutes N] [--retry-hours N] [--install]

Prints a plan by default. Run it on the fleet-rollout controller only (the
host whose machine-global config declares [host_class.*]).
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --shipyard) SHIPYARD="${2:-}"; shift 2 ;;
    --interval) INTERVAL="${2:-}"; shift 2 ;;
    --soak-minutes) SOAK_MINUTES="${2:-}"; shift 2 ;;
    --retry-hours) RETRY_HOURS="${2:-}"; shift 2 ;;
    --install) APPLY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

for value in "$INTERVAL" "$SOAK_MINUTES" "$RETRY_HOURS"; do
  case "$value" in ''|*[!0-9]*) echo "numeric options must be positive integers" >&2; exit 2 ;; esac
done
[ "$INTERVAL" -ge 300 ] || { echo "--interval must be at least 300 seconds" >&2; exit 2; }
[ "$RETRY_HOURS" -ge 1 ] || { echo "--retry-hours must be at least 1" >&2; exit 2; }
case "$SHIPYARD" in /*) ;; *) echo "--shipyard must be an absolute path (got '$SHIPYARD')" >&2; exit 2 ;; esac
[ -x "$SHIPYARD" ] || { echo "Shipyard executable is unavailable: $SHIPYARD" >&2; exit 2; }
"$SHIPYARD" runner fleet-reconcile --help >/dev/null 2>&1 || {
  echo "$SHIPYARD has no 'runner fleet-reconcile'; update it before installing the agent" >&2
  exit 2
}
[ -f "$TEMPLATE" ] || { echo "installer must run from a complete Shipyard checkout" >&2; exit 2; }

PLIST_DIR="$HOME/Library/LaunchAgents"
PLIST="$PLIST_DIR/$LABEL.plist"

echo "Shipyard fleet-reconcile install plan:"
echo "  shipyard=$SHIPYARD"
echo "  interval=${INTERVAL}s soak=${SOAK_MINUTES}m retry=${RETRY_HOURS}h"
echo "  launch_agent=$PLIST"
echo "  log=$HOME/Library/Logs/shipyard-fleet-reconcile.log"
[ "$APPLY" = 1 ] || { echo "  action=dry-run (pass --install to apply)"; exit 0; }

umask 077
mkdir -p "$PLIST_DIR" "$HOME/Library/Logs"
STAGED="$(mktemp "$PLIST_DIR/.$LABEL.XXXXXX")"
trap 'rm -f "$STAGED"' EXIT
escape() { printf '%s' "$1" | sed -e 's/[&|\\]/\\&/g'; }
sed -e "s|@SHIPYARD@|$(escape "$SHIPYARD")|g" \
    -e "s|@HOME@|$(escape "$HOME")|g" \
    -e "s|@INTERVAL@|$INTERVAL|g" \
    -e "s|@SOAK_MINUTES@|$SOAK_MINUTES|g" \
    -e "s|@RETRY_HOURS@|$RETRY_HOURS|g" \
    "$TEMPLATE" > "$STAGED"
if grep -q '@[A-Z_]*@' "$STAGED"; then
  echo "template left an unrendered placeholder" >&2
  exit 1
fi
"$PLUTIL" -lint "$STAGED" >/dev/null
chmod 644 "$STAGED"
mv -f "$STAGED" "$PLIST"
trap - EXIT
DOMAIN="gui/$(id -u)"
"$LAUNCHCTL" bootout "$DOMAIN/$LABEL" >/dev/null 2>&1 || true
"$LAUNCHCTL" bootstrap "$DOMAIN" "$PLIST"
echo "  action=installed and loaded $LABEL"
