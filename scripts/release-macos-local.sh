#!/usr/bin/env bash
# Build, sign, notarize, AND TEST the macOS shipyard binary on this Mac.
#
# Motivation (#219): CI-signed + Apple-notarized binaries shipped in
# v0.42.0 and v0.43.0 both SIGKILL'd with "Taskgated Invalid Signature"
# on the primary maintainer's Mac — even though they passed CI's own
# launch gate on a GH Actions macOS-15 runner. That strongly suggests
# the problem is either (a) CI's signing pipeline producing a ticket
# that's subtly different from what Apple returns for the same cert
# locally, or (b) a per-Mac Gatekeeper/taskgated state that won't
# accept tickets from a specific CI environment.
#
# This script is "Option B" from the #219 discussion: take signing
# completely off CI and put it on the Mac that's actually going to
# run the binary. The build is deterministic in the sense that the
# PyInstaller flags match CI's exactly — only the environment is
# different. If the locally-built+signed binary launches cleanly
# here, that's definitive proof CI signing was broken.
#
# Even more important per user instruction: THIS SCRIPT TESTS THE
# BINARY AFTER NOTARIZATION AND BEFORE UPLOAD. No asset uploads if
# `--version` doesn't print. That's the whole point — if CI did
# this we'd have caught v0.42.0 before it shipped.
#
# Usage:
#   ./scripts/release-macos-local.sh [--tag vX.Y.Z] [--upload] [--arch arm64|x64]
#
# Env vars required for notarization:
#   SHIPYARD_NOTARIZE_APPLE_ID           (Apple ID email)
#   SHIPYARD_NOTARIZE_TEAM_ID            (Developer Team ID, e.g. 95CX6P84C4)
#   SHIPYARD_NOTARIZE_APP_PASSWORD       (app-specific password from appleid.apple.com)
#   SHIPYARD_SIGNING_IDENTITY            (SHA-1 fingerprint OR subject CN of Developer-ID cert)
#
# Flags:
#   --tag vX.Y.Z   Assume the current commit is tagged vX.Y.Z.
#                  Defaults to `git describe --tags`.
#   --upload       Upload the signed+tested asset to the GitHub release
#                  for --tag. Without this, the script only builds and
#                  verifies — useful for diagnosing without shipping.
#   --arch         Build for arm64 (default) or x64. The host CPU is
#                  what you'll actually get; x64 needs Rosetta or a
#                  different host.
#
# Exit codes:
#   0   Build + sign + notarize + local-launch test all passed.
#       Asset was uploaded if --upload was passed.
#   1   One of the steps failed. Diagnostics on stderr.
#   2   Missing required env var.
#   3   --version smoke test FAILED on this Mac. Do NOT ship this binary.

set -euo pipefail

ARCH="arm64"
TAG=""
DO_UPLOAD=0

while [ $# -gt 0 ]; do
    case "$1" in
        --tag) TAG="$2"; shift 2 ;;
        --arch) ARCH="$2"; shift 2 ;;
        --upload) DO_UPLOAD=1; shift ;;
        -h|--help)
            sed -n '/^# Usage:/,/^$/p' "$0" | sed 's/^# //; s/^#//'
            exit 0
            ;;
        *) echo "Unknown flag: $1" >&2; exit 1 ;;
    esac
done

if [ -z "$TAG" ]; then
    TAG=$(git describe --tags --exact-match 2>/dev/null || true)
    if [ -z "$TAG" ]; then
        echo "ERROR: --tag required (current commit is not a tagged release)" >&2
        echo "       Pass --tag vX.Y.Z explicitly, or tag the commit first." >&2
        exit 2
    fi
fi

# Every env var must be set for notarization. We bail out BEFORE
# the expensive build step rather than letting the user burn 60s
# on PyInstaller only to discover they forgot a secret.
for var in SHIPYARD_NOTARIZE_APPLE_ID SHIPYARD_NOTARIZE_TEAM_ID \
           SHIPYARD_NOTARIZE_APP_PASSWORD SHIPYARD_SIGNING_IDENTITY; do
    if [ -z "${!var:-}" ]; then
        echo "ERROR: $var is not set in the environment." >&2
        echo "       See scripts/release-macos-local.sh header for the full list." >&2
        exit 2
    fi
done

ARTIFACT="shipyard-macos-${ARCH}"
DIST_BINARY="dist/${ARTIFACT}"

echo ""
echo "═══ Local macOS release build: ${TAG} (${ARCH}) ═══"
echo ""

