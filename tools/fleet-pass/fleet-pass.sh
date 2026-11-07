#!/usr/bin/env bash
# fleet-pass.sh ROLE [--dry-run] | --selftest   (ROLE: reviewer, adversary or repair)
# Run ONE bounded pass of the standing reviewer or repair role, on whichever
# provider currently has quota. Lock per role, idle (no output growth) stall detection,
# hard ceiling, one log line.
set -u
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:$PATH"

ROLE=""; DRY=0; SELFTEST=0
for a in "$@"; do
  case "$a" in --dry-run) DRY=1 ;; --selftest) SELFTEST=1 ;; *) ROLE="$a" ;; esac
done
[ $SELFTEST = 1 ] && ROLE=selftest
case "$ROLE" in
  reviewer) BRIEF=2026-10-30-standing-reviewer-brief.md; SIGNER=per-repo ;;
  # The second, adversarial review Vellum's release-path-review status needs:
  # a different signer from the reviewer, in its own fleet-pass-reviewer-* session.
  adversary) BRIEF=2026-11-04-release-path-adversary-brief.md; SIGNER=infra-review2 ;;
  repair)   BRIEF=2026-10-31-standing-repair-brief.md;   SIGNER=repair-lane ;;
  operator) BRIEF=2026-10-31-standing-repair-brief.md;   SIGNER=drain-operator ;;
  selftest) BRIEF=none; SIGNER=selftest ;;
  *) echo "usage: fleet-pass.sh reviewer|adversary|repair [--dry-run] | --selftest" >&2; exit 2 ;;
esac

IDLE_MINUTES="${IDLE_MINUTES:-10}"          # no output growth this long => stalled
CEILING_MINUTES="${CEILING_MINUTES:-90}"    # absolute max pass length
# Second-granularity overrides (used by --selftest); win over the minute values.
IDLE_S="${FLEET_PASS_IDLE_SECONDS:-$(( IDLE_MINUTES * 60 ))}"
CEILING_S="${FLEET_PASS_CEILING_SECONDS:-$(( CEILING_MINUTES * 60 ))}"
POLL_S="${FLEET_PASS_POLL_SECONDS:-5}"
MIN_WEEKLY="${FLEET_PASS_MIN_WEEKLY_PCT:-15}"      # refuse accounts below this
# Release-path changes get the strongest model.
DEFAULT_CLAUDE_MODEL=sonnet; [ "$ROLE" = adversary ] && DEFAULT_CLAUDE_MODEL=opus
CLAUDE_MODEL="${FLEET_PASS_CLAUDE_MODEL:-$DEFAULT_CLAUDE_MODEL}"
CODEX_MODEL="${FLEET_PASS_CODEX_MODEL:-}"          # empty = codex default
BRIEF_DIR="${FLEET_PASS_BRIEF_DIR:-}"              # local copy override
# Tools the claude pass may use without a prompt (no blanket bypass).
ALLOWED="${FLEET_PASS_ALLOWED_TOOLS:-Read Grep Glob Bash(ghapp:*) Bash(git:*) Bash(shipyard:*) Bash(cat:*) Bash(ls:*) Bash(mktemp:*) Bash(grep:*) Bash(sed:*) Bash(head:*) Bash(tail:*) Bash(wc:*) Bash(jq:*) Bash(env:*) Bash(SHIPYARD_AGENT_ID=*)}"
ADD_DIR="${FLEET_PASS_ADD_DIR:-/Volumes/Workshop/Code}"
WORKTREES_DIR="${FLEET_PASS_WORKTREES_DIR:-$ADD_DIR/agent-worktrees}"
STATE="${FLEET_PASS_STATE:-$HOME/.local/share/shipyard-steward/pass-runner}"
LOGDIR="${FLEET_PASS_LOGDIR:-$HOME/Library/Logs/fleet-pass}"; LOG="$LOGDIR/$ROLE.log"
LOCK="$STATE/$ROLE.lock"
mkdir -p "$LOGDIR" "$STATE"
now() { date -u +%Y-%m-%dT%H:%M:%SZ; }
ENDED=""
fmt_took() { local t=$1; if [ "$t" -ge 60 ]; then echo "$(( t / 60 ))m$(( t % 60 ))s"; else echo "${t}s"; fi; }
logline() { echo "$(now) role=$ROLE provider=$1 model=$2 exit=$3 duration=${4}s ended=${ENDED:-none} took=$(fmt_took "$4") ${5:-}" >> "$LOG"; }

