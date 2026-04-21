#!/bin/bash
# SessionStart hook for the Claude Code plugin.
#
# Three jobs:
#   1. Bootstrap: if no `shipyard` binary is on PATH, curl install.sh
#      (best-effort). The plugin is deferential — it only installs
#      when something's actually missing.
#   2. Staleness signal: if a shipyard binary IS on PATH but is older
#      than this plugin build's `min_shipyard_version`, emit a Claude
#      Code SessionStart JSON response with `systemMessage` (renders
#      as a user-visible banner at session start) and
#      `hookSpecificOutput.additionalContext` (agent-facing).
#   3. Honor a user's prior "don't ask again" choice via a dismiss
#      file under the shipyard state dir. Re-prompts only when the
#      plugin raises its min_shipyard_version past what was dismissed.
#
# We never auto-upgrade. Project pinners (e.g. pulp with
# tools/shipyard.toml) rely on the plugin not surprise-upgrading
# their CLI.

set -u

# ── State-dir resolution (matches shipyard/core/config.py) ──────────

case "$(uname -s)" in
    Darwin)  SHIPYARD_STATE_DIR="$HOME/Library/Application Support/shipyard" ;;
    Linux)   SHIPYARD_STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/shipyard" ;;
    *)       SHIPYARD_STATE_DIR="$HOME/.shipyard" ;;  # fallback for Windows / weird hosts
esac
DISMISS_FILE="$SHIPYARD_STATE_DIR/plugin-upgrade-dismissed.json"

# ── Bootstrap ───────────────────────────────────────────────────────

if ! command -v shipyard &>/dev/null; then
  echo ""
  echo "[Shipyard] CLI binary not found. Installing..."
  echo ""
  curl -fsSL https://raw.githubusercontent.com/danielraffel/Shipyard/main/install.sh | sh
  if command -v shipyard &>/dev/null; then
    echo ""
    echo "[Shipyard] Installed successfully: $(shipyard --version)"
  elif [ -f "$HOME/.local/bin/shipyard" ]; then
    echo ""
    echo "[Shipyard] Installed to ~/.local/bin/shipyard"
    echo "[Shipyard] Add to PATH: export PATH=\"\$HOME/.local/bin:\$PATH\""
  else
    echo ""
    echo "[Shipyard] Installation may have failed. Try manually:"
    echo "  curl -fsSL https://raw.githubusercontent.com/danielraffel/Shipyard/main/install.sh | sh"
  fi
  exit 0
fi

# ── Staleness check ─────────────────────────────────────────────────

PLUGIN_JSON="${CLAUDE_PLUGIN_ROOT:-}/.claude-plugin/plugin.json"
if [ -z "${CLAUDE_PLUGIN_ROOT:-}" ] || [ ! -f "$PLUGIN_JSON" ]; then
  exit 0
fi

MIN_VERSION=$(sed -n 's/^[[:space:]]*"min_shipyard_version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$PLUGIN_JSON" | head -1)
if [ -z "$MIN_VERSION" ]; then
  exit 0
fi

INSTALLED=$(shipyard --version 2>/dev/null | awk 'NF{print $NF}' | head -1 | sed 's/^v//')
if [ -z "$INSTALLED" ]; then
  exit 0
fi

MIN_CMP="${MIN_VERSION#v}"

# Pure-shell version compare via sort -V. If MIN_CMP sorts first or
# is equal to INSTALLED, installed is >= min (no action).
version_gte() {
    # $1 >= $2 ?
    [ "$1" = "$2" ] && return 0
    [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -1)" = "$2" ]
}

if version_gte "$INSTALLED" "$MIN_CMP"; then
  exit 0
fi

# ── Honor a prior "don't ask again" dismissal ───────────────────────

if [ -f "$DISMISS_FILE" ]; then
  DISMISSED_MIN=$(sed -n 's/.*"dismissed_for_min"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$DISMISS_FILE" | head -1)
  DISMISSED_CMP="${DISMISSED_MIN#v}"
  if [ -n "$DISMISSED_CMP" ] && version_gte "$DISMISSED_CMP" "$MIN_CMP"; then
    # User already dismissed this (or a later) min version. Stay silent.
    exit 0
  fi
fi

# ── Emit the SessionStart JSON response ─────────────────────────────
#
# Plain-text stdout goes into the agent's context but agents often
# treat it as ignorable ambient info. Using the structured JSON
# response renders `systemMessage` as a user-visible banner at session
# start (doesn't depend on the agent saying anything) and separately
# populates `additionalContext` for when the user does ask the agent
# about the upgrade.

if command -v python3 &>/dev/null; then
  INSTALLED="$INSTALLED" MIN_VERSION="$MIN_VERSION" \
    SHIPYARD_STATE_DIR="$SHIPYARD_STATE_DIR" DISMISS_FILE="$DISMISS_FILE" \
    python3 <<'PY'
import json, os
installed = os.environ['INSTALLED']
minv      = os.environ['MIN_VERSION']
sdir      = os.environ['SHIPYARD_STATE_DIR']
dfile     = os.environ['DISMISS_FILE']

print(json.dumps({
    "systemMessage": (
        f"Shipyard CLI is on {installed}; plugin expects ≥ {minv}. "
        f"Run /shipyard:upgrade to update."
    ),
    "hookSpecificOutput": {
        "hookEventName": "SessionStart",
        "additionalContext": (
            f"The installed shipyard CLI is {installed} but this plugin "
            f"expects >= {minv}. If the user asks about upgrading, suggest "
            f"/shipyard:upgrade. If they say their install is project-pinned "
            f"(e.g. pulp's tools/install-shipyard.sh) and want to silence "
            f"this on future sessions, write the dismiss file:\n"
            f'  mkdir -p "{sdir}"\n'
            f'  printf \'%s\\n\' \'{{"dismissed_for_min":"{minv}"}}\' '
            f'> "{dfile}"'
        ),
    },
}))
PY
else
  # Fallback: plain text for environments without python3. Renders as
  # agent context only — agents may or may not surface it.
  cat <<EOF

[Shipyard] SHIPYARD_CLI_STALE installed=${INSTALLED} min_expected=${MIN_VERSION}
[Shipyard] Run /shipyard:upgrade to update the CLI.

EOF
fi
exit 0
