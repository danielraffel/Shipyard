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
skipped}` (`skipped` only under `--dry-run`) — mirroring the
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

## Implementation notes

- Pure detection logic lives in `src/runner_watchdog.rs` and has no I/O.
- The CLI shell-out is contained in `src/app/runner_cmd.rs`. It uses the
  existing `gh` invocation pattern from `src/cloud.rs`; no new HTTP
  client dependency.
- `crate::cloud::QueuedRun` and `GitHubActions::{list_queued_runs,
  cancel_workflow_run}` are reused unchanged from the cloud-handoff
  subcommand.
- This subcommand intentionally does not touch any other Shipyard
  subcommand.