# Recursively signal a process and its descendants.
kill_tree() {
  local sig=$1 pid=$2 c
  for c in $(ps -A -o pid= -o ppid= | awk -v p="$pid" '$2==p{print $1}'); do kill_tree "$sig" "$c"; done
  kill "-$sig" "$pid" 2>/dev/null
}
fmt_idle() { if [ "$IDLE_S" -ge 60 ]; then echo "$(( IDLE_S / 60 ))m"; else echo "${IDLE_S}s"; fi; }

# run_watched OUTFILE cmd...  -> sets RC, ENDED (ok|stalled|ceiling), DUR, NOTE.
# Stalled = OUTFILE size has not grown for IDLE_S. Ceiling = CEILING_S total.
run_watched() {
  local out=$1; shift
  local start last_growth size last_size t
  start=$(date +%s); last_growth=$start; last_size=-1
  : >"$out"
  "$@" >"$out" 2>&1 </dev/null &
  CPID=$!
  ENDED=ok; NOTE=""
  while kill -0 "$CPID" 2>/dev/null; do
    sleep "$POLL_S"
    t=$(date +%s)
    size=$(stat -f %z "$out" 2>/dev/null || stat -c %s "$out" 2>/dev/null || echo 0)
    if [ "$size" != "$last_size" ]; then last_size=$size; last_growth=$t; fi
    if [ $(( t - start )) -ge "$CEILING_S" ]; then
      ENDED=ceiling; NOTE="INCIDENT ceiling reached"
    elif [ $(( t - last_growth )) -ge "$IDLE_S" ]; then
      ENDED=stalled; NOTE="idle=$(fmt_idle)"
    fi
    if [ "$ENDED" != ok ]; then
      kill_tree TERM "$CPID"; sleep 2; kill_tree KILL "$CPID"
      break
    fi
  done
  wait "$CPID" 2>/dev/null; RC=$?
  DUR=$(( $(date +%s) - start ))
  CPID=""
}
CPID=""
cleanup() { [ -n "${CPID:-}" ] && kill_tree KILL "$CPID"; rm -rf "$LOCK"; }

# --- lock (mkdir is atomic; stale if owner pid dead) ---
take_lock() {
  if ! mkdir "$LOCK" 2>/dev/null; then
    local old; old="$(cat "$LOCK/pid" 2>/dev/null)"
    if [ -n "$old" ] && kill -0 "$old" 2>/dev/null; then
      ENDED=skipped
      logline "${PROVIDER:-none}" "${MODEL:-none}" 0 0 "skipped: previous pass pid $old still running"; exit 0
    fi
    rm -rf "$LOCK"; mkdir "$LOCK" || exit 5
  fi
  echo $$ > "$LOCK/pid"
  trap cleanup EXIT
  trap 'exit 143' INT TERM HUP
}

# finish_pass PROVIDER MODEL: log the summary (+ INCIDENT line) for run_watched.
finish_pass() {
  logline "$1" "$2" "$RC" "$DUR" "$NOTE"
  [ "$ENDED" = ceiling ] && echo "$(now) role=$ROLE INCIDENT ceiling reached after ${DUR}s (CEILING_S=${CEILING_S})" >> "$LOG"
}

