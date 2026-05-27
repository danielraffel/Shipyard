# Local Mac Pool Plan

Status: active implementation
Last updated: 2026-05-26
Owner: unassigned
Master status: `planning/phase-handoff-status.md`

## Goal

Add a clear path for using more than one local Mac as Shipyard capacity.
The immediate use case is a Mac Studio that should be preferred over the
current laptop, with local fallback when needed. The longer-term use case is
a small local Mac pool that can transfer work, clean up after itself, cache
smartly, and make better routing decisions than static fallback.

This is a separate project from `planning/github-auth-boundary.md`. The
GitHub auth/quota work can proceed first. The pool work should not depend on
GitHub App tokens, higher GitHub quotas, or any hidden GitHub repository
variables.

## Current Status

| Area | Status | Notes |
|---|---|---|
| RepoPrompt discovery | done | Selected context covered targets/fallback docs, dispatch resolution, SSH/local executors, queue, warm pool, ship command wiring, runner watchdog, and Shipyard/CI skills. |
| RepoPrompt planning | done | Initial plan split the work into current fallback use, first-class host pools, and operator cleanup/cache surfaces. Follow-up incorporated adaptive routing for GitHub-hosted macOS overflow. |
| Claude review | done | Two passes completed with RepoPrompt context. Pass 2 found no remaining blockers; minor wording fixes are incorporated. |
| Implementation | P2a and P2b foundations done | P1 docs/config rollout is done. P2a added `src/host_pool.rs`, JSON lease-store primitives, `shipyard targets pool status`, runnable `backend = "host-pool"` local/SSH materialization, and stale-lease cleanup. P2b added cooperative queue concurrency for non-conflicting jobs under one local drain owner. |

## Review Notes

Claude pass 1 completed on 2026-05-26 using the RepoPrompt context export.
Incorporated findings:

- Documented that today's queue has one active job and that Phase 2a does not
  provide parallel throughput across multiple Macs.
- Split the pool work into Phase 2a host-pool leases/status and Phase 2b queue
  concurrency.
- Replaced Phase 3a slot math with serialized local-depth routing until queue
  concurrency exists.
- Added supersedence behavior for scheduler-owned route plans.
- Tightened host-pool lease locking, dispatch materialization, warm-pool
  wording, fallback validation inheritance, and the GitHub-hosted macOS config
  example.
- Claude pass 2 found no blockers. Minor follow-up edits clarified route
  granularity for multi-target jobs, cloud fallback key validation, the exact
  queue supersedence key, and status rendering.

## Relationship To Quota Work

Keep the two efforts independent:

- Quota/auth project: consolidate GitHub CLI calls and add opt-in token
  support for higher limits and portable credentials.
- Local Mac pool project: route work across explicit local Macs and explicit
  cloud fallback capacity.

The only deliberate overlap is that Phase 3a may choose GitHub-hosted macOS
as explicit overflow. That choice must use whatever GitHub auth boundary
exists at the time, but the scheduler design must not require GitHub App auth.

## Locked Decisions

- Local/self-hosted capacity must be explicit in Shipyard config.
- Do not add hidden repository-variable fallback to self-hosted runners.
- Do not change runner-watchdog behavior as part of this project.
- Do not route to GitHub-hosted macOS unless a cloud fallback is explicitly
  configured for the target.
- Phase 1 is not load balancing. It is ordered fallback.
- Phase 2 is one-controller local host-pool scheduling, not distributed
  multi-controller scheduling.
- Phase 2a added named pool members, leases, status, and cleanup.
- Phase 2b added queue concurrency for non-conflicting jobs, so Shipyard can
  drain several queued Mac jobs in parallel across available local capacity
  under one drain owner.
- Phase 3a may mutate only pending or not-yet-started route assignments.
  It must not interrupt running work.

## Non-Goals

- No live migration of an already-running validation from one Mac to another.
- No automatic cancellation of already-running GitHub-hosted macOS jobs.
- No cross-SHA binary artifact reuse.
- No automatic deletion of arbitrary remote directories.
- No distributed queue shared by multiple independent controllers.
- No implicit use of GitHub-hosted macOS as a quota or capacity fallback.

