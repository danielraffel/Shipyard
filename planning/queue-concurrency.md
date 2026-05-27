# Shipyard Queue Concurrency Plan

Status: implemented for the first Phase P2b concurrency release
Last updated: 2026-05-27
Primary handoff: `planning/phase-handoff-status.md`
Supporting plan: `planning/local-mac-pool.md`

This document records the Phase P2b design and implementation status for
concurrent execution of non-conflicting queued jobs. P2b is the step between
Phase P2a host-pool leases/status and future Phase P3a adaptive Mac routing.

## Goals

- Allow multiple non-conflicting jobs to run at the same time under one local
  Shipyard controller.
- Let a local Mac host pool drain multiple queued jobs in parallel when
  multiple eligible members are idle.
- Preserve current `shipyard run` and `shipyard ship` command UX: the
  submitting command waits for its own job and prints the same class of result.
- Keep target execution serial within a single job for this phase.
- Keep one queue-drain owner at a time. This is not distributed scheduling.
- Preserve queue priority and FIFO ordering where resource conflicts allow it.
- Keep JSON changes additive where practical so older consumers can continue
  reading the first active job.

## Non-Goals

- Adaptive local/cloud routing.
- Queue-depth overflow to GitHub-hosted macOS.
- Moving pending work from GitHub back to a local Mac.
- Cancelling or retargeting already-running GitHub Actions jobs.
- Per-target dynamic rescheduling inside a running job.
- Multi-controller or network-distributed scheduling.
- Remote workdir cleanup or managed-root deletion.
- Built-in GitHub App JWT signing. Higher-limit app auth remains a separate
  auth track.

## Current State

`shipyard run` and `shipyard ship` still present a synchronous UX to the
submitting process, but execution now goes through durable submit/wait/drain
paths:

- `src/app/run_cmd.rs` and `src/app/ship_cmd.rs` resolve targets, preflight,
  build stores, submit durable jobs, and render from durable queue/outcome
  state.
- `src/ship.rs` has `submit_run` / `submit_ship`, `drain_or_wait_run` /
  `drain_or_wait_ship`, and `execute_run_worker` / `execute_ship_worker`.
- A submitter that owns the drain lock admits compatible pending jobs, hydrates
  their durable requests, and runs bounded in-process workers. A losing
  submitter waits on durable state and periodically retries drain ownership.
- `src/queue.rs` is now a file-backed handle. Queue read-modify-write
  operations take `queue.state.lock`; stale-running recovery and orphan pending
  request cancellation are explicit drain-owner-only primitives.
- `src/queue_request.rs` persists durable request and outcome envelopes under
  `<state_dir>/queue/requests` and `<state_dir>/queue/outcomes` using
  queue-owned snapshot types rather than serializing executor runtime structs.
- `src/app/queue_cmd.rs` preserves singular `active` / `active_run`
  compatibility and adds `active_runs`; human queue output can render multiple
  running jobs.
- `src/daemon_runtime.rs` is an IPC/webhook/reconcile/status daemon. It is not
  a queue worker and does not currently execute `run` or `ship` jobs.
- `src/host_pool.rs` already provides JSON-backed leases with advisory locking,
  capacity checks, heartbeat, release, and stale pruning.
- `src/executor/dispatch.rs` can validate `ResolvedBackend::HostPool`, and
  queued host-pool leases carry the owning queue job id for pool status.

The important conclusion is that P2b cannot be a small change from
`get_active()` to `get_active_jobs()`. The first concurrency release now
includes queue-state locking, durable request/outcome stores, shared-store
locks, resource planning, host-pool capacity checks, and a submitter-owned
cooperative drain scheduler. Daemon-owned queue draining remains future work.

## Architecture

P2b adds a cooperative queue drain controller rather than requiring the existing
daemon to become a queue runner immediately.

The submitting `shipyard run` or `shipyard ship` process:

1. Resolve config and targets as it does today.
2. Run preflight as it does today.
3. Persist a durable queued execution request for the new job.
4. Enqueue the job as `Pending`.
5. Try to acquire the queue drain lock.
6. If it becomes the drain owner, run a scheduler loop that starts compatible
   pending jobs, including jobs submitted by other waiting processes.
7. If another process owns the drain lock, wait for its own job to reach a
   terminal state by reading the durable queue/outcome state and periodically
   retry drain ownership if the owner exits. A non-owner must not call
   `execute_run_worker` or `execute_ship_worker`.
8. Read the final queue job and outcome snapshot, then render the existing CLI
   response shape.

This keeps the user-facing command synchronous while making the queue durable
enough for another process to continue after a drain owner dies. A future phase
can move the same scheduler into `shipyard daemon run`, but P2b should not
depend on that daemon migration.

## New Persistent Stores

### Queue Request Store

Done in P2b.2: `src/queue_request.rs` contains `QueueRequestStore`,
`QueuedExecutionEnvelope`, queue-owned resolved-target snapshots, and the
scheduler-facing `JobResourcePlan`.

Path:

```text
<state_dir>/queue/requests/<job_id>.json
```

Implemented envelope:

```rust
struct QueuedExecutionEnvelope {
    schema_version: u32,
    job_id: String,
    kind: QueuedExecutionKind,
    cwd: PathBuf,
    created_at: DateTime<Utc>,
    resource_plan: JobResourcePlan,
    request: QueuedExecutionRequest,
}

enum QueuedExecutionKind {
    Run,
    Ship,
}

enum QueuedExecutionRequest {
    Run(QueuedRunRequest),
    Ship(QueuedShipRequest),
}
```

The queued request must contain enough resolved execution detail to run even if
the project config changes while the job is pending. It should not contain
secrets. GitHub auth should still be resolved at execution time through
Shipyard's configured auth boundary.

