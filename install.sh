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

ARTIFACT_PREFIX="${SHIPYARD_ARTIFACT_PREFIX:-shipyard}"
PROVIDER_ARTIFACT_PREFIX="${SHIPYARD_PROVIDER_ARTIFACT_PREFIX:-shipyard-workstream-provider}"
BINARY_NAME="shipyard"
PROVIDER_BINARY_NAME="shipyard-workstream-provider"
BINARY_VERSION_NAME="shipyard"
PROVIDER_VERSION_NAME="shipyard-workstream-provider"
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
PROVIDER_ARTIFACT="${PROVIDER_ARTIFACT_PREFIX}-${OS}-${ARCH}"
if [ "$OS" = "windows" ]; then
    ARTIFACT="${ARTIFACT}.exe"
    PROVIDER_ARTIFACT="${PROVIDER_ARTIFACT}.exe"
    BINARY_NAME="${BINARY_NAME}.exe"
    PROVIDER_BINARY_NAME="${PROVIDER_BINARY_NAME}.exe"
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

# v0.127.0 introduced the separately executable workstream provider. Older
# pinned releases intentionally install only the historical CLI and remove a
# newer companion after the old CLI has passed its smoke test.
REQUIRE_PROVIDER=1
if [ "${VERSION_LABEL}" != "latest" ]; then
    ver="${VERSION_LABEL#v}"
    major="${ver%%.*}"
    rest="${ver#*.}"
    minor="${rest%%.*}"
    if [ "${major:-0}" -eq 0 ] 2>/dev/null \
            && [ "${minor:-0}" -lt 127 ] 2>/dev/null; then
        REQUIRE_PROVIDER=0
    fi
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
    echo "PROVIDER_ARTIFACT=${PROVIDER_ARTIFACT}"
    echo "BINARY_NAME=${BINARY_NAME}"
    echo "PROVIDER_BINARY_NAME=${PROVIDER_BINARY_NAME}"
    echo "REQUIRE_PROVIDER=${REQUIRE_PROVIDER}"
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
    binary_label="$2"
    version="$(binary_semver "${binary}" "${binary_label}")" || version=""
    if [ -n "${version}" ]; then
        printf '%s\n' "${version}"
        return 0
    fi
    if [ "${OS}" = "macos" ]; then
        xattr -d com.apple.provenance "${binary}" 2>/dev/null || true
        sleep 1
    fi
    version="$(binary_semver "${binary}" "${binary_label}")" || version=""
    if [ -n "${version}" ]; then
        printf '%s\n' "${version}"
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
    version="$(binary_semver "${binary}" "${binary_label}")" || version=""
    if [ -z "${version}" ]; then
        echo "ERROR: ${binary_label} installed but failed post-install smoke." >&2
        echo "Expected '${binary_label} <semantic-version>' from '${binary} --version'." >&2
        exit 1
    fi
    printf '%s\n' "${version}"
}

binary_semver() {
    binary="$1"
    binary_label="$2"
    output="$("${binary}" --version 2>/dev/null)" || return 1
    printf '%s\n' "${output}" | awk -v label="${binary_label}" '
        NR == 1 && NF == 2 && $1 == label {
            version = $2
            sub(/^v/, "", version)
            if (version ~ /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$/) {
                parsed = version
            }
        }
        NR > 1 { invalid = 1 }
        END { if (!invalid && parsed != "") print parsed }
    '
}

