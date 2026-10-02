#!/usr/bin/env bash
# Refuse to merge a PR whose validating check has not actually gone green.
#
# WHY THIS EXISTS
# GitHub grants rulesets, branch protection, and required status checks free
# only on PUBLIC repos. On a PRIVATE repo under a free org plan there is no way
# to make a check blocking — so `gh pr merge --auto` does NOT mean "merge when
# green". With nothing required to wait for, it fires the moment the PR is
# mergeable, usually within seconds.
#
# Generous-Corp/forge is exactly that shape. On 2026-07-27 an armed --auto
# merged Forge #45 about 90 seconds into its macOS run; that run then FAILED,
# and the post-merge run that would have caught it was cancelled as superseded.
# The change landed with nothing having validated it.
#
# Shipyard already solves this properly for repos it manages — `shipyard pr`
# validates locally and merges only on its own green. What was missing was
# anything stopping the bypass. This is that.
#
# Usage:  merge-guard <args...>        # the argv of a `gh pr merge` invocation
# Exit 0 = allow the merge, 1 = refuse (caller must not proceed).
#
# Guarded repos are declared in $MERGE_GUARD_CONFIG as {"owner/repo": ["check"]}.
# A repo absent from that file is not guarded and passes straight through, so
# this is inert for Pulp (which has real server-side required checks).
set -uo pipefail

CONFIG="${MERGE_GUARD_CONFIG:-$HOME/.config/shipyard/merge-guard.json}"
GH_REAL="${MERGE_GUARD_GH:-/opt/homebrew/bin/gh}"

command -v python3 >/dev/null 2>&1 || exit 0
[ -f "$CONFIG" ] || exit 0

repo=""; pr=""; auto=0; prev=""
for arg in "$@"; do
    case "$prev" in --repo|-R) repo="$arg" ;; esac
    case "$arg" in
        --auto) auto=1 ;;
        *) [ -z "$pr" ] && [[ "$arg" =~ ^[0-9]+$ ]] && pr="$arg" ;;
    esac
    prev="$arg"
done

# No --repo: resolve the checkout's origin the same way gh would.
if [ -z "$repo" ]; then
    url="$(git remote get-url origin 2>/dev/null || true)"
    repo="$(printf '%s' "$url" | sed -E 's#^git@github\.com:##; s#^https://github\.com/##; s#\.git$##')"
fi
[ -n "$repo" ] || exit 0

checks="$(python3 -c '
import json, sys
try:
    cfg = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(0)
v = cfg.get(sys.argv[2])
if isinstance(v, str):
    v = [v]
print("\n".join(v or []))
' "$CONFIG" "$repo" 2>/dev/null)"
[ -n "$checks" ] || exit 0   # repo is not guarded

if [ "${GHAPP_ALLOW_UNVALIDATED_MERGE:-0}" = "1" ]; then
    {
        echo "merge-guard: ⚠︎ OVERRIDDEN for $repo (GHAPP_ALLOW_UNVALIDATED_MERGE=1)."
        echo "merge-guard:   proceeding WITHOUT confirming: $(echo $checks | tr '\n' ' ')"
    } >&2
    exit 0
fi

if [ "$auto" = "1" ]; then
    cat >&2 <<EOF
merge-guard: refusing \`--auto\` on $repo.

  $repo has no required status checks — GitHub grants those free only on PUBLIC
  repos, and this one is private on a free org plan. So --auto does not wait for
  green; it merges as soon as the PR is mergeable, typically before CI finishes.

  Use instead:
    shipyard pr                                  # validates locally, merges on its own green
  or wait for the check and merge explicitly:
    ghapp pr checks $pr --repo $repo
    ghapp pr merge $pr --repo $repo --squash
EOF
    exit 1
fi

[ -n "$pr" ] || exit 0

failed=0
while IFS= read -r name; do
    [ -n "$name" ] || continue
    state="$("$GH_REAL" pr checks "$pr" --repo "$repo" 2>/dev/null \
             | awk -F'\t' -v n="$name" '$1==n{print $2; exit}')"
    if [ -z "$state" ]; then
        echo "merge-guard: refusing — check '$name' has not reported on $repo#$pr." >&2
        failed=1
    elif [ "$state" != "pass" ]; then
        echo "merge-guard: refusing — check '$name' is '$state', not pass, on $repo#$pr." >&2
        failed=1
    fi
done <<EOF
$checks
EOF

if [ "$failed" = "1" ]; then
    {
        echo "merge-guard:   $repo cannot enforce this server-side, so it is enforced here."
        echo "merge-guard:   override deliberately with GHAPP_ALLOW_UNVALIDATED_MERGE=1."
    } >&2
    exit 1
fi
exit 0