## What Works Today

Shipyard already has useful building blocks:

| Capability | Current support | Notes |
|---|---|---|
| Ordered fallback | yes | `src/executor/dispatch.rs` resolves targets and tries fallback entries in order. |
| SSH job transfer | yes | `src/executor/ssh.rs` pushes git bundles to an existing remote repo path. |
| Local fallback | yes | `src/executor/local.rs` can run on the current checkout. |
| Machine-global queue | yes | `src/queue.rs` serializes work for one controller process/machine. |
| Warm workdir reuse | partial | `src/warm_pool.rs` stores entries keyed by target/host with the entry SHA recorded; callers should reuse only when the entry SHA matches the active SHA. |
| Job concurrency | multiple non-conflicting jobs | `src/queue.rs` preserves singular compatibility helpers and adds multi-running state; the cooperative drain owner admits compatible jobs while respecting resource claims and pool capacity. |
| Mid-flight cloud retarget commands | partial | `shipyard cloud retarget` and `cloud add-lane` are manual operator tools, not scheduler policy. |

Current static fallback is good enough for a first Mac Studio setup:

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

This means:

1. Probe Mac Studio first.
2. If it is unreachable or has an infrastructure failure, try the local Mac.
3. If the Mac Studio produces a real validation failure, stop and report that
   failure instead of hiding it behind fallback.

## Current Gaps

| Gap | Why it matters |
|---|---|
| Cleanup/status is still partial | Host-pool status shows leases, but not warm entries by member or cleanup candidates. |
| No remote/workdir cleanup command | Shipyard cannot delete arbitrary remote paths unless it owns an explicit managed root. Stale lease cleanup is implemented. |
| No queue-aware routing | The dispatcher does not see queue depth, wait time, or planned route assignments. |
| No scheduler-owned route state | Queued jobs do not record whether a route was picked by the scheduler or manually overridden. |
| Warm pool is not member-aware enough | Warm entries are keyed by target/host, but status needs concrete pool member identity. |
| Status is not adaptive-aware | Operators need pending adaptive local/cloud route counts once Phase 3a exists. |

## Phase 1 - Use Existing SSH/Local Fallback

Status: done for docs/config rollout.

Phase 1 is a docs/config rollout. It should not add new scheduler behavior.

### Deliverables

- Add `docs/local-mac-pool.md` with setup guidance for a Mac Studio plus local
  fallback.
- Update `docs/targets.md` with an explicit Mac Studio example.
- Update `docs/workflows.md` with a local Mac capacity workflow.
- Update `skills/shipyard/SKILL.md` and `skills/ci/SKILL.md` so agents know
  local capacity must be explicit.

Completed in this slice.

### Operator Contract

Phase 1 supports:

- Prefer Mac Studio by making it the primary target.
- Fall back to this Mac when Mac Studio is unreachable.
- Transfer code to Mac Studio with the existing SSH git bundle path.
- Reuse warm same-SHA remote workdirs where current warm-pool behavior applies.
- Inspect or drain warm entries with existing warm-pool/targets surfaces.
- Reuse the target's existing validation block for both the primary and local
  fallback. Users should not need to duplicate `command`, `stages`, or
  `contract` settings in the fallback entry.

Phase 1 does not support:

- Load balancing.
- Busy/idle host awareness.
- Retargeting pending jobs based on queue pressure.
- Automated deletion of unmanaged workdirs.
- Moving already-submitted GitHub-hosted macOS work back to local.

### Bootstrap Checklist

1. Enable SSH from the controller Mac to the Mac Studio.
2. Clone the repo on the Mac Studio at the configured `repo_path`.
3. Install matching toolchains and dependencies on the Mac Studio.
4. Add the explicit `targets.mac` config.
5. Run `shipyard targets test mac`.
6. Run one validation with `shipyard run --targets mac`.
7. Drain stale warm entries for the old local-only target if needed.
8. Confirm warm-pool behavior and remote disk usage.

