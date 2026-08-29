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

select_release_tag() {
    command -v python3 >/dev/null 2>&1 || return 1
    python3 -c '
import json
import sys

value = json.load(sys.stdin).get("tag_name", "")
if isinstance(value, str):
    print(value)
'
}

version_requires_provider() {
    label="$1"
    [ "${label}" = "latest" ] && return 0
    ver="${label#v}"
    major="${ver%%.*}"
    rest="${ver#*.}"
    minor="${rest%%.*}"
    patch="${rest#*.}"
    patch="${patch%%[-+]*}"
    [ -n "${major}" ] && [ -n "${minor}" ] && [ -n "${patch}" ] || return 0
    if [ "${major}" -gt 0 ] 2>/dev/null \
            || [ "${minor}" -gt 126 ] 2>/dev/null \
            || { [ "${minor}" -eq 126 ] 2>/dev/null \
                 && [ "${patch}" -ge 3 ] 2>/dev/null; }; then
        return 0
    fi
    return 1
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

# v0.126.3 introduced the separately executable workstream provider. Older
# pinned releases intentionally install only the historical CLI and remove a
# newer companion after the old CLI has passed its smoke test.
REQUIRE_PROVIDER=1
if ! version_requires_provider "${VERSION_LABEL}"; then
    REQUIRE_PROVIDER=0
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
CHECKSUM_URL=""
CHECKSUM_URL_IS_API=0
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
    if [ "${VERSION_LABEL}" = "latest" ]; then
        RESOLVED_TAG=$(printf '%s' "${RELEASE_JSON}" | select_release_tag || true)
        if [ -z "${RESOLVED_TAG}" ]; then
            echo "Latest release response has no valid tag_name; refusing an ambiguous install." >&2
            exit 1
        fi
        if version_requires_provider "${RESOLVED_TAG}"; then
            REQUIRE_PROVIDER=1
        else
            REQUIRE_PROVIDER=0
        fi
    fi
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
    if [ "${REQUIRE_PROVIDER}" = "1" ] && [ "$OS" != "macos" ] \
            && [ -z "${PROVIDER_RELEASE_URL}" ]; then
        echo "No companion binary found for ${PROVIDER_ARTIFACT} in ${VERSION_LABEL}." >&2
        exit 1
    fi
fi

DEST="${INSTALL_DIR}/${BINARY_NAME}"
STAGED_DEST="${INSTALL_DIR}/.${BINARY_NAME}.install.$$"
DOWNLOAD_TMP_DIR=""
PROVIDER_DEST="${INSTALL_DIR}/${PROVIDER_BINARY_NAME}"
STAGED_PROVIDER_DEST="${INSTALL_DIR}/.${PROVIDER_BINARY_NAME}.install.$$"
BACKUP_DEST="${INSTALL_DIR}/.${BINARY_NAME}.backup"
BACKUP_PROVIDER_DEST="${INSTALL_DIR}/.${PROVIDER_BINARY_NAME}.backup"
BACKUP_ALIAS="${INSTALL_DIR}/.${ALIAS_NAME}.backup"
RECOVERY_JOURNAL="${INSTALL_DIR}/.shipyard-install-recovery"
INSTALL_LOCK_DIR="${INSTALL_DIR}/.shipyard-install.lock"
LOCK_HELD=0
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
    if [ -n "${DOWNLOAD_TMP_DIR}" ] && [ -d "${DOWNLOAD_TMP_DIR}" ]; then
        rm -rf "${DOWNLOAD_TMP_DIR}"
    fi
    if [ "${LOCK_HELD}" = "1" ]; then
        rm -rf "${INSTALL_LOCK_DIR}"
    fi
    exit "${status}"
}

acquire_install_lock() {
    if ! mkdir "${INSTALL_LOCK_DIR}" 2>/dev/null; then
        owner=$(cat "${INSTALL_LOCK_DIR}/pid" 2>/dev/null || true)
        if [ -n "${owner}" ] && kill -0 "${owner}" 2>/dev/null; then
            echo "Another Shipyard installation is active (pid ${owner}); refusing a concurrent swap." >&2
            exit 1
        fi
        rm -rf "${INSTALL_LOCK_DIR}"
        mkdir "${INSTALL_LOCK_DIR}" || {
            echo "Could not acquire Shipyard installation lock." >&2
            exit 1
        }
    fi
    printf '%s\n' "$$" > "${INSTALL_LOCK_DIR}/pid"
    LOCK_HELD=1
}

