#!/usr/bin/env bash
set -euo pipefail

# Shipyard installer: downloads the correct production binary for your
# platform and installs it as `shipyard` with the `sy` convenience symlink.
#
# Environment variables:
#   SHIPYARD_VERSION          Tag to install, or "latest" (default)
#   SHIPYARD_INSTALL_DIR      Install directory (default: ~/.local/bin)
#   SHIPYARD_DRY_RUN          Print resolved settings and exit
#   SHIPYARD_SKIP_DOWNLOAD    Reuse an existing binary in install dir
#   SHIPYARD_SKIP_SMOKE       Skip post-install --version smoke
#   SHIPYARD_EXPECTED_BINARY_SHA256
#                             Require these exact staged and installed bytes
#   SHIPYARD_REPO             Override release repo
#   SHIPYARD_GITHUB_TOKEN     Optional token for private release repos

REPO="${SHIPYARD_REPO:-danielraffel/Shipyard}"
INSTALL_DIR="${SHIPYARD_INSTALL_DIR:-${HOME}/.local/bin}"
REQUESTED_VERSION="${SHIPYARD_VERSION:-latest}"
GITHUB_TOKEN_VALUE="${SHIPYARD_GITHUB_TOKEN:-${GITHUB_TOKEN:-}}"
EXPECTED_BINARY_SHA256="${SHIPYARD_EXPECTED_BINARY_SHA256:-}"

binary_sha256() {
    path="$1"
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "${path}" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "${path}" | awk '{print $1}'
    else
        openssl dgst -sha256 "${path}" | awk '{print $NF}'
    fi
}

verify_expected_binary_sha256() {
    path="$1"
    phase="$2"
    [ -z "${EXPECTED_BINARY_SHA256}" ] && return 0
    case "${EXPECTED_BINARY_SHA256}" in
        *[!0-9a-fA-F]*|'')
            echo "SHIPYARD_EXPECTED_BINARY_SHA256 must be exactly 64 hexadecimal characters." >&2
            exit 1
            ;;
    esac
    if [ "${#EXPECTED_BINARY_SHA256}" -ne 64 ]; then
        echo "SHIPYARD_EXPECTED_BINARY_SHA256 must be exactly 64 hexadecimal characters." >&2
        exit 1
    fi
    actual_sha256="$(binary_sha256 "${path}")"
    if [ "$(printf '%s' "${actual_sha256}" | tr '[:upper:]' '[:lower:]')" != "$(printf '%s' "${EXPECTED_BINARY_SHA256}" | tr '[:upper:]' '[:lower:]')" ]; then
        echo "Shipyard ${phase} binary SHA-256 mismatch: expected ${EXPECTED_BINARY_SHA256}, got ${actual_sha256}." >&2
        exit 1
    fi
}

curl_shipyard() {
    if [ -n "${GITHUB_TOKEN_VALUE}" ]; then
        curl -H "Authorization: Bearer ${GITHUB_TOKEN_VALUE}" "$@"
    else
        curl "$@"
    fi
}

select_asset_url() {
    asset_name="$1"
    prefer_api_url="$2"
    command -v python3 >/dev/null 2>&1 || return 1
    python3 -c '
import json
import sys

asset_name = sys.argv[1]
prefer_api_url = sys.argv[2] == "1"
payload = json.load(sys.stdin)
for asset in payload.get("assets", []):
    if asset.get("name") == asset_name:
        key = "url" if prefer_api_url else "browser_download_url"
        print(asset.get(key, ""))
        break
' "${asset_name}" "${prefer_api_url}"
}

download_asset() {
    url="$1"
    output="$2"
    api_asset="$3"
    if [ "${api_asset}" = "1" ]; then
        curl_shipyard -sL -H "Accept: application/octet-stream" "${url}" -o "${output}"
    else
        curl_shipyard -sL "${url}" -o "${output}"
    fi
}

ARTIFACT_PREFIX="${SHIPYARD_ARTIFACT_PREFIX:-shipyard}"
BINARY_NAME="shipyard"
ALIAS_NAME="sy"

UNAME_S="${SHIPYARD_INSTALL_TEST_UNAME_S:-$(uname -s)}"
UNAME_M="${SHIPYARD_INSTALL_TEST_UNAME_M:-$(uname -m)}"

case "${UNAME_S}" in
    Darwin)  OS="macos" ;;
    Linux)   OS="linux" ;;
    MINGW*|MSYS*|CYGWIN*) OS="windows" ;;
    *)
        echo "Unsupported OS: ${UNAME_S}" >&2
        exit 1
        ;;
esac

case "${UNAME_M}" in
    arm64|aarch64) ARCH="arm64" ;;
    x86_64|amd64)  ARCH="x64" ;;
    *)
        echo "Unsupported architecture: ${UNAME_M}" >&2
        exit 1
        ;;
esac

ARTIFACT="${ARTIFACT_PREFIX}-${OS}-${ARCH}"
if [ "$OS" = "windows" ]; then
    ARTIFACT="${ARTIFACT}.exe"
    BINARY_NAME="${BINARY_NAME}.exe"
fi

