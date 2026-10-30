#!/usr/bin/env bash
# Install the auto-merge arming backstop as a launchd agent.
#
# Prints the plan by default and changes nothing. --install renders the
# template, validates it with plutil, writes it atomically, and (re)loads the
# agent with launchctl bootout/bootstrap. The agent runs
# `shipyard --json runner steward --arm-unqueued --apply` every 15 minutes, so
# a green pull request cannot sit unqueued because nothing armed it. Each run
# is bounded by /usr/bin/timeout: launchd never starts a tick while the last
# one is still running, so one hung pass would otherwise stop every later one.
# --status reports the loaded agent and its log; --uninstall unloads it and
# removes the plist.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
TEMPLATE="$HERE/launchd/com.danielraffel.shipyard.arm-unqueued.plist.template"
LABEL="com.danielraffel.shipyard.arm-unqueued"
SHIPYARD="$(command -v shipyard 2>/dev/null || true)"
INTERVAL=900
TIMEOUT=
BASE=main
APPLY=0
ACTION=plan
REPOS=()
LAUNCHCTL="${SHIPYARD_LAUNCHCTL_BIN:-/bin/launchctl}"
PLUTIL="${SHIPYARD_PLUTIL_BIN:-/usr/bin/plutil}"
TIMEOUT_BIN="${SHIPYARD_TIMEOUT_BIN:-/usr/bin/timeout}"
# After TERM at the deadline, launchd's job gets this long before KILL.
KILL_AFTER=30

usage() {
  cat <<'USAGE'
usage: install_arm_unqueued.sh --repo OWNER/REPO [--repo OWNER/REPO ...]
       [--shipyard ABSOLUTE_PATH] [--base BRANCH] [--interval SECONDS]
       [--timeout SECONDS] [--install]
       install_arm_unqueued.sh --status | --uninstall

Prints a plan by default. At least one --repo is required: an unattended agent
must never infer its repositories from whatever directory launchd starts it in.
--timeout bounds each run (default: interval minus 60 seconds).
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --repo) REPOS+=("${2:-}"); shift 2 ;;
    --shipyard) SHIPYARD="${2:-}"; shift 2 ;;
    --base) BASE="${2:-}"; shift 2 ;;
    --interval) INTERVAL="${2:-}"; shift 2 ;;
    --timeout) TIMEOUT="${2:-}"; shift 2 ;;
    --install) APPLY=1; shift ;;
    --status) ACTION=status; shift ;;
    --uninstall) ACTION=uninstall; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

PLIST_DIR="$HOME/Library/LaunchAgents"
PLIST="$PLIST_DIR/$LABEL.plist"
LOG="$HOME/Library/Logs/shipyard-arm-unqueued.log"
DOMAIN="gui/$(id -u)"

if [ "$ACTION" = status ]; then
  echo "launch_agent=$PLIST ($([ -f "$PLIST" ] && echo present || echo absent))"
  if "$LAUNCHCTL" print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    echo "loaded=yes"
    "$LAUNCHCTL" print "$DOMAIN/$LABEL" | grep -E 'state =|last exit code|runs =' || true
  else
    echo "loaded=no"
  fi
  echo "log=$LOG"
  [ -f "$LOG" ] && tail -n 20 "$LOG"
  exit 0
fi
if [ "$ACTION" = uninstall ]; then
  "$LAUNCHCTL" bootout "$DOMAIN/$LABEL" >/dev/null 2>&1 || true
  rm -f "$PLIST"
  echo "  action=unloaded $LABEL and removed $PLIST (log kept at $LOG)"
  exit 0
fi

