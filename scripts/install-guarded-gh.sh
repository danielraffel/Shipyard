#!/usr/bin/env bash
# Install the guarded plain-gh entry point and the real queue-removal guard.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
bin_dir="${SHIPYARD_GH_BIN_DIR:-$HOME/.local/bin}"
guards_dir="${SHIPYARD_GHAPP_GUARDS_DIR:-$HOME/.config/shipyard/guards}"
while (($#)); do
  case "$1" in
    --bin-dir) bin_dir="$2"; shift 2 ;;
    --guards-dir) guards_dir="$2"; shift 2 ;;
    *) echo "usage: $0 [--bin-dir DIR] [--guards-dir DIR]" >&2; exit 2 ;;
  esac
done
mkdir -p "$bin_dir" "$guards_dir"
install -m 0755 "$root/gh" "$bin_dir/gh"
install -m 0755 "$root/ghapp_queue_removal_guard.py" "$guards_dir/queue-removal-guard"
echo "installed guarded gh at $bin_dir/gh"
echo "queue-removal guard at $guards_dir/queue-removal-guard"
echo "put $bin_dir before the native gh directory in PATH"
