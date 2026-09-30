#!/usr/bin/env bash
# Read-only Shipyard adoption audit for one repository.
#
# Usage (from a checkout of the repository being audited):
#   skills/ci/scripts/adoption_audit.sh OWNER/REPO [BASE]
#
# Prints one row per feature with two separate verdicts:
#   STATUS  present / partial / absent / n/a / UNKNOWN: is it configured?
#   PROVEN  yes / no / unmeasured: did it have a non-zero effect in the
#           sampled window? "present" alone never means "working".
# plus the evidence behind both and the recommended next feature. Nothing here writes to GitHub,
# the repository, or Shipyard state. Every probe is paired with a control on
# the same instrument; when a control fails the row reads UNKNOWN, never absent.
#
# Requires: git, ghapp (or GH=gh), shipyard, python3.

set -u

REPO="${1:-}"
BASE="${2:-main}"
GH="${GH:-ghapp}"
PR_SAMPLE="${PR_SAMPLE:-2}"
# Merged PRs read for the cheap per-PR proxies (author, auto-merge events).
EFFECT_SAMPLE="${EFFECT_SAMPLE:-10}"
# The GitHub App identity `shipyard pr` opens pull requests as.
APP_LOGIN="${SHIPYARD_APP_LOGIN:-shipyard-local[bot]}"

if [ -z "$REPO" ]; then
  echo "usage: $0 OWNER/REPO [BASE]" >&2
  exit 2
fi

# An error body (404 "Branch not protected") arrives on stdout, so a failed
# read must yield nothing rather than be parsed as data.
api() { local o; if o="$($GH api "$@" 2>/dev/null)"; then printf '%s\n' "$o"; fi; }
# ok when the endpoint answers; used where an empty answer would otherwise be
# read as "nothing configured" (a 401/403/5xx must not become absent).
api_ok() { $GH api "$@" >/dev/null 2>&1; }

rows=()
# row FEATURE STATUS EVIDENCE [PROVEN]
row() { rows+=("$1|$2|$3|${4:-}"); }

# ── Controls: prove each instrument can see this repository at all ─────────
ctl_git=ok
git fetch -q origin "$BASE" 2>/dev/null || true
if ! git rev-parse --verify -q "origin/$BASE" >/dev/null; then
  ctl_git="origin/$BASE not resolvable in $(pwd)"
elif [ "$(git ls-tree --name-only "origin/$BASE" | wc -l | tr -d ' ')" = 0 ]; then
  ctl_git="origin/$BASE has an empty tree"
fi
origin_url="$(git remote get-url origin 2>/dev/null || true)"
case "$origin_url" in
  *"$REPO"|*"$REPO.git") ;;
  *) ctl_git="cwd origin ($origin_url) is not $REPO; run from a checkout of $REPO" ;;
esac

ctl_api=ok
seen="$(api "repos/$REPO" --jq .full_name)"
if [ "$(printf '%s' "$seen" | tr '[:upper:]' '[:lower:]')" != "$(printf '%s' "$REPO" | tr '[:upper:]' '[:lower:]')" ]; then
  ctl_api="$GH api repos/$REPO returned '$seen'"
fi

ctl_cli=ok
shipyard --version >/dev/null 2>&1 || ctl_cli="shipyard not runnable"

cfg=""
if [ "$ctl_git" = ok ]; then
  cfg="$(git show "origin/$BASE:.shipyard/config.toml" 2>/dev/null || true)"
fi
has_cfg() { printf '%s\n' "$cfg" | grep -Eq "$1"; }
tree_has() { git ls-tree -r --name-only "origin/$BASE" | grep -Eq "$1"; }