if [ $SELFTEST = 1 ]; then
  T="$(mktemp -d "${TMPDIR:-/tmp}/fleet-pass-selftest.XXXXXX")" || exit 5
  STATE="$T/state"; LOGDIR="$T/logs"; LOG="$LOGDIR/selftest.log"; LOCK="$STATE/selftest.lock"
  mkdir -p "$STATE" "$LOGDIR"
  POLL_S=1; FAILS=0
  check() { if [ "$2" = "$3" ]; then echo "PASS $1 (got $2)"; else echo "FAIL $1: expected $3 got $2"; FAILS=$((FAILS+1)); fi; }
  take_lock
  IDLE_S=3; CEILING_S=60
  run_watched "$T/o1" bash -c 'echo hello; sleep 120'; finish_pass fake fake
  check stall-detected "$ENDED" stalled; check stall-fast "$([ $DUR -lt 15 ] && echo y)" y
  IDLE_S=3; CEILING_S=6
  run_watched "$T/o2" bash -c 'while :; do echo tick; sleep 0.5; done'; finish_pass fake fake
  check ceiling-detected "$ENDED" ceiling
  if grep -q 'INCIDENT ceiling reached' "$LOG"; then echo "PASS incident-line"; else echo "FAIL incident-line"; FAILS=$((FAILS+1)); fi
  IDLE_S=3; CEILING_S=60
  run_watched "$T/o3" bash -c 'echo done'; finish_pass fake fake
  check normal-ok "$ENDED" ok
  echo "--- log"; cat "$LOG"
  cleanup; trap - EXIT
  if [ -d "$LOCK" ]; then echo "FAIL lock not released"; FAILS=$((FAILS+1)); else echo "PASS lock-released"; fi
  rm -rf "$T"
  if [ $FAILS = 0 ]; then echo "SELFTEST OK"; exit 0; else echo "SELFTEST FAILED ($FAILS)"; exit 1; fi
fi

# --- pick provider from `sr status -json` ---
PICK="$(sr status -json 2>/dev/null | python3 -c '
import json,sys
mn=float(sys.argv[1])
try: d=json.load(sys.stdin)["providers"]
except Exception as e: print("ERR sr status unreadable: %s"%e); sys.exit(0)
def ok(a):
    st=str(a.get("state") or "").lower()
    if any(k in st for k in ("cooked","throttled","error")): return False
    w=a.get("weekly_left_pct"); s=a.get("session_left_pct")
    if w is not None and w<mn: return False
    if s is not None and s<5: return False
    return True
out=[]
for p in ("claude","codex"):
    acc=[a for a in d.get(p,{}).get("accounts",[]) if ok(a)]
    acc.sort(key=lambda a:-(a.get("weekly_left_pct") or 0))
    out.append("%s %d %s"%(p,len(acc),acc[0]["label"] if acc else "-"))
print("OK "+" | ".join(out))
' "$MIN_WEEKLY")"
case "$PICK" in
  ERR*|"") echo "refusing: ${PICK:-sr status produced no output}" >&2; [ $DRY = 1 ] || logline none none 3 0 "refused: sr status unreadable"; exit 3 ;;
esac
CL_N="$(sed -E 's/.*claude ([0-9]+) .*/\1/' <<<"$PICK")"
CL_ACC="$(sed -E 's/.*claude [0-9]+ ([^ ]+) .*/\1/' <<<"$PICK")"
CX_N="$(sed -E 's/.*codex ([0-9]+) .*/\1/' <<<"$PICK")"
CX_ACC="$(sed -E 's/.*codex [0-9]+ ([^ ]+).*/\1/' <<<"$PICK")"
# After a Claude failover, go straight to Codex for six hours: sr status can list an
# account as active while it answers 401.
CL_RECENT_FAIL="$STATE/claude-failed-at"
if [ "$CX_N" -gt 0 ] && [ -f "$CL_RECENT_FAIL" ] && [ -n "$(find "$CL_RECENT_FAIL" -mmin -360 2>/dev/null)" ]; then
  PROVIDER=codex; MODEL="${CODEX_MODEL:-default}"; ACC="$CX_ACC"
