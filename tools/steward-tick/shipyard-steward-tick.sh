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
# Config-only sparse clones live on the internal disk: a launchd job that opens
# a path on /Volumes blocks in the kernel waiting on an invisible privacy prompt.
clones="$HOME/.local/share/shipyard-steward"
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
  # PRs; pr_refused names them, since repos[].errors does not count them) prints counts; a pass that could not take the steward lock (a gate
  # VM's admission check held it) is a skip; anything else is an ERROR line.
  if [ -s "$out" ]; then
    counts=$(jq -r '. as $root | ([.repos[]?.errors[]?]|length) as $e | [.repos[]?.prs[]?|.mutation|tostring] | "enqueued=\(map(select(test("enqueued|merged_exact_head")))|length) waiting_approval=\(map(select(.=="waiting_head_approval"))|length) errors=\($e) pr_refused=\([$root.repos[]?.prs[]?|select(.error!=null)|"#\(.number)"]|join(",")|if .=="" then "0" else . end)"' "$out" 2>/dev/null || echo "ERROR unreadable-output")
    # ok, FAIL:<id>,<id>, or off when the pass reported none (an older binary,
    # --no-invariants, or a repository it could not observe).
    invariants=$(jq -r '[.repos[]? | select(has("invariants"))] as $r | if ($r|length)==0 then "off" else ([$r[].invariants[] | select(.ok==false and (.skipped|not)) | .id] | if length==0 then "ok" else "FAIL:"+join(",") end) end' "$out" 2>/dev/null || echo ERROR)
    oldest=$(jq -r '[.repos[]?.blocked_summary?.oldest[]?|"#\(.number):\(.blocked_on):\(.age_minutes)m"]|join(",")' "$out" 2>/dev/null || true)
    echo "$stamp $repo rc=$rc $counts took=${took}s ver=$steward_ver invariants=$invariants${oldest:+ oldest=$oldest}" >> "$log_dir/summary.log"
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
find "$log_dir" -name '*.json*' -mtime +3 -delete
