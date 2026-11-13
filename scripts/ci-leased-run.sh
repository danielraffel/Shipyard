#!/usr/bin/env bash
# Run one CI command on a self-hosted runner without taking the whole host.
#
#   scripts/ci-leased-run.sh cargo test --all-targets --locked
#
# Shipyard's self-hosted Macs are also gate hosts for other repositories.
# Unbounded, `cargo test --all-targets` builds and tests with one job per core,
# which on m1 pushed the load average past 100 and timed out the gate lanes
# beside it. On a self-hosted runner this wrapper:
#
# 1. asks tartci for a background-class build lease (when `tartci` is on PATH
#    or SHIPYARD_TARTCI_BIN names it), waits up to
#    SHIPYARD_CI_LEASE_WAIT_SECS for one, keeps it alive with a heartbeat and
#    releases it on exit;
# 2. caps cargo's build jobs and the test harness's threads at the granted
#    share, or at SHIPYARD_CI_FLOOR_JOBS (default 2) under utility QoS when no
#    lease is granted, so the job is bounded either way.
#
# tartci keeps its lease store in the invoking user's home. A runner that runs
# as its own user only competes with the host's gate lanes if its environment
# sets TARTCI_LEASE_DIR to the store those lanes use.
#
# On a GitHub-hosted runner the command runs unchanged.
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "ci-leased-run: usage: ci-leased-run.sh <command...>" >&2
  exit 2
fi

log() { echo "[ci-leased-run] $*" >&2; }

if [ "${RUNNER_ENVIRONMENT:-}" != "self-hosted" ] && [ "${SHIPYARD_CI_LEASE_FORCE:-}" != "1" ]; then
  exec "$@"
fi

positive_int() {
  case "${1:-}" in ''|*[!0-9]*) return 1 ;; esac
  [ "$1" -ge 1 ]
}

WANT="${SHIPYARD_CI_JOBS:-4}"
positive_int "$WANT" || WANT=4
FLOOR="${SHIPYARD_CI_FLOOR_JOBS:-2}"
positive_int "$FLOOR" || FLOOR=2
[ "$FLOOR" -le "$WANT" ] || FLOOR="$WANT"
WAIT="${SHIPYARD_CI_LEASE_WAIT_SECS:-900}"
positive_int "$WAIT" || WAIT=900
HEARTBEAT="${SHIPYARD_CI_LEASE_HEARTBEAT_SECS:-60}"
positive_int "$HEARTBEAT" || HEARTBEAT=60

TARTCI="${SHIPYARD_TARTCI_BIN:-$(command -v tartci 2>/dev/null || true)}"
[ -n "$TARTCI" ] && [ -x "$TARTCI" ] || TARTCI=""

LEASE_ID=""
HEARTBEAT_PID=""

cleanup() {
  if [ -n "$HEARTBEAT_PID" ]; then
    kill "$HEARTBEAT_PID" 2>/dev/null || true
    wait "$HEARTBEAT_PID" 2>/dev/null || true
    HEARTBEAT_PID=""
  fi
  if [ -n "$LEASE_ID" ]; then
    "$TARTCI" leases release --id "$LEASE_ID" --json >/dev/null 2>&1 || true
    log "released lease id=$LEASE_ID"
    LEASE_ID=""
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Prints the granted core count for a well-formed grant; anything else is no grant.
granted_cores() {
  local out="$1" size
  grep -q '"ok"[[:space:]]*:[[:space:]]*true' <<<"$out" || return 1
  size="$(grep -o '"lease_size_cores"[[:space:]]*:[[:space:]]*[0-9][0-9]*' <<<"$out" \
    | grep -o '[0-9][0-9]*$' | sed -n 1p || true)"
  positive_int "$size" && [ "$size" -le "$WANT" ] || return 1
  echo "$size"
}

jobs=""
if [ -n "$TARTCI" ]; then
  id="shipyard-ci-${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-0}-${GITHUB_JOB:-job}-$$"
  common=(--id "$id" --cores "$WANT" --priority build --kind shipyard-ci --owner shipyard-ci
          --pid "$$" --job-id "${GITHUB_RUN_ID:-}" --repo "${GITHUB_REPOSITORY:-}" --json)
  rc=0
  out="$("$TARTCI" leases acquire "${common[@]}" --class background --min-cores "$FLOOR" \
    --wait-secs "$WAIT" 2>/dev/null)" || rc=$?
  if [ "$rc" = 2 ]; then
    # A tartci too old for class-aware leases: one plain request.
    out="$("$TARTCI" leases acquire "${common[@]}" 2>/dev/null)" || true
  fi
  if size="$(granted_cores "$out")"; then
    LEASE_ID="$id"
    jobs="$size"
    log "lease id=$LEASE_ID granted $jobs of $WANT cores"
    parent=$$
    (
      trap 'exit 0' TERM INT
      while :; do
        sleep "$HEARTBEAT" &
        wait $! 2>/dev/null || exit 0
        kill -0 "$parent" 2>/dev/null || exit 0
        "$TARTCI" leases heartbeat --id "$id" --json >/dev/null 2>&1 || true
      done
    ) >/dev/null 2>&1 &
    HEARTBEAT_PID=$!
  else
    # A malformed or oversized grant must not stay held.
    "$TARTCI" leases release --id "$id" --json >/dev/null 2>&1 || true
    log "no lease granted within ${WAIT}s; running at the floor of $FLOOR jobs"
  fi
else
  log "tartci not found; running at the floor of $FLOOR jobs"
fi

prefix=()
if [ -z "$jobs" ]; then
  jobs="$FLOOR"
  if command -v taskpolicy >/dev/null 2>&1; then
    prefix=(taskpolicy -c utility)
  fi
fi

export CARGO_BUILD_JOBS="$jobs" RUST_TEST_THREADS="$jobs"
log "CARGO_BUILD_JOBS=$jobs RUST_TEST_THREADS=$jobs: $*"
# Run as a child, not exec, so the lease is released when the command ends.
rc=0
"${prefix[@]+"${prefix[@]}"}" "$@" || rc=$?
exit "$rc"
