---
name: shipyard
description: Shipyard operations guardrails. Use when working in /Users/danielraffel/Code/shipyard, /Users/danielraffel/Code/shipyard-rust, or /Users/danielraffel/Code/shipyard-macos-gui on parity checks, drift checks, sandbox validation, live Tailscale/GitHub webhook validation, release signing, GUI validation, Pulp/consumer pin cutover, or any go/no-go migration work.
---

# Shipyard

## Core Rule

Preserve the user's active Shipyard install and rollback path. Rust Shipyard is
the daily implementation as of `v0.51.0` / `v0.51.1`, but do not replace
`/Users/danielraffel/.local/bin/shipyard`, remove preserved backups, change
Pulp pins, reset Tailscale Funnel, or merge GUI cutover support without a clear
go/no-go for that operation.

## First Steps

1. Confirm the active repo and dirty state with `git status --short`.
2. Use RepoPrompt for code analysis across Shipyard, historical shipyard-rust,
   and the macOS GUI before declaring parity or implementation gaps.
3. Read the current planning packet before making release/cutover claims:
   `planning/post-cutover-status.md`, `planning/go-no-go-completion-audit.md`,
   `planning/upstream-drift.md`, `planning/documentation-backlog.md`, and
   `docs/plan/README.md`.
4. Use `--mode isolated`, temporary install directories, and sandbox HOME/PATH
   roots for rehearsals that must not touch the active production state.
5. For contributor-controlled revisions or `/shipyard review`, use
   `skills/review-external-contributions/SKILL.md`. The dedicated disposable-VM
   controller is intentionally separate from normal Shipyard targets and must
   fail closed rather than use local, SSH, host-pool, self-hosted, or fallback
   execution. Its untrusted job VM has no virtual NIC; only the trusted
   controller talks to GitHub. Timer activation remains an operator decision.

## Merge-queue ownership

When GitHub's live branch queue or evaluated rules require a merge queue,
Shipyard is a validator and queue supervisor, not a second merge authority. A
passing `shipyard ship` calls GitHub's queue mutation with
`expectedHeadOid=<validated SHA>` and waits for GitHub to merge it. The
one-shot `shipyard auto-merge` returns exit 3 while queued and retains
ship-state for later ticks.

For private-free repositories, GitHub may return an explicit null live queue
and its exact plan-entitlement 403 from evaluated rules. That one combination
selects classic merge while retaining the validated-head preflight,
`--match-head-commit`, and REST `sha=` guard. Generic 401/403 responses,
malformed authority, and a non-null queue still stop or select the queue path.

Never treat queue absence alone as permission to rearm. The PR must first have
been observed in the queue (durably recorded across restarts), and only an
`invalid_merge_commit` removal may be re-enqueued automatically. Failed checks,
manual/unknown removal, head drift, malformed authority data, or a
403/rate-limit response stop fail-closed.

For fleets, configure exactly one mutation host in the trusted machine-global
`config.toml` reported by `shipyard paths` (never in tracked project config):

```toml
[merge_queue]
mutation_machine = "studio"
```

Each box must have its stable `shipyard runner tag`. Validation can execute on
any host, but a non-authority host must fail before queue mutation. Use
`shipyard merge-queue hold --reason "<incident>"` on the configured mutation
machine as the authority stop and `shipyard merge-queue status` there to verify
it. Propagate the hold to other hosts when consistent fleet status matters. Do
not resume until the incident owner has restored the intended queue order.
Queue writes are serialized process-wide and recorded in machine-global
`merge_queue/mutations.jsonl`.

## Local/SSH VM Watch

Use `shipyard watch local` for long target-backed jobs that are not GitHub
Actions runs, such as following a build inside a Tart Linux VM:

```sh
shipyard watch local \
  --target linux-vm \
  --command './build-v8.py --target linux-x64 --seal --audit' \
  --milestone-regex '\[[0-9]+/[0-9]+\]' \
  --terminal-regex 'AUDIT FAIL|ld\.lld: error'
```

This mode supports `backend = "local"` and POSIX `backend = "ssh"` targets. It
streams target output, emits milestone lines for matching regexes, emits exactly
one terminal line on process exit or terminal-regex match, and exits with the
process status unless a terminal regex stops it early.

## Target Command Evidence

Use `shipyard run command` when a local or POSIX SSH target should run a single
workload-specific command, assert its exit code, pull declared artifact globs
back to the host, and store a typed evidence bundle that is separate from
merge-ready validation evidence:

```sh
shipyard run command \
  --target linux-vm \
  --name v8-linux-x64-seal \
  --expect-code 0 \
  --artifact 'build/linux-x64/lib/libv8.so' \
  --artifact 'logs/v8-audit.log' \
  -- bash -lc './build-v8.py --target linux-x64 --seal --audit'
```

Query the newest bundle with `shipyard evidence command --json` or list stored
bundles with `shipyard evidence command --list`. Artifact globs are relative to
the target working directory (`cwd` for local targets, `repo_path` for SSH
targets, or `--target-cwd` when overridden).

## Runner Metrics

Use `shipyard metrics` when an agent needs historical runner timing, queue, and
health context before recommending a routing, cache, or monitoring change. The
metrics store is optional and local to Shipyard state; projects do not need
tartci to participate. GitHub-hosted workflows, local commands, SSH targets, and
other VM managers can all record or import rows.

`shipyard run command` writes a best-effort metrics row alongside command
evidence. Import cloud and VM history explicitly when comparing local hardware
with GitHub-hosted runners:

```sh
shipyard metrics import github --repo danielraffel/pulp --limit 50 --json
tartci runtime export --repo danielraffel/pulp |
  shipyard metrics import tartci --json
shipyard metrics summary --project pulp --json
shipyard metrics watch --project pulp --since 14d --json
shipyard metrics advise --project pulp --json
shipyard metrics compare --project pulp --baseline github-hosted --candidate macstudio --json
```

The agent-facing commands return conservative JSON findings. Low sample counts
are a collection gap, not a regression. Prefer filing issues or changing
profiles only when `watch`, `advise`, or `compare` reports enough samples and a
material delta relative to that repo's baseline.

