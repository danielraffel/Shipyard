---
name: ci
description: Cross-platform CI coordination with Shipyard — validates, ships, manages queue, and runs cloud workflows
---

# CI Operations with Shipyard

Shipyard coordinates validation across local, SSH, and cloud targets.

## Quick reference

| Task | Command |
|------|---------|
| Validate current branch | `shipyard run --json` |
| Validate specific targets | `shipyard run --targets mac,ubuntu --json` |
| Iterate on one platform's CI failure | `shipyard run --skip-target <others>` (see [Iterating on a single-platform failure](#iterating-on-a-single-platform-failure)) |
| Fast smoke check | `shipyard run --smoke --json` |
| Start the live-mode webhook daemon | `shipyard daemon start` |
| Inspect the daemon | `shipyard daemon status --json` |
| Stop the daemon | `shipyard daemon stop` |
| Full ship (PR + validate + merge) | `shipyard ship --json` |
| Ship to develop instead of main | `shipyard ship --base develop --json` |
| Resume an interrupted ship | `shipyard ship --resume --json` (auto when state exists) |
| Force-restart a stale ship | `shipyard ship --no-resume --json` |
| List in-flight ship states | `shipyard ship-state list --json` |
| Inspect one PR's ship state | `shipyard ship-state show <pr> --json` |
| Live-tail the active ship | `shipyard watch` (or `shipyard watch --pr <n>`) |
| One-shot snapshot | `shipyard watch --no-follow --json` |
| Merge on green (cron-safe one-shot) | `shipyard auto-merge <pr>` (0=merged, 1=fail, 2=not-found, 3=in-flight) |
| Diagnose RELEASE_BOT_TOKEN | `shipyard release-bot status --json` |
| Configure RELEASE_BOT_TOKEN | `shipyard release-bot setup` (guided) |
| Re-paste token after rotation | `shipyard release-bot setup --paste` |
| Opt in to post-release docs sync | `shipyard changelog init` then `shipyard release-bot hook install` |
| Regenerate CHANGELOG.md from tags | `shipyard changelog regenerate` |
| CI drift gate for CHANGELOG.md | `shipyard changelog check` |
| Run the post-tag hook locally | `shipyard release-bot hook run --tag v0.9.0` |
| Live-probe the release chain | `shipyard doctor --release-chain` (dispatches + waits) |
| Show queue and status | `shipyard status --json` |
| Show all queued jobs | `shipyard queue --json` |
| Show run logs | `shipyard logs <job_id> --json` |
| Show logs for one target | `shipyard logs <job_id> --target windows` |
| Check merge readiness | `shipyard evidence --json` |
| Bump job priority | `shipyard bump <job_id> high` |
| Cancel a job | `shipyard cancel <job_id>` |
| List cloud workflows | `shipyard cloud workflows --json` |
| Show cloud defaults | `shipyard cloud defaults --json` |
| Dispatch a cloud workflow | `shipyard cloud run build --json` |
| Dispatch only if remote matches HEAD | `shipyard cloud run build --require-sha HEAD --json` |
| Opt a target into cross-PR reuse | set `reuse_if_paths_unchanged = ["src/backend/**"]` under `[targets.<name>]` |
| Opt a target into warm-pool reuse | set `warm_keepalive_seconds = 600` under `[targets.<name>]` (see "Warm-pool reuse" below) |
| Inspect warm-pool entries | `shipyard targets warm status --json` |
| Drain the warm-pool (force cold-start everywhere) | `shipyard targets warm drain --yes` |
| Force cold-start for one ship only | `shipyard ship --no-warm` (or `shipyard run --no-warm`) |
| Global warm-pool kill switch | `SHIPYARD_NO_WARM_POOL=1` in the environment |
| Retarget one lane on an in-flight PR | `shipyard cloud retarget --pr <n> --target macos --provider namespace` (dry-run; add `--apply`) |
| Add a new lane to an in-flight PR | `shipyard cloud add-lane --pr <n> --target windows [--provider namespace]` (dry-run; add `--apply`) |
| Skip a version-bump gate | `shipyard pr --skip-bump sdk --bump-reason "docs only"` |
| Skip a skill-sync gate | `shipyard pr --skip-skill-update ci --skill-reason "mechanical"` |
| Deliberately skip one lane | `shipyard run --skip-target windows` (repeatable; no probe run) |
| Proceed with unreachable lanes (VALIDATION GAP) | `shipyard run --allow-unreachable-targets` (prints a loud warning; exits 3 without the flag) |
| Inspect tracked cloud runs | `shipyard cloud status --json` |
| Environment check | `shipyard doctor --json` |
| Probe SSH runner reachability | `shipyard doctor --runners --json` |
| Clean up artifacts | `shipyard cleanup --apply` |
| Mark a target advisory | `[targets.<n>] advisory = true` in `.shipyard/config.toml` (see "Advisory lanes" below) |
| Flip lane policy for one PR | `Lane-Policy: <target>=required\|advisory` trailer on the tip commit |
| List quarantined targets | `shipyard quarantine list --json` |
| Quarantine a flaky target | `shipyard quarantine add <target> --reason "..."` |
| Remove from quarantine | `shipyard quarantine remove <target>` |

