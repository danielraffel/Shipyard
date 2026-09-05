# Runner watchdog

`shipyard runner` detects and (optionally) auto-recovers a self-hosted
GitHub Actions runner that has gotten itself stuck.

## Why

On 2026-05-12 a Pulp self-hosted runner sat busy on a UBSan job from a
closed branch for >75 minutes while 17 stale queued runs piled up behind
it. A critical-path PR was blocked for hours before a human noticed. The
watchdog is the structural fix: detect the symptoms automatically, report
them, and offer a guarded recovery path.

## Symptoms it catches

| Symptom | Detection | Default action |
|---|---|---|
| `orphaned_busy` | API reports `busy=true` but no `Runner.Worker` process is visible locally | Report only (clears in 1-5 min) |
| `hung_worker` | A `Runner.Worker` has been running longer than `max_job_min` | Report on `status`; terminate with full recovery via `runner kill --pid <pid>` |
| `stale_queued_runs` | Queued runs older than `max_queue_age_hours` | Report on `status`; cancel on `cleanup --fix` |
| `hung_in_progress` (run-level) | A workflow run stuck `in_progress` past `reap_in_progress_max_min` | Cancel on `runner watch --reap-stale-runs` |
| `orphaned_queued` (run-level) | A workflow run stuck `queued` past `reap_queued_max_min` | Cancel on `runner watch --reap-stale-runs` |

## Subcommands

### `shipyard runner status`

One-shot health check. Exit codes:

- `0` runner healthy, no symptoms
- `1` runner online but at least one symptom detected
- `2` runner offline / API unreachable

```bash
shipyard runner status                              # uses config defaults
shipyard runner status --runner-id 1763             # explicit override
shipyard runner status --max-queue-age-hours 4      # widen the queue cutoff
shipyard runner status --json                       # structured output
```

### `shipyard runner cleanup`

Lists stale queued runs. Default is `--dry-run`; pass `--fix` to actually
cancel them via `POST /actions/runs/<id>/cancel`. Exits non-zero when
stale runs are found in dry-run mode (matches the prototype script's
contract, so cron consumers see drift).

Repository-wide cleanup is fail-closed to `pull_request` and `merge_group`
runs. `Release CLI` and `Sign and Release` are never candidates, matched by
both workflow name and filename. Push, schedule, tag, and `workflow_dispatch`
runs require an exact-run operation instead of broad age-based cleanup.
Protected stale runs remain visible in status and dry-run reports; protection
limits mutation authority, not observability.
Human output labels each run `cancellable` or `protected: not cancellable`;
JSON includes `cancellation_safe` per run and `protected_run_ids` in cleanup
results.

```bash
shipyard runner cleanup                             # dry-run, prints stale ids
shipyard runner cleanup --fix                       # cancel them
shipyard runner cleanup --stale-hours 4 --fix
shipyard runner cleanup --json --fix                # structured cancel report
```

`--force-kill` is the original advisory flag and is retained for
backwards compatibility. It requires `--fix` and two confirmation
prompts (`y` then the literal word `KILL`); on non-TTY stdin it is
ignored unless `--yes` is also passed. The current implementation **does
not** actually terminate the process — it prints diagnostic guidance and
points users at `shipyard runner kill` (below), which has a full
recovery sequence baked in. Direct silent worker-kill from `cleanup`
would still risk corrupting in-flight build artifacts; the explicit
subcommand is the safe path.

### `shipyard runner kill`

Explicit `Runner.Worker` termination with a full recovery sequence.
Unlike `cleanup --force-kill`, this subcommand actually sends signals
— but every kill is preceded by a snapshot to
`~/.shipyard/kill-recovery.jsonl`, escalates `SIGTERM` → `SIGKILL` only
after a 10 s grace period, reaps orphaned `cmake`/`ninja`/`make`/`ctest`
children, **moves** (does not delete) any matching partial `build*`
directories from `_work/` to `/tmp/shipyard-killed-builds/<event-id>/`,
verifies that `Runner.Listener` is still alive, and waits for GitHub to
recognise that the run has flipped to `completed`. `--retrigger` then
re-queues the killed PR's CI via
`POST /actions/runs/<id>/rerun-failed-jobs`.

```bash
shipyard runner kill --pid 59996 --reason "wedged on agentB/81"
shipyard runner kill --pid 59996 --reason "..." --retrigger
shipyard runner kill --pid 59996 --reason "..." --yes       # skip prompt
shipyard runner kill --history                              # review past kills
shipyard runner kill --history --last 5
shipyard runner kill --recover kill-59996-deadbeef          # restore quarantine
```