# ── Build ──────────────────────────────────────────────────────────
# Mirror CI's invocation from .github/workflows/release.yml line 251
# so the resulting binary is byte-identical to what CI would produce
# on the same commit (modulo signing timestamp). If this binary
# launches but the CI-signed one doesn't, the delta is signing.
echo "Step 1/5: PyInstaller build..."
if ! command -v pyinstaller >/dev/null 2>&1; then
    echo "ERROR: pyinstaller not on PATH. pip install pyinstaller." >&2
    exit 1
fi
rm -rf dist/ build/ "${ARTIFACT}".spec
pyinstaller --onefile \
    --name "$ARTIFACT" \
    --codesign-identity "$SHIPYARD_SIGNING_IDENTITY" \
    src/shipyard/cli.py >/dev/null

if [ ! -f "$DIST_BINARY" ]; then
    echo "ERROR: PyInstaller did not produce $DIST_BINARY" >&2
    exit 1
fi

# ── Re-sign with hardened runtime + timestamp ──────────────────────
echo "Step 2/5: Re-sign with hardened runtime + secure timestamp..."
codesign --force --options runtime --timestamp \
    --sign "$SHIPYARD_SIGNING_IDENTITY" \
    "$DIST_BINARY"
codesign -dv --verbose=4 "$DIST_BINARY" 2>&1 | grep -E "Authority|TeamIdentifier|Timestamp" | head -5

# ── Package into .dmg ──────────────────────────────────────────────
# #219 / task #52: bare Mach-O binaries cannot be stapled (no
# Info.plist for stapler to anchor the ticket to). The online
# notarization check Apple falls back to is demonstrably flaky on
# some Macs — v0.42.0 + v0.43.0 both shipped binaries that launched
# on CI + on the build machine but SIGKILL'd on the end-user Mac.
# Wrapping the binary in a .dmg and stapling the DMG puts the
# notarization ticket in a local file the mount path reads
# offline. Gatekeeper + taskgated verify against that ticket
# instead of calling Apple, so per-Mac network/CDN/provenance
# state stops mattering.
echo "Step 3/6: Package signed Mach-O into .dmg..."
DMG_NAME="${ARTIFACT}.dmg"
DIST_DMG="dist/${DMG_NAME}"
DMG_STAGING="$(mktemp -d)/stage"
mkdir -p "$DMG_STAGING"
# Staging dir contains just the bare binary — hdiutil turns the
# directory into a mountable volume whose root has that binary.
# install.sh's mount-extract step looks for /Volumes/Shipyard/shipyard
# (volname matches the HFS+ volume label).
cp "$DIST_BINARY" "$DMG_STAGING/shipyard"
rm -f "$DIST_DMG"
hdiutil create -volname "Shipyard" \
    -srcfolder "$DMG_STAGING" \
    -ov -format UDZO \
    "$DIST_DMG" >/dev/null

# ── Sign the DMG ───────────────────────────────────────────────────
# The DMG itself needs to carry a valid Developer-ID signature so
# notarytool can match + return a staple-able ticket. Without this
# the DMG ships unsigned and notarize fails with "package is not
# signed".
echo "Step 4/6: Sign the DMG..."
codesign --force --sign "$SHIPYARD_SIGNING_IDENTITY" "$DIST_DMG"
codesign -dv --verbose=2 "$DIST_DMG" 2>&1 | grep -E "Authority|TeamIdentifier" | head -3

# ── Notarize the DMG, then staple ──────────────────────────────────
# Submit the DMG itself (not a zip-wrapped Mach-O like the
# pre-#52 path). notarytool accepts .dmg as a first-class input.
# After acceptance, `stapler staple` embeds the ticket in the DMG
# so first-launch verification is OFFLINE — the entire point of
# task #52.
echo "Step 5/6: Submit DMG to Apple notarization service (~30-90s)..."
NOTARIZE_LOG="$(mktemp)"
if ! xcrun notarytool submit "$DIST_DMG" \
        --apple-id "$SHIPYARD_NOTARIZE_APPLE_ID" \
        --team-id "$SHIPYARD_NOTARIZE_TEAM_ID" \
        --password "$SHIPYARD_NOTARIZE_APP_PASSWORD" \
        --wait 2>&1 | tee "$NOTARIZE_LOG"; then
    echo "ERROR: notarytool submit failed. Log: $NOTARIZE_LOG" >&2
    exit 1
