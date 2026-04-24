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

# #256: Intel Macs (x86_64) are no longer supported as of v0.50.0.
# Surface a clean message rather than letting the user hit a 404 on
# a missing asset. Exit 2 distinguishes "this platform is
# unsupported" from exit 1 ("something went wrong").
if [ "$OS" = "macos" ] && [ "$ARCH" = "x64" ]; then
    echo "Intel Macs (x86_64) are not supported by Shipyard v0.50.0 and later." >&2
    echo "Apple Silicon (arm64) Macs only. If you're pinned to an older tag" >&2
    echo "(SHIPYARD_VERSION=vX.Y.Z) that still has an Intel dmg, that install" >&2
    echo "remains functional — the cutover only affects new releases." >&2
    exit 2
fi

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
#
# macOS release assets are distributed as a stapled .dmg starting
# in v0.44.0 (task #52 / #219). The DMG wraps a single Mach-O with
# the notarization ticket stapled to the DMG container — Gatekeeper
# verifies the ticket OFFLINE when the DMG is mounted, eliminating
# the per-Mac online-check flakiness that sank v0.42.0 + v0.43.0
# launches.
#
# Fallback to bare Mach-O for:
#   - non-macOS platforms (Linux, Windows)
#   - older tags that predate the .dmg pipeline
#   - self-built forks that haven't run scripts/release-macos-local.sh

mkdir -p "${INSTALL_DIR}"

# Short-circuit when the installed binary is already at the target
# version (#231). Saves ~15MB per redundant invocation — common when
# pulp + spectr pins both sit at the same Shipyard version and the
# user runs both install-shipyard.sh wrappers back-to-back.
#
# Only applies when:
#   - Target is a specific version (not "latest" — we'd have to
#     resolve latest to know if there's drift, which defeats the
#     purpose of skipping network work).
#   - SHIPYARD_FORCE_REINSTALL is not set.
#   - SHIPYARD_SKIP_DOWNLOAD is not set (tests need their own flow).
#   - Existing binary actually reports a version. A SIGKILL'd binary
#     falls through to the full install path where the smoke + ad-hoc
#     fallback can recover it.
SHIPYARD_ALREADY_AT_TARGET=0
if [ "${SHIPYARD_SKIP_DOWNLOAD:-0}" != "1" ] \
    && [ "${SHIPYARD_FORCE_REINSTALL:-0}" != "1" ] \
    && [ -x "${INSTALL_DIR}/shipyard" ] \
    && [ "${VERSION_LABEL}" != "latest" ]; then
    # Expect `shipyard, version 0.46.0` — extract just the version.
    existing=$("${INSTALL_DIR}/shipyard" --version 2>/dev/null \
        | sed -n 's/^shipyard, version \([^ ]*\).*/\1/p' | head -1)
    target="${VERSION_LABEL#v}"
    if [ -n "${existing}" ] && [ "${existing}" = "${target}" ]; then
        echo "Already at v${target} at ${INSTALL_DIR}/shipyard — skipping download."
        echo "Set SHIPYARD_FORCE_REINSTALL=1 to re-download anyway."
        SHIPYARD_ALREADY_AT_TARGET=1
    fi
fi

# SHIPYARD_SKIP_DOWNLOAD=1 preserves an already-present binary at
# ${INSTALL_DIR}/shipyard. Used by tests to exercise the smoke +
# remediation paths without hitting the network. When it's set we
# skip URL resolution entirely (tests don't want live API calls).
# Same when we short-circuited on version match above.
if [ "${SHIPYARD_SKIP_DOWNLOAD:-0}" != "1" ] \
    && [ "${SHIPYARD_ALREADY_AT_TARGET}" != "1" ]; then
    # `set -o pipefail` means a pipe containing `grep` that matches
    # nothing kills the script via set -e — so `|| true` the final
    # cut lets us handle the empty-URL case below instead of dying
    # silently mid-script.
    DMG_URL=""
    if [ "${OS}" = "macos" ]; then
        DMG_URL=$(curl -sL "https://api.github.com/repos/${REPO}/${API_PATH}" \
            | grep "browser_download_url.*${ARTIFACT}\.dmg" \
            | head -1 \
            | cut -d '"' -f 4 || true)
    fi

    RELEASE_URL=""
    if [ -z "${DMG_URL}" ]; then
        # Match either `<ARTIFACT>"` (bare Mach-O / Linux binaries)
        # or `<ARTIFACT>.exe"` (Windows) at the end of the asset name.
        # The trailing double-quote anchor is load-bearing: without it,
        # `shipyard-macos-arm64` would also match `shipyard-macos-arm64.dmg`
        # in the JSON, which is the bug the DMG_URL branch is there to
        # handle separately. Codex caught an earlier iteration that
        # anchored with just `"` and broke Windows (#227 P1).
        RELEASE_URL=$(curl -sL "https://api.github.com/repos/${REPO}/${API_PATH}" \
            | grep -E "browser_download_url.*${ARTIFACT}(\.exe)?\"" \
            | head -1 \
            | cut -d '"' -f 4 || true)
    fi

    if [ -z "${DMG_URL}" ] && [ -z "${RELEASE_URL}" ]; then
        echo "No binary found for ${ARTIFACT} in ${VERSION_LABEL}." >&2
        echo "Check https://github.com/${REPO}/releases for available builds." >&2
        exit 1
    fi
