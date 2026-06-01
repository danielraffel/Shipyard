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
| **Runner watchdog: health check** | `shipyard runner status --repo <r> --runner-id <id>` |
| **Runner watchdog: list stale queued runs (dry-run)** | `shipyard runner cleanup --dry-run` |
| **Runner watchdog: cancel stale queued runs** | `shipyard runner cleanup --fix` |
| **Runner watchdog: daemon mode** | `shipyard runner watch --fix` |
| **Runner watchdog: auto-kill hung workers (full recovery)** | `shipyard runner watch --kill-hung-workers` (implies `--fix`) |
| **Runner provisioning: set this box's machine tag** | `shipyard runner tag --set <studio\|m1\|m5>` (stored per-box; never hostname-derived) |
| **Runner provisioning: register N runners for a repo** | `shipyard runner register --repo <owner/repo> --count <N> [--ci-root <dir>]` (names `<repo>-<tag>-NN`, continues the index) |
| **Runner provisioning: dry-run the registration plan** | `shipyard runner register --repo <owner/repo> --count <N> --dry-run` |
| **Runner provisioning: live cross-repo pool view** | `shipyard runner list [--repo <owner/repo>]` (groups by machine; flags orphaned local dirs) |
| **Runner provisioning: deregister a runner** | `shipyard runner remove --name <repo>-<tag>-NN --yes [--purge-dir]` |
| **Self-update: check if a new release is available** | `shipyard update --check --json` |
| **Self-update: apply latest stable** | `shipyard update` (delegates to `install.sh`) |
| **Self-update: pin / rollback to a specific tag** | `shipyard update --to v0.53.0` |
| **Stuck-runner: kill specific worker (with recovery)** | `shipyard runner kill --pid <pid> --reason "..." [--retrigger]` |
| **Stuck-runner: review past kills** | `shipyard runner kill --history` |
| **Stuck-runner: restore quarantined build after a misclick** | `shipyard runner kill --recover <event-id>` |
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
| Retarget one lane on an in-flight PR | `shipyard cloud retarget --pr <n> --target macos --provider github-hosted` (dry-run; add `--apply`) |
| Add a new lane to an in-flight PR | `shipyard cloud add-lane --pr <n> --target windows [--provider github-hosted]` (dry-run; add `--apply`) |
| Rescue a PR whose runs are wedged on a self-hosted runner | `shipyard rescue <pr>` (cancels + redispatches queued runs to `github-hosted`; add `--dry-run` to preview, `--rerun-failed` for watchdog-cancelled runs) |
| Rescue every stuck run repo-wide | `shipyard rescue --all-stuck` |
| Skip a version-bump gate | `shipyard pr --skip-bump sdk --bump-reason "docs only"` |
| Skip a skill-sync gate | `shipyard pr --skip-skill-update ci --skill-reason "mechanical"` |
| Deliberately skip one lane | `shipyard run --skip-target windows` (repeatable; no probe run) |
| Proceed with unreachable lanes (VALIDATION GAP) | `shipyard run --allow-unreachable-targets` (prints a loud warning; exits 3 without the flag) |
| Inspect tracked cloud runs | `shipyard cloud status --json` |
| Environment check | `shipyard doctor --json` |
| Probe SSH runner reachability | `shipyard doctor --runners --json` |
| Inspect GitHub REST + GraphQL rate-limit buckets (both separately) | `shipyard doctor --rate-limit --json` |
| Inspect effective GitHub auth only | `shipyard auth doctor --json` |
| Export/import GitHub auth config only | `shipyard auth export --output shipyard-auth.toml` / `shipyard auth import shipyard-auth.toml --scope local` |
| Clean up artifacts | `shipyard cleanup --apply` |
| Wait for a release to fully upload | `shipyard wait release v0.23.0 --timeout 900 --json` |
| Wait for a PR's required checks to go green | `shipyard wait pr 151 --state green --timeout 1800 --json` |
| Wait for a workflow run to finish | `shipyard wait run 223344 --success --timeout 1200 --json` |
| Mark a target advisory | `[targets.<n>] advisory = true` in `.shipyard/config.toml` (see "Advisory lanes" below) |
| Flip lane policy for one PR | `Lane-Policy: <target>=required\|advisory` trailer on the tip commit |
| List quarantined targets | `shipyard quarantine list --json` |
| Quarantine a flaky target | `shipyard quarantine add <target> --reason "..."` |
| Remove from quarantine | `shipyard quarantine remove <target>` |

