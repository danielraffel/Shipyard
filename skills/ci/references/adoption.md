# Adopting Shipyard in a repository

Use this when a repository is new to Shipyard, or when you are about to build
CI machinery (a PR gate, a queue, a receipt, a runner router) and need to know
whether the repository already has it. Detect first; the most expensive
mistake here is reimplementing something that is already wired, or wiring the
queue before the checks it depends on.

Pulp (`Generous-Corp/pulp`) is the most complete consumer and is used below as
the worked example. Where a capability exists only in Pulp's own tooling, this
page says so and names what would have to move into Shipyard for another
repository to get it without copying Pulp.

## Run the adoption audit first

From a checkout of the repository being audited (the GitHub App wrapper derives
repository provenance from the working directory):

```sh
skills/ci/scripts/adoption_audit.sh OWNER/REPO [BASE]
```

It is read-only. It prints one row per feature (`present`, `partial`,
`absent`, `n/a`, or `UNKNOWN`), the evidence for each, and the next feature to
adopt in dependency order. Three controls run before any verdict: `origin/BASE`
resolves to a non-empty tree, the checkout's `origin` is the named repository,
and the API returns that repository's own name. If a control fails, the rows it
feeds read `UNKNOWN`, never `absent`.

Measured on 2026-09-30:

| feature | Pulp | tartci | Shipyard |
|---|---|---|---|
| `shipyard pr` flow | present | absent | present |
| required checks | present (6 live, declared) | partial (1 live, undeclared) | absent (branch unprotected) |
| version/skill-sync gates | present (required) | absent | partial (runs, not required) |
| merge queue | present (MERGE) | absent | absent |
| auto-merge allowed | present | present | partial (auto-merge off) |
| PR-head fast tier | present | absent | absent |
| protected receipt reuse | present | absent | absent |
| host classes (machine) | present | present (same machine) | present (same machine) |
| runner governance | present | n/a (hosted only) | absent (1 runner, undeclared) |

Recommended next: tartci, the `shipyard pr` flow; Shipyard, required checks.

Two instrument traps the audit already avoids, and that hand-run probes hit:

- **Classic branch protection does not report rulesets.** `branches/main/protection`
  has no merge-queue field, so its silence is not absence. Read
  `repos/O/R/rules/branches/BASE` (effective rules, including organisation
  rulesets) or `repos/O/R/rulesets`.
- **A 404 body arrives on stdout.** `ghapp api repos/O/R/branches/main/protection`
  on an unprotected branch prints `{"message":"Branch not protected",...}`,
  which a `--jq` filter or a line count happily reads as one required context.
  Test the exit status, not the output.

And three that apply to Shipyard's own readers:

- `shipyard landability` and `shipyard governance status` read
  `.shipyard/config.toml` from the **working tree**, not from the base. On a
  stale checkout of Pulp, `landability` reported five required contexts as
  `no_producer` because that branch predates `[landability]`; from a checkout of
  `origin/main` all six resolved. A sparse checkout is enough:
  `git worktree add --no-checkout --detach DIR origin/main && git -C DIR sparse-checkout set --no-cone '/.shipyard/' '/.github/' && git -C DIR checkout`.
- `shipyard config show` merges the machine-global config (host classes,
  `changed_surface_execution`), so it shows keys a repository never declared.
  For repository adoption, read `git show origin/BASE:.shipyard/config.toml`.
- `shipyard governance status` on a repository with no config compares live
  protection with the profile defaults and prints `fix: shipyard governance
  apply`. That is a write that would change branch protection. Never run it
  from an audit.

## Adoption order

Each step assumes the ones before it:

1. `shipyard pr` flow (config + gate scripts).
2. Required checks, live and declared to Shipyard.
3. Version/skill-sync gate as a required check.
4. Auto-merge allowed, merge method MERGE.
5. Merge queue, only once strict up-to-date protection is causing a treadmill.
6. PR-head fast tier, only with a queue (the full suite has to run somewhere).
7. Protected receipt reuse, only with a queue and full-suite evidence on heads.
8. Runner governance and host classes, only with self-hosted runners.