else
    DMG_URL=""
    RELEASE_URL=""
fi

if [ "${SHIPYARD_SKIP_DOWNLOAD:-0}" = "1" ] \
    || [ "${SHIPYARD_ALREADY_AT_TARGET}" = "1" ]; then
    : # existing binary at INSTALL_DIR/shipyard is what we'll test
elif [ -n "${DMG_URL}" ]; then
    echo "Downloading ${ARTIFACT}.dmg (${VERSION_LABEL})..."
    # Stapled DMG path (#52): download the dmg, mount read-only,
    # copy the binary OUT of the mount, detach. The ticket binding
    # is on the DMG and Gatekeeper evaluated it at mount time, so
    # the extracted binary inherits "trusted origin" state in
    # macOS's provenance tracking — taskgated won't trigger an
    # online check on first launch.
    DMG_TMP="$(mktemp -d)/shipyard.dmg"
    curl -sL "${DMG_URL}" -o "${DMG_TMP}"
    MOUNT_POINT="$(mktemp -d)/mnt"
    if ! hdiutil attach -nobrowse -readonly \
            -mountpoint "${MOUNT_POINT}" "${DMG_TMP}" >/dev/null 2>&1; then
        echo "Failed to mount ${DMG_TMP} — the DMG may be corrupt or" >&2
        echo "unsigned. Try re-downloading or check the release page." >&2
        rm -f "${DMG_TMP}"
        exit 1
    fi
    if [ ! -f "${MOUNT_POINT}/shipyard" ]; then
        echo "DMG mounted but no 'shipyard' binary inside at ${MOUNT_POINT}." >&2
        echo "Contents:" >&2
        ls -la "${MOUNT_POINT}" >&2 || true
        hdiutil detach "${MOUNT_POINT}" >/dev/null 2>&1 || true
        rm -f "${DMG_TMP}"
        exit 1
    fi
    cp "${MOUNT_POINT}/shipyard" "${INSTALL_DIR}/shipyard"
    hdiutil detach "${MOUNT_POINT}" >/dev/null 2>&1 || true
    rm -f "${DMG_TMP}"
else
    echo "Downloading ${ARTIFACT} (${VERSION_LABEL})..."
    curl -sL "${RELEASE_URL}" -o "${INSTALL_DIR}/shipyard"
fi
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

