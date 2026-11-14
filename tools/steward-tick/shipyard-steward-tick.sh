#!/bin/bash
# Single admission writer: run Shipyard's steward locally for each repo.
# Shipyard re-checks exact-head approval before every enqueue/arm and fails
# closed; gh must resolve to the ghapp wrapper so arms are guarded and ledgered.
# Repos run one after another: they share one steward state lock.
set -uo pipefail
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
case "$(command -v gh)" in "$HOME/.local/bin/gh") ;; *) echo "refusing: gh is not the ghapp wrapper" >&2; exit 2;; esac
log_dir="$HOME/Library/Logs/shipyard-steward-tick"
stamp=$(date -u +%Y%m%dT%H%M%SZ)
# Run the tick under ShipyardLauncher.app once this host activated it
# (tools/shipyard-launcher/README.md). launchd starts this script with
# /bin/bash, so every Shipyard it runs is attributed to bash for macOS privacy
# consent. exec keeps the job's pid and makes the launcher bundle that
# identity; the launcher then runs this same installed file as its child, with
# SHIPYARD_LAUNCHER_APP set so it does not route again. Only the installed copy
# routes (the launcher runs nothing else), and only when the launcher answers
# --launcher-active; otherwise the tick runs directly, as before. Each repo
# line in summary.log says launcher=app or launcher=direct.
# SHIPYARD_STEWARD_LAUNCHER=off turns it off.
launcher_exe="$HOME/.local/libexec/ShipyardLauncher.app/Contents/MacOS/shipyard-launcher"
launcher_record="$HOME/.local/share/shipyard/launcher/activation.json"
tick_installed="$HOME/.local/libexec/shipyard-steward-tick.sh"
launcher_path=direct
if [ -n "${SHIPYARD_LAUNCHER_APP:-}" ]; then
  launcher_path=app
elif [ -n "${SHIPYARD_LAUNCHER_FALLBACK:-}" ]; then
  # The launcher refused to run this file under its identity (its bytes are
  # not the ones fleet-deploy bound) and handed it to plain /bin/bash. Do not
  # route again.
  echo "$stamp launcher=direct reason=$SHIPYARD_LAUNCHER_FALLBACK" >> "$log_dir/summary.log"