When fixing GitHub importer bugs, keep Actions list endpoints absolute
(`/repos/<owner>/<repo>/...`) and force `gh api -X GET` whenever `-f` supplies
query parameters. `gh api -f` defaults to POST, which can turn a valid list
endpoint into a misleading 404.

## GitHub Auth And Quota

Shipyard's operational GitHub calls can be configured with `[github.auth]`.
Default behavior is ambient `gh` auth. Configured env or command-helper tokens
are injected only into child `gh` commands as `GH_TOKEN`; Shipyard must never
write raw tokens, GitHub App private keys, Keychain items, 1Password sessions,
or token caches to config, state, logs, or release artifacts.

`shipyard update` reads public release metadata, so it works with no auth — but
unauthenticated GitHub API calls are capped at 60/hr. As of v0.68.0 `update`
opportunistically authenticates: it uses `SHIPYARD_GITHUB_TOKEN` / `GH_TOKEN` /
`GITHUB_TOKEN` if set, else falls back to `gh auth token`, and threads that token
into both its own `releases/latest` query and the `install.sh` it invokes. No
token is ever required (the repo is public). If you see *"GitHub API rate limit
exceeded"* from `update`/`install.sh`, that is the 60/hr unauthenticated cap, not
a missing macOS `.dmg` — run `gh auth login` (or export `GITHUB_TOKEN`) and retry.

To raise the quota above the ambient `gh` user token's 5,000/hr, point
`[github.auth]` at a GitHub App installation token helper
(`scripts/shipyard-github-app-token`) for the 12,500/hr installation bucket.
Put the block in the **global** config dir (find it with `shipyard paths`;
macOS `~/Library/Application Support/shipyard/config.toml`) to cover every repo
on the machine — not the tracked project config. The same App private key works
across multiple Macs (M1/Studio/M5). Full setup, permissions, and the
additional-client steps: [`docs/github-app-quota.md`](../../docs/github-app-quota.md).

When debugging GitHub behavior:

- Run `shipyard doctor --rate-limit --json` to see the effective auth source
  and REST/GraphQL buckets. This actively resolves configured auth, so command
  helpers may run and GitHub App helpers may mint installation tokens.
- Optional-provider rows stay green when unused: `nsc` reads "not configured
  (optional)" unless a Namespace provider is configured, and a `{repo_slug}`
  `token_command` that can't resolve in a repo-less context (doctor) is
  green with a "pin `--repo`" hint rather than a red "misconfigured". The
  **daemon** no longer hits this: webhook registration passes the served
  `--repo` as a `{repo_slug}` hint (`GhClient::with_repo_hint`), so a
  `{repo_slug}` `token_command` mints a token from the daemon's repo-less CWD
  instead of looping on "placeholder requires remote.origin.url" with live mode
  stuck on "updates paused". The
  `gh-scope` row is also green-informational for App/Env/helper tokens (scopes
  not inspectable locally), keeping the "verify Actions: Read/write" reminder.
- Check `.shipyard/config.toml`, `.shipyard.local/config.toml`, and global
  config for `[github.auth]` before assuming ambient `gh auth status` explains
  the operation.
- Treat GitHub App installation tokens and fine-grained tokens as permissions
  that may not be locally inspectable through `gh auth status`; verify App or
  token permissions in GitHub when cloud retarget/handoff needs Actions: Read
  and write.
- Keep `RELEASE_BOT_TOKEN` separate. `shipyard release-bot setup/status` are
  operator actions and intentionally use ambient `gh` auth.
- Keep high-volume GitHub inspection on the configured Shipyard auth source.
  PR creation is the only intentional ambient-auth escape hatch: if a GitHub App
  installation token is rejected for PR creation by both GraphQL and REST,
  Shipyard prints an explicit notice and uses ambient `gh` auth for that create
  operation only.
- Mac-to-Mac portability is config-only. Reprovision env vars, Keychain items,
  1Password sign-in, or App private keys outside Shipyard on the destination
  Mac.
- Use `shipyard auth export` and `shipyard auth import --scope local` only for
  sanitized config movement. The bundle must not contain tokens, private keys,
  Keychain exports, 1Password sessions, queue state, daemon sockets, or token
  caches.

## Drift And Parity

Run drift checks whenever Python Shipyard may have changed:

```sh
python3 scripts/update_drift_tracker.py
```

Only advance the baseline with `--mark-reviewed` after the new upstream changes
have been audited and reflected in Rust or explicitly risk-accepted.

Compare command surfaces safely:

```sh
python3 scripts/compare_cli_surface.py \
  --python-bin /Users/danielraffel/Code/shipyard/.venv/bin/shipyard \
  --rust-bin target/release/shipyard \
  --allow-rust-only paths
```

Run the finish-line credential gate before signing or release claims:

```sh
python3 scripts/finish_line_status.py \
  --env-file /Users/danielraffel/Code/PlunderTube/.env \
  --json
```

## Runner Watchdog (self-hosted runner recovery)

Shipyard ships a `runner` subcommand family for detecting and recovering from
stuck self-hosted GitHub Actions runner state. Built after the 2026-05-12
incident where a UBSan job from a closed branch wedged Pulp's local runner for
>75 min while 17 stale queued runs piled up behind it, blocking PR #1859 for
hours.

### When to reach for it

- Runner reports `busy=true` to GitHub but no Worker process running locally
- Worker process running >90 min on a job that should take ~20-30 min
- Queue depth growing while runner appears stalled
- Stale queued runs from closed/rebased branches monopolizing the runner

### Safe commands (read-only or advisory)

- `shipyard runner status` — one-shot health check, exit 0/1/2, `--json` supported
- `shipyard runner cleanup --dry-run` — list stale queued runs without cancelling
- `shipyard runner watch` — advisory daemon mode, polls every 5 min

### Mutating commands (require explicit flags)

- `shipyard runner cleanup --fix` — cancel stale queued runs (1s gap between cancels)
- `shipyard runner watch --fix` — auto-recovery loop (cron-friendly)
- `shipyard runner kill --pid X --reason "..."` — kill a specific Worker; requires typed `KILL` confirmation

### `runner kill` recovery sequence

10 steps, all reversible. **Nothing is destroyed.**

