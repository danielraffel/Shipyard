# Rollups

A rollup lands several approved pull requests as one tested merge. It is
off unless a repository's base config turns it on, and it works with or
without a merge queue.

## Why a rollup, and when it saves nothing

A merge queue that requires checks on every entry builds each entry's own
group: entry 1 is tested as base+1, entry 2 as base+1+2, and so on. N pull
requests still cost N full runs. A rollup is one branch and one pull
request, so it costs one full run for N members.

That saving is real only when the members have not already spent a full run
each. A member that ran the full suite on its own pull request costs the
same whether it then lands alone or in a rollup. The intended shape is that
members are approved on their contract lane (the cheap lane a tiered check
runs on a pull request), and the rollup head is the one head that runs the
full lane.

## Configuration

In the base branch's `.shipyard/config.toml`, read through the GitHub API,
never from a head:

```toml
[train]
enabled = true
mode = "rollup"            # "off" (default), "burst" or "rollup"
landing = "merge-queue"    # or "direct"
max_members = 4            # 1 to 4; 4 is the hard cap
exclude_paths = []         # globs no member may change (empty by default)
hot_files = []             # globs no two members may both change (empty by default)
require_counted_approval = true
```

Shipyard ships no paths, check names or cadence for any repository. A
repository lists its own `exclude_paths` and `hot_files`.

On a host, `SHIPYARD_TRAIN=off` (or `0` or `false`), optionally scoped as
`off:owner/repo`, stops rollups, and so does a `train.pause` file in the
guards directory. Both stop the guard from accepting a rollup too, not only
the CLI from forming one.

## The shape

The branch is `shipyard-train/rollup-<n>`, `n` being the oldest member. It
holds the base commit `B` plus one merge commit per member, oldest first:
`M_i = merge(M_{i-1}, H_i)` with `M_0 = B`. The second parent of each merge
is the member's exact approved head `H_i`. Nothing else is committed.

`shipyard rollup build` forms it with `git merge-tree --write-tree` and
`git commit-tree`, so it never touches the checkout's work tree. A member
that does not merge cleanly is refused. The rollup pull request gets one
notice comment, one `rollup-of: #n@H_i` line per member, for people. The
guard never reads it, and it carries no `reviewer:` line.

Each member stays revertable on its own: `git revert -m 1 M_i` removes
exactly member `i`.

## What the guard accepts

A rollup head has no verdict of its own. The queue-arm guard
(`rollup_acceptance` in `scripts/ghapp_queue_arm_guard.py`) and Shipyard's
own gate (`crate::rollup::accept`, reached from `crate::head_approval`)
apply the same rule. A rollup lands only when all of these hold:

- **(a) Members.** Each `H_i` is the current head of exactly one open pull
  request on the same base, and none is a draft. Each member passes the review rule at its own
  exact head, with no later HOLD. With `require_counted_approval` (the
  default) a carried approval does not count: the verdict must name `H_i`.
- **(b) Chain and replay.** The first-parent chain from the rollup head back
  to `B` is exactly `k` two-parent merges, at most `max_members`, whose second
  parents are the `H_i`. Each merge changes exactly its member's files to the
  member's blobs. An added commit, or an edit inside a merge, refuses.
- **(c) Files.** Members change disjoint files. At the rollup head every
  member file has the member's approved blob and mode, and every other file
  is as at `B`. The base touched none of a member's files between the
  member's merge base and `B`, and none of them between `B` and the base's
  current tip.
- **(d) Paths.** No member changes a path in `exclude_paths` or in the
  guard's carry exclusions (`carry_excluded_paths`, default
  `DEFAULT_CARRY_EXCLUDED_PATHS`). No two members change files that match
  the same `hot_files` glob.

A HOLD naming the rollup head itself stops it, like any head.

The rule is a bounded extension of the merge-only carry to several sources.
Each member's approval carries to the rollup head because every file it
changes keeps its approved blob and mode, and nothing else changed. No rule
can prove that two separately reviewed changes are right together. The
rollup's own full run tests that.

