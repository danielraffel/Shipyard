#!/usr/bin/env bash
set -euo pipefail

# Shipyard installer — downloads the correct binary for your platform.
#
# Environment variables (all optional):
#
#   SHIPYARD_VERSION   Install a specific version instead of the latest
#                      release. Accepts "v0.22.1", "0.22.1", or "latest".
#                      Default: "latest".
#
#   SHIPYARD_INSTALL_DIR
#                      Where to place the binary + the `sy` symlink.
#                      Default: "${HOME}/.local/bin".
#
#   SHIPYARD_DRY_RUN   When "1", resolve platform + version + install
#                      dir, print them as KEY=value lines, and exit
#                      without downloading or writing anything. Used
#                      by the unit tests; harmless to use manually.
#
# Examples:
#
#   # Default: latest release to ~/.local/bin
#   curl -fsSL https://generouscorp.com/Shipyard/install.sh | bash
#
#   # Pin to a specific version (useful for project-level pins):
#   SHIPYARD_VERSION="v0.22.1" bash install.sh
#
#   # Install somewhere else (e.g. a project-private toolchain dir):
#   SHIPYARD_INSTALL_DIR="${HOME}/.mytools/bin" bash install.sh
#
# The canonical install location is `${HOME}/.local/bin`; the Claude
# Code plugin's auto-install hook, the Codex one-liner, and this
# script all agree on that by default. Override only when you have a
# good reason (e.g. a project wants versioned artifacts side-by-side).

REPO="danielraffel/Shipyard"
INSTALL_DIR="${SHIPYARD_INSTALL_DIR:-${HOME}/.local/bin}"
REQUESTED_VERSION="${SHIPYARD_VERSION:-latest}"

# ── platform detection ──────────────────────────────────────────────

case "$(uname -s)" in
    Darwin)  OS="macos" ;;
    Linux)   OS="linux" ;;
    MINGW*|MSYS*|CYGWIN*) OS="windows" ;;
    *)
        echo "Unsupported OS: $(uname -s)" >&2
        exit 1
        ;;
esac

case "$(uname -m)" in
    arm64|aarch64) ARCH="arm64" ;;
    x86_64|amd64)  ARCH="x64" ;;
    *)
        echo "Unsupported architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

ARTIFACT="shipyard-${OS}-${ARCH}"
if [ "${SHIPYARD_DRY_RUN:-0}" != "1" ]; then
    echo "Detected platform: ${OS}-${ARCH}"
fi

# ── version resolution ──────────────────────────────────────────────
#
# Accept "v0.22.1", "0.22.1", and "latest". Normalize to the tag name
# GitHub's release API uses ("v0.22.1" / "latest").

if [ "${REQUESTED_VERSION}" = "latest" ] || [ -z "${REQUESTED_VERSION}" ]; then
    API_PATH="releases/latest"
    VERSION_LABEL="latest"
else
    TAG="${REQUESTED_VERSION}"
    # Allow "0.22.1" as shorthand for "v0.22.1".
    case "${TAG}" in
        v*) : ;;
        *)  TAG="v${TAG}" ;;
    esac
    API_PATH="releases/tags/${TAG}"
    VERSION_LABEL="${TAG}"
fi

# Dry-run short-circuit — print the resolved config and exit before
# doing any network or filesystem work. Kept minimal + parseable
# (KEY=value lines) for test assertions.
if [ "${SHIPYARD_DRY_RUN:-0}" = "1" ]; then
    echo "OS=${OS}"
    echo "ARCH=${ARCH}"
    echo "ARTIFACT=${ARTIFACT}"
    echo "INSTALL_DIR=${INSTALL_DIR}"
    echo "VERSION_LABEL=${VERSION_LABEL}"
    echo "API_PATH=${API_PATH}"
    exit 0
fi

echo "Resolving ${VERSION_LABEL} from ${REPO}..."

# ── fetch release asset URL ─────────────────────────────────────────