Required flags:

- `--pid <pid>` — Worker PID. Sanity-checked against
  `Runner.Worker` + the configured `runner_dir` before any signal is
  sent. Refusing to kill an unrelated process is the first guardrail.
- `--reason "<text>"` — free-text reason. Stored in the recovery log so
  the audit trail tells future-you why you did this.

Optional flags:

- `--retrigger` — after the GitHub run flips to `completed/failure`,
  call `rerun-failed-jobs` on the same run id so the killed PR's CI
  starts immediately. The recovery log records `retriggered: true` (or
  `retrigger_error` if the API call failed).
- `--yes` — skip the typed `KILL` confirmation. Intended for scripted
  use after a human has already invoked the command interactively at
  least once.
- `--history [--last N]` — print the recovery log as a human table,
  most recent first.
- `--recover <event-id>` — restore the quarantined build for a prior
  kill event back to `_work/`. Skips destination paths that already
  exist (so a re-run that produced a fresh build will not be
  clobbered). If `--retrigger` was not used at kill time, `--recover`
  will also issue `rerun-failed-jobs` so the recovered build has a CI
  run to attach to.

Hidden test hooks (`--grace-secs`, `--recovery-log`,
`--quarantine-root`, `--no-wait-github`) exist so the integration test
suite can drive the flow against a synthetic process and ephemeral
filesystem paths.

#### Recovery sequence (the 10-step flow)

1. **Snapshot** — append a JSONL line to `~/.shipyard/kill-recovery.jsonl`
   capturing pid, reason, PR, job, branch, etime, `_work` dir, GitHub
   run id, and a per-kill `id` (`kill-<pid>-<unix-nanos-hex>`).
2. **Confirmation** — require typed `KILL` (not `y`/`yes`) unless
   `--yes` was passed.
3. **SIGTERM** — send `kill -TERM <pid>` and poll `ps -p <pid>` every
   500 ms for up to `--grace-secs` (default 10 s).
4. **SIGKILL** — only if the worker is still alive after the grace
   window. The recovery log records `signal: SIGKILL`.
5. **Reap orphans** — `pkill -P <pid> -f 'cmake|ninja|make|ctest|build'`.
6. **Quarantine partial builds** — move any `build*` directory under
   `_work/` whose mtime is within `etime_min + 5` minutes of `now` to
   `/tmp/shipyard-killed-builds/<event-id>/`. Never deletes.
7. **Verify Runner.Listener** — `pgrep -f Runner.Listener`. If absent,
   the summary prints restart guidance (`svc.sh restart` / `run.sh`).
8. **Wait for GitHub status flip** — poll `GET /actions/runs/<id>`
   every 2 s for up to 90 s, waiting for `status = completed`.
9. **Optional retrigger** — `POST /actions/runs/<id>/rerun-failed-jobs`
   if `--retrigger` is set.
10. **Summary** — print a multi-line recovery summary that ends with the
    `--recover` invocation needed to undo this kill.

#### Manual recovery (without `--recover`)

If `--recover` is unavailable for some reason, the quarantine path is
deterministic:

```bash
ls /tmp/shipyard-killed-builds/<event-id>/
# Move directories back manually:
mv /tmp/shipyard-killed-builds/<event-id>/build* ~/actions-runner/_work/<repo>/<branch>/
# Re-queue CI:
gh api -X POST repos/<owner>/<repo>/actions/runs/<run_id>/rerun-failed-jobs
```

The recovery log entry stores `worker_dir`, `github_run_id`, and
`quarantine_dir`, so the values are easy to recover via `jq` over the
JSONL file.

### `shipyard runner watch`

Polling daemon. Defaults to the `runner.watchdog.watch_interval_seconds`
config value (300 s). Logs one line per tick. With `--fix`, cancels stale
queued runs every tick.

```bash
shipyard runner watch
shipyard runner watch --interval 60 --fix
shipyard runner watch --json   # NDJSON-style structured ticks
```

The loop never exits on its own; press Ctrl-C or run it under
`launchd` / `systemd` for unattended operation. A hidden
`--max-iterations N` flag exists for tests.

When `[host_class.*]` is configured, every watchdog tick also runs the
read-only fleet/merge-queue observation by default. This remains active when an
interactive agent exits or reaches a usage limit. Set
`runner.watchdog.fleet_liveness = false` only when another durable scheduler
owns it; use `fleet_liveness_every_ticks = N` to reduce its cadence. The
monitor resolves the repository default branch automatically. Override it with
`shipyard runner watch --fleet-base <branch>` or
`runner.watchdog.fleet_base = "<branch>"`.