# ── Sample recent merged PRs once; several rows read these reports ────────
tier_fast=0; tier_seen=0; reuse=0; refuse=0; sampled=""; landing_ok=0; mg_runs=0; outs=""; merged=""
if [ "$ctl_git" = ok ] && [ "$ctl_api" = ok ] && [ "$ctl_cli" = ok ]; then
  merged="$(api "repos/$REPO/pulls?state=closed&base=$BASE&per_page=30" \
      --jq '[.[]|select(.merged_at)][]|"\(.number) \(.user.login)"')"
  prs="$(printf '%s\n' "$merged" | sed '/^$/d' | head -n "$PR_SAMPLE" | cut -d' ' -f1)"
  for n in $prs; do
    out="$(shipyard landing --repo "$REPO" --pr "$n" 2>&1 || true)"
    sampled="$sampled #$n"
    # Control: a landing report that never reached its VALIDATION section
    # measured nothing, so its zero annotations are not evidence of absence.
    printf '%s\n' "$out" | grep -q '^VALIDATION' && landing_ok=$((landing_ok+1))
    tier_fast=$((tier_fast + $(printf '%s\n' "$out" | grep -c ': tier fast')))
    tier_seen=$((tier_seen + $(printf '%s\n' "$out" | grep -cE ': tier (fast|full)')))
    reuse=$((reuse + $(printf '%s\n' "$out" | grep -c 'reused receipt')))
    refuse=$((refuse + $(printf '%s\n' "$out" | grep -c 'receipt refused')))
    mg_runs=$((mg_runs + $(printf '%s\n' "$out" | grep -E '^  merge group ' | grep -vc 'no merge group runs')))
    outs="$outs
$out"
  done
fi
# A required context is "reported" when a sampled head's landing report
# lists a check run for it.
reported() { printf '%s\n' "$outs" | grep -Fq "    $1: "; }

# ── 1. shipyard pr flow ─────────────────────────────────────────────────────
if [ "$ctl_git" != ok ]; then
  row "shipyard pr flow" UNKNOWN "$ctl_git"
else
  s=0; ev=""
  if [ -n "$cfg" ]; then s=$((s+1)); ev=".shipyard/config.toml"; fi
  if has_cfg '^skill_sync_script' || tree_has '(^|/)(tools/scripts|scripts)/skill_sync_check\.py$'; then
    s=$((s+1)); ev="$ev skill_sync_check.py"
  fi
  if has_cfg '^version_bump_script' || tree_has '(^|/)(tools/scripts|scripts)/version_bump_check\.py$'; then
    s=$((s+1)); ev="$ev version_bump_check.py"
  fi
  case $s in 3) st=present ;; 0) st=absent ;; *) st=partial ;; esac
  # Proxy: share of recent merged PRs opened as the App. ghapp uses the same
  # identity, so this proves App-routed submission, not the gates by itself.
  pv=unmeasured
  n_m="$(printf '%s\n' "$merged" | sed '/^$/d' | head -n "$EFFECT_SAMPLE" | wc -l | tr -d ' ')"
  if [ "${n_m:-0}" -gt 0 ]; then
    n_app="$(printf '%s\n' "$merged" | sed '/^$/d' | head -n "$EFFECT_SAMPLE" | grep -cF " $APP_LOGIN")"
    if [ "$n_app" -gt 0 ]; then pv="yes ($n_app/$n_m merged PRs opened as $APP_LOGIN)"
    else pv="no (0/$n_m merged PRs opened as $APP_LOGIN)"; fi
  fi
  row "shipyard pr flow" "$st" "${ev:-no config, no gate scripts}" "$pv"
fi

# ── Live protection: required contexts + effective rules ──────────────────
req=""; rules=""; prot_read=ok; rules_read=ok
if [ "$ctl_api" = ok ]; then
  # An unprotected branch answers 404 "Branch not protected", which is a real
  # absence. Any other failure (401, 403 without admin, 5xx) is unreadable.
  if api_ok "repos/$REPO/branches/$BASE/protection"; then
    req="$(api "repos/$REPO/branches/$BASE/protection" \
        --jq '.required_status_checks.contexts[]')"
  elif ! $GH api "repos/$REPO/branches/$BASE/protection" 2>&1 | grep -q 'Branch not protected'; then
    prot_read="branch protection unreadable"
  fi
  api_ok "repos/$REPO/rules/branches/$BASE" || rules_read="effective branch rules unreadable"
  rules="$(api "repos/$REPO/rules/branches/$BASE" --jq '.[].type')"
  req="$req
$(api "repos/$REPO/rules/branches/$BASE" \
      --jq '.[]|select(.type=="required_status_checks")|.parameters.required_status_checks[].context')"
  req="$(printf '%s\n' "$req" | sed '/^$/d' | sort -u)"