elif [ "${SHIPYARD_STEWARD_LAUNCHER:-on}" != off ] && [ -f "$launcher_record" ] && [ -x "$launcher_exe" ] \
  && [ "$(cd "$(dirname "$0")" 2>/dev/null && pwd -P)/$(basename "$0")" \
     = "$(cd "$(dirname "$tick_installed")" 2>/dev/null && pwd -P)/$(basename "$tick_installed")" ]; then
  # The launcher's own verdict: the activation record names this exact bundle
  # and a grant still in place (rechecked without a prompt). Only exit 0 with
  # "active": true routes; anything else runs the tick directly.
  # The check is bounded (SHIPYARD_STEWARD_LAUNCHER_TIMEOUT_S, default 45 s,
  # like Shipyard's own): a hung launcher is killed and counts as not active,
  # so it can never stall the steward.
  launcher_rc=0
  launcher_out=$(/usr/bin/python3 -c '
import subprocess, sys
try:
    run = subprocess.run(sys.argv[2:], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                         stdin=subprocess.DEVNULL, timeout=float(sys.argv[1]))
except subprocess.TimeoutExpired:
    sys.exit(124)
except OSError:
    sys.exit(126)
sys.stdout.buffer.write(run.stdout)
sys.exit(run.returncode)
' "${SHIPYARD_STEWARD_LAUNCHER_TIMEOUT_S:-45}" "$launcher_exe" --launcher-active 2>/dev/null) || launcher_rc=$?
  if [ "$launcher_rc" = 0 ] && [ "$(printf '%s' "$launcher_out" | jq -r '.active' 2>/dev/null)" = true ]; then
    # execfail: if the exec itself fails, keep going directly instead of exiting.
    shopt -s execfail
    exec "$launcher_exe" --launcher-script steward-tick "$@"
    shopt -u execfail
    launcher_rc=exec_failed
  fi
  if [ "$launcher_rc" = 124 ]; then
    launcher_why="timed out after ${SHIPYARD_STEWARD_LAUNCHER_TIMEOUT_S:-45}s"
  else
    launcher_why=$(printf '%s' "$launcher_out" | jq -r '.reason // empty' 2>/dev/null)
  fi
  echo "$stamp launcher=direct reason=not_active rc=$launcher_rc: $(printf '%s' "${launcher_why:-$launcher_out}" | head -c 160 | tr '\n' ' ')" >> "$log_dir/summary.log"
fi
# Config-only sparse clones live on the internal disk: a launchd job that opens
# a path on /Volumes blocks in the kernel waiting on an invisible privacy prompt.
clones="$HOME/.local/share/shipyard-steward"
# Evidence retention: per-tick JSON stays live for SHIPYARD_STEWARD_LIVE_DAYS
# days (default 3), then is bundled by UTC day. Bundled ticks stay for
# SHIPYARD_STEWARD_ARCHIVE_RETENTION_DAYS days (default 180). Archive problems
# are logged to archive.log, never to summary.log, and never fail the tick.
# Deploy merged fleet scripts first (this tick, the deploy script, the reviewer
# pass-runner): copied by hand, they drifted for days with nothing noticing
# (2026-11-12). It acts at most every 10 minutes, installs only what validates,
# logs "fleet-deploy" lines here, and never fails the tick. The new tick takes
# effect on the next run. SHIPYARD_FLEET_DEPLOY=off turns it off.
# It runs detached in its own session: the pass-runner installer's self-tests
# take minutes, and waiting on them ate the tick's admission budget
# (2026-11-13, about 4 minutes). launchd ends the job's process group when
# the tick exits, so the deploy is started with setsid to outlive it; its own
# lock stops two deploys overlapping.
fleet_deploy="$HOME/.local/libexec/shipyard-fleet-deploy.sh"
if [ -x "$fleet_deploy" ]; then
  FLEET_DEPLOY_LOG="$log_dir/summary.log" /usr/bin/python3 -c '
import os, sys
if os.fork():
    sys.exit(0)
os.setsid()
if os.fork():
    os._exit(0)
fd = os.open(os.devnull, os.O_RDWR)
for n in (0, 1, 2):
    os.dup2(fd, n)
os.execv("/bin/bash", ["bash", sys.argv[1]])
' "$fleet_deploy" || true
fi
# The tick previously pinned its own Shipyard build; a hand pin silently missed
# merged steward fixes (2026-11-08: #1165), so it now follows the fleet release.
# Use the fleet-updated Shipyard (tartci tool-freshness keeps ~/.local/bin/shipyard on the
# latest release), not a hand-pinned build; fall back to the old pin only if it is missing.
steward_bin="$HOME/.local/bin/shipyard"
[ -x "$steward_bin" ] || steward_bin="$clones/bin/shipyard"
steward_ver=$("$steward_bin" --version 2>/dev/null | awk '{print $2}')
# The fleet repo list is shared with the standing reviewer brief, so one file
# decides what is admitted and reviewed.
repos=()
while IFS= read -r line; do
  case "$line" in ''|'#'*) continue ;; esac
  repos+=("$line")
done < "$clones/fleet-repos.txt"

# Tick-start invariants. The fleet-level one runs here: every fleet row has a
# config clone with a readable .shipyard/config.toml, and --heal clones a
# missing one in the existing clones' remote form (fleet-ssh was listed with no
# clone and skipped on every tick, 2026-11-12). Each repository's pass then
# reports its own invariants in its JSON (`repos[].invariants`). Only actions
# (heals) are logged line by line; a standing incident is one summary token per
# tick, never repeated detail. SHIPYARD_STEWARD_INVARIANTS=0 turns both off.
invariant_args=()
if [ "${SHIPYARD_STEWARD_INVARIANTS:-1}" = "0" ]; then
  invariant_args=(--no-invariants)
  echo "$stamp fleet invariants=off" >> "$log_dir/summary.log"