Take a proxy baseline (last section) **before** step 1 and after each step.

---

## 1. `shipyard pr` flow

**Gives:** one command that runs skill-sync and version-bump gates, pushes,
opens the PR as the App, arms native auto-merge (MERGE), and validates.

**Detect:**

```sh
git ls-tree --name-only origin/main .shipyard/config.toml        # finding
git ls-tree --name-only origin/main | wc -l                        # control: > 0
git ls-tree -r --name-only origin/main \
  | grep -E '(^|/)(tools/scripts|scripts)/(skill_sync_check|version_bump_check)\.py$'
git show origin/main:.shipyard/config.toml | grep -E '^(skill_sync|version_bump)_script'
```

Present when the config exists and both scripts resolve by the order in
[gate-scripts.md](../../../docs/gate-scripts.md) (env var, `[validation]` key,
`tools/scripts/`, `scripts/`). Do not use the PR author as evidence: both
`shipyard pr` and plain `ghapp` open PRs as `shipyard-local[bot]` (tartci's PRs
are App-authored and it has no Shipyard config).

**Adopt:**

1. `shipyard init --discover-only` to see what Shipyard detects, then
   `shipyard init` to write `.shipyard/config.toml`.
2. Copy `scripts/skill_sync_check.py`, `scripts/version_bump_check.py` and
   `scripts/versioning.json` from Shipyard (the schema is shared with Pulp).
   The repository supplies `versioning.json` (its version surfaces) and, if it
   has skills, a `skill_path_map.json`.
3. Optional pin: `tools/shipyard.toml`, then `shipyard pin show` / `pin bump`.
4. Optional: `[pr.body] attribution = "..."` so agents stop hand-patching bodies.

**Verify:** share of PRs merged in the window that carry a version-bump or
trailer decision from the gate (`git log --merges origin/main` against
`Version-Bump:` / `chore: bump versions`), and `shipyard pr` exit 0 on a trivial
branch. Gate refusals caught locally per PR is the proxy; how long `pr` took is
not.

## 2. Required checks

**Gives:** the contexts GitHub itself enforces. Everything later (auto-merge,
the queue, `landability`) keys off this list.

**Detect:**

```sh
ghapp api repos/O/R/branches/main/protection --jq '.required_status_checks'   # exit 1 = unprotected
ghapp api repos/O/R/rules/branches/main \
  --jq '.[]|select(.type=="required_status_checks")|.parameters.required_status_checks[].context'
ghapp api repos/O/R --jq .full_name                                          # control
git show origin/main:.shipyard/config.toml | grep -A3 '^\[governance\]'
shipyard landability --repo O/R --base main        # from a checkout of origin/main
```

Partial when checks are live but `[governance] required_status_checks` is not
declared (Shipyard cannot then tell a required red from an advisory one).

**Adopt:** declare `[governance] required_status_checks` to match the live
protection exactly, and `[landability] workflows` listing every workflow that
produces one of those contexts. Then `shipyard landability` must report every
required context `triggered` and every lane `served`. Keep required workflows
free of `paths:` filters under `pull_request`: a path-filtered required check
stays pending forever.

Found while writing this: Pulp's `[governance]` lists 3 contexts, live
protection requires 6 (`shipyard governance status` exit 1). Declared and live
drifting is the normal failure; re-check after every protection change.

**Verify:** `shipyard landability` exit 0 with zero `no_producer` and zero
`unserved`; `shipyard governance status` exit 0.

## 3. Version / skill-sync gates as a required check

**Gives:** the same two scripts `shipyard pr` runs locally, enforced in CI so a
PR opened any other way is still gated.

**Detect:**

```sh
git grep -l -E 'skill_sync_check|version_bump_check' origin/main -- .github/workflows   # finding
git grep -l 'runs-on' origin/main -- .github/workflows | wc -l                          # control
```

