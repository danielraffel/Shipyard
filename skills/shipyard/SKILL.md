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

## GitHub Auth And Quota

Shipyard's operational GitHub calls can be configured with `[github.auth]`.
Default behavior is ambient `gh` auth. Configured env or command-helper tokens
are injected only into child `gh` commands as `GH_TOKEN`; Shipyard must never
write raw tokens, GitHub App private keys, Keychain items, 1Password sessions,
or token caches to config, state, logs, or release artifacts.

When debugging GitHub behavior:

- Run `shipyard doctor --rate-limit --json` to see the effective auth source
  and REST/GraphQL buckets. This actively resolves configured auth, so command
  helpers may run and GitHub App helpers may mint installation tokens.
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
- Prefer `shipyard config set <dotted.key> <value> --scope local|project|global`
  and `shipyard config unset ...` for operator setup changes. Do not tell users
  to hand-edit TOML when a CLI-backed setting write is available.
- Use `shipyard config export --output shipyard-setup.toml` for a portable
  non-secret setup bundle. It can be restored with `shipyard config import
  shipyard-setup.toml --from local --scope local`. It does not move raw tokens,
  PEM/private-key material, Keychain items, daemon sockets, queue state, logs,
  or token caches.

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
```

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

## Validation Gates

Prefer non-mutating checks first:

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

For controller/client multi-host designs, keep one controller as the sole
writer for shared queue, lease, ship, warm-pool, and cloud state. Do not use a
shared filesystem as a distributed state backend. Pair clients explicitly and
route remote enqueue/status through the authenticated protocol described in
`docs/multi-host-protocol.md`. `shipyard network tailscale status --json`
exposes the existing Tailscale probe for private tailnet reachability; this is
separate from Tailscale Funnel readiness used by webhook tunneling.
The implemented SSH-backed first slice includes `shipyard controller init`,
`shipyard controller invite`, `shipyard controller join --controller
ssh://... --token ...`, `shipyard controller status`, controller-backed
`shipyard status`, controller-backed `shipyard queue`, controller-backed
`shipyard logs`, controller-backed `shipyard evidence`, `shipyard --local-state
status`, `shipyard --local-state queue`, `shipyard --local-state logs`,
`shipyard --local-state evidence`, controller-backed `shipyard node list`,
`shipyard --local-state node list`, controller-backed
`shipyard watch --no-follow`, `shipyard --local-state watch --no-follow`,
controller-backed `shipyard targets pool status`, `shipyard --local-state
targets pool status`, `shipyard leave`, and controller-backed `shipyard node
remove <this-machine-id>`. Public laptop `shipyard run` uses the
authenticated/idempotent `rpc-enqueue` boundary to persist queued execution
envelopes on the controller. Treat public remote `shipyard ship`, streaming
`watch --follow`, mutating host-pool cleanup, revoking another node from a
client, and HTTPS controller RPC as planned, not shipped.

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