DMG_URL=""
DMG_URL_IS_API=0
RELEASE_URL=""
RELEASE_URL_IS_API=0
PROVIDER_RELEASE_URL=""
PROVIDER_RELEASE_URL_IS_API=0
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
    if [ "${REQUIRE_PROVIDER}" = "1" ] && [ "$OS" != "macos" ]; then
        PROVIDER_RELEASE_URL=$(printf '%s' "${RELEASE_JSON}" \
            | select_asset_url "${PROVIDER_ARTIFACT}" "${PREFER_API_ASSET_URL}" || true)
        if [ -n "${PROVIDER_RELEASE_URL}" ] && [ "${PREFER_API_ASSET_URL}" = "1" ]; then
            PROVIDER_RELEASE_URL_IS_API=1
        fi
        if [ -z "${PROVIDER_RELEASE_URL}" ]; then
            PROVIDER_RELEASE_URL=$(printf '%s' "${RELEASE_JSON}" \
                | grep -E "browser_download_url.*${PROVIDER_ARTIFACT}\"" \
                | head -1 \
                | cut -d '"' -f 4 || true)
        fi
    fi
    if [ -z "${DMG_URL}" ] && [ -z "${RELEASE_URL}" ]; then
        echo "No binary found for ${ARTIFACT} in ${VERSION_LABEL}." >&2
        echo "Check https://github.com/${REPO}/releases for available builds." >&2
        exit 1
    fi
    if [ "${REQUIRE_PROVIDER}" = "1" ] && [ "$OS" != "macos" ] \
            && [ -z "${PROVIDER_RELEASE_URL}" ]; then
        echo "No companion binary found for ${PROVIDER_ARTIFACT} in ${VERSION_LABEL}." >&2
        exit 1
    fi
fi

DEST="${INSTALL_DIR}/${BINARY_NAME}"
STAGED_DEST="${INSTALL_DIR}/.${BINARY_NAME}.install.$$"
PROVIDER_DEST="${INSTALL_DIR}/${PROVIDER_BINARY_NAME}"
STAGED_PROVIDER_DEST="${INSTALL_DIR}/.${PROVIDER_BINARY_NAME}.install.$$"
BACKUP_DEST="${INSTALL_DIR}/.${BINARY_NAME}.backup.$$"
BACKUP_PROVIDER_DEST="${INSTALL_DIR}/.${PROVIDER_BINARY_NAME}.backup.$$"
BACKUP_ALIAS="${INSTALL_DIR}/.${ALIAS_NAME}.backup.$$"
RECOVERY_JOURNAL="${INSTALL_DIR}/.shipyard-install-recovery.$$"
TRANSACTION_ACTIVE=0
HAD_DEST=0
HAD_PROVIDER_DEST=0
HAD_ALIAS=0