Metal and planned machines that are not Tart host classes can be declared as
expected fleet inventory. Each entry matches registered runners by a
case-insensitive required-label subset and defaults to one required online
runner:

```toml
[runner.fleet.expected_host.macmini]
labels = ["self-hosted", "macOS", "X64", "pulp-host-macmini"]

[runner.fleet.expected_host.future_host]
active = false
labels = ["self-hosted", "Linux", "ARM64", "pulp-host-future"]
```

An active missing/offline host makes fleet liveness unhealthy. An inactive
entry is reported for planning visibility without alerting.

#### `--reap-stale-runs` — repo-wide stale-run reaper

`--kill-hung-workers` reaps hung *processes* on the runner host;
`--reap-stale-runs` reaps stale GitHub Actions *workflow runs* repo-wide.
On every tick it lists the repo's runs and cancels:

- runs stuck `in_progress` longer than `--reap-in-progress-max-min`
  (default ~5h — "hung", e.g. a `Coverage` run squatting until GitHub's
  6h timeout); and
- runs stuck `queued` longer than `--reap-queued-max-min` (default ~8h —
  "orphaned", e.g. a run waiting on a runner label that no longer exists,
  which never hits any `timeout-minutes`).

Both thresholds are deliberately well past any healthy run, so an
in-flight validation run is never cancelled. Unlike host-process reaping,
this also covers runs on **GitHub-hosted** runners.

The reaper only considers `pull_request` and `merge_group` runs and always
protects `Release CLI` and `Sign and Release`; other event types are outside
the authority of repository-wide age-based cleanup.

```bash
# Auto-cancel stale runs on every tick:
shipyard runner watch --reap-stale-runs

# Preview only — log what would be cancelled, cancel nothing:
shipyard runner watch --reap-stale-runs --dry-run --json

# Tighter thresholds (minutes):
shipyard runner watch --reap-stale-runs \
  --reap-in-progress-max-min 240 --reap-queued-max-min 360
```

With `--json`, each candidate emits a `runner.watch` envelope with
`event=reap_stale_run` and `phase ∈ {attempt, cancelled, failed,
skipped}` (`skipped` means dry-run or protected by cancellation policy) — mirroring the
`event=auto_kill_worker` envelopes from `--kill-hung-workers`.

Cancellation goes through the GitHub REST API
(`POST /repos/{owner}/{repo}/actions/runs/{id}/cancel`), the same path
`runner cleanup --fix` and `shipyard rescue` use.

## Configuration

Defaults live in `.shipyard/config.toml`:

```toml
[runner.watchdog]
runner_id = 1763
runner_dir = "/Users/runner/actions-runner"
max_job_min = 90
max_queue_age_hours = 2
watch_interval_seconds = 300
auto_fix = false
# Stale-run reaper thresholds (minutes), used by
# `runner watch --reap-stale-runs`:
reap_in_progress_max_min = 300
reap_queued_max_min = 480
```

Per-machine overrides go in `.shipyard.local/config.toml` and follow the
standard Shipyard layered-config rules.

Every command-line flag wins over config; config wins over the built-in
defaults (`max_job_min=90`, `max_queue_age_hours=2`,
`watch_interval_seconds=300`, `reap_in_progress_max_min=300`,
`reap_queued_max_min=480`).

## Lessons learned (from the prototype)

- Stale queued runs from 5+ hours ago can sit forever and monopolize the
  runner when they eventually get FIFO'd in.
- A worker PID staying alive 1-5 min after `gh run cancel` is normal —
  the runner takes time to honour graceful shutdown. Don't treat that as
  a symptom on its own.
- `concurrency: cancel-in-progress: true` on a workflow *should*
  auto-cancel duplicate runs on force-push, but doesn't always (see Pulp
  issue #1884).
- Auto-killing the Worker process is too risky to wire silently from
  `cleanup --fix`; `--force-kill` deliberately stops short of `kill -9`
  and points at the explicit `shipyard runner kill` subcommand instead.
- The kill subcommand never deletes work — partial builds move to
  `/tmp/shipyard-killed-builds/<event-id>/` so a misclick is recoverable
  with `--recover`.

## Fleet service assertions — asserting service, not liveness

`src/fleet_service.rs` answers a different question from the rest of this
document. The watchdog above asks *"is this runner wedged?"*. A service
assertion asks *"is anything actually serving this lane?"* — and the two come
apart badly, because a host can be perfectly up and serving nothing.

