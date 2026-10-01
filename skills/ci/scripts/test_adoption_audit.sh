#!/usr/bin/env bash
# Offline tests for adoption_audit.sh.
#
# The audit's contract is that a probe which could not read its surface says
# UNKNOWN, never absent or present. Each case below breaks one instrument
# (API down, a foreign repository, no admin scope, the wrong checkout, a
# landing report that never reached VALIDATION, no shipyard binary) with stub
# `ghapp` / `shipyard` binaries and asserts the rows that depend on it read
# UNKNOWN. A healthy case and a genuinely unprotected branch are the controls:
# they prove the same fixtures can produce present and absent.
#
# Run: skills/ci/scripts/test_adoption_audit.sh   (needs bash, git, jq, python3)

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
AUDIT="$HERE/adoption_audit.sh"
T="$(mktemp -d "${TMPDIR:-/tmp}/adoption-audit-test.XXXXXX")"
trap 'rm -rf "$T"' EXIT
REPO=acme/widget
fails=0; passes=0

command -v jq >/dev/null || { echo "SKIP: jq not installed"; exit 0; }

# ── Fixture repository with every repo-side feature declared ──────────────
mkdir -p "$T/acme" "$T/work"
git init -q --bare "$T/acme/widget.git"
(
  cd "$T/work" && git init -q -b main
  git config user.email t@example.com && git config user.name t
  mkdir -p .shipyard scripts .github/workflows
  cat > .shipyard/config.toml <<'EOF'
[governance]
required_status_checks = ["Enforce version & skill sync", "macos"]
[runner.fleet.expected_host.box]
labels = ["self-hosted"]
[landability]
workflows = [".github/workflows/gate.yml"]
EOF
  : > scripts/skill_sync_check.py
  : > scripts/version_bump_check.py
  cat > .github/workflows/gate.yml <<'EOF'
jobs:
  gate:
    runs-on: ubuntu-latest
    name: Enforce version & skill sync
    steps:
      - run: python3 scripts/skill_sync_check.py && python3 scripts/version_bump_check.py
EOF
  git add -A && git commit -qm fixture
  git remote add origin "$T/acme/widget.git" && git push -q origin main
)
git init -q "$T/other" && git -C "$T/other" remote add origin "$T/acme/other.git"

# ── Stub ghapp: canned JSON per endpoint, failure modes via STUB_GH ────────
mkdir -p "$T/bin" "$T/fx"
cat > "$T/fx/repo.json" <<EOF
{"full_name":"$REPO","allow_auto_merge":true,"allow_merge_commit":true}
EOF
echo '{"required_status_checks":{"contexts":["Enforce version & skill sync","macos"]}}' > "$T/fx/protection.json"
echo '[{"type":"merge_queue","parameters":{"merge_method":"MERGE"}}]' > "$T/fx/rules.json"
echo '[{"id":1}]' > "$T/fx/rulesets.json"
echo '{"total_count":2}' > "$T/fx/runners.json"
echo '[{"number":7,"merged_at":"2026-09-30T00:00:00Z","user":{"login":"shipyard-local[bot]"}}]' > "$T/fx/pulls.json"
echo '{"data":{"repository":{"pullRequest":{"timelineItems":{"totalCount":1}}}}}' > "$T/fx/graphql.json"

cat > "$T/bin/ghapp" <<'EOF'
#!/usr/bin/env bash
# args: api PATH [--jq EXPR]
path="$2"; jq_expr="."
shift 2
while [ $# -gt 0 ]; do
  case "$1" in --jq) jq_expr="$2"; shift 2 ;; *) shift ;; esac