## Phase 2 - First-Class Local Host Pool

Phase 2 adds a real pool abstraction while preserving ordered, explicit
capacity.

Phase 2 is split into two milestones:

- Phase 2a: host-pool leases, status, cleanup, and ordered member selection.
- Phase 2b: queue concurrency for non-conflicting jobs. This is the work that
  turns several Macs into higher total throughput for multiple queued jobs.

The first P2b implementation uses conservative job-level resource claims and a
small worker cap. Operators should expect overlap only when jobs do not contend
for the same checkout, PR state, evidence lane, or exhausted host-pool
capacity.

Current implementation status:

- Done: parse `[host_pools]` with ordered `ssh` and `local` members.
- Done: JSON-backed `HostPoolLeaseStore` under
  `<state_dir>/host_pool/leases.json` with advisory locking, acquire/release,
  heartbeat, and stale-prune primitives.
- Done: `shipyard targets pool status` for human and JSON status.
- Done: `backend = "host-pool"` target resolution.
- Done: ordered member selection with `requires` filtering.
- Done: local/SSH materialization with lease acquire, heartbeat, and release
  around validation.
- Done: stale-lease cleanup with `shipyard targets pool cleanup --dry-run` and
  `--fix`.
- Done: queue concurrency for non-conflicting jobs with durable request/outcome
  state and cooperative drain ownership.
- Not done: warm-pool `member_id` visibility.
- Not done: remote/workdir cleanup under explicit managed roots.
- Not done: adaptive routing.

### Proposed Config

```toml
[host_pools.local_macs]
strategy = "ordered"
lease_stale_seconds = 180
heartbeat_interval_seconds = 15

[[host_pools.local_macs.members]]
id = "mac-studio"
type = "ssh"
host = "mac-studio"
repo_path = "/Users/shipyard/work/shipyard"
max_concurrency = 1
capabilities = ["macos", "arm64"]

[[host_pools.local_macs.members]]
id = "local"
type = "local"
cwd = "/Users/danielraffel/Code/shipyard"
max_concurrency = 1
capabilities = ["macos", "arm64"]

[targets.mac]
backend = "host-pool"
pool = "local_macs"
platform = "macos-arm64"
requires = ["macos", "arm64"]
warm_keepalive_seconds = 1800
```

### New Concepts

`HostPoolTargetConfig`

- Resolved target config for `backend = "host-pool"`.
- Names the pool and required capabilities.

`HostPoolMember`

- One concrete execution backend.
- Supports `ssh` and `local` first.
- Defaults `max_concurrency = 1`.

`HostPoolLeaseStore`

- File-backed state under Shipyard's state dir.
- Tracks active leases, stale leases, and heartbeat timestamps.
- Assumes one controller owns scheduling for a pool.
- Uses an advisory file lock, matching the `fs2::FileExt` pattern in
  `src/queue.rs`, for example under
  `<state_dir>/host_pool/leases.json`.

`HostPoolLease`

Suggested fields:

```rust
struct HostPoolLease {
    lease_id: String,
    pool_name: String,
    member_id: String,
    target_name: String,
    backend: String,
    host: Option<String>,
    job_id: Option<String>,
    branch: String,
    sha: String,
    owner_pid: u32,
    acquired_at: DateTime<Utc>,
    heartbeat_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}
```

`ResolvedBackend::HostPool`

- Should be visible to target resolution, the scheduler, and status surfaces.
- Should not reach a final local or SSH executor unchanged.
- Validation should resolve a concrete member, then construct a transient
  `ResolvedTarget` with `ResolvedBackend::Local` or `ResolvedBackend::Ssh`
  and the target's resolved validation contract.

### Execution Flow

1. Resolve the `host-pool` target.
2. Filter pool members by `requires`.
3. Load lease state.
4. Select the first reachable member with available capacity.
5. Acquire a lease.
6. Materialize a concrete `ResolvedTarget` for the existing local or SSH
   executor.
