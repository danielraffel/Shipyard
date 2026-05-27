# Shipyard Phase Handoff Status

Status: active
Last updated: 2026-05-27
Owner: current Codex session

This is the single handoff/status document for the current quota/auth and local
Mac pool work. Keep this file current as phases move. The detailed design docs
remain supporting references, but a new agent should be able to start here and
understand what changed, what was validated, and what to do next.

## Active Goal

Execute Shipyard quota/auth and local Mac pool work phase by phase while
maintaining a single handoff/status document that lets another agent resume
safely.

Current ordering:

1. Quota/auth GitHub boundary.
2. Local Mac pool planning and later implementation.
3. Adaptive queue routing only after local host-pool foundations exist.

## Worktree

| Field | Value |
|---|---|
| Worktree path | `/Users/danielraffel/Code/shipyard` |
| Worktree identity | primary checkout, not a `.claude/worktrees/agent-*` worktree |
| Branch | `feat/github-app-local-mac-queue` |
| HEAD at last update | Run `git rev-parse --short HEAD` in this checkout |
| Tracking status at last update | `feat/github-app-local-mac-queue...origin/feat/github-app-local-mac-queue` |
| Required repo instruction | Read `CLAUDE.md` before making changes. |

Local worktree status at this update:

```text
?? prompt-exports/
```

The untracked `prompt-exports/` directory is an intentional scratch artifact.
Do not revert unrelated user changes.

## Project Map

| Track | Primary doc | Current status | Next phase |
|---|---|---|---|
| Quota/auth | `planning/github-auth-boundary.md` | Q4 auth portability CLI done for the first implementation slice; local CLI/auth smoke passed with ambient `gh`, a `gh auth token` command helper, and a real GitHub App installation helper; the App installation verified a `12,500/hour` REST/GraphQL bucket across 259 repositories | No active auth phase unless user asks for more auth polish |
| Local Mac pool | `planning/local-mac-pool.md`, `planning/queue-concurrency.md` | Phase P2a host-pool dispatch/status/stale-lease cleanup foundation done; P2b.1 queue-state safety, P2b.2 request/outcome stores, additive `active_runs` queue/status output, P2b.3a-e submit/worker/cancellation factoring, P2b.3f drain-owned orphan request cancellation primitive, latest Claude plan review incorporation, P2b.4a shared-store locking primitives, P2b.4b ship-state writer migration, P2b.4c persisted resource-plan claims, P2b.4d host-pool lease job-id wiring, P2b.4e host-pool capacity primitive, P2b.4f host-pool lease-unavailable deferral signal, P2b.5a scheduler admission primitive, P2b.5b pure admit-pass planner, P2b.5c request-store-backed admit planning, P2b.5d same-PR ship admission planning, P2b.5e drain-owned queue mutation primitives, P2b.5f admit-pass queue application, P2b.5g durable request hydration, post-P2b.5g scheduler plan cleanup, P2b.5h started-job worker handoff, P2b.5i scheduler-deferred detection/requeue primitive, P2b.5j CLI durable-submit/durable-outcome entry-point swap, P2b.5k cooperative drain wait/ownership loop, P2b.5l bounded drain-owned worker spawn/reap cycle, P2b.5m request/outcome retention plus worker cap, P2b.5n queue-concurrency integration tests, and P2b.6 docs/skills cleanup done | Continue final packaging and PR readiness checks |
| Master handoff | this file | active | Update after every meaningful phase step |

## Current Implementation State

### Quota/Auth

Completed in this session:

- Added `src/gh.rs`.
- Exported `pub mod gh;` from `src/lib.rs`.
- Implemented `GhClient` as a shared GitHub CLI command boundary.
- Supported auth sources:
  - ambient `gh-cli`
  - env var token
  - argv-array command helper
- Added child-process `GH_TOKEN` injection for configured token sources.
- Added supervised and unsupervised `gh` command preparation.
- Added helper token parsing:
  - plain token stdout
  - JSON stdout with `token`, `expires_at`, and `kind`
- Added process-local command-helper cache with TTL or expiry/skew.
- Added repo placeholder expansion:
  - `{repo_slug}`
  - `{repo_owner}`
  - `{repo_name}`
  - `{cwd}`
- Added reusable helpers:
  - `parse_github_remote_slug`
  - `is_graphql_rate_limited`
- Migrated supervised PR/wait/auto-merge built-in `gh` paths to `GhClient`:
  - `src/pr.rs`
  - `src/wait_transport.rs`
  - `src/app/auto_merge_cmd.rs`
  - `src/app/pr_cmd.rs`
- Migrated cloud/reconcile runtime `gh` paths to `GhClient`:
  - `src/cloud.rs`
  - `src/reconcile.rs`
  - `src/app/cloud_cmd.rs`
  - `src/app/runner_cmd.rs`
  - `src/app/rescue_cmd.rs`
  - `src/app/cleanup_cmd.rs`
  - `src/app/ship_state_cmd.rs`
- `GitHubActions` now owns a process-local `GhClient` so command-helper tokens
  can be cached across a cloud operation.
- Active ship-state reconcile now constructs one `GhClient` per reconcile pass
  so command-helper tokens can be reused across state files.
- Migrated failure diagnostics and pin/update GitHub calls to `GhClient`:
  - `src/diagnostics.rs`
  - `src/app/ship_cmd.rs`
  - `src/pin.rs`
  - `src/app/pin_cmd.rs`
- Migrated `doctor --rate-limit` to use the effective configured auth.
- Migrated release-bot's local `gh` helper through `GhClient` with
  `GhAuthPolicy::AmbientOnly`, preserving the release-bot policy decision that
  setup/status use operator ambient auth rather than `[github.auth]`.
- Migrated governance branch-protection calls through a shared `GovernanceGh`
  wrapper that preserves explicit test binary overrides while using `GhClient`
  for operational command construction:
  - `src/governance.rs`
  - `src/branch.rs`
  - `src/app/branch_cmd.rs`
  - `src/app/governance_cmd.rs`
  - `src/app/ship_cmd.rs`
- Migrated registrar webhook create/update/delete calls through `GhClient`
  while preserving stdin piping, timeout handling, output classification, and
  fake-`gh` test overrides:
  - `src/registrar.rs`
  - `src/daemon_runtime.rs`
- Migrated legacy doctor release-chain/default-branch/secret-listing helpers
  through `GhClient` so they honor the active runtime mode and configured auth:
  - `src/doctor.rs`
  - `src/app/doctor_cmd.rs`
- Added Q3 source-aware auth diagnostics:
  - `shipyard doctor` now reports `Cloud providers/github-auth`.
  - `gh-scope` uses ambient `gh auth status` only when the effective source is
    ambient `gh-cli`.
  - Env, command-helper, and GitHub App helper auth are reported as configured
    tokens whose permissions require manual verification because they may not
    be locally inspectable through `gh auth status`.
  - `shipyard doctor --rate-limit` now includes an `auth` row before the REST
    and GraphQL bucket rows.
- Ran a focused Claude review of the Q3 diagnostics/docs slice and applied the
  actionable findings:
  - configured-token permission rows now render as manual verification required
    instead of a green verified check
  - helper stderr display redacts common GitHub token prefixes
  - docs/skills call out token-helper side effects and parent `GH_TOKEN`
    precedence under ambient `gh-cli`
- Added Q3 docs and skill notes:
  - `docs/install.md`
  - `RELEASING.md`
  - `skills/shipyard/SKILL.md`
  - `skills/ci/SKILL.md`
- Added Q4 auth portability CLI:
  - `shipyard auth doctor`
  - `shipyard auth export [--output <path>]`
  - `shipyard auth import <path> [--scope local|project|global]`
- Auth export writes a config-only bundle containing `[github.auth]`,
  requirement command/env names, and notes.
- Auth import writes only `[github.auth]` into the selected config layer,
  validates the supported auth schema, and rejects unknown auth keys so bundles
  cannot quietly smuggle direct token/private-key fields into Shipyard config.
- Built `target/debug/shipyard` from the current worktree and smoke-tested it
  on this Mac with the installed ambient `gh` credential.
- Smoke-tested `shipyard auth export`, `shipyard auth import`, and
  `shipyard auth doctor` in an isolated temp project.
- Smoke-tested configured command-helper auth with
  `token_command = ["gh", "auth", "token"]`; Shipyard resolved the helper and
  `doctor --rate-limit` used configured auth instead of ambient `gh-cli`.
- `src/gh.rs` now redacts cached tokens from `Debug` output by omitting the
  token cache from its formatter.
- Kept custom auto-merge command execution outside configured auth injection.
- Updated GraphQL rate-limit reset probes to use the same `GhClient` and auth
  policy as the failed command for the migrated PR/wait/auto-merge paths.
- Added `scripts/shipyard-github-app-token`, a zero-Python-dependency GitHub
  App installation token helper that uses `openssl` for JWT signing and Python
  stdlib for GitHub REST calls. This gives Shipyard's existing
  `token_command` auth path a concrete helper for real quota testing once a
  GitHub App is manually registered and installed.
- Added `scripts/test_shipyard_github_app_token.py` with offline helper
  regression coverage for base64url encoding, env fallback, invalid repo
  lookup input, and successful JSON output with signing/network calls mocked.
- Updated `docs/install.md` with the manual GitHub App registration/install
  boundary, helper environment variables, and the local helper command path.
- Added `docs/github-app-quota.md` as a dedicated GitHub App quota-extension
  guide, and added a bottom-of-README FAQ entry linking to it. The guide covers
  repository-count scaling, the 12,500/hour cap, GitHub App registration fields,
  Shipyard helper config, export/import portability, and quota validation.

Not done yet:

- Built-in GitHub App JWT signing is not implemented; GitHub App support still
  goes through external command helpers.
- The latest direct raw-`gh` audit shows only intentional central factories in
  `src/gh.rs` and `src/supervised.rs`. Remaining `fn gh` and `run_gh` helper
  names are wrappers already routed through `GhClient` or supervision.
- No manual smoke test has been run with a Keychain helper, 1Password helper,
  or env-var token distinct from the ambient `gh` login.
