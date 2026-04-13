#!/bin/bash
# Layer-1 agent hook: run the versioning + skill-sync gates in --mode=hint
# after every Edit|Write so drift is surfaced while iterating rather than
# at pr-push time. Advisory only — never blocks. Does not interfere with
# the SessionStart CLI-install hook.

FILE=$(echo "$TOOL_INPUT" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    print(data.get('file_path', ''))
except Exception:
    pass
" 2>/dev/null)

if [ -z "$FILE" ]; then
    exit 0
fi

# Locate repo root by walking up from the edited file.
REPO_ROOT=""
candidate="$(dirname "$FILE")"
while [ -n "$candidate" ] && [ "$candidate" != "/" ]; do
    if [ -f "$candidate/scripts/versioning.json" ]; then
        REPO_ROOT="$candidate"
        break
    fi
    candidate="$(dirname "$candidate")"
done

if [ -z "$REPO_ROOT" ]; then
    exit 0
fi

VBC="$REPO_ROOT/scripts/version_bump_check.py"
SSC="$REPO_ROOT/scripts/skill_sync_check.py"
CFG="$REPO_ROOT/scripts/versioning.json"

if [ -x "$VBC" ]; then
    "$VBC" --base origin/main --config "$CFG" --mode=hint 2>/dev/null || true
fi
if [ -x "$SSC" ]; then
    "$SSC" --base origin/main --config "$CFG" --mode=hint 2>/dev/null || true
fi