That is not hypothetical: a Linux lane once went unserved for ~19 days while
`systemctl status` reported the pool `active (running)` the entire time (it
was — and failing every 30 seconds, 36,088 times), the required gate kept
merging, and a GitHub-hosted fallback absorbed the work so the lane's *output*
stayed green. An uptime ping would have called it healthy every minute of it.

### Verdicts are typed, because the distinctions are the point

| Verdict | Means |
| --- | --- |
| `Served` | demand can be satisfied — proven, not assumed |
| `Idle` | nothing registered **and** nothing asking; a just-in-time pool at rest |
| `Degraded` | serving, but consuming a budget (latency, blind cycles, a climbing restart counter) |
| `Starved` | demand exists and an online server exists, but the demand is not being reached |
| `Unserved` | declared local, served by nobody: aged demand and no online runner in **either** scope |
| `Unknown` | the instrument could not measure — never folded into a pass |

`Unserved` vs `Idle` is only decidable by pairing the runner census with queued
demand; an empty census alone cannot tell a dead pool from a resting one.
`Unserved` vs `Starved` matters because the remedies are opposite — one is a
routing fix, the other a capacity fix. Verdicts are ordered by severity, so
`roll_up` takes the worst; `roll_up(&[])` is `Unknown`, since asserting nothing
is not the same as asserting everything passed.

### Query both runner scopes, always

`repos/{owner}/{repo}/actions/runners` **omits org-registered runners
entirely**. On the fleet this was written against, three of six declared
self-hosted lanes are served only by org-scope runners — so a repo-scope-only
census reports them unserved while they are online, which is the identical
empty reading it gives when a host is genuinely dead.

`assess_lane_service` takes one census spanning both scopes and records which
scope satisfied each lane (`LaneReport::served_only_by_org_scope`), so this
blindness is visible in the output instead of silently changing the answer.

### Routing values have three encodings

`parse_runs_on` handles all of them, because a parser that assumes a JSON array
drops lanes without saying so:

```
["self-hosted","macOS","ARM64","pulp-build"]   JSON array  -> SelfHosted
"macos-15"                                     JSON string -> Hosted
macos-15                                       bare string -> Hosted
local-only                                     sentinel    -> names no runner
```

### A refusal must name its boundary

`Unknown` always carries a `Boundary`, and no measured verdict carries one. The
facts it separates otherwise collapse into a single opaque failure — the same
defect as a supervisor logging `self-restarting for fresh gh auth` for what was
a *timeout*, then performing an auth restart that could not possibly help.

`Grammar` (a command wrapper refused the verb), `Scope` (asked where the answer
is invisible), `Identity` (wrong principal), `Permission`, `Parse`, and
`Transport` (timeout or rate limit — explicitly *not* an auth fault). Only
`Permission` denies that an equivalent path exists; for the rest,
`Boundary::next_action` names what to try instead, so a caller does not stop at
a wall that has a door in it.

Design note: `planning/2026-09-04-fleet-service-assertions.md`.

### Supervisor scan blindness is a ratio, never a sample

`src/fleet_supervisor.rs` asserts that a tartci supervisor can *see* the work.
A supervisor decides whether to boot a VM by scanning the queue; when that scan
cannot finish in time it logs `SCAN BLIND (gh queue scan failed) N/9`, boots
nothing, and every job on that lane's labels waits with no error visible
anywhere in GitHub. The jobs are simply `queued`.

**Do not sample it.** A release supervisor measured on this fleet had **1598 of
its last 2000 log lines blind** against 80 sighted, with the final ~70 lines
reading perfectly healthy. Any single-sample check taken at that moment returns
green. `assess_supervisor_scan` therefore reports the blind/sighted **ratio over
a window** against `max_blind_ratio`, alongside the supervisor's own
consecutive-blind counter (`N/9`) against its ceiling — a burst of nine
consecutive is a different fault from nine scattered, and both are tracked.

There is deliberately **no minimum-window guard**. A "only trust the ratio after
N cycles" threshold is an escape hatch that lets a short, entirely blind window
read as a pass, which is a blind spot inside a blindness detector.

### A timeout is not an auth failure