# Post-install smoke test (#219).
#
# Bare Mach-O binaries on macOS can pass `codesign --verify` and
# still SIGKILL at launch with "Taskgated Invalid Signature" —
# notarization tickets can't be stapled to a Mach-O (only to .app
# / .pkg / .dmg), so Gatekeeper falls back to an ONLINE check the
# first time the binary runs. If that check hiccups (network, DNS,
# Apple-side CDN), taskgated rejects launch and the user sees
# exit 137 with zero output. Without this gate the installer
# cheerfully reports "installed" and leaves the user with a dead
# binary; that's exactly #219.
#
# If the smoke fails, try one recovery round (remove provenance
# xattr + force a second launch) before giving up with a specific,
# actionable error message. Skip entirely if SHIPYARD_SKIP_SMOKE=1
# is set — useful for CI that's dispatching its own verification.
if [ "${SKIP_SMOKE:-${SHIPYARD_SKIP_SMOKE:-0}}" != "1" ]; then
    if ! "${INSTALL_DIR}/shipyard" --version >/dev/null 2>&1; then
        # Recovery step 1: strip com.apple.provenance and retry.
        # macOS 26+ sometimes caches a taskgated rejection against
        # the provenance record; clearing it lets the online
        # notarization check run fresh on the next launch.
        if [ "${OS}" = "macos" ]; then
            xattr -d com.apple.provenance "${INSTALL_DIR}/shipyard" 2>/dev/null || true
            sleep 1
        fi
        if ! "${INSTALL_DIR}/shipyard" --version >/dev/null 2>&1; then
            # Recovery step 2 (macOS, Developer-ID signed only):
            # ad-hoc fallback. The notarized binary can't pass
            # taskgated on this Mac (online notarization check
            # failing for whatever reason — #219). Re-signing
            # locally with an ad-hoc signature LOSES notarization
            # trust (Gatekeeper fast-path, XProtect deep-scan skip)
            # but gains a launchable binary. For users who just
            # need shipyard to WORK right now, this is a strictly
            # better outcome than exit-1 + a dead install.
            #
            # Opt out via SHIPYARD_NO_ADHOC_FALLBACK=1 if you'd
            # rather the installer fail loud (e.g. in a corporate
            # environment where ad-hoc signing is prohibited).
            if [ "${OS}" = "macos" ] \
                && [ "${SHIPYARD_NO_ADHOC_FALLBACK:-0}" != "1" ] \
                && command -v codesign >/dev/null 2>&1; then
                team_line=$(codesign -dv "${INSTALL_DIR}/shipyard" 2>&1 \
                    | grep "^TeamIdentifier=") || team_line=""
                if [ -n "${team_line}" ] \
                    && [ "${team_line}" != "TeamIdentifier=not set" ]; then
                    echo "" >&2
                    echo "WARN: Notarized binary wouldn't launch (taskgated rejection)." >&2
                    echo "      Falling back to local ad-hoc signature. Gatekeeper's" >&2
                    echo "      fast-path + XProtect deep-scan skip are DISABLED" >&2
                    echo "      until a v0.44+ binary is released with a stapled .dmg." >&2
                    echo "      Set SHIPYARD_NO_ADHOC_FALLBACK=1 to skip this fallback." >&2
                    xattr -cr "${INSTALL_DIR}/shipyard" 2>/dev/null || true
                    codesign --remove-signature "${INSTALL_DIR}/shipyard" 2>/dev/null || true
                    codesign --force --sign - "${INSTALL_DIR}/shipyard" 2>/dev/null || true
                    if "${INSTALL_DIR}/shipyard" --version >/dev/null 2>&1; then
                        echo "      OK: ad-hoc fallback succeeded." >&2
                        echo "" >&2
                        # Fall through to success path below.
                    else
                        echo "      ad-hoc fallback also failed — see hint below." >&2
                    fi
                fi
            fi
            # Final launch probe — might have succeeded via fallback.
            if ! "${INSTALL_DIR}/shipyard" --version >/dev/null 2>&1; then
                echo "" >&2
                echo "ERROR: shipyard was installed but failed its post-install smoke test." >&2
                echo "" >&2
                if [ "${OS}" = "macos" ]; then
                    echo "On macOS this usually means one of:" >&2
                    echo "  - Gatekeeper's first-launch online notarization check failed" >&2
                    echo "    (transient network / Apple CDN). Retry: ${INSTALL_DIR}/shipyard --version" >&2
                    echo "  - taskgated rejected the binary. Check the crash report under" >&2
                    echo "    ~/Library/Logs/DiagnosticReports/shipyard-*.ips for 'Code Signature Invalid'." >&2
                    echo "    If that's the signature, see https://github.com/danielraffel/Shipyard/issues/219" >&2
                    echo "    for status on the .dmg-stapling fix." >&2
                    if [ "${SHIPYARD_NO_ADHOC_FALLBACK:-0}" = "1" ]; then
                        echo "  - Ad-hoc fallback is disabled (SHIPYARD_NO_ADHOC_FALLBACK=1)." >&2
                        echo "    Remove that env var to let install.sh try a local re-sign." >&2
                    fi
                else
                    echo "Run '${INSTALL_DIR}/shipyard --version' manually for a specific error." >&2
                fi
                echo "" >&2
                exit 1
            fi
        fi
    fi
fi

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

# #236: classic PAT flow — flag missing `workflow` scope here so
# the user fixes it at install time instead of discovering it
# during `cloud retarget --apply`. Silent when gh isn't installed /
# logged out (the doctor covers those) and silent for fine-grained
# tokens + GitHub Apps which don't expose their scope list locally.
if command -v gh >/dev/null 2>&1; then
    # Scope probe to github.com specifically (Codex P2 on #237).
    # Bare `gh auth status` inspects every configured host (GHE,
    # etc.) and would false-positive or false-negative from
    # unrelated hosts. Shipyard's retarget/handoff calls go to
    # github.com, so that's the only host whose scope matters.
    gh_status_out=$(gh auth status --hostname github.com 2>&1 || true)
    if echo "${gh_status_out}" | grep -qi "Token scopes:" \
        && ! echo "${gh_status_out}" | grep -q "'workflow'"; then
        echo "heads up: \`shipyard cloud retarget\` + \`cloud handoff run --apply\`"
        echo "  need the 'workflow' scope on your gh token, which isn't set."
        echo "  Fix: gh auth refresh -h github.com -s workflow"
        echo "  (Fine-grained tokens + GitHub Apps: docs/install.md § First-run auth)"
        echo ""
    fi
fi

echo "Next steps:"
echo "  shipyard init        # set up a project"
echo "  shipyard doctor      # check your environment"
echo "  shipyard run         # validate current branch"