fi
if ! grep -q "status: Accepted" "$NOTARIZE_LOG"; then
    SUB_ID=$(grep -E "^\s+id:" "$NOTARIZE_LOG" | head -1 | awk '{print $NF}')
    echo "ERROR: notarization not Accepted. Submission id: $SUB_ID" >&2
    echo "       Fetch the log with:" >&2
    echo "         xcrun notarytool log $SUB_ID \\" >&2
    echo "           --apple-id \$SHIPYARD_NOTARIZE_APPLE_ID \\" >&2
    echo "           --team-id \$SHIPYARD_NOTARIZE_TEAM_ID \\" >&2
    echo "           --password \$SHIPYARD_NOTARIZE_APP_PASSWORD" >&2
    exit 1
fi
echo "  ✓ Notarization Accepted"

echo "  Stapling ticket to DMG..."
if ! xcrun stapler staple "$DIST_DMG"; then
    echo "ERROR: xcrun stapler staple failed. DMG ships unstapled —" >&2
    echo "       refusing to upload because that re-introduces the" >&2
    echo "       online-check dependency #52 is supposed to remove." >&2
    exit 1
fi
# Validate the staple — if this passes, the ticket is bound to
# the DMG and Gatekeeper can verify offline.
if ! xcrun stapler validate "$DIST_DMG" >/dev/null 2>&1; then
    echo "ERROR: stapler validate failed — ticket is not bound to DMG." >&2
    exit 1
fi
echo "  ✓ Ticket stapled and validated"

# ── Local launch test (#219 — THE WHOLE POINT) ─────────────────────
# Mount the stapled DMG, run the binary inside. This simulates the
# Gatekeeper-verifies-offline flow end users will hit after
# install.sh extracts. If it fails here, the dmg is broken —
# refuse to upload.
echo "Step 6/6: Local launch test via mounted DMG..."
MOUNT_POINT="$(mktemp -d)/mnt"
# -nobrowse: don't show the volume in Finder. -readonly: prevent
# accidental writes. Explicit mountpoint so we don't scrape
# /Volumes/ looking for the right volume.
if ! hdiutil attach -nobrowse -readonly \
        -mountpoint "$MOUNT_POINT" "$DIST_DMG" >/dev/null; then
    echo "ERROR: could not mount DMG." >&2
    exit 1
fi
trap 'hdiutil detach "$MOUNT_POINT" >/dev/null 2>&1 || true' EXIT
if ! OUTPUT=$("$MOUNT_POINT/shipyard" --version 2>&1); then
    echo "" >&2
    echo "ERROR: LOCAL LAUNCH TEST FAILED from mounted DMG." >&2
    echo "       DMG: $DIST_DMG" >&2
    echo "       Do NOT ship this binary." >&2
    echo "" >&2
    echo "       Diagnostics:" >&2
    # `|| true` on every probe (Codex P2 on #224). These run under
    # `set -euo pipefail`, and on a genuinely-broken DMG
    # `codesign --verify` / `stapler validate` / `spctl --assess`
    # will all exit non-zero — which is the *point* of running them.
    # Without the fallthrough, pipefail terminates the script on the
    # first probe failure and the operator loses the rest of the
    # diagnostics AND the documented exit 3 (launch-test failure
    # signal) becomes a generic exit 1 instead.
    codesign --verify --deep --strict --verbose=4 "$DIST_DMG" 2>&1 | sed 's/^/         /' >&2 || true
    xcrun stapler validate -v "$DIST_DMG" 2>&1 | sed 's/^/         /' >&2 || true
    spctl --assess --type install -vv "$DIST_DMG" 2>&1 | sed 's/^/         /' >&2 || true
    echo "" >&2
    echo "       Crash report (if any) under:" >&2
    echo "         ~/Library/Logs/DiagnosticReports/shipyard-*.ips" >&2
    echo "" >&2
    hdiutil detach "$MOUNT_POINT" >/dev/null 2>&1 || true
    exit 3
fi
echo "  ✓ --version printed: ${OUTPUT}"

# Detach the DMG before further work — the mount is no longer
# needed and leaving it attached confuses the later e2e step
# which mounts a freshly-downloaded copy.
hdiutil detach "$MOUNT_POINT" >/dev/null 2>&1 || true
trap - EXIT