1. Snapshot kill event to `~/.shipyard/kill-recovery.jsonl`
2. Typed `KILL` confirmation (skip with `--yes`)
3. SIGTERM with 10s grace (configurable via `--grace-secs`)
4. SIGKILL only if still alive
5. Reap orphaned children (`cmake|ninja|make|ctest|build`)
6. **Move** (not delete) partial `build*` dirs to `/tmp/shipyard-killed-builds/<event-id>/`
7. Verify `Runner.Listener` health via `pgrep`
8. Poll GitHub for status flip to `completed`/`failure`
9. Optional `--retrigger` re-queues the killed PR's CI
10. Print recovery summary with `--recover` invocation hint

A misclick costs ~2 min of cmake re-configure. To recover:
`shipyard runner kill --recover <event-id>` walks the quarantined `build*` dir
back to `_work/<repo>/` and re-queues the killed run.

### Gotchas

- The watchdog's `busy=true but no Worker process` check has a brief 1-5 min
  false-positive window after `cleanup --fix` cancels a run — the runner needs
  time to gracefully exit. Don't double-cancel.
- `runner kill --pid` REFUSES non-Runner.Worker PIDs as a safety check. Override
  via `--runner-dir` only if your install path is non-standard.
- The `concurrency: cancel-in-progress: true` workflow setting SHOULD auto-cancel
  on force-push but doesn't always (Pulp issue #1884). The watchdog's stale-queue
  detection catches the consequences.

### Config

Per-machine overrides in `.shipyard.local/config.toml`:

```toml
[runner.watchdog]
runner_id = 1763
runner_dir = "/Users/me/actions-runner"
max_job_min = 90
max_queue_age_hours = 2
watch_interval_seconds = 300
auto_fix = false
# Stale-run reaper thresholds (minutes) for `runner watch --reap-stale-runs`:
reap_in_progress_max_min = 300
reap_queued_max_min = 480
```

## Host-Health Pre-Dispatch Gate (optional)

For self-hosted runners *co-located with heavy interactive work*: read a shared
`host_vitals` signal during ship/run preflight and surface — or, opt-in,
hard-stop on — a saturated host before a ship runs into a jetsam/reboot failure
that reds the required gate for an *infra* reason.

**Off by default, fails open.** No `[host_health]` block or no signal file → no
read, no change. Missing/unreadable signal → treated as "no opinion", the ship
proceeds. (Inverse of backend-reachability preflight, which fails closed:
reachability gates correctness, host-health gates only crash-avoidance.)

```toml
[host_health]
gate = true                     # master opt-in (pre-dispatch gate)
block_on_critical = false       # true → `critical` hard-stops preflight (exit 4); default warns
classify_local_failures = false # true → relabel a local TEST failure as INFRA when a host
                                #        jetsam/WindowServer crash overlapped the leg window
# file = "/custom/host_vitals.json"   # default: ~/.local/state/pulp/host_vitals.json
```

`classify_local_failures` is an independent opt-in: it relabels a **local** leg's
`TEST` failure to `INFRA` (with an honest note) when the signal shows a host
incident during the leg. Conservative — only `TEST` is eligible, only local legs,
and it's a pure label (never flips `TargetStatus`, so a failed leg still blocks
merge). Fails open. Full behavior: `docs/local-mac-pool.md` § Infra-vs-code
failure labelling.

Signal contract: JSON with numeric `code` (0/10/20) and/or string `level`
(green/warn/critical) + optional `reason`; `code` wins. Shipyard ships no
producer — bring your own (Pulp's `tools/scripts/host_vitals.sh` + its launchd
sensor writes the default path). `SHIPYARD_HOST_VITALS_FILE` overrides the path.
Behavior when `gate=true`: green/absent → silent; warn → warn + proceed;
critical → warn + proceed, or fail (exit 4) when `block_on_critical`. Full table:
`docs/local-mac-pool.md` § Host-Health Pre-Dispatch Gate.

**Same-backend transient retry (`[ship] transient_local_retries`).** Off by
default (`0`, clamped `0..=2`). When set, a **local** leg that fails with a
transient `INFRA` blip is re-run once (or up to the bound) on the same backend
before the failure is recorded. Deliberately stricter than the global
`is_retryable` taxonomy: **`INFRA` only** — a local `TIMEOUT` would just re-burn
its wall-clock budget, and `CONTRACT`/`TEST`/`TREE_DRIFT` are authoritative.
Remote legs already have next-backend failover, so same-leg retry is local-only.
Each retry uses a distinct `<log>.retry<N>` path (attempt-0 evidence preserved);
a recovered leg is noted in `phase`, an exhausted one in its error message. With
the default `0`, execution is byte-identical to no retry. Composes with
`classify_local_failures` (a relabelled-`INFRA` failure becomes retry-eligible).
Full behavior: `docs/local-mac-pool.md` § Same-backend transient retry.

## Durable Queue: killed-worker recovery (stale-running reaping)

A `shipyard ship` / `shipyard pr` worker that is killed (SIGTERM, crash,
`kill <pid>`) leaves its job `status: running` in the durable queue
(`queue.json`). Before v0.68.0 this wedged the PR: every later same-PR ship was
refused with `SamePrShipRunning`, and there was no clean way out — `shipyard
cancel <id>` only handled pending jobs, `shipyard ship-state discard <pr>` left
the queue job intact, and the startup reaper
(`recover_stale_running_jobs_for_drain`) only fires on daemon restart, so a
long-lived daemon never recovered. The only fix was hand-editing `queue.json`.

As of v0.68.0 the queue auto-recovers: a `Running` job whose freshest heartbeat
is older than `DEFAULT_RUNNING_JOB_STALE_SECONDS` (180s) is treated as a dead
worker and reaped to `Cancelled` — at ship-submit time
(`refuse_same_pr_running_ship` reaps the stale job, then proceeds) and on every
drain admission pass (`apply_admit_pass_for_drain`). The reap re-checks
staleness under the queue lock, so a worker merely between heartbeats is never
killed; a "stale" job that revived between plan and apply defers conflicting
starts to the next pass rather than double-running the PR.

### Gotchas

- Recovery is heartbeat-age based, so a retry waits up to ~180s after the
  worker dies before it goes through. That is intentional — it must not reap a
  slow-but-live worker. Don't shorten it below the ~15s heartbeat interval's
  safety margin.