Present only when one of those workflows' job `name:` is also a required
context (Pulp: `Enforce version & skill sync` in `version-skill-check.yml`).
Shipyard runs the gate but does not require it, which the audit reports as
`partial`.

**Adopt:** copy Shipyard's `.github/workflows/version-skill-check.yml`, point
it at the repository's script paths, then add its job name to protection and
to `[governance]`. Pre-push layer: `.githooks/pre-push` calling the same
scripts in `--mode=report`.

**Verify:** count of PRs in the window whose check `Enforce version & skill
sync` (or the repo's name for it) failed and was then fixed by a bump or
trailer commit before merge; zero merged PRs with a source change and no bump.

## 4. Auto-merge (MERGE)

**Gives:** server-owned landing that survives the agent process. `shipyard pr`
and every route into `ship` arm it with method MERGE, so nobody hand-rolls the
GraphQL mutation. MERGE, not SQUASH: squash folds the bump commit into one and
breaks commit-derived release signals.

**Detect:**

```sh
ghapp api repos/O/R --jq '{allow_auto_merge,allow_merge_commit}'
shipyard landing --repo O/R --base main      # ACTION line names the landing path
```

**Adopt:** enable "Allow auto-merge" and merge commits in repository settings.
Nothing in Shipyard config. `shipyard:no-auto-merge` label opts a PR out.

**Verify:** green PRs that are neither armed nor queued should be 0.
`shipyard runner steward --arm-unqueued` (audit-only without `--apply`) lists
them; `shipyard landing` BACKLOG shows how many are armed.

## 5. Merge queue

**Gives:** batched validation of up to N PRs on the merged result. With strict
up-to-date protection and no queue, every merge makes every other PR `behind`
and each must revalidate alone; `shipyard landing` names this the treadmill.

**Detect:**

```sh
ghapp api repos/O/R/rules/branches/main --jq '.[]|select(.type=="merge_queue")|.parameters'
ghapp api repos/O/R/rulesets --jq '.[]|"\(.name) \(.enforcement)"'   # control: readable list
shipyard landing --repo O/R --base main     # MERGE QUEUE: PRESENT/ABSENT, surfaces consulted
```

`shipyard landing` reads rulesets, classic protection and the GraphQL
`mergeQueue`, and says which surface could not answer. Pulp: ruleset
`main-merge-queue`, active, ALLGREEN, MERGE, max 5 to merge.

**Adopt:** a branch ruleset with a `merge_queue` rule, `merge_method: MERGE`,
enforcement `active`. Every required workflow must also trigger on
`merge_group` or the queue waits forever (`shipyard landability` checks
triggers). Land with `shipyard ship --pr N`; do not merge directly.

**Verify:** `shipyard metrics gate-cost` `merge queue: attempts per merged PR`
(Pulp 1.18, n=22) and `batch fullness` (Pulp 1.05 of max 5, n=21). A
queue that only ever forms batches of 1 is serialising, not batching.

## 6. PR-head fast tier

**Gives:** PR heads run a narrowed deterministic tier and the full suite runs
in the merge group. Heads report in seconds instead of occupying a full gate.

**Generic part (Shipyard):** the `shipyard-test-tier/v1` annotation contract
([validation-signals.md](../../../docs/validation-signals.md)) and its
readers: `shipyard landing --pr N` prints "GREEN on the fast tier, NOT full
validation" instead of a bare green.

**Repository part:** choosing the tier. Pulp uses a ctest label `pr-fast`
(`test/cmake/pr_fast_tests.cmake`), a contract test that stops the tier going
empty (`tools/scripts/pr_fast_tier_check.py`), and, additionally, ctests
selected from the PR's base-to-head diff (`tools/ci/pr_head_affected_tests.py`).
That selector is Pulp-only: it reads Pulp's CMake graph and `pulp affected`.

**Detect:**

```sh
shipyard landing --repo O/R --pr <recent merged PR>
#   macos: tier fast (selector pr-fast); full suite runs in merge_group   <- present
#   lint: tier unknown (no shipyard-test-tier annotation)                  <- absent
```

Control: the report must reach its `VALIDATION` section; if it does not, the
zero is the instrument.

**Adopt:** after the queue. In the gate job, branch on `github.event_name`:
on `pull_request` run the narrowed selector and emit
`::notice title=shipyard-test-tier::{"schema":"shipyard-test-tier/v1","tier":"fast","selector":"<name>","full_suite_runs_in":"merge_group"}`;
on `merge_group` run everything and emit `{"schema":"shipyard-test-tier/v1","tier":"full"}`.
Keep the fast tier's JUnit separate so it can never be mistaken for full
evidence.

**Verify:** merge-group ejections caused by a PR's own test, per merged PR
(Pulp's pre-queue poka-yoke took this from 4.3/day to 2.2/day). If that rises
after adopting the tier, the tier is missing tests that PRs break.

## 7. Protected receipt reuse

**Gives:** a singleton merge group whose tree is exactly one that already
passed the full suite skips re-running it.

**Generic part (Shipyard):** the `shipyard-receipt-decision/v1` annotation, its
rendering in `shipyard landing --pr N`, and `shipyard metrics gate-cost`
`receipt reuse` rate. `[targets.*] reuse_if_paths_unchanged` is a different,
Shipyard-side evidence reuse for Shipyard's own targets; it does not make a
GitHub required check skip.

**Pulp-only:** issuing and verifying the receipt. `tools/scripts/protected_merge_receipt.py`
(`download`, `verify`, `note`, `publish-notes`), the `protected-receipt-reuse`
job in `build.yml`, and the rule that a receipt must carry real ctest evidence
(JUnit, exit, selection; a fast-tier run never qualifies). The verifier is
loaded from the protected base, never the head. To make this generic, Shipyard
needs the invariant "no reuse receipt without evidence that validation ran"
plus an issuer/verifier with a repository-supplied test-result adapter
(JUnit path, inventory count, selection digest).

**Detect:**

```sh
shipyard landing --repo O/R --pr <recent merged PR>
#   macos: reused receipt from run 123: 812 selected / 800 passed   <- present
#   macos: validated in full: receipt refused because ...            <- wired, refusing
shipyard metrics gate-cost --repo O/R --workflow build.yml --gate-job macos --since 48h
#   receipt reuse: 5 of 27 merge-group runs (0.19); 18 refused, 4 no decision
```

**Adopt:** only after 5 and 6, and only when heads run the full suite as
evidence (Pulp runs it non-gating on the head so a receipt exists). Emit a
decision annotation for every merge group, reuse or refuse, so "no decision"
stays a visible gap rather than a silent full run.

**Verify:** `gate-cost` receipt reuse rate over singleton merge groups, with
refusals by reason. A 0 with every run "no decision" is a broken emitter, not a
low rate.

## 8. Host classes and `runner fleet-update`

**Gives:** named self-hosted machines Shipyard can probe for VM capacity, and
one command that rolls an exact Shipyard release across them.

**Scope:** machine-global config (`shipyard paths` -> `global_dir/config.toml`),
not the repository. Every repository audited from one machine shows the same
answer.

**Detect:**

```sh
shipyard runner capacity --json | jq '.configured, (.hosts|length)'   # true 4 on the Pulp fleet
shipyard --mode isolated runner capacity --json | jq .configured       # control: false
```

**Adopt:** a `[host_class.<name>]` block per host with `ssh`, `shipyard_bin`,
`github_cli`, `github_token_helper`, `shipyard_mode`, `shipyard_global_dir`,
`shipyard_state_dir` ([install.md](../../../docs/install.md)). Then
`shipyard runner fleet-update --to vX.Y.Z --host-class <name>` prints the plan;
`--apply` rolls it. Do not use `fleet-update` as a detection probe: it is a
fleet operation even when it only plans, and `runner capacity` answers the
detection question without touching the rollout path. `shipyard runner fleet-reconcile` is the
backstop for releases that did not roll themselves out.

**Verify:** `shipyard runner fleet-reconcile` reports zero lagging classes
after soak; `runner fleet-status` exit 0.

## 9. Self-hosted runner governance

**Generic (Shipyard):** `[runner.fleet.expected_host.*]` (label-subset
inventory that stays visible when nothing is registered), `[landability]`,
`runner fleet-status`, `runner audit` (host-class naming drift), `runner
watch` (hung workers, stale runs), `rescue`, and health leases that fall back
to hosted runners when a lease expires ([fleet-lease.md](../../../docs/fleet-lease.md)).

**Not in Shipyard:** core+memory admission leases, gate VM pools and
work-conserving tiers live in tartci; Pulp routes its raw builds through them
with `tools/ci/governed-build.sh`. A repository gets those by running tartci on
its hosts, not by adopting Shipyard.

**Detect:**

```sh
ghapp api repos/O/R/actions/runners --jq .total_count     # org-registered runners are not counted
git show origin/main:.shipyard/config.toml | grep -E '^\[(runner\.fleet\.expected_host\.|landability\])'
shipyard landability --repo O/R --base main               # served / unserved per lane
```

`n/a` for a hosted-only repository (tartci). A repository with registered
runners and neither section is `absent` (Shipyard itself).

**Adopt:** declare expected hosts by labels unique to each host; if every
runner shares one label set, a per-host entry matches the whole pool and reads
all hosts online while one serves, so leave those to `fleet-status`'s host
list instead (Pulp's config explains this for its gate hosts).

**Verify:** `shipyard metrics gate-cost` `starvation` (jobs cancelled before a
runner was assigned / jobs) and `placement` (jobs on label sets nothing serves).
Pulp: 5 of 77 starved, all withdrawn by a push; 0 of 496 unplaced.

## 10. Proxy-first measurement

**Gives:** a before/after verdict that does not depend on how busy the hosts
were. Pulp's `proxy-first-eval` skill states the habit; Shipyard implements the
instruments.

**Generic (Shipyard):** `shipyard metrics gate-cost` (gate runs per merged PR,
wasted attempts, starvation, placement, queue wait per job ahead, batch
fullness, receipt reuse; minutes shown as load-dependent context only) and
`shipyard metrics scorecard` (verdict against the previous equal window, with
minimum samples and detection floors).

**Pulp-only:** `tools/scripts/build_speed_scorecard.py` (build-graph proxies:
compile units rebuilt, cache hit rate, header blast radius).

**Detect:** a habit is not in the tree. Ask whether the repository's recent
CI changes cite a count with source, floor and n. The instrument is available
wherever `shipyard metrics gate-cost --help` runs.

**Adopt:** before step 1, record a baseline:

```sh
shipyard metrics gate-cost --repo O/R --workflow <file.yml> --gate-job <job> --base main --since 7d
```

tartci baseline, 7 days: 1.96 PR-head gate runs per merged PR, 0.22 wasted,
0 of 133 starved, n=67. Re-run the same window length after each step; below
the reported minimum sample, draw no verdict. Pair every zero with a control on
the same instrument (a known-present repository, or `--mode isolated`).

**Verify:** each adopted feature names its own proxy above; a feature whose
proxy did not move was not adopted, whatever the wall clock says.

## Also detectable, optional

| feature | detect |
|---|---|
| version pin | `tools/shipyard.toml`; `shipyard pin show` (refuses without it) |
| changelog regeneration | `[release.changelog]`, `[release.post_tag_hook]`; see the `changelog` skill |
| changed-surface selection | `[targets.<t>.changed_surface_selection]`; `shipyard changed-surface-plan` ([changed-surface-selection.md](../../../docs/changed-surface-selection.md)) |
| batch attribution | `[queue.attribution] command = [...]` (argv list; Pulp's attributor `tools/scripts/queue_batch_attribute.py` is Pulp-only) |
| one retry after a network ejection | `[queue.environment_requeue] enabled = true` |
| base health | `shipyard landing` BASE HEALTH; needs a `main-health-detector.yml` workflow (Pulp's is repository code) |