- Quota lifting is now verified for the real `shipyard-local` GitHub App
  installation on `danielraffel`: direct `gh api rate_limit` with the helper's
  installation token reported REST core and GraphQL limits of `12,500/hour`,
  and `/installation/repositories` reported 259 repositories.
- `target/debug/shipyard --mode isolated doctor --rate-limit` was also
  smoke-tested through a temp repo's configured `token_command` helper and
  reported `REST (core): 12498/12500 remaining` and
  `GraphQL: 12500/12500 remaining`.
- The GitHub App private key was moved out of iCloud Downloads to
  `/Users/danielraffel/.config/shipyard/github-apps/shipyard-local.private-key.pem`
  with file mode `0600`.
- This checkout now has a gitignored `.shipyard.local/config.toml` using an
  absolute helper path plus App ID, installation ID, and private-key path.
- A release binary from this checkout was built and installed to
  `/Users/danielraffel/.local/bin/shipyard`, with the previous binary backed
  up as
  `/Users/danielraffel/.local/bin/shipyard.pre-github-app-auth.20260526185834`.
  The installed local build reports `shipyard 0.58.0`.
- Normal `shipyard doctor --rate-limit` from this checkout now resolves
  `github-auth: ok command helper (github-app-installation)` and reports
  REST/GraphQL `12,500/hour` buckets.
- `shipyard doctor` / `shipyard doctor --rate-limit` are the right quota/auth
  visibility surfaces. `shipyard status` intentionally stays limited to
  queue/target/evidence state and does not spend GitHub API budget.
- `docs/cli-reference.md` and CLI help now document `doctor --rate-limit`,
  `auth doctor`, `auth export`, and `auth import`; auth export/import remain
  config-only and do not move tokens or private keys.

### Local Mac Pool

Completed in this session:

- Added `planning/local-mac-pool.md`.
- Built plan from RepoPrompt-selected context.
- Ran Claude pass 1 against the plan and RepoPrompt export.
- Incorporated pass 1 findings:
  - Current queue is one-active-job.
  - Phase 2a host-pool work does not provide parallel multi-Mac throughput.
  - Phase 2b is the separate queue-concurrency milestone.
  - Phase 3a adaptive routing uses serialized local depth until Phase 2b.
  - Scheduler-owned route plans are dropped on `Queue::enqueue` supersedence.
- Ran Claude pass 2.
- Claude pass 2 found no blockers and approved the plan as ready to use.
- Added Phase P1 docs/config guidance:
  - `docs/local-mac-pool.md`
  - `docs/targets.md`
  - `docs/workflows.md`
  - `skills/shipyard/SKILL.md`
  - `skills/ci/SKILL.md`
- P1 documents Mac Studio primary plus local fallback using existing SSH/local
  fallback. It explicitly says this is not load balancing, host-pool leasing,
  queue concurrency, or dynamic retargeting.
- Added the first Phase P2a host-pool foundation slice:
  - `src/host_pool.rs`
  - `src/app/targets_cmd.rs`
  - `src/app/cli.rs`
  - `src/lib.rs`
- `src/host_pool.rs` parses `[host_pools]` with ordered `ssh` and `local`
  members, default lease/heartbeat windows, member capabilities, and
  `max_concurrency`.
- `HostPoolLeaseStore` writes JSON lease state under
  `<state_dir>/host_pool/leases.json` with an advisory lock, acquire/release,
  heartbeat, and stale-prune primitives.
- `shipyard targets pool status` now reports configured pools, members,
  busy/idle state, available slots, active leases, and stale leases in human
  and JSON modes.
- Began Phase P2b queue-concurrency design work:
  - Bound RepoPrompt to `/Users/danielraffel/Code/shipyard`.
  - Selected queue/job/ship/run/dispatch/host-pool/preflight/warm/evidence/
    ship-state/planning context.
  - Asked RepoPrompt Oracle for a P2b implementation plan and exported it to
    `prompt-exports/oracle-plan-2026-05-26-144730-untitled-chat-8ec13f-c7b2.md`.
  - Inspected `src/daemon_runtime.rs` and `src/app/daemon_cmd.rs`; current
    daemon handles IPC/webhook/reconcile/status, not queued `run`/`ship`
    execution.
  - Conclusion so far: P2b needs a real scheduler/request-store design before
    implementation; there is no existing long-lived queue worker loop to wire
    into directly.
  - Added `planning/queue-concurrency.md` as the P2b design draft.
  - Ran a fresh Claude review of the P2b draft and incorporated the actionable
    findings into `planning/queue-concurrency.md`.
  - Claude review blockers incorporated:
    - stale-running recovery must be drain-owner-only, not a side effect of
      opening a non-drain queue handle
    - same-PR `ship` requests need explicit resume-aware behavior before
      implementation
    - running cancellation requires workers to re-read durable queue state
    - host-pool lease TOCTOU should return/retry work instead of recording a
      final busy target failure
    - fallback resource claims should only claim the primary backend at admit
      time
    - global `cloud-serial` should not be introduced in P2b
    - ship-state per-PR locks must cover all writers and the
      `resumed_existing_state` check
- Added the second Phase P2a host-pool dispatch slice:
  - `src/executor/dispatch.rs`
  - `src/app/run_cmd.rs`
  - `src/app/ship_cmd.rs`
- `backend = "host-pool"` now resolves to a named pool, filters members by
  target `requires`, materializes eligible `local` or `ssh` members, acquires a
  lease before validation, heartbeats the lease while validation runs, and
  releases it afterward.
- `shipyard run` and `shipyard ship` now construct dispatchers with the
  state-dir-backed host-pool lease store.
- Host-pool dispatch remains ordered and serialized under today's one-active
  job queue.
- Added the safe P2a cleanup surface:
  - `shipyard targets pool cleanup --dry-run`
  - `shipyard targets pool cleanup --fix`
- Pool cleanup currently prunes stale lease records from Shipyard's own state
  file. It does not delete remote workdirs or arbitrary local paths.
- Added P2b.3f drain-owned orphan request cancellation primitive:
  - `Queue::cancel_orphan_pending_jobs_for_drain` requires a held `DrainLock`
    and a request-envelope probe callback.
  - The primitive cancels only pending jobs whose request envelope is missing
    or unreadable, preserves running/completed jobs, keeps cancelled jobs in
    recent terminal history, and leaves scheduler/request-store wiring for the
    next slice.

Not done yet:

- Warm-pool status does not yet record or display host-pool `member_id`.
- Remote/workdir cleanup under explicit managed roots is not implemented.
- No Mac Studio target config has been applied.

## Validation

Commands run:

```bash
cargo fmt
cargo fmt -- --check
cargo test gh::
cargo test pr::
cargo test wait_transport::
cargo test app::auto_merge_cmd::
cargo test app::pr_cmd::
cargo test cloud::
cargo test reconcile::
cargo test app::cloud_cmd::
cargo test app::runner_cmd::
cargo test app::rescue_cmd::
cargo test app::ship_state_cmd::
cargo test app::cleanup_cmd::
cargo test diagnostics::
cargo test app::ship_cmd::
cargo test pin::
cargo test app::pin_cmd::
cargo test app::doctor_cmd::
cargo test app::release_bot_cmd::
cargo test governance::
cargo test branch::
cargo test app::governance_cmd::
cargo test app::branch_cmd::
cargo test registrar::
cargo test daemon_runtime::
cargo test doctor::
cargo test cloud::
cargo test doctor::
cargo test app::doctor_cmd::
cargo test gh::
cargo test app::auth_cmd::
cargo run --quiet -- --mode isolated --cwd <tmp> auth export
cargo run --quiet -- --mode isolated --cwd <tmp> --json auth doctor
cargo test host_pool::
cargo test app::targets_cmd::
cargo test executor::dispatch::
cargo test app::run_cmd::
cargo test app::ship_cmd:: -- --skip ship_command_green_merge_failure_keeps_active_state_and_exits_success
cargo run --quiet -- --mode isolated --cwd <tmp> --state-dir <tmp> --json targets pool status
cargo run --quiet -- --mode isolated --cwd <tmp> --state-dir <tmp> --json targets pool cleanup --dry-run
cargo run --quiet -- --mode isolated --cwd <tmp> --state-dir <tmp> --json targets pool cleanup --fix
cargo run --quiet -- --mode isolated --cwd <tmp git repo> --state-dir <tmp> --json run --targets mac
cargo run --quiet -- --mode isolated --cwd <tmp git repo> --state-dir <tmp> --json targets test mac
git diff --check
rg -n 'Command::new\("gh"\)' src
rg -n 'Command::new\("gh"\)|gh_supervised\(|fn gh\(|run_gh\(' src
cargo test
cargo test app::tests::auto_merge_failure_preserves_state -- --exact
cargo test app::ship_cmd::tests::ship_command_green_merge_failure_keeps_active_state_and_exits_success -- --exact
gh --version
cargo build
target/debug/shipyard --version
target/debug/shipyard --json auth doctor
target/debug/shipyard doctor --rate-limit
target/debug/shipyard --json auth export
target/debug/shipyard --json auth export --output <tmp bundle>
target/debug/shipyard --mode isolated --cwd <tmp project> --json auth import <tmp bundle> --scope local
target/debug/shipyard --mode isolated --cwd <tmp project> --json auth doctor
target/debug/shipyard --mode isolated --cwd <tmp command-helper project> --json auth import <tmp command-helper bundle> --scope local
target/debug/shipyard --mode isolated --cwd <tmp command-helper project> --json auth doctor
target/debug/shipyard --mode isolated --cwd <tmp command-helper project> doctor --rate-limit
```

Results:

- `cargo fmt -- --check`: passed.
- `cargo test gh::`: passed, 12 tests.
- `cargo test pr::`: passed, 7 tests.
- `cargo test wait_transport::`: passed, 12 tests.
- `cargo test app::auto_merge_cmd::`: passed, 6 tests.
- `cargo test app::pr_cmd::`: passed, 12 tests.
- `cargo test cloud::`: passed, 14 tests.
- `cargo test reconcile::`: passed, 7 tests.
- `cargo test app::cloud_cmd::`: passed, 23 tests.
- `cargo test app::runner_cmd::`: passed, 11 tests.
- `cargo test app::rescue_cmd::`: passed, 8 tests.
- `cargo test app::ship_state_cmd::`: passed, 13 tests.
- `cargo test app::cleanup_cmd::`: passed as a compile/filter check; 0 tests
  matched that filter.