if [ "${REQUESTED_VERSION}" = "latest" ] || [ -z "${REQUESTED_VERSION}" ]; then
    API_PATH="releases/latest"
    VERSION_LABEL="latest"
else
    TAG="${REQUESTED_VERSION}"
    case "${TAG}" in
        v*) : ;;
        *) TAG="v${TAG}" ;;
    esac
    API_PATH="releases/tags/${TAG}"
    VERSION_LABEL="${TAG}"
fi

# Match current mainline policy: macOS x86_64 is unsupported from
# v0.50.0 onward, but older pinned versions may still install if they
# shipped Intel artifacts.
if [ "$OS" = "macos" ] && [ "$ARCH" = "x64" ]; then
    intel_blocked=0
    if [ "${VERSION_LABEL}" = "latest" ]; then
        intel_blocked=1
    else
        ver="${VERSION_LABEL#v}"
        major="${ver%%.*}"
        rest="${ver#*.}"
        minor="${rest%%.*}"
        if [ -n "$major" ] && [ -n "$minor" ] \
                && { [ "$major" -gt 0 ] \
                     || { [ "$major" -eq 0 ] && [ "$minor" -ge 50 ]; }; } 2>/dev/null; then
            intel_blocked=1
        fi
    fi
    if [ "$intel_blocked" -eq 1 ]; then
        echo "Intel Macs (x86_64) are not supported by Shipyard v0.50.0 and later." >&2
        echo "Apple Silicon (arm64) Macs only." >&2
        echo "Pin SHIPYARD_VERSION=v0.49.0 if you need an older Intel-capable release." >&2
        exit 2
    fi
fi

if [ "${SHIPYARD_DRY_RUN:-0}" = "1" ]; then
    echo "REPO=${REPO}"
    echo "OS=${OS}"
    echo "ARCH=${ARCH}"
    echo "ARTIFACT_PREFIX=${ARTIFACT_PREFIX}"
    echo "ARTIFACT=${ARTIFACT}"
    echo "BINARY_NAME=${BINARY_NAME}"
    echo "ALIAS_NAME=${ALIAS_NAME}"
    echo "INSTALL_DIR=${INSTALL_DIR}"
    echo "VERSION_LABEL=${VERSION_LABEL}"
    echo "API_PATH=${API_PATH}"
    exit 0
fi

mkdir -p "${INSTALL_DIR}"

prepare_macos_binary() {
    binary="$1"
    if [ "${OS}" != "macos" ]; then
        return 0
    fi
    xattr -cr "${binary}" 2>/dev/null || true
    if command -v codesign >/dev/null 2>&1; then
        team_line=$(codesign -dv "${binary}" 2>&1 | grep "^TeamIdentifier=") || team_line=""
        if [ -n "${team_line}" ] && [ "${team_line}" != "TeamIdentifier=not set" ]; then
            echo "Detected Developer-ID-signed binary (${team_line#TeamIdentifier=}); preserving notarization."
        else
            codesign --force --sign - "${binary}" 2>/dev/null || true
            echo "Detected ad-hoc-signed binary; re-signed locally for Gatekeeper."
        fi
    fi
}

smoke_binary_or_repair() {
    binary="$1"
    if [ "${SHIPYARD_SKIP_SMOKE:-0}" = "1" ]; then
        return 0
    fi
    if "${binary}" --version >/dev/null 2>&1; then
        return 0
    fi
    if [ "${OS}" = "macos" ]; then
        xattr -d com.apple.provenance "${binary}" 2>/dev/null || true
        sleep 1
    fi
    if "${binary}" --version >/dev/null 2>&1; then
        return 0
    fi
    if [ "${OS}" = "macos" ] \
        && [ "${SHIPYARD_NO_ADHOC_FALLBACK:-0}" != "1" ] \
        && command -v codesign >/dev/null 2>&1; then
        team_line=$(codesign -dv "${binary}" 2>&1 | grep "^TeamIdentifier=") || team_line=""
        if [ -n "${team_line}" ] && [ "${team_line}" != "TeamIdentifier=not set" ]; then
            echo "WARN: notarized binary would not launch; trying local ad-hoc fallback." >&2
            xattr -cr "${binary}" 2>/dev/null || true
            codesign --remove-signature "${binary}" 2>/dev/null || true
            codesign --force --sign - "${binary}" 2>/dev/null || true
        fi
    fi
    if ! "${binary}" --version >/dev/null 2>&1; then
        echo "ERROR: ${BINARY_NAME} installed but failed post-install smoke." >&2
        echo "Run '${binary} --version' manually for details." >&2
        exit 1
    fi
}