7. Heartbeat the lease while validation runs.
8. Release the lease on completion.
9. If a member has infrastructure failure, try the next eligible member.
10. If a member returns a real validation failure, stop and report the failure.

### Queue Model

Phase 2b extends the queue to support concurrent non-conflicting jobs under one
drain owner. The design is tracked in `planning/queue-concurrency.md` and uses:

- `active_runs` while preserving singular `active` / `active_run`
  compatibility.
- Durable request/outcome stores so losing submitters can wait without
  dispatching work.
- Conservative resource claims and host-pool capacity math.
- Prevent two jobs from using the same worktree or same one-slot pool member.
- Preserve queue priority and supersedence semantics.
- Keep evidence and ship-state updates deterministic when jobs complete out of
  order.

### Cleanup And Cache Rules

- Same-SHA warm workdir reuse remains valid.
- Warm entries should record optional `member_id` for status/reporting.
- Do not reuse binary artifacts across SHAs.
- Evict a warm entry after any non-pass result when that warm entry was reused.
- Add pool status and cleanup commands before automated deletion.
- Automated deletion is allowed only under an explicit Shipyard-owned managed
  root, not arbitrary `repo_path` or `cwd` values.

### Phase 2 CLI And Status

Current/planned commands:

```text
shipyard targets pool status
shipyard targets pool cleanup --dry-run
shipyard targets pool cleanup --fix
```

`shipyard targets pool status` is implemented. Cleanup currently prunes stale
lease records only; remote/workdir deletion remains planned and must be under
explicit Shipyard-managed roots.

Status should show:

- Pool members and reachability.
- Busy/idle state.
- Active lease owner, branch, SHA, and age.
- Stale leases.
- Warm entries per member.
- Cleanup candidates and whether Shipyard owns the path.

## Phase 3a - Adaptive Mac Routing

This phase covers the newer requirement: jobs sometimes land on
GitHub-hosted macOS, which can be slow. Shipyard should prefer the local Mac
pool when it is likely faster, overflow to GitHub-hosted macOS only when the
local pool is badly overqueued, and move pending work back to local when
capacity opens.

This requires Phase 2 leases first. Without busy/idle pool state, Shipyard
cannot make a defensible queue-aware routing decision.

### Scope

Phase 3a adds a pending-work planner:

- Prefer explicit local Mac pool capacity.
- Overflow to explicit GitHub-hosted macOS only when local queue depth crosses
  configured thresholds.
- Reassign queued/not-yet-started work back to local when local capacity
  becomes available.
- If overflow has selected GitHub-hosted macOS but the work has not been
  dispatched to GitHub yet, the scheduler should pull it back to a newly
  available local macOS slot instead of making the operator wait for GitHub
  runner capacity.
- Never mutate `Locked`, `Submitted`, `Running`, or `Completed` work.
- Preserve manual retarget overrides.

Phase 3a does not cancel already-running GitHub-hosted macOS jobs. A later
Phase 3b may evaluate cancellation/race semantics for submitted-but-not-started
GitHub jobs, but that is intentionally out of scope until the safe boundary is
designed.

### Proposed Config

```toml
[targets.mac]
backend = "host-pool"
pool = "local_macs"
platform = "macos-arm64"
requires = ["macos", "arm64"]
warm_keepalive_seconds = 1800
routing_policy = "adaptive_local_overflow"
local_queue_soft_limit = 1
local_queue_hard_limit = 3
return_to_local_below = 1
scheduler_recheck_seconds = 15

fallback = [
  { type = "cloud", provider = "github-hosted", runner_selector = "macos-latest" },
]
```

Implementation note: confirm that the fallback table keys round-trip through
the current cloud resolver. In particular, verify whether the code path expects
`provider` or `runner_provider` after `merged_fallback_table`, and keep the
documentation example aligned with the implemented config syntax.

Validation rules:

- `routing_policy = "adaptive_local_overflow"` is valid only when the primary
  target is `backend = "host-pool"`.