- `cargo test diagnostics::`: passed, 17 tests.
- `cargo test app::ship_cmd::`: failed only the known
  `ship_command_green_merge_failure_keeps_active_state_and_exits_success`
  auto-merge failure; 6 passed, 1 failed.
- `cargo test pin::`: passed, 6 tests.
- `cargo test app::pin_cmd::`: passed, 19 tests.
- `cargo test app::doctor_cmd::`: passed, 5 tests.
- `cargo test app::release_bot_cmd::`: passed, 16 tests.
- `cargo test governance::`: passed, 4 tests.
- `cargo test branch::`: passed, 4 tests.
- `cargo test app::governance_cmd::`: passed, 4 tests.
- `cargo test app::branch_cmd::`: passed, 3 tests.
- `cargo test registrar::`: passed, 6 tests.
- `cargo test daemon_runtime::`: passed, 24 tests.
- `cargo test doctor::`: passed, 19 tests.
- `cargo test cloud::`: passed, 14 tests after the mode-aware
  `GitHubActions::from_cwd` change.
- After Q3 diagnostics/docs, `cargo test doctor::`: passed, 23 tests.
- After Q3 diagnostics/docs, `cargo test app::doctor_cmd::`: passed, 5 tests.
- After Q3 diagnostics/docs, `cargo test gh::`: passed, 13 tests.
- After Q4 auth CLI, `cargo test app::auth_cmd::`: passed, 5 tests.
- After Q4 auth CLI, `cargo run --quiet -- --mode isolated --cwd <tmp> auth export`:
  passed and printed a credential-free ambient `gh-cli` bundle.
- After Q4 auth CLI, `cargo run --quiet -- --mode isolated --cwd <tmp> --json auth doctor`:
  passed and printed an `auth.doctor` JSON envelope.
- After Q4 auth CLI, `cargo fmt -- --check`: passed.
- `git diff --check`: passed.
- After local Mac P1 docs, `git diff --check`: passed.
- After local Mac P2a lease/status foundation, `cargo test host_pool::`:
  passed, 4 tests.
- After local Mac P2a lease/status foundation, `cargo test app::targets_cmd::`:
  passed, 2 tests.
- After local Mac P2a dispatch wiring, `cargo test executor::dispatch::`:
  passed, 13 tests.
- After local Mac P2a dispatch wiring, `cargo test app::run_cmd::`: passed,
  6 tests.
- After local Mac P2a dispatch wiring,
  `cargo test app::ship_cmd:: -- --skip ship_command_green_merge_failure_keeps_active_state_and_exits_success`:
  passed, 6 tests. The skipped test is the known pre-existing auto-merge
  failure listed below.
- After local Mac P2a dispatch wiring, `cargo test host_pool::`: passed,
  4 tests.
- After local Mac P2a cleanup wiring, `cargo test app::targets_cmd::`:
  passed, 3 tests.
- After local Mac P2a lease/status foundation,
  `cargo run --quiet -- --mode isolated --cwd <tmp> --state-dir <tmp> --json targets pool status`:
  passed and printed a `targets.pool.status` JSON envelope with configured
  idle `ssh` and `local` members.
- After local Mac P2a dispatch wiring,
  `cargo run --quiet -- --mode isolated --cwd <tmp git repo> --state-dir <tmp> --json run --targets mac`:
  passed against a temporary `backend = "host-pool"` local member and returned
  a passing result with backend `host-pool:local_macs/local`.
- After local Mac P2a dispatch wiring,
  `cargo run --quiet -- --mode isolated --cwd <tmp git repo> --state-dir <tmp> --json targets test mac`:
  passed and reported the host-pool target reachable.
- After local Mac P2a cleanup wiring,
  `cargo run --quiet -- --mode isolated --cwd <tmp> --state-dir <tmp> --json targets pool cleanup --dry-run`:
  passed and printed a `targets.pool.cleanup` JSON envelope.
- After local Mac P2a cleanup wiring,
  `cargo run --quiet -- --mode isolated --cwd <tmp> --state-dir <tmp> --json targets pool cleanup --fix`:
  passed and printed a `targets.pool.cleanup` JSON envelope.
- After local Mac P2a lease/status foundation, `cargo fmt -- --check`: passed.
- After local Mac P2a lease/status foundation, `git diff --check`: passed.
- After local Mac P2a dispatch/docs updates, `cargo fmt -- --check`: passed.
- After local Mac P2a dispatch/docs updates, `git diff --check`: passed.
- After P2b.1 queue-state safety, `cargo test queue::`: passed, 17 tests.
- After P2b.1 queue-state safety, `cargo test job::`: passed, 11 tests.
- After P2b.1 queue-state safety, `cargo test app::run_cmd::`: passed,
  6 tests.
- After P2b.1 queue-state safety,
  `cargo test app::ship_cmd:: -- --skip ship_command_green_merge_failure_keeps_active_state_and_exits_success`:
  passed, 6 tests. The skipped test is the known pre-existing auto-merge
  failure listed below.
- After P2b.1 queue-state safety, app queue/status focused tests passed:
  - `cargo test app::tests::queue`
  - `cargo test app::tests::status_json_reports_empty_queue_and_targets`
  - `cargo test app::tests::cancel_json_marks_pending_job_cancelled`
- After P2b.1 queue-state safety, `cargo fmt -- --check`: passed.
- After P2b.1 queue-state safety, `git diff --check`: passed.
- After P2b.2 request/outcome stores, `cargo test queue_request::`: passed,
  5 tests.
- After P2b.2 request/outcome stores, `cargo test job::`: passed, 12 tests.
- After P2b.2 request/outcome stores, `cargo test queue::`: passed, 17 tests.
- After P2b.2 request/outcome stores, `cargo test app::run_cmd::`: passed,
  6 tests.
- After P2b.2 request/outcome stores,
  `cargo test app::ship_cmd:: -- --skip ship_command_green_merge_failure_keeps_active_state_and_exits_success`:
  passed, 6 tests. The skipped test is the known pre-existing auto-merge
  failure listed below.
- After P2b.2 request/outcome stores, app queue/status focused tests passed:
  - `cargo test app::tests::queue`
  - `cargo test app::tests::status_json_reports_empty_queue_and_targets`
  - `cargo test app::tests::cancel_json_marks_pending_job_cancelled`
- After P2b.2 request/outcome stores, `cargo fmt -- --check`: passed.
- After P2b.2 request/outcome stores, `git diff --check`: passed.
- After additive P2b queue/status `active_runs` compatibility output:
  - `cargo test app::tests::queue`: passed, 3 tests.
  - `cargo test app::tests::status_json`: passed, 2 tests.
  - `cargo test app::tests::cancel_json_marks_pending_job_cancelled`:
    passed, 1 test.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.3a inline request/outcome persistence:
  - `cargo test ship::`: passed, 12 tests.
  - `cargo test app::run_cmd::`: passed, 6 tests.
  - `cargo test app::ship_cmd:: -- --skip ship_command_green_merge_failure_keeps_active_state_and_exits_success`:
    passed, 6 tests. The skipped test is the known pre-existing auto-merge
    failure listed below.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
  - `cargo test queue_request::`: passed, 5 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.3b run submit/worker factoring:
  - `cargo test ship::`: passed, 13 tests.
  - `cargo test app::run_cmd::`: passed, 6 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.3c ship submit/worker factoring:
  - `cargo test ship::`: passed, 14 tests.
  - `cargo test app::ship_cmd:: -- --skip ship_command_green_merge_failure_keeps_active_state_and_exits_success`:
    passed, 6 tests. The skipped test is the known pre-existing auto-merge
    failure listed below.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.3d worker-side durable cancellation pickup:
  - `cargo test ship::`: passed, 16 tests.
  - `cargo test app::tests::cancel_json_marks_pending_job_cancelled`:
    passed, 1 test.
  - `cargo test app::run_cmd::`: passed, 6 tests.
  - `cargo test app::ship_cmd:: -- --skip ship_command_green_merge_failure_keeps_active_state_and_exits_success`:
    passed, 6 tests. The skipped test is the known pre-existing auto-merge
    failure listed below.
- After P2b.3e progress-callback cancellation pickup:
  - `cargo test ship::tests::run_worker_honors_durable_cancel_from_progress_callback`:
    passed, 1 test.
  - `cargo test ship::`: passed, 17 tests.
  - `cargo test app::run_cmd::`: passed, 6 tests.
  - `cargo fmt -- --check`: passed.
- After P2b.3f drain-owned orphan request cancellation primitive:
  - `cargo test queue::`: passed, 18 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After latest P2b queue-concurrency Claude review incorporation:
  - `git diff --check`: passed.
- After P2b.4a shared-store locking primitives:
  - `cargo test evidence::`: passed, 10 tests.
  - `cargo test warm_pool::`: passed, 9 tests.
  - `cargo test ship_state::`: passed, 11 tests.
  - `cargo test ship::`: passed, 17 tests.
  - `cargo test app::targets_cmd::`: passed, 3 tests.
  - `cargo test app::ship_cmd:: -- --skip ship_command_green_merge_failure_keeps_active_state_and_exits_success`:
    passed, 6 tests. The skipped test is the known pre-existing auto-merge
    failure listed below.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.4b ship-state writer migration:
  - `cargo test reconcile::`: passed, 7 tests.
  - `cargo test app::ship_state_cmd::`: passed, 13 tests.
  - `cargo test app::cloud_cmd::`: passed, 23 tests.
  - `cargo test daemon_runtime::`: passed, 24 tests.
  - `cargo test app::auto_merge_cmd::`: passed, 6 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.4c persisted resource-plan claims:
  - `cargo test queue_request::`: passed, 11 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.4d host-pool lease job-id wiring:
  - `cargo test executor::dispatch::`: passed, 13 tests.
  - `cargo test ship::`: passed, 17 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.4e host-pool capacity primitive:
  - `cargo test queue_scheduler::`: passed, 4 tests.
  - `cargo test queue_request::`: passed, 11 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.4f host-pool lease-unavailable deferral signal:
  - `cargo test executor::dispatch::`: passed, 14 tests.
  - `cargo test job::`: passed, 12 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.5a scheduler admission primitive:
  - `cargo test queue_scheduler::`: passed, 10 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.5b pure admit-pass planner:
  - `cargo test queue_scheduler::`: passed, 13 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.5c request-store-backed admit planning:
  - `cargo test queue_scheduler::`: passed, 16 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.5d same-PR ship admission planning:
  - `cargo test queue_scheduler::`: passed, 18 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.5e drain-owned queue mutation primitives:
  - `cargo test queue::`: passed, 20 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.5f admit-pass queue application:
  - `cargo test queue_scheduler::`: passed, 20 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After P2b.5g durable request hydration:
  - `cargo test queue_request::`: passed, 12 tests.
  - `cargo fmt -- --check`: passed.
  - `git diff --check`: passed.