## Live mode (`shipyard daemon`) — when it helps and when to ignore it

Shipyard has a long-running webhook receiver that converts GitHub
Actions events into a push-based event stream. When it's running,
`shipyard watch` can subscribe to the daemon instead of polling —
near-realtime updates with zero GitHub API budget spent on the watch
itself.

| You're here | Does live mode matter? |
|---|---|
| Solo macOS dev with Tailscale + Funnel enabled | **Yes, big win.** `shipyard daemon start` registers webhooks on tracked repos and streams events; the macOS menu-bar app and any `shipyard watch` invocation in a terminal both consume the same stream. |
| CI / headless server / someone without Tailscale | **Ignore it.** The daemon needs a public tunnel (Tailscale Funnel in v1) to receive webhooks. Without that, `shipyard watch` and everything else fall back to polling — behavior is unchanged from the pre-daemon CLI. |
| Agent running one-shot `shipyard ship` + `watch --follow` | **Probably doesn't matter.** The daemon helps most when multiple sessions or the GUI are tracking the same state concurrently; a single session blocking on `watch --follow` already has its own connection. |

**When in doubt, don't start the daemon.** The daemon is an
optimization, not a requirement. Polling is the correct fallback
for everything it doesn't cover and is always safe. The `run` /
`ship` / `watch` / `auto-merge` commands don't require the daemon
to be running.

`shipyard daemon status` is free (no `gh api` calls, just reads
the local socket) and cheap to probe from an agent — use it if
you want to know whether the user has live mode on before
deciding whether to rely on webhook-speed updates vs polling
cadence.

See [`docs/live-mode.md`](../../docs/live-mode.md) for setup (≈1
click on a Tailscale-ready Mac) and troubleshooting. The macOS
menu-bar app (`shipyard-macos-gui`) is a thin subscriber to this
same daemon.

## When to use `watch` (agent decision guide)

After dispatching a ship (`shipyard ship`), agents have four ways to
track it to completion. Pick by **session posture**, not by how long you
think the build takes:

| Posture | Command | Why |
|---|---|---|
| You can hold the session open until merge | `shipyard watch --follow --json` | Blocks; exits `0` pass, `1` fail, `130` SIGINT. Zero polling logic needed. |
| You want to release the session, re-check later | `shipyard watch --no-follow --json` + `ScheduleWakeup` | One-shot snapshot is cheap. Re-check on wakeup; exits `3` while in-flight. |
| The agent is stepping away entirely | `shipyard auto-merge <pr>` on cron / GitHub schedule | Idempotent one-shot. Exits `0` merged, `1` fail, `2` not-found, `3` in-flight. |
| You just want a status peek right now | `shipyard watch --no-follow --json` | Same as a `ship-state show` but uses the live event schema. |

**Rules of thumb for agents:**

- If you just ran `shipyard ship` in the same turn and the user is
  waiting, `shipyard watch --follow --json` is almost always right —
  you already own the session.
- If you'll need more than ~5 minutes and want to yield back to the
  user, prefer `--no-follow` + `ScheduleWakeup`. Don't `sleep` inside
  the session.
- **Never poll with `watch --follow` in a tight loop.** `--follow`
  already blocks; calling it repeatedly is wasted cache and clock.