elif "$steward_bin" runner steward-invariants --help >/dev/null 2>&1; then
  fleet_out="$log_dir/fleet-invariants-$stamp.json"
  "$steward_bin" runner steward-invariants --json --heal \
    --fleet-repos "$clones/fleet-repos.txt" --clones "$clones" >"$fleet_out" 2>"$fleet_out.err"
  echo "$stamp fleet invariants=$(jq -r '.summary // "ERROR"' "$fleet_out" 2>/dev/null || echo ERROR)" >> "$log_dir/summary.log"
  jq -r --arg stamp "$stamp" '.invariants[]? | .repo as $r | .healed[]? | "\($stamp) \($r) healed: \(.)"' \
    "$fleet_out" >> "$log_dir/summary.log" 2>/dev/null || true
fi

steward_one() {
  local repo=$1 dir="$clones/${1#*/}" out rc waiting
  out="$log_dir/${repo//\//_}-$stamp.json"
  git -C "$dir" pull -q --ff-only >/dev/null 2>&1 \
    || echo "$stamp $repo config refresh failed; using last checkout" >> "$log_dir/summary.log"
  # A pass with an exhausted REST budget reads zero PRs and reports a clean, empty pass
  # (2026-11-09 12:19Z: Vellum showed prs=0 errors=[] while #276 waited). Skip it loudly.
  local rest
  rest=$(GH_REPO="$repo" "$HOME/.local/bin/ghapp" api rate_limit --jq .resources.core.remaining 2>/dev/null || echo "")
  if [ -n "$rest" ] && [ "$rest" -lt 300 ] 2>/dev/null; then
    echo "$stamp $repo skipped=rest_exhausted remaining=$rest" >> "$log_dir/summary.log"
    return 0
  fi
  local t0=$SECONDS
  (cd "$dir" && timeout 840 "$steward_bin" runner steward --repo "$repo" --base main --arm-unqueued --apply --json --pass-budget-secs 600 ${invariant_args[@]+"${invariant_args[@]}"}) >"$out" 2>"$out.err"
  rc=$?
  local took=$((SECONDS - t0))
  # Three outcomes: a completed pass (rc 0, or rc 1 when it only refused some
  # PRs; pr_refused names them, since repos[].errors does not count them; read_deferred names PRs the pass could not read) prints counts; a pass that could not take the steward lock (a gate
  # VM's admission check held it) is a skip; anything else is an ERROR line.
  if [ -s "$out" ]; then
    # A PR error is a transient read failure (HTTP 5xx, a timeout, an unreadable
    # approval) or a policy refusal. The pass may name the kind (`error_kind`:
    # read | policy); an older binary does not, so fall back to the error text.
    # Failing closed on an unread PR is right, but it is not a refusal: those PRs
    # go to read_deferred=, and only real refusals go to pr_refused=.
    counts=$(jq -r '
      def deferred: .error != null and (.error_kind == "read" or (.error_kind == null and ((.error|tostring)|test("HTTP 50[0-9]|timed out|unreadable|cannot read|read failed"))));
      def names(f): [. as $root | $root.repos[]?.prs[]? | select(f) | "#\(.number)"] | join(",") | if . == "" then "0" else . end;
      . as $root | ([.repos[]?.errors[]?]|length) as $e | [.repos[]?.prs[]?|.mutation|tostring] |
      "enqueued=\(map(select(test("enqueued|merged_exact_head")))|length) waiting_approval=\(map(select(.=="waiting_head_approval"))|length) errors=\($e) pr_refused=\($root|names(.error != null and (deferred|not))) read_deferred=\($root|names(deferred))"' "$out" 2>/dev/null || echo "ERROR unreadable-output")
    # rc=1 from a pass whose only per-PR errors were transient reads is not a failed tick.
    if [ "$rc" = 1 ] && [[ "$counts" == *"errors=0 pr_refused=0 read_deferred="* ]] && [[ "$counts" != *"read_deferred=0" ]]; then rc=0; fi
    # ok, FAIL:<id>,<id>, or off when the pass reported none (an older binary,
    # --no-invariants, or a repository it could not observe).
    invariants=$(jq -r '[.repos[]? | select(has("invariants"))] as $r | if ($r|length)==0 then "off" else ([$r[].invariants[] | select(.ok==false and (.skipped|not)) | .id] | if length==0 then "ok" else "FAIL:"+join(",") end) end' "$out" 2>/dev/null || echo ERROR)
    oldest=$(jq -r '[.repos[]?.blocked_summary?.oldest[]?|"#\(.number):\(.blocked_on):\(.age_minutes)m"]|join(",")' "$out" 2>/dev/null || true)
    # Base-level episodes: one required check failing the same test on 3+ open PRs.
    red_everywhere=$(jq -r '[.repos[]?.red_everywhere?.groups[]?]|length' "$out" 2>/dev/null || echo ERROR)
    echo "$stamp $repo rc=$rc $counts took=${took}s ver=$steward_ver launcher=$launcher_path invariants=$invariants red_everywhere=$red_everywhere${oldest:+ oldest=$oldest}" >> "$log_dir/summary.log"
  elif grep -q "another steward apply pass is already running" "$out.err" 2>/dev/null; then
    echo "$stamp $repo skipped=lock_busy" >> "$log_dir/summary.log"
  else
    echo "$stamp $repo ERROR rc=$rc $(head -c 160 "$out.err" 2>/dev/null | tr '\n' ' ')" >> "$log_dir/summary.log"
  fi
}

for repo in "${repos[@]}"; do
  [ -d "$clones/${repo#*/}/.git" ] || { echo "$stamp $repo missing config clone at $clones/${repo#*/}" >> "$log_dir/summary.log"; continue; }
  # Sequential: every repo's pass takes the same steward state lock.
  steward_one "$repo"
done

archive_old_tick_files() {
  local live_days="${SHIPYARD_STEWARD_LIVE_DAYS:-3}"
  local archive_days="${SHIPYARD_STEWARD_ARCHIVE_RETENTION_DAYS:-180}"
  local today cutoff_day archive_dir candidates work file day list archive tmp stage listing base deletions archive_failed=0
  archive_dir="$log_dir/archive"
  candidates=$(mktemp "${TMPDIR:-/tmp}/shipyard-steward-tick-candidates.XXXXXX") || {
    echo "$stamp evidence archive failed: cannot create candidate list" >> "$log_dir/archive.log"
    return 0
  }
  work=$(mktemp -d "${TMPDIR:-/tmp}/shipyard-steward-tick-archive.XXXXXX") || {
    rm -f "$candidates"
    echo "$stamp evidence archive failed: cannot create temporary directory" >> "$log_dir/archive.log"
    return 0
  }
  if ! find "$log_dir" -type f -name '*.json*' ! -path "$archive_dir/*" -mtime +"$live_days" -print >"$candidates"; then
    rm -f "$candidates"
    find "$work" -type f -delete 2>/dev/null || true
    find "$work" -depth -type d -empty -delete 2>/dev/null || true
    echo "$stamp evidence archive failed: cannot list candidates" >> "$log_dir/archive.log"
    return 0
  fi
  today=$(date -u +%Y%m%d)
  # Archive a UTC day only once every file of that day has left the live
  # window, so each day is bundled once instead of being rebuilt every tick.
  cutoff_day=$(date -u -v-"$((live_days + 1))"d +%Y%m%d 2>/dev/null) || cutoff_day=""
  while IFS= read -r file; do
    [ -n "$file" ] || continue
    if ! day=$(date -u -r "$(stat -f %m "$file")" +%Y%m%d 2>/dev/null); then
      archive_failed=1
      echo "$stamp evidence archive failed: cannot determine UTC day for $(basename "$file")" >> "$log_dir/archive.log"
      continue
    fi
    [ "$day" = "$today" ] && continue
    if [ -n "$cutoff_day" ] && [ ! "$day" \< "$cutoff_day" ]; then
      continue
    fi
    list="$work/$day.list"
    printf '%s\n' "$file" >> "$list"
  done < "$candidates"
  rm -f "$candidates"

  if ! mkdir -p "$archive_dir"; then
    find "$work" -type f -delete 2>/dev/null || true
    find "$work" -depth -type d -empty -delete 2>/dev/null || true
    echo "$stamp evidence archive failed: cannot create archive directory" >> "$log_dir/archive.log"
    return 0
  fi
  find "$archive_dir" -type f -name '.ticks-*.tar.gz.tmp.*' -delete 2>/dev/null || true
  deletions="$work/deletions"
  for list in "$work"/*.list; do
    [ -f "$list" ] || continue
    day=${list##*/}
    day=${day%.list}
    archive="$archive_dir/ticks-$day.tar.gz"
    tmp="$archive_dir/.ticks-$day.tar.gz.tmp.$$"
    stage="$work/$day"
    mkdir "$stage" 2>/dev/null || true
    if [ -f "$archive" ] && ! tar -xzf "$archive" -C "$stage"; then
      rm -f "$tmp"
      archive_failed=1
      echo "$stamp evidence archive failed day=$day: cannot extract existing archive" >> "$log_dir/archive.log"
      continue
    fi
    while IFS= read -r file; do
      [ -f "$file" ] || continue
      base=${file##*/}
      if ! cp "$file" "$stage/$base"; then
        rm -f "$tmp"
        archive_failed=1
        echo "$stamp evidence archive failed day=$day: cannot stage $base" >> "$log_dir/archive.log"
        continue 2
      fi
    done < "$list"
    if ! tar -czf "$tmp" -C "$stage" .; then
      rm -f "$tmp"
      archive_failed=1
      echo "$stamp evidence archive failed day=$day: tar failed" >> "$log_dir/archive.log"
      continue
    fi
    listing="$work/$day.listing"
    if ! tar -tzf "$tmp" > "$listing"; then
      rm -f "$tmp"
      archive_failed=1
      echo "$stamp evidence archive failed day=$day: archive verification failed" >> "$log_dir/archive.log"
      continue
    fi
    while IFS= read -r file; do
      [ -f "$file" ] || continue
      base=${file##*/}
      if ! grep -Fqx "./$base" "$listing" && ! grep -Fqx "$base" "$listing"; then
        rm -f "$tmp"
        archive_failed=1
        echo "$stamp evidence archive failed day=$day: missing member $base" >> "$log_dir/archive.log"
        continue 2
      fi
    done < "$list"
    if ! mv "$tmp" "$archive"; then
      rm -f "$tmp"
      archive_failed=1
      echo "$stamp evidence archive failed day=$day: cannot install archive" >> "$log_dir/archive.log"
      continue
    fi
    cat "$list" >> "$deletions"
  done
  # Do not delete any live evidence until every archive in this pass has been
  # installed and verified. A later-day failure must leave earlier-day sources
  # available for the next retry alongside their installed archive.
  if [ "$archive_failed" -eq 0 ] && [ -f "$deletions" ]; then
    while IFS= read -r file; do
      [ -n "$file" ] && rm -f "$file"
    done < "$deletions"
  fi
  find "$archive_dir" -type f -name 'ticks-*.tar.gz' -mtime +"$archive_days" -delete 2>/dev/null || true
  find "$work" -type f -delete 2>/dev/null || true
  find "$work" -depth -type d -empty -delete 2>/dev/null || true
}

archive_old_tick_files

# Drain and fault watch (docs/drain-watch.md): stalled drains, stale HOLDs, stuck
# repair claims and new pass-runner fault signatures, from local files only. It
# announces each finding once as a `drain-watch` line in summary.log and, when a
# finding persists into a second run, files one issue via ghapp. Detached like
# the others; its own lock; exits 0 whatever happens. SHIPYARD_DRAIN_WATCH=off
# turns it off.
drain_watch="${SHIPYARD_DRAIN_WATCH_SCRIPT:-$clones/deploy/Shipyard/tools/metrics/drain_watch.py}"
if [ "${SHIPYARD_DRAIN_WATCH:-on}" != "off" ] && [ -f "$drain_watch" ]; then
  /usr/bin/python3 -c '
import os, sys
if os.fork():
    sys.exit(0)
os.setsid()
if os.fork():
    os._exit(0)
fd = os.open(os.devnull, os.O_RDWR)
for n in (0, 1, 2):
    os.dup2(fd, n)
os.execv("/usr/bin/python3", ["python3", "-I", sys.argv[1], "--tick", "--log-dir", sys.argv[2]])
' "$drain_watch" "$log_dir" || true
fi

# Daily loss report (docs/rollups.md, "Daily loss report"): episodes, agent-free
# resolution and per-day series from the live and archived tick JSON. The script
# itself gates to one run per UTC day, takes its own lock, writes
# loss-report-YYYYMMDD.{json,md} here plus one `loss-report` line in summary.log,
# and exits 0 whatever happens. It runs from the deploy clone fleet-deploy keeps
# at origin/main, detached the same way as fleet-deploy so the tick never waits
# on it. SHIPYARD_LOSS_REPORT=off turns it off.
loss_report="${SHIPYARD_LOSS_REPORT_SCRIPT:-$clones/deploy/Shipyard/tools/metrics/loss_report.py}"
if [ "${SHIPYARD_LOSS_REPORT:-on}" != "off" ] && [ -f "$loss_report" ]; then
  /usr/bin/python3 -c '
import os, sys
if os.fork():
    sys.exit(0)
os.setsid()
if os.fork():
    os._exit(0)
fd = os.open(os.devnull, os.O_RDWR)
for n in (0, 1, 2):
    os.dup2(fd, n)
os.execv("/usr/bin/python3", ["python3", "-I", sys.argv[1], "--daily", "--log-dir", sys.argv[2]])
' "$loss_report" "$log_dir" || true
fi

# Release-readiness canary (`[release.canary]` in a repo's .shipyard/config.toml):
# rehearses the NEXT version on a clean clone of main and opens, updates or
# closes one issue per repo. `--daily` makes the command itself run at most once
# per UTC day per repo (stamp file in the Shipyard state dir), so this every-
# 5-minutes hook is cheap; a repo without the section is a no-op. Only results
# that did something (pass, fail, error) reach summary.log as a `release-canary`
# line. Detached like the others since a rehearsal can take many minutes; the
# command takes its own per-repo lock. SHIPYARD_RELEASE_CANARY=off turns it off.
if [ "${SHIPYARD_RELEASE_CANARY:-on}" != "off" ] && "$steward_bin" release-canary run --help >/dev/null 2>&1; then
  /usr/bin/python3 -c '
import os, sys
if os.fork():
    sys.exit(0)
os.setsid()
if os.fork():
    os._exit(0)
fd = os.open(os.devnull, os.O_RDWR)
for n in (0, 1, 2):
    os.dup2(fd, n)
os.execv("/bin/bash", ["bash", "-c", sys.argv[1], "release-canary"] + sys.argv[2:])
' '
  bin=$1 clones=$2 log=$3 stamp=$4; shift 4
  for repo in "$@"; do
    [ -d "$clones/${repo#*/}/.git" ] || continue
    line=$(cd "$clones/${repo#*/}" && timeout 3600 "$bin" release-canary run --repo "$repo" --daily --apply 2>&1 | tail -n 1)
    case "$line" in
      "release-canary status=skipped"*|"") ;;
      *) echo "$stamp $repo $line" >> "$log/summary.log" ;;
    esac
  done
' "$steward_bin" "$clones" "$log_dir" "$stamp" "${repos[@]}" || true
fi
