#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"; TEMPLATE="$HERE/launchd/com.danielraffel.shipyard.baseline-collector.plist.template"; LABEL="com.danielraffel.shipyard.baseline-collector"
PYTHON="$(command -v python3 2>/dev/null || true)"; SOURCE_COLLECTOR="$HERE/scripts/collect_baseline.py"; SOURCE_HANDOFF="$HERE/scripts/merge_receipt_handoff.py"; MANAGED_DIR="$HOME/Library/Application Support/shipyard"; COLLECTOR="$MANAGED_DIR/collect_baseline.py"; HANDOFF="$MANAGED_DIR/merge_receipt_handoff.py"; DB="$MANAGED_DIR/metrics/metrics.db"
PROJECT=pulp; REPO=Generous-Corp/pulp; STATE="$HOME/Library/Application Support/shipyard/baseline-status.json"; STATUS="$HOME/Library/Application Support/shipyard/baseline-status.md"; MERGE_RECEIPTS="$HOME/Library/Application Support/shipyard/merge-receipts.jsonl"; MERGE_RECEIPT_HEALTH="$HOME/Library/Application Support/shipyard/merge-receipts.health.json"; TARTCI_HOST=m5s; INTERVAL=3600; INSTALL=0
usage() { echo "usage: $0 [--python ABS] [--collector ABS] [--managed-dir ABS] [--db ABS] [--project NAME] [--repo OWNER/REPO] [--state ABS] [--status-markdown ABS] [--merge-receipts ABS] [--merge-receipt-health ABS] [--tartci-host HOST] [--interval SECONDS] [--install]"; }
while [ "$#" -gt 0 ]; do
  case "$1" in
    --python) PYTHON="${2:-}"; shift 2;; --collector) SOURCE_COLLECTOR="${2:-}"; shift 2;; --managed-dir) MANAGED_DIR="${2:-}"; COLLECTOR="$MANAGED_DIR/collect_baseline.py"; shift 2;; --db) DB="${2:-}"; shift 2;; --project) PROJECT="${2:-}"; shift 2;;
    --repo) REPO="${2:-}"; shift 2;; --state) STATE="${2:-}"; shift 2;; --status-markdown) STATUS="${2:-}"; shift 2;; --merge-receipts) MERGE_RECEIPTS="${2:-}"; shift 2;; --merge-receipt-health) MERGE_RECEIPT_HEALTH="${2:-}"; shift 2;; --tartci-host) TARTCI_HOST="${2:-}"; shift 2;;
    --interval) INTERVAL="${2:-}"; shift 2;; --install) INSTALL=1; shift;; -h|--help) usage; exit 0;; *) echo "unknown option: $1" >&2; exit 2;;
  esac
done
case "$INTERVAL" in ''|*[!0-9]*) echo "--interval must be numeric" >&2; exit 2;; esac; [ "$INTERVAL" -ge 900 ] || { echo "--interval must be at least 900 seconds" >&2; exit 2; }
SOURCE_HISTORY="$(dirname "$SOURCE_COLLECTOR")/baseline_pass_history.py"
for path in "$PYTHON" "$SOURCE_COLLECTOR" "$SOURCE_HISTORY" "$SOURCE_HANDOFF" "$TEMPLATE"; do [ -f "$path" ] || [ -x "$path" ] || { echo "missing required path: $path" >&2; exit 2; }; done; "$PYTHON" "$SOURCE_COLLECTOR" --help >/dev/null
PLIST_DIR="$HOME/Library/LaunchAgents"; PLIST="$PLIST_DIR/$LABEL.plist"; LOG="$HOME/Library/Logs/shipyard-baseline-collector.log"; echo "Shipyard baseline collector: interval=${INTERVAL}s host=$TARTCI_HOST"; echo "  source=$SOURCE_COLLECTOR"; echo "  managed_collector=$COLLECTOR"; echo "  plist=$PLIST"
[ "$INSTALL" = 1 ] || { echo "  action=dry-run (pass --install to apply)"; exit 0; }
umask 077; mkdir -p "$PLIST_DIR" "$HOME/Library/Logs" "$MANAGED_DIR" "$(dirname "$DB")" "$(dirname "$STATE")"; STAGED="$(mktemp "$PLIST_DIR/.$LABEL.XXXXXX")"; STAGED_COLLECTOR="$(mktemp "$MANAGED_DIR/.collect_baseline.py.XXXXXX")"; STAGED_HISTORY="$(mktemp "$MANAGED_DIR/.baseline_pass_history.py.XXXXXX")"; STAGED_HANDOFF="$(mktemp "$MANAGED_DIR/.merge_receipt_handoff.py.XXXXXX")"; trap 'rm -f "$STAGED" "$STAGED_COLLECTOR" "$STAGED_HISTORY" "$STAGED_HANDOFF"' EXIT
cp "$SOURCE_COLLECTOR" "$STAGED_COLLECTOR"; cp "$SOURCE_HISTORY" "$STAGED_HISTORY"; cp "$SOURCE_HANDOFF" "$STAGED_HANDOFF"; chmod 0755 "$STAGED_COLLECTOR" "$STAGED_HANDOFF"; chmod 0644 "$STAGED_HISTORY"; mv -f "$STAGED_COLLECTOR" "$COLLECTOR"; mv -f "$STAGED_HISTORY" "$MANAGED_DIR/baseline_pass_history.py"; mv -f "$STAGED_HANDOFF" "$HANDOFF"
escape() { printf '%s' "$1" | sed -e 's/[&|\\]/\\&/g'; }
sed -e "s|@PYTHON@|$(escape "$PYTHON")|g" -e "s|@COLLECTOR@|$(escape "$COLLECTOR")|g" -e "s|@DB@|$(escape "$DB")|g" -e "s|@PROJECT@|$(escape "$PROJECT")|g" -e "s|@REPO@|$(escape "$REPO")|g" -e "s|@STATE@|$(escape "$STATE")|g" -e "s|@STATUS@|$(escape "$STATUS")|g" -e "s|@MERGE_RECEIPTS@|$(escape "$MERGE_RECEIPTS")|g" -e "s|@MERGE_RECEIPT_HEALTH@|$(escape "$MERGE_RECEIPT_HEALTH")|g" -e "s|@TARTCI_HOST@|$(escape "$TARTCI_HOST")|g" -e "s|@INTERVAL@|$INTERVAL|g" -e "s|@HOME@|$(escape "$HOME")|g" -e "s|@LOG@|$(escape "$LOG")|g" "$TEMPLATE" > "$STAGED"
/usr/bin/plutil -lint "$STAGED" >/dev/null; mv -f "$STAGED" "$PLIST"; trap - EXIT; DOMAIN="gui/$(id -u)"; /bin/launchctl bootout "$DOMAIN/$LABEL" >/dev/null 2>&1 || true; /bin/launchctl bootstrap "$DOMAIN" "$PLIST"; echo "installed and loaded $LABEL"