elif [ "$CL_N" -gt 0 ]; then PROVIDER=claude; MODEL="$CLAUDE_MODEL"; ACC="$CL_ACC"
elif [ "$CX_N" -gt 0 ]; then PROVIDER=codex; MODEL="${CODEX_MODEL:-default}"; ACC="$CX_ACC"
else
  echo "refusing: no Claude or Codex account with >=${MIN_WEEKLY}% weekly quota and non-cooked state (sr status: $PICK)" >&2
  [ $DRY = 1 ] || logline none none 3 0 "refused: no provider has quota"
  exit 3
fi

# --- brief ---
BRIEF_TXT=""; BRIEF_SRC=""
if [ -n "$BRIEF_DIR" ] && [ -f "$BRIEF_DIR/$BRIEF" ]; then
  BRIEF_TXT="$(cat "$BRIEF_DIR/$BRIEF")"; BRIEF_SRC="$BRIEF_DIR/$BRIEF"
elif [ $DRY = 0 ]; then
  BRIEF_TXT="$(ghapp api -H 'Accept: application/vnd.github.raw' "repos/danielraffel/pulp-planning/contents/architecture/$BRIEF?ref=main" 2>/dev/null)"
  BRIEF_SRC="pulp-planning main architecture/$BRIEF"
  if [ -n "$BRIEF_TXT" ]; then printf '%s\n' "$BRIEF_TXT" > "$STATE/$BRIEF.cache"
  elif [ -f "$STATE/$BRIEF.cache" ]; then BRIEF_TXT="$(cat "$STATE/$BRIEF.cache")"; BRIEF_SRC="cache $STATE/$BRIEF.cache"; fi
else
  BRIEF_SRC="pulp-planning main architecture/$BRIEF (fetched via ghapp at run time)"
fi
if [ $DRY = 0 ] && [ -z "$BRIEF_TXT" ]; then
  echo "refusing: could not load brief $BRIEF" >&2; logline "$PROVIDER" "$MODEL" 4 0 "refused: no brief"; exit 4
fi

PROMPT="You are the standing $ROLE for the fleet drain. This is a single non-interactive pass run by a scheduler. Do exactly one pass of the brief below, then stop. Do not loop, do not wait, do not use /loop, and do not start a second pass. Your signer for SHIPYARD_AGENT_ID is $SIGNER. For the reviewer role the signer is per repository: use SHIPYARD_AGENT_ID=pulp-review for Generous-Corp/pulp and Generous-Corp/vellum, and SHIPYARD_AGENT_ID=infra-review2 for danielraffel/Shipyard and danielraffel/tartci. You are the only reviewer: never post on a head that already has a counted verdict, and never APPROVE a head that carries a HOLD. A HOLD also covers commits carried in from other PRs. Before approving PR P, check whether P carries a commit that ANOTHER open PR Q introduced: Q introduced commit C only if C is in Q's own diff against Q's own base branch (git log origin/<Q base>..<Q head>), not merely reachable from Q. If such a C is in P's diff against main, Q is not stacked on P (directly or through a chain of base branches), and Q's current head has a HOLD or no APPROVE, then HOLD P and name C and Q. Commits that P itself introduced and that PRs stacked on P inherit are never a reason to HOLD P. Never arm, enqueue, merge or close anything. If there is nothing to do, say so and stop.

=== BRIEF ===
$BRIEF_TXT"
if [ "$ROLE" = adversary ]; then
  PROMPT="You are the adversarial reviewer for Vellum release-path pull requests. This is a single non-interactive pass run by a scheduler. Do exactly one pass of the brief below, then stop. Do not loop, do not wait, do not use /loop, and do not start a second pass. Your signer is infra-review2: run every ghapp verdict command with SHIPYARD_AGENT_ID=infra-review2 and end each verdict with the line 'reviewer: infra-review2' before the verdict line. Each queued head already carries one counted APPROVE from another reviewer; yours is the second, independent review the required release-path-review status needs. Do not trust that APPROVE or its reasoning: try to break the change. Post exactly one APPROVE or HOLD per queued head, at that exact head. Never APPROVE a head you or your session authored or pushed, a head with a HOLD, or a head whose diff you could not read in full; HOLD instead and say why. You review only: never edit, commit, push, arm, enqueue, merge or close anything. If there is nothing to do, say so and stop.