- After post-P2b.5g scheduler plan cleanup:
  - Fresh Claude review attempt failed because the Claude CLI was not logged in.
  - `planning/queue-concurrency.md` was updated from a direct
    implementation-aware review of the remaining scheduler handoff.
  - `git diff --check`: passed.
- After P2b.5h started-job worker handoff and code sweep:
  - Fresh Claude review succeeded after login was restored and found the plan
    sound through P2b.5h, with remaining blockers around scheduler-deferred
    worker outcomes, lock-free planning assumptions, CLI entry-point swap,
    full-ship-state-lock duration, request/outcome retention, and cooperative
    cancellation UX.
  - `planning/queue-concurrency.md` was updated with the actionable Claude
    findings and the corrected next-step ordering.
  - `execute_run_worker` and `execute_ship_worker` now accept jobs already
    transitioned to `Running` by the drain owner, while preserving the existing
    synchronous pending-job start path.
  - `cargo fmt -- --check`: passed.
  - `cargo test ship::`: passed, 19 tests.
  - `cargo test queue::`: passed, 20 tests.
  - `cargo test queue_scheduler::`: passed, 20 tests.
  - `cargo test queue_request::`: passed, 12 tests.
  - `cargo test app::run_cmd::`: passed, 6 tests.
  - `cargo test app::ship_cmd:: -- --skip ship_command_green_merge_failure_keeps_active_state_and_exits_success`:
    passed, 6 tests.
  - `git diff --check`: passed after the final handoff update.
  - `cargo build --release`: passed.
  - Installed `target/release/shipyard` to `/Users/danielraffel/.local/bin/shipyard`
    after backing up the prior binary to
    `/Users/danielraffel/.local/bin/shipyard.pre-p2b5h.20260526193520`.
  - `shipyard --version`: passed, `shipyard 0.58.0`.
  - `shipyard doctor --rate-limit`: passed with
    `github-auth: ok command helper (github-app-installation)`, GraphQL
    `12500/12500`, and REST core `12494/12500`.
  - Full `cargo test`: failed with 766 passed and 2 failed:
    `app::ship_cmd::tests::ship_command_green_merge_failure_keeps_active_state_and_exits_success`
    and `app::tests::auto_merge_failure_preserves_state`. These are the same
    known auto-merge failures; do not claim full-suite green.
- After P2b.5i scheduler-deferred detection/requeue:
  - Added scheduler-defer metadata to `Job`, the drain-owned
    `Queue::requeue_deferred_running_jobs_for_drain` primitive, typed internal
    target execution outcomes, and scheduler-mode target execution deferral
    detection before final target-result persistence.
  - `cargo test queue::tests::drain_owner_requeues_scheduler_deferred_running_jobs`:
    passed, 1 test.
  - `cargo test ship::tests::scheduler_deferred_target_is_not_persisted_as_final_result`:
    passed, 1 test.
  - `cargo test queue::`: passed, 21 tests.
  - `cargo test ship::`: passed, 20 tests.
  - `cargo test queue_scheduler::`: passed, 20 tests.
  - `cargo test queue_request::`: passed, 12 tests.
  - `cargo test app::auth_cmd::`: passed, 5 tests.
  - `cargo test app::doctor_cmd::`: passed, 5 tests.
  - `cargo build`: passed.
- After P2b.5j CLI durable-submit/durable-outcome entry-point swap:
  - `shipyard run` command handling now calls `submit_run`, executes the worker
    for that submitted job, and renders by loading `QueueOutcomeStore` plus the
    final durable queue job through `load_run_outcome`.
  - `shipyard ship` command handling now calls `submit_ship`, executes the
    worker for that submitted job, and renders by loading `QueueOutcomeStore`
    plus the final durable queue job through `load_ship_outcome`.
  - `submit_ship` now refuses before enqueue when a matching same-repo,
    same-PR ship job is already running, with a message pointing to
    `shipyard watch --pr <pr>` or queue/status inspection.
  - Added durable outcome loader coverage and same-PR running refusal coverage
    in `ship::` tests.
  - `cargo test ship::`: passed, 21 tests.
  - `cargo test app::run_cmd::`: passed, 6 tests.
  - `cargo test app::ship_cmd:: -- --skip ship_command_green_merge_failure_keeps_active_state_and_exits_success`:
    passed, 6 tests.
  - `cargo test queue_scheduler::`: passed, 20 tests.
- After P2b.5k cooperative drain wait/ownership loop:
  - Added `CooperativeDrainOptions`, `drain_or_wait_run`, and
    `drain_or_wait_ship`.
  - After submitting a durable job, run/ship submitters now check durable
    terminal state, attempt to acquire `queue.lock`, run the submitted worker
    only when they own the drain lock, and otherwise poll durable state without
    dispatching work.
  - The current owner path still runs only the submitted job inline; sibling
    admission, request hydration, worker thread spawning/reaping, and
    scheduler-deferred requeue handling remain P2b.5l.
  - Focused tests prove the cooperative run path executes after acquiring the
    drain lock and does not dispatch while another process owns the drain lock.
  - `cargo test ship::`: passed, 23 tests.
  - `cargo test app::run_cmd::`: passed, 6 tests.
  - `cargo test app::ship_cmd:: -- --skip ship_command_green_merge_failure_keeps_active_state_and_exits_success`:
    passed, 6 tests.
  - `cargo test queue_scheduler::`: passed, 20 tests.
- After P2b.5l bounded drain-owned worker spawn/reap cycle:
  - The drain owner now snapshots queue jobs, creates a request-backed admit
    pass, applies drain-owned cancellations/starts, hydrates admitted durable
    requests, spawns scoped in-process workers, reaps them, and requeues
    scheduler-deferred host-pool lease misses.
  - Spawned workers open the actual queue root from the owner queue handle, so
    durable job updates land in the same queue even when tests use a separate
    queue root and runtime state dir.
  - `Queue::get_all` was added for scheduler snapshots.
  - Focused tests prove the cooperative run path executes the admitted worker
    after acquiring the drain lock.
  - `cargo test ship::`: passed, 23 tests.
  - `cargo test app::run_cmd::`: passed, 6 tests.
  - `cargo test app::ship_cmd:: -- --skip ship_command_green_merge_failure_keeps_active_state_and_exits_success`:
    passed, 6 tests.
  - `cargo test queue::`: passed, 21 tests.
  - `cargo test queue_scheduler::`: passed, 20 tests.
  - `cargo test queue_request::`: passed, 12 tests.
- After latest P2b queue-concurrency Claude review:
  - Fresh Claude review completed successfully and found the current design
    safe to continue with P2b.5m/P2b.5n, but called out plan edits needed
    before coding the next slice.
  - `planning/queue-concurrency.md` was updated to make P2b.5m cover
    outcome-before-terminal ordering, drain-acquired envelope sweeps for jobs
    absent from `queue.json`, and a conservative worker admission cap before
    broad integration tests.
  - Stale plan text was corrected for same-PR submit-time refusal, cooperative
    sibling-worker wait UX, scheduler-defer job fields, and the max-worker cap.
  - P2b.5n acceptance now explicitly includes dead drain-owner recovery after
    admit/start and before worker completion, plus a dedicated integration test
    file for end-to-end queue concurrency coverage.
- After P2b.5m request/outcome retention and worker cap:
  - `Queue::trim_terminal_jobs_for_drain` was added as a drain-owned terminal
    trim primitive that returns ids removed from `queue.json`.
  - `QueueRequestStore` and `QueueOutcomeStore` now support deleting individual
    envelopes and sweeping envelopes whose job ids are absent from `queue.json`
    after a grace window.
  - The drain-owned worker cycle trims terminal queue history, sweeps stale
    request/outcome envelopes, and caps admitted worker starts at two minus
    already-running jobs for the first concurrency release.
  - Run and ship workers now persist durable outcome envelopes before terminal
    `queue.update`, so losing submitters that observe terminal state can load
    the matching outcome.
  - Focused tests cover request/outcome envelope sweeping, drain-owned terminal
    trim ids, and the admission worker cap.
- After P2b.5n queue-concurrency integration tests:
  - Added `tests/queue_concurrency.rs`.
  - Integration tests prove non-conflicting jobs overlap under one drain owner,
    conflicting local-cwd jobs serialize across submitters, and a losing
    submitter waits on durable state without dispatching targets.
  - Integration tests also cover same-PR pending ship supersedence and abandoned
    drain recovery after admit/start, including durable outcome persistence for
    recovered jobs.
  - Stale-running recovery now returns recovered jobs, and drain-owned run/ship
    loops persist recovered durable outcomes so submitters do not hit terminal
    queue state without a matching outcome envelope.
- After P2b.6 status/docs/skills cleanup:
  - Updated `docs/local-mac-pool.md`, `docs/targets.md`, and
    `docs/workflows.md` so host-pool queue concurrency is described as
    available for non-conflicting jobs under one local drain owner.
  - Updated `skills/ci/SKILL.md` and `skills/shipyard/SKILL.md` with the same
    host-pool throughput and serialization caveats.
  - Refreshed `planning/local-mac-pool.md` and
    `planning/queue-concurrency.md` so P2b is no longer described as pending.
  - Added the final pre-release GUI compatibility task for
    `/Users/danielraffel/Code/shipyard-macos-gui`: verify the GUI still works
    with the updated SDK/CLI surfaces, reports macOS pool jobs, updates state,
    and is closed afterward.