RELEASE_URL=$(curl -sL "https://api.github.com/repos/${REPO}/${API_PATH}" \
    | grep "browser_download_url.*${ARTIFACT}" \
    | head -1 \
    | cut -d '"' -f 4)

if [ -z "${RELEASE_URL}" ]; then
    echo "No binary found for ${ARTIFACT} in ${VERSION_LABEL}." >&2
    echo "Check https://github.com/${REPO}/releases for available builds." >&2
    exit 1
fi

echo "Downloading ${ARTIFACT} (${VERSION_LABEL})..."
mkdir -p "${INSTALL_DIR}"
curl -sL "${RELEASE_URL}" -o "${INSTALL_DIR}/shipyard"
chmod +x "${INSTALL_DIR}/shipyard"

# macOS post-download signature handling.
#
# Two orthogonal problems to handle:
#
# 1. `com.apple.provenance` / `com.apple.quarantine` xattrs from the
#    GitHub download. macOS 26.3+ Gatekeeper SIGKILLs ad-hoc-signed
#    binaries carrying these with "Taskgated Invalid Signature".
#    Always strip them — they're only metadata anyway.
#
# 2. The binary's code signature. Two cases:
#
#    a. Developer-ID-signed + Apple-notarized (shipyard main releases
#       with all 5 signing secrets set — see RELEASING.md). Notarization
#       makes Gatekeeper trust the binary fast (~1s); XProtect skips the
#       deep scan. We must PRESERVE this signature. `codesign --force
#       --sign -` would strip the Developer ID + notarization ticket,
#       defeating exactly the trust we want. On a test install
#       2026-04-23 the ad-hoc re-sign turned v0.35.0's ~1s cold start
#       into a ~6s cold start because XProtect resumed deep-scanning
#       every invocation.
#
#    b. Ad-hoc-signed (forks, local builds, PRs from external
#       contributors where the signing secrets don't propagate). These
#       DO need the `xattr -cr` + local ad-hoc re-sign to stop
#       Taskgated from SIGKILLing them on every launch.
#
# Detection: `codesign -dv` prints `TeamIdentifier=<team>` for
# Developer-ID-signed binaries and `TeamIdentifier=not set` for
# ad-hoc. The presence/absence of a real Team ID is the fastest
# reliable discriminator.
if [ "${OS}" = "macos" ]; then
    xattr -cr "${INSTALL_DIR}/shipyard" 2>/dev/null || true
    if command -v codesign >/dev/null 2>&1; then
        team_line=$(codesign -dv "${INSTALL_DIR}/shipyard" 2>&1 | grep "^TeamIdentifier=") || team_line=""
        if [ -n "${team_line}" ] && [ "${team_line}" != "TeamIdentifier=not set" ]; then
            # Developer-ID signed. Preserve the signature + notarization.
            echo "Detected Developer-ID-signed binary (${team_line#TeamIdentifier=}); preserving notarization."
        else
            # Ad-hoc signed (fork / local / unsigned fallback path).
            # Re-sign locally so Gatekeeper accepts it without the
            # xattr SIGKILL dance. No notarization to lose.
            codesign --force --sign - "${INSTALL_DIR}/shipyard" 2>/dev/null || true
            echo "Detected ad-hoc-signed binary; re-signed locally for Gatekeeper."
        fi
    fi
fi

# `sy` is the short-form alias that shipyard's packaging ships as an
# entry point; mirror it with a symlink here so both names resolve.
ln -sf "${INSTALL_DIR}/shipyard" "${INSTALL_DIR}/sy"

echo ""
echo "Installed shipyard to ${INSTALL_DIR}/shipyard"
echo "Symlink: ${INSTALL_DIR}/sy"
echo ""

# ── PATH hint ───────────────────────────────────────────────────────

if ! echo "${PATH}" | tr ':' '\n' | grep -q "^${INSTALL_DIR}$"; then
    echo "Add ${INSTALL_DIR} to your PATH:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    echo ""
fi

echo "Next steps:"
echo "  shipyard init        # set up a project"
echo "  shipyard doctor      # check your environment"
echo "  shipyard run         # validate current branch"
