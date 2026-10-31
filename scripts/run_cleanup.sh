#!/usr/bin/env bash
set -euo pipefail

SHIPYARD=""
STATE_DIR=""
STATUS=""
OLDER_THAN_HOURS=6
INTERVAL=86400

usage() {
  cat <<'USAGE'
usage: run_cleanup.sh --shipyard ABSOLUTE_PATH --state-dir ABSOLUTE_PATH
       --status ABSOLUTE_PATH [--older-than-hours N] [--interval SECONDS]

Run bounded log retention and validation-TMP cleanup, recording one compact
receipt even when either command refuses to act.
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --shipyard) SHIPYARD="${2:-}"; shift 2 ;;
    --state-dir) STATE_DIR="${2:-}"; shift 2 ;;
    --status) STATUS="${2:-}"; shift 2 ;;
    --older-than-hours) OLDER_THAN_HOURS="${2:-}"; shift 2 ;;
    --interval) INTERVAL="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$SHIPYARD:$STATE_DIR:$STATUS" in
  /*:/*:/*) ;;
  *) echo "--shipyard, --state-dir and --status must be absolute paths" >&2; exit 2 ;;
esac
case "$OLDER_THAN_HOURS" in ''|*[!0-9]*) echo "--older-than-hours must be numeric" >&2; exit 2 ;; esac
case "$INTERVAL" in ''|*[!0-9]*) echo "--interval must be numeric" >&2; exit 2 ;; esac
[ "$OLDER_THAN_HOURS" -ge 1 ] || { echo "--older-than-hours must be at least 1" >&2; exit 2; }
[ "$INTERVAL" -ge 900 ] || { echo "--interval must be at least 900 seconds" >&2; exit 2; }
[ -x "$SHIPYARD" ] || { echo "Shipyard executable is unavailable: $SHIPYARD" >&2; exit 2; }
command -v python3 >/dev/null 2>&1 || { echo "python3 is required to write cleanup receipts" >&2; exit 2; }

run_and_capture() {
  local output_var="$1" exit_var="$2"; shift 2
  local output exit_code
  set +e
  output="$("$@" 2>&1)"
  exit_code=$?
  set -e
  printf -v "$output_var" '%s' "$output"
  printf -v "$exit_var" '%s' "$exit_code"
}

mkdir -p "$(dirname "$STATUS")"
LOCK_DIR="${STATUS%.json}.lock"
LOCK_OWNER="$LOCK_DIR/owner"
lock_acquired=0
for _lock_attempt in 1 2; do
  if mkdir "$LOCK_DIR" 2>/dev/null; then
    printf '%s\n' "$$" >"$LOCK_OWNER"
    chmod 600 "$LOCK_OWNER"
    lock_acquired=1
    break
  fi
  owner_pid="$(cat "$LOCK_OWNER" 2>/dev/null || true)"
  owner_command=""
  case "$owner_pid" in
    ''|*[!0-9]*) ;;
    *) owner_command="$(ps -p "$owner_pid" -o command= 2>/dev/null || true)" ;;
  esac
  case "$owner_command" in
    *run_cleanup.sh*)
      echo "another scheduled cleanup is already running for $STATUS" >&2
      exit 1
      ;;
  esac
  if [ -n "$owner_pid" ] && rm -f "$LOCK_OWNER" 2>/dev/null \
    && rmdir "$LOCK_DIR" 2>/dev/null; then
    continue
  fi
  echo "another scheduled cleanup is already running for $STATUS" >&2
  exit 1
done
[ "$lock_acquired" -eq 1 ] || {
  echo "could not acquire scheduled cleanup lock for $STATUS" >&2
  exit 1
}
release_lock() {
  rm -f "$LOCK_OWNER"
  rmdir "$LOCK_DIR" 2>/dev/null || true
}
started_tmp="$(mktemp "$(dirname "$STATUS")/.in-progress.XXXXXX")"
trap 'rm -f "$started_tmp"; release_lock' EXIT
started_at="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
printf '{"schema":"shipyard-cleanup-progress/v1","pid":%s,"started_at":"%s","interval_seconds":%s}\n' \
  "$$" "$started_at" "$INTERVAL" >"$started_tmp"
chmod 600 "$started_tmp"
mv -f "$started_tmp" "${STATUS%.json}.in-progress"
status_written=0
cleanup_status_finish() {
  if [ "$status_written" -eq 1 ]; then
    rm -f "${STATUS%.json}.in-progress"
  fi
  release_lock
}
trap cleanup_status_finish EXIT

# Match the launchd environment: scheduled cleanup must use machine-global
# policy and must not accidentally load a repository-local overlay.
cd /
cleanup_output=""; cleanup_exit=0
validation_output=""; validation_exit=0
run_and_capture cleanup_output cleanup_exit \
  "$SHIPYARD" --state-dir "$STATE_DIR" --json cleanup --apply
run_and_capture validation_output validation_exit \
  "$SHIPYARD" --state-dir "$STATE_DIR" --json cleanup --validation-tmp --apply \
  --older-than-hours "$OLDER_THAN_HOURS" --owned-only

status_tmp="$(mktemp "$(dirname "$STATUS")/.last-run.XXXXXX")"
trap 'rm -f "$status_tmp"; release_lock' EXIT
observed_at="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
status_cleanup_output="${cleanup_output:0:4000}"
status_validation_output="${validation_output:0:4000}"
STATUS_CLEANUP_OUTPUT="$status_cleanup_output" \
STATUS_VALIDATION_OUTPUT="$status_validation_output" \
STATUS_OBSERVED_AT="$observed_at" \
STATUS_CLEANUP_EXIT="$cleanup_exit" \
STATUS_VALIDATION_EXIT="$validation_exit" \
STATUS_INTERVAL_SECONDS="$INTERVAL" \
  python3 - "$status_tmp" <<'PY'
import json
import os
import pathlib
import sys

payload = {
    "schema": "shipyard-cleanup-status/v1",
    "observed_at": os.environ["STATUS_OBSERVED_AT"],
    "ok": os.environ["STATUS_CLEANUP_EXIT"] == "0" and os.environ["STATUS_VALIDATION_EXIT"] == "0",
    "cleanup_exit": int(os.environ["STATUS_CLEANUP_EXIT"]),
    "validation_tmp_exit": int(os.environ["STATUS_VALIDATION_EXIT"]),
    "interval_seconds": int(os.environ["STATUS_INTERVAL_SECONDS"]),
    "cleanup_output": os.environ["STATUS_CLEANUP_OUTPUT"],
    "validation_tmp_output": os.environ["STATUS_VALIDATION_OUTPUT"],
}
path = pathlib.Path(sys.argv[1])
path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY
chmod 600 "$status_tmp"
mv -f "$status_tmp" "$STATUS"
status_written=1
trap - EXIT
rm -f "${STATUS%.json}.in-progress"
release_lock

printf 'scheduled cleanup complete: cleanup_exit=%s validation_tmp_exit=%s\n' \
  "$cleanup_exit" "$validation_exit"
[ "$cleanup_exit" -eq 0 ] && [ "$validation_exit" -eq 0 ]