- Adaptive routing requires at least one explicit cloud fallback.
- The cloud fallback must satisfy the target `requires`.
- The default policy remains `ordered`.

### Persisted Route State

Queued jobs need optional per-target route state so the scheduler can mutate
only safe work:

```rust
struct MacRoutingPlan {
    target_name: String,
    planned_route: PlannedMacRoute,
    source: RouteAssignmentSource,
    state: RouteDispatchState,
    reason: String,
    updated_at: DateTime<Utc>,
    revision: u64,
}

enum PlannedMacRoute {
    LocalPool { pool_name: String },
    CloudOverflow { backend_label: String },
}

enum RouteAssignmentSource {
    Scheduler,
    Manual,
}

enum RouteDispatchState {
    Pending,
    Locked,
    Submitted,
    Running,
    Completed,
}
```

Missing route metadata in old queue state must load as "no adaptive plan yet".

### Scheduler Loop

Add `src/mac_scheduler.rs` after Phase 2 exists.

Inputs:

- Queue snapshot.
- Resolved adaptive target config.
- Host-pool lease snapshot.
- Optional warm-pool status for operator reporting.

Trigger points:

- On enqueue.
- On job completion or lease release.
- Immediately before dequeuing the next job.
- Periodic tick while a queue owner process is alive.

Locking discipline:

1. Read the queue snapshot.
2. Release the queue lock.
3. Read the lease snapshot.
4. Compute route mutations in memory.
5. Reacquire the queue lock.
6. Revalidate that each target route is still `Pending` and scheduler-owned.
7. Persist mutations.

Do not hold a queue lock while probing SSH, probing cloud, or waiting on
heartbeats.

### Initial Routing Algorithm

Use deterministic queue depth before adding runtime prediction.

Now that Phase 2b has queue concurrency, the adaptive scheduler should use
local pool slot math rather than the older serialized-depth approximation:

1. Read total local pool slots from configured eligible members.
2. Subtract active non-stale leases and already-planned local jobs ahead.
3. Choose local when free local slots remain.
4. Choose cloud overflow when explicit overflow is configured and local
   capacity is exhausted beyond the configured threshold.
5. Apply hysteresis before moving cloud-planned work back to local. Only return
   when the route is still pending and local availability is below the
   configured return threshold.

The earlier serialized queue approximation is retained only as historical
context for Phase 2a.

### Invariants

- Explicit config only: no hidden GitHub-hosted or self-hosted fallback.
- Pending-only mutation: scheduler can change only `Pending` routes.
- Manual override wins: `source = Manual` blocks scheduler rewrites.
- No running-job interruption.
- Local-first default when capacity is available.
- One controller owns queue plus pool leases.
- Supersedence drops scheduler-owned route plans. When `Queue::enqueue`
  removes a pending job with the same `branch`, `target_names`, and `mode`, the
  replacement job must be planned from scratch.
- No nested long-lived locks. The scheduler must not hold the queue lock while
  probing hosts, probing cloud, or waiting on lease heartbeats.

### Capacity Model Guardrail

The initial implementation can treat local pool members as physical macOS
hosts, but the route model should not bake in "host OS == artifact capability".
Track the selected execution environment separately from the capability it can
satisfy:

- `runs_on` / host environment: physical macOS, Linux VM, SSH host, cloud
  runner, etc.
- `can_build` / target capability: macOS arm64 artifact, Linux artifact,
  Windows artifact, etc.

This keeps room for a future Linux VM that can build macOS-architecture
artifacts once upstream projects add that support. That cross-build path is not
supported or tested in the current local Mac pool work; the only requirement now
is to avoid schema and scheduler names that would make it impossible later.

### Manual Retarget Vs Scheduler Reassignment

Existing `shipyard cloud retarget` and `cloud add-lane` are manual operator
tools. They can remain explicit and higher-precedence than automation.

Scheduler-driven reassignment should initially update only Shipyard-owned
queue route metadata. It should not shell out to the manual CLI or parse human
CLI output. If typed retarget/add-lane helpers exist, Phase 3a can reuse those
helpers later for backend labels or request construction.