fi
nreq="$(printf '%s\n' "$req" | sed '/^$/d' | wc -l | tr -d ' ')"

# ── 2. required checks (live, and declared to Shipyard) ─────────────────────
if [ "$ctl_api" != ok ]; then
  row "required checks" UNKNOWN "$ctl_api"
elif [ "$prot_read" != ok ] || [ "$rules_read" != ok ]; then
  row "required checks" UNKNOWN "$prot_read / $rules_read"
else
  declared=no; has_cfg '^required_status_checks' && declared=yes
  if [ "$nreq" -gt 0 ] && [ "$declared" = yes ]; then st=present
  elif [ "$nreq" -gt 0 ]; then st=partial
  else st=absent; fi
  k=0
  while IFS= read -r ctx; do [ -n "$ctx" ] && reported "$ctx" && k=$((k+1)); done <<< "$req"
  if [ "$landing_ok" = 0 ]; then pv=unmeasured
  elif [ "$nreq" -gt 0 ] && [ "$k" = "$nreq" ]; then pv="yes ($k/$nreq reported on sampled heads)"
  else pv="no ($k/$nreq reported on sampled heads)"; fi
  row "required checks" "$st" "$nreq live context(s); [governance] declared=$declared" "$pv"
fi

# ── 3. version / skill-sync gates wired as a REQUIRED check ────────────────
if [ "$ctl_git" != ok ]; then
  row "version/skill-sync gates" UNKNOWN "$ctl_git"
else
  wf_ctl="$(git grep -l 'runs-on' "origin/$BASE" -- .github/workflows 2>/dev/null | wc -l | tr -d ' ')"
  gate_wfs="$(git grep -l -E 'skill_sync_check|version_bump_check' "origin/$BASE" -- .github/workflows 2>/dev/null || true)"
  n_gate_wfs="$(printf '%s\n' "$gate_wfs" | sed '/^$/d' | wc -l | tr -d ' ')"
  required_gate=no; gate_ctx=""
  if [ "$n_gate_wfs" -gt 0 ] && [ "$nreq" -gt 0 ]; then
    while IFS= read -r ctx; do
      [ -z "$ctx" ] && continue
      while IFS= read -r wf; do
        [ -z "$wf" ] && continue
        if git show "$wf" 2>/dev/null | grep -Fq "name: $ctx"; then required_gate=yes; gate_ctx="$ctx"; fi
      done <<< "$gate_wfs"
    done <<< "$req"
  fi
  if [ "$wf_ctl" = 0 ]; then st=UNKNOWN
  elif [ "$n_gate_wfs" -gt 0 ] && { [ "$ctl_api" != ok ] || [ "$prot_read" != ok ] || [ "$rules_read" != ok ]; }; then
    st=UNKNOWN; required_gate=unreadable
  elif [ "$required_gate" = yes ]; then st=present
  elif [ "$n_gate_wfs" -gt 0 ]; then st=partial
  else st=absent; fi
  # Running is not blocking: the CI copy is proven only by blocks, which
  # need per-step job reads this audit does not spend.
  if [ "$landing_ok" = 0 ] || [ -z "$gate_ctx" ]; then pv=unmeasured
  elif reported "$gate_ctx"; then pv="unmeasured (runs on heads; blocks not counted)"
  else pv="no ('$gate_ctx' never reported on sampled heads)"; fi
  row "version/skill-sync gates" "$st" "$n_gate_wfs of $wf_ctl workflow(s) run the gate scripts; required=$required_gate" "$pv"
fi

# ── 4. merge queue ─────────────────────────────────────────────────────────
if [ "$ctl_api" != ok ]; then
  row "merge queue" UNKNOWN "$ctl_api"
elif [ "$rules_read" != ok ]; then
  row "merge queue" UNKNOWN "$rules_read"