- `auto-merge` is for out-of-session automation (cron, systemd timer,
  GitHub Actions schedule). Not a substitute for `watch` within a live
  agent session.

Example — agent blocks until merge in-session:

```sh
shipyard ship --json
shipyard watch --follow --json   # exits when ship completes
```

Example — agent yields, re-checks later via `ScheduleWakeup`:

```sh
shipyard ship --json
shipyard watch --no-follow --json | jq '.state'
# → "in_flight" → ScheduleWakeup 20m, re-run the same snapshot
# → "passed"    → done
# → "failed"    → inspect logs
```

### Reading rich watch output

`shipyard watch` (human mode) shows per-run elapsed time, heartbeat
age (`last_seen=12s_ago`, tagged `stale` when > `WATCH_STALE_SECS`,
default 90s), a progress summary (`2/3 targets complete`), color +
symbols (`✓`/`✗`/`⋯`), and a timestamp separator between snapshots.
Honors `NO_COLOR=1` (XDG) for piped output. JSON mode adds
`last_heartbeat_at`, `phase`, and `elapsed_seconds` fields to each
dispatched-run emission; existing consumers keep working.

When a runner goes silent past the stale threshold, `FallbackChain`
auto-demotes it to UNREACHABLE and continues with the next provider.
Use `shipyard doctor --runners` to probe SSH targets without running
a ship.

## Mid-flight runner retargeting

When a provider change would be valuable *during* an in-flight PR drain — e.g., you notice halfway through shipping 10 PRs that Namespace macOS is faster than GitHub-hosted — use `shipyard cloud retarget`:

```sh
# Preview first (dry-run by default):
shipyard cloud retarget --pr 224 --target macos --provider namespace

# Apply when the plan looks right:
shipyard cloud retarget --pr 224 --target macos --provider namespace --apply
```

What it does:
1. Finds the PR's latest workflow run.
2. Cancels the **one job** matching `--target` on the old provider (substring match on the job name, e.g. `macos` matches `macOS (ARM64) [github-hosted]`).
3. Dispatches a fresh workflow run with the new provider.

**Known limitation (read before running):** step 3 starts a new workflow run, so targets other than the one you retargeted will also re-run in that new run. Their *prior* pass/fail statuses persist on the PR's check rollup, and pulp-style `resolve-provider` matrix workflows reuse caches — so the net effect is "flip the lane" without losing ground on the other lanes, even though they technically re-execute.

## Mid-flight lane addition

Sibling to retarget. Use when a ship is already in flight and you realize you want to validate against an *additional* platform without cancelling and re-dispatching the whole matrix — e.g., you started with `[macos, linux]` and want to add `windows`:

```sh
# Preview (dry-run by default):
shipyard cloud add-lane --pr 224 --target windows

# Apply when the plan looks right:
shipyard cloud add-lane --pr 224 --target windows --provider namespace --apply
```

What it does:
1. Loads the PR's ShipState. Refuses if absent (no in-flight ship) or terminal (merge already issued).
2. Idempotent: if the target is already in `dispatched_runs`, reports a no-op and does nothing.
3. Dispatches the single workflow for that target/provider.
4. Appends a new `DispatchedRun` to the ShipState so the watch loop joins it into the overall verdict.

See `docs/cloud-retarget.md` for full context; add-lane complements retarget.

## Ship workflow (the main flow)

1. Work on a feature branch. Commit your changes.
2. Run `shipyard ship --json` — this pushes, creates a PR, validates on all
   platforms, and merges when green.