=== BRIEF ===
$BRIEF_TXT"
fi

# Reviewer: the candidate set is computed here, not chosen by the model, so a
# repaired head cannot be skipped. Open, non-draft, unlabelled PRs whose required
# checks are green and which have no verdict at their head; last pass's misses first.
MUST_FILE="$STATE/$ROLE.must-review"; MISSED_FILE="$STATE/$ROLE.missed"
MUST_ARGS=(); [ "$ROLE" = repair ] && MUST_ARGS=(--repair); [ "$ROLE" = adversary ] && MUST_ARGS=(--adversary)
if { [ "$ROLE" = reviewer ] || [ "$ROLE" = adversary ] || [ "$ROLE" = repair ]; } && [ $DRY = 0 ]; then
  # Take the lock before the (API-heavy) query so a short schedule never stacks passes.
  take_lock
  MUST_OK=1
  python3 "$STATE/must-review.py" ${MUST_ARGS[@]+"${MUST_ARGS[@]}"} --missed "$MISSED_FILE" > "$MUST_FILE.tmp" 2>>"$LOG" || MUST_OK=0
  mv "$MUST_FILE.tmp" "$MUST_FILE"
  if [ -s "$MUST_FILE" ] && [ "$ROLE" = reviewer ]; then
    PROMPT="$PROMPT

=== MUST-REVIEW QUEUE (computed by the scheduler; review every entry, in this order, before anything else) ===
Each line is <repo>#<pr> <head sha> [DISPUTED]. Each has green required checks and no verdict at that head. Post exactly one APPROVE or HOLD verdict for each, at that exact head. Skip an entry only if its head has changed since this list was made, and say so.
AUTHOR-SELF-APPROVED means the approvals already on that head were posted by the PR's own author lane (the authoring session or a child it spawned), so they do not count. Review it independently from the code, ignore those approvals, and post your own verdict at that head: this is the one exception to 'never post on a head that already has a verdict'.
DISPUTED means the author answered the previous HOLD with a 'dispute:' comment. Read the HOLD and the dispute first, re-derive the disputed point from the code and the rules (not from the earlier verdict), and use high effort. If the dispute shows the earlier HOLD was mistaken, do not repeat that point; say in your verdict that the dispute was upheld. If the dispute is wrong, explain why concretely.
$(cat "$MUST_FILE")"
  elif [ -s "$MUST_FILE" ] && [ "$ROLE" = adversary ]; then
    PROMPT="$PROMPT

=== ADVERSARIAL QUEUE (computed by the scheduler; review every entry, in this order) ===
Each line is <repo>#<pr> <head sha> RELEASE-PATH approved-by:<signer>. Each changes a release-path file, has green required checks, one counted APPROVE at that head from the named signer, and no HOLD. Skip an entry only if its head has changed since this list was made, and say so.
$(cat "$MUST_FILE")"
  elif [ -s "$MUST_FILE" ]; then
    PROMPT="$PROMPT

=== RESCUE QUEUE (computed by the scheduler; nothing has pushed to these for 45+ minutes) ===
Each line is <repo>#<pr> <head sha> <reason>. Take the FIRST entry you can make progress on, claim it, and repair it per the brief, regardless of which agent authored it: an idle author session does not own the PR. Reasons:
- HOLD: fix what the HOLD at that head names. If you judge the HOLD itself mistaken, still push one real improvement or a merge of the PR's base, and post 'dispute: <concrete reason>' after your repaired comment so the reviewer re-judges it.
- FAILED:<checks>: fix the failure if this PR caused it. If it is infrastructure, re-run the failed job once and say so.
- CONFLICT: merge the PR's own base branch.
- STACK-BASE-MERGED: the PR's base branch no longer has an open PR. Retarget it with ghapp api -X PATCH repos/<repo>/pulls/<n> -f base=main, merge origin/main, push once.
$(cat "$MUST_FILE")"
  elif [ $MUST_OK = 1 ]; then
    # Nothing to do: do not start a model at all.
    ENDED=idle; logline none none 0 0 "skipped: $ROLE queue empty"; exit 0
  fi