[ "${#REPOS[@]}" -gt 0 ] || { echo "--repo is required" >&2; usage >&2; exit 2; }
for repo in "${REPOS[@]}"; do
  case "$repo" in
    */*/*|/*|*/) echo "--repo must be OWNER/REPO (got '$repo')" >&2; exit 2 ;;
    */*) ;;
    *) echo "--repo must be OWNER/REPO (got '$repo')" >&2; exit 2 ;;
  esac
done
case "$INTERVAL" in ''|*[!0-9]*) echo "--interval must be a positive integer" >&2; exit 2 ;; esac
[ "$INTERVAL" -ge 300 ] || { echo "--interval must be at least 300 seconds" >&2; exit 2; }
[ -n "$TIMEOUT" ] || TIMEOUT=$((INTERVAL - 60))
case "$TIMEOUT" in ''|*[!0-9]*) echo "--timeout must be a positive integer" >&2; exit 2 ;; esac
[ "$TIMEOUT" -ge 60 ] && [ "$TIMEOUT" -lt "$INTERVAL" ] || {
  echo "--timeout must be at least 60 seconds and less than --interval" >&2; exit 2; }
[ -x "$TIMEOUT_BIN" ] || { echo "timeout executable is unavailable: $TIMEOUT_BIN" >&2; exit 2; }
case "$BASE" in ''|*[!A-Za-z0-9._/-]*) echo "--base must be a plain branch name" >&2; exit 2 ;; esac
case "$SHIPYARD" in /*) ;; *) echo "--shipyard must be an absolute path (got '$SHIPYARD')" >&2; exit 2 ;; esac
[ -x "$SHIPYARD" ] || { echo "Shipyard executable is unavailable: $SHIPYARD" >&2; exit 2; }
# A Shipyard without the flag would run the steward's ordinary pass with
# --apply, which is a different and much broader mutation than arming.
"$SHIPYARD" runner steward --help 2>/dev/null | grep -q -- '--arm-unqueued' || {
  echo "$SHIPYARD has no 'runner steward --arm-unqueued'; update it before installing the agent" >&2
  exit 2
}
[ -f "$TEMPLATE" ] || { echo "installer must run from a complete Shipyard checkout" >&2; exit 2; }

echo "Shipyard auto-merge arming backstop install plan:"
echo "  shipyard=$SHIPYARD"
echo "  repos=${REPOS[*]}"
echo "  base=$BASE interval=${INTERVAL}s timeout=${TIMEOUT}s"
echo "  launch_agent=$PLIST"
echo "  log=$LOG"
[ "$APPLY" = 1 ] || { echo "  action=dry-run (pass --install to apply)"; exit 0; }

# Rehearse exactly what launchd will run, under the plist's environment, but
# WITHOUT --apply: the steward's audit mode mutates nothing, so a rehearsal
# proves the repos are readable and the pass is healthy before it is loaded.
AGENT_PATH="/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
REPO_FLAGS=()
for repo in "${REPOS[@]}"; do REPO_FLAGS+=(--repo "$repo"); done
set +e
REHEARSAL="$(cd "$HOME" && /usr/bin/env -i HOME="$HOME" PATH="$AGENT_PATH" \
  "$TIMEOUT_BIN" -k "$KILL_AFTER" "$TIMEOUT" \
  "$SHIPYARD" --json runner steward --base "$BASE" "${REPO_FLAGS[@]}" \
  --arm-unqueued --no-coalesce --no-preempt-capacity 2>&1)"
REHEARSAL_EXIT=$?
set -e
if [ "$REHEARSAL_EXIT" != 0 ]; then
  echo "arming rehearsal under the agent environment exited $REHEARSAL_EXIT; not loading the agent:" >&2
  printf '%s\n' "$REHEARSAL" | tail -n 20 >&2
  exit 1
fi
echo "  rehearsal=ok (audit mode mutated nothing)"

umask 077
mkdir -p "$PLIST_DIR" "$HOME/Library/Logs"
STAGED="$(mktemp "$PLIST_DIR/.$LABEL.XXXXXX")"
escape() { printf '%s' "$1" | sed -e 's/[&|\\]/\\&/g'; }
# The repo arguments are multi-line, so they are substituted with sed's `r`
# (read file) rather than through a variable: BSD/macOS awk rejects a newline
# inside a -v assignment, and `s|...|multi-line|` is not portable either.
REPO_ARGS_FILE="$(mktemp "${TMPDIR:-/tmp}/shipyard-arm-repos.XXXXXX")"
trap 'rm -f "$STAGED" "$REPO_ARGS_FILE"' EXIT
for repo in "${REPOS[@]}"; do
  printf '        <string>--repo</string>\n        <string>%s</string>\n' \
    "$(printf '%s' "$repo" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g')" \
    >> "$REPO_ARGS_FILE"
done
sed -e "s|@SHIPYARD@|$(escape "$SHIPYARD")|g" \
    -e "s|@HOME@|$(escape "$HOME")|g" \
    -e "s|@BASE@|$(escape "$BASE")|g" \
    -e "s|@INTERVAL@|$INTERVAL|g" \
    -e "s|@TIMEOUT_BIN@|$(escape "$TIMEOUT_BIN")|g" \
    -e "s|@KILL_AFTER@|$KILL_AFTER|g" \
    -e "s|@TIMEOUT@|$TIMEOUT|g" \
    "$TEMPLATE" \
  | sed -e "/^@REPO_ARGS@$/r $REPO_ARGS_FILE" -e "/^@REPO_ARGS@$/d" > "$STAGED"
if grep -q '@[A-Z_]*@' "$STAGED"; then
  echo "template left an unrendered placeholder" >&2
  exit 1
fi
"$PLUTIL" -lint "$STAGED" >/dev/null
chmod 644 "$STAGED"
mv -f "$STAGED" "$PLIST"
trap - EXIT
"$LAUNCHCTL" bootout "$DOMAIN/$LABEL" >/dev/null 2>&1 || true
"$LAUNCHCTL" bootstrap "$DOMAIN" "$PLIST"
echo "  action=installed and loaded $LABEL"