- Do NOT launch a second `shipyard pr` for the same PR while the first is still
  alive. That is what strands a `running` job in the first place — one ship per
  PR at a time.
- On a pre-0.68.0 binary the manual recovery is still: `shipyard ship-state
  discard <pr>`, then mark the stuck `queue.json` job terminal (or restart the
  daemon to trigger startup recovery).

## Orphaned ship-state reporting

The queue reaper above recovers the *queue* `Job` when a worker dies. The
durable *ship-state* (`<state>/ship/<pr>.json`) is a separate store with no
orphan lifecycle: when the owning process dies mid-validation (host reboot from
a jetsam kill, daemon crash, `cmux` relaunch), the ship-state freezes in an
in-flight verdict forever. `ship_terminal_verdict` returns `None`, so auto-merge
reports `InFlight` and refuses to merge — the PR silently stalls with no signal.

Both `shipyard ship-state list` and `shipyard status` flag these. A state is
reported orphaned when it is still in flight (the same `ship_terminal_verdict`
predicate the auto-merge gate uses, so the two never drift) **and** the queue
confirms — or fails to disprove — a dead worker. The signal is **source-aware**,
established by cross-referencing a single queue snapshot (module
`src/ship_liveness.rs`), strongest to weakest:

| Evidence | Meaning | When flagged |
|----------|---------|--------------|
| `queue_stale` | matching *running* job whose heartbeat is dead past the reaper's 180s window (`is_stale_running`) — a provably gone worker | immediately (no time gate) |
| `queue_terminal` | matching job already terminal while the ship-state never finalized | immediately |
| `queue_absent` | queue consulted, no matching job (ship-state stores no job id → absence is *inferred*) | time-gated |
| `time_fallback` | queue unavailable — pure `updated_at` staleness | time-gated |

A live worker (running with a fresh heartbeat) or a *pending* (queued,
not-yet-started) job is never flagged, however old `updated_at` looks. The
`queue_stale`/`queue_terminal` signals surface a genuinely dead ship in ~3
minutes; the weak (`queue_absent`/`time_fallback`) signals require the staleness
threshold. Human output prints `ORPHANED? [<evidence>]: ...`; JSON gains
`orphaned: [{pr, stalled_minutes, evidence}]` (`ship-state list`) /
`orphaned_ship_states: [...]` (`status`).

The threshold defaults to 45 minutes and is configurable:

```toml
[ship_state]
orphan_stale_minutes = 45    # clamped to [1, 525600]
auto_resume = false          # opt-in daemon abandon sweep (default off)
```

Detection (`shipyard ship-state list` / `status`) is **report-only** — it never
mutates anything and cannot affect merge readiness (a flagged state is in flight,
which auto-merge already refuses; the harm it surfaces is the *inverse* — a PR
that silently never merges).

**Opt-in abandon sweep (`auto_resume`, default off).** When enabled, the daemon's
periodic reconcile pass runs `ship_resume::sweep_orphaned_ship_states`: for a
strongly-orphaned in-flight state it sets a terminal `abandoned` marker
(`ship_terminal_verdict` → `Some(false)`), so the wait/auto-merge path stops
blocking and a human re-ships. Deliberately conservative — marking a *live* ship
failed is the one catastrophic error, so it abandons **only** on `queue_stale`
evidence (a provably dead owning worker) and fails **closed** (an unavailable
queue never abandons). The sweep snapshot only *selects candidates*; the abandon
decision is re-made per PR under the per-PR lock, re-checking in-flight status
**and re-reading the queue live** — never the sweep snapshot — so a worker that
starts or resumes during the sweep (e.g. an operator re-shipping the moment they
see the orphan) shows live and is spared. It never auto-re-dispatches (no
resume→die→resume loop), and abandonment is idempotent (an abandoned state is
terminal). Emits a `ship_state_abandoned` daemon IPC event. With the default
`false`, the sweep opens no queue and mutates nothing.

**Recovery clears the marker.** Re-shipping an abandoned PR (`shipyard ship
<pr>`) clears `abandoned` when the ship execution begins — on both the
reuse-existing-state and the archive-and-replace fresh-attempt paths — so the
re-validated PR is no longer short-circuited to failure. `shipyard ship-state
discard <pr>` remains the manual alternative. The config load is scoped to the
daemon's own runtime mode, so an isolated daemon reads its own overlay.

### Gotchas

- Only the strong signals (`queue_stale`/`queue_terminal`) are proof of a dead
  worker; the weak ones are staleness heuristics. The time threshold only gates
  the weak signals, so don't shorten it toward normal leg durations — a false
  positive is harmless (it just invites an operator to look; it cannot merge or
  cancel anything), but the weak signals are the noisy ones.
- Classification is lazy and read-only: computed at `ship-state list` / `status`
  time from one queue snapshot, with no daemon startup sweep and no write-back.
  The module opens the queue/ship stores only when they already exist, so
  running a diagnostic in a fresh directory materializes nothing.
- A state stops being reported the moment a live worker touches it or it reaches
  a verdict.

## Runner Provisioning (register / list / remove / tag)

The `runner` family also *provisions* self-hosted GitHub Actions runners, not
just recovers them. This is the generic, repo-agnostic path for bringing a Mac
into a repo's CI fleet — used to stand up the Mac Studio's pulp runners. Pure
naming/index/label/table logic lives in `src/runner_provision.rs`; the shell
side (gh, `config.sh`, `svc.sh`, local `~/actions-runner-*` dirs) is
`src/app/runner_provision_cmd.rs`. See `docs/runner-provisioning.md`.

### Machine tag (load-bearing for multi-Mac fleets)

Runners are named `<repo>-<machine-tag>-NN` (e.g. `pulp-studio-01`). The tag is
an explicit per-box value stored at `<state_dir>/machine-tag`, **never derived
from the hostname** — two MacBook Pros can share a hostname, so a
hostname-derived tag would collide. Set it once per machine:

```bash
shipyard runner tag --set studio   # or m1, m5, …
shipyard runner tag                # prints the stored tag
```

### Register

```bash
# Host must already have the toolchain/caches (repo-specific bootstrap).
# This step only registers runners and points their .env at the shared caches.
shipyard runner register --repo danielraffel/pulp --count 3 \
  --ci-root /Volumes/Workshop/ci/pulp [--dry-run]
```

