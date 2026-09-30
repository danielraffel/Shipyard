# Adopting Shipyard in a repository

Use this when a repository is new to Shipyard, or when you are about to build
CI machinery (a PR gate, a queue, a receipt, a runner router) and need to know
whether the repository already has it. Detect first; the most expensive
mistake here is reimplementing something that is already wired, or wiring the
queue before the checks it depends on.

**Only features with a demonstrated effect are recommended here.** Each
section carries a verdict from the 2026-09-30 feature-proof audit (Pulp's
planning repo, `research/2026-09-30-feature-proof-audit.md`), which asked of
every feature: is it on main, does it run on the live path, and does it produce
a measurable effect, with a count, a source, a sample size and a control.

| verdict | meaning here |
|---|---|
| WORKING | documented; adopt it |
| not ready: `<reason>` | BROKEN or UNPROVEN. Detect it so you do not rebuild it, but do not adopt or rely on it yet |
| (absent from this page) | DEAD. It never runs; do not adopt it |

Re-check a "not ready" verdict against a newer audit before acting on it; they
are dated facts, not permanent ones.

Pulp (`Generous-Corp/pulp`) is the most complete consumer and is the worked
example. Where a capability exists only in Pulp's own tooling, this page says
so and names what would have to move into Shipyard for another repository to
get it without copying Pulp.

## Run the adoption audit first

From a checkout of the repository being audited (the GitHub App wrapper derives
repository provenance from the working directory):

```sh
skills/ci/scripts/adoption_audit.sh OWNER/REPO [BASE]
```

It is read-only. Each row has two separate verdicts:

- **STATUS** (`present`, `partial`, `absent`, `n/a`, `UNKNOWN`): is it
  configured?
- **PROVEN** (`yes`, `no`, `unmeasured`, `-`): did it have a non-zero effect on
  recently merged PRs, measured with the proxy named in its section below?

`present` alone never means working. The report ends with the next feature to
adopt (only WORKING features are recommended) and a "present but not proven"
list.

Controls run before any verdict: `origin/BASE` resolves to a non-empty tree,
the checkout's `origin` is the named repository, the API returns that
repository's own name, and each landing report reached its `VALIDATION`
section. If a control fails, the rows it feeds read `UNKNOWN`, never `absent`.
A partial read counts as a failed read: branch protection answering 403
without admin scope, `allow_auto_merge` omitted for a caller without push
access, or a missing tool all yield `UNKNOWN`; only GitHub's explicit 404
"Branch not protected" is read as a real absence. A low-yield effect (receipt
reuse) needs 10 decisions before a zero reads `no`.

`skills/ci/scripts/test_adoption_audit.sh` covers each failure case offline
with stub `ghapp` and `shipyard` binaries, plus healthy and unprotected
controls. Run it after editing the audit.

Measured on 2026-09-30 (10 merged PRs for author and auto-merge, 2 for
landing reports):

| feature | Pulp status / proven | tartci status / proven |
|---|---|---|
| `shipyard pr` flow | present / yes (6 of 10 opened as the App) | absent |
| required checks | present / yes (6 of 6 reported on heads) | partial (1 live, undeclared) / yes (1 of 1) |
| version/skill-sync gates (CI) | present / unmeasured (not ready) | absent |
| merge queue | present / yes (2 merge-group runs) | absent |
| auto-merge (MERGE) | present / yes (10 of 10) | present / yes (10 of 10) |
| PR-head fast tier | present / yes | absent |
| protected receipt reuse | present / unmeasured (0 of 2, below floor) | absent |
| host classes (machine) | present / yes (4 of 4 readable) | same machine, same answer |
| runner governance | present / unmeasured (not ready) | n/a (hosted runners only) |

Recommended next: Pulp, nothing in the core set; tartci, the `shipyard pr`
flow.

Instrument traps the audit already avoids, and that hand-run probes hit:

- **Classic branch protection does not report rulesets.** `branches/main/protection`
  has no merge-queue field, so its silence is not absence. Read
  `repos/O/R/rules/branches/BASE` (effective rules, including organisation
  rulesets) or `repos/O/R/rulesets`.
- **A 404 body arrives on stdout.** `ghapp api repos/O/R/branches/main/protection`
  on an unprotected branch prints `{"message":"Branch not protected",...}`,
  which a `--jq` filter or a line count reads as one required context. Test the
  exit status, not the output.