- After macOS GUI compatibility check:
  - Reviewed `/Users/danielraffel/Code/shipyard-macos-gui` integration points.
    The GUI consumes additive `shipyard --json ship-state list` data through
    `ShipStateListPoller`/`WatchEvent`, already understands the `local`
    provider, renders Shipyard-dispatched running targets, and exposes
    retarget/add-lane provider choices including local macOS jobs.
  - `xcodebuild ... test` and signed Debug build attempts were interrupted
    because Xcode repeatedly probed a locked attached iOS device during
    destination/signing work. This is an environment/device issue, not a
    Shipyard SDK compile failure.
  - Unsigned macOS Debug build passed:
    `xcodebuild -project ShipyardMenuBar.xcodeproj -scheme ShipyardMenuBar -configuration Debug -sdk macosx -destination 'generic/platform=macOS' CODE_SIGNING_ALLOWED=NO build`.
  - The GUI app was not launched and was not left open. Orphaned signing
    processes from interrupted signed attempts were killed.
- After fresh 2026-05-27 Claude review of `planning/queue-concurrency.md`:
  - Claude found no new code blocker from the current plan, but flagged stale
    pre-implementation statements in this handoff and
    `planning/queue-concurrency.md`.
  - Updated both docs to reflect that P2b.1 through P2b.6 are implemented for
    the first concurrency release.
  - Documented current retention/backoff details:
    `QUEUE_ENVELOPE_SWEEP_GRACE = 60s`,
    `DEFAULT_DRAIN_MAX_WORKERS = 2`, and `scheduler_defer_until` is persisted
    for status/debugging but not yet used to delay admission.
- After Windows CI failure on PR #314 head `2a07b65`:
  - Windows `Run tests` failed in
    `doctor::tests::ready_depends_on_core_tools_only` because the advisory
    `shipyard-on-path` row affected `report.ready` in the CI PATH, and in
    three host-pool dispatch tests because Windows temp paths were
    interpolated into TOML double-quoted strings without escaping backslashes.
  - Fixed doctor readiness so advisory `shipyard-on-path` does not gate
    `report.ready`.
  - Fixed host-pool dispatch tests to TOML-escape command/cwd strings and use
    cross-platform local commands.
  - Local focused validation passed:
    `cargo fmt --all --check`,
    `cargo test doctor::tests::ready_depends_on_core_tools_only -- --exact`,
    `cargo test host_pool_dispatch -- --nocapture`, and
    `cargo clippy --all-targets --locked -- -D warnings`.
  - The next Linux CI run passed tests but failed clippy on
    `clippy::uninlined_format_args` in the same Windows-safe test command
    construction; fixed by using inline format variables and reran the focused
    tests/clippy locally.
  - The following PR run showed Linux green, then Windows failed because the
    host-pool lease test still depended on Windows shell/file quoting. The
    test now observes the active lease through Shipyard's progress callback
    while running a portable phase-marker echo.
  - The same PR run showed a coverage-only failure in a release-bot fake-`gh`
    test; the fake CLI argument matching is now wildcard-tolerant so the test
    cannot fall through to stdin prompting when the secret probe should match.
- Direct raw `Command::new("gh")` audit now reports only:
  - `src/supervised.rs`
  - `src/gh.rs`
- Broader helper audit still reports wrapper names in `src/pr.rs`,
  `src/app/auto_merge_cmd.rs`, `src/app/release_bot_cmd.rs`, `src/cloud.rs`,
  `src/registrar.rs`, `src/app/cloud_cmd.rs`, and `src/app/runner_cmd.rs`;
  these are central helpers or call through `GhClient`/`GitHubActions`.
- `cargo test`: failed with 694 passed and 2 failed.
- The two full-suite failures reproduce individually:
  - `app::tests::auto_merge_failure_preserves_state`
  - `app::ship_cmd::tests::ship_command_green_merge_failure_keeps_active_state_and_exits_success`
- These failures are outside `src/gh.rs` and were not changed in this quota
  slice.
- Full `cargo test` has not been rerun after the final governance, registrar,
  and doctor migration slice; focused tests and formatting/check audits passed.
- `gh` version observed: `gh version 2.92.0 (2026-04-28)`.
- `cargo build`: passed and produced local debug CLI `target/debug/shipyard`.
- `target/debug/shipyard --version`: passed, `shipyard 0.58.0`.
- `target/debug/shipyard --json auth doctor`: passed with ambient
  `gh-cli`.
- `target/debug/shipyard doctor --rate-limit`: passed with ambient `gh-cli`;
  observed GraphQL `1590/5000` remaining and REST core `4940/5000` remaining.
- No `GH_TOKEN`, `GITHUB_TOKEN`, or `RELEASE_BOT_TOKEN` env var was present in
  the agent shell during the smoke test.
- `target/debug/shipyard --json auth export`: passed and emitted a sanitized
  ambient `gh-cli` config bundle with no secrets.
- `target/debug/shipyard --json auth export --output <tmp bundle>` plus
  isolated `auth import` and `auth doctor`: passed, importing into
  `<tmp project>/.shipyard-dev.local/config.toml` and resolving ambient
  `gh-cli`.
- Isolated command-helper bundle with
  `token_command = ["gh", "auth", "token"]`: `auth import` passed,
  `auth doctor` resolved `command helper`, and `doctor --rate-limit` passed
  with configured auth. Observed GraphQL `1128/5000` remaining and REST core
  `4924/5000` remaining.
- The command-helper smoke did not prove quota lifting because the helper
  returned the same ambient `gh` credential. A distinct higher-limit token or
  GitHub App installation helper is still needed to verify raised quotas.
- After adding `scripts/shipyard-github-app-token`:
  - `python3 -m unittest scripts/test_shipyard_github_app_token.py`: passed, 4
    tests.
  - `python3 -m py_compile scripts/shipyard-github-app-token scripts/test_shipyard_github_app_token.py`:
    passed.
  - `scripts/shipyard-github-app-token --help`: passed.
  - `git diff --check`: passed.
- Real GitHub App quota smoke:
  - `scripts/shipyard-github-app-token --repo danielraffel/shipyard`: passed
    with `kind=github-app-installation` and an expiring token; token value was
    not printed.
  - `GH_TOKEN=<installation-token> gh api rate_limit`: passed with REST core
    `12500/12500` and GraphQL `12500/12500`.
  - `GH_TOKEN=<installation-token> gh api /installation/repositories --paginate`:
    passed and counted 259 repositories.
  - `target/debug/shipyard --mode isolated doctor --rate-limit` in a temp repo
    configured with the App token helper: passed with REST core
    `12498/12500` and GraphQL `12500/12500`.
- After installing the local release binary and moving the key:
  - `shipyard --version`: passed, `shipyard 0.58.0`.
  - `shipyard doctor --rate-limit`: passed with
    `github-auth: ok command helper (github-app-installation)`, REST core
    `12496/12500`, and GraphQL `12500/12500`.
  - `shipyard auth export --output <tmp>`: passed. The export contained the
    absolute helper path and did not contain PEM contents or GitHub token
    prefixes.
  - `shipyard --mode isolated auth import <export> --scope local` in a temp
    repo: passed.
- After adding the README quota FAQ and `docs/github-app-quota.md`:
  - Official GitHub REST API rate-limit docs were rechecked on 2026-05-27.
  - `git diff --check`: passed.
  - Imported temp repo `shipyard --mode isolated auth doctor`: passed with
    `github-auth: ok command helper (github-app-installation)`.
  - Imported temp repo `shipyard --mode isolated doctor --rate-limit`: passed
    with REST core `12495/12500` and GraphQL `12500/12500`.
- After the doctor/auth CLI help and `docs/cli-reference.md` update:
  - `target/debug/shipyard doctor --help`: passed.
  - `target/debug/shipyard auth --help`: passed.
  - `target/debug/shipyard auth export --help`: passed.
  - `target/debug/shipyard auth import --help`: passed.
  - Decision recorded: quota/auth status belongs in
    `shipyard doctor --rate-limit`; `shipyard status` should not make GitHub
    API calls.

## Phase Plan

### Phase Q1 - Shared GitHub Boundary

Status: done for the current slice.

Acceptance for completing Q1:

- `src/gh.rs` exists and compiles.
- Config parsing and token resolution tests pass.
- Command preparation preserves caller-owned args, stdio, timeout, and output
  handling.
- Supervised command setup can carry both `SHIPYARD_PR_RUNNING=1` and
  `GH_TOKEN`.
- Planning doc status is updated.

### Phase Q2 - Migrate Operational `gh` Call Sites

Status: done for the currently identified built-in call sites.

Suggested order:

1. Finish supervised PR/wait/auto-merge paths:
   - `src/pr.rs` - migrated
   - `src/wait_transport.rs` - migrated
   - `src/app/auto_merge_cmd.rs` - migrated for built-in `gh`; custom merge
     commands intentionally bypass configured auth
   - `src/app/pr_cmd.rs` - migrated
2. Migrate cloud and reconcile paths:
   - `src/cloud.rs` - migrated
   - `src/reconcile.rs` - migrated
   - `src/app/cloud_cmd.rs` - migrated
   - `src/app/runner_cmd.rs` - migrated
   - `src/app/rescue_cmd.rs` - migrated
   - `src/app/cleanup_cmd.rs` - migrated
   - `src/app/ship_state_cmd.rs` - migrated
   - `src/diagnostics.rs` - migrated
3. Classify and migrate:
   - `src/pin.rs` - migrated
   - `src/app/pin_cmd.rs` - migrated
   - `src/governance.rs` - migrated
   - `src/branch.rs` - migrated where it applies governance rules
   - `src/app/branch_cmd.rs` - migrated
   - `src/app/governance_cmd.rs` - migrated
   - `src/registrar.rs` - migrated
   - `src/daemon_runtime.rs` - migrated where it constructs `Registrar`
