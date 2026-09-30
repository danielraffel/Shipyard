#!/usr/bin/env bash
# Read-only Shipyard adoption audit for one repository.
#
# Usage (from a checkout of the repository being audited):
#   skills/ci/scripts/adoption_audit.sh OWNER/REPO [BASE]
#
# Prints one row per feature: present / partial / absent / n/a, the evidence
# behind it, and the recommended next feature. Nothing here writes to GitHub,
# the repository, or Shipyard state. Every probe is paired with a control on
# the same instrument; when a control fails the row reads UNKNOWN, never absent.
#
# Requires: git, ghapp (or GH=gh), shipyard, python3.

set -u

REPO="${1:-}"
BASE="${2:-main}"
GH="${GH:-ghapp}"
PR_SAMPLE="${PR_SAMPLE:-2}"

if [ -z "$REPO" ]; then
  echo "usage: $0 OWNER/REPO [BASE]" >&2
  exit 2
fi

# An error body (404 "Branch not protected") arrives on stdout, so a failed
# read must yield nothing rather than be parsed as data.
api() { local o; if o="$($GH api "$@" 2>/dev/null)"; then printf '%s\n' "$o"; fi; }

rows=()
row() { rows+=("$1|$2|$3"); }

# ── Controls: prove each instrument can see this repository at all ─────────
ctl_git=ok
git fetch -q origin "$BASE" 2>/dev/null || true
if ! git rev-parse --verify -q "origin/$BASE" >/dev/null; then
  ctl_git="origin/$BASE not resolvable in $(pwd)"
elif [ "$(git ls-tree --name-only "origin/$BASE" | wc -l | tr -d ' ')" = 0 ]; then
  ctl_git="origin/$BASE has an empty tree"
fi
origin_url="$(git remote get-url origin)"
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
  row "shipyard pr flow" "$st" "${ev:-no config, no gate scripts}"
fi

# ── Live protection: required contexts + effective rules ──────────────────
req=""; rules=""
if [ "$ctl_api" = ok ]; then
  req="$(api "repos/$REPO/branches/$BASE/protection" \
      --jq '.required_status_checks.contexts[]')"
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
else
  declared=no; has_cfg '^required_status_checks' && declared=yes
  if [ "$nreq" -gt 0 ] && [ "$declared" = yes ]; then st=present
  elif [ "$nreq" -gt 0 ]; then st=partial
  else st=absent; fi
  row "required checks" "$st" "$nreq live context(s); [governance] declared=$declared"
fi

# ── 3. version / skill-sync gates wired as a REQUIRED check ────────────────
if [ "$ctl_git" != ok ]; then
  row "version/skill-sync gates" UNKNOWN "$ctl_git"
else
  wf_ctl="$(git grep -l 'runs-on' "origin/$BASE" -- .github/workflows 2>/dev/null | wc -l | tr -d ' ')"
  gate_wfs="$(git grep -l -E 'skill_sync_check|version_bump_check' "origin/$BASE" -- .github/workflows 2>/dev/null || true)"
  n_gate_wfs="$(printf '%s\n' "$gate_wfs" | sed '/^$/d' | wc -l | tr -d ' ')"
  required_gate=no
  if [ "$n_gate_wfs" -gt 0 ] && [ "$nreq" -gt 0 ]; then
    while IFS= read -r ctx; do
      [ -z "$ctx" ] && continue
      while IFS= read -r wf; do
        [ -z "$wf" ] && continue
        if git show "$wf" 2>/dev/null | grep -Fq "name: $ctx"; then required_gate=yes; fi
      done <<< "$gate_wfs"
    done <<< "$req"
  fi
  if [ "$wf_ctl" = 0 ]; then st=UNKNOWN
  elif [ "$required_gate" = yes ]; then st=present
  elif [ "$n_gate_wfs" -gt 0 ]; then st=partial
  else st=absent; fi
  row "version/skill-sync gates" "$st" "$n_gate_wfs of $wf_ctl workflow(s) run the gate scripts; required=$required_gate"
fi

# ── 4. merge queue ─────────────────────────────────────────────────────────
if [ "$ctl_api" != ok ]; then
  row "merge queue" UNKNOWN "$ctl_api"
