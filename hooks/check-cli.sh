#!/bin/bash
# Check if the shipyard CLI is installed. If not, offer to install it.
# This runs as a SessionStart hook from the Claude Code plugin.

if command -v shipyard &>/dev/null; then
  exit 0
fi

echo ""
echo "[Shipyard] CLI not found on PATH."
echo ""
echo "Install it with:"
echo "  curl -fsSL https://raw.githubusercontent.com/danielraffel/Shipyard/main/install.sh | sh"
echo ""
echo "Or build from source:"
echo "  git clone https://github.com/danielraffel/Shipyard.git && cd Shipyard && pip install -e ."
echo ""

exit 0
