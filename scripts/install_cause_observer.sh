#!/usr/bin/env bash
# Install the supervised cause-record observer on a macOS controller.
#
# The default is a dry run. --install renders and validates a launchd agent
# which runs the read-only `shipyard cause record` observer independently of
# baseline collection. The archive is append-only and the command's own lock
# makes overlapping passes safe.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
TEMPLATE="$HERE/launchd/com.danielraffel.shipyard.cause-observer.plist.template"
LABEL="com.danielraffel.shipyard.cause-observer"
SHIPYARD="$(command -v shipyard 2>/dev/null || true)"
REPO="Generous-Corp/pulp"
WORKFLOW="build.yml"
DIR="$HOME/Library/Application Support/shipyard/ci-cause"
BASE="main"
CWD="$HOME"
INTERVAL=3600
APPLY=0
LAUNCHCTL="${SHIPYARD_LAUNCHCTL_BIN:-/bin/launchctl}"
PLUTIL="${SHIPYARD_PLUTIL_BIN:-/usr/bin/plutil}"

usage() {
  cat <<'USAGE'
usage: install_cause_observer.sh [--shipyard ABSOLUTE_PATH] [--repo OWNER/REPO]
       [--workflow FILE] [--dir ABSOLUTE_PATH] [--base BRANCH]
       [--cwd ABSOLUTE_PATH] [--interval SECONDS] [--install]

Prints a plan by default. The observer is read-only toward GitHub and writes
only its append-only local cause/fix-link archive.
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --shipyard) SHIPYARD="${2:-}"; shift 2 ;;
    --repo) REPO="${2:-}"; shift 2 ;;
    --workflow) WORKFLOW="${2:-}"; shift 2 ;;
    --dir) DIR="${2:-}"; shift 2 ;;
    --base) BASE="${2:-}"; shift 2 ;;
    --cwd) CWD="${2:-}"; shift 2 ;;
    --interval) INTERVAL="${2:-}"; shift 2 ;;
    --install) APPLY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$INTERVAL" in ''|*[!0-9]*) echo "--interval must be numeric" >&2; exit 2 ;; esac
[ "$INTERVAL" -ge 900 ] || { echo "--interval must be at least 900 seconds" >&2; exit 2; }
case "$SHIPYARD" in /*) ;; *) echo "--shipyard must be an absolute path (got '$SHIPYARD')" >&2; exit 2 ;; esac
case "$DIR" in /*) ;; *) echo "--dir must be an absolute path (got '$DIR')" >&2; exit 2 ;; esac
case "$CWD" in /*) ;; *) echo "--cwd must be an absolute path (got '$CWD')" >&2; exit 2 ;; esac
OWNER="${REPO%%/*}"
NAME="${REPO#*/}"
[ -n "$OWNER" ] && [ -n "$NAME" ] && [ "$REPO" = "$OWNER/$NAME" ] || {
  echo "--repo must be OWNER/REPO with nonempty components" >&2
  exit 2
}
case "$NAME" in */*)
  echo "--repo must contain exactly one slash" >&2
  exit 2
  ;;
esac
case "$REPO" in *[[:space:]]*|*[![:print:]]*)
  echo "--repo must not contain whitespace or control characters" >&2
  exit 2
  ;;
esac
[ -n "$WORKFLOW" ] || { echo "--workflow must not be empty" >&2; exit 2; }
[ -n "$BASE" ] || { echo "--base must not be empty" >&2; exit 2; }
[ -x "$SHIPYARD" ] || { echo "Shipyard executable is unavailable: $SHIPYARD" >&2; exit 2; }
[ -f "$TEMPLATE" ] || { echo "installer must run from a complete Shipyard checkout" >&2; exit 2; }
"$SHIPYARD" cause record --help >/dev/null 2>&1 || {
  echo "$SHIPYARD has no 'cause record'; update it before installing the agent" >&2
  exit 2
}

PLIST_DIR="$HOME/Library/LaunchAgents"
PLIST="$PLIST_DIR/$LABEL.plist"
LOG="$HOME/Library/Logs/shipyard-cause-observer.log"
echo "Shipyard cause observer install plan:"
echo "  shipyard=$SHIPYARD"
echo "  repo=$REPO workflow=$WORKFLOW base=$BASE interval=${INTERVAL}s"
echo "  cwd=$CWD"
echo "  archive=$DIR"
echo "  launch_agent=$PLIST"
echo "  log=$LOG"
[ "$APPLY" = 1 ] || { echo "  action=dry-run (pass --install to apply)"; exit 0; }

umask 077
mkdir -p "$PLIST_DIR" "$HOME/Library/Logs" "$DIR"
STAGED="$(mktemp "$PLIST_DIR/.$LABEL.XXXXXX")"
trap 'rm -f "$STAGED"' EXIT
escape() { printf '%s' "$1" | sed -e 's/[&|\\]/\\&/g'; }
sed -e "s|@SHIPYARD@|$(escape "$SHIPYARD")|g" \
    -e "s|@REPO@|$(escape "$REPO")|g" \
    -e "s|@WORKFLOW@|$(escape "$WORKFLOW")|g" \
    -e "s|@DIR@|$(escape "$DIR")|g" \
    -e "s|@BASE@|$(escape "$BASE")|g" \
    -e "s|@CWD@|$(escape "$CWD")|g" \
    -e "s|@HOME@|$(escape "$HOME")|g" \
    -e "s|@INTERVAL@|$INTERVAL|g" \
    -e "s|@LOG@|$(escape "$LOG")|g" \
    "$TEMPLATE" > "$STAGED"
if grep -q '@[A-Z_]*@' "$STAGED"; then
  echo "template left an unrendered placeholder" >&2
  exit 1
fi
"$PLUTIL" -lint "$STAGED" >/dev/null
chmod 644 "$STAGED"
mv -f "$STAGED" "$PLIST"
trap - EXIT
DOMAIN="gui/$(id -u)"
"$LAUNCHCTL" bootout "$DOMAIN/$LABEL" >/dev/null 2>&1 || true
"$LAUNCHCTL" bootstrap "$DOMAIN" "$PLIST"
echo "  action=installed and loaded $LABEL"