## GitHub Auth Diagnostics

Before blaming ambient `gh auth status`, check whether the repo config has
`[github.auth]`. Shipyard can inject env or command-helper tokens into its
built-in `gh` subprocesses as `GH_TOKEN`, including helpers that mint GitHub
App installation tokens. `shipyard doctor --rate-limit --json` reports the
effective source and rate-limit buckets. For GitHub App or fine-grained tokens,
permissions may not be locally inspectable, so verify Actions: Read and write
on the token/App when cloud retarget or handoff fails with auth/scope errors.
That doctor command actively resolves configured auth, so command helpers may
run and GitHub App helpers may mint installation tokens.

GitHub App installation tokens are the preferred path for high-volume
inspection because Shipyard injects them into its built-in `gh` subprocesses
and REST/GraphQL fallback paths. Do not silently fall back to ambient user auth
for polling, watch, retarget, handoff, or diagnostics. The narrow exception is
pull-request creation: if GitHub rejects App-token PR creation with `Resource
not accessible by integration` through both GraphQL and REST, Shipyard may print
an explicit notice and use ambient `gh` auth for that one low-volume create
operation.
PR merge should stay on the configured token: if GitHub rejects the App token's
GraphQL merge probe, Shipyard falls back to its REST merge path with the same
configured token.

## Supervised-Push Signal (`SHIPYARD_PR_RUNNING=1`)

