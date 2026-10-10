#!/bin/bash
set -euo pipefail

# Release a new version of Shipyard — MANUAL FALLBACK PATH.
#
# The default release flow is: open a PR via `shipyard pr` (or
# `shipyard ship`), let CI validate + merge, and let
# .github/workflows/auto-release.yml create the tag on push to main. The
# existing tag-triggered release.yml then builds and publishes.
#
# This script is the break-glass path when the automatic flow is
# unavailable (e.g. the auto-release workflow is disabled, or an
# emergency hotfix needs direct tag control).
#
# Usage:
#   ./scripts/release.sh patch    # 0.1.0 → 0.1.1
#   ./scripts/release.sh minor    # 0.1.0 → 0.2.0
#   ./scripts/release.sh major    # 0.1.0 → 1.0.0
#   ./scripts/release.sh 0.3.0    # explicit version
#
# Steps:
#   0. Pre-release version-bump gate (fails fast if bumps are missing)
#   1. Bump Cargo.toml package.version
#   2. Commit the bump
#   3. Tag and push — triggers release.yml binary build

DRY_RUN=0
if [ "${1:-}" = "--dry-run" ]; then
  DRY_RUN=1
  shift
fi
BUMP="${1:-}"

if [ -z "$BUMP" ]; then
  echo "Usage: ./scripts/release.sh [--dry-run] <patch|minor|major|X.Y.Z>"
  exit 1
fi

# ── Pre-release version-bump gate ───────────────────────────────────────
# Refuse to release when version_bump_check would fail against
# origin/main. Set RELEASE_SKIP_VERSION_CHECK=1 with a logged reason to
# bypass (rare — only when this script is being used precisely because
# the automatic path refused to tag).
if [ "${RELEASE_SKIP_VERSION_CHECK:-0}" != "1" ]; then
  if [ -x "scripts/version_bump_check.py" ] && [ -f "scripts/versioning.json" ]; then
    echo "▸ Pre-release version-bump gate..."
    if ! python3 scripts/version_bump_check.py \
           --base origin/main \
           --config scripts/versioning.json \
           --mode=report; then
      echo "release.sh: version_bump_check failed — fix the bump or rerun with" >&2
      echo "            RELEASE_SKIP_VERSION_CHECK=1 and a commit trailer reason." >&2
      exit 1
    fi
  fi
fi

# Get current version from Cargo.toml's [package] table.
CURRENT=$(python3 - <<'PY'
import re
from pathlib import Path

text = Path("Cargo.toml").read_text(encoding="utf-8")
match = re.search(r'(?ms)^\[package\].*?^version\s*=\s*"([^"]+)"', text)
if not match:
    raise SystemExit("could not read Cargo.toml package.version")
print(match.group(1))
PY
)
echo "Current version: $CURRENT"

# Calculate new version
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"
case "$BUMP" in
  patch) NEW="$MAJOR.$MINOR.$((PATCH + 1))" ;;
  minor) NEW="$MAJOR.$((MINOR + 1)).0" ;;
  major) NEW="$((MAJOR + 1)).0.0" ;;
  [0-9]*) NEW="$BUMP" ;;
  *) echo "Invalid bump: $BUMP (use patch, minor, major, or X.Y.Z)"; exit 1 ;;
esac

echo "New version: $NEW"
echo ""

if [ "$DRY_RUN" -eq 1 ]; then
  echo "Dry run: would update Cargo.toml, Cargo.lock, .claude-plugin/plugin.json, and .claude-plugin/marketplace.json"
  echo "Dry run: would create tag v${NEW} and let release.yml publish it."
  exit 0
fi

# Confirm
read -p "Release v${NEW}? [y/N] " -n 1 -r
echo ""
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
  echo "Cancelled."
  exit 0
fi

# Update Cargo package version.
python3 - "$CURRENT" "$NEW" <<'PY'
import re
import sys
from pathlib import Path

current, new = sys.argv[1], sys.argv[2]
path = Path("Cargo.toml")
text = path.read_text(encoding="utf-8")
updated = re.sub(
    r'(?ms)(^\[package\].*?^version\s*=\s*")' + re.escape(current) + r'(")',
    lambda match: f"{match.group(1)}{new}{match.group(2)}",
    text,
    count=1,
)
if updated == text:
    raise SystemExit("failed to update Cargo.toml package.version")
path.write_text(updated, encoding="utf-8")
PY

# Update version in plugin files
sed -i '' "s/\"version\": \"$CURRENT\"/\"version\": \"$NEW\"/g" .claude-plugin/plugin.json
sed -i '' "s/\"version\": \"$CURRENT\"/\"version\": \"$NEW\"/g" .claude-plugin/marketplace.json
cargo generate-lockfile

# Commit and tag
git add Cargo.toml Cargo.lock .claude-plugin/plugin.json .claude-plugin/marketplace.json
git commit -m "Release v${NEW}"
git tag -a "v${NEW}" -m "Shipyard v${NEW}"

# Push
echo ""
echo "Pushing tag v${NEW}..."
git push origin main
git push origin "v${NEW}"

echo ""
echo "Done. Release workflow will build binaries and publish to:"
echo "  https://github.com/danielraffel/Shipyard/releases/tag/v${NEW}"
echo ""
echo "Monitor with:"
echo "  gh run list --repo danielraffel/Shipyard --limit 3"