4. Migrate release-bot with `GhAuthPolicy::AmbientOnly`:
   - `src/app/release_bot_cmd.rs` - migrated
5. Migrate legacy doctor operational helpers:
   - `src/doctor.rs` - migrated for release-chain/default-branch/secret-listing
   - `src/app/doctor_cmd.rs` - migrated for rate-limit and release-chain entry
     points

Rules:

- Do not inject configured auth into custom user-provided merge commands.
- Do not silently fall back to ambient auth when configured env/command auth
  fails.
- GraphQL reset probes must use the same `GhClient` and auth policy as the
  failed command.

### Phase Q3 - Auth Diagnostics And Docs

Status: done for the first implementation slice.

Scope:

- `doctor --rate-limit` includes the effective auth source and probes with
  configured auth.
- Doctor distinguishes ambient auth, env token, command token, GitHub App
  helper kind/expiry, and non-inspectable configured-token permissions.
- Docs cover env, Keychain, 1Password, GitHub App helper, and Mac-to-Mac
  credential portability.
- Relevant skills have been updated.

### Phase Q4 - Auth Portability CLI

Status: done for the first implementation slice; local CLI smoke passed.

Scope:

- `shipyard auth doctor` implemented.
- `shipyard auth export` implemented.
- `shipyard auth import` implemented.
- Config-only export/import. No secrets, tokens, private keys, keychain items,
  queue state, daemon sockets, or runtime state.
- Local smoke verified ambient export/import and a command-helper configured
  auth path. Higher-limit/App token smoke remains pending.

### Phase P1 - Local Mac Pool Docs/Config

Status: done.

Scope:

- Document Mac Studio primary plus local fallback using existing SSH/local
  fallback.
- No load balancing.
- No busy/idle scheduling.
- No adaptive retargeting.

### Phase P2a - Host-Pool Leases And Status

Status: done for the first implementation slice.

Scope:

- Add `host-pool` target support.
- Add pool members, leases, heartbeat, stale lease handling, pool status, and
  safe cleanup.
- Keep today's one-active-job queue.

Current slice completed:

- Pool config parsing.
- JSON-backed lease store with stale handling primitives.
- `shipyard targets pool status`.
- `ResolvedBackend::HostPool` target resolution.
- Ordered member selection with `requires` filtering.
- Lease acquire, heartbeat, and release around real local/SSH validation.
- `shipyard targets pool cleanup --dry-run` and `--fix` for stale lease
  records.

Still pending in P2a:

- Warm-pool member identity in status.
- Remote/workdir cleanup under explicit managed roots.

### Phase P2b - Queue Concurrency

Status: P2b.1 through P2b.6 are implemented for the first concurrency release.

Scope:

- Extend queue from one active job to concurrent non-conflicting jobs.
- Required before multiple local Macs can drain multiple queued jobs in
  parallel.
