#!/usr/bin/env bash
set -euo pipefail

# Shipyard installer — downloads the correct binary for your platform.

REPO="danielraffel/Shipyard"
INSTALL_DIR="${HOME}/.local/bin"

# Detect OS
case "$(uname -s)" in
    Darwin)  OS="macos" ;;
    Linux)   OS="linux" ;;
    MINGW*|MSYS*|CYGWIN*) OS="windows" ;;
    *)
        echo "Unsupported OS: $(uname -s)"
        exit 1
        ;;
esac

# Detect architecture
case "$(uname -m)" in
    arm64|aarch64) ARCH="arm64" ;;
    x86_64|amd64)  ARCH="x64" ;;
    *)
        echo "Unsupported architecture: $(uname -m)"
        exit 1
        ;;
esac

ARTIFACT="shipyard-${OS}-${ARCH}"
echo "Detected platform: ${OS}-${ARCH}"

# Get latest release URL
RELEASE_URL=$(curl -sL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep "browser_download_url.*${ARTIFACT}" \
    | head -1 \
    | cut -d '"' -f 4)

if [ -z "${RELEASE_URL}" ]; then
    echo "No binary found for ${ARTIFACT} in latest release."
    echo "Check https://github.com/${REPO}/releases for available builds."
    exit 1
fi

echo "Downloading ${ARTIFACT}..."
mkdir -p "${INSTALL_DIR}"
curl -sL "${RELEASE_URL}" -o "${INSTALL_DIR}/shipyard"
chmod +x "${INSTALL_DIR}/shipyard"

# macOS 26.3+ hardened Gatekeeper SIGKILLs ad-hoc-signed binaries that
# carry the `com.apple.provenance` / `com.apple.quarantine` xattrs from
# a GitHub Release download. PyInstaller produces ad-hoc-signed Mach-O,
# so a fresh install silently crashes with "killed" on every run and
# the user has to dig through ~/Library/Logs/DiagnosticReports to see
# the real reason ("Taskgated Invalid Signature" / SIGKILL Code
# Signature Invalid).
#
# Fix: strip the download xattrs + re-apply an ad-hoc signature on the
# user's machine. The re-sign produces a trust-anchored signature
# because it's local (no download-origin tracking). This is the same
# two-command dance every PyInstaller / binary-distribution project
# on macOS ends up running; folding it into the installer saves every
# user from hitting it.
if [ "${OS}" = "macos" ]; then
    xattr -cr "${INSTALL_DIR}/shipyard" 2>/dev/null || true
    if command -v codesign >/dev/null 2>&1; then
        codesign --force --sign - "${INSTALL_DIR}/shipyard" 2>/dev/null || true
    fi
fi

# Create sy symlink
ln -sf "${INSTALL_DIR}/shipyard" "${INSTALL_DIR}/sy"

echo ""
echo "Installed shipyard to ${INSTALL_DIR}/shipyard"
echo "Symlink: ${INSTALL_DIR}/sy"
echo ""

# Check if install dir is in PATH
if ! echo "${PATH}" | tr ':' '\n' | grep -q "^${INSTALL_DIR}$"; then
    echo "Add ${INSTALL_DIR} to your PATH:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    echo ""
fi

echo "Next steps:"
echo "  shipyard init        # set up a project"
echo "  shipyard doctor      # check your environment"
echo "  shipyard run         # validate current branch"