### Status UX

Pending jobs should show planned routing:

```text
sy-104  feature/foo  mac=local_macs(pending)              reason=slot_available
sy-105  feature/bar  mac=github-hosted(pending-overflow)  reason=local_overqueued depth=3
```

Pool status should show:

- Total local pool concurrency.
- Active leases by member.
- Pending adaptive jobs planned local.
- Pending adaptive jobs planned cloud overflow.
- Manual overrides.
- Stale planned routes that no longer match config.

Doctor should report:

- Adaptive mac routing enabled or disabled.
- Explicit cloud overflow configured or missing.
- Invalid thresholds.
- Manual overrides.
- Pending jobs whose planned route conflicts with current config.

## Phase 3b - Submitted Cloud Retargeting

This is intentionally deferred.

Potential goal:

- If a GitHub-hosted macOS job has been submitted but has not started, cancel or
  replace it with local Mac pool work when local capacity becomes available.

Required design before implementation:

- A precise "not started" signal for GitHub-hosted jobs.
- Race handling when GitHub starts the job while Shipyard is retargeting.
- Efficient pull-back semantics: if a GitHub-hosted macOS overflow job is still
  pending/not started and a local macOS pool slot opens, Shipyard should cancel
  or supersede the cloud lane and run locally without waiting on GitHub runner
  capacity.
- Idempotent cancellation and replacement.
- Evidence semantics when two lanes race.
- User-visible policy for whether wasting a little GitHub time is acceptable.
- Capability semantics that distinguish the runner host from the artifact it can
  build, so future Linux-based macOS artifact builders can be represented
  without redefining adaptive routing.

Do not include this in Phase 3a.

## File Impact

| File | Phase | Expected change |
|---|---:|---|
| `docs/local-mac-pool.md` | 1 | New operator guide. |
| `docs/targets.md` | 1/2/3a | Add Mac Studio fallback, host-pool, and adaptive overflow examples. |
| `docs/workflows.md` | 1/3a | Add local pool and adaptive routing workflows. |
| `skills/shipyard/SKILL.md` | 1/3a | Preserve explicit local capacity guardrails and describe scheduler limits. |
| `skills/ci/SKILL.md` | 1/3a | Same guardrails for CI agents. |
| `src/host_pool.rs` | 2 | New lease/store/pool selection module. |
| `src/executor/dispatch.rs` | 2/3a | Add `ResolvedBackend::HostPool`, host-pool config, adaptive routing config validation, and route materialization helpers. Update every exhaustive `ResolvedBackend` match, including validate, probe, diagnose, `workdir`, `with_workdir`, and backend labels. |
| `src/warm_pool.rs` | 2 | Add optional `member_id` and pool-aware status. |
| `src/app/ship_cmd.rs` | 2/3a | Thread lease store and scheduler through the queue/dispatch boundary. |
| `src/app/targets_cmd.rs` | 2/3a | Add pool status/cleanup and adaptive route counts. |
| `src/job.rs` | 3a | Add optional per-target route plan metadata if this is the durable job owner. |
| `src/queue.rs` | 3a | Add route-plan update helpers and compatibility tests. |
| `src/mac_scheduler.rs` | 3a | New pure scheduler/planner module. |
| Cloud retarget files | 3a/3b | Validate typed helper reuse; do not depend on human CLI output. |

Open validation item: confirm whether `src/job.rs` is the right durable owner
for route metadata before implementation.

## Test Plan

Phase 1:

- Documentation examples match accepted config syntax.
- `shipyard targets test mac` works against a configured SSH Mac.
- `shipyard run --targets mac` uses primary SSH and local fallback.

Phase 2:

