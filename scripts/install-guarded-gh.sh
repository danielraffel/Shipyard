#!/usr/bin/env bash
# Install the guarded plain-gh entry point. Guard files are owned by the
# Shipyard binary; install them with `shipyard guards install` so status and
# doctor see one writer and can audit the exact bundled bytes.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
bin_dir="${SHIPYARD_GH_BIN_DIR:-$HOME/.local/bin}"
while (($#)); do
  case "$1" in
    --bin-dir) bin_dir="$2"; shift 2 ;;
    --guards-dir) echo "--guards-dir is obsolete; run 'shipyard guards install'" >&2; exit 2 ;;
    *) echo "usage: $0 [--bin-dir DIR]" >&2; exit 2 ;;
  esac
done
mkdir -p "$bin_dir"
install -m 0755 "$root/gh" "$bin_dir/gh"
echo "installed guarded gh at $bin_dir/gh"
echo "install guards with: shipyard guards install"
echo "put $bin_dir before the native gh directory in PATH"
