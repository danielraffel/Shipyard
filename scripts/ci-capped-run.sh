#!/usr/bin/env bash
# Run one CI command on a self-hosted runner without taking the whole host.
#
#   scripts/ci-capped-run.sh cargo test --all-targets --locked
#
# Shipyard's self-hosted Macs are also gate hosts for other repositories.
# Unbounded, `cargo test --all-targets` builds and tests with one job per core,
# which on m1 pushed the load average past 100 and timed out the gate lanes
# beside it. On a self-hosted runner this caps cargo's build jobs and the test
# harness's threads at SHIPYARD_CI_JOBS (default 2) and runs the command under
# utility QoS. The cap is intentional: it is the whole mechanism.
#
# On a GitHub-hosted runner the command runs unchanged.
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "ci-capped-run: usage: ci-capped-run.sh <command...>" >&2
  exit 2
fi

if [ "${RUNNER_ENVIRONMENT:-}" != "self-hosted" ] && [ "${SHIPYARD_CI_CAP_FORCE:-}" != "1" ]; then
  exec "$@"
fi

jobs="${SHIPYARD_CI_JOBS:-2}"
case "$jobs" in ''|*[!0-9]*|0) jobs=2 ;; esac
export CARGO_BUILD_JOBS="$jobs" RUST_TEST_THREADS="$jobs"
echo "[ci-capped-run] CARGO_BUILD_JOBS=$jobs RUST_TEST_THREADS=$jobs: $*" >&2

if command -v taskpolicy >/dev/null 2>&1; then
  exec taskpolicy -c utility "$@"
fi
exec "$@"