- Current design notes:
  - `Queue` is now a file-backed handle guarded by `queue.state.lock`; the
    submitter-owned drain scheduler performs all pending-to-running
    transitions under `queue.lock`.
  - `queue_cmd` JSON/human output now preserves singular `active`/
    `active_run` compatibility and also exposes additive `active_runs`.
  - Host-pool leases now carry the owning queue job id.
  - The daemon is not currently a queue runner; it is an IPC/webhook/
    reconcile/status process.
  - P2b uses a submitter-owned cooperative drain controller plus durable
    request/outcome stores; daemon-owned drain is deferred to a later explicit
    phase.
  - Fresh Claude review on 2026-05-26 found the design direction sound after
    edits, but not safe to implement until key blockers were incorporated.
  - Fresh Claude review on 2026-05-27 found no code blocker from the plan, but
    called out stale handoff/plan language; those doc edits are incorporated.
  - Incorporated review edits include:
    - move stale-running recovery out of `Queue::load()` into a drain-owner
      scheduler recovery pass
    - make non-drain queue handles read-only on load
    - add orphan pending-job cancellation for missing/unreadable request
      envelopes
    - require workers to re-read durable queue state for cooperative
      cancellation
    - change host-pool lease race handling from final busy target failure to
      pending/retry scheduling behavior
    - claim only primary fallback backend resources at admit time
    - avoid global `cloud-serial` and allow unrelated cloud jobs to overlap
    - broaden ship-state per-PR locking to all writers and the
      `resumed_existing_state` check
  - Same-PR `ship` behavior is now resolved for P2b:
    - if the existing same-PR ship job is `Pending`, cancel it with reason
      `superseded by newer ship request for the same PR` and enqueue the newer
      request
    - if the existing same-PR ship job is `Running`, refuse the newer request
      before enqueue and point the user to `shipyard watch --pr <pr>` or
      queue/status inspection
    - do not attach a second synchronous CLI to a running worker in P2b
  - P2b.1 queue-state safety implementation:
    - `Queue` no longer caches `jobs: Vec<Job>` or performs one-time lazy
      loading.
    - Added short-lived `queue.state.lock` around queue snapshot reads and
      read/modify/write operations.
    - `Queue::load()` side-effect recovery was removed; ordinary queue readers
      no longer mutate `queue.json`.
    - Added explicit `recover_stale_running_jobs_for_drain(&DrainLock)` for
      future drain-owner recovery.
    - Added `Queue::get_running()` and kept `get_active()` as a first-running
      compatibility helper.
    - Pending supersedence now marks old pending jobs `Cancelled` with a
      cancellation reason instead of deleting them.
    - Recent/terminal trimming now includes cancelled jobs.
    - Added `Job::cancellation_reason` and `Job::cancel_with_reason`.
  - P2b.2 request/outcome store implementation:
    - Added `src/queue_request.rs` with queue-owned serde snapshots rather
      than making runtime executor config structs a stable serde contract.
    - Added request envelopes under `<state_dir>/queue/requests/<job_id>.json`
      with schema version, job id, kind, cwd, created timestamp, resource plan,
      and resolved run/ship request snapshots.
    - Added outcome envelopes under `<state_dir>/queue/outcomes/<job_id>.json`
      for run and ship completions; ship outcomes include PR, ship state, and
      `resumed_existing_state`.
    - Store loads reject unsupported schema versions.
    - Request snapshots preserve repo/PR identity for same-PR ship detection and
      cover local, SSH, Windows SSH, cloud, host-pool, and fallback target
      shapes without token/secret fields.
    - Added optional `JobKind`, `cancel_requested_at`, and resource-claim debug
      fields to `Job`; legacy jobs remain readable through serde defaults.
    - `execute_run` and `execute_ship` now tag inline-created jobs with
      `JobKind::Run` and `JobKind::Ship`.
  - P2b.3a inline request/outcome persistence implementation:
    - `RunStores` and `ShipStores` now carry the original CLI cwd.
    - Current inline `execute_run` and `execute_ship` save a durable request
      envelope before enqueueing the job.
    - Current inline `execute_run` and `execute_ship` save an outcome envelope
      after terminal queue/state writes.
    - Unit tests prove final run and ship request/outcome envelopes can be read
      back from disk for the completed job id.
  - P2b.3b run submit/worker factoring implementation:
    - Added `submit_run` to persist the request envelope and pending job without
      executing it.
    - Added `execute_run_worker` to start and complete a previously submitted
      run job, preserving the current synchronous `execute_run` wrapper for the
      CLI.
    - Added a test proving `submit_run` leaves a pending durable job, writes
      the run request envelope, and does not write an outcome before execution.
  - P2b.3c ship submit/worker factoring implementation:
    - Added `submit_ship` to persist the request envelope and pending job
      without creating ship state or executing targets.
    - Added `execute_ship_worker` to own ship-state load/create/save, start and
      complete the submitted job, and write the ship outcome.
    - If ship-state validation refuses before the job starts, the worker now
      cancels the pending job with the refusal reason so the queue does not
      retain a stale pending ship.
    - Added a test proving `submit_ship` leaves a pending durable job, writes
      the ship request envelope, and does not create ship state or an outcome
      before execution.
  - P2b.3d worker-side durable cancellation pickup implementation:
    - Workers now re-read durable queue state before starting a submitted job
      and before/after each target.
    - If a durable job is already cancelled before worker start, run/ship
      workers return the cancelled job without dispatching targets.
    - If a job is cancelled between targets, `execute_targets` returns the
      durable cancelled job so workers do not overwrite cancellation with a
      completion transition.
    - Added tests proving run and ship workers honor durable cancellation before
      start without invoking the dispatcher; ship also avoids creating ship
      state in that path.
  - P2b.3e progress-callback cancellation pickup implementation:
    - Progress callbacks now check durable queue cancellation before updating
      progress and again after saving progress.
    - If cancellation is observed from a progress callback, `execute_targets`
      returns the durable cancelled job instead of overwriting it with the
      target result.
    - Added a run-worker test that cancels the durable job during a progress
      callback and proves the worker returns the cancelled job.
  - P2b.3f drain-owned orphan request cancellation primitive implementation:
    - `Queue::cancel_orphan_pending_jobs_for_drain` requires a held
      `DrainLock` and a request-envelope probe callback.
    - The primitive cancels only pending jobs whose request envelope is missing
      or unreadable, preserves running/completed jobs, keeps cancelled jobs in
      recent terminal history, and leaves scheduler/request-store wiring for
      P2b.5.
  - Latest Claude review against the current implementation was run on
    2026-05-26 and incorporated into `planning/queue-concurrency.md`.
  - Latest review blockers now assigned in the plan:
    - same-PR ship admission must scan pending/running ship jobs via
      `QueueRequestStore`; generic queue supersedence is not sufficient
    - only the drain owner may call `execute_run_worker` or
      `execute_ship_worker`; losing submitters must wait on durable state
    - host-pool lease TOCTOU needs a scheduler-level deferred/lease-unavailable
      contract instead of persisting transient busy as a final target failure
    - production host-pool lease requests still need
      `HostPoolLeaseRequest.job_id = Some(job.id.clone())`
    - ship-state per-PR locking, evidence per-branch locking, and warm-pool JSON
      locking are owned by P2b.4 before concurrent workers are admitted
    - the current persisted `JobResourcePlan` is preliminary and must be
      extended or replaced by admit-time derivation before it is authoritative
  - P2b.4a shared-store locking implementation:
    - `EvidenceStore::record` now performs branch read/modify/write under a
      per-branch evidence lock.
    - Added `EvidenceStore::with_branch_records_locked` for future scheduler
      updates that need an evidence critical section.
    - `WarmPool::save_entries`, `upsert`, `evict`, `drain`, and
      `prune_expired` now mutate under a warm-pool file lock.
    - Added `WarmPool::with_entries_locked` for future scheduler mutations that
      need to keep warm-pool read/modify/write in one critical section.
    - `ShipStateStore` now has per-PR lock helpers:
      `lock_pr`, `get_locked`, `save_locked`, `archive_locked`,
      `archive_and_replace_locked`, and `with_pr_state_locked`.
    - `execute_ship_worker` now holds the per-PR ship-state lock across its
      ship-state lifecycle, including the `resumed_existing_state` check,
      initial save, and final save.
    - Tests cover two-handle mutations for evidence, warm-pool, and ship-state
      helper APIs.
  - P2b.4c persisted resource-plan claims implementation:
    - `JobResourcePlan` keeps the existing compatibility fields (`targets`,
      `cloud_targets`, `host_pools`) and now adds sorted `exclusive_claims`.
    - Run and ship request envelope construction derives resource plans from
      the full request context, including branch, original CLI cwd, and ship
      `(repo, pr)` identity.
    - Resource plans now claim local cwd, SSH repo, Windows repo, evidence,
      warm-pool, and ship-state resources.
    - Cloud targets remain non-exclusive and only populate `cloud_targets`.
    - Host-pool targets produce pool demands with slots and capability keys
      instead of serializing every concrete member.
    - Fallback targets claim only the primary backend at admit time.
    - Focused `queue_request::` tests cover each backend's claims.
  - P2b.4d host-pool lease job-id wiring implementation:
    - `DispatchValidationRequest` now carries an optional queued job id.
    - `execute_targets` passes the durable queue job id to target dispatch.
    - Fallback and host-pool nested dispatch preserve the job id.
    - Host-pool lease acquisition now sets `HostPoolLeaseRequest.job_id` from
      the queued dispatch request.
    - Focused dispatcher coverage validates that the in-flight host-pool lease
      JSON contains the queued job id during member execution.
  - P2b.4e host-pool capacity primitive implementation:
    - Added `src/queue_scheduler.rs` with
      `host_pool_capacity_deficits(candidate, running, pools, leases, now)`.
    - The primitive counts configured eligible member capacity by pool and
      required capabilities, subtracts non-stale active leases, subtracts
      overlapping running resource-plan reservations, and returns structured
      deficits without starting workers.
    - Persisted host-pool resource demands now fold duplicate same-pool
      demands by increasing `slots` instead of deduping repeated targets away.
    - Focused tests cover a second admissible host-pool job with two members,
      exhausted running reservations, stale lease exclusion, and missing-pool
      deficits.
  - P2b.4f host-pool lease-unavailable deferral signal implementation:
    - `DispatchValidationRequest` now has
      `defer_host_pool_lease_unavailable` for scheduler-owned dispatch.
    - Existing synchronous CLI behavior is preserved: the flag is false on the
      current inline `execute_targets` path, so busy host-pool members keep the
      existing terminal busy/failover behavior.
    - When the flag is true and `HostPoolLeaseStore::acquire` returns `None`,
      host-pool dispatch returns a non-terminal `TargetStatus::Pending` result
      with `scheduler_defer_reason = "host_pool_lease_unavailable"` and no
      failure class.
    - `TargetResult::is_scheduler_deferred` identifies scheduler-deferred
      results.
    - Focused dispatcher coverage validates the scheduler-mode deferral path.
  - P2b.4 remaining before scheduler admission:
    - audit any newly-added ship-state writers before scheduler work starts and
      keep future writers on the locked helper APIs
  - P2b.5a scheduler admission primitive implementation:
    - `queue_scheduler::admission_blockers` reports exclusive-claim conflicts
      and host-pool capacity deficits for a candidate resource plan.
    - `queue_scheduler::can_admit` returns whether a pending plan can run beside
      currently running plans and active host-pool leases.
    - Focused tests cover same local cwd, same SSH repo, same Windows repo,
      same PR ship-state, unrelated cloud overlap, host-pool two-member
      capacity, exhausted host-pool capacity, and fallback secondary
      non-serialization.
    - This is pure planning logic only: it does not acquire the drain lock,
      transition queue jobs, spawn workers, consume deferred target results, or
      enforce same-PR ship admission through `QueueRequestStore`.
  - P2b.5b pure admit-pass planner implementation:
    - `queue_scheduler::plan_admit_pass` consumes already-sorted pending
      admission requests, currently running resource plans, host-pool config,
      and leases.
    - It greedily returns admitted job ids, deferred jobs with blockers, and
      orphaned pending jobs whose request envelopes are missing or unreadable.
    - Focused tests prove newly admitted jobs block later conflicting pending
      jobs, missing request envelopes are reported for later drain-owned
      cancellation, host-pool capacity defers a blocked job, and independent
      later jobs can still be admitted.
    - This remains pure planning logic only: it does not acquire the drain
      lock, load requests from disk, mutate queue state, spawn workers, consume
      deferred target results, or enforce same-PR ship admission.
  - P2b.5c request-store-backed admit planning implementation:
    - `queue_scheduler::plan_admit_pass_from_jobs` loads pending and running
      request envelopes from `QueueRequestStore` before calling the pure admit
      planner.
    - Pending jobs are sorted by scheduler priority/FIFO rules before
      admission planning.
    - Missing or unreadable pending request envelopes are surfaced as orphaned
      pending jobs for later drain-owned cancellation.
    - Missing or unreadable running request envelopes are surfaced as running
      request load errors so the future drain loop can avoid admitting new work
      when occupied resources are unknown.
    - Focused tests cover request-backed sorting, missing pending envelopes,
      and missing running envelopes.
    - This remains planning logic only: it does not acquire the drain lock,
      mutate queue state, spawn workers, consume deferred target results, or
      perform same-PR ship queue mutation.
  - P2b.5d same-PR ship admission planning implementation:
    - `queue_scheduler::plan_admit_pass_from_jobs` now reports same-PR ship
      admission decisions based on loaded `QueuedExecutionRequest::Ship`
      envelopes.
    - Older pending same-PR ship jobs are surfaced as pending cancellations for
      later drain-owned queue mutation and are excluded from the generic admit
      plan.
    - Pending same-PR ship jobs are surfaced as running conflicts, and excluded
      from the generic admit plan, when a matching ship job is already running.
    - Focused tests cover older pending same-PR cancellation and pending
      same-PR running conflict detection.
    - This remains planning logic only: it does not acquire the drain lock,
      mutate queue state, spawn workers, or consume deferred target results.
  - P2b.5e drain-owned queue mutation primitive implementation:
    - Added `QueuePendingCancellation`.
    - Added `Queue::start_pending_jobs_for_drain`, which requires a held
      `DrainLock` and transitions selected pending jobs to running in the
      admit-plan order.
    - Added `Queue::cancel_pending_jobs_for_drain`, which requires a held
      `DrainLock` and cancels selected pending jobs by id with caller-provided
      reasons.
    - Focused tests cover selected pending start order, duplicate id handling,
      ignoring non-pending jobs, selected pending cancellation, and recent
      terminal retention.
    - Full admit-pass orchestration, worker spawning, and deferred/requeue
      handling remain pending.
  - P2b.5f admit-pass queue application implementation:
    - Added `queue_scheduler::apply_admit_pass_for_drain`.
    - It consumes a request-backed admit pass, cancels orphaned and superseded
      same-PR pending jobs through `Queue::cancel_pending_jobs_for_drain`, and
      starts admitted jobs through `Queue::start_pending_jobs_for_drain`.
    - It skips starting admitted jobs when running request envelopes failed to
      load, preventing the scheduler from admitting work while occupied
      resources are unknown.
    - Focused tests cover cancelling orphan/same-PR jobs before starting
      admitted jobs and skipping starts when a running request envelope is
      missing.
    - Worker spawning, submitter wait/drain ownership, and scheduler-deferred
      target requeue handling remain pending.
  - P2b.5g durable request hydration implementation:
    - `QueuedExecutionEnvelope::to_run_request` and `to_ship_request` convert
      durable request snapshots back into executable request values.
    - Reverse target conversion preserves local, SSH, Windows, cloud,
      host-pool, fallback, validation, contract, and failure-parser fields.
    - Focused tests cover run/ship restoration and nested host-pool plus
      fallback target restoration.
    - Worker spawning, submitter wait/drain ownership, and scheduler-deferred
      target requeue handling remain pending.
  - Post-P2b.5g scheduler plan cleanup:
    - An initial requested fresh Claude review could not run because the Claude
      CLI returned `Not logged in · Please run /login`.
    - A direct implementation-aware review updated
      `planning/queue-concurrency.md` around the remaining scheduler work.
    - The plan now calls out the concrete blocker that
      `apply_admit_pass_for_drain` starts jobs as `Running` while the existing
      `execute_run_worker` and `execute_ship_worker` helpers still call
      `job.start()` internally. P2b.5h must add scheduler-safe worker
      entrypoints or make worker start idempotent before spawning admitted jobs.
    - The plan now separates P2b.5i scheduler-deferred host-pool requeue/
      backoff from the later cooperative drain loop, and requires a
      drain-owned `Running -> Pending` requeue primitive for transient
      lease-unavailable results.
    - The plan now clarifies that workers remain responsible for outcome
      snapshot persistence unless a future slice deliberately moves that
      responsibility into the scheduler.
    - The plan now clarifies that the scheduler admit-pass same-PR guard does
      not replace submit-time same-PR ship refusal while a matching ship job is
      already running.
    - The next-step section now points to P2b.5h/P2b.5i/cooperative drain
      loop work instead of stale P2b.4 resource planning.
  - P2b.5h started-job worker handoff:
    - A later Claude retry succeeded and reviewed the current plan through
      P2b.5h.
    - `ensure_worker_running_job` now lets `execute_run_worker` and
      `execute_ship_worker` accept either a pending job from the synchronous
      wrapper or an already-running job started by the drain owner.
    - Focused tests prove run and ship workers accept jobs started by
      `Queue::start_pending_jobs_for_drain` without attempting a second
      `job.start()` call.
    - Claude review findings incorporated into `planning/queue-concurrency.md`:
      scheduler-deferred target results must be detected before persistence,
      lock-free planning versus locked apply must be treated as intentional,
      run/ship CLI entry-point swap is its own scheduler slice, per-PR
      ship-state locking across the target loop is an explicit decision,
      request/outcome retention needs a drain-owned trim policy, and
      cooperative cancellation UX should use a standard operator reason.
    - Remaining scheduler work after P2b.5h started at P2b.5i.
  - P2b.5i scheduler-deferred detection/requeue implementation:
    - `Job` now records scheduler-defer metadata:
      `scheduler_defer_reason`, `scheduler_defer_count`, and
      `scheduler_defer_until`.
    - `Job::defer_for_scheduler` transitions a running job back to pending,
      clears started/completed timestamps, preserves terminal target results,
      and removes non-terminal in-flight target results.
    - `Queue::requeue_deferred_running_jobs_for_drain` requires a held
      `DrainLock`, dedupes selected job ids, ignores non-running jobs, and
      applies the defer metadata for transient scheduler deferrals.
    - `execute_targets_with_options` now accepts scheduler mode, passes
      `defer_host_pool_lease_unavailable` into dispatch, detects
      scheduler-deferred target results before persisting them as final target
      results, and returns a typed internal deferred outcome.
    - Existing synchronous `execute_run` / `execute_ship` behavior is
      preserved through `TargetExecutionOutcome::into_completed`; the new
      deferred path is for the future drain-owned scheduler worker flow.
    - Focused tests prove a scheduler-deferred target is not persisted as a
      final target result and that the drain-owned queue requeue primitive
      preserves terminal target results while clearing non-terminal ones.
  - P2b.5j CLI durable-submit/durable-outcome entry-point swap:
    - `shipyard run` and `shipyard ship` no longer call the legacy
      submit-then-inline `execute_run` / `execute_ship` wrappers from the CLI
      handlers.
    - CLI handlers now call `submit_run` / `submit_ship` and render from
      durable queue/outcome state via `load_run_outcome` and
      `load_ship_outcome`.
    - `submit_ship` refuses before enqueue when a matching same-repo, same-PR
      ship job is already running and points the operator to
      `shipyard watch --pr <pr>` or queue/status inspection.
    - At this slice, execution was still synchronous and one-active-job inline;
      later P2b.5k/P2b.5l slices added the losing-submitter wait loop and
      concurrent drain-owned worker admission.
  - P2b.5k cooperative drain wait/ownership loop:
    - `drain_or_wait_run` and `drain_or_wait_ship` now wrap submitted jobs in
      a cooperative wait loop.
    - A submitter checks durable terminal state first, then attempts to acquire
      `queue.lock`; only the drain owner calls `execute_run_worker` or
      `execute_ship_worker`.
    - A non-owner polls durable state and retries drain ownership without
      dispatching work.
    - At this slice, ownership still ran only the submitted job inline; later
      P2b.5l added sibling request hydration, compatible job admission, worker
      thread spawn/reap, and scheduler-deferred requeue handling.
  - P2b.4b ship-state writer migration implementation:
    - Cloud add-lane now re-opens current PR state under the PR lock after
      dispatch and appends the new lane there instead of saving the stale
      pre-dispatch snapshot.
    - Cloud retarget now re-opens current PR state under the PR lock after
      cancel/dispatch and replaces the target lane there.
    - Daemon reconcile and manual `ship-state reconcile` now fetch GitHub
      rollups without holding the lock, then re-open current PR state under the
      PR lock before applying reconcile changes.
    - Daemon PR-close archival now holds the PR lock across repo verification
      and archive.
    - Auto-merge now holds the PR lock across verdict, merge, and archive.
  - Additive queue/status compatibility output:
    - `shipyard queue --json` still emits `active` as the first running job or
      null, and now also emits `active_runs`.
    - `shipyard status --json` still emits `active_run` as the first running
      job when present, and now also emits `active_runs`.
    - Queue human output now renders `Running (N)` with one row per running job
      when multiple running jobs exist in durable state.
    - P2b.5l/P2b.5n now exercise this shape with concurrent durable running
      jobs.

