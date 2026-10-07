#!/bin/sh
# PostToolUse entry for the PR hand-back inbox. It runs on every tool call, so
# it does the least possible: take the session id from the head of the hook
# payload, test whether that session's inbox is non-empty, and only then start
# handback-inbox.py. It prints nothing and starts nothing otherwise, and it
# never fails the tool call.
head=$(dd bs=4096 count=1 2>/dev/null)
cat >/dev/null 2>&1
sid=$(printf '%s' "$head" | sed -n 's/.*"session_id"[[:space:]]*:[[:space:]]*"\([A-Za-z0-9_:][A-Za-z0-9._:-]*\)".*/\1/p' | head -n 1)
[ -n "$sid" ] || exit 0
dir=${SHIPYARD_INBOX_DIR:-$HOME/.local/state/shipyard/inbox}
[ -s "$dir/$sid.jsonl" ] || exit 0
printf '{"session_id": "%s", "hook_event_name": "PostToolUse"}' "$sid" |
  python3 "$(dirname "$0")/handback-inbox.py" || true
exit 0