Every `git` / `gh` subprocess spawned by `shipyard pr` / `ship` /
`auto-merge` / `overflow` / `wait` runs with `SHIPYARD_PR_RUNNING=1`
in its environment. Consumer-side pre-push hooks (notably
[`danielraffel/pulp#1406`](https://github.com/danielraffel/pulp/pull/1406))
use this to differentiate a Shipyard-orchestrated push (full local
validation, version-bump gate, etc.) from a raw `git push` that
bypasses those gates and turns CI into the discovery channel.

Quick smoke from a checkout that wants to verify the hook side:

```sh
SHIPYARD_PR_RUNNING=1 git push --dry-run    # what shipyard pr looks like to the hook
unset SHIPYARD_PR_RUNNING ; git push --dry-run    # what a raw push looks like
```

The marker is set inside `src/supervised.rs` and routed through
every supervised spawn site. Diagnostic subcommands (`doctor`,
`pin`, `runner`, `cleanup`) intentionally do not set it. See
`skills/shipyard/SKILL.md` → "Supervised Subprocess Marker" for the
helper API.

## Runner Provider Defaults

Shipyard's own workflows default to GitHub-hosted runners for Linux, macOS, and
Windows. Namespace is optional and account-dependent; do not assume `nsc` or
Namespace capacity is available. If a workflow or repo variable still points at
Namespace during an outage/account-expired period, set
`DEFAULT_RUNNER_PROVIDER=github-hosted` or pass `-f runner_provider=github-hosted`.

Explicit `*_runner_selector_json` workflow-dispatch inputs can still route
trusted jobs to self-hosted GitHub Actions runners, such as a local Mac or SSH
VM fleet. Do not add hidden repo-variable fallbacks that silently override the
GitHub-hosted default; a trusted self-hosted run should be an explicit per-run
choice. GitHub dispatches by `runs-on` labels; SSH is only the management layer
for those machines.

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

**Idle behavior (v0.56.0+):** when no IPC subscriber is attached
(no `shipyard watch` running, no GUI), the daemon skips the
periodic `gh` reconcile poll. Webhooks still update state in real
time, so correctness is unchanged — the daemon just doesn't burn
GitHub REST budget for ticks no one is watching. The reconcile
resumes the moment a subscriber attaches. Webhook registration
also retries on a 5-minute backoff after failure rather than every
loop iteration.

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
- `auto-merge` and `wait pr` auto-degrade to REST when GraphQL is
  rate-limited. `gh pr merge` and `gh pr view --json` (used internally)
  call GraphQL for the mergeable-state probe; if either fails with
  `GraphQL: API rate limit already exceeded`, Shipyard falls back to
  `PUT /repos/:r/pulls/:n/merge` (auto-merge) and `GET /repos/:r/pulls/:n`
  + `GET /repos/:r/commits/:sha/check-runs` (wait pr) directly. REST
  has its own 5000/hr bucket, separate from GraphQL. Agents do not
  need to hand-roll `gh api` calls anymore. Check both buckets with
  `shipyard doctor --rate-limit --json`. The REST `wait pr` fallback
  is conservative — all check runs are treated as required, so a
  green verdict cannot incorrectly fire when non-required checks
  fail. Snapshot output carries `_rest_fallback: true` when the
  fallback path served the value.

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

When a provider change would be valuable *during* an in-flight PR drain — e.g., you need to move a lane from an unavailable paid pool back to GitHub-hosted — use `shipyard cloud retarget`:

```sh
# Preview first (dry-run by default):
shipyard cloud retarget --pr 224 --target macos --provider github-hosted

# Apply when the plan looks right:
shipyard cloud retarget --pr 224 --target macos --provider github-hosted --apply
```

What it does:
1. Finds the PR's latest workflow run.
2. Cancels the **one job** matching `--target` on the old provider (substring match on the job name, e.g. `macos` matches `macOS (ARM64) [github-hosted]`). If every active job in the run matches that target, Shipyard can safely fall back to cancelling the whole run.
3. Dispatches a fresh workflow run with the new provider.

Cancellation failures are fail-closed. If GitHub denies or cannot find the
job/run, Shipyard does **not** dispatch a replacement. It reports
`event=cancel_failed`, classifies the failure (`auth`, `scope`, `not_found`,
`unsupported`, `transient`, `unknown`), includes the run/job URLs, and prints
manual recovery steps. Do not treat a standalone `workflow_dispatch` as an
equivalent fallback unless the workflow/check integration is known to satisfy
the same required PR check context.

**Known limitation (read before running):** step 3 starts a new workflow run, so targets other than the one you retargeted will also re-run in that new run. Their *prior* pass/fail statuses persist on the PR's check rollup, and pulp-style `resolve-provider` matrix workflows reuse caches — so the net effect is "flip the lane" without losing ground on the other lanes, even though they technically re-execute.

## Mid-flight lane addition

Sibling to retarget. Use when a ship is already in flight and you realize you want to validate against an *additional* platform without cancelling and re-dispatching the whole matrix — e.g., you started with `[macos, linux]` and want to add `windows`:

```sh
# Preview (dry-run by default):
shipyard cloud add-lane --pr 224 --target windows

# Apply when the plan looks right:
shipyard cloud add-lane --pr 224 --target windows --provider github-hosted --apply
```

What it does:
1. Loads the PR's ShipState. Refuses if absent (no in-flight ship) or terminal (merge already issued).
2. Idempotent: if the target is already in `dispatched_runs`, reports a no-op and does nothing.
3. Dispatches the single workflow for that target/provider.
4. Appends a new `DispatchedRun` to the ShipState so the watch loop joins it into the overall verdict.

See `docs/cloud-retarget.md` for full context; add-lane complements retarget.

## Rescuing wedged runners (`shipyard rescue`)

Use this when a self-hosted runner has wedged — orphaned `Runner.Worker`
process, queued runs sitting >30m, repo PRs all in
`mergeable_state=blocked` — and you need to move the work to a different
provider in one shot:

```sh
# Most common case: one PR is stuck. Rescue it (default target is github-hosted):
shipyard rescue 286

# Preview without acting:
shipyard rescue 286 --dry-run

# Also re-arm runs a watchdog sweep marked failed/cancelled, then hand them off:
shipyard rescue 286 --rerun-failed

# Repo-wide: rescue every queued run older than 30m:
shipyard rescue --all-stuck

# Override the queue-age threshold or destination provider:
shipyard rescue 286 --threshold 10m --to github-hosted
```

What it does:
1. Resolves the PR's head branch (skipped under `--all-stuck`).
2. Lists queued workflow runs and filters to (a) the PR's branch and (b) ones older than `--threshold` (default `30m`).
3. With `--rerun-failed`, additionally pulls `status=completed conclusion=cancelled` runs on that branch — these get `gh run rerun --failed` first, then the same cancel+redispatch handoff.
4. For each candidate, cancels the existing run and dispatches a fresh one with `--to <provider>` as the provider override.
5. Emits a per-run summary (`applied`, `rerun+applied`, `planned`, `skipped-completed`, `skipped-no-plan`, `failed`) with a top-level `event=cloud.rescue` JSON envelope under `--json`.

**Do not reach for `runner-watchdog.sh --fix` instead of `shipyard rescue`.**
The watchdog's cancellation registers as required-check `failure` on the PR
without redispatching — it makes the wedge look terminal to branch
protection. `shipyard rescue` is the safe primitive because it cancels +
redispatches atomically under one transaction; no orphaned `failure`
contexts, no destructive ops on the runner host itself.

`shipyard rescue` is the discoverable surface for what was previously a
5-step recipe (`gh api` + `cloud handoff list-stuck` + per-run
`cloud handoff run --apply`). Both `cloud handoff list-stuck` and
`cloud handoff run` remain available for cases where you need to operate
on a specific run ID outside the PR-scoped flow.

### Preventing wedges: `runner watch --kill-hung-workers`

`shipyard rescue` recovers from a wedge after the fact. The companion
preventive surface is the auto-kill mode of `runner watch`:

```sh
# Daemon mode that auto-cancels stale queued runs AND auto-kills hung Workers
# whose etime exceeds the watchdog threshold (default 90 min):
shipyard runner watch --kill-hung-workers

# Adjust the threshold (e.g. for long-running iOS builds):
shipyard runner watch --kill-hung-workers --interval 300
```

What it does on every tick (default every 5 min):

1. Calls the same `assess_runner` logic `runner status` uses.
2. If `Symptom::HungWorker` fires, enumerates local `Runner.Worker`
   processes via `ps`, finds those whose etime exceeds the
   `runner.watchdog.max_job_min` threshold, and invokes the same
   recovery sequence as `shipyard runner kill --pid <pid> --yes`:
   snapshot → SIGTERM → grace → SIGKILL → reap children → quarantine
   partial builds → verify `Runner.Listener` → optionally wait for
   GitHub status to flip.
3. `--fix` is implied — stale queued runs are cancelled in the same
   tick so neither the host process nor the Actions side is left
   wedged.
4. Emits `runner.watch` JSON envelopes with `event=auto_kill_worker`
   and per-PID `phase` ∈ {`attempt`, `killed`, `failed`,
   `no-pid-found`} under `--json`.

Run it as a launchd/systemd service for prevention; pair with
`shipyard rescue <pr>` for the after-the-fact PR rescue path. Together
they replace the legacy `runner-watchdog.sh --fix` workflow that today
masks wedges as required-check failures.

## Waiting on conditions (`shipyard wait`)

Whenever you'd otherwise write a polling loop around `gh` — wait for a release to upload, wait for a PR's required checks to go green, wait for a dispatched workflow run to finish — reach for `shipyard wait` instead. It opens a daemon subscription first (if one's running), takes one authoritative `gh` snapshot, and either exits 0 immediately or keeps re-evaluating on real webhook events (no extra REST budget). When the daemon isn't running, it falls back to polling transparently — safe to use on headless CI too.

### Before/after

| Before | After |
|---|---|
| `for i in {1..60}; do status=$(gh run view 22345 --json status -q .status); [ "$status" = "completed" ] && break; sleep 20; done` | `shipyard wait run 22345 --success --timeout 1200 --json` |
| `while ! gh release view v0.23.0 --json assets -q '.assets\|length' \| grep -q '^5$'; do sleep 10; done` | `shipyard wait release v0.23.0 --timeout 900 --json` |
| `gh pr checks 151 --watch` (blocking; no structured output) | `shipyard wait pr 151 --state green --timeout 1800 --json` |

### Detection gate (when to use it vs hand-rolled `gh`)

Only use `shipyard wait` when:

1. `command -v shipyard` succeeds (binary is installed).
2. The project has `.shipyard/config.toml` **or** `tools/shipyard.toml` (i.e. opted in to Shipyard).

If either check fails, fall back to `gh run watch` / `gh pr checks --watch`.

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | condition matched |
| 1 | `--timeout` elapsed |
| 4 | `wait run --success` reached a terminal-but-wrong conclusion |
| 5 | invalid input (PR/release/run not found, bad tag) |
| 6 | daemon unreachable + snapshot didn't match + `--no-fallback` |
| 7 | unsupported scope — rulesets / merge-queue governance detected; switch lanes or do it manually |
| 130 | SIGINT / SIGTERM |

### JSON shape

```json
{
  "schema_version": 1,
  "command": "wait:pr",
  "matched": true,
  "condition": {"type": "pr_green", "pr": 151, "repo": "owner/repo", "head_sha": "f521fa9b"},
  "observed": {
    "checks": [{"name": "Linux", "conclusion": "SUCCESS", "required": true}],
    "advisory": []
  },
  "transport": "daemon",
  "fallback_used": false,
  "events_received": 3,
  "elapsed_seconds": 12.4
}
```

Branch on `matched` + `transport`. `transport == "daemon"` means a webhook woke the wait; `transport == "polling"` means the daemon wasn't reachable and you got the fallback (which is fine — still correct, just slower).

### Always set `--timeout`

Unbounded waits in an agent workflow hang sessions. Pick a realistic ceiling (10–30 minutes for most checks, longer for a full release). The flag is required in practice even though the CLI has a default.

See `docs/waiting.md` for the full reference: subcommand semantics, event sources, fallback contract, and the rulesets-unsupported caveat.

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

### Local Mac capacity

For simple two-Mac capacity, use explicit ordered fallback:

```toml
[targets.mac]
backend = "ssh"
host = "mac-studio"
platform = "macos-arm64"
repo_path = "/Users/shipyard/work/shipyard"
warm_keepalive_seconds = 1800

fallback = [
  { type = "local", cwd = "/Users/danielraffel/Code/shipyard" },
]
```

This makes Mac Studio the first backend tried for macOS work, then falls back
locally only for infrastructure failures. Real test failures remain
authoritative.

For named members and lease visibility, use `backend = "host-pool"` with
explicit `[host_pools]` members, then inspect with
`shipyard targets pool status`. Stale lease records can be pruned with
`shipyard targets pool cleanup --fix`. Host-pool targets can drain multiple
non-conflicting queued jobs across available members under one local drain
owner; jobs still serialize when they claim the same checkout, PR state,
evidence lane, or exhausted pool capacity. Use `shipyard targets test mac` and
then `shipyard run --targets mac` when bringing the Mac Studio online. See
`docs/local-mac-pool.md`.

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
- A row in the run summary that reads `<target>   error   ssh` prints the underlying backend error on the following indented line (`✗ <target>: Bundle apply failed: …` plus the log path). `shipyard targets test` exercises only `ssh <host> echo ok` — it does *not* run bundle create/upload/apply or the remote validation command, so a probe pass does not imply `run`/`pr` will succeed. When the error line says `Bundle apply failed` / `Bundle upload failed`, inspect the per-target log first; the probe's "reachable" verdict is a prerequisite, not a guarantee.
- If a target is unreachable with no fallback, `run` / `ship` / `pr` exit **3** (distinct from 1 validation-failed and 2 config-error) with a message that names the target, the failure category (`auth`, `host_key`, `network`, `timeout`, `unknown`), and the last ssh error.
- `shipyard run --allow-unreachable-targets --json` — proceed with the lane **SKIPPED, NOT validated**. The warning is loud by design because muscle-memory use of this flag (Pulp pre-2026-04-20) hid real backend outages.
- `shipyard run --skip-target <name>` — **deliberately** skip a lane (no probe run). Use this when you already know you don't want to validate the target — `--allow-unreachable-targets` is for "I want this target, but the backend is down right now."
- `shipyard cloud defaults --json` — inspect the current cloud workflow/provider dispatch plan

## Shipping a PR (the `shipyard pr` path)

When the user says "push a PR", "ship this", "ship it", "we're done", "merge this", or "push it" — run `shipyard pr` (or the `/pr` slash command — see `commands/pr.md`). It wraps `shipyard ship` with the versioning gates: skill-sync check, version-bump apply, and a `chore: bump versions` commit before handing off to the push/PR/validate/merge flow.

The orchestration, in order:

1. `skill_sync_check.py --mode=report` — hard-fails if a mapped path was touched without a `SKILL.md` update or a `Skill-Update:` trailer on the tip commit.
2. `version_bump_check.py --mode=apply` — rewrites `Cargo.toml` for CLI-surface bumps and `.claude-plugin/plugin.json` for plugin-surface bumps. The two version streams are independent per `RELEASING.md`.
3. `git commit` + `gh pr create` + `shipyard ship`.
4. On merge, `.github/workflows/auto-release.yml` tags the CLI bump as `v<x.y.z>`. The existing tag-triggered `release.yml` builds the 5-platform binaries and publishes the GitHub Release.

Never run `gh pr create` + release separately. Never run the gate scripts by hand.

### Gate-script path resolution

`shipyard pr` looks up each gate script in this order — the first hit wins:

1. Env var (`SHIPYARD_SKILL_SYNC_SCRIPT`, `SHIPYARD_VERSION_BUMP_SCRIPT`, `SHIPYARD_VERSIONING_CONFIG`).
2. `.shipyard/config.toml` `[validation]` keys (`skill_sync_script`, `version_bump_script`, `versioning_config`).
3. `tools/scripts/<file>` — common CI-tooling layout (used by Pulp).
4. `scripts/<file>` — Shipyard's own default.

Missing-script errors list every probed location and every override knob. Consumer repos that keep their tooling under `tools/scripts/` need no configuration; other layouts should set the env var or the `[validation]` key rather than moving the script.

## Consumer-repo pin bumps (`shipyard pin bump`)

Consumer repos (pulp, spectr, …) pin a specific Shipyard release via `tools/shipyard.toml` and install it through `./tools/install-shipyard.sh`. `shipyard pin bump` is the one-shot: it rewrites the pin, runs the installer, verifies `shipyard --version` matches, and opens the PR.

**Mental model for multi-worktree / multi-project setups:** just run `shipyard pin bump` in whichever consumer worktree is most up-to-date. Don't hand-edit `tools/shipyard.toml` — the command's guards are what keep you out of trouble. Two refuse-by-default guards fire before any side effect:

1. **Downgrade refusal** — if the target is older than the currently-installed `shipyard` binary (the `~/.local/bin/shipyard` that `install-shipyard.sh` will overwrite), the command refuses. The common trigger is running this in a stale worktree that still pins an old version. Remediation: rebase onto main, or pass `--allow-downgrade` if you really do mean to regress the global.
2. **Redundant-branch refusal** — if `origin/main:tools/shipyard.toml` already pins a version >= the target, the command refuses. Trigger: branch is behind main; opening a PR here produces a no-op at merge time or a conflict. Remediation: rebase/merge `origin/main`, or pass `--allow-redundant`.

Both guards are skipped silently when their inputs are unavailable (no `shipyard` on PATH, offline, no `origin/main`) — advisory, not load-bearing.

`shipyard pin show` reports the current pin and the latest upstream release without touching anything — safe to run anywhere.

## State-machine lane + doc-sync gate

A dedicated Rust test suite exercises ship-state transitions under `cargo test --all-targets --locked`. Failures show up in the cross-platform test matrix and the coverage gate.

A doc-sync gate enforces that `docs/ship-state-machine.md` moves whenever the mapped Rust ship-state or command modules change. Mechanism is `scripts/doc_sync_check.py` + `scripts/doc_sync_map.json` (mirrors `skill_sync_check.py` but targets free-form docs). Bypass via `Doc-Update: skip doc=<path> reason="..."` trailer.

## Bypass trailers (tip commit)

| Gate          | Trailer                                                      |
|---------------|--------------------------------------------------------------|
| Version bump  | `Version-Bump: <surface>=<patch\|minor\|major\|skip> reason="..."` |
| Skill update  | `Skill-Update: skip skill=<name> reason="..."`              |
| Doc-sync      | `Doc-Update: skip doc=<path> reason="..."`                  |
| Auto-release  | `Release: skip reason="..."`                                 |
| Lane policy   | `Lane-Policy: <target>=required\|advisory` (escalate/demote for this PR only) |

**`Version-Bump` is authoritative when set.** The override wins against both the path-based heuristic and the conventional-commit subject ceiling. If you want a bug fix to ship as `cli=patch` even though it touches many public-API files, write `Version-Bump: cli=patch reason="bug fix"` — the trailer is the author's explicit accountability, and the reason string is reviewable. Two escape hatches stay in place: `skip` zeroes the level, and an override on a surface that wasn't actually touched is ignored (no rubber-stamping unrelated bumps).

**Gotcha:** anything under `.github/workflows/**`, `.claude-plugin/**`, `commands/**`, `agents/**`, `hooks/**`, `scripts/release.sh`, `scripts/ci_matrix.py`, release packaging scripts, or `src/**` triggers the `ci` skill's path map (`scripts/skill_path_map.json`). Update this SKILL.md in the same PR — or use the `Skill-Update: skip` trailer with a real reason.

**Manual release fallback:** `./scripts/release.sh` still exists for emergencies but is no longer the happy path. Normal releases flow through `shipyard pr` → merge → auto-release workflow.

**`RELEASE_BOT_TOKEN` is required for the auto-release chain to fire.** Without it, auto-release silently degrades — tags get created via `GITHUB_TOKEN` but GitHub doesn't trigger workflows on `GITHUB_TOKEN`-pushed tags, so `release.yml` never runs and no binaries ship. Run `shipyard doctor` to check; if the secret is missing, follow the "One-time setup" section in `RELEASING.md`. `shipyard pr` will also print a heads-up before pushing the PR if the secret isn't present.
