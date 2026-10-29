#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTALLER="$ROOT/scripts/install_cleanup.sh"
WRAPPER="$ROOT/scripts/run_cleanup.sh"
HOME_DIR="$(mktemp -d)"
TOOLS="$HOME_DIR/tools"
mkdir -p "$TOOLS"

cat >"$HOME_DIR/fake-shipyard" <<'SH'
#!/bin/sh
case "$*" in
  *--validation-tmp*) echo '{"command":"cleanup.validation_tmp","candidates":[],"reclaimable_bytes":0}'; exit 0;;
  *cleanup\ --help*) echo --owned-only; exit 0;;
  *) echo "refusing log cleanup: malformed terminal job sy-1" >&2; exit 1;;
esac
SH
chmod +x "$HOME_DIR/fake-shipyard"
cat >"$TOOLS/plutil" <<'SH'
#!/bin/sh
exit 0
SH
cat >"$TOOLS/launchctl" <<'SH'
#!/bin/sh
if [ "$1" = print ]; then
  exit 1
fi
echo "$*" >>"$FAKE_LAUNCHCTL_LOG"
SH
chmod +x "$TOOLS/plutil" "$TOOLS/launchctl"

HOME="$HOME_DIR" "$INSTALLER" --shipyard "$HOME_DIR/fake-shipyard" >"$HOME_DIR/plan"
grep -q 'action=dry-run' "$HOME_DIR/plan"
FAKE_LAUNCHCTL_LOG="$HOME_DIR/launchctl.log" HOME="$HOME_DIR" \
  SHIPYARD_PLUTIL_BIN="$TOOLS/plutil" SHIPYARD_LAUNCHCTL_BIN="$TOOLS/launchctl" \
  "$INSTALLER" --shipyard "$HOME_DIR/fake-shipyard" --install >"$HOME_DIR/install"
PLIST="$HOME_DIR/Library/LaunchAgents/com.danielraffel.shipyard.cleanup.plist"
test -f "$PLIST"
! grep -q '@[A-Z_]*@' "$PLIST"
grep -q 'bootstrap gui/' "$HOME_DIR/launchctl.log"
grep -q "$HOME_DIR/Library/Application Support/shipyard/cleanup/run_cleanup.sh" "$PLIST"
grep -q '<key>RunAtLoad</key>' "$PLIST"
python3 - "$PLIST" <<'PY'
import sys
text = open(sys.argv[1], encoding="utf-8").read()
assert '<key>RunAtLoad</key>\n    <true/>' in text
PY
grep -q -- '--owned-only' "$HOME_DIR/Library/Application Support/shipyard/cleanup/run_cleanup.sh"
grep -q 'preview=cleanup exit=1' "$HOME_DIR/install"
grep -q 'preview=validation-tmp exit=0 count=0 bytes=0' "$HOME_DIR/install"

RECEIPT="$HOME_DIR/receipt.json"
set +e
"$WRAPPER" --shipyard "$HOME_DIR/fake-shipyard" --state-dir "$HOME_DIR/state" \
  --status "$RECEIPT" >"$HOME_DIR/run.log" 2>&1
exit_code=$?
set -e
[ "$exit_code" -eq 1 ]
python3 - "$RECEIPT" <<'PY'
import json
import sys

receipt = json.load(open(sys.argv[1], encoding="utf-8"))
assert receipt["ok"] is False
assert receipt["cleanup_exit"] == 1
assert receipt["validation_tmp_exit"] == 0
assert "malformed terminal job" in receipt["cleanup_output"]
PY

SLOW_SHIPYARD="$HOME_DIR/slow-shipyard"
cat >"$SLOW_SHIPYARD" <<'SH'
#!/bin/sh
sleep 2
echo '{"command":"cleanup.validation_tmp","candidates":[],"reclaimable_bytes":0}'
SH
chmod +x "$SLOW_SHIPYARD"
SLOW_STATE="$HOME_DIR/slow-state"
SLOW_STATUS="$HOME_DIR/slow-status.json"
"$WRAPPER" --shipyard "$SLOW_SHIPYARD" --state-dir "$SLOW_STATE" \
  --status "$SLOW_STATUS" >"$HOME_DIR/slow-first.log" 2>&1 &
slow_pid=$!
sleep 0.2
set +e
"$WRAPPER" --shipyard "$SLOW_SHIPYARD" --state-dir "$SLOW_STATE" \
  --status "$SLOW_STATUS" >"$HOME_DIR/slow-second.log" 2>&1
