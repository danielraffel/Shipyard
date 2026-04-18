"""Render the ``post-tag-sync.yml`` GitHub Actions workflow.

Shipyard-owned file: ``release-bot hook install`` writes it, uninstall
deletes it. Never YAML-surgery on the consumer's ``auto-release.yml``.

The workflow:

1. Fires on ``push: tags: [<only_for_tag_pattern>]``.
2. Checks out ``main`` with ``fetch-depth: 0`` + ``fetch-tags: true`` +
   ``RELEASE_BOT_TOKEN`` falling back to ``GITHUB_TOKEN``.
3. Installs shipyard via the official install script, pinned to
   ``SHIPYARD_VERSION``.
4. Runs ``shipyard release-bot hook run --tag "${GITHUB_REF#refs/tags/}"``.
"""

from __future__ import annotations

from dataclasses import dataclass

# Pinned so upgrading shipyard doesn't accidentally drift consumers.
# Bumped in the same PR as the CLI minor that introduces the hook.
DEFAULT_SHIPYARD_VERSION = "0.9.0"

INSTALL_URL = "https://generouscorp.com/Shipyard/install.sh"


@dataclass
class WorkflowOptions:
    """Knobs for rendering ``post-tag-sync.yml``."""

    tag_pattern: str = "v*"
    shipyard_version: str = DEFAULT_SHIPYARD_VERSION
    install_url: str = INSTALL_URL


def render_workflow(opts: WorkflowOptions | None = None) -> str:
    """Return the rendered YAML text for ``post-tag-sync.yml``."""
    o = opts or WorkflowOptions()
    return f"""\
name: Post-tag docs sync

# Installed by `shipyard release-bot hook install`. Shipyard-owned file:
# re-running the install command overwrites this file in place.
#
# Fires after any tag matching the configured pattern lands on the
# default branch. Installs the pinned shipyard CLI, then hands off to
# `shipyard release-bot hook run` which reads `[release.post_tag_hook]`
# from `.shipyard/config.toml` to know what to do.

on:
  push:
    tags: ["{o.tag_pattern}"]

concurrency:
  group: shipyard-post-tag-sync
  cancel-in-progress: false

permissions:
  contents: write

env:
  SHIPYARD_VERSION: "{o.shipyard_version}"

jobs:
  sync:
    name: Regenerate docs for ${{{{ github.ref_name }}}}
    runs-on: ubuntu-latest
    steps:
      - name: Checkout main with full history
        uses: actions/checkout@v5
        with:
          ref: main
          fetch-depth: 0
          fetch-tags: true
          persist-credentials: true
          # RELEASE_BOT_TOKEN if present so the subsequent commit re-
          # triggers CI checks that are gated to non-GITHUB_TOKEN
          # pushes; fall back to GITHUB_TOKEN for bootstrap.
          token: ${{{{ secrets.RELEASE_BOT_TOKEN || secrets.GITHUB_TOKEN }}}}

      - name: Install shipyard (pinned)
        shell: bash
        run: |
          set -euo pipefail
          curl -fsSL "{o.install_url}" | SHIPYARD_VERSION="$SHIPYARD_VERSION" sh
          shipyard --version

      - name: Run post-tag docs sync
        shell: bash
        env:
          GITHUB_TOKEN: ${{{{ secrets.RELEASE_BOT_TOKEN || secrets.GITHUB_TOKEN }}}}
        run: |
          tag="${{GITHUB_REF#refs/tags/}}"
          shipyard release-bot hook run --tag "$tag"
"""