3. If a target fails, read the logs with `shipyard logs <id> --target <name>`.
   If the failure is confined to one platform (which it usually is), **iterate
   locally against that target instead of re-shipping the full matrix** — see
   [Iterating on a single-platform failure](#iterating-on-a-single-platform-failure)
   below. Once the local lane is green, `shipyard ship --json` again.

Shipyard refuses to merge unless every required platform has passing evidence
for the exact HEAD SHA.

### Iterating on a single-platform failure

When CI goes red on exactly one platform (e.g. only the Windows leg of a
matrix, only the macOS sanitizer), **do not default to push → wait for full
matrix → read one platform's result → repeat**. That burns the dispatch cost
on every platform you didn't touch — typically 15–25 minutes per iteration
re-validating lanes that were already green.

Use `shipyard run` with target selection to validate the fix against the real
target, fast:

```bash
# Iterate on the Windows lane only (skips mac + ubuntu)
shipyard run --skip-target mac --skip-target ubuntu --json

# Or, equivalent inclusive form
shipyard run --targets windows --json
```

`run` validates locally via the configured backend for that target (SSH host,
local VM, or cloud runner — whichever `.shipyard/config.toml` assigns). You
get a real result in ~5–10 minutes per target with no GitHub Actions runner
minutes burned and no re-validation of lanes you didn't change. Once the
local lane passes cleanly, `shipyard ship --json` to kick the final cross-
platform gate.

**When this loop doesn't fit:**

- **Final pre-merge gate.** `shipyard ship` / `shipyard pr` is still the
  only command that produces a merge-eligible evidence record. `shipyard run`
  iteration is for getting-to-green; `ship` is for landing it.
- **Platform-specific to a backend you don't have.** If the failure is
  specific to a GitHub-hosted runner (e.g. the `[github-hosted]` leg of a
  matrix where your local lane is SSH or Namespace), the local lane is a
  good proxy but not identical. Consider `shipyard cloud run build <branch>`
  as the middle ground — dispatches to the same cloud backend CI uses
  without re-running everything.
- **Cross-target behavioral differences you're actually testing.** If the
  bug only manifests when two targets interact (rare but real — e.g.
  shared caches), the single-target loop hides it.

**When `shipyard run` fails for reasons that don't match your change:**

Long-running SSH or VM backends accumulate per-run state — stale build
artifacts, partially-applied branches from interrupted earlier runs,
environment drift. If `run` errors on a lane with messages that look
unrelated to the code you changed (`cmake` complaining about files you
didn't touch, configure steps timing out on line one, paths pointing at
an earlier branch), check the host before assuming your code is wrong.

Typical diagnostic pass on an SSH backend:

```bash
ssh <backend-host>
cd <worktree>
git log -1 && git status             # did we land on the expected SHA?
ls -la .shipyard-stage-*             # old stage dirs still pinning files?
rm -rf .shipyard-stage-*             # nuclear reset; safe — always re-staged
```

Local VM backends usually have their own `reset` path in the project's
`.shipyard/` config. Re-run `shipyard run` after cleanup.

### Recovering an interrupted ship

If a ship was interrupted (laptop closed, session ended, OS restart), just
run `shipyard ship --json` again. Shipyard writes per-PR state to disk on
every dispatch and evidence event; the second invocation auto-resumes from
the same run IDs without re-dispatching. On SHA or merge-policy drift the
resume is refused with a clear message — re-run with `--no-resume` to
archive the stale state and start fresh. Full details in
[`docs/ship-resume.md`](../../docs/ship-resume.md).

## Queue management

When multiple jobs are queued (common with parallel worktrees):

- `shipyard queue --json` — see what's running and pending
- `shipyard bump <id> high` — make a job run next
- `shipyard bump <id> low` — deprioritize a job
- `shipyard cancel <id>` — cancel a pending or running job

## Target configuration

Targets are defined in `.shipyard/config.toml`:

```toml
[targets.mac]
backend = "local"
platform = "macos-arm64"

[targets.ubuntu]
backend = "ssh"
host = "ubuntu"
platform = "linux-x64"

# Optional fallback chain
fallback = [
    { type = "cloud", provider = "namespace", repository = "owner/repo", workflow = "build" },
]
```

There is no `shipyard config` or `shipyard targets` subcommand yet. Inspect
target definitions in `.shipyard/config.toml` and `.shipyard.local/config.toml`,
and use `shipyard status --json` for live target state.

### Locality routing (`requires`)

Targets can declare capability constraints with `requires = [...]`; the
fallback chain is then filtered to providers whose profile matches
every required capability. Vocabulary: `gpu`, `arm64`, `x86_64`,
`macos`, `linux`, `windows`, `nested_virt`, `privileged` (plus any
user-defined strings). Missing `requires` = no filter (backward
compatible). When nothing matches, the target errors with
`no provider satisfies requires=[…]: tried [namespace.default, …]`.
Full docs: [`docs/targets.md`](../../docs/targets.md) and
[`docs/profiles.md`](../../docs/profiles.md).

## SSH delivery: incremental bundles

SSH-backed targets deliver code via `git bundle`. On the first run the bundle is full (every object reachable from the target SHA, ~443 MB for Pulp-sized repos). On every subsequent run Shipyard probes the remote for its current HEAD over SSH (`git rev-parse HEAD`), verifies that the local clone has that commit as an ancestor, and emits `git bundle create <bundle> <target> ^<remote_head>` — a delta bundle that is typically kilobytes instead of megabytes. Any failure in the probe, ancestry check, or delta create silently falls back to the full-bundle path so the behavior on cold/corrupt remotes is unchanged. Each run logs a `bundle_mode=delta|full bundle_bytes=<N>` line to the per-target log so operators can confirm the optimisation is active.

## Cross-PR evidence reuse

When PR B rebases onto PR A's merged SHA and B's diff doesn't touch any
path that a target actually exercises, Shipyard can reuse A's passing
evidence instead of re-running the target. Off by default; opt-in per
target via `reuse_if_paths_unchanged`.

```toml
[targets.ubuntu-cpu]
backend = "ssh"
host = "ubuntu"
platform = "linux-x64"
# Only dispatch this target if HEAD changed one of these paths. If
# none match, borrow the most-recent passing evidence from an ancestor
# SHA and skip dispatch.
reuse_if_paths_unchanged = ["src/backend/**", "Cargo.lock"]
```

### When reuse fires

Pre-dispatch, for each target with `reuse_if_paths_unchanged` set:

1. Walk HEAD's first-parent ancestors and query the evidence store for
   the most recent PASS on this target whose SHA is in that list.
2. If found, compute `git diff --name-only <ancestor>..HEAD`.
3. If no changed file matches any glob, write a synthetic PASS evidence
   record with `reused_from: <ancestor_sha>` and skip dispatch.
4. Otherwise dispatch normally.

### Safety rules (always enforced)

| Refusal | Why |
|---|---|
| Non-fast-forward lineage | `git merge-base --is-ancestor` must succeed; rebases across unrelated history never reuse |
| Validation contract changed | The `[validation.contract]` subtable's digest is stored with each record; any change forces a re-run |
| Stage list changed | Adding / removing a stage between the ancestor and HEAD forces a re-run |
| No passing ancestor | If the most recent ancestor failed, or there's no record, reuse is declined |
| Chain reuse | A reused record is never itself a reuse source — we only borrow from real dispatches |

### How it surfaces

- `shipyard watch --json` emits `{"status": "reused", "reused_from": "<sha>"}` for reused targets (instead of the bare `"pass"`).
- `shipyard watch` human mode prints `evidence: <target>=✓ reused (from a1b2c3)`.
- Evidence records in the store carry `reused_from`; `shipyard evidence --json` shows it verbatim.
- The ship-state merge gate still counts reused targets as `pass`, so PR drain isn't blocked on a borrowed lane.

### When to enable

Reuse pays off on projects where the target's exercised surface is a
small subset of the repo — think a backend-only test lane on a mixed
frontend/backend monorepo, or a Cargo `cargo test -p backend` lane
whose output only changes when the crate or its dependencies move.
Don't enable it on a lane that runs the full suite — the globs would
have to cover the whole tree, at which point you're back to
re-running everything anyway.

## Warm-pool runner reuse

Cross-PR evidence reuse (above) skips the whole target when nothing
the target cares about changed. Warm-pool reuse is a narrower
optimisation: even when the diff *did* touch paths the target runs
against, the *runner itself* (SSH host, local workdir) doesn't need
to be re-cloned and re-dep-installed every time. When a PASS landed
within the last few minutes, the next ship on the same SHA can
re-enter the already-populated workdir and skip the pre-stage
(clone / sync / deps install). Validate — configure / build / test —
re-runs in full, so a code change is never silently masked.

Off by default. Opt in per target:

```toml
[targets.ubuntu]
backend = "ssh"
host = "ubuntu"
platform = "linux-x64"
# Hold the workdir open for 10 minutes after a PASS. Same-SHA ships
# within the window skip clone/sync/deps. Default 0 = feature off.
warm_keepalive_seconds = 600
```

### Three disable levels — why all three exist

| Level | Knob | When to reach for it |
|-------|------|----------------------|
| Per-target | `warm_keepalive_seconds = 0` (default) | Targets that rely on a pristine env (release validation, flaky build scripts) stay cold-only. |
| Global kill switch | `SHIPYARD_NO_WARM_POOL=1` env var | A CI that shells out to `shipyard` from inside another workflow — the outer runner is already ephemeral, and warm-pool state on that runner would be per-job noise. One-shot fresh escape hatch. |
| Per-ship CLI flag | `shipyard ship --no-warm` / `shipyard run --no-warm` | An agent deliberately wants a cold-start for this one ship — typically when debugging a pre-stage regression or confirming a clean-room build. |

The three levels compose: any one of them is enough to force a cold
start. Why this isn't simply always-on:

1. **Cloud runners cost money per second.** Silent always-on reuse on
   a paid provider would surprise a monthly bill.
2. **State drift is real.** Tests leave tmp files, build scripts
   assume fresh `~/.cache`, background processes upgrade deps.
   "Cold every time" is a correctness fence some users rely on.
3. **Sometimes the point IS cold.** Release-validation lanes
   deliberately want a pristine env to catch "works on my machine"
   regressions.

### Mechanics (what gets skipped, what still runs)

When a warm-pool hit fires, the dispatcher passes `resume_from=configure`
to the executor — the same machinery that powers `shipyard run
--resume-from <stage>`. The remote:

- Keeps the existing workdir at the recorded SHA — no re-clone, no
  bundle delivery, no `git checkout`.
- Skips the `setup` stage (the conventional home for deps installs).
- Runs `configure`, `build`, `test` as normal.

A validation config that uses a single `command` field (no stage
breakdown) can still benefit — the pre-stage skip still applies, but
the single command always runs in full.

### Eligibility and eviction

| Condition | Behavior |
|---|---|
| Target is on backend `cloud` / `github-hosted` | Silently ineligible. Workflow runs are ephemeral — there's nothing to keep warm. Shipyard warns once per invocation so a misconfigured target surfaces, not silently. |
| Current job SHA differs from the pool entry's SHA | Miss → cold start. The pool is strictly same-SHA; it is not a cross-SHA workdir cache. |
| Pool entry past `expires_at` | Pruned on lookup; cold start. |
| Any non-PASS outcome after a warm reuse was applied | Entry evicted. The pool never serves a dirty workdir twice. |
| `SHIPYARD_NO_WARM_POOL=1` set | Every lookup short-circuits to miss; no entries are recorded either. |

### How it surfaces

- `shipyard targets warm status --json` lists every live entry with
  target, host, backend, workdir, SHA, TTL remaining, expires_at,
  created_at. Expired entries are pruned as a side effect.
- `shipyard targets warm drain [--yes]` empties the pool — use after a
  host reboot, runner-image change, or any event that invalidates
  the tracked workdirs.
- Pool file lives at `<state_dir>/warm_pool.json`. Safe to delete
  manually; worst case, the next ship cold-starts.

### When to enable

- SSH lanes against a long-lived host where `apt install` / `npm
  install` / `cargo fetch` dominates the per-run wall clock.
- Local lanes with expensive first-run setup (e.g. virtualenv
  creation, system framework bootstrap).

### When NOT to enable

- Release-validation lanes — you want pristine every time.
- Flaky targets that sometimes leave lockfiles behind.
- Cloud / GitHub-hosted lanes — the backend is ineligible; the knob
  has no effect and Shipyard warns to reconcile the config.

## Failure classification

Every non-passing `TargetResult` and `EvidenceRecord` carries a `failure_class` (visible in `shipyard run --json`, `shipyard evidence --json`, and `shipyard watch --json`):

| Class | Meaning | Retry policy |
|-------|---------|--------------|
| `INFRA` | Network/SSH/runner availability problem (`Connection refused`, `ssh: connect`, `Network is unreachable`, `RUN_IN_DAYS_DEAD`, etc.) | Auto-retry on the next backend in the fallback chain |
| `TIMEOUT` | Hit the wall-clock cap | Auto-retry once |
| `CONTRACT` | `[validation.contract]` marker missing | Never retry — product bug |
| `TEST` | Non-zero exit with no infra/contract markers | Never retry — authoritative test failure |
| `UNKNOWN` | Fallback when the heuristics can't decide | Surfaced to the agent; not auto-retried |

Agents should read `failure_class` before deciding whether to retry, escalate, or surface to a human.

## Advisory lanes (lane degrade-mode)

Not every lane should block the merge. A matrix with one noisy runner (flaky Windows, experimental macOS-ARM64) still wants to keep shipping when the known-problem lane is red. Mark it advisory:

```toml
[targets.windows]
backend = "cloud"
platform = "windows-arm64"
advisory = true
```

A red advisory lane surfaces in `shipyard watch` and the PR body but does **not** block `shipyard ship` / `shipyard auto-merge`. Required lanes (the default — `advisory = false` or unset) still must be green.

### Overriding per PR — the `Lane-Policy:` trailer

Sometimes a release candidate needs to treat a normally-advisory lane as must-green (or vice versa). Put a trailer on the **tip commit** (never in the PR body):

```
Lane-Policy: windows=required
```

Multiple pairs, space- or comma-separated, are fine:

```
Lane-Policy: windows=required macos=advisory
```

The trailer overlays the config for this PR only. Unknown target names are ignored silently.

### Advisory vs quarantine — when to reach for which

| Question | Tool |
|---|---|
| "This lane is permanently flaky, I want to suppress TEST/UNKNOWN failures but still block on INFRA/TIMEOUT/CONTRACT." | `.shipyard/quarantine.toml` |
| "This lane is intentionally noisy / experimental / optional; its status is informational at all times." | `advisory = true` |
| "Just this one PR: escalate a normally-advisory lane to required." | `Lane-Policy: <target>=required` trailer |

They compose cleanly: a target can be both quarantined and advisory; the advisory flag is the wider knob.

### What the surfaces look like

- `shipyard watch` (human) dims advisory evidence/runs and tags them `(advisory)`.
- `shipyard watch --json` emits each dispatched run with a `required: bool` field so a downstream agent can filter without re-reading the config.
- The PR body opened by `shipyard ship` lists advisory lanes under an "Advisory lanes" section, calling out any overrides that came from the `Lane-Policy` trailer.

## Flaky-target quarantine

`.shipyard/quarantine.toml` is an opt-in list of targets whose `TEST` or `UNKNOWN` failures should be treated as advisory during the merge decision. `INFRA`, `TIMEOUT`, and `CONTRACT` failures are *never* suppressed — quarantine only hides authentic test flakiness, not infrastructure or contract bugs.

```toml
[[quarantine]]
target = "windows-arm64"
reason = "flaky Windows runner apr-2026 outage"
added_at = "2026-04-18"
```

Manage via `shipyard quarantine {list,add,remove}` (see table above). The merge check surfaces quarantined failures in the `advisory` field of the JSON payload; reviewers still see them but the merge is not blocked.

Remove a target from quarantine the moment the underlying flakiness is fixed — the list is meant to be short-lived.

## Troubleshooting

- `shipyard doctor --json` — checks git, ssh, gh, nsc are installed
- `shipyard status --json` — shows configured targets, queue state, and live target status
- `shipyard logs <id> --target <name>` — full log for a failed target
- If a target is unreachable with no fallback, `run` / `ship` / `pr` exit **3** (distinct from 1 validation-failed and 2 config-error) with a message that names the target, the failure category (`auth`, `host_key`, `network`, `timeout`, `unknown`), and the last ssh error.
- `shipyard run --allow-unreachable-targets --json` — proceed with the lane **SKIPPED, NOT validated**. The warning is loud by design because muscle-memory use of this flag (Pulp pre-2026-04-20) hid real backend outages.
- `shipyard run --skip-target <name>` — **deliberately** skip a lane (no probe run). Use this when you already know you don't want to validate the target — `--allow-unreachable-targets` is for "I want this target, but the backend is down right now."
- `shipyard cloud defaults --json` — inspect the current cloud workflow/provider dispatch plan

## Shipping a PR (the `shipyard pr` path)

When the user says "push a PR", "ship this", "ship it", "we're done", "merge this", or "push it" — run `shipyard pr` (or the `/pr` slash command — see `commands/pr.md`). It wraps `shipyard ship` with the versioning gates: skill-sync check, version-bump apply, and a `chore: bump versions` commit before handing off to the push/PR/validate/merge flow.

The orchestration, in order:

1. `skill_sync_check.py --mode=report` — hard-fails if a mapped path was touched without a `SKILL.md` update or a `Skill-Update:` trailer on the tip commit.
2. `version_bump_check.py --mode=apply` — rewrites `pyproject.toml` + `src/shipyard/__init__.py` for CLI-surface bumps and `.claude-plugin/plugin.json` for plugin-surface bumps. The two version streams are independent per `RELEASING.md`.
3. `git commit` + `gh pr create` + `shipyard ship`.
4. On merge, `.github/workflows/auto-release.yml` tags the CLI bump as `v<x.y.z>`. The existing tag-triggered `release.yml` builds the 5-platform binaries and publishes the GitHub Release.

Never run `gh pr create` + release separately. Never run the Python gate scripts by hand.

### Gate-script path resolution

`shipyard pr` looks up each gate script in this order — the first hit wins:

1. Env var (`SHIPYARD_SKILL_SYNC_SCRIPT`, `SHIPYARD_VERSION_BUMP_SCRIPT`, `SHIPYARD_VERSIONING_CONFIG`).
2. `.shipyard/config.toml` `[validation]` keys (`skill_sync_script`, `version_bump_script`, `versioning_config`).
3. `tools/scripts/<file>` — common CI-tooling layout (used by Pulp).
4. `scripts/<file>` — Shipyard's own default.

Missing-script errors list every probed location and every override knob. Consumer repos that keep their tooling under `tools/scripts/` need no configuration; other layouts should set the env var or the `[validation]` key rather than moving the script.

## State-machine lane + doc-sync gate

A dedicated `state-machine` CI job runs `pytest -m state_machine -v` on ubuntu-latest. Failures show up as a distinct check row (not mixed into the cross-platform `test` matrix), so a ship-state regression is visually separable from an infra blip. When writing a test that exercises ship-state transitions, add `pytestmark = pytest.mark.state_machine` at module scope.

A doc-sync gate enforces that `docs/ship-state-machine.md` moves whenever `src/shipyard/core/ship_state.py` or `src/shipyard/ship/**` does. Mechanism is `scripts/doc_sync_check.py` + `scripts/doc_sync_map.json` (mirrors `skill_sync_check.py` but targets free-form docs). Bypass via `Doc-Update: skip doc=<path> reason="..."` trailer.

## Bypass trailers (tip commit)

| Gate          | Trailer                                                      |
|---------------|--------------------------------------------------------------|
| Version bump  | `Version-Bump: <surface>=<patch\|minor\|major\|skip> reason="..."` |
| Skill update  | `Skill-Update: skip skill=<name> reason="..."`              |
| Doc-sync      | `Doc-Update: skip doc=<path> reason="..."`                  |
| Auto-release  | `Release: skip reason="..."`                                 |
| Lane policy   | `Lane-Policy: <target>=required\|advisory` (escalate/demote for this PR only) |

**Gotcha:** anything under `.github/workflows/**`, `.claude-plugin/**`, `commands/**`, `agents/**`, `hooks/**`, `scripts/release.sh`, `src/shipyard/cli/**`, `src/shipyard/runners/**`, or `src/shipyard/config/**` triggers the `ci` skill's path map (`scripts/skill_path_map.json`). Update this SKILL.md in the same PR — or use the `Skill-Update: skip` trailer with a real reason.

**Manual release fallback:** `./scripts/release.sh` still exists for emergencies but is no longer the happy path. Normal releases flow through `shipyard pr` → merge → auto-release workflow.

**`RELEASE_BOT_TOKEN` is required for the auto-release chain to fire.** Without it, auto-release silently degrades — tags get created via `GITHUB_TOKEN` but GitHub doesn't trigger workflows on `GITHUB_TOKEN`-pushed tags, so `release.yml` never runs and no binaries ship. Run `shipyard doctor` to check; if the secret is missing, follow the "One-time setup" section in `RELEASING.md`. `shipyard pr` will also print a heads-up before pushing the PR if the secret isn't present.
