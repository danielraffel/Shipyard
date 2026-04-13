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
#   1. Bump pyproject.toml and __init__.py
#   2. Commit the bump
#   3. Tag and push — triggers release.yml binary build

BUMP="${1:-}"

if [ -z "$BUMP" ]; then
  echo "Usage: ./scripts/release.sh <patch|minor|major|X.Y.Z>"
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

# Get current version from pyproject.toml
CURRENT=$(grep '^version = ' pyproject.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
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

# Confirm
read -p "Release v${NEW}? [y/N] " -n 1 -r
echo ""
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
  echo "Cancelled."
  exit 0
fi

# Update version in pyproject.toml
sed -i '' "s/^version = \"$CURRENT\"/version = \"$NEW\"/" pyproject.toml

# Update version in __init__.py
sed -i '' "s/__version__ = \"$CURRENT\"/__version__ = \"$NEW\"/" src/shipyard/__init__.py

# Update version in plugin files
sed -i '' "s/\"version\": \"$CURRENT\"/\"version\": \"$NEW\"/g" .claude-plugin/plugin.json
sed -i '' "s/\"version\": \"$CURRENT\"/\"version\": \"$NEW\"/g" .claude-plugin/marketplace.json

# Commit and tag
git add pyproject.toml src/shipyard/__init__.py .claude-plugin/plugin.json .claude-plugin/marketplace.json
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