Decision: request persistence uses queue-owned snapshot structs
(`QueuedBackendSnapshot`, `QueuedValidationSnapshot`, and nested `Queued*`
types) instead of making `ResolvedTarget` and executor runtime structs a serde
compatibility contract.

### Queue Outcome Store

Done in P2b.2: final outcome snapshots live next to requests:

```text
<state_dir>/queue/outcomes/<job_id>.json
```

The final queue job is still authoritative for lifecycle and target results,
but outcome snapshots let submitters reconstruct the existing command output
without keeping worker-local state in memory.

Implemented shape:

```rust
enum QueuedExecutionOutcome {
    Run {
        schema_version: u32,
        job_id: String,
    },
    Ship {
        schema_version: u32,
        job_id: String,
        pr: u64,
        ship_state: ShipState,
        resumed_existing_state: bool,
    },
}
```

For `run`, the final job record is enough. For `ship`, the current output path
also needs final ship state and whether an existing compatible state was
resumed.

The current store rejects every schema version other than
`QUEUED_EXECUTION_SCHEMA_VERSION`; future schema bumps need either migration or
explicit bridge deserialization.

### Retention

Done as P2b.5m:

- Added a drain-owned trim primitive that returns the ids of terminal jobs
  removed from `queue.json`.
- Request/outcome envelopes are not deleted at terminal job completion.
- Every drain-acquired cycle sweeps `<state_dir>/queue/requests/` and
  `<state_dir>/queue/outcomes/` for entries whose job id is absent from
  `queue.json`. Delete only entries older than the current 60-second grace
  window (`QUEUE_ENVELOPE_SWEEP_GRACE`) so a dead drain owner cannot leak
  envelopes forever while normal completion races stay protected.
- Workers persist the outcome envelope before the terminal `queue.update`, so a
  submitter that observes terminal queue state can also load the outcome. If
  outcome persistence fails, the worker does not mark the job terminal in
  `queue.json`.
- Submitters waiting for their own job handle a missing queue job by
  reading the outcome snapshot if present. If both queue job and outcome are
  gone, report a clear `queue trimmed before outcome was observed` error rather
  than rerunning work or inventing output.

## Queue Store Refactor

Done in P2b.1: `Queue` is now a file-backed handle with short-lived mutation
locks.

Current shape:

```rust
pub struct Queue {
    state_dir: PathBuf,
}
```

Keep `queue.lock` as the long-lived drain-owner lock. Use the separate
short-lived queue state lock:

```text
<state_dir>/queue.state.lock
```

All queue read-modify-write operations must:

1. Acquire `queue.state.lock`.
2. Read `queue.json`.
3. Apply the mutation.
4. Atomically rewrite `queue.json`.
5. Release `queue.state.lock`.

No target execution or network call may run while holding `queue.state.lock`.

Stale-running recovery must move out of `Queue::load()` and become a
drain-owner-only step. Only the scheduler may recover stale running jobs, and
only while it holds both `queue.lock` and `queue.state.lock`. Non-drain handles
such as `status`, `cancel`, and waiting submitters must never mutate
`queue.json` on load.

Implemented API shape:

```rust
pub fn get_running(&self) -> QueueResult<Vec<Job>>;
pub fn with_jobs_locked<T>(
    &self,
    f: impl FnOnce(&mut Vec<Job>) -> QueueResult<T>,
) -> QueueResult<T>;
```

Worker transitions and progress updates use `Queue::update(&job)`, which
performs the read-modify-write under `queue.state.lock`; no separate
progress-specific API is required.

Keep `get_active()` as a compatibility helper returning the oldest or
highest-priority running job. New callers should use `get_running()`.

### Supersedence

Done in P2b.1: generic queue supersedence now marks matching pending jobs as
cancelled and retains them until normal terminal trimming. The generic
supersedence key is still `(branch, target_names, mode)`.

Done in P2b.2: jobs have an optional cancellation reason:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub cancellation_reason: Option<String>;
```

The reason for queue supersedence should be explicit:

```text
Superseded by a newer queued job for the same branch, targets, and mode.
```

Done in P2b.1: `get_recent()` and terminal trimming include both `Completed` and
`Cancelled`.

A new `ship` request for the same `(repo, pr)` needs explicit resume-aware
handling that is separate from generic queue supersedence. P2b should compute
same-PR overlap by scanning pending and running ship jobs, loading their
`QueueRequestStore` envelopes, and comparing `QueuedShipRequest { repo, pr }`.
A future optimization may promote `repo` and `pr` onto `Job`, but the scheduler
must not rely on `(branch, target_names, mode)` for same-PR safety.

Use this rule:

- If the existing same-PR ship job is still `Pending`, cancel it with reason
  `superseded by newer ship request for the same PR` and enqueue the newer
  request. This preserves today's pending supersedence behavior.
- If the existing same-PR ship job is `Running`, refuse the newer request before
  enqueue with an already-in-flight message that points the user to
  `shipyard watch --pr <pr>` or queue/status inspection. Do not attach a second
  synchronous CLI to the running worker in P2b; shared command output and
  detach/reattach semantics need a separate design.

P2b must not queue multiple independent workers that race the same PR ship
state.

Done as P2b.5d/P2b.5f for scheduler admit passes: the request-backed planner
finds older pending same-PR ship jobs and `apply_admit_pass_for_drain` cancels
them before starting admitted jobs.

Done as P2b.5j: `submit_ship` refuses before enqueue when a matching same-repo,
same-PR ship job is already running, and points the operator at
`shipyard watch --pr <pr>` or queue/status inspection. The scheduler
admit-pass same-PR exclusion remains a safety net for jobs that reached the
queue through other paths.

### Running Cancellation

`Job::cancel_with_reason` now marks a job terminal `Cancelled`, sets
`cancel_requested_at`, and sets `completed_at` in one queue update. Workers
observe that durable terminal state through `durable_cancelled_job`.

P2b should support:

- Immediate cancellation for `Pending`.
- Workers re-read their own durable job record under `queue.state.lock` between
  targets and from progress callbacks before writing another update. If the
  durable job is terminal `Cancelled`, the worker stops admitting more targets
  and returns the durable terminal job without overwriting it. The in-process
  `Job` copy is not authoritative for cancellation.

P2b should not try to kill in-flight local, SSH, Windows, or cloud validation
commands. Cancellation is cooperative at Shipyard's durable-state polling
boundaries.

## Job Model Additions

Done in P2b.2: existing enum values and target result shapes were extended
additively:

```rust
pub enum JobKind {
    Run,
    Ship,
}

