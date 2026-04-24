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

# ── Notarize ───────────────────────────────────────────────────────
echo "Step 3/5: Submit to Apple notarization service (this takes ~30-90s)..."
NOTARIZE_ZIP="$(mktemp -d)/${ARTIFACT}.zip"
/usr/bin/ditto -c -k --keepParent "$DIST_BINARY" "$NOTARIZE_ZIP"

# Capture notarytool output so we can grep for the submission id on
# failure — Apple's rejection reasons are only findable via
# `notarytool log <submission-id>`.
NOTARIZE_LOG="$(mktemp)"
if ! xcrun notarytool submit "$NOTARIZE_ZIP" \
        --apple-id "$SHIPYARD_NOTARIZE_APPLE_ID" \
        --team-id "$SHIPYARD_NOTARIZE_TEAM_ID" \
        --password "$SHIPYARD_NOTARIZE_APP_PASSWORD" \
        --wait 2>&1 | tee "$NOTARIZE_LOG"; then
    echo "ERROR: notarytool submit failed. Log: $NOTARIZE_LOG" >&2
    exit 1
fi

if grep -q "status: Accepted" "$NOTARIZE_LOG"; then
    echo "  ✓ Notarization Accepted"
else
    SUB_ID=$(grep -E "^\s+id:" "$NOTARIZE_LOG" | head -1 | awk '{print $NF}')
    echo "ERROR: notarization not Accepted. Submission id: $SUB_ID" >&2
    echo "       Fetch the log with:" >&2
    echo "         xcrun notarytool log $SUB_ID \\" >&2
    echo "           --apple-id \$SHIPYARD_NOTARIZE_APPLE_ID \\" >&2
    echo "           --team-id \$SHIPYARD_NOTARIZE_TEAM_ID \\" >&2
    echo "           --password \$SHIPYARD_NOTARIZE_APP_PASSWORD" >&2
    exit 1
fi

# ── Local launch test (#219 — THE WHOLE POINT) ─────────────────────
# CI's launch gate runs on a GH Actions macOS-15 runner, which is
# NOT the Mac users will actually run the binary on. A notarized
# binary that launches on the CI runner but SIGKILLs on the user's
# Mac is exactly the #219 failure mode. Here we test on the ACTUAL
# Mac that's about to ship the binary — if it dies here, we refuse
# to upload.
echo "Step 4/5: Local launch test (taskgated / Gatekeeper / codesign)..."
if ! OUTPUT=$("$DIST_BINARY" --version 2>&1); then
    echo "" >&2
    echo "ERROR: LOCAL LAUNCH TEST FAILED on this Mac." >&2
    echo "       Binary: $DIST_BINARY" >&2
    echo "       This is the #219 failure shape. Do NOT ship this binary." >&2
    echo "" >&2
    echo "       Diagnostics:" >&2
    codesign --verify --deep --strict --verbose=4 "$DIST_BINARY" 2>&1 | sed 's/^/         /' >&2
    spctl --assess --type execute -vv "$DIST_BINARY" 2>&1 | sed 's/^/         /' >&2
    xattr -l "$DIST_BINARY" 2>&1 | sed 's/^/         /' >&2
    echo "" >&2
    echo "       Crash report (if any) under:" >&2
    echo "         ~/Library/Logs/DiagnosticReports/shipyard-*.ips" >&2
    echo "" >&2
    exit 3
fi
echo "  ✓ --version printed: ${OUTPUT}"

# ── Upload (opt-in) ────────────────────────────────────────────────
if [ "$DO_UPLOAD" -eq 1 ]; then
    echo "Step 5/5: Uploading $DIST_BINARY to release $TAG..."
    # Overwrite if an asset already exists — typically this script
    # replaces CI's broken asset with a known-working local one.
    gh release upload "$TAG" "$DIST_BINARY" --clobber
    # Update the checksum line for this artifact. The release's
    # checksums.sha256 contains one line per platform; we replace
    # just our line. If the file doesn't exist yet, this creates it.
    CHECKSUM=$(shasum -a 256 "$DIST_BINARY" | awk '{print $1}')
    LINE="${CHECKSUM}  ${ARTIFACT}"
    CHECKSUMS_TMP="$(mktemp)"
    if gh release view "$TAG" --json assets --jq '.assets[] | select(.name=="checksums.sha256") | .name' | grep -q checksums.sha256; then
        gh release download "$TAG" --pattern checksums.sha256 --output "$CHECKSUMS_TMP" --clobber
        # Drop any existing line for this artifact; append the new one.
        grep -v "  ${ARTIFACT}\$" "$CHECKSUMS_TMP" > "${CHECKSUMS_TMP}.new" || true
        echo "$LINE" >> "${CHECKSUMS_TMP}.new"
        mv "${CHECKSUMS_TMP}.new" "$CHECKSUMS_TMP"
    else
        echo "$LINE" > "$CHECKSUMS_TMP"
    fi
    gh release upload "$TAG" "$CHECKSUMS_TMP" --clobber
    # Rename into the expected filename via clobber upload.
    # gh upload uses the basename, so rename first:
    mv "$CHECKSUMS_TMP" "$(dirname "$CHECKSUMS_TMP")/checksums.sha256"
    gh release upload "$TAG" "$(dirname "$CHECKSUMS_TMP")/checksums.sha256" --clobber
    echo "  ✓ Uploaded to release $TAG"
else
    echo "Step 5/5: SKIPPED (--upload not passed)."
    echo ""
    echo "Signed + notarized + tested binary is at: $DIST_BINARY"
    echo "Re-run with --upload to ship it to release $TAG."
fi

echo ""
echo "═══ Done. ═══"
echo ""