DMG_URL=""
DMG_URL_IS_API=0
RELEASE_URL=""
RELEASE_URL_IS_API=0
if [ "${SHIPYARD_SKIP_DOWNLOAD:-0}" != "1" ]; then
    echo "Resolving ${VERSION_LABEL} from ${REPO}..."
    RELEASE_JSON="$(curl_shipyard -sL "https://api.github.com/repos/${REPO}/${API_PATH}")"
    PREFER_API_ASSET_URL=0
    if [ -n "${GITHUB_TOKEN_VALUE}" ] && command -v python3 >/dev/null 2>&1; then
        PREFER_API_ASSET_URL=1
    fi
    if [ "$OS" = "macos" ]; then
        DMG_URL=$(printf '%s' "${RELEASE_JSON}" \
            | select_asset_url "${ARTIFACT}.dmg" "${PREFER_API_ASSET_URL}" || true)
        if [ -n "${DMG_URL}" ] && [ "${PREFER_API_ASSET_URL}" = "1" ]; then
            DMG_URL_IS_API=1
        fi
        if [ -z "${DMG_URL}" ]; then
            DMG_URL=$(printf '%s' "${RELEASE_JSON}" \
            | grep "browser_download_url.*${ARTIFACT}\.dmg" \
            | head -1 \
            | cut -d '"' -f 4 || true)
        fi
    fi
    if [ -z "${DMG_URL}" ]; then
        RELEASE_URL=$(printf '%s' "${RELEASE_JSON}" \
            | select_asset_url "${ARTIFACT}" "${PREFER_API_ASSET_URL}" || true)
        if [ -n "${RELEASE_URL}" ] && [ "${PREFER_API_ASSET_URL}" = "1" ]; then
            RELEASE_URL_IS_API=1
        fi
        if [ -z "${RELEASE_URL}" ]; then
            RELEASE_URL=$(printf '%s' "${RELEASE_JSON}" \
            | grep -E "browser_download_url.*${ARTIFACT}\"" \
            | head -1 \
            | cut -d '"' -f 4 || true)
        fi
    fi
    if [ -z "${DMG_URL}" ] && [ -z "${RELEASE_URL}" ]; then
        echo "No binary found for ${ARTIFACT} in ${VERSION_LABEL}." >&2
        echo "Check https://github.com/${REPO}/releases for available builds." >&2
        exit 1
    fi
fi

DEST="${INSTALL_DIR}/${BINARY_NAME}"
STAGED_DEST="${INSTALL_DIR}/.${BINARY_NAME}.install.$$"
trap 'rm -f "${STAGED_DEST:-}"' EXIT
if [ "${SHIPYARD_SKIP_DOWNLOAD:-0}" = "1" ]; then
    if [ ! -f "${DEST}" ]; then
        echo "SHIPYARD_SKIP_DOWNLOAD=1 but ${DEST} does not exist." >&2
        exit 1
    fi
    cp "${DEST}" "${STAGED_DEST}"
elif [ -n "${DMG_URL}" ]; then
    echo "Downloading ${ARTIFACT}.dmg (${VERSION_LABEL})..."
    DMG_TMP="$(mktemp -d)/shipyard.dmg"
    download_asset "${DMG_URL}" "${DMG_TMP}" "${DMG_URL_IS_API}"
    MOUNT_POINT="$(mktemp -d)/mnt"
    if ! hdiutil attach -nobrowse -readonly \
            -mountpoint "${MOUNT_POINT}" "${DMG_TMP}" >/dev/null 2>&1; then
        echo "Failed to mount ${DMG_TMP}; the DMG may be corrupt or unsigned." >&2
        rm -f "${DMG_TMP}"
        exit 1
    fi
    if [ ! -f "${MOUNT_POINT}/${BINARY_NAME}" ]; then
        echo "DMG mounted but no '${BINARY_NAME}' binary exists at ${MOUNT_POINT}." >&2
        ls -la "${MOUNT_POINT}" >&2 || true
        hdiutil detach "${MOUNT_POINT}" >/dev/null 2>&1 || true
        rm -f "${DMG_TMP}"
        exit 1
    fi
    cp "${MOUNT_POINT}/${BINARY_NAME}" "${STAGED_DEST}"
    hdiutil detach "${MOUNT_POINT}" >/dev/null 2>&1 || true
    rm -f "${DMG_TMP}"
else
    echo "Downloading ${ARTIFACT} (${VERSION_LABEL})..."
    download_asset "${RELEASE_URL}" "${STAGED_DEST}" "${RELEASE_URL_IS_API}"
fi
chmod +x "${STAGED_DEST}"

prepare_macos_binary "${STAGED_DEST}"
verify_expected_binary_sha256 "${STAGED_DEST}" "prepared"
smoke_binary_or_repair "${STAGED_DEST}"
verify_expected_binary_sha256 "${STAGED_DEST}" "smoke-tested"
mv -f "${STAGED_DEST}" "${DEST}"
ln -sf "${DEST}" "${INSTALL_DIR}/${ALIAS_NAME}"
verify_expected_binary_sha256 "${DEST}" "installed"
smoke_binary_or_repair "${DEST}"
verify_expected_binary_sha256 "${DEST}" "post-smoke"

echo ""
echo "Installed ${BINARY_NAME} to ${DEST}"
echo "Symlink: ${INSTALL_DIR}/${ALIAS_NAME}"
echo ""

if ! echo "${PATH}" | tr ':' '\n' | grep -q "^${INSTALL_DIR}$"; then
    echo "Add ${INSTALL_DIR} to your PATH:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    echo ""
fi

echo "Next steps:"
echo "  ${BINARY_NAME} --version"
echo "  ${BINARY_NAME} doctor"