pub struct Job {
    // existing fields...

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<JobKind>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_reason: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_requested_at: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_claims: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler_defer_reason: Option<String>,

    #[serde(default, skip_serializing_if = "is_zero")]
    pub scheduler_defer_count: u32,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler_defer_until: Option<DateTime<Utc>>,
}
```

`resource_claims` is for debugging/status compatibility. The scheduler should
use the richer `JobResourcePlan` persisted with the queued request.

## Scheduler

Add `src/queue_scheduler.rs`.

Suggested core types:

```rust
pub struct QueueScheduler {
    queue: Queue,
    requests: QueueRequestStore,
    outcomes: QueueOutcomeStore,
    runtime_paths: RuntimePaths,
    mode: RuntimeMode,
}

pub struct SchedulerOptions {
    pub submitted_job_id: Option<String>,
    pub drain_until_idle: bool,
    pub poll_interval: Duration,
    pub max_workers: usize,
}
```

Primary entry points:

```rust
pub fn run_until_job_terminal(
    &self,
    job_id: &str,
    options: SchedulerOptions,
) -> Result<Job, QueueSchedulerError>;

pub fn drain_once(&self) -> Result<ScheduleIteration, QueueSchedulerError>;
```

### Drain Ownership

Only the process holding `Queue::acquire_drain_lock()` may transition pending
jobs to running or spawn workers. Production submitters now use
`drain_or_wait_run` / `drain_or_wait_ship` to cooperatively acquire that drain
lock. Daemon-owned drain remains a later phase.

If a submitter cannot acquire the drain lock, it waits for its own job while
periodically trying to acquire the lock. If the prior owner exited, normal
stale-running recovery should complete the dead running jobs when the next
owner acquires the drain lock and performs a scheduler-owned recovery pass.

### Scheduling Loop

Each scheduler iteration:

1. Reap completed worker threads and collect terminal worker results. Worker
   functions remain responsible for writing outcome snapshots unless a future
   slice deliberately moves outcome persistence into the scheduler.
2. Read running jobs.
3. Read pending jobs sorted by priority and FIFO.
4. Load each pending job's queued request and resource plan.
5. Build occupied resources from running jobs and active host-pool leases.
6. Greedily admit the highest-priority pending jobs whose resource plans fit.
7. Atomically transition admitted jobs `Pending -> Running`.
8. Hydrate each started job's durable request envelope back into an executable
   run/ship request, then spawn one worker per admitted job. For P2b, use
   in-process worker threads owned by the drain process; child-process or
   daemon-owned workers are a later migration.
9. Sleep or poll briefly before the next iteration.

The scheduler must not hold queue state locks while workers execute targets.

Admission planning is intentionally lock-free. The scheduler may read queue
jobs, request envelopes, and host-pool leases outside `queue.state.lock`, then
apply the chosen plan later under the queue state lock. This means a higher
priority peer submission or a lease-state change can arrive between planning
and apply. That is acceptable only because:

- `Queue::start_pending_jobs_for_drain` is the authoritative queue-state gate
  and only starts jobs that are still pending while the drain owner holds
  `queue.state.lock`.
- Durable request envelopes are written before jobs are enqueued. Reversing
  that order would make orphan cancellation unsafe.
- Host-pool lease acquisition in dispatch is the final capacity authority. The
  scheduler capacity check is advisory; worker-side scheduler deferral and
  requeue must handle lease misses after admission.

Done as P2b.5h: `apply_admit_pass_for_drain` transitions admitted jobs to
`Running`, and `execute_run_worker` / `execute_ship_worker` can now accept jobs
already started by the drain owner without attempting a second `job.start()`.

Done as P2b.5a: `queue_scheduler::admission_blockers` and `can_admit`
implement the scheduler's pure admission decision over persisted
`JobResourcePlan` snapshots. The primitive checks exclusive-claim collisions
and host-pool capacity deficits against currently running plans and active
leases. It does not acquire the drain lock, mutate queue state, or start
workers; those remain P2b.5 scheduler-loop work.

Done as P2b.5b: `queue_scheduler::plan_admit_pass` implements the next pure
admission layer. Given queue-sorted pending request data, running resource
plans, host-pool config, and leases, it greedily returns admitted job ids,
deferred jobs with blockers, and orphaned pending jobs whose request envelopes
are missing or unreadable. This is still planning-only; the drain owner must
wire the orphan list to `Queue::cancel_orphan_pending_jobs_for_drain` and must
perform the actual pending-to-running transitions in a later P2b.5 slice.

Done as P2b.5c: `queue_scheduler::plan_admit_pass_from_jobs` loads request
envelopes from `QueueRequestStore` for pending and running queue jobs before
calling the pure admit-pass planner. It sorts pending jobs by queue priority/FIFO
rules, converts missing or unreadable pending request envelopes into orphaned
pending outputs, and reports running jobs whose request envelopes cannot be
loaded so the future drain owner can avoid admitting new work when occupied
resources are unknown.

### Orphan Pending Jobs

If a pending job exists in `queue.json` but its request envelope is missing or
fails schema validation, the drain owner should cancel it on the next admit pass
with cancellation reason `Queued request envelope missing or unreadable` plus
the load error when present. The drain-owned queue primitive exists in P2b.3f;
P2b.5f wires request-backed admit-pass cancellation into
`Queue::cancel_pending_jobs_for_drain`.

### Scheduler Deferred Jobs

P2b.4f added a dispatcher signal for scheduler-mode host-pool lease contention:
`scheduler_defer_reason = "host_pool_lease_unavailable"` on a non-terminal
pending target result. The remaining scheduler loop must consume this signal
instead of treating the worker result as terminal success or failure.

Done as P2b.5i: worker-side detection now checks
`TargetResult::is_scheduler_deferred()` at the per-target return point, before
the target result is persisted into the durable job. `execute_targets` now uses
a typed internal target-execution outcome so it can distinguish "incomplete,
transiently deferred, and safe to retry" from terminal completion or failure:

```rust
enum TargetExecutionOutcome {
    Completed(Job),
    Cancelled(Job),
    Deferred {
        job: Job,
        reason: String,
    },
}
```

A scheduler-deferred target is not persisted as a final target result and does
not let the worker complete a job whose target map still contains non-terminal
`Pending` results.

Concrete P2b behavior:

- If every unfinished target is scheduler-deferred for transient lease
  unavailability, the drain owner should move the queue job back from
  `Running` to `Pending` and clear transient target results that should be
  retried.
- Requeued jobs record durable backoff/debug metadata:
  `scheduler_defer_count`, `scheduler_defer_reason`, and
  `scheduler_defer_until`.
- A deferred job must continue to count as occupied only while it is `Running`;
  once returned to `Pending`, future admit passes re-evaluate it from the latest
  host-pool leases and running request plans.
- A non-transient target failure must stay terminal and must not be converted
  into a scheduler deferral.

Done as P2b.5i: `Queue::requeue_deferred_running_jobs_for_drain` provides a
drain-owned queue mutation primitive for `Running -> Pending`. It refuses to
requeue terminal or unrelated jobs, clears non-terminal transient target
results, preserves terminal results, increments `scheduler_defer_count`, and
records `scheduler_defer_reason` plus optional `scheduler_defer_until` for
status/debugging and bounded retry.

Current limitation: `scheduler_defer_until` is persisted for status/debugging,
but admission does not yet use it to delay a retry, and there is no hard
terminal cap on `scheduler_defer_count`. A future polish should either enforce
the persisted backoff/cap or remove the field if immediate re-admission remains
the intended behavior.

### Submitter Exit Policy

For default `shipyard run` and `shipyard ship`, the process should stop
admitting new jobs once its submitted job is terminal. It must wait for any
workers it already spawned before releasing the drain lock. This preserves the
synchronous command UX and avoids a single command unexpectedly draining the
entire queue forever.

The drain owner cannot release `queue.lock` while any worker it spawned is still
live, including workers for jobs submitted by other waiting processes. If the
owner's own job finishes quickly, it may still need to keep the process alive
until sibling workers finish. Follow-up UX polish can surface this as a concise
`waiting on N sibling worker(s)` status line; the current first release does
not promise that human output.

Later, a separate explicit `shipyard queue drain` or daemon-owned drain mode
can run until the queue is idle.

### Submitter Waiting And Output

The losing submitter path must never reconstruct command output from in-memory
request state. It should wait by polling durable queue state and then read the
matching `QueueOutcomeStore` record once its job is terminal.

Required behavior:

- Workers must persist the outcome snapshot before marking the job terminal in
  `queue.json`; terminal queue state without a matching required outcome is an
  error, not permission to rerun or synthesize output.
- If the submitted job completes normally, render the existing `run` or `ship`
  output from the final queue job plus outcome snapshot.
- If the submitted job is cancelled before a worker writes an outcome, render
  cancellation from the durable queue job alone.
- If the submitted job is terminal but the expected outcome snapshot is missing,
  report a clear queue outcome error rather than re-running targets or silently
  inventing output.
- If `queue.json` no longer contains the submitted job because terminal history
  was trimmed, read the outcome snapshot if present. If neither queue job nor
  outcome snapshot is available, report a clear `queue trimmed before outcome
  was observed` error.
- Non-owners periodically retry drain ownership so abandoned queues can recover,
  but they must not call worker functions while another process holds
  `queue.lock`.

Done as P2b.5j for the pre-drain synchronous path: `shipyard run` and
`shipyard ship` now submit durable jobs through `submit_run` / `submit_ship`,
execute the worker for the submitted job, and render from
`QueueOutcomeStore` plus the final durable queue job via `load_run_outcome` /
`load_ship_outcome`. This moved CLI output onto the durable outcome-read path
that losing submitters now also use.

Done as P2b.5k for the current single-worker path: `drain_or_wait_run` and
`drain_or_wait_ship` implement cooperative submitter-owned drain ownership.
After submitting a durable job, a process first checks durable queue/outcome
state; if the job is not terminal it attempts to acquire `queue.lock`. The
owner runs the submitted job's worker and renders from the durable outcome. A
non-owner only polls durable state and periodically retries drain ownership; it
does not call `execute_run_worker` or `execute_ship_worker` while another
process owns the drain lock. This still does not admit sibling jobs or spawn
worker threads; that remains P2b.5l.

Done as P2b.5l: the drain owner now runs one bounded worker-admission cycle.
It snapshots queue jobs, builds a request-backed admit pass from
`QueueRequestStore`, applies drain-owned cancellations/starts, hydrates each
started request back into a `RunExecutionRequest` or `ShipExecutionRequest`,
spawns scoped in-process workers, reaps them, and returns scheduler-deferred
host-pool lease misses to pending through
`Queue::requeue_deferred_running_jobs_for_drain`. Workers open the actual queue
root from the owner queue handle, so durable job updates land in the same queue
even when tests use a separate queue root and runtime state dir. This is the
first concurrent worker path; broader integration tests still belong to
P2b.5n.

## Resource Model

P2b should use conservative job-level resource plans. A job claims the union of
resources that any of its targets may need for the whole job duration. This is
less parallel than per-target scheduling, but it keeps this phase tractable and
still unlocks multiple-Mac throughput.

Target resource-plan shape for the scheduler:

```rust
pub struct JobResourcePlan {
    pub exclusive_claims: BTreeSet<String>,
    pub pool_demands: Vec<HostPoolDemand>,
}

