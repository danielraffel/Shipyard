#!/usr/bin/env bash
set -euo pipefail

# Shipyard installer: downloads the correct production binary for your
# platform and installs it as `shipyard` with the `sy` convenience symlink.
#
# Environment variables:
#   SHIPYARD_VERSION          Tag to install, or "latest" (default)
#   SHIPYARD_INSTALL_DIR      Install directory (default: ~/.local/bin)
#   SHIPYARD_CURL_BIN         Absolute curl binary selected by the updater
#   SHIPYARD_DRY_RUN          Print resolved settings and exit
#   SHIPYARD_SKIP_DOWNLOAD    Reuse an existing binary in install dir
#   SHIPYARD_SKIP_SMOKE       Skip post-install --version smoke
#   SHIPYARD_REPO             Override release repo
#   SHIPYARD_GITHUB_TOKEN     Optional token for private release repos

REPO="${SHIPYARD_REPO:-danielraffel/Shipyard}"
INSTALL_DIR="${SHIPYARD_INSTALL_DIR:-${HOME}/.local/bin}"
REQUESTED_VERSION="${SHIPYARD_VERSION:-latest}"
GITHUB_TOKEN_VALUE="${SHIPYARD_GITHUB_TOKEN:-${GITHUB_TOKEN:-}}"
CURL_BIN="${SHIPYARD_CURL_BIN:-curl}"

curl_shipyard() {
    if [ -n "${GITHUB_TOKEN_VALUE}" ]; then
        printf 'Authorization: Bearer %s\n' "${GITHUB_TOKEN_VALUE}" \
            | "${CURL_BIN}" -H @- "$@"
    else
        "${CURL_BIN}" "$@"
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

checksum_for_asset() {
    manifest="$1"
    asset_name="$2"
    awk -v wanted="${asset_name}" '
        {
            line = $0
            sub(/\r$/, "", line)
            digest = ""
            filename = ""
            if (length(line) >= 67 && substr(line, 65, 2) == "  ") {
                digest = substr(line, 1, 64)
                filename = substr(line, 67)
            } else if (length(line) >= 66 && substr(line, 65, 2) == " *") {
                digest = substr(line, 1, 64)
                filename = substr(line, 67)
            } else {
                fields = split(line, parts, /[[:space:]]+/)
                if (fields > 1 && parts[fields] == wanted) {
                    invalid = 1
                }
                next
            }
            if (filename == wanted) {
                matches += 1
                if (length(digest) != 64 || digest !~ /^[0-9A-Fa-f]+$/) {
                    invalid = 1
                }
                expected = tolower(digest)
            }
        }
        END {
            if (matches != 1 || invalid) {
                exit 1
            }
            print expected
        }
    ' "${manifest}"
}

sha256_file() {
    path="$1"
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "${path}" | awk '{ print tolower($1) }'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "${path}" | awk '{ print tolower($1) }'
    else
        echo "Neither shasum nor sha256sum is available; refusing an unverified install." >&2
        return 1
    fi
}

verify_release_asset() {
    asset_path="$1"
    asset_name="$2"
    manifest="$3"
    if ! expected_digest=$(checksum_for_asset "${manifest}" "${asset_name}"); then
        echo "checksums.sha256 must contain exactly one valid entry for ${asset_name}." >&2
        return 1
    fi
    actual_digest=$(sha256_file "${asset_path}") || return 1
    if [ "${actual_digest}" != "${expected_digest}" ]; then
        echo "SHA-256 verification failed for ${asset_name}; refusing to install." >&2
        return 1
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
CHECKSUM_URL=""
CHECKSUM_URL_IS_API=0
if [ "${SHIPYARD_SKIP_DOWNLOAD:-0}" != "1" ]; then
    echo "Resolving ${VERSION_LABEL} from ${REPO}..."
    if ! RELEASE_RESPONSE="$(curl_shipyard -sSL -w '\n%{http_code}' "https://api.github.com/repos/${REPO}/${API_PATH}")"; then
        echo "GitHub release lookup failed before receiving a complete response." >&2
        exit 1
    fi
    RELEASE_HTTP_CODE=$(printf '%s\n' "${RELEASE_RESPONSE}" | tail -n 1)
    RELEASE_JSON=$(printf '%s\n' "${RELEASE_RESPONSE}" | sed '$d')
    case "${RELEASE_HTTP_CODE}" in
        2??) ;;
        403|429)
            if [ -n "${GITHUB_TOKEN_VALUE}" ]; then
                echo "GitHub release lookup returned HTTP ${RELEASE_HTTP_CODE}; the configured token may lack repository access or be rate-limited." >&2
            else
                echo "GitHub release lookup returned HTTP ${RELEASE_HTTP_CODE}; anonymous API access is rate-limited." >&2
            fi
            exit 1
            ;;
        *)
            echo "GitHub release lookup returned HTTP ${RELEASE_HTTP_CODE}; this is not a missing platform asset." >&2
            exit 1
            ;;
    esac
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
    CHECKSUM_URL=$(printf '%s' "${RELEASE_JSON}" \
        | select_asset_url "checksums.sha256" "${PREFER_API_ASSET_URL}" || true)
    if [ -n "${CHECKSUM_URL}" ] && [ "${PREFER_API_ASSET_URL}" = "1" ]; then
        CHECKSUM_URL_IS_API=1
    fi
    if [ -z "${CHECKSUM_URL}" ]; then
        CHECKSUM_URL=$(printf '%s' "${RELEASE_JSON}" \
            | grep 'browser_download_url.*checksums\.sha256"' \
            | head -1 \
            | cut -d '"' -f 4 || true)
    fi
    if [ -z "${CHECKSUM_URL}" ]; then
        echo "Release ${VERSION_LABEL} has no checksums.sha256 asset; refusing an unverified install." >&2
        exit 1
    fi