second_exit=$?
set -e
[ "$second_exit" -eq 1 ]
grep -q 'another scheduled cleanup is already running' "$HOME_DIR/slow-second.log"
wait "$slow_pid"
test ! -e "${SLOW_STATUS%.json}.lock"

# Delay the first owner's no-clobber publication. A concurrent starter must
# fail closed on the owner-less lock; it must not reclaim and race the owner.
DELAY_TOOLS="$HOME_DIR/delay-tools"
mkdir -p "$DELAY_TOOLS"
cat >"$DELAY_TOOLS/ln" <<'SH'
#!/bin/sh
sleep 2
exec /bin/ln "$@"
SH
chmod +x "$DELAY_TOOLS/ln"
RACE_STATE="$HOME_DIR/race-state"
RACE_STATUS="$HOME_DIR/race-status.json"
PATH="$DELAY_TOOLS:$PATH" "$WRAPPER" --shipyard "$SLOW_SHIPYARD" \
  --state-dir "$RACE_STATE" --status "$RACE_STATUS" >"$HOME_DIR/race-first.log" 2>&1 &
race_pid=$!
sleep 0.2
set +e
"$WRAPPER" --shipyard "$SLOW_SHIPYARD" --state-dir "$RACE_STATE" \
  --status "$RACE_STATUS" >"$HOME_DIR/race-second.log" 2>&1
race_second_exit=$?
set -e
[ "$race_second_exit" -eq 1 ]
grep -q 'another scheduled cleanup is already running' "$HOME_DIR/race-second.log"
wait "$race_pid"
test ! -e "${RACE_STATUS%.json}.lock"

# If the atomic owner publication itself fails, the wrapper must reclaim the
# lock directory it just created. Leaving it behind would permanently wedge
# every later scheduled cleanup as an owner-less lock.
PUBLISH_FAIL_TOOLS="$HOME_DIR/publish-fail-tools"
mkdir -p "$PUBLISH_FAIL_TOOLS"
cat >"$PUBLISH_FAIL_TOOLS/ln" <<'SH'
#!/bin/sh
exit 1
SH
chmod +x "$PUBLISH_FAIL_TOOLS/ln"
PUBLISH_FAIL_STATUS="$HOME_DIR/publish-fail-status.json"
set +e
PATH="$PUBLISH_FAIL_TOOLS:$PATH" "$WRAPPER" --shipyard "$SLOW_SHIPYARD" \
  --state-dir "$HOME_DIR/publish-fail-state" --status "$PUBLISH_FAIL_STATUS" \
  >"$HOME_DIR/publish-fail.log" 2>&1
publish_fail_exit=$?
set -e
[ "$publish_fail_exit" -eq 1 ]
grep -q 'could not publish cleanup lock owner' "$HOME_DIR/publish-fail.log"
test ! -e "${PUBLISH_FAIL_STATUS%.json}.lock"

STALE_STATUS="$HOME_DIR/stale-status.json"
mkdir -p "${STALE_STATUS%.json}.lock"
printf '999999\n' >"${STALE_STATUS%.json}.lock/owner"
"$WRAPPER" --shipyard "$SLOW_SHIPYARD" --state-dir "$HOME_DIR/stale-state" \
  --status "$STALE_STATUS" >"$HOME_DIR/stale.log" 2>&1
test ! -e "${STALE_STATUS%.json}.lock"

EMPTY_STATUS="$HOME_DIR/empty-status.json"
mkdir -p "${EMPTY_STATUS%.json}.lock"
set +e
"$WRAPPER" --shipyard "$SLOW_SHIPYARD" --state-dir "$HOME_DIR/empty-state" \
  --status "$EMPTY_STATUS" >"$HOME_DIR/empty.log" 2>&1
empty_exit=$?
set -e
[ "$empty_exit" -eq 1 ]
grep -q 'another scheduled cleanup is already running' "$HOME_DIR/empty.log"
test -d "${EMPTY_STATUS%.json}.lock"

FAKE_LAUNCHCTL_LOG="$HOME_DIR/launchctl.log" HOME="$HOME_DIR" \
  SHIPYARD_LAUNCHCTL_BIN="$TOOLS/launchctl" "$INSTALLER" --uninstall >"$HOME_DIR/uninstall"
test ! -e "$PLIST"
test ! -e "$HOME_DIR/Library/Application Support/shipyard/cleanup/run_cleanup.sh"
test ! -e "$HOME_DIR/Library/Application Support/shipyard/cleanup/last-run.json"

echo "cleanup installer and receipt checks passed"