The supervisor's own message for a timeout is `self-restarting the supervisor
for fresh gh auth`. Authentication was never broken, and the restart it performs
cannot help. `classify_scan_failure` maps a timeout to `Boundary::Transport`,
whose `next_action` says not to re-authenticate, and reserves
`Boundary::Permission` for actual credential evidence (`HTTP 401`, `bad
credentials`). A rate limit is explicitly *not* credential evidence — it is a
completed call.

`SupervisorReport` carries two boundary fields on purpose: `boundary` (why *this
assertion* could not measure, `Some` only when `Unknown`) and
`observed_boundary` (what the *supervisor's own* failures classify into — the
field that reads `transport` on a log that says "auth").

### A budget set by absence is the shape of the bug

`assess_scan_budget` takes the lane's declared scan timeout as an `Option`.
`None` means the lane inherits the 15s default, which is how the release lane
came to have a budget nobody chose while the gate lane on the same host declared
180s and absorbed the identical latency. Observed latency is reported as a ratio
against whichever budget applies, so the same measurements can read `Served`
under a declared budget and `Degraded` under an inherited one.

### A host can refuse work it could do, and four causes print one line

`src/fleet_slot.rs` covers the case where capacity exists, is free, and is being
**withheld**. Measured on this fleet: a host with both macOS VM slots free and a
release job queued, yielding indefinitely to a priority lane whose own two
supervisors on the same host reported `queued=0`.

`priority_demand=1` / `priority lane 'X' has the slot` is printed for four
causes whose remedies do not overlap:

| Cause | Correct? | Verdict | Remedy |
|---|---|---|---|
| genuine **queued** priority demand | yes | `Served` | wait |
| an **unusable** job (assigned, in progress, or hosted-only) that cannot take a self-hosted slot | **no** | `Degraded` | stop counting it |
| the scan **failed** and fell closed to `1` | invisible | `Degraded` | fix the scan |
| `host_health_yield` on real memory saturation | yes | `Served` | free memory — forcing a boot would harm |

Two of those are correct behaviour and must not raise: raising on correct
behaviour is what trains an operator to ignore the raise, which is how the real
defect hid among identical lines in the first place. `WithholdCause` is a
report-level type mapping onto the shared `ServiceVerdict`, so the crate keeps
one severity order.

**Cross-supervisor coherence needs no external oracle.** Two supervisors on the
same host scanning the same repo that contradict each other prove, between
them, that one is wrong. The check encodes only the sharp case — lane A yields
*citing* lane B while every supervisor actually serving B reports `queued=0` —
because supervisors watch different label sets, so differing `queued=` is not
by itself a contradiction.

### Relay hops: "does the proxy answer?" is the wrong question

`src/fleet_relay.rs` asserts that **each declared hop** connects within a
budget, in order — not that the relay as a whole responds.

The distinction is the whole incident. A relay was invoked
`--relay-host macmini --relay-host m1` with macmini unreachable. The relay kept
working, because the fallback succeeded, so every liveness-shaped question
stayed green. What it cost was a tax paid on **every** connection before that
fallback: 18 s via proxy against 2 s direct on one host, 5.5 s against 0.2 s on
another. The tax silently exceeded a *downstream* timeout — a supervisor whose
queue scan inherited a 15 s budget went blind, booted no VM, and a release lane
starved for about a day, twice.

So:

- **Position decides the cost.** A hop failing *before* the first hop that
  answers is paid by every connection; the identical hop *behind* an answer
  costs nothing today. Both are `Degraded` — an unreachable fallback is
  redundancy already lost, not a hop at rest — but the reported tax and
  `attempted` tell them apart, and the detail is chosen by cost rather than by
  verdict.
- **Connect time is a ratio against its budget**, never collapsed to pass/fail.
  A hop at 4.9 s of a 5 s budget is one bad afternoon from being the incident
  and must not read like one at 0.2 s.
- **An unmeasurable hop is `Unknown`** with a named `Boundary`, even when its
  siblings are healthy.
- **No hop answers → `Unserved`**: the relay is severed, not taxed.

`any_hop_connected()` ships as a documented *fact*, never a verdict — it is
precisely the naive question that stayed green throughout the outage.

The proposal side is describe-only: reorder healthy hops first, drop ones that
cannot connect, and refuse outright if any hop was unmeasured or if the drop
would leave nothing that connects.

Reproduction note: early attempts to reproduce this used `env -i`, which strips
the `*_proxy` variables. The control was cleaner than the thing it controlled
for, and passed every time.

## Implementation notes

- Pure detection logic lives in `src/runner_watchdog.rs` and has no I/O.
- Fleet service assertions live in `src/fleet_service.rs`, same shape: no I/O,
  no ambient clock, `now` injected so tests never touch system time.
- The CLI shell-out is contained in `src/app/runner_cmd.rs`. It uses the
  existing `gh` invocation pattern from `src/cloud.rs`; no new HTTP
  client dependency.
- `crate::cloud::QueuedRun` and `GitHubActions::{list_queued_runs,
  cancel_workflow_run}` are reused unchanged from the cloud-handoff
  subcommand.
- This subcommand intentionally does not touch any other Shipyard
  subcommand.