done
fx="$STUB_FX"; mode="${STUB_GH:-healthy}"
deny() { echo "{\"message\":\"$1\",\"status\":\"$2\"}"; echo "gh: $1 (HTTP $2)" >&2; exit 1; }
[ "$mode" = down ] && deny "Bad credentials" 401
case "$path" in
  repos/*/*/branches/*/protection)
    [ "$mode" = noadmin ] && deny "Resource not accessible by integration" 403
    [ "$mode" = unprotected ] && deny "Branch not protected" 404
    f=protection.json ;;
  repos/*/*/rules/branches/*)
    if [ "$mode" = unprotected ]; then echo '[]' | jq -r "$jq_expr"; exit 0; fi
    f=rules.json ;;
  repos/*/*/rulesets) f=rulesets.json ;;
  repos/*/*/actions/runners)
    [ "$mode" = noadmin ] && deny "Resource not accessible by integration" 403
    f=runners.json ;;
  repos/*/*/pulls*) f=pulls.json ;;
  graphql) f=graphql.json ;;
  repos/*/*)
    if [ "$mode" = foreign ]; then echo '{"full_name":"someone/else"}' | jq -r "$jq_expr"; exit 0; fi
    if [ "$mode" = noadmin ]; then jq -r "del(.allow_auto_merge,.allow_merge_commit) | $jq_expr" "$fx/repo.json"; exit 0; fi
    f=repo.json ;;
  *) deny "Not Found" 404 ;;
esac
jq -r "$jq_expr" "$fx/$f"
EOF

cat > "$T/bin/shipyard" <<'EOF'
#!/usr/bin/env bash
case "$1" in
  --version) echo "shipyard 0.0.0" ;;
  landing)
    if [ "${STUB_LANDING:-ok}" = broken ]; then echo "error: GitHub API rate limited"; exit 9; fi
    if [ "${STUB_LANDING:-ok}" = refuse ]; then
      printf 'VALIDATION\n  merge group def (merge commit): full suite ran in this merge group\n    macos: validated in full: receipt refused because no artifact\n'
      exit 0
    fi
    cat <<'OUT'
PR #7 in acme/widget: MERGED
VALIDATION
  PR head abc GREEN on the fast tier, NOT full validation
    Enforce version & skill sync: tier unknown (no shipyard-test-tier annotation)  [check run 1]
    macos: tier fast (selector pr-fast); full suite runs in merge_group  [check run 2]
  merge group def (merge commit): full suite ran in this merge group
    macos: reused receipt from run 5: 10 selected / 10 passed (0 skipped)
    macos: tier full
OUT
    ;;
  runner) echo '{"configured":true,"hosts":[{"class":"a","readable":true}]}' ;;
  *) exit 2 ;;
esac
EOF
chmod +x "$T/bin/ghapp" "$T/bin/shipyard"

# Directory holding only the tools the audit needs, minus shipyard.
mkdir -p "$T/noship"
for tool in git jq python3 bash sed grep sort wc tr head cut cat mktemp dirname env; do
  p="$(command -v "$tool" 2>/dev/null)"; case "$p" in /*) ln -sf "$p" "$T/noship/$tool" ;; esac
done
ln -sf "$T/bin/ghapp" "$T/noship/ghapp"

run() { # run CWD PATHDIR [VAR=VAL...]
  local cwd="$1" pdir="$2"; shift 2
  (cd "$cwd" && env PATH="$pdir:$PATH_BASE" GH="$T/bin/ghapp" STUB_FX="$T/fx" "$@" \
     bash "$AUDIT" "$REPO" main 2>&1)
}
PATH_BASE="$T/noship"

proven_of() { # proven_of OUTPUT FEATURE -> first word of the PROVEN column
  printf '%s\n' "$1" | awk -v f="$2" 'substr($0,1,length(f))==f { rest=substr($0,39); split(rest,a," "); print a[1]; exit }'
}
expect_proven() { # expect_proven CASE OUTPUT FEATURE WANT
  local got; got="$(proven_of "$2" "$3")"
  if [ "$got" = "$4" ]; then passes=$((passes+1))
  else fails=$((fails+1)); echo "FAIL [$1] $3 proven: want $4, got '${got:-<missing>}'"; fi
}
status_of() { # status_of OUTPUT FEATURE
  printf '%s\n' "$1" | awk -v f="$2" 'substr($0,1,length(f))==f { rest=substr($0,29); split(rest,a," "); print a[1]; exit }'
}
expect() { # expect CASE OUTPUT FEATURE WANT
  local got; got="$(status_of "$2" "$3")"
  if [ "$got" = "$4" ]; then passes=$((passes+1))
  else fails=$((fails+1)); echo "FAIL [$1] $3: want $4, got '${got:-<missing>}'"; fi
}

api_rows=("required checks" "version/skill-sync gates" "merge queue" "auto-merge (MERGE)" \
          "PR-head fast tier" "protected receipt reuse" "runner governance")

# Control: healthy fixtures produce present everywhere.
out="$(run "$T/work" "$T/bin")"
for f in "shipyard pr flow" "${api_rows[@]}" "host classes (this machine)"; do expect healthy "$out" "$f" present; done
for f in "shipyard pr flow" "required checks" "merge queue" "auto-merge (MERGE)" \
         "PR-head fast tier" "protected receipt reuse" "host classes (this machine)"; do
  expect_proven healthy "$out" "$f" yes
done
# Running is not blocking: the gate row must never claim proof from presence.
expect_proven healthy "$out" "version/skill-sync gates" unmeasured

# Control: a genuinely unprotected branch is a real absence, not UNKNOWN.
out="$(run "$T/work" "$T/bin" STUB_GH=unprotected)"
expect unprotected "$out" "required checks" absent
expect unprotected "$out" "merge queue" absent

# 401 / no network: every API-fed row is UNKNOWN; git-only rows still answer.
out="$(run "$T/work" "$T/bin" STUB_GH=down)"
for f in "${api_rows[@]}"; do expect api-down "$out" "$f" UNKNOWN; done
expect_proven api-down "$out" "shipyard pr flow" unmeasured
expect api-down "$out" "shipyard pr flow" present

# The API answers for a different repository.
out="$(run "$T/work" "$T/bin" STUB_GH=foreign)"
for f in "${api_rows[@]}"; do expect foreign "$out" "$f" UNKNOWN; done

# Readable repository, no admin scope: protection 403, allow_* fields omitted.
out="$(run "$T/work" "$T/bin" STUB_GH=noadmin)"
expect noadmin "$out" "required checks" UNKNOWN
expect noadmin "$out" "version/skill-sync gates" UNKNOWN
expect noadmin "$out" "auto-merge (MERGE)" UNKNOWN
expect noadmin "$out" "merge queue" present

# Run from a checkout of a different repository.
out="$(run "$T/other" "$T/bin")"
expect wrong-cwd "$out" "shipyard pr flow" UNKNOWN
expect wrong-cwd "$out" "version/skill-sync gates" UNKNOWN

# Landing report never reached VALIDATION (rate limit, auth, crash).
out="$(run "$T/work" "$T/bin" STUB_LANDING=broken)"
expect landing-broken "$out" "PR-head fast tier" UNKNOWN
expect landing-broken "$out" "protected receipt reuse" UNKNOWN

# Two refusals and no reuse: too few decisions to call a low-yield effect absent.
out="$(run "$T/work" "$T/bin" STUB_LANDING=refuse)"
expect refuse-only "$out" "protected receipt reuse" present
expect_proven refuse-only "$out" "protected receipt reuse" unmeasured

# No shipyard binary on PATH.
out="$(run "$T/work" "$T/noship")"
expect no-shipyard "$out" "host classes (this machine)" UNKNOWN
expect no-shipyard "$out" "PR-head fast tier" UNKNOWN

echo "adoption_audit tests: $passes passed, $fails failed"
[ "$fails" = 0 ]