cleanup_install_transaction() {
    status=$?
    set +e
    restore_failed=0
    if [ "${TRANSACTION_ACTIVE}" = "1" ]; then
        if [ -e "${BACKUP_DEST}" ] || [ -L "${BACKUP_DEST}" ]; then
            rm -f "${DEST}" || restore_failed=1
            if [ "${restore_failed}" = "0" ]; then
                mv -f "${BACKUP_DEST}" "${DEST}" || restore_failed=1
            fi
        elif [ "${HAD_DEST}" = "0" ]; then
            rm -f "${DEST}" || restore_failed=1
        fi
        if [ -e "${BACKUP_PROVIDER_DEST}" ] || [ -L "${BACKUP_PROVIDER_DEST}" ]; then
            provider_restore_ready=1
            rm -f "${PROVIDER_DEST}" || provider_restore_ready=0
            if [ "${provider_restore_ready}" = "1" ]; then
                mv -f "${BACKUP_PROVIDER_DEST}" "${PROVIDER_DEST}" \
                    || restore_failed=1
            else
                restore_failed=1
            fi
        elif [ "${HAD_PROVIDER_DEST}" = "0" ]; then
            rm -f "${PROVIDER_DEST}" || restore_failed=1
        fi
        if [ -e "${BACKUP_ALIAS}" ] || [ -L "${BACKUP_ALIAS}" ]; then
            alias_restore_ready=1
            rm -f "${INSTALL_DIR}/${ALIAS_NAME}" || alias_restore_ready=0
            if [ "${alias_restore_ready}" = "1" ]; then
                mv -f "${BACKUP_ALIAS}" "${INSTALL_DIR}/${ALIAS_NAME}" \
                    || restore_failed=1
            else
                restore_failed=1
            fi
        elif [ "${HAD_ALIAS}" = "0" ]; then
            rm -f "${INSTALL_DIR}/${ALIAS_NAME}" || restore_failed=1
        fi
        if [ "${restore_failed}" = "1" ]; then
            echo "ERROR: automatic Shipyard install rollback was incomplete." >&2
            echo "Recovery journal: ${RECOVERY_JOURNAL}" >&2
            echo "CLI backup: ${BACKUP_DEST} -> ${DEST}" >&2
            echo "Provider backup: ${BACKUP_PROVIDER_DEST} -> ${PROVIDER_DEST}" >&2
            echo "Alias backup: ${BACKUP_ALIAS} -> ${INSTALL_DIR}/${ALIAS_NAME}" >&2
            echo "Preserve these paths and follow the journal for manual recovery." >&2
            rm -f "${STAGED_DEST}" "${STAGED_PROVIDER_DEST}"
            exit 1
        fi
        rm -f "${BACKUP_DEST}" "${BACKUP_PROVIDER_DEST}" \
            "${BACKUP_ALIAS}" "${RECOVERY_JOURNAL}"
    fi
    rm -f "${STAGED_DEST}" "${STAGED_PROVIDER_DEST}"
    exit "${status}"
}
trap cleanup_install_transaction EXIT
if [ "${SHIPYARD_SKIP_DOWNLOAD:-0}" = "1" ]; then
    if [ ! -f "${DEST}" ]; then
        echo "SHIPYARD_SKIP_DOWNLOAD=1 but ${DEST} does not exist." >&2
        exit 1
    fi
    cp "${DEST}" "${STAGED_DEST}"
    if [ "${REQUIRE_PROVIDER}" = "1" ]; then
        if [ ! -f "${PROVIDER_DEST}" ]; then
            echo "SHIPYARD_SKIP_DOWNLOAD=1 but ${PROVIDER_DEST} does not exist." >&2
            exit 1
        fi
        cp "${PROVIDER_DEST}" "${STAGED_PROVIDER_DEST}"
    fi
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
    if [ "${REQUIRE_PROVIDER}" = "1" ] \
            && [ ! -f "${MOUNT_POINT}/${PROVIDER_BINARY_NAME}" ]; then
        echo "DMG mounted but no '${PROVIDER_BINARY_NAME}' binary exists at ${MOUNT_POINT}." >&2
        hdiutil detach "${MOUNT_POINT}" >/dev/null 2>&1 || true
        rm -f "${DMG_TMP}"
        exit 1
    fi
    cp "${MOUNT_POINT}/${BINARY_NAME}" "${STAGED_DEST}"
    if [ "${REQUIRE_PROVIDER}" = "1" ]; then
        cp "${MOUNT_POINT}/${PROVIDER_BINARY_NAME}" "${STAGED_PROVIDER_DEST}"
    fi
    hdiutil detach "${MOUNT_POINT}" >/dev/null 2>&1 || true
    rm -f "${DMG_TMP}"
else
    echo "Downloading ${ARTIFACT} (${VERSION_LABEL})..."
    download_asset "${RELEASE_URL}" "${STAGED_DEST}" "${RELEASE_URL_IS_API}"
    if [ "${REQUIRE_PROVIDER}" = "1" ]; then
        echo "Downloading ${PROVIDER_ARTIFACT} (${VERSION_LABEL})..."
        download_asset "${PROVIDER_RELEASE_URL}" "${STAGED_PROVIDER_DEST}" \
            "${PROVIDER_RELEASE_URL_IS_API}"
    fi
fi
chmod +x "${STAGED_DEST}"
if [ "${REQUIRE_PROVIDER}" = "1" ]; then
    chmod +x "${STAGED_PROVIDER_DEST}"
fi