# ── Upload (opt-in) ────────────────────────────────────────────────
if [ "$DO_UPLOAD" -eq 1 ]; then
    echo "Step 7/8: Uploading $DIST_DMG to release $TAG..."
    gh release upload "$TAG" "$DIST_DMG" --clobber
    # Update the checksum line for this artifact. Keyed on the
    # full asset filename (e.g. shipyard-macos-arm64.dmg) so an
    # older shipyard-macos-arm64 (bare Mach-O) line from a
    # previous release of the same tag doesn't collide.
    CHECKSUM=$(shasum -a 256 "$DIST_DMG" | awk '{print $1}')
    LINE="${CHECKSUM}  ${DMG_NAME}"
    CHECKSUMS_DIR="$(mktemp -d)"
    CHECKSUMS_FILE="${CHECKSUMS_DIR}/checksums.sha256"
    if gh release view "$TAG" --json assets \
            --jq '.assets[] | select(.name=="checksums.sha256") | .name' \
            | grep -q checksums.sha256; then
        gh release download "$TAG" --pattern checksums.sha256 \
            --output "$CHECKSUMS_FILE" --clobber
        grep -v "  ${DMG_NAME}\$" "$CHECKSUMS_FILE" \
            > "${CHECKSUMS_FILE}.new" || true
        echo "$LINE" >> "${CHECKSUMS_FILE}.new"
        mv "${CHECKSUMS_FILE}.new" "$CHECKSUMS_FILE"
    else
        echo "$LINE" > "$CHECKSUMS_FILE"
    fi
    gh release upload "$TAG" "$CHECKSUMS_FILE" --clobber
    rm -rf "$CHECKSUMS_DIR"
    echo "  ✓ Uploaded to release $TAG"

    # ── End-to-end verification (#55 codification) ─────────────────
    # Test the same install.sh → launch path end users hit. The
    # local launch test above proved the DMG works when mounted
    # directly; this proves the DOWNLOAD + install path works,
    # which is the gap we kept shipping past.
    #
    # Uses the install.sh in THIS checkout — not the remote copy on
    # main — so a change-in-progress to install.sh gets validated
    # against the real DMG *before* the change is merged. After
    # merge + release, the tag's install.sh and this checkout's
    # install.sh are the same file, so the test remains honest.
    #
    # If this fails, the upload already happened: operator must
    # manually delete the uploaded asset or re-run after fixing.
    # Exit 4 distinguishes this from the local mounted-launch test
    # failure (exit 3).
    echo "Step 8/8: End-to-end verification (local install.sh → launch)..."
    E2E_TMPDIR="$(mktemp -d)"
    E2E_INSTALL_DIR="$E2E_TMPDIR/bin"
    LOCAL_INSTALL_SH="$(cd "$(dirname "$0")/.." && pwd)/install.sh"
    if ! SHIPYARD_INSTALL_DIR="$E2E_INSTALL_DIR" \
            SHIPYARD_VERSION="$TAG" \
            bash "$LOCAL_INSTALL_SH" \
            >"$E2E_TMPDIR/install.log" 2>&1; then
        echo "" >&2
        echo "ERROR: E2E install.sh FAILED for $TAG." >&2
        echo "       Upload already happened — this DMG is live but" >&2
        echo "       end-users won't be able to install it cleanly." >&2
        echo "       Install log:" >&2
        sed 's/^/         /' "$E2E_TMPDIR/install.log" >&2
        rm -rf "$E2E_TMPDIR"
        exit 4
    fi
    if ! E2E_OUTPUT=$("$E2E_INSTALL_DIR/shipyard" --version 2>&1); then
        echo "" >&2
        echo "ERROR: E2E installed binary FAILED to launch." >&2
        echo "       Upload already happened. The DMG mounts + verifies" >&2
        echo "       but the extracted binary does not survive the" >&2
        echo "       install.sh download + post-processing path." >&2
        echo "       Install log:" >&2
        sed 's/^/         /' "$E2E_TMPDIR/install.log" >&2
        rm -rf "$E2E_TMPDIR"
        exit 4
    fi
    echo "  ✓ End-to-end install.sh + launch passed: ${E2E_OUTPUT}"
    rm -rf "$E2E_TMPDIR"
else
    echo "Step 7/8: SKIPPED (--upload not passed)."
    echo "Step 8/8: E2E verification SKIPPED (requires upload)."
    echo ""
    echo "Signed + notarized + stapled DMG at: $DIST_DMG"
    echo "Re-run with --upload to ship it to release $TAG."
fi

echo ""
echo "═══ Done. ═══"
echo ""