else
  n_rulesets="$(api "repos/$REPO/rulesets" --jq 'length')"; n_rulesets="${n_rulesets:-?}"
  if printf '%s\n' "$rules" | grep -qx merge_queue; then
    method="$(api "repos/$REPO/rules/branches/$BASE" \
        --jq '.[]|select(.type=="merge_queue")|.parameters.merge_method' 2>/dev/null | head -n1)"
    if [ "$method" = MERGE ]; then st=present; else st=partial; fi
    if [ "$landing_ok" = 0 ]; then pv=unmeasured
    elif [ "$mg_runs" -gt 0 ]; then pv="yes ($mg_runs merge-group run(s) on sampled PRs)"
    else pv="no (0 merge-group runs on sampled PRs)"; fi
    row "merge queue" "$st" "merge_queue rule on $BASE, method=$method" "$pv"
  else
    row "merge queue" absent "no merge_queue rule on $BASE ($n_rulesets ruleset(s) read)"
  fi
fi

# ── 5. auto-merge allowed ──────────────────────────────────────────────────
if [ "$ctl_api" != ok ]; then
  row "auto-merge (MERGE)" UNKNOWN "$ctl_api"
else
  am="$(api "repos/$REPO" --jq '.allow_auto_merge')"
  mc="$(api "repos/$REPO" --jq '.allow_merge_commit')"
  # Both fields are omitted for a caller without push access; null is unreadable.
  if { [ "$am" != true ] && [ "$am" != false ]; } || { [ "$mc" != true ] && [ "$mc" != false ]; }; then st=UNKNOWN
  elif [ "$am" = true ] && [ "$mc" = true ]; then st=present
  elif [ "$am" = true ] || [ "$mc" = true ]; then st=partial
  else st=absent; fi
  # Proxy: merged PRs whose timeline shows AutoMergeEnabledEvent.
  pv=unmeasured; n_ae=0; n_read=0
  owner="${REPO%%/*}"; name="${REPO#*/}"
  for n in $(printf '%s\n' "$merged" | sed '/^$/d' | head -n "$EFFECT_SAMPLE" | cut -d' ' -f1); do
    c="$(api graphql -f query="query{repository(owner:\"$owner\",name:\"$name\"){pullRequest(number:$n){timelineItems(itemTypes:[AUTO_MERGE_ENABLED_EVENT],first:1){totalCount}}}}" \
        --jq '.data.repository.pullRequest.timelineItems.totalCount')"
    case "$c" in ''|*[!0-9]*) continue ;; esac
    n_read=$((n_read+1)); [ "$c" -gt 0 ] && n_ae=$((n_ae+1))
  done
  if [ "$n_read" -gt 0 ]; then
    if [ "$n_ae" -gt 0 ]; then pv="yes ($n_ae/$n_read merged PRs had auto-merge enabled)"
    else pv="no (0/$n_read merged PRs had auto-merge enabled)"; fi
  fi
  row "auto-merge (MERGE)" "$st" "allow_auto_merge=$am allow_merge_commit=$mc" "$pv"
fi

if [ -z "$sampled" ] || [ "$landing_ok" = 0 ]; then
  row "PR-head fast tier" UNKNOWN "no readable landing report (PRs:${sampled:- none})"
  row "protected receipt reuse" UNKNOWN "no readable landing report (PRs:${sampled:- none})"
else
  if [ "$tier_fast" -gt 0 ]; then st=present
  elif [ "$tier_seen" -gt 0 ]; then st=partial
  else st=absent; fi
  if [ "$tier_fast" -gt 0 ]; then pv="yes ($tier_fast fast-tier head check(s))"; else pv=no; fi
  row "PR-head fast tier" "$st" "shipyard-test-tier: $tier_fast fast / $tier_seen annotated (PRs$sampled)" "$pv"
  if [ $((reuse + refuse)) -gt 0 ]; then st=present; else st=absent; fi
  # Reuse is a low-yield effect (a few merge groups in ten), so a small sample
  # with no reuse is not evidence of "no": below the floor it is unmeasured.
  dec=$((reuse + refuse))
  if [ "$reuse" -gt 0 ]; then pv="yes ($reuse of $dec decisions reused)"
  elif [ "$dec" -ge "${RECEIPT_MIN_DECISIONS:-10}" ]; then pv="no (0 of $dec decisions reused)"
  else pv="unmeasured (0 of $dec reused; below ${RECEIPT_MIN_DECISIONS:-10}, use metrics gate-cost)"; fi
  row "protected receipt reuse" "$st" "shipyard-receipt-decision: $reuse reuse / $refuse refuse (PRs$sampled)" "$pv"