recover_incomplete_install() {
    [ -f "${RECOVERY_JOURNAL}" ] || return 0
    old_had_dest=$(awk -F= '$1 == "had_dest" { print $2 }' "${RECOVERY_JOURNAL}")
    old_had_provider=$(awk -F= '$1 == "had_provider" { print $2 }' "${RECOVERY_JOURNAL}")
    old_had_alias=$(awk -F= '$1 == "had_alias" { print $2 }' "${RECOVERY_JOURNAL}")
    for value in "${old_had_dest}" "${old_had_provider}" "${old_had_alias}"; do
        case "${value}" in 0|1) ;; *)
            echo "Invalid Shipyard recovery journal; refusing to overwrite installed files." >&2
            exit 1
        esac
    done
    if [ -e "${BACKUP_DEST}" ] || [ -L "${BACKUP_DEST}" ]; then
        rm -f "${DEST}" && mv -f "${BACKUP_DEST}" "${DEST}"
    elif [ "${old_had_dest}" = "0" ]; then
        rm -f "${DEST}"
    elif [ ! -e "${DEST}" ] && [ ! -L "${DEST}" ]; then
        echo "Shipyard recovery is missing its CLI backup; refusing to continue." >&2
        exit 1
    fi
    if [ -e "${BACKUP_PROVIDER_DEST}" ] || [ -L "${BACKUP_PROVIDER_DEST}" ]; then
        rm -f "${PROVIDER_DEST}" && mv -f "${BACKUP_PROVIDER_DEST}" "${PROVIDER_DEST}"
    elif [ "${old_had_provider}" = "0" ]; then
        rm -f "${PROVIDER_DEST}"
    elif [ ! -e "${PROVIDER_DEST}" ] && [ ! -L "${PROVIDER_DEST}" ]; then
        echo "Shipyard recovery is missing its provider backup; refusing to continue." >&2
        exit 1
    fi
    if [ -e "${BACKUP_ALIAS}" ] || [ -L "${BACKUP_ALIAS}" ]; then
        rm -f "${INSTALL_DIR}/${ALIAS_NAME}" \
            && mv -f "${BACKUP_ALIAS}" "${INSTALL_DIR}/${ALIAS_NAME}"
    elif [ "${old_had_alias}" = "0" ]; then
        rm -f "${INSTALL_DIR}/${ALIAS_NAME}"
    elif [ ! -e "${INSTALL_DIR}/${ALIAS_NAME}" ] \
            && [ ! -L "${INSTALL_DIR}/${ALIAS_NAME}" ]; then
        echo "Shipyard recovery is missing its alias backup; refusing to continue." >&2
        exit 1
    fi
    rm -f "${RECOVERY_JOURNAL}"
    echo "Recovered an interrupted Shipyard pair installation before continuing." >&2
}

trap cleanup_install_transaction EXIT
acquire_install_lock
recover_incomplete_install
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
elif [ "${SHIPYARD_SKIP_DOWNLOAD:-0}" != "1" ]; then
    ASSET_NAME="${ARTIFACT}"
    echo "Downloading ${ARTIFACT} (${VERSION_LABEL})..."
    download_asset "${RELEASE_URL}" "${STAGED_DEST}" "${RELEASE_URL_IS_API}"
    verify_release_asset "${STAGED_DEST}" "${ASSET_NAME}" "${CHECKSUMS_TMP}"
    if [ "${REQUIRE_PROVIDER}" = "1" ]; then
        echo "Downloading ${PROVIDER_ARTIFACT} (${VERSION_LABEL})..."
        download_asset "${PROVIDER_RELEASE_URL}" "${STAGED_PROVIDER_DEST}" \
            "${PROVIDER_RELEASE_URL_IS_API}"
        verify_release_asset "${STAGED_PROVIDER_DEST}" "${PROVIDER_ARTIFACT}" \
            "${CHECKSUMS_TMP}"
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
    printf 'had_dest=%s\n' "${HAD_DEST}"
    printf 'had_provider=%s\n' "${HAD_PROVIDER_DEST}"
    printf 'had_alias=%s\n' "${HAD_ALIAS}"
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