fi

# Operator: unstick what the reviewer, repair lane and steward left stuck.
if [ "$ROLE" = operator ] && [ $DRY = 0 ]; then
  take_lock
  OP_OK=1; FIND="$STATE/operator.findings"
  python3 "$STATE/operator-findings.py" > "$FIND.tmp" 2>>"$LOG" || OP_OK=0
  mv "$FIND.tmp" "$FIND"
  if [ ! -s "$FIND" ] && [ $OP_OK = 1 ]; then
    ENDED=idle; logline none none 0 0 "skipped: nothing stuck"; exit 0
  fi
  PROMPT="You are the standing drain OPERATOR on m3. The reviewer, repair lane and steward run on their own schedules; you fix what they left stuck. This is one bounded pass. Signer: drain-operator. Use ghapp for every GitHub read and write, and work only in fresh worktrees under $WORKTREES_DIR.

Each finding below names a stuck PR and the steward's own blocked_on reason. For each one, in order, diagnose the real cause from live state (steward JSON in ~/Library/Logs/shipyard-steward-tick/, check-run logs, PR comments) and take the smallest allowed action that unsticks it:
- A failed check caused by infrastructure (cancelled run, bootstrap timeout, runner wedge): re-run that run once with ghapp api -X POST .../actions/runs/<id>/rerun-failed-jobs.
- A PR whose stacked base PR has merged: retarget it with ghapp api -X PATCH repos/<repo>/pulls/<n> -f base=main, merge origin/main, and push once.
- A HOLD or a code failure: repair it per the brief (claim, fix with a focused test, one push, a repaired comment).
- An uncertain merge-queue mutation: reconcile it against GitHub state, then shipyard merge-queue resolve <id> --outcome accepted|rejected.
- A steward or guard bug that blocks admission (it blocks every PR, or blocked_on names a guard that misfires): file or update ONE Shipyard issue with the evidence and the exact failing rule. Do not work around it.
- A wait that only needs time (CI running, queue checks): leave it and say so.
Never approve, post verdicts, arm, enqueue, merge, close, remove labels, write or cite an owner decision, or skip pre-push gates with PULP_SKIP_PREPUSH. If something needs Daniel, say so in one line and move on. End with one line per finding: what you did, or why nothing was safe.

=== FINDINGS ===
$(cat "$FIND")

=== REPAIR BRIEF (for repairs) ===
$BRIEF_TXT"
fi

# --- command ---
if [ "$ROLE" = repair ] || [ "$ROLE" = operator ]; then
  ALLOWED="$ALLOWED Edit Write Bash(python3:*) Bash(tools/*) Bash(cmake:*) Bash(ctest:*) Bash(pulp:*) Bash(cargo:*) Bash(PULP_SKIP_DIFF_COVER=*) Bash(cd:*) Bash(mkdir:*)"
fi
if [ "$PROVIDER" = claude ]; then
  CMD=(sr claude proxy -p "$PROMPT" --model "$MODEL" --output-format stream-json --verbose --allowedTools "$ALLOWED" --add-dir "$ADD_DIR")
else
  CMD=(sr codex exec --skip-git-repo-check --sandbox workspace-write -c sandbox_workspace_write.network_access=true -c model_reasoning_effort=high -C "$HOME" --add-dir "$ADD_DIR")
  [ -n "$CODEX_MODEL" ] && CMD+=(-m "$CODEX_MODEL")
  CMD+=("$PROMPT")
fi