fi

DEST="${INSTALL_DIR}/${BINARY_NAME}"
STAGED_DEST="${INSTALL_DIR}/.${BINARY_NAME}.install.$$"
DOWNLOAD_TMP_DIR=""
cleanup_install() {
    rm -f "${STAGED_DEST:-}"
    if [ -n "${DOWNLOAD_TMP_DIR}" ] && [ -d "${DOWNLOAD_TMP_DIR}" ]; then
        rm -rf "${DOWNLOAD_TMP_DIR}"
    fi
}
trap cleanup_install EXIT
if [ "${SHIPYARD_SKIP_DOWNLOAD:-0}" = "1" ]; then
    if [ ! -f "${DEST}" ]; then
        echo "SHIPYARD_SKIP_DOWNLOAD=1 but ${DEST} does not exist." >&2
        exit 1
    fi
    cp "${DEST}" "${STAGED_DEST}"
else
    DOWNLOAD_TMP_DIR="$(mktemp -d)"
    CHECKSUMS_TMP="${DOWNLOAD_TMP_DIR}/checksums.sha256"
    download_asset "${CHECKSUM_URL}" "${CHECKSUMS_TMP}" "${CHECKSUM_URL_IS_API}"
fi
if [ "${SHIPYARD_SKIP_DOWNLOAD:-0}" != "1" ] && [ -n "${DMG_URL}" ]; then
    ASSET_NAME="${ARTIFACT}.dmg"
    echo "Downloading ${ASSET_NAME} (${VERSION_LABEL})..."
    DMG_TMP="${DOWNLOAD_TMP_DIR}/${ASSET_NAME}"
    download_asset "${DMG_URL}" "${DMG_TMP}" "${DMG_URL_IS_API}"
    verify_release_asset "${DMG_TMP}" "${ASSET_NAME}" "${CHECKSUMS_TMP}"
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
elif [ "${SHIPYARD_SKIP_DOWNLOAD:-0}" != "1" ]; then
    ASSET_NAME="${ARTIFACT}"
    echo "Downloading ${ARTIFACT} (${VERSION_LABEL})..."
    download_asset "${RELEASE_URL}" "${STAGED_DEST}" "${RELEASE_URL_IS_API}"
    verify_release_asset "${STAGED_DEST}" "${ASSET_NAME}" "${CHECKSUMS_TMP}"
fi
chmod +x "${STAGED_DEST}"

prepare_macos_binary "${STAGED_DEST}"
smoke_binary_or_repair "${STAGED_DEST}"
mv -f "${STAGED_DEST}" "${DEST}"
ln -sf "${DEST}" "${INSTALL_DIR}/${ALIAS_NAME}"
smoke_binary_or_repair "${DEST}"

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
