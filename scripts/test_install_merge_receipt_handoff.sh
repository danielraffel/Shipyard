#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
FAKE_SHIPYARD="$TMP/shipyard"
FAKE_PYTHON="$(command -v python3)"
FAKE_LAUNCHCTL="$TMP/launchctl"
cat > "$FAKE_SHIPYARD" <<'EOF'
#!/bin/sh
exit 0
EOF
cat > "$FAKE_LAUNCHCTL" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "${SHIPYARD_TEST_LAUNCHCTL_LOG:?}"
exit 0
EOF
chmod +x "$FAKE_SHIPYARD" "$FAKE_LAUNCHCTL"
export HOME="$TMP/home" SHIPYARD_LAUNCHCTL_BIN="$FAKE_LAUNCHCTL" SHIPYARD_TEST_LAUNCHCTL_LOG="$TMP/launchctl.log"
mkdir -p "$HOME"
"$ROOT/scripts/install_merge_receipt_handoff.sh" --repo owner/repo --shipyard "$FAKE_SHIPYARD" --python "$FAKE_PYTHON" --interval 900 >"$TMP/dry"
grep -q 'action=dry-run' "$TMP/dry"
"$ROOT/scripts/install_merge_receipt_handoff.sh" --repo owner/repo --shipyard "$FAKE_SHIPYARD" --python "$FAKE_PYTHON" --interval 900 --install >"$TMP/install"
PLIST="$HOME/Library/LaunchAgents/com.danielraffel.shipyard.merge-receipt-handoff.plist"
[ -f "$PLIST" ]
/usr/bin/plutil -lint "$PLIST" >/dev/null
grep -q 'bootstrap gui/' "$TMP/launchctl.log"
grep -q 'owner/repo' "$PLIST"
grep -q -- '--base-branch' "$PLIST"
"$ROOT/scripts/install_merge_receipt_handoff.sh" --repo owner/repo --python "$FAKE_PYTHON" \
  --source-host m3 --source-shipyard /Users/danielraffel/.local/bin/shipyard \
  --source-state-dir '/Users/danielraffel/Library/Application Support/shipyard' \
  --interval 900 --install >"$TMP/observer-install"
grep -q '<string>fetch</string>' "$PLIST"
grep -q '<string>m3</string>' "$PLIST"
grep -q '<string>--source-shipyard</string>' "$PLIST"
if grep -q '<string>export</string>' "$PLIST"; then exit 1; fi
echo "merge receipt handoff installer checks passed"