if [ $DRY = 1 ]; then
  echo "role=$ROLE signer=$SIGNER provider=$PROVIDER model=$MODEL account=$ACC"
  echo "sr status: $PICK"
  echo "brief: $BRIEF_SRC"
  echo "env: SHIPYARD_AGENT_ID=$SIGNER idle=${IDLE_S}s ceiling=${CEILING_S}s lock=$LOCK log=$LOG"
  printf 'command:'
  for a in "${CMD[@]}"; do
    if [ "$a" = "$PROMPT" ]; then printf ' "<prompt: %d chars, brief + one-pass instruction>"' "${#PROMPT}"; else printf ' %q' "$a"; fi
  done; echo
  exit 0
fi

[ -f "$LOCK/pid" ] && [ "$(cat "$LOCK/pid")" = "$$" ] || take_lock

# --- run with idle-stall detection and a hard ceiling ---
export SHIPYARD_AGENT_ID="$SIGNER"
# ghapp's verdict stamp attributes a session from CLAUDE_CODE_SESSION_ID or
# CODEX_COMPANION_SESSION_ID, and the steward excludes "unattributed" verdicts. codex
# exec under launchd sets neither, so give every scheduled pass its own run id.
# The adversary's session is fleet-pass-reviewer-adversarial-*, so the base's
# reviewer_stamp_sessions glob (fleet-pass-reviewer-*) counts it, and it is
# always a different session from the reviewer's.
STAMP_ROLE=$ROLE; [ "$ROLE" = adversary ] && STAMP_ROLE=reviewer-adversarial
export CODEX_COMPANION_SESSION_ID="${CODEX_COMPANION_SESSION_ID:-fleet-pass-$STAMP_ROLE-$(hostname -s)-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
# A dedicated stamp id that Claude Code cannot override inside its tool subprocesses.
export SHIPYARD_STAMP_SESSION_ID="$CODEX_COMPANION_SESSION_ID"
cd "$HOME" || exit 5
OUT="$LOGDIR/$ROLE.last-output.txt"
run_watched "$OUT" "${CMD[@]}"
# A Claude account that is cooked, throttled or unauthorised fails fast; run the
# same pass once on Codex instead of losing the cycle.
if [ "$RC" != 0 ] && [ "$PROVIDER" = claude ] && [ "${CX_N:-0}" -gt 0 ] \
   && grep -qiE '401|unauthori[sz]ed|Failed to authenticate|rate.?limit|quota|overloaded|cooked|429|503' "$OUT"; then
  NOTE="failover: claude failed ($(grep -oiE '401|unauthori[sz]ed|rate.?limit|quota|overloaded|429|503' "$OUT" | head -1)); retrying on codex"
  finish_pass "$PROVIDER" "$MODEL"
  PROVIDER=codex; MODEL="${CODEX_MODEL:-default}"; ACC="$CX_ACC"
  CMD=(sr codex exec --skip-git-repo-check --sandbox workspace-write -c sandbox_workspace_write.network_access=true -c model_reasoning_effort=high -C "$HOME" --add-dir "$ADD_DIR")
  [ -n "$CODEX_MODEL" ] && CMD+=(-m "$CODEX_MODEL")
  CMD+=("$PROMPT")
  cp "$OUT" "$OUT.claude-failed" 2>/dev/null
  touch "$STATE/claude-failed-at"
  run_watched "$OUT" "${CMD[@]}"
fi
finish_pass "$PROVIDER" "$MODEL"
# Any listed PR still without a verdict at the same head is a miss: log it loudly
# and put it first next pass.
if { [ "$ROLE" = reviewer ] || [ "$ROLE" = adversary ]; } && [ -s "${MUST_FILE:-}" ]; then
  python3 "$STATE/must-review.py" ${MUST_ARGS[@]+"${MUST_ARGS[@]}"} > "$MUST_FILE.after" 2>>"$LOG"
  grep -Fxf "$MUST_FILE.after" "$MUST_FILE" > "$MISSED_FILE.tmp"; mv "$MISSED_FILE.tmp" "$MISSED_FILE"
  while read -r pr head _; do
    echo "$(now) role=$ROLE MISSED $pr head=$head reason=no verdict after pass; first next pass" >> "$LOG"
  done < "$MISSED_FILE"
  rm -f "$MUST_FILE.after"
fi
exit $RC