## Landing

Both landings need the merge method MERGE. `shipyard rollup land` refuses
otherwise, because squash or rebase would flatten the member merges into one
commit and lose per-member revert. It checks:

- merge commits are allowed on the repository;
- for `merge-queue`, the base's merge-queue rule merges with `MERGE`;
- for `direct`, the base has no merge queue, and its pull-request rule (if
  any) allows `merge`.

The rollup pull request is the full-tier dispatch target. When a required
check is tiered (`[queue] tiered_required_checks`, docs/ghapp-guards.md),
`land` refuses until a full lane has tested the rollup pull request's merge
ref at its head. The refusal names the dispatch.

**`landing = "merge-queue"`.** `land` verifies and prints the merge
steward's handoff for the rollup head. The steward enqueues it through its
managed path, and its enqueue gate accepts the rollup by the rule above.
Nothing in `shipyard rollup` enqueues directly.

**`landing = "direct"`.** Shipyard lands the rollup itself. `land --apply`:

1. requires every required check of the base (rulesets and branch
   protection) to have passed on the rollup head as a check run: the latest
   run with that name, from the App the rule binds it to (`integration_id`,
   `app_id`) when it binds one. A commit status never counts, because a pull
   request's own workflow can post one under any name. A base that requires
   no checks refuses;
2. appends the tested record (`merged_base` = `B`, `tested_sha` = the rollup
   head, the members) to `rollup-landings.jsonl` in Shipyard's state
   directory;
3. reads the base ref again immediately before merging, and refuses on any
   move. A moved base means forming the rollup again and running its checks
   again;
4. merges only through the pull request merge API, with `sha=` bound to the
   rollup head and `merge_method=merge`, through the same guarded `gh` as any
   landing, so the guard judges it. Nothing pushes to the base or
   fast-forwards it;
5. checks that the merge commit's parents are exactly the re-read tip and
   the rollup head. If the base moved between the re-read and the merge, the
   landed tree was never tested as a whole: the landing is recorded with
   `"untested": true`, a comment on the rollup pull request says so, and
   `land` exits 1.

## Members after landing

However a rollup landed, its members are reconciled. `land` does it after a
direct landing; the merge steward does it on every `--apply` pass for a
base that takes rollups, which covers a merge-queue landing; and `shipyard
rollup reconcile --apply` does it by hand. Members are read from the landed
head's own merge commits, not from the notice comment. Each member GitHub
has not marked merged within two minutes of the landing is closed with a
comment naming the rollup and its merge commit, and only once its exact head
is proven on the base (`compare <base>...<head>` is `ahead_by` 0). A head
that is not on the base is never closed.

Whether GitHub marks members merged by itself when their heads become
reachable has not been tested live yet; the close above is the fallback
either way. Test it on a scratch repository before turning rollups on for a
real one.

## When the rollup fails

`shipyard rollup split` plans the bisect. A failed rollup is tested as two
halves (the older half takes the extra member). A single failing member is
ejected and the rest form again. If both halves pass although the whole
failed, the failure needs members from both halves, or is a flake:
quarantine the failing check and run the whole rollup once more. Never land
it on the halves' results.

## What first-parent tooling sees

On the base, a rollup is one first-parent commit: the merge of the rollup
pull request. `git bisect --first-parent`, a first-parent changelog, and
attribution by first-parent commit see one change for all members. Use the
second parents, or the `rollup-of:` notice, to name the members.

## Commands

```
shipyard rollup plan [--repo OWNER/REPO] [--base BRANCH]
shipyard rollup build --members 11,12 [--base BRANCH] [--apply]
shipyard rollup verify --pr N
shipyard rollup land --pr N [--apply]
shipyard rollup reconcile [--base BRANCH] --apply
shipyard rollup split --members 11,12,13,14 [--passed] [--first-passed BOOL --second-passed BOOL]
```

All of them exit 0 when done, 1 when refused and 2 when unreadable.