- **The PR author is the App for `ghapp` too.** `shipyard-local[bot]` opens PRs
  from both `shipyard pr` and plain `ghapp`; tartci's PRs are App-authored with
  no Shipyard config. Read the author share as "App-routed", not "gated".
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

The feature-proof audit did not measure `landability`, `governance status` or
`landing` as features; they are the instruments. Cross-check what they report
against the raw API reads above before acting on it.

## Adoption order

Each step assumes the ones before it:

1. `shipyard pr` flow (config + gate scripts).
2. Required checks, live and declared to Shipyard.
3. Auto-merge allowed, merge method MERGE.
4. Merge queue, once strict up-to-date protection is causing a treadmill.
5. PR-head fast tier, only with a queue (the full suite has to run somewhere).
6. Protected receipt reuse, only with a queue and full-suite evidence on heads.

Host classes and `fleet-update` are machine setup, independent of this order.
Take a proxy baseline (last section) **before** step 1 and after each step.

---

## 1. `shipyard pr` flow: WORKING

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
`tools/scripts/`, `scripts/`).

**Adopt:**

1. `shipyard init --discover-only` to see what Shipyard detects, then
   `shipyard init` to write `.shipyard/config.toml`.
2. Copy `scripts/skill_sync_check.py`, `scripts/version_bump_check.py` and
   `scripts/versioning.json` from Shipyard (the schema is shared with Pulp).
   The repository supplies `versioning.json` (its version surfaces) and, if it
   has skills, a `skill_path_map.json`.
3. Optional pin: `tools/shipyard.toml`, then `shipyard pin show` / `pin bump`.
4. Optional: `[pr.body] attribution = "..."` so agents stop hand-patching bodies.

**Not ready: the local mac validation lane.** `shipyard pr` also dispatches the
repository's `[targets.*]` lanes. For Pulp's `backend = "local"` mac target the
audit found 1 green of 147 runs since 2026-09-02, and 64 of 64 failed in the
last 7 days (42.3 host-hours), while nothing consumed the verdict. Do not
declare a local target as a merge signal in a new repository. Opt-in targets
(below) are the planned way to keep such a lane without it running on every
PR; until they ship, a failing local-lane verdict is not a PR failure
(GitHub's required checks decide merging).

### Opt-in targets (`default = false`): not ready, unreleased

**Not ready: not merged or released.** Proposed in Shipyard #655 (branch
`feat/opt-in-targets`, expected in CLI 0.234.0, to be confirmed when it merges). Re-check before adopting: the
contract below is the PR's, not observed behavior, and it has no effect count
yet.

- **Gives:** `[targets.<name>] default = false` in `.shipyard/config.toml`
  keeps a target declared but out of `shipyard pr`, `shipyard ship` and
  `shipyard run` unless it is named: `--target <name>` (pr/ship, repeatable),
  `--targets` (run), or the active profile's list. When every target is
  opt-in, pr/ship still push, open the PR and arm MERGE, but queue no job,
  write no ship-state, and report `validation: "delegated"` /
  `verdict_owner: "required-checks"`. Skipping every default target with
  `--skip-target` still exits 2.
- **Adopt when:** GitHub required checks already decide landing and the local
  lane only duplicates them. Pulp plans it for `[targets.mac]`, whose local
  lane failed 64 of 64 runs in 7 days at 42.3 host-hours.
- **Detect:** `git show origin/main:.shipyard/config.toml | grep -n 'default *= *false'`
  (control: the same file lists `[targets.` headers), and `shipyard --version`
  at or above the release containing #655. `shipyard pr --json` then shows
  `"validation":"delegated"`.
- **Verify, once released:** local-lane host-hours and ship jobs queued per
  merged PR should drop to zero for the opted-out target, while merged PRs
  keep landing through required checks (the auto-merge proxy above stays at
  its level). Full contract: `docs/targets.md`, "Opt-in targets
  (`default = false`)", once #655 lands.

**Verify (audit proxy):** share of recent merged PRs opened as the App (Pulp:
14 of 20) together with Shipyard ship-state records on the submitting hosts
(`shipyard ship-state list`; Pulp: 83 in 7 days). The audit script reports the
first as PROVEN.

## 2. Required checks: WORKING (GitHub)

**Gives:** the contexts GitHub itself enforces. Auto-merge, the queue and
`landability` all key off this list. This is a GitHub feature, not a Shipyard
one; the feature-proof audit relied on it rather than scoring it.