fi

# ── 8. host classes + fleet-update (MACHINE scope, not repository) ─────────
if [ "$ctl_cli" != ok ]; then
  row "host classes (this machine)" UNKNOWN "$ctl_cli"
else
  capj="$(shipyard runner capacity --json 2>/dev/null || true)"
  conf="$(printf '%s' "$capj" | python3 -c 'import sys,json
try:
    d=json.load(sys.stdin); h=d.get("hosts",[])
    print("%s %d %d" % (str(d.get("configured")).lower(), len(h), sum(1 for x in h if x.get("readable"))))
except Exception: print("unreadable 0 0")')"
  set -- $conf
  if [ "${3:-0}" -gt 0 ]; then pv="yes ($3 of $2 host(s) readable)"; else pv="no (0 of ${2:-0} readable)"; fi
  case "$conf" in
    true*) row "host classes (this machine)" present "runner capacity: configured, $2 host class(es)" "$pv" ;;
    false*) row "host classes (this machine)" absent "runner capacity: configured=false" ;;
    *) row "host classes (this machine)" UNKNOWN "runner capacity output unreadable" ;;
  esac
fi

# ── 9. self-hosted runner governance ───────────────────────────────────────
if [ "$ctl_api" != ok ]; then
  row "runner governance" UNKNOWN "$ctl_api"
else
  nrun="$(api "repos/$REPO/actions/runners" --jq '.total_count')"; nrun="${nrun:-?}"
  decl=0
  has_cfg '^\[runner\.fleet\.expected_host\.' && decl=$((decl+1))
  has_cfg '^\[landability\]' && decl=$((decl+1))
  if [ "$decl" = 2 ]; then st=present
  elif [ "$decl" = 1 ]; then st=partial
  elif [ "$nrun" = "?" ]; then st=UNKNOWN
  elif [ "$nrun" = 0 ]; then st="n/a"
  else st=absent; fi
  row "runner governance" "$st" "$nrun repo runner(s) (org runners not counted); declared sections=$decl/2"
fi

# ── 10. extras: pin, changed-surface selection, changelog ──────────────────
if [ "$ctl_git" = ok ]; then
  x=""
  tree_has '(^|/)tools/shipyard\.toml$' && x="$x pin"
  has_cfg '^\[targets\.[^]]+\.changed_surface_selection\]' && x="$x changed-surface"
  has_cfg '^\[release\.changelog\]' && x="$x changelog"
  has_cfg '^\[queue\.attribution\]' && x="$x queue-attribution"
  row "extras" info "${x:- none}"
fi

# ── Report ──────────────────────────────────────────────────────────────────
echo "Shipyard adoption audit: $REPO (base $BASE)"
echo "controls: git=$ctl_git api=$ctl_api cli=$ctl_cli"
printf '%-28s %-9s %-40s %s\n' FEATURE STATUS PROVEN EVIDENCE
unproven=""
for r in "${rows[@]}"; do
  IFS='|' read -r f s e p <<< "$r"
  # Nothing configured means nothing to prove.
  case "$s" in present|partial) : "${p:=unmeasured}" ;; *) p=- ;; esac
  case "$s:$p" in present:no*|present:unmeasured) unproven="$unproven; $f" ;; esac
  printf '%-28s %-9s %-40s %s\n' "$f" "$s" "$p" "$e"
done

# Adoption order: each feature depends on the ones above it. The CI copy of
# the version/skill gates and runner governance are left out: neither has a
# demonstrated effect yet (see references/adoption.md), so neither is
# recommended.
next=""
for f in "shipyard pr flow" "required checks" "auto-merge (MERGE)" "merge queue" \
         "PR-head fast tier" "protected receipt reuse"; do
  for r in "${rows[@]}"; do
    IFS='|' read -r rf rs _ <<< "$r"
    if [ "$rf" = "$f" ] && { [ "$rs" = absent ] || [ "$rs" = partial ] || [ "$rs" = UNKNOWN ]; }; then
      next="$f ($rs)"; break 2
    fi
  done
done
echo "recommended next: ${next:-nothing in the core set; baseline with shipyard metrics gate-cost}"
echo "present but not proven: ${unproven#; }"