else
  n_rulesets="$(api "repos/$REPO/rulesets" --jq 'length')"; n_rulesets="${n_rulesets:-?}"
  if printf '%s\n' "$rules" | grep -qx merge_queue; then
    method="$(api "repos/$REPO/rules/branches/$BASE" \
        --jq '.[]|select(.type=="merge_queue")|.parameters.merge_method' 2>/dev/null | head -n1)"
    if [ "$method" = MERGE ]; then st=present; else st=partial; fi
    row "merge queue" "$st" "merge_queue rule on $BASE, method=$method"
  else
    row "merge queue" absent "no merge_queue rule on $BASE ($n_rulesets ruleset(s) read)"
  fi
fi

# ── 5. auto-merge allowed ──────────────────────────────────────────────────
if [ "$ctl_api" != ok ]; then
  row "auto-merge" UNKNOWN "$ctl_api"
else
  am="$(api "repos/$REPO" --jq '.allow_auto_merge')"
  mc="$(api "repos/$REPO" --jq '.allow_merge_commit')"
  if [ "$am" = true ] && [ "$mc" = true ]; then st=present
  elif [ "$am" = true ] || [ "$mc" = true ]; then st=partial
  else st=absent; fi
  row "auto-merge (MERGE)" "$st" "allow_auto_merge=$am allow_merge_commit=$mc"
fi

# ── 6/7. PR-head fast tier + receipt reuse (annotation contract) ───────────
tier_fast=0; tier_seen=0; reuse=0; refuse=0; sampled=""; landing_ok=0
if [ "$ctl_api" = ok ] && [ "$ctl_cli" = ok ]; then
  prs="$(api "repos/$REPO/pulls?state=closed&base=$BASE&per_page=30" \
      --jq '[.[]|select(.merged_at)][].number' 2>/dev/null | head -n "$PR_SAMPLE")"
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
  done
fi
if [ -z "$sampled" ] || [ "$landing_ok" = 0 ]; then
  row "PR-head fast tier" UNKNOWN "no readable landing report (PRs:${sampled:- none})"
  row "protected receipt reuse" UNKNOWN "no readable landing report (PRs:${sampled:- none})"
else
  if [ "$tier_fast" -gt 0 ]; then st=present
  elif [ "$tier_seen" -gt 0 ]; then st=partial
  else st=absent; fi
  row "PR-head fast tier" "$st" "shipyard-test-tier: $tier_fast fast / $tier_seen annotated (PRs$sampled)"
  if [ "$reuse" -gt 0 ]; then st=present
  elif [ "$refuse" -gt 0 ]; then st=partial
  else st=absent; fi
  row "protected receipt reuse" "$st" "shipyard-receipt-decision: $reuse reuse / $refuse refuse (PRs$sampled)"
fi

# ── 8. host classes + fleet-update (MACHINE scope, not repository) ─────────
if [ "$ctl_cli" != ok ]; then
  row "host classes (this machine)" UNKNOWN "$ctl_cli"
else
  capj="$(shipyard runner capacity --json 2>/dev/null || true)"
  conf="$(printf '%s' "$capj" | python3 -c 'import sys,json
try: d=json.load(sys.stdin); print("%s %d" % (str(d.get("configured")).lower(), len(d.get("hosts",[]))))
except Exception: print("unreadable 0")')"
  case "$conf" in
    true*) row "host classes (this machine)" present "runner capacity: configured, ${conf#* } host class(es)" ;;
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
printf '%-28s %-9s %s\n' FEATURE STATUS EVIDENCE
for r in "${rows[@]}"; do
  IFS='|' read -r f s e <<< "$r"
  printf '%-28s %-9s %s\n' "$f" "$s" "$e"
done

# Adoption order: each feature depends on the ones above it.
next=""
for f in "shipyard pr flow" "required checks" "version/skill-sync gates" \
         "auto-merge (MERGE)" "merge queue" "PR-head fast tier" \
         "protected receipt reuse" "runner governance"; do
  for r in "${rows[@]}"; do
    IFS='|' read -r rf rs _ <<< "$r"
    if [ "$rf" = "$f" ] && { [ "$rs" = absent ] || [ "$rs" = partial ] || [ "$rs" = UNKNOWN ]; }; then
      next="$f ($rs)"; break 2
    fi
  done
done
echo "recommended next: ${next:-nothing in the core set; baseline with shipyard metrics gate-cost}"