- Existing configs without `[host_pools]` continue to resolve unchanged.
- Host-pool config resolves.
- Ordered strategy selects the first reachable idle member.
- `requires` filters members.
- Busy primary member causes next eligible member to run.
- Stale leases are ignored or cleaned according to policy.
- Real validation failure stops fallback.
- Infrastructure failure tries the next member.
- Warm-pool state remains backward-compatible.
- Old queue JSON remains valid while Phase 2a keeps one active job.
- Cleanup refuses unmanaged paths.

Phase 3a:

- Adaptive config rejects missing host-pool primary.
- Adaptive config rejects missing explicit cloud fallback.
- Scheduler chooses local while serialized local depth is below the hard limit.
- Scheduler chooses cloud overflow when serialized local depth reaches the hard
  limit.
- Pending cloud-planned work flips back to local when capacity opens and
  hysteresis allows it.
- A pending job that was planned for GitHub-hosted macOS overflow but has not
  been dispatched to GitHub is pulled back to local when a local macOS slot opens.
- `Manual` route source blocks scheduler mutation.
- `Locked`, `Submitted`, `Running`, and `Completed` routes are immutable.
- Old queue JSON without route metadata still loads.
- Route/capability schema leaves room for future cross-build hosts by separating
  where a job runs from what platform artifact it can build.

## Risks

| Risk | Mitigation |
|---|---|
| Host-pool config is not understood by older binaries | Treat as a new feature; document minimum version once implemented. |
| Queue schema changes break old state | Make route metadata optional and test old JSON fixtures. |
| Pool concurrency is overread under the serialized queue | Document Phase 2a as one-active-job; keep real multi-job throughput in Phase 2b. |
| Scheduler flaps routes between local and cloud | Use hysteresis and stop mutating at `Locked`. |
| Cleanup deletes user-owned data | Only delete under explicit Shipyard-managed roots. |
| Multiple controllers overbook the same Mac | Keep Phase 2/3a one-controller scoped; document this clearly. |
| Adaptive routing hides GitHub quota costs | Show route reasons and require explicit cloud fallback config. |
| Phase 3b expands scope too early | Keep submitted/running job cancellation out of Phase 3a. |

## Acceptance Criteria

Phase 1:

- A user can configure Mac Studio primary plus local fallback with current
  primitives.
- Docs explain exactly what transfers, what caches, and what cleanup remains
  manual.
- Agents do not infer hidden self-hosted capacity.

Phase 2:

- A `host-pool` target can lease an idle local Mac member and run through the
  existing local or SSH executor.
- Pool status shows busy/idle members and stale leases.
- Cleanup is safe by construction and dry-run first.
- Warm-pool behavior remains same-SHA and member-visible.

Phase 3a:

- A target can explicitly declare local host-pool primary plus GitHub-hosted
  macOS overflow.
- Pending macOS jobs show planned route, state, source, and reason.
- When local capacity becomes available before dispatch, scheduler-owned
  pending jobs can move from cloud overflow back to local.
- When the local pool is badly overqueued, later pending jobs can plan for
  explicit GitHub-hosted macOS overflow.
- The scheduler never changes manual, locked, submitted, running, or completed
  route assignments.

## Agent Handoff Checklist

Before starting implementation:

- Read `CLAUDE.md`.
- Re-read this planning doc and `planning/github-auth-boundary.md`.
- Confirm whether the current task is quota/auth or local pool. Do not mix code
  changes across both unless explicitly requested.
- Run `rg -n 'ResolvedBackend|fallback|WarmPool|Queue|cloud retarget|add-lane' src docs skills`.
- Validate the durable job/queue state owner for Phase 3a route metadata.
- Validate the exact files behind `shipyard cloud retarget` and
  `cloud add-lane`.
- Run targeted tests before broad tests.

## References

- `docs/targets.md`
- `docs/workflows.md`
- `docs/runner-watchdog.md`
- `src/executor/dispatch.rs`
- `src/executor/ssh.rs`
- `src/executor/local.rs`
- `src/queue.rs`
- `src/warm_pool.rs`
- `src/app/ship_cmd.rs`
- `src/app/targets_cmd.rs`
- `skills/shipyard/SKILL.md`
- `skills/ci/SKILL.md`