prepare_macos_binary "${STAGED_DEST}"
STAGED_VERSION="$(smoke_binary_or_repair "${STAGED_DEST}" "${BINARY_VERSION_NAME}")"
if [ "${REQUIRE_PROVIDER}" = "1" ]; then
    prepare_macos_binary "${STAGED_PROVIDER_DEST}"
    STAGED_PROVIDER_VERSION="$(smoke_binary_or_repair \
        "${STAGED_PROVIDER_DEST}" "${PROVIDER_VERSION_NAME}")"
    if [ "${STAGED_VERSION}" != "${STAGED_PROVIDER_VERSION}" ]; then
        echo "ERROR: release binary version mismatch: ${BINARY_NAME}=${STAGED_VERSION} ${PROVIDER_BINARY_NAME}=${STAGED_PROVIDER_VERSION}." >&2
        exit 1
    fi
fi

if [ -e "${DEST}" ] || [ -L "${DEST}" ]; then HAD_DEST=1; fi
if [ -e "${PROVIDER_DEST}" ] || [ -L "${PROVIDER_DEST}" ]; then HAD_PROVIDER_DEST=1; fi
if [ -e "${INSTALL_DIR}/${ALIAS_NAME}" ] || [ -L "${INSTALL_DIR}/${ALIAS_NAME}" ]; then HAD_ALIAS=1; fi
TRANSACTION_ACTIVE=1
{
    printf 'Shipyard install recovery journal\n'
    printf 'CLI backup: %s -> %s\n' "${BACKUP_DEST}" "${DEST}"
    printf 'Provider backup: %s -> %s\n' \
        "${BACKUP_PROVIDER_DEST}" "${PROVIDER_DEST}"
    printf 'Alias backup: %s -> %s\n' \
        "${BACKUP_ALIAS}" "${INSTALL_DIR}/${ALIAS_NAME}"
} > "${RECOVERY_JOURNAL}"
if [ "${HAD_DEST}" = "1" ]; then mv -f "${DEST}" "${BACKUP_DEST}"; fi
if [ "${HAD_PROVIDER_DEST}" = "1" ]; then
    mv -f "${PROVIDER_DEST}" "${BACKUP_PROVIDER_DEST}"
fi
if [ "${HAD_ALIAS}" = "1" ]; then
    mv -f "${INSTALL_DIR}/${ALIAS_NAME}" "${BACKUP_ALIAS}"
fi
mv -f "${STAGED_DEST}" "${DEST}"
if [ "${REQUIRE_PROVIDER}" = "1" ]; then
    mv -f "${STAGED_PROVIDER_DEST}" "${PROVIDER_DEST}"
else
    rm -f "${PROVIDER_DEST}"
fi
ln -sf "${DEST}" "${INSTALL_DIR}/${ALIAS_NAME}"
INSTALLED_VERSION="$(smoke_binary_or_repair "${DEST}" "${BINARY_VERSION_NAME}")"
if [ "${REQUIRE_PROVIDER}" = "1" ]; then
    INSTALLED_PROVIDER_VERSION="$(smoke_binary_or_repair \
        "${PROVIDER_DEST}" "${PROVIDER_VERSION_NAME}")"
    if [ "${INSTALLED_VERSION}" != "${INSTALLED_PROVIDER_VERSION}" ]; then
        echo "ERROR: installed binary version mismatch: ${BINARY_NAME}=${INSTALLED_VERSION} ${PROVIDER_BINARY_NAME}=${INSTALLED_PROVIDER_VERSION}." >&2
        exit 1
    fi
fi
TRANSACTION_ACTIVE=0
rm -f "${BACKUP_DEST}" "${BACKUP_PROVIDER_DEST}" \
    "${BACKUP_ALIAS}" "${RECOVERY_JOURNAL}"

echo ""
echo "Installed ${BINARY_NAME} to ${DEST}"
if [ "${REQUIRE_PROVIDER}" = "1" ]; then
    echo "Installed ${PROVIDER_BINARY_NAME} to ${PROVIDER_DEST}"
fi
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