**Detect:**

```sh
ghapp api repos/O/R/branches/main/protection --jq '.required_status_checks'   # exits 1 when unprotected; the 404 body is on stdout
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
produces one of those contexts. Keep required workflows free of `paths:`
filters under `pull_request`: a path-filtered required check stays pending
forever.

Found while writing this: Pulp's `[governance]` lists 3 contexts, live
protection requires 6 (`shipyard governance status` exit 1). Declared and live
drifting is the normal failure; re-check after every protection change.

**Verify:** every required context reported by a check run on recent PR heads
(the audit script's PROVEN: Pulp 6 of 6). `shipyard landability` should also
report zero `no_producer` and zero `unserved`.

## 3. Version / skill-sync gates in CI: not ready

**Not ready: UNPROVEN in CI.** The audit found 0 blocks by the skill-sync or
version-bump steps in 200 runs of Pulp's required `Enforce version & skill
sync` (control: the same job failed 3 times, on other steps). `shipyard pr`
runs the same checks before it pushes, so the CI copy is a backstop whose
effect has not been observed. Adopt the local gates through step 1; add the CI
copy only if PRs reach the repository by routes other than `shipyard pr`, and
measure its blocks when you do.

**Detect:**

```sh
git grep -l -E 'skill_sync_check|version_bump_check' origin/main -- .github/workflows   # finding
git grep -l 'runs-on' origin/main -- .github/workflows | wc -l                          # control
```

Present only when one of those workflows' job `name:` is also a required
context. Shipyard's own repository runs the gate but does not require it
(`partial`).

**Verify (audit proxy):** failures of the skill-sync and version-bump steps,
by step conclusion, per 100 runs of the required job. Zero with the job failing
on other steps is "unproven", not "working".

## 4. Auto-merge (MERGE): WORKING

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
Nothing in Shipyard config. The `shipyard:no-auto-merge` label opts a PR out.

**Verify (audit proxy):** merged PRs whose timeline has an
`AutoMergeEnabledEvent` (Pulp: 39 of 40; control: 40 of 40 have
`AddedToMergeQueueEvent`). The audit script reads it per PR through GraphQL.

## 5. Merge queue: WORKING

**Gives:** batched validation of up to N PRs on the merged result. With strict
up-to-date protection and no queue, every merge makes every other PR `behind`
and each must revalidate alone; `shipyard landing` names this the treadmill.

**Detect:**

```sh
ghapp api repos/O/R/rules/branches/main --jq '.[]|select(.type=="merge_queue")|.parameters'
ghapp api repos/O/R/rulesets --jq '.[]|"\(.name) \(.enforcement)"'   # control: readable list
shipyard landing --repo O/R --base main     # MERGE QUEUE: PRESENT/ABSENT, surfaces consulted
```

Pulp: ruleset `main-merge-queue`, active, ALLGREEN, MERGE, max 5 to merge.

**Adopt:** a branch ruleset with a `merge_queue` rule, `merge_method: MERGE`,
enforcement `active`. Every required workflow must also trigger on
`merge_group`, or the queue waits forever. Let GitHub's native auto-merge do
the enqueueing: in the audit it did all of it. Shipyard's own enqueue path
was dormant (last write 2026-09-15), and the queue-tick merge path and the
merge-steward workflow were dead (0 merges in 1,005 ticks; no run since
2026-08-27), so do not adopt either.

**Verify (audit proxy):** `shipyard metrics gate-cost`: merge-queue attempts
per merged PR and batch fullness (audit, Pulp: 125 merged PRs, 1.29 PRs per
batch of max 5, 36 ejections). A queue that only forms batches of 1 is
serialising, not batching.

## 6. PR-head fast tier: WORKING (Pulp)

**Gives:** PR heads run a narrowed deterministic tier and the full suite runs
in the merge group. Heads report in seconds instead of occupying a full gate.

**Generic part (Shipyard):** the `shipyard-test-tier/v1` annotation contract
([validation-signals.md](../../../docs/validation-signals.md)) and its readers:
`shipyard landing --pr N` prints "GREEN on the fast tier, NOT full validation"
instead of a bare green.

**Repository part:** choosing the tier. Pulp uses a ctest label `pr-fast`
(`test/cmake/pr_fast_tests.cmake`) with `--no-tests=error` so an empty label
fails, and additionally runs the ctests its base-to-head diff reaches
(`tools/ci/pr_head_affected_tests.py`, WORKING in the audit: 94 selections, 10
real failures caught on PR heads). That selector is Pulp-only: it reads Pulp's
CMake graph through `pulp affected`.

**Trap seen in the audit:** anything that reads the full-suite JUnit on every
job breaks on fast heads, because the fast tier writes a different file. Pulp's
"Observe ctest non-runs" step read `ctest.junit.xml`, found nothing, and put a
red annotation on 48 of 48 fast-tier PR jobs. Key such readers on the tier
annotation.

**Detect:**

```sh
shipyard landing --repo O/R --pr <recent merged PR>
#   macos: tier fast (selector pr-fast); full suite runs in merge_group   <- present
#   lint: tier unknown (no shipyard-test-tier annotation)                  <- absent
```

**Adopt:** after the queue. In the gate job, branch on `github.event_name`: on
`pull_request` run the narrowed selector and emit
`::notice title=shipyard-test-tier::{"schema":"shipyard-test-tier/v1","tier":"fast","selector":"<name>","full_suite_runs_in":"merge_group"}`;
on `merge_group` run everything and emit `{"schema":"shipyard-test-tier/v1","tier":"full"}`.
Keep the fast tier's JUnit separate so it can never become full evidence.

**Verify (audit proxy):** fast-tier PR head jobs that failed on a real test
(Pulp macOS: 15 of 129; each a failure caught before the queue), against
`shipyard-test-tier` fast notices as the control that the tier ran (106).

## 7. Protected receipt reuse: WORKING, low yield

**Gives:** a merge group whose tree already passed the full suite on its PR
head skips re-running it.

**Audit:** reuse in 5 of 31 macOS merge groups (16%) since the job-level
conclusion fix at 2026-09-29T15:30Z; each reused `macos` job took 2 to 5 s and
logged that it did not run the suite. The main loss: 31 of 70 heads had a
receipt, but on a base the queue had already moved past.

**Generic part (Shipyard):** the `shipyard-receipt-decision/v1` annotation
(WORKING: 172 notices in 94 merge-group runs), its rendering in `shipyard
landing --pr N`, and the reuse rate in `shipyard metrics gate-cost`.

**Pulp-only:** issuing and verifying the receipt. `tools/scripts/protected_merge_receipt.py`
(`issue`, `download`, `verify`, `note`, `publish-notes`), the
`protected-receipt-reuse` job in `build.yml`, and the rule that a receipt must
carry real ctest evidence (JUnit, exit, selection; a fast-tier run never
qualifies). The verifier is loaded from the protected base, never the head. To
make this generic, Shipyard needs the invariant "no reuse receipt without
evidence that validation ran" plus an issuer and verifier with a
repository-supplied test-result adapter (JUnit path, inventory count, selection
digest).

**Not ready around it:**
- Binary-identity shadow: BROKEN, `compared` never emitted in 70 of 70 merge
  groups.
- Per-test receipts, read half: BROKEN, 0 receipts used in 28 of 28 runs while
  every prior receipt was refused (the write half works).
- The label-set refusal fix: UNPROVEN, no post-fix sample yet.

**Detect:**

```sh
shipyard landing --repo O/R --pr <recent merged PR>
#   macos: reused receipt from run 123: 812 selected / 800 passed   <- reuse
#   macos: validated in full: receipt refused because ...            <- wired, refusing
```

**Adopt:** only after the merge queue and the fast tier, and only when heads run the full suite as
evidence (Pulp runs it non-gating on the head so a receipt exists). Emit a
decision annotation for every merge group, reuse or refuse, so "no decision"
stays a visible gap rather than a silent full run.

**Verify (audit proxy):** reuse decisions / merge-group runs since the last
change to the receipt path, with refusals by reason:

```sh
shipyard metrics gate-cost --repo O/R --workflow build.yml --gate-job macos --since 48h
#   receipt reuse: 5 of 27 merge-group runs (0.19); 18 refused, 4 no decision
```

A zero with every run "no decision" is a broken emitter, not a low rate. Below
about 10 decisions a zero is not evidence either way; the audit script reports
it as `unmeasured`.

## 8. Host classes and `runner fleet-update`: WORKING

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
`--apply` rolls it. `shipyard runner fleet-reconcile` is the backstop for
releases that did not roll out on their own. Do not use `fleet-update` as a
detection probe: it is a fleet operation even when it only plans, and
`runner capacity` answers the detection question without touching the rollout
path.

Known risk: the release soak keys on the newest tag, so several releases in
an hour can hold every rollout until the cadence slows.

**Verify (audit proxy):** rollouts and verified host installs from
`fleet-reconcile` / `fleet-update` records (audit: 10 rollouts, 37 verified
installs, 1 failed). The audit script's PROVEN counts readable host classes,
which proves the probe path, not a rollout.

## 9. Self-hosted runner governance: not ready

**Not ready: UNPROVEN as a Shipyard feature.** The audit measured only
Shipyard's queue admission (160 admission locks in 7 days, which proves
attempts, not effect). It did not measure `[runner.fleet.expected_host.*]`,
`runner fleet-status`, `runner audit`, `runner watch`, `rescue` or the health
leases in [fleet-lease.md](../../../docs/fleet-lease.md). Detect them so you do
not rebuild them; do not treat them as proven.

**Not in Shipyard:** core and memory admission leases live in tartci (WORKING,
though its release signal reports failure on 2,670 of 2,670 releases, so a real
failure is indistinguishable). Pulp routes raw builds through them with
`tools/ci/governed-build.sh`. tartci's VM reaper is BROKEN (0 stopped VMs
deleted since 2026-08-15, about 400 GB stranded), and its pulp CLI auto-apply
is BROKEN on two of three hosts. A repository gets these by running tartci, not
by adopting Shipyard.

**Detect:**

```sh
ghapp api repos/O/R/actions/runners --jq .total_count     # org-registered runners are not counted
git show origin/main:.shipyard/config.toml | grep -E '^\[(runner\.fleet\.expected_host\.|landability\])'
```

`n/a` for a hosted-only repository (tartci). A repository with registered
runners and neither section is `absent` (Shipyard itself).

**Verify:** `shipyard metrics gate-cost` `starvation` (jobs cancelled before a
runner was assigned / jobs) and `placement` (jobs on label sets nothing
serves). Pulp, 24 h: 5 of 77 starved, all withdrawn by a push; 0 of 496
unplaced. Use these to prove the effect before calling it adopted.

## 10. Proxy-first measurement

**Gives:** a before/after verdict that does not depend on how busy the hosts
were. Pulp's `proxy-first-eval` skill states the habit; Shipyard provides the
instrument.

**WORKING:** `shipyard metrics gate-cost` reads live from GitHub: gate runs per
merged PR, wasted attempts, starvation, placement, queue wait per job ahead,
batch fullness, receipt reuse. Minutes are shown as load-dependent context
only.

**Not ready:** `shipyard metrics scorecard`, `watch` and `trend` read the local
metrics store, whose GitHub import has been stale since 2026-09-27 and reports
"insufficient sample" rather than "stale". Pulp's
`tools/scripts/build_speed_scorecard.py` (build-graph proxies) is Pulp-only.

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

## Also detectable, all not ready

Detect these so you do not rebuild them. None had a demonstrated effect in the
audit.

| feature | detect | why not ready |
|---|---|---|
| changed-surface selection | `[targets.<t>.changed_surface_selection]`; `shipyard changed-surface-plan` | BROKEN: 0 of 174 bounded selections in 7 days; 63 refused on a policy-digest mismatch |
| changelog post-tag sync | `[release.post_tag_hook]`, `.github/workflows/post-tag-sync.yml` | Pulp's copy is DEAD: 100 of 100 runs cancelled on an unserved runner label; CHANGELOG 73 releases stale |
| base health | `shipyard landing` BASE HEALTH; a `main-health-detector.yml` workflow (Pulp's is repository code) | UNPROVEN: `batch_streak` 0 in 112 verdicts while a streak existed; judges one context only |
| batch attribution | `[queue.attribution] command = [...]` (Pulp's attributor is Pulp-only) | not measured by the audit |
| one retry after a network ejection | `[queue.environment_requeue] enabled = true` | not measured by the audit |
| daemon-driven waits | `shipyard daemon status`; `shipyard wait` | BROKEN in part: 38% and 66% of webhook deliveries rejected on two hosts, 0 daemon events to waiters. `shipyard wait` still works by polling (first answer 18 to 30 s) |
| opt-in targets | `default = false` under `[targets.<name>]` | unreleased (Shipyard #655); see section 1 |
| version pin | `tools/shipyard.toml`; `shipyard pin show` | Pulp's pin (v0.143.0) is read only by two dead workflows |