### Phase P3a - Adaptive Mac Routing

Status: planned, not started.

Scope:

- Prefer local Mac pool.
- Overflow pending work to explicit GitHub-hosted macOS when local depth is too
  high.
- Move scheduler-owned pending work back to local when capacity opens,
  including jobs that were planned for GitHub-hosted macOS overflow but have
  not been dispatched yet.
- Do not interrupt running work.
- Do not use hidden GitHub or self-hosted fallback.
- Keep route/capability modeling open for future non-macOS hosts that can build
  macOS artifacts; do not implement or claim that support in P2b/P3a.

## Known Risks And Decisions

- The current branch is `feat/github-app-local-mac-queue`, tracking
  `origin/feat/github-app-local-mac-queue`.
- Full `cargo test` currently has two unrelated auto-merge failures. Do not
  claim the suite is green until those are resolved or explained.
- `planning/github-auth-boundary.md` and `planning/local-mac-pool.md` are
  detailed design docs, but this file is the status source of truth.
- The local Mac pool plan deliberately separates Phase 2a leases/status from
  Phase 2b queue concurrency.
- Adaptive mac routing is pending-only in Phase 3a. Submitted/running GitHub
  job cancellation is deferred to a later explicit design.
- Auth quota lifting is proven on this Mac for the real `shipyard-local`
  GitHub App installation. Ambient `gh` and a `gh auth token` helper still
  report normal user buckets of `5000`, but the installation-token helper
  reports `12,500/hour`.
- Local installed `shipyard` was updated during the auth/P2b work, but the PR
  branch has moved since then. Build and reinstall again only after the current
  code/doc checks pass.
- GitHub App quota clarification: a Shipyard GitHub App can be installed on the
  `danielraffel` personal account and granted all or selected personal repos.
  The current installation has access to 259 repositories and hits GitHub's
  documented `12,500/hour` installation-token cap.
- The RepoPrompt Oracle export for P2b appears truncated near the fallback
  resource-model section. Treat it as useful starting context, not complete
  source of truth. Re-run or continue Oracle if deeper coverage is needed.
- Earlier Claude review attempts were interrupted or hung through the cmux
  wrapper. A direct `/Users/danielraffel/.local/bin/claude -p` review completed
  and its actionable findings are now incorporated in
  `planning/queue-concurrency.md`.
- A later requested fresh Claude review initially failed on 2026-05-27 because
  the Claude CLI returned `Not logged in · Please run /login`. After login was
  restored, a retry succeeded and its actionable findings were incorporated
  into `planning/queue-concurrency.md`.

## Update Rules For Agents

When continuing this work:

1. Read `CLAUDE.md`.
2. Read this file first.
3. Read the detailed docs for the active phase:
   - quota/auth: `planning/github-auth-boundary.md`
   - local pool: `planning/local-mac-pool.md`
   - queue concurrency: `planning/queue-concurrency.md`
4. Update this file before ending a session or after any meaningful phase
   change.
5. Keep the worktree path, branch, HEAD, dirty files, and validation results
   current.
6. Keep phase statuses honest: `not started`, `planned`, `in progress`,
   `blocked`, or `done`.
7. Quota/auth is not the active track now. Do not return to it unless the user
   explicitly changes priority.

## Resume Prompt For A New Agent

Use this prompt if another agent needs to pick up the session:

```text
You are continuing Shipyard quota/auth and local Mac pool work in
/Users/danielraffel/Code/shipyard.

First read CLAUDE.md, then read planning/phase-handoff-status.md and treat it
as the single status source of truth. Supporting details live in
planning/github-auth-boundary.md, planning/local-mac-pool.md, and
planning/queue-concurrency.md.

Current state:
- Branch: feat/github-app-local-mac-queue.
- PR: https://github.com/danielraffel/Shipyard/pull/314.
- Quota/auth Q1-Q4 first slice is implemented. The real shipyard-local GitHub
  App installation has verified REST and GraphQL 12,500/hour buckets on the
  danielraffel personal account installation with 259 repositories.
- P2a host-pool dispatch/status/stale-lease cleanup is implemented.
- P2b.1 through P2b.6 are implemented for the first queue-concurrency release:
  queue-state locking, durable request/outcome stores, cooperative
  submitter-owned drain, bounded in-process worker admission, host-pool
  capacity/lease deferral, additive active_runs JSON, docs/skills, and
  tests/queue_concurrency.rs.
- Fresh Claude review on 2026-05-27 found no new code blocker from the current
  queue-concurrency plan, but requested doc consistency edits. Those edits were
  incorporated into planning/queue-concurrency.md and this handoff.
- macOS GUI compatibility check passed at compile level with an unsigned Debug
  build in /Users/danielraffel/Code/shipyard-macos-gui. The app was not
  launched or left open. Signed build/test attempts were blocked by Xcode
  probing a locked attached iOS device.
- Adaptive routing is not started. Do not start it while finishing PR/readiness
  unless the user explicitly redirects.

Validation already recorded in this handoff includes focused P2b tests,
cargo fmt/clippy/full locked test runs for the PR branch, and the GUI build
check. Re-run only the checks needed for new changes.

Known caveats:
- Historical full cargo test failures were the two auto-merge tests listed in
  this handoff; later locked validation on the PR branch passed after origin/main
  fixed those paths. Keep reporting exactly what was run rather than claiming
  broader coverage.
- prompt-exports/ is an untracked scratch artifact. Do not commit it unless the
  user asks.

Recommended next step:
Monitor PR #314 checks after the latest push. Do not start adaptive routing.
When adaptive routing starts later, include a test where GitHub-hosted macOS
overflow can be pulled back to a newly available local macOS slot before GitHub
dispatch. Update planning/phase-handoff-status.md after each completed slice.
```