- Names continue from the highest existing `<repo>-<tag>-NN` (any machine), so
  re-running appends capacity without collisions.
- Default labels: `self-hosted,macos,arm64,<repo>-build,<repo>-build-<tag>`.
  `<repo>-build` is what a repo's workflow selects for normal routing;
  `<repo>-build-<tag>` pins work to one machine. Override with `--labels`.
- Per-runner `_work` is `<ci-root>/work/<name>`; the `.env` points ccache and
  FetchContent at `<ci-root>/cache/*`. Cache *size* is owned by the host's
  `ccache.conf`, not this command.

### List and remove

```bash
shipyard runner list --repo danielraffel/pulp   # live pool, grouped by machine
shipyard runner remove --name pulp-studio-03 --yes [--purge-dir]
```

`list` aggregates across machines straight from GitHub (no controller needed)
and reconciles local `~/actions-runner-*` dirs against GitHub to flag orphans.

### Audit (host-class drift)

```bash
shipyard runner audit --repo danielraffel/Shipyard   # exit 1 on any drift
```

`audit` checks every runner against the host-class scheme — a conforming
`<repo>-<class>-NN` name, the shared `<repo>-build` routing label, the
`<repo>-build-<class>` pin label, and agreement between the class in the name
and the class in the labels. It flags non-conforming names (e.g. a hand-named
`daniels-macbook-shipyard`) and missing labels (e.g. a runner registered with a
bespoke `--labels` that dropped `<repo>-build-<class>`), exiting non-zero so CI
or a cron can gate on a clean fleet. This is the foundation for the M5 joining
by class with zero bespoke setup. Pure naming/label logic; physically
confirming a `*-studio-*` runner is on the Studio is `runner capacity`'s job
(reads the host machine tag over SSH). Full design:
`planning/2026-06-01-multi-mac-controller.md` (Shipyard #316).

### Capacity (VM-slot accounting)

```bash
shipyard runner capacity --json   # exit 1 if any host unreadable
```

macOS caps **2 running VMs per host** (XNU kernel quota; Pulp plan Appendix D).
`runner capacity` reads each `[host_class.<name>]`'s running Tart VMs (locally
for the controller's own box, over SSH otherwise), enriches each running VM with
`tart get <name> --format json`, and counts only macOS/darwin VMs as consuming
the macOS quota. Set `tart_home` when launchd supervisors use a non-default Tart
store; the probe then runs with `TART_HOME=<absolute-path>` and reads the same
store. Linux/Windows Tart VMs do not reduce this free-slot count.
**Fail-closed:** an unreadable host or VM OS counts the host as 0 free and the
command exits non-zero — a silent host must never read as spare capacity.
Configure host classes (operator-specific, so keep these in
`~/.config/shipyard/config.toml` or `.shipyard.local/`, not the committed repo
config):

```toml
[host_class.studio]
# ssh omitted → the controller's own box, read locally
cap = 2                                    # Studio may raise via Appendix-D override
tart_bin = "/opt/homebrew/bin/tart"        # if tart isn't on the SSH PATH
tartci_bin = "/Users/ci/.local/bin/tartci" # for fleet-status doctor probes
tart_home = "/Users/ci/VMs"                # absolute path; no shell/tilde expansion
labels = ["self-hosted", "macos", "arm64", "shipyard-build-studio"]

[host_class.m1]
ssh = "m1-ci.local"
cap = 2
tart_bin = "/opt/homebrew/bin/tart"
tartci_bin = "/Users/ci/.local/bin/tartci"
tart_home = "/Users/ci/VMs"

# [host_class.m5] arrives later — same shape, inherits cap = 2.
```

This free-slot count is what the cloud→local reroute watcher (#316 Part C)
gates on: drain a still-queued cloud macOS job to local only when `free > 0`.

Use `shipyard runner fleet-status --repo <owner/repo> --target macos --json`
for the operator view that answers "can queued jobs actually drain?" It combines
capacity with host-local `tartci doctor --reap --json`, supervisor heartbeat
freshness, per-host routability, and oldest queued macOS age. It is read-only
and exits non-zero when a host is unreadable/unhealthy or when queued macOS work
is older than `--queued-age-threshold-secs` while routable capacity exists. Use
`--queue-run-limit N` to keep live debugging snappy on a large queued backlog.

The report retains optional workflows, finds queued jobs inside `in_progress`
workflows, and compares exact merge-group SHAs. A tick spends at most two
active-run list requests plus 50 per-run job requests; larger observations
fail visibly with `OBSERVATION_TRUNCATED` instead of exhausting the monitor's
own API budget. Its local snapshot detects cleared auto-merge enrollment with
a separate 25-PR reconciliation cap and the same truncation signal.
Consume the stable reason codes rather than chat-turn counts. With host classes
configured, `runner watch` invokes this observer by default. This path never
uses Orchard and never mutates GitHub.
The watcher resolves the repository default branch; use `--fleet-base` or
`runner.watchdog.fleet_base` for a different merge target.

Use `shipyard runner steward` for agent-neutral merge-on-green reconciliation.
It observes by default; `--apply` enables exact-head guarded queue admission,
reruns, and narrowly policy-scoped capacity-preemption mutations. Client-side
REST direct merge is refused because GitHub cannot atomically bind both complete
check materialization and the validated base revision; use a server-owned merge
queue or manual merge instead. Apply mode is restricted by central mutation
authority, durable write-ahead intent, and live revalidation immediately before
every GitHub write. The built-in capacity preemption preset applies only to
explicitly advisory Pulp workflows; required workflows and unknown repositories
are disabled because GitHub cannot bind a cancellation to an atomic job-state
snapshot.

Read [references/merge-steward.md](references/merge-steward.md) before operating
the steward, changing its policy, or recovering a pending cancellation.

### Reroute watcher (cloud→local drain)

```bash
shipyard runner reroute-watch --repo danielraffel/Shipyard            # observe
shipyard runner reroute-watch --apply --interval 30 --flap-window 300 # act
```

Ports Pulp's `macos_reroute_watcher.py` (task #22), generalized to multi-host
VM-slot accounting. Each tick: read free slots (`runner capacity`), list the
repo's cloud-queued macOS jobs (`gh` runs+jobs, cloud markers `macos-15` /
`nscloud-` / `namespace-profile-`), and — when `free > 0` and a job is still
waiting on cloud — drain **one** PR back to local. Safety properties (pure logic
in `src/reroute.rs`): **slot-safe/fail-closed** (unreadable hosts count as 0
free, so an all-unreadable fleet does nothing), **flap-guard** (skip a PR
rerouted within `--flap-window`), **one reroute per tick** (natural pacing), and
**deterministic** oldest-run-first choice. **Observe by default** — without
`--apply` it logs each decision, per-host capacity, and the candidate list but
acts on nothing. `--apply` shells `shipyard
cloud retarget … --provider local --apply`, which works for PRs Shipyard is
shipping (ship-state-backed). `cloud retarget` has no `--repo` flag — it
resolves the repo from the current checkout — so run `reroute-watch --apply`
inside the target repo's checkout (its `--repo` only scopes which queued runs
are listed, not where the reroute acts). To prevent retargeting the wrong repo,
`--apply` **fails fast** unless the monitored `--repo` matches the repo
`cloud retarget` will dispatch to — the `[cloud].repository` override if set,
otherwise the checkout (so a configured cross-repo controller setup is allowed);
observe mode may monitor any repo. **Follow-up (Part C.2):** rerouting a PR with no
ship-state, and spinning an ephemeral JIT VM runner on a free-slot host (drive
Pulp's `tart-run-job.sh` equivalent) — until then a persistent host-class runner
handles pickup. Full design: `planning/2026-06-01-multi-mac-controller.md`.

### Gotchas

- These four subcommands are newer than the watchdog set; an older installed
  binary will not have them. Verify with `shipyard runner register --help`.
- `register` does **not** provision the host toolchain (Xcode, Homebrew deps,
  Skia, ccache sizing). Run the repo's own host bootstrap first; this command
  assumes a buildable host and only wires up runners + caches.
- A fresh python.org Python with no CA certs breaks asset downloads in repo
  bootstraps (`SSL: CERTIFICATE_VERIFY_FAILED`) — run the bundled
  `Install Certificates.command`.

### Routing Shipyard CI to a registered Mac (the `local` provider)

Registering a runner only stands up the machine; it does not move any job
onto it. Shipyard's own workflows pick a runner via `scripts/ci_matrix.py`,
which now understands a `local` provider in addition to `github-hosted` and
`namespace`. Set repo variable `DEFAULT_RUNNER_PROVIDER=local` (or dispatch
with `-f runner_provider=local`) and the **macOS ARM64** leg resolves to the
label set `["self-hosted","local-mac"]`; Linux/Windows have no local box and
fall back to GitHub-hosted. So to send Shipyard's macOS **release** build to
the Mac Studio, register a Studio runner that carries those labels —

```bash
shipyard runner tag --set studio
shipyard runner register --repo danielraffel/Shipyard --count 1 \
  --labels self-hosted,macos,arm64,local-mac \
  --ci-root /Volumes/Workshop/ci/shipyard
```

— then flip `DEFAULT_RUNNER_PROVIDER=local`. The signing identity already
lives in the Studio keychain, so the signed/notarized dmg build skips
GitHub's hosted-macOS queue. Full provider semantics:
`skills/ci/SKILL.md` → "Runner Provider Defaults" → "The `local` provider".

### CI routing profiles

Use `shipyard ci profile show <name>` and
`shipyard ci profile plan <name> --repo owner/repo` to inspect repo-owned CI
routing profiles without requiring Tart or any provider-specific CLI. The
planner reads `.tartci/<name>.toml`, `.shipyard/ci-profiles/<name>.toml`, or
`ci-profiles/<name>.toml`, then prints the ordered target chain and the GitHub
variables that would route each lane. It is intentionally read-only; live
capacity resolution and variable writes happen outside this command.

## Supervised Subprocess Marker (issue #266)

Every `git` / `gh` child process spawned by the supervised
`pr` / `ship` / `auto-merge` / `overflow` / `wait` flows is launched
with `SHIPYARD_PR_RUNNING=1` in its environment. Downstream tooling
(notably Pulp's pre-push hook in `danielraffel/pulp#1406`) uses this
to distinguish a Shipyard-orchestrated push from a raw `git push`.

When adding a new subprocess spawn site inside one of those flows,
route through the helpers in `src/supervised.rs`:

- `crate::supervised::gh_supervised(gh_command)` instead of
  `Command::new("gh")` (mirrors the existing `gh(gh_command)`
  helper in `src/pr.rs`).
- `crate::supervised::git_supervised()` instead of
  `Command::new("git")`.
- `crate::supervised::supervised(cmd)` when wrapping an
  injection-style `git_command.map_or_else(..., Command::new)`
  pattern (see `src/branch.rs` for the precedent).

Diagnostic subcommands (`doctor`, `pin`, `runner`, `cleanup`,
`cloud`, `governance`, `release_bot`, `reconcile`) deliberately
skip the marker — they are not "supervised pushes" per the
audit-log use case. If you add a brand new orchestrated flow,
extend the scope deliberately rather than blanket-supervising
everything.

## GraphQL And GitHub App Fallback Behaviour

Five operations detect `is_graphql_rate_limited` in `gh` stderr and
fall through to a REST equivalent: PR list, PR create, PR view, PR
snapshot (in `wait_transport`), and PR merge (in
`app/auto_merge_cmd`). When that happens, `pr::report_rate_limit_fallback(operation, cwd)`
prints a one-line user-visible notice on stderr, including the
GraphQL reset time when a best-effort `gh api rate_limit` probe
succeeds. Add this call to any new REST-fallback dispatch site so
the operator-visible signal stays consistent.

GitHub App installation tokens can also be rejected by GitHub's GraphQL
`createPullRequest` / `mergePullRequest` mutations even when the App token is
otherwise the right auth source for inspection. PR creation first tries the
existing GraphQL path, then REST with the same configured token. If both are
blocked with `Resource not accessible by integration`, Shipyard prints a second
explicit notice and falls back to ambient `gh` auth for PR creation only. PR
merge falls back from GraphQL to the existing REST merge path with the same
configured token. Do not apply ambient-auth fallback to polling, watch,
retarget, diagnostics, merge, or other high-volume operations.

`GitHubActions::pr_head_ref` also falls back from `gh pr view` to
`GET /repos/:owner/:repo/pulls/:number` when GraphQL is rate-limited; both
attempts must use the same configured `GhClient` so GitHub App quota is
preserved.

The REST merge path (`merge_pr_rest`) passes the original head SHA
as `-f sha=<oid>` on the PUT so GitHub enforces the merge race-guard
server-side. On a `405 Base branch was modified` response, it
refetches head info via `pr_head_info_rest` and retries exactly once
if and only if the head SHA is unchanged. A changed head SHA means
a new commit landed during the merge attempt — the retry is refused
because the prior green evidence may no longer apply.

Before that merge ever runs, `execute_auto_merge` does a **client-side
superseded-SHA preflight** (#321): it fetches the live PR head via
`fetch_live_head_sha` (which accepts either `headRefOid` or `head.sha`
from a snapshot or a fresh `gh`/REST read) and compares it with
`shas_match` against the `state.head_sha` Shipyard actually validated.
If they differ, it returns `AutoMergeOutcome::SupersededSha { validated,
current }` and **refuses to merge** rather than landing a SHA whose
green evidence is stale — `ship_cmd`'s `post_run_merge_state` maps that
outcome to `GreenNotMerged`. This is fail-closed: if the live head
cannot be read, the preflight does not assume safety. It is a belt-and-
suspenders layer in front of the server-side `--match-head-commit`/`sha=`
guard above, because GraphQL auto-merge can otherwise land a commit
pushed *after* validation completed (the bug that merged pulp #3128 at a
pre-fix SHA).

### GraphQL selection sets are unverified until something runs them

`gh api graphql` failures arrive as prose on stderr with no machine-readable
code, and a malformed *document* is indistinguishable at the call site from a
legitimate rejection. Two rules follow.

**Verify a new or edited selection set against the live schema.** Nothing in
`cargo build` checks that a field exists; a wrong guess fails only when the
query runs, in front of an operator. Shipyard ≤0.80.1 selected
`autoMergeRequest{id}` in the merge-queue poll query — `AutoMergeRequest` is a
plain OBJECT implementing no interfaces, so it is not a `Node` and has never had
an `id`. GitHub rejected the entire document. Since `queue_admission` issues that
query *before* any mutation, merge-queue admission failed outright for every
queue-governed repository: PRs validated green and were never enqueued.

```sh
gh api graphql -f query='{__type(name:"AutoMergeRequest"){fields{name}}}'
```

The poll query is now built by `queue_poll_query()` from a named probe constant,
with the type's real field list pinned in `AUTO_MERGE_REQUEST_FIELDS` and
asserted by tests — so a stale selection fails in `cargo test`, not at merge
time. Follow that shape for any query whose selection set could drift: build it
from constants and assert the shape, rather than inlining a string literal. Note
that GraphQL forbids an empty selection set, so a mere presence probe still has
to name one real field.

**Never let a client defect render as a PR problem.**
`is_graphql_malformed_query_error` classifies malformed-document stderr, and
`post_run_merge_state` routes it to `ShipRenderState::GreenNotMergedClientDefect`
*before* any PR-inspecting classification runs. That state renders a diagnostic
naming Shipyard as the fault, reports `status:"green_not_merged_client_defect"`
in `--json`, and exits `8` — distinct from `1` (validation genuinely failed) and
from the `0` the other green-but-unmerged states keep. The classifier fails
closed: unrecognized stderr stays an ordinary merge rejection, so genuine blocks
never lose their branch-protection guidance. Extend
`GRAPHQL_MALFORMED_QUERY_SIGNATURES` when GitHub adds a phrasing, and keep the
"must NOT match" test cases — a false positive would tell an operator to report a
Shipyard bug when their PR really was blocked.

### Flaky-required-leg wedge → rescue hand-off (`auto_rescue`)

When Shipyard validated every target green but `gh pr merge` is *rejected*
(`post_run_merge_state` sees `AutoMergeOutcome::MergeFailed`), the usual cause
is a GitHub branch-protection **required check that is RED on the exact SHA
Shipyard just validated** — a *flaky* required leg, not a real regression.
`classify_merge_failure` (`src/app/ship_cmd.rs`, backed by the pure
`auto_rescue::classify_wedge`) decides whether that's the case and, if so,
renders `GreenNotMergedFlakyRequired` — a hand-back that hands the operator the
one-liner `shipyard rescue <PR> --rerun-failed` instead of the generic message.
It never mutates the merge path (only guidance text + one additive JSON field,
`flaky_required_recovery`).

It is deliberately fail-closed — it falls back to the plain `GreenNotMerged`
hand-back on **any** ambiguity:
- a red *or pending* check with absent `isRequired` (ruleset / merge-queue
  governance, older `gh`, REST-synthesized rollup that lacks requiredness);
- a red required check whose name doesn't map to a Shipyard-validated-green
  target;
- an unreadable ship-state, a failed rollup fetch, or a live `headRefOid` that
  no longer matches the validated `head_sha` (same head-advance guard as the
  superseded-SHA preflight above).

**Mapping is exact or explicitly configured — never fuzzy.** A red required
check maps to a validated-green target only when the check context name equals
the target name (case-insensitive) *or* the target declares
`required_check_context = "<check name>"` in `[targets.<t>]`. This bridges the
common case where the Shipyard target is `mac` but the GitHub required check is
`macos`: without the config line the classifier fail-closes to a no-op, so a
consuming repo must add `required_check_context` to activate the hint. Do NOT
reach for the fuzzy reconcile matcher here — a wrong mapping would tell the
operator to "just rerun" a genuinely-failing required check.

The destructive counterpart (actually invoking `rescue --rerun-failed` + arming
auto-merge automatically, behind a default-off flag) is a planned follow-up; the
current behavior is diagnostic-only.

## Validation Gates

**Before `shipyard pr` / `shipyard ship`, run the *exact* chain the `mac`
target enforces** (`.shipyard/config.toml` `[targets.mac]`). `--lib`-scoped
checks are NOT enough — `--all-targets -- -D warnings` and `cargo fmt` catch
things the lib build won't, and a miss costs a full ship round-trip (the
2026-06-01 runner-provisioning PR failed mac validation twice this way):

```sh
cargo fmt --all --check \
  && cargo clippy --all-targets --locked -- -D warnings \
  && cargo test --all-targets --locked
```

**`Cargo.lock` gotcha after a version bump:** `shipyard pr` rewrites
`Cargo.toml` / `.claude-plugin/plugin.json` but does NOT touch `Cargo.lock`,
so the `--locked` steps then fail with a lock-vs-manifest mismatch. After any
bump, refresh the lock (`cargo build`/`cargo check`) and commit `Cargo.lock`
in the same PR. (`cargo fmt --all` on new modules is the other easy miss.)

**Ship-state SHA drift recovery (`--adopt-head`, #346):** if you amend or
force-push a PR's tip after Shipyard recorded ship-state (e.g. adding a
required `Version-Bump: skip` trailer), the next `shipyard ship`/`pr` aborts
with `ship state SHA drift: existing <old>, current <new>`. Re-run with
`--adopt-head` (`shipyard ship --adopt-head` / `shipyard pr --adopt-head`): it
adopts the current head and **clears the recorded remote runs + evidence** so
the new head re-validates from scratch — it never blesses stale validation for
a possibly-different tree. The policy-signature guard still applies (a changed
merge policy is still refused). Without the flag the old dead-end (manual `gh pr
merge`) stands.

Other non-mutating checks:

```sh
cargo test --all-targets --locked
python3 -m unittest discover -s scripts -p 'test_*.py'
python3 scripts/update_drift_tracker.py
python3 scripts/compare_cli_surface.py --allow-rust-only paths
scripts/validate_webhook_tunnel_live.py --json
```

The live webhook gate is intentionally dangerous because it resets the local
Funnel config:

```sh
scripts/validate_webhook_tunnel_live.py \
  --repo danielraffel/Shipyard \
  --binary "$(command -v shipyard)" \
  --apply \
  --allow-funnel-reset \
  --json
```

Run that only in an approved window where briefly taking over the
machine-global Tailscale Serve/Funnel route is acceptable. The validator knows
about the App Store Tailscale binary at
`/Applications/Tailscale.app/Contents/MacOS/Tailscale`; do not assume a
`tailscale` PATH shim exists.

## macOS GUI

The GUI lives at `/Users/danielraffel/Code/shipyard-macos-gui`. Validate it
against a sandboxed or signed rehearsal artifact before replacing the active
production `shipyard`. Update GUI docs during migration/release work, not
after the fact.

## Platform Notes

Read `references/platforms.md` when work touches Tailscale, live mode,
signing, packaging, Namespace/GitHub Actions runners, Windows SSH/PowerShell,
or cross-platform sandbox E2E behavior.

Namespace is optional and account-dependent. When Namespace is unavailable,
Shipyard should default to GitHub-hosted Linux/macOS/Windows runners or explicit
self-hosted GitHub Actions labels. Do not assume `nsc` access, and do not route
new Shipyard CI to Namespace unless the user explicitly confirms active access.
Do not add hidden repo-variable fallbacks to local/self-hosted macOS runners:
local runner use should be explicit via workflow-dispatch selector inputs so
default GitHub-hosted runs cannot be stolen by stale local runner variables.

For local capacity, keep GitHub Actions as the dispatch layer and use SSH only
to manage the runner hosts. Stable labels such as `shipyard-macos-arm64`,
`shipyard-linux-arm64`, and `shipyard-windows-x64` are preferable to raw host
names in workflow `runs-on` selectors.

For a simple Mac Studio setup, use explicit Shipyard fallback config rather
than hidden self-hosted runner state:

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

For named members and lease visibility, use `backend = "host-pool"`:

```toml
[host_pools.local_macs]
strategy = "ordered"

[[host_pools.local_macs.members]]
id = "mac-studio"
type = "ssh"
host = "mac-studio"
repo_path = "/Users/shipyard/work/shipyard"
capabilities = ["macos", "arm64"]

[[host_pools.local_macs.members]]
id = "local"
type = "local"
cwd = "/Users/danielraffel/Code/shipyard"
capabilities = ["macos", "arm64"]

[targets.mac]
backend = "host-pool"
pool = "local_macs"
platform = "macos-arm64"
requires = ["macos", "arm64"]
```

Host-pool targets acquire/release local leases, show state through
`shipyard targets pool status`, and prune stale lease records with
`shipyard targets pool cleanup --fix`. They can drain multiple
non-conflicting queued jobs across available members under one local drain
owner, but they still do not interrupt running GitHub-hosted macOS jobs. Jobs
serialize when they claim the same checkout, PR state, evidence lane, or
exhausted pool capacity. See `docs/local-mac-pool.md` before claiming
multi-Mac throughput.

For Pulp/tartci macOS VM work, prefer local queueing over hosted overflow: a
full local fleet should leave jobs queued on the self-hosted VM labels until a
controller/secondary Mac slot opens. Add GitHub-hosted macOS only as an
explicit operator fallback when fleet status says the local Macs are
offline/unhealthy, or when the workflow intentionally asks for hosted coverage.

## Cloud Retargeting

`shipyard cloud retarget --apply` is intentionally fail-closed. It cancels
matching GitHub Actions jobs first, uses whole-run cancellation only when every
active job in the run matches the target, and does not dispatch a replacement
if cancellation cannot be proven complete. When handling `event=cancel_failed`,
preserve the classification (`auth`, `scope`, `not_found`, `unsupported`,
`transient`, `unknown`), run/job URLs, manual recovery steps, and
branch-protection warning; do not collapse HTTP 404/not-found into an
`actions:write` scope hint unless the raw error also indicates auth or
permission trouble.

## Cutover Discipline

Release/cutover is a human decision, not an implementation side effect. Before
asking for go/no-go, ensure:

- Drift tracker has no untriaged upstream changes.
- CLI surface comparison is clean.
- CI, coverage, sandbox E2E, and GUI validation are green on the current Rust
  commit.
- Tailscale/GitHub live delivery is either passed in an approved reset window
  or explicitly risk-accepted.
- Signing/notarization and rollback paths are validated.
- Documentation changes for Shipyard, GUI, and Pulp/consumer pins are tracked.