pub struct HostPoolDemand {
    pub pool_name: String,
    pub capability_key: String,
    pub slots: u32,
}
```

Done as P2b.4c: the persisted `JobResourcePlan` now keeps compatibility fields
(`targets`, `cloud_targets`, `host_pools`) and adds scheduler-facing
`exclusive_claims`. Request envelope construction derives claims from the full
run/ship request context, including branch, PR identity, original CLI cwd, and
resolved backend details.

### Common Claims

Add these conservative claims where applicable:

- `evidence:<branch>:<target>` to avoid concurrent writes racing for the same
  branch and target evidence record.
- `ship-state:<repo>:pr-<pr>` for `ship` jobs so only one job mutates a PR's
  ship state.
- `warm:<target>:<host_key>` for warm-pool eligible targets. The warm pool now
  has file locking, but the claim remains useful for conservative scheduler
  admission until per-target scheduling exists.

### Local

Claim:

```text
local-cwd:<canonical-cwd>
```

If the resolved local target has no configured `cwd`, use the submitting
request's `cwd`. Canonicalization should be best effort when the path exists.
If a local job's effective cwd does not exist, refuse to admit it rather than
falling back to a lexical key that could diverge from another process's
canonical key for the same directory.

### SSH

Claim:

```text
ssh-repo:<host>:<repo_path>
```

Different repo paths on the same host may run concurrently. The same repo path
on the same host may not.

### Windows SSH

Claim:

```text
ssh-windows-repo:<host>:<repo_path>
```

Use the same conflict rule as POSIX SSH.

### Cloud

Do not add a global cloud claim. GitHub Actions already provides queueing across
workflow runs, and two cloud jobs against different PRs or branches should be
allowed to overlap in P2b. If finer scoping becomes necessary, prefer
`cloud-repo:<repo>` over `cloud-serial` and revisit adaptive cloud overflow in
P3a.

### Host Pool

For `ResolvedBackend::HostPool`, the scheduler should not claim every member.
That would serialize the whole pool and defeat P2b.

Instead:

1. Filter members with the same `requires` logic used by dispatch.
2. Count eligible capacity by pool and capability key.
3. Demand one slot for each host-pool target in the job.
4. Admit the job only if running jobs plus non-stale active leases leave enough
   capacity for the demand.

Done as P2b.4e: `queue_scheduler::host_pool_capacity_deficits` implements this
capacity check as a standalone planning primitive. It counts configured member
capacity, subtracts non-stale active leases and overlapping running resource
reservations, and reports deficits without starting workers. The persisted
resource plan now folds duplicate same-pool host-pool demands by incrementing
`slots` instead of deduping away repeated targets.

The dispatch layer remains the final authority. A worker still must acquire a
real `HostPoolLeaseStore` lease before validation. The scheduler-side capacity
check is a hint, not the authority. If `HostPoolLeaseStore::acquire` returns
`None` because capacity changed between scheduling and execution, the worker
must surface a scheduler-level deferred/lease-unavailable outcome rather than a
final target result. The scheduler then returns the job to `Pending` or retries
with bounded backoff. Today's `host_pool_busy_result` must remain reserved for
non-scheduler command paths or permanent ineligibility, not transient capacity
contention under P2b.

Done as P2b.4f: `DispatchValidationRequest` has an opt-in
`defer_host_pool_lease_unavailable` flag. When false, existing synchronous CLI
behavior is unchanged and a busy member produces the existing terminal
host-pool busy result/failover path. When true, transient lease contention
returns a non-terminal `TargetStatus::Pending` result with
`scheduler_defer_reason = "host_pool_lease_unavailable"` and no failure class,
so the P2b.5 scheduler can return the job to pending or retry with backoff
without recording a final failed target result.

Done as P2b.4d: queued target execution threads the queue job id through
`DispatchValidationRequest`, host-pool and fallback nested dispatch preserve it,
and host-pool lease acquisition sets `HostPoolLeaseRequest.job_id`. Pool status
can then show the owning queue job.

For P2b, treat `max_concurrency > 1` on a single member as supported only if
the lease store already enforces it and the operator explicitly configured it.
The scheduler should still be conservative in docs and tests; the primary use
case is one slot per Mac.

### Fallback

For `ResolvedBackend::Fallback`, claim only the primary backend's resources at
admit time. Secondary backends in the fallback chain are not admit-time claims;
runtime fallback should rely on the same per-attempt lease, lock, and probe
paths used by first-class backends. Claiming every possible fallback would
serialize unrelated jobs through machines or pools they may never reach.

## Store Locking Hazards

### Queue

Done in P2b.1: queue read-modify-write operations use `queue.state.lock`.

### Warm Pool

Done as P2b.4a: `WarmPool::upsert`, `evict`, `drain`,
`prune_expired`, and direct `save_entries` writes are guarded by a warm-pool
file lock. The public `with_entries_locked` helper supports future scheduler
mutations that need to keep read/modify/write in one critical section.

### Evidence

Done as P2b.4a: `EvidenceStore::record` uses a per-branch evidence lock before
loading, inserting, and rewriting a branch JSON file. The public
`with_branch_records_locked` helper supports future scheduler mutations in the
same critical section while allowing unrelated branches to proceed.

### Ship State

Partially done as P2b.4a: `ShipStateStore` has per-PR locks, locked get/save/
archive helpers, and a `with_pr_state_locked` helper for read/modify/write
updates. `execute_ship_worker` now holds the PR lock across its ship-state
lifecycle, including the `resumed_existing_state` check and final save.

Decision: `execute_ship_worker` intentionally holds the per-PR ship-state lock
across the full target loop for that PR. This serializes reconcile, add-lane,
retarget, and auto-merge mutations against an active ship worker for the same
PR. Do not narrow that critical section without re-solving the
`resumed_existing_state` and ship-state lifecycle TOCTOU.

Done as P2b.4b: cloud add-lane/retarget, daemon reconcile, manual
`ship-state reconcile`, daemon PR-close archival, and auto-merge archival now
use the per-PR lock helpers for their active ship-state mutations.

Ongoing hygiene: keep future ship-state writers on the locked helper APIs, and
audit new writers before widening concurrency or introducing daemon-owned drain.

### Host Pool Leases

`HostPoolLeaseStore` already uses advisory locking and remains the
authoritative capacity gate. Queued host-pool execution passes
`HostPoolLeaseRequest.job_id = Some(job.id.clone())`, so pool status can show
the owning queue job.

## CLI And JSON Compatibility

### `shipyard status --json`

Keep existing fields where possible:

```json
{
  "queue": {
    "pending": 2,
    "running": 2,
    "completed_recent": 5
  },
  "active_run": { "id": "oldest-running-job" },
  "active_runs": [
    { "id": "job-a" },
    { "id": "job-b" }
  ]
}
```

`active_run` remains the first running job for older consumers.
`active_runs` is the new authoritative list.

### `shipyard queue --json`

Keep `active` as the first running job or `null`, and add `active_runs`.

Human output should show `Running (N)` followed by one row per running job.

### `shipyard cancel`

For pending jobs: mark `Cancelled` immediately.

For running jobs under the current implementation: mark the durable job
terminal `Cancelled`; workers observe that state at their next durable-state
poll and stop without overwriting it.

Decision for P2b: `shipyard cancel` should return success once the durable
cancellation request is recorded for a pending or running job. Human output may
continue to print `Cancelled <job_id>`, and the JSON envelope remains the
existing job envelope with `command: "cancel"` and `job.status: "cancelled"`.
Docs and JSON consumers should treat running-job cancellation as cooperative:
the worker may still be winding down until its next durable-state poll.

Cancellation latency is bounded by worker poll points. Local/SSH/Windows/cloud
commands are not killed in P2b; a long target can delay cancellation
observation until the next target boundary or progress callback.

### `shipyard watch --json`

P2b does not change the watch event stream shape. GUI and automation consumers
should continue reading ship-state/watch events as before; queue concurrency is
surfaced additively through queue/status `active_runs` and through the existing
per-target ship-state rows.

## Implementation Slices

### P2b.1 - Queue State Safety

- Done: refactor `Queue` into a file-backed handle with `queue.state.lock`.
- Done: add `get_running`.
- Done: keep `get_active` as first-running compatibility helper.
- Done: change supersedence to cancel pending jobs instead of deleting them.
- Done: include cancelled jobs in recent terminal history and trimming.
- Done: add tests for concurrent update preservation using two `Queue` handles.
- Done: add tests that opening a non-drain `Queue` handle never performs
  stale-running recovery or mutates `queue.json` on load.

Acceptance:

- Existing queue tests pass.
- New tests prove two handles cannot lose independent target progress updates.
- New tests prove stale-running recovery only runs from a drain-owner scheduler
  path.
- `queue` and `status` outputs still pass existing tests.

### P2b.2 - Request And Outcome Stores

- Done: add `QueueRequestStore`.
- Done: add `QueueOutcomeStore`.
- Done: add queue-owned request snapshot types for resolved execution requests.
- Done: add `JobKind`, cancellation reason, cancellation request timestamp, and
  resource-claim debug fields.
- Done: define legacy behavior when `JobKind` is absent on pre-P2b queue records;
  default old records to run-like queue display unless a durable ship request
  envelope exists.
- Done: persist enough ship request identity to detect same `(repo, pr)`
  pending and running jobs before enqueue. Same-PR enforcement still belongs to
  P2b.5 scheduler/submitter admission.

Acceptance:

- Round-trip tests for queued run and ship requests.
- Bundle rejects unknown schema versions.
- Request snapshots contain no token fields.

### P2b.3 - Execution Split

- Done as P2b.3a: current inline `execute_run` and `execute_ship` persist
  request envelopes before enqueue and outcome envelopes after terminal queue
  state, using the original CLI cwd carried through `RunStores`/`ShipStores`.
- Done as P2b.3b: `shipyard run` now has `submit_run` and
  `execute_run_worker` helpers under the synchronous `execute_run` wrapper.
- Done as P2b.3c: `shipyard ship` now has `submit_ship` and
  `execute_ship_worker` helpers under the synchronous `execute_ship` wrapper.
- Done as P2b.3c: ship-state load/create/save moved into
  `execute_ship_worker`.
- Done as P2b.3d: workers re-read durable queue state before start and
  before/after each target, honoring durable job cancellation without
  dispatching new work or overwriting cancellation with completion.
- Done as P2b.3e: progress callbacks check durable cancellation before and
  after progress writes, so a cancelled job is not overwritten by later target
  results.
- Done as P2b.3f: `Queue` has a drain-owned orphan pending cancellation
  primitive for jobs whose durable request envelope is missing or unreadable;
  scheduler/request-store wiring is still pending.
- Done: progress updates use `Queue::update`, which is backed by the locked
  queue handle. Worker signatures still take `&mut Queue` for compatibility,
  but the handle no longer caches jobs.
- Done: inline workers write outcome snapshots when done. Detached worker
  ownership is still pending.

Acceptance:

- Existing `run` and `ship` focused tests pass with the new execution split.
- A submitter can render final output by reading the final job/outcome from
  disk.

### P2b.4 - Resource Planner

- Done as P2b.4a: add per-branch evidence locking so concurrent jobs on the
  same branch cannot lose records.
- Done as P2b.4a: add warm-pool JSON locking around upsert, evict, drain,
  prune, and direct save operations.
- Done as P2b.4a: add per-PR ship-state lock helpers and wire the main
  `execute_ship_worker` ship-state lifecycle through the PR lock.
- Done as P2b.4b: migrate cloud add-lane/retarget, daemon reconcile, manual
  `ship-state reconcile`, daemon PR-close archival, and auto-merge archival to
  the per-PR ship-state lock helpers.
- Done as P2b.4c: extend persisted `JobResourcePlan` with scheduler-facing
  exclusive claims for local cwd, SSH repo, Windows repo, evidence, warm-pool,
  and ship-state resources while preserving existing serialized fields.
- Done as P2b.4c: add resource-plan construction for local, SSH, Windows,
  cloud, host-pool, and fallback backends. Cloud targets do not receive a
  global exclusive claim. Host-pool targets record pool demand instead of member
  serialization claims. Fallback targets claim only the primary backend at
  admit time.
- Done as P2b.4d: pass queued job ids through dispatcher validation requests
  and into host-pool lease acquisition.
- Done as P2b.4e: add scheduler host-pool capacity math based on eligible
  members, overlapping running reservations, and non-stale leases.
- Done as P2b.4f: define and implement the host-pool lease-unavailable/deferred
  contract between dispatcher and scheduler. Transient scheduler-mode capacity
  contention is surfaced as a non-terminal scheduler-deferred target result, not
  a final failed target result.

Acceptance:

- Unit tests cover each backend's resource-plan claims. Done for persisted
  request resource-plan construction in P2b.4c; scheduler admission tests still
  belong to P2b.5.
- Scheduler refuses same local cwd, same SSH host/repo, same Windows host/repo,
  and same PR ship-state overlap. Done for the P2b.5a admission primitive;
  queue-state wiring remains pending.
- Scheduler allows unrelated cloud jobs to overlap. Done for the P2b.5a
  admission primitive; queue-state wiring remains pending.
- Scheduler allows two host-pool jobs when two eligible one-slot members exist.
  Done for the standalone P2b.4e capacity primitive; scheduler admission wiring
  remains P2b.5.
- Scheduler does not serialize a fallback job against secondary backends it has
  not attempted. Done for the P2b.5a admission primitive; queue-state wiring
  remains pending.
- Worker-side host-pool validation can signal transient lease unavailability to
  the scheduler without writing a final failed target result. Done as P2b.4f
  for dispatcher signaling; P2b.5 still needs to consume the signal and requeue
  or retry.
- Workers pass `HostPoolLeaseRequest.job_id = Some(job.id.clone())`, and pool
  status can show the owning queue job. Done as P2b.4d for queued target
  execution; scheduler worker ownership is still pending in P2b.5.
- Concurrent evidence, warm-pool, and ship-state mutation tests retain both
  writers' changes. Done for the new store helper APIs in P2b.4a.

### P2b.5 - Scheduler

- Done as P2b.5a: add a pure scheduler admission primitive that reports
  exclusive-claim and host-pool capacity blockers for a pending job.
- Done as P2b.5b: add a pure greedy admit-pass planner that admits compatible
  pending requests, defers blocked requests, and reports missing/unreadable
  request envelopes as orphaned pending jobs for later drain-owned
  cancellation.
- Done as P2b.5c: load pending and running request envelopes from
  `QueueRequestStore` before planning an admit pass, including pending
  priority/FIFO sorting and running-envelope load error reporting.
- Done as P2b.5d: report same-PR ship admission decisions from loaded
  request envelopes before generic admission planning. The request-backed
  planner now identifies older pending same-PR ship jobs for later drain-owned
  cancellation and excludes pending same-PR ship jobs while a matching ship is
  already running. This is still pure planning; queue mutation wiring remains
  pending.
- Done as P2b.5e: add drain-owned queue mutation primitives for the scheduler
  loop. `Queue::start_pending_jobs_for_drain` transitions selected pending jobs
  to running in the admit-plan order, and `Queue::cancel_pending_jobs_for_drain`
  cancels selected pending jobs by id with caller-provided reasons. Both require
  a held `DrainLock`; worker spawning and full admit-pass orchestration remain
  pending.
- Done as P2b.5f: add `queue_scheduler::apply_admit_pass_for_drain`, a
  drain-owned bridge from request-backed planning to queue mutation. It cancels
  orphaned and superseded same-PR pending jobs, starts admitted jobs in plan
  order, and skips starts when running request envelopes cannot be loaded. It
  still does not spawn workers or implement submitter wait/drain ownership.
- Done as P2b.5g: add durable request hydration from `QueueRequestStore`
  snapshots back into executable `RunExecutionRequest` and
  `ShipExecutionRequest` values. The reverse conversion preserves nested
  host-pool, fallback, SSH, Windows, cloud, local validation, contract, and
  target metadata so a future drain worker can load admitted jobs from disk
  instead of relying on submitter stack state.
- Done as P2b.5h: resolve the started-job worker handoff. `execute_run_worker`
  and `execute_ship_worker` now accept jobs already transitioned to `Running`
  by the drain owner while preserving the existing synchronous pending-job
  start path. This unblocks later worker spawning from
  `apply_admit_pass_for_drain`.
- Done as P2b.5i: add scheduler-deferred worker detection and a drain-owned
  requeue/defer primitive for scheduler-deferred host-pool lease contention.
  Deferred target results are intercepted before persistence, and requeued jobs
  carry bounded retry/debug metadata.
- Done as P2b.5j: swap the run/ship CLI entry points to submit durable jobs
  and render from durable queue/outcome state instead of calling the legacy
  submit-then-inline `execute_run` / `execute_ship` wrappers. `submit_ship`
  now refuses a newer same-PR ship request before enqueue when a matching ship
  is already running, and points the operator to watch/queue/status.
- Done as P2b.5k: add the cooperative drain wait/ownership loop for the
  current submitted job. Submitters wait for their own job and attempt to
  become drain owner; a non-owner never calls `execute_run_worker` or
  `execute_ship_worker` while another process owns `queue.lock`.
- Done as P2b.5l: drain owner hydrates admitted requests from
  `QueueRequestStore`, starts compatible pending jobs concurrently, spawns
  in-process workers, reaps workers, handles cancellation/defer outcomes, and
  preserves existing synchronous command output through durable queue/outcome
  reads.
- Done as P2b.5m: request/outcome retention is tied to drain-owned terminal
  queue trimming through a grace-window sweep for jobs no longer present in
  `queue.json`; run/ship workers now persist outcomes before terminal queue
  updates; the drain worker caps the first concurrent admission burst at two
  workers minus already-running jobs.
- Done as P2b.5n: `tests/queue_concurrency.rs` provides end-to-end coverage for
  overlapping non-conflicting jobs, serialized conflicting jobs, same-PR
  pending ship supersedence, abandoned drain recovery after admit/start, and
  losing submitters that wait without dispatching targets.

Acceptance:

- Done: integration test with fake dispatcher and barriers proves two non-conflicting
  jobs overlap.
- Done: integration test proves conflicting jobs serialize.
- Done: integration test proves a newer same-PR ship cancels an older pending
  ship. Same-PR running enqueue refusal remains covered by focused `ship::`
  tests.
- Done: drain-owner death recovery marks running jobs completed with stale
  recovery results only from a new drain-owner scheduler pass.
- Done: integration test proves a drain owner that dies mid-cycle after admit/start
  but before workers finish is recovered by the next drain owner: orphaned
  running jobs become stale-recovery terminal, and deferred jobs that did not
  reach requeue do not silently become terminal failures.
- Done: losing submitters wait on durable queue/outcome state and never dispatch
  targets while another process owns the drain lock.

### P2b.6 - Status, Docs, And Skills

- Done: add `active_runs` to queue/status JSON while preserving existing
  singular fields.
- Done: render multiple running jobs in queue human output.
- Done: update docs and skills to describe host-pool throughput under P2b queue
  concurrency, while preserving resource-conflict and capacity caveats.
- Keep `planning/phase-handoff-status.md` current.

Acceptance:

- Existing JSON consumers can still read `active` or `active_run`.
- New tests cover multi-running status output.
- `cargo fmt -- --check` and `git diff --check` pass.

## Validation Plan

Focused tests:

```bash
cargo test queue::
cargo test job::
cargo test host_pool::
cargo test executor::dispatch::
cargo test app::queue_cmd::
cargo test app::run_cmd::
cargo test app::ship_cmd:: -- --skip ship_command_green_merge_failure_keeps_active_state_and_exits_success
```

Specific scheduler/queue invariants to keep covered:

```bash
cargo test queue::tests::non_drain_open_does_not_recover_or_mutate_running_jobs
cargo test queue::tests::drain_owner_starts_selected_pending_jobs_in_requested_order
cargo test queue_scheduler::tests::apply_admit_pass_cancels_orphans_and_same_pr_then_starts_admitted_jobs
```

P2b.5n added end-to-end queue concurrency integration coverage in
`tests/queue_concurrency.rs`, rather than relying only on queue and scheduler
unit tests.

CLI smokes:

```bash
cargo run --quiet -- --mode isolated --cwd <tmp git repo> --state-dir <tmp> --json run --targets mac
cargo run --quiet -- --mode isolated --cwd <tmp git repo> --state-dir <tmp> --json queue
cargo run --quiet -- --mode isolated --cwd <tmp git repo> --state-dir <tmp> --json status
cargo run --quiet -- --mode isolated --cwd <tmp git repo> --state-dir <tmp> --json targets pool status
```

Do not claim full-suite green until the known auto-merge failures are either
fixed or explicitly excluded with rationale.

## Closed Implementation Decisions

- Keep the first implementation inside submitter-owned cooperative drains. An
  explicit `shipyard queue drain` command or daemon-owned drain loop can reuse
  the same scheduler later, but P2b should not expand the user-facing command
  surface until submitter-owned concurrency is proven.
- Done as P2b.5m: use a conservative worker cap before P2b.5n broad
  integration testing. Resource plans and host-pool capacity still gate actual
  admission, but `DEFAULT_DRAIN_MAX_WORKERS = 2` limits blast radius during the
  first concurrency release.
- Do not offer early detach in the first P2b implementation. A drain owner must
  stay alive until every worker it spawned is terminal before releasing
  `queue.lock`.

## Next Step

P2b.1 through P2b.6 are implemented. Continue with final packaging and PR
readiness checks. The non-blocking macOS GUI compatibility check in
`/Users/danielraffel/Code/shipyard-macos-gui` passed at compile level with an
unsigned macOS Debug build; signed build/test attempts were blocked by Xcode
probing a locked attached iOS device, and the GUI app was not launched.

Do not start adaptive routing until P2b queue concurrency exists.
