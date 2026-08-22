#!/bin/bash
# Host-owned cleanup guardian for the M3 Sandbox daemon canary.
set -u

if [ "$#" -ne 8 ]; then
  exit 64
fi

owner_pid="$1"
binary="$2"
global_dir="$3"
state_dir="$4"
done_file="$5"
receipt="$6"
lease_dir="$7"
guardian_label="$8"

case "$state_dir" in
  /tmp/shipyard-sandbox-m3-*/state) ;;
  *) exit 65 ;;
esac

cleanup() {
  "$binary" --mode isolated --global-dir "$global_dir" --state-dir "$state_dir" daemon stop \
    >/dev/null 2>&1 || true
  deadline=$((SECONDS + 10))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if ! "$binary" --mode isolated --global-dir "$global_dir" --state-dir "$state_dir" \
      --json daemon status 2>/dev/null | /usr/bin/jq -e '.running == true' >/dev/null; then
      tmp="$receipt.tmp.$$"
      /usr/bin/printf '{"schema_version":1,"candidate_stopped":true}\n' > "$tmp"
      /bin/mv "$tmp" "$receipt"
      /bin/rmdir "$lease_dir" 2>/dev/null || true
      trap - EXIT INT TERM HUP
      /bin/launchctl bootout "gui/$(/usr/bin/id -u)/$guardian_label" 2>/dev/null || true
      exit 0
    fi
    /bin/sleep 1
  done
  /usr/bin/printf '{"schema_version":1,"candidate_stopped":false}\n' > "$receipt"
  trap - EXIT INT TERM HUP
  /bin/launchctl bootout "gui/$(/usr/bin/id -u)/$guardian_label" 2>/dev/null || true
  exit 1
}
trap cleanup EXIT INT TERM HUP

while /bin/kill -0 "$owner_pid" 2>/dev/null && [ ! -e "$done_file" ]; do
  /bin/sleep 1
done
