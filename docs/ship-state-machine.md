# Ship state machine (audit — Phase A)

This document is the [#101](https://github.com/danielraffel/Shipyard/issues/101)
Phase A deliverable: a hand-written map of every state, every transition,
and every external dependency in the `shipyard ship` / `shipyard watch` /
`shipyard auto-merge` flow, written by reading the code end-to-end and
reviewed by a second pass (Codex via RepoPrompt MCP) that cross-checked
each claim against the Shipyard implementation at exact line numbers.

Keep this doc in step with `src/ship_state.rs`, `src/ship.rs`,
`src/app/ship_state_cmd.rs`, `src/app/ship_cmd.rs`,
`src/app/watch_cmd.rs`, `src/app/auto_merge_cmd.rs`, and
`src/app/cloud_cmd.rs`.

**Phase B** (transition tests, see below) and **Phase C** (pre-merge
doc-sync hook, dedicated CI lane) land in follow-up PRs.

## Vocabulary note: the state labels are derived

The labels in the diagram below (`STATE_FRESH`, `STATE_IN_FLIGHT`,
`STATE_VERDICT_PASS`, etc.) are **not persisted**. `ShipState.to_dict()`
does not carry a state enum. Every label is a predicate over the tuple
`(evidence_snapshot, dispatched_runs, state file present?, archive file
present?)`. The names exist so Phase B tests can reference edges
unambiguously — they are test vocabulary, not runtime observables.

## The core persisted object: `ShipState`

`ShipState` lives at
`<state_dir>/ship/scoped/<repository-key>/<pr>.json` during the active ship.
The repository key is a filesystem-safe encoding of the canonical owner/name.
Older `<state_dir>/ship/<pr>.json` records are compatibility mirrors that a
scoped operation migrates without allowing one repository to claim another
repository's matching PR number. When two repositories share a PR number, a
durable `<pr>.scoped-collision` fence makes newer binaries ignore any legacy
mirror recreated by an older process until the number is unambiguous again.
The active file is archived to
`<state_dir>/ship/archive/<repository-key>/<pr>-<utc>.json` on one of:

- `shipyard ship` success (`merge_pr` returned a merged PR)
- `shipyard auto-merge` success (same)
- `shipyard ship-state discard <pr>` (manual tombstone — works on any
  active state, not only MERGED)

Failed verdicts (`STATE_VERDICT_FAIL`), refused merges, and merge
attempts that hit a GhError all leave the active file in place for
inspection. `shipyard cleanup --ship-state` ages these out (see T12).

### Writer-domain audit boundary

The per-PR `ShipStatePrLock` remains the semantic concurrency boundary for
ship-state transitions. When the store lives beneath a Sandbox-audited real
home, directory creation, atomic saves, archive/delete operations, legacy
mirror reconciliation, and coordination-lock-file creation additionally join
the machine-wide writer domain. That lease is bounded and mutation-scoped.
State-file mutations acquire it after any required per-PR lock is held,
immediately before the filesystem mutation, and release it when that mutation
completes. Directory and coordination-lock-file creation use a short creation
guard, which is dropped before waiting on the coordination lock; this avoids a
writer-domain/application-lock inversion. Read-only operations on an
initialized store do not join the writer domain; first-use directory creation
is itself a mutation and is fenced. This audit boundary changes neither the
persisted schema nor any state transition; it allows an idle daemon to coexist
with an exclusive Sandbox snapshot-to-contamination audit while preventing a
real ship-state write from overlapping that audit.

`ShipState` carries:

| Field               | Purpose                                                                             |
|---------------------|-------------------------------------------------------------------------------------|
| `pr`                | GitHub PR number — unique only within `repo`; `(repo, pr)` is the durable identity. |
| `repo`              | Owner/name (`danielraffel/pulp`) captured at dispatch and used for scoped persistence, lookup, cleanup, and GitHub routing. |
| `branch`            | PR head branch.                                                                     |
| `base_branch`       | Merge target.                                                                       |
| `head_sha`          | PR head SHA at dispatch. Drift vs this value refuses resume.                        |
| `policy_signature`  | SHA-256[:16] of (required_platforms, target_names, mode) at dispatch. Drift refuses resume. |
| `dispatched_runs`   | List of `DispatchedRun`. Upsert key is `(target, run_id)`, not just `target` — a single target can hold multiple rows if a new run id was issued (e.g. from a peer dispatch under the same state). Phase B should either add deduplication logic or document the multi-row invariant. |
| `evidence_snapshot` | `{target: "pass" | "fail"}` written by `_update_ship_state_from_job` (cli.py:4576). No other values are ever written by the normal path — `"pending"` is accepted by `_ship_terminal_verdict` but never produced. |
| `attempt`           | Intended to be a monotonic counter bumped on `--no-resume`. **Currently broken** — see T8 and "Bugs discovered by this audit" below. |
| `pr_url`, `pr_title`, `commit_subject` | Human context. Refreshed by the `ship` resume path (cli.py:2679) on each invocation; NOT refreshed by add-lane's `save` (cli.py:2359) or by `_update_ship_state_from_job`. Test coverage: `ship-state show` after a force-push + `shipyard ship` resume should see updated fields; after a `cloud add-lane` against the same state, should not. |
| `created_at`        | Attempt-scoped: stable for the life of an attempt.                                  |
| `updated_at`        | Last `touch()` — bumped after every mutation helper.                                |
| `schema_version`    | `SCHEMA_VERSION` (currently 1). `from_dict` defaults to `SCHEMA_VERSION` when reading older files that omit it.                                                                 |

`DispatchedRun` is the per-dispatch record (not strictly per-target — see
`dispatched_runs` note above):

| Field                | Purpose                                                                            |
|----------------------|------------------------------------------------------------------------------------|
| `target`             | Lane name (`macos`, `ubuntu`, …) — matches `[targets.<name>]` in `.shipyard/config.toml`. |
| `provider`           | Dispatch channel or backend label: `namespace`, `github-hosted`, `ssh`, `ssh-windows`, `local`, `host_pool`, etc. |
| `run_id`             | GH Actions run ID for cloud, Shipyard job id for local/SSH/host-pool work, or `pending-<target>` when `cloud add-lane` couldn't discover the real run id. **No code backfills this sentinel today** — `watch` is read-only with respect to ship state (cli.py:3497). |
| `status`             | Last observed lifecycle string. `cloud add-lane` records `queued`; the Rust ship worker mirrors terminal target results as `completed` or `failed`. `reused` is **not** a valid `DispatchedRun.status` — cross-PR evidence reuse synthesizes a `TargetStatus.PASS` with `backend="reused"` (cli.py:4510) and persists it as `status="completed"` (cli.py:4586). |
| `attempt`            | `ShipState.attempt` at dispatch time. Intended to survive resume so old attempts don't reattach, but coupled to the broken `attempt` counter from T8. |
| `last_heartbeat_at`  | Additive liveness signal (default `None`) — written by the poller via `_update_ship_state_from_job`, used by `watch` to mark `stale` runs. |
| `phase`              | Additive validation-phase tag (setup/configure/build/test, default `None`), same source as `last_heartbeat_at`. |
| `required`           | Lane policy **at dispatch time**, snapshotted in `DispatchedRun.required` by add-lane (cli.py:2357) and by `_update_ship_state_from_job` (cli.py:4593). `from_dict` defaults to `True` for legacy files written before #87. `_ship_terminal_verdict` reads this persisted value (cli.py:3809) to decide which failures tolerate. |

## State diagram (textual)

```
                          ┌─────────────────────────────────────────┐
                          │   No state file exists for this PR      │
                          └───────────────────┬─────────────────────┘
                                              │
                                              ▼  shipyard ship (first run — state saved BEFORE preflight)
                                   ┌──────────────────────┐
                                   │   STATE_FRESH        │
                                   │   evidence_snapshot  │
                                   │   is empty; may have │
                                   │   zero or more       │
                                   │   DispatchedRuns     │
                                   │   if add-lane hit    │
                                   │   this PR before     │
                                   │   ship completed     │
                                   └───────────┬──────────┘
                                               │  _execute_job ends;
                                               │  _update_ship_state_from_job writes
                                               │  one evidence row per terminal target
                                               │  IN A SINGLE save (not per-target)
                                               ▼
                                   ┌──────────────────────┐
                                   │   STATE_IN_FLIGHT    │◀────┐
                                   │   some evidence      │     │ cloud add-lane
                                   │   rows written       │─────┘   (appends DispatchedRun)
                                   │   but not a full     │
                                   │   verdict            │         cloud retarget
                                   │                      │         (dispatches, does NOT
                                   │                      │◀─────── write ShipState — see T9)
                                   └───────────┬──────────┘
                                               │
                       ┌───────────────────────┼───────────────────────┐
                       │                       │                       │
                       ▼                       ▼                       ▼
              ┌────────────────┐      ┌────────────────┐      ┌────────────────┐
              │ STATE_VERDICT  │      │ STATE_VERDICT  │      │ STATE_STALE    │
              │ _PASS          │      │ _FAIL          │      │ (session died; │
              │                │      │                │      │  --no-resume   │
              │ every required │      │ any required   │      │  or drift      │
              │ target has     │      │ target has     │      │  refuses       │
              │ "pass" in      │      │ "fail" in      │      │  resume)       │
              │ evidence AND   │      │ evidence       │      └──────┬─────────┘
              │ every present  │      │                │             │
              │ value is       │      │                │             │ archive_and_replace
              │ terminal       │      │                │             │ (BUG: returned
              │                │      │                │             │  replacement with
              │ ⚠ see Bug B1:  │      │                │             │  bumped attempt is
              │  partial       │      │                │             │  discarded; fresh
              │  coverage can  │      │                │             │  state uses attempt=1)
              │  be false-PASS │      │                │             ▼
              └──────┬─────────┘      └──────┬─────────┘      ┌────────────────┐
                     │                       │                 │ STATE_FRESH    │
          ship       │                       │                 │  (attempt=1)   │
          end-of-    │                       │                 └────────────────┘
          flow or    │                       │
          auto-merge │                       │
                     │                       │
                     ▼                       ▼
              ┌────────────────┐      ┌────────────────┐
              │ STATE_MERGE    │      │ STATE_MERGE_   │
              │  _ATTEMPTING   │      │  REFUSED       │
              │ (no local try/ │      │ (auto-merge    │
              │  catch in      │      │  only; active  │
              │  `ship`; auto- │      │  state file is │
              │  merge catches │      │  retained for  │
              │  GhError)      │      │  inspection)   │
              └──────┬─────────┘      └──────┬─────────┘
                     │                       │
          ┌──────────┴──────────┐             │
          ▼                     ▼             │
  ┌────────────────┐  ┌────────────────┐      │
  │ STATE_MERGED   │  │ STATE_MERGE_   │      │
  │                │  │  FAILED        │      │
  │ merge_pr ok,   │  │ ship: exits 1  │      │
  │ archive call   │  │  on GhError,   │      │
  │ then follows   │  │  no archive;   │      │
  │                │  │ auto-merge:    │      │
  │                │  │  same, also    │      │
  │                │  │  no _pr_is_    │      │
  │                │  │  merged probe  │      │
  │                │  │  (that only    │      │
  │                │  │  fires when    │      │
  │                │  │  the state     │      │
  │                │  │  file is       │      │
  │                │  │  absent)       │      │
  └──────┬─────────┘  └──────┬─────────┘      │
         │                   │                │
         │ archive()         │ (no archive —  │ (no archive —
         │                   │  state lives   │  final verdict
         ▼                   │  for retry)    │  retained)
  ┌────────────────┐         ▼                ▼
  │ STATE_ARCHIVED │  [stays STATE_    [stays STATE_
  └────────────────┘   VERDICT_PASS     VERDICT_FAIL]
                       until archive
                       succeeds on
                       next attempt]
```

## Entry points and which states they read/write

| CLI command                 | Reads                                               | Writes                                                  |
|-----------------------------|-----------------------------------------------------|---------------------------------------------------------|
| `shipyard ship --pr <n>` explicit recovery | Authenticated live PR repository, head branch, and full head SHA; canonical GitHub origin plus the current local branch and full `HEAD` | None until all checkout facts match. A wrong repository, fork origin, branch, detached checkout, abbreviated/different SHA, or unreadable fact rejects before queue, ship-state, evidence, or dispatch mutation. |
| `shipyard pr` prospective changed-surface push | Trusted machine-global `changed_surface_execution`; protected-base selector policy; clean local head/tree; configured repository-relative `core.hooksPath/pre-push` | Before PR discovery, prepares a private nonce-bound prospective receipt only for one non-delete branch update. The hook must be a protected-base-tracked regular file with the platform-valid Git tree mode (executable on POSIX) and byte-identical content before and after the supervised push. Shipyard marks push success in the parent process, then verifies the hook result against the exact head, tree, changed paths, selected tests, and hook digest. No `ShipState` pass evidence is written: missing or drifting identity merely declines the optimization so ordinary full validation remains authoritative. |
| `shipyard ship` schema-v3 selected execution | Protected-base build/test policy plus trusted machine-global activation | Replaces `build` and `test` atomically. `--resume-from test` first authenticates every eligible target read-only, then hard-refuses any schema-v3 transaction before activation persistence or substitution because it could skip producer builds and test stale warm artifacts. If all plans are schema v2 or ineligible, Shipyard preserves the original stages, performs no second observation or activation, and resumes the ordinary test stage. Restart schema v3 from `build` or start fresh. A missing canonical build stage follows the full-preserving fallback. |
| `shipyard ship` stale-base shadow comparison | Exact PR head SHA; old and live protected bases; complete base delta; conflict-free synthesized integration commit/tree; trusted machine-global `shadow_compare` policy | Keeps stale base as the authoritative full-suite disposition. A bounded shadow assessment may materialize a content-addressed integration checkout and run selected-versus-full there. Activation, result, checkout custody, cleanup intent, and restart recovery remain exact-identity fenced. Any ambiguity preserves ordinary full validation; the shadow receipt is always blocked from merge authority and cannot mutate the PR, queue, or runner. |
| `shipyard pr` metadata-only authority | Trusted machine-global `[metadata_authority]` repository policy; exact protected base/head/tree/merge base and complete changed-path closure; configured hosted checks terminal green at the exact head | Replaces native targets with an immutable metadata receipt, so the scheduler allocates no local worker, VM, configure, build, or test capacity. The daemon reloads trusted policy and rechecks the local head/tree plus live GitHub head/check state before accepting the zero-target job. Unknown paths, incomplete observations, stale or duplicate checks, SHA drift, malformed policy, or missing provenance preserve ordinary full validation at submission or refuse stale execution. Tracked project and checkout-local config cannot activate or widen this authority. |
| `shipyard pr` with provenance and/or steward handoff | Project `[pr.provenance]` argv plus the submitting process environment; protected `origin/<base>:.shipyard/config.toml`, or explicit `--workstream-id` / `--context-url` / private `--launch-profile` | After the exact PR and head are resolved, runs the configured provenance hook before any durable receipt or validation dispatch. A required hook failure exits with no steward status/label or queued validation. On success, the handoff first persists private crash-consistent intent, writes `shipyard/steward-handoff`, revalidates the open PR and exact head, adds `shipyard:managed`, and advances the private receipt to ready/managed. With an exact launch profile and enabled trusted consumer it then publishes a zero-wake canonical ledger obligation and only afterward reports `monitoring_transferred=true`; provider delivery cannot decide disposition. `continue` is the default, while `pause` requires a digest-bound durable task graph proving no independent runnable work. Public status exposes only an opaque route id. Replay is idempotent. Explicit `shipyard ship --pr` recovery does not rerun submitter provenance. |
| `shipyard ship` (fresh)     | `ShipStateStore.get_scoped(repo, pr)` (auto-resume decision; returns None) | Saves fresh state BEFORE preflight (cli.py:2675). Calls `_update_ship_state_from_job` once after `_execute_job` ends. `archive_scoped(repo, pr)` on MERGED. |
| `shipyard ship --no-resume` | Same                                                | `ShipStateStore.archive_and_replace(state)` archives prior attempt; then a new `ShipState(...)` is constructed with `attempt=1` (see Bug B2). |
| `shipyard ship --resume`    | Refuses on SHA/policy drift via `_detect_ship_state_drift` | Refreshes `pr_url` / `pr_title` / `commit_subject` on the existing state and saves (cli.py:2679–2689). |
| `shipyard cloud add-lane`   | `ShipStateStore.get_scoped(repo, pr)`; verdict check; idempotent `has_target` | `append_run` + `save`. Does NOT refresh human-context fields. |
| `shipyard cloud retarget`   | None (the command operates on the live GH Actions run; it does not load `ShipState` at all) | **None** — cancels old job, dispatches new workflow; never writes `ShipState`. See T9 + Bug B3. |
| `shipyard watch`            | `ShipStateStore.get_scoped(repo, pr)` loop          | Never mutates; signature-based change detection emits NDJSON. |
| `shipyard auto-merge`       | `ShipStateStore.get_scoped(repo, pr)` + `gh pr view` fallback when state is absent | `archive_scoped(repo, pr)` on success; no writes on failure. `_pr_is_merged` only runs on the no-state branch. |
| `shipyard ship-state list`  | `list_active()` across all repository namespaces; human output includes `repo` beside each PR | None |
| `shipyard ship-state show`  | `get_scoped(checkout_repo, pr)`; unscoped fallback only when the PR number is unambiguous | None |
| `shipyard ship-state discard` | `get_scoped(checkout_repo, pr)` (accepts any state, not only MERGED) | `archive_scoped(repo, pr)` (manual tombstone) |
| `shipyard cleanup --ship-state` | `prune(active_days=14, archive_days=30, closed_prs=...)` | Queries each active state's recorded repository and deletes aged-out active state only for the matching closed `(repo, PR)`, plus aged archives. Unlinks are unguarded — a failure raises. |
| `shipyard runner recovery-worker` | Durable steward recovery requests plus the live exact PR head; trusted machine-global `[merge_steward.recovery_worker]` only | Without `--apply`, no writes and no model launch. With `--apply`, claims at most one request by default and persists a bounded terminal escalation/failure receipt; it never writes ship state or GitHub/queue/merge/release state. |

The daemon's GitHub check-rollup reconciler may heal only dispatched runs whose
`run_id` is a numeric GitHub Actions workflow-run database id. Local and SSH
runs retain Shipyard-generated `sy-*` ids, so their terminal evidence remains
authoritative even when an unrelated hosted check has a similar target name.

## Transitions — preconditions, postconditions, failure modes

### T1 — Create a fresh ship state

- **From:** no state file exists for `(repo, pr)`
- **To:** `STATE_FRESH`
- **Trigger:** `shipyard ship` on a branch
- **Writes:** `ShipStateStore.save(ShipState(..., dispatched_runs=[], evidence_snapshot={}))` at cli.py:2675 — **before** preflight runs at cli.py:2679
- **Externals:** `git push -u origin <branch>` at cli.py:2602 (return code ignored — see "External matrix" below), `gh pr list` / `gh pr create` for PR number. The Rust implementation falls back to REST `gh api repos/<owner>/<repo>/pulls` when GitHub GraphQL is rate-limited, so PR creation can still produce a tracked ship-state record.
- **Failure modes**
  - Explicit `shipyard ship --pr <n>` runs from a checkout whose canonical
    GitHub repository, local branch, or full `HEAD` differs from the
    authenticated live PR → Shipyard rejects before creating or resuming queue,
    ship-state, evidence, or validation work. *Recovery: switch to the exact
    lineage-classified PR worktree and rerun; never adopt the unrelated
    checkout as the PR head.*
  - The exact checkout matches the live PR, but existing scoped ship-state
    records a different head or base → Shipyard rejects synchronously before
    queue insertion. *Recovery: first prove the head change is intentional,
    then rerun with explicit `--adopt-head`; never enable automatic adoption.*
  - Configured PR provenance is malformed, cannot start, or exits nonzero while required → `shipyard pr` exits before the steward status/label or queue/validation state is created. The argv is executed directly, exact PR facts are expanded and exported as `SHIPYARD_PR_*`, and the submitting process environment supplies agent/router context. *Recovery: repair provenance and rerun `shipyard pr`; do not use a recovery agent to overwrite submitter attribution.*
  - Requested steward handoff fails (invalid workstream/context, GitHub write denial, closed PR, exact-head mismatch, conflicting route ownership, or ambiguous provider identity) → `shipyard pr` exits before queue or validation state is created. Private intent is crash-consistent before the first GitHub mutation; status is written before the label and the head is re-read between them, so a concurrent head move can leave only a harmless stale-head status, never a managed label authorized by that stale receipt. Replay paginates status observations and uses the newest matching status to reconcile an uncertain write. Intentional owner replacement requires an explicit transfer with the same immutable work identity. *Recovery: resolve the failure and resubmit the current exact head; do not infer transfer or pause from an existing receipt.*
  - `git push` fails silently → `find_pr_for_branch` may still find an existing PR; the local SHA may not match the remote. A fresh state is saved for a branch whose tip may not be pushed. *Recovery: none automatic — the drift check on the next resume will catch it, but between the stale push and the next resume the state claims a SHA that doesn't exist on the remote.*
  - `gh pr create` fails after the REST fallback also fails → `create_pr` raises `GhError`; `ship` exits without saving state because the save at cli.py:2675 runs only after the PR has been found or created. *Recovery: retry or create the PR through REST and run `shipyard ship --pr <n>` to track it.*
  - `save` fails (disk, permission) → `save` raises; tmp file is cleaned up by the `except` branch in `core/ship_state.py`. *Recovery: resolve disk issue, retry.*

### T2 — Dispatch targets within `_execute_job`

- **From:** `STATE_FRESH` or `STATE_IN_FLIGHT`
- **To:** `STATE_IN_FLIGHT`
- **Trigger:** `_execute_job` per-target loop; or `shipyard cloud add-lane --apply`; or `shipyard cloud retarget --apply` (see T9 — retarget does NOT advance ship state)
- **Writes for the `ship` path:** `_execute_job` does NOT save `ShipState` at each target boundary. It only calls `_update_ship_state_from_job` **once** after `job.complete()` (cli.py:4345), which performs one `save()` for the whole batch (cli.py:4595). Within the loop, only the per-job `queue.update(job)` is written.
- **Writes for `cloud add-lane --apply`:** `append_run(DispatchedRun(..., run_id=discovered or f"pending-{target}"))` then `save`.
- **Queue scheduler note:** The Rust queue path persists a durable
  `QueuedExecutionRequest` next to each queued job. A drain owner may move
  admitted jobs from `Pending` to `Running` before a worker process observes
  them. Workers now accept a job already transitioned to `Running` by the
  drain owner and execute it, instead of trying to start it again. If the
  scheduler starts a job only to defer it for host-pool capacity or an
  unavailable local lease, the drain owner requeues that transient `Running`
  job with a retry timestamp; admission ignores it until that timestamp expires.
  On Unix and macOS, normal `run`, `ship`, and `pr` submission instead hands the
  durable request to the same-version Shipyard daemon and returns after the
  daemon accepts it. The daemon runs one worker at a time in a separate process
  group and records an execution generation plus canonical checkout, origin,
  HEAD, tree, and configuration provenance. After a daemon restart, it adopts
  only an exactly matching live receipt. A `Running` job without that proof is
  terminalized as `UNCERTAIN` and is never replayed. Windows remains explicitly
  foreground-only for this bounded durability slice.
  Before admission, the owner also groups pending ship requests by
  `(repository, PR)` and observes each distinct PR at most once per 30 seconds.
  If GitHub reports `MERGED` and the reported head exactly matches the durable
  queued SHA, every matching pending job is cancelled as already complete.
  Ship validation claims and evidence records use the same `(repository, PR)`
  identity, so same-branch Forge Modular and Forge Sequencer ships can run
  concurrently without replacing one another's target evidence. Repository
  evidence display aggregates those PR namespaces newest-per-target. The
  daemon applies the same exact-head observation to a running ship job only
  with typed `CancellationProof::AlreadyMerged` authority containing the exact
  repository, pull request, and merged head SHA. It first requests cancellation,
  then freezes the exact receipt generation and durably snapshots the complete
  process tree. Termination advances monotonically through `Frozen`, `TreeDead`,
  and `LeasesReleased`; only after tree death and lease release are durable may
  the queue become terminal and the typed outcome be committed. Cleanup removes
  the old receipt and transaction with a generation comparison, so it cannot
  erase a replacement worker receipt. If a restart loses the separate receipt
  after the `Frozen` transaction is durable, the transaction's exact process
  snapshot remains sufficient to finish tree-death proof and release capacity.
  A missing receipt without that transaction, or a present receipt for another
  generation, remains fenced for agent review because detached descendants
  cannot otherwise be proven dead. An open PR, a different merged head, an auth
  or network failure, or a malformed response is a no-op. Stale worker progress
  cannot overwrite the winning terminal record, and a live or unresolved worker
  continues to reserve the sole execution slot until process-tree death is
  proven. Each daemon tick also repairs a missing typed outcome from terminal
  queue state, so a restart after a transient outcome-write failure preserves a
  durable disposition.
  These reads use a refreshable command-sourced GitHub credential, explicit repository scope,
  and one 15-second credential-plus-command budget. An unavailable observation,
  malformed response, or different merged head leaves the job pending.
  A durable `shipyard cancel` observed by a running worker is also an execution
  transition, not merely a queue-state update. The worker's progress callback
  returns `ProgressAction::Terminate`; local and SSH streaming validation then
  terminates its supervised process tree (including descendants) and preserves
  the durable cancellation reason. A cancellation that lands during a silent
  command is observed by the next idle heartbeat.
  A running PR validation also re-reads `headRefOid` through the delayed
  worker's proven command-auth configuration at most once per minute. A
  different valid 40-hex head durably requests cancellation and terminates the
  process tree through the same callback path. Auth/network errors, missing
  fields, and malformed SHAs do not manufacture cancellation; the final merge
  boundary still performs its independent exact-head check.
  The drain owner starts a replacement as each worker completes instead of
  waiting for the whole admitted batch, so an independent idle slot is refilled
  while slower siblings continue. If refill admission itself fails, the owner
  retains that first error but still consumes every active worker completion and
  durably applies any scheduler deferral before returning the error. Completed
  worker threads are joined as each completion arrives, keeping retained thread
  resources bounded by active concurrency. Once the
  job awaited by the invoking command becomes terminal, the owner stops admitting
  replacements but drains workers it already started, allowing the command to
  return without consuming an unbounded stream of unrelated queued work. If the
  admission pass itself cancels the awaited job, its cancellation is applied but
  planned replacement starts are suppressed. Refill
  host-pool accounting counts an active worker once only when its lease member
  is eligible for that `Running` job's matching pool reservation; fallback
  leases outside the reserved capability set remain visible. macOS VM admission remains
  conservative because the live Tart probe cannot identify which VMs correspond
  to queue reservations; treating every reservation as already live can exceed
  the fleet cap while a mixed-target job has not reached its macOS target.
- **Externals:** `workflow_dispatch` (cloud), `find_dispatched_run` (best-effort run id discovery), `ExecutorDispatcher.{probe,diagnose,validate}`.
- **Failure modes**
  - `workflow_dispatch` fails in add-lane → `sys.exit(1)` at cli.py:2328 before any DispatchedRun is appended. *Recovery: retry.*
  - `workflow_dispatch` succeeds but `find_dispatched_run` times out → DispatchedRun is still appended with `run_id="pending-<target>"` (cli.py:2351). **No code backfills this sentinel** — `watch` emits state but never writes it, and `_update_ship_state_from_job` keys its upsert on `(target, run_id)` so a later real dispatch would *append a second row* for the same target rather than overwrite. Phase B test: assert the watcher does not silently drop the pending lane's verdict.
  - Preflight raises `BackendUnreachableError` / `ValueError` → `ship` exits (3 / 1) with the fresh state already on disk from T1. *Recovery: fix backend or use `--skip-target` / `--allow-unreachable-targets`; resume picks up the existing state.*

### T3 — Record terminal target outcomes

- **From:** `STATE_IN_FLIGHT`
- **To:** `STATE_IN_FLIGHT` (with `evidence_snapshot` grown) or `STATE_VERDICT_*` (when `_ship_terminal_verdict` flips; but see Bug B1)
- **Trigger:** `_update_ship_state_from_job` at the end of `_execute_job`.
- **Writes:** The loop at cli.py:4572 mutates `update_evidence(target, "pass"|"fail")` and `upsert_run(...)` for every terminal result, and a single `ctx.ship_state.save(ship_state)` runs after the loop (cli.py:4595). **If the process dies mid-loop, the whole batch is lost** — not just the last record.
- **Externals:** `_cloud_runs_by_platform(ctx, sha)` maps platform → cloud run_id from `CloudRecordStore.list_recent`. See Bug B4: the `sha` parameter is accepted but unused; the map is keyed only by platform, so repeat ships on the same machine can mis-attribute a run_id to a later SHA's DispatchedRun.
- **Failure modes**
  - `save` fails → exception propagates; previous state file is byte-identical thanks to tmp+replace (core/ship_state.py:342–357). *Recovery: retry (the job is terminal in the queue, but the evidence mirror is missing until a future save succeeds).*
  - Advisory lane (`required=False`) failing → evidence records `"fail"` but the verdict computer tolerates it via the persisted `DispatchedRun.required` flag at cli.py:3809.

### T4 — Compute the terminal verdict

- **From:** `STATE_IN_FLIGHT` with at least one row in `evidence_snapshot`
- **To:** `STATE_VERDICT_PASS`, `STATE_VERDICT_FAIL`, or still in flight (`None`)
- **Computation:** `_ship_terminal_verdict(state)` at cli.py:3790
- **Externals:** none
- **⚠ Known bug — Bug B1.** The verdict is computed only from `evidence_snapshot.values()` (cli.py:3806) and `evidence_snapshot.items()` (cli.py:3812). The function does **not** check that every `DispatchedRun.target` has a matching evidence row. A ship that dispatched targets `[macos, ubuntu, windows]` and only persisted evidence for `[macos]` (all "pass") will be reported `STATE_VERDICT_PASS` — and `auto-merge` will proceed to `merge_pr`. This is the single highest-impact silent-failure candidate in the state machine; Phase B must have a dedicated regression test for it.
- **Other failure modes:** none — pure function.

### T5 — Merge on PASS

- **From:** `STATE_VERDICT_PASS`
- **To:** `STATE_MERGED` → `STATE_ARCHIVED`
- **Trigger:** end of `shipyard ship` or `shipyard auto-merge <pr>`
- **Writes:** `merge_pr(...)` (gh); on success, `ctx.ship_state.archive(pr)`
- **Externals:** GitHub merge-queue APIs. Automatic classic direct merge is disabled; a manual maintainer exact-head merge is outside Shipyard.
- **Failure-handling split:**
  - Ordinary downstream merge failure is "green but not merged". Deterministic classic-policy refusal is instead `green_automatic_merge_refused`, exit 10, retains state, and is not retryable.
  - `shipyard auto-merge` returns `merge-failed` only when the PR is still unmerged. If `gh pr merge --delete-branch` exits nonzero after GitHub has already merged the PR (for example, local branch deletion failed because another worktree has it checked out), Shipyard archives state and exits 0 with a `cleanup_warning`.
- **App-authenticated branch cleanup:** after governance selects the native merge queue, `--delete-branch` preflights the machine-global trusted native Git path before queue mutation. Classic-policy refusal happens first, so missing cleanup configuration cannot mask typed exit 10. Cleanup runs the exact-SHA deletion lease from a newly initialized isolated bare repository. Before creating that temporary repository, Shipyard acquires the machine-wide writer-domain lease for the selected temporary root and keeps it until the repository has been removed; this prevents post-merge cleanup from mutating a Sandbox-audited production tree during its exclusive audit window. Lease acquisition or isolated-repository setup failure is explicit, and the remote branch is not deleted.
- **Classic direct-merge refusal:** when governance discovery identifies a classic branch, Shipyard refuses automatic merge before any merge command. `admin=false` cannot prove the authenticated user or App is excluded from every admin, custom-role, ruleset, or App bypass. Use GitHub's native merge queue for automatic merging; a manual maintainer exact-head merge remains outside Shipyard.
- **Private-free rules entitlement:** merge-governance discovery first requires an authoritative `repository.mergeQueue(branch:)` response. When that response is explicitly `null` and the evaluated-rules endpoint returns GitHub's exact `Upgrade to GitHub Pro or make this repository public to enable this feature. (HTTP 403)` plan-entitlement error, Shipyard classifies the branch as classic and applies the refusal above. Generic 401/403 responses, malformed payloads, command failures, and non-null queue authority remain fail-closed.
- **Superseded-SHA preflight (#321).** Before `merge_pr` runs, `execute_auto_merge` reads the live PR head via `fetch_live_head_sha` (snapshot or fresh `gh`/REST; accepts either `headRefOid` or `head.sha`) and compares it with `shas_match` against `state.head_sha` (the SHA Shipyard actually validated). If they differ, it returns `AutoMergeOutcome::SupersededSha { validated, current }` and does **not** merge — `ship_cmd::post_validation::post_run_merge_state` records this as `GreenNotMergedHeadSuperseded { validated, current }` (state file stays active for a re-validated retry, not `STATE_MERGED`). Fail-closed: an unreadable live head does not assume safety. This client-side guard sits in front of the server-side `--match-head-commit` / `-f sha=<oid>` race-guard on the PUT, because GraphQL auto-merge can otherwise land a commit pushed *after* validation completed (the regression that merged pulp #3128 at a pre-fix SHA).
- **Malformed-GraphQL-query classification of a `MergeFailed`.** Before any PR-inspecting classification, `classify_merge_failure` runs `is_graphql_malformed_query_error` over the error. GitHub rejects an invalid GraphQL *document* with prose on `gh`'s stderr and no machine-readable code, so the phrases in `GRAPHQL_MALFORMED_QUERY_SIGNATURES` are the only signal. A match records `GreenNotMergedClientDefect` (still not `STATE_MERGED`; state stays active), which renders a hand-back naming Shipyard as the fault rather than branch protection, and is the one non-merged state with a distinct nonzero exit code (`SHIP_EXIT_MERGE_CLIENT_DEFECT` = 8). Fail-closed: unrecognized stderr stays plain `GreenNotMerged`, so a genuine block never loses its branch-protection guidance. Diagnostic only — it changes the hand-back, exit code, and `status`/`merge_error` JSON fields, never the merge action. The known instance: the merge-queue poll query selected `autoMergeRequest{id}`, which GitHub's schema does not expose, so queue *admission* (T15) failed before any mutation on every queue-governed repo.
- **Post-validation readiness never rewrites validation proof.** After the queued validation job has passed, `InFlight` and `TargetFailed` observations from the separate merge-readiness phase return `GreenPendingMergeReadiness` with exit `0`. The completed target results and evidence remain passed; deterministic stewardship owns the readiness wait, and the hand-back directs callers to `shipyard wait pr <N> --state green` instead of rerunning validation. `PrNotFound` is different: it means the durable scoped ship state is missing, so it returns `GreenValidationStateMissing` with exit `9`. That operational failure still preserves the validation proof, explicitly forbids an automatic rerun, and requires state recovery before stewardship can own readiness. Daemon-owned queue outcomes persist this separately as a typed `post_validation` disposition; `load_ship_outcome` exposes it through `ShipExecutionOutcome`, and JSON rendering reports it without changing the completed job or its evidence.
- **Every non-merged terminal render reports `status` + `merge_error`.** `merged:false` alone cannot distinguish a failed validation from a passed validation whose merge call broke, so the `--json` envelope carries a stable `status` tag (`validation_failed`, `green_not_merged`, `green_pending_merge_readiness`, `green_validation_state_missing`, `green_not_merged_flaky_required`, `green_not_merged_head_superseded`, `green_not_merged_client_defect`, `green_automatic_merge_refused`, `merged`) plus the reason verbatim. `validation_failed` stays exit `1`; ordinary green-but-unmerged/readiness states stay `0`; malformed merge-client requests use exit `8`; missing durable validation state uses exit `9`; and deterministic automatic-merge policy refusal uses exit `10`.
- **Archive failure remains a store error.** If `archive(pr)` itself fails after GitHub merge succeeds, the active state file remains and the command exits nonzero. A later retry can still recover if `gh pr merge` reports "already merged" or PR-state lookup confirms `MERGED`.
- **Flaky-required-leg classification of a `MergeFailed` (`auto_rescue`).** When `execute_auto_merge` returns `AutoMergeOutcome::MergeFailed`, `ship_cmd::post_validation::classify_merge_failure` inspects whether the block is a *flaky required leg*: it fetches the live `headRefOid` + `statusCheckRollup` in one `gh pr view`, and only if the head still matches `state.head_sha` runs the pure `auto_rescue::classify_wedge`. If a required check is RED and **every** red required check maps (exactly, or via `[targets.<t>].required_check_context`) to a target Shipyard validated green, `post_run_merge_state` records `GreenNotMergedFlakyRequired` (still not `STATE_MERGED`; state stays active) and the hand-back prints the `shipyard rescue <pr> --rerun-failed` recovery. Fail-closed to plain `GreenNotMerged` on any ambiguity — a red/pending check with absent `isRequired`, an unmapped red required check, an unreadable ship-state, a failed rollup fetch, or a head that advanced past the validated SHA. Runs only after the malformed-query check below, since a malformed document says nothing about mergeability. This is diagnostic only: it changes the hand-back text and adds a `flaky_required_recovery` JSON field, never the merge action.

### T6 — Refuse to merge on FAIL

- **From:** `STATE_VERDICT_FAIL`
- **To:** `STATE_MERGE_REFUSED` (test vocabulary; the state file is unchanged)
- **Trigger:** `shipyard ship` or `shipyard auto-merge <pr>`
- **Writes:** none — the file is retained for inspection. Aged out by T12.
- **Externals:** none

### T7 — Resume an interrupted ship

- **From:** state file exists + no drift
- **To:** `STATE_IN_FLIGHT` — but note that **every lane is revalidated**, even ones with existing `"pass"` evidence.
- **Trigger:** `shipyard ship` (auto-resume when state exists) or `shipyard ship --resume`
- **Writes:** refreshes `pr_url` / `pr_title` / `commit_subject`; then runs `_execute_job` which iterates **every** `job.target_names` at cli.py:4219 regardless of the existing `evidence_snapshot`.
- **Externals:** `git rev-parse HEAD` (drift check) — the check only runs after `ship` has already confirmed branch/SHA exist (cli.py:2582); a missing HEAD aborts before drift detection, not after.
- **Failure modes**
  - SHA drift (`is_sha_drift`): ship refuses to resume. *Recovery: `--no-resume`, or `--adopt-head` (#346) to adopt the current head when you amended/force-pushed the tip (e.g. added a required trailer). `--adopt-head` updates `head_sha` to the live SHA and **clears `dispatched_runs` + `evidence_snapshot`** so the new head re-validates from scratch — it never preserves evidence across a possibly-different tree, and the policy-signature guard below still applies. The Rust path implements this in `load_or_create_state` (`ship.rs`), gated by the flag plumbed through `ShipExecutionRequest`/`QueuedShipRequest`.*
  - Policy drift: required-platforms / target-list / mode changed. *Recovery: same.*
  - State file is corrupt → `ShipStateStore.get` catches `JSONDecodeError`/`KeyError`/`ValueError` and returns None; the caller creates a fresh state and overwrites the corrupt file.
- **Observation for Phase B:** resume does NOT skip a lane that already passed. A Phase B test that asserts lane-skip-on-resume would be asserting behavior that doesn't exist today. That may itself be a bug (double-work on resume) — if so, file it as a Phase B-adjacent issue rather than codifying the wrong expectation.

### T8 — Force-restart via `--no-resume`

- **From:** any existing state for `<pr>` (FRESH / IN_FLIGHT / VERDICT_*)
- **To:** prior state archived; new `STATE_FRESH` created with `attempt=1` (see bug below)
- **Trigger:** `shipyard ship --no-resume`
- **Writes:**
  1. `ship_state_store.archive_and_replace(existing_state)` at cli.py:2644. The call **archives the prior state and returns a new `ShipState` with `attempt+1`** — but the caller discards the return value.
  2. The CLI then sets `existing_state = None` and falls through to cli.py:2663 where a fresh `ShipState(...)` is constructed with no `attempt=` kwarg, defaulting to `attempt=1`.
- **⚠ Known bug — Bug B2.** Every `--no-resume` resets the attempt counter. Phase B test: assert `attempt` is `N+1` after N `--no-resume` invocations; today it stays at 1.
- **Failure modes**
  - `archive` succeeds but the subsequent `save(fresh_state)` at cli.py:2675 fails → the prior attempt is archived and no active state file exists for the PR, effectively the "no state" branch. *Recovery: a fresh `shipyard ship` creates a new state.*
  - `archive` fails (disk) → the prior state file remains active; no new attempt started.

### T9 — `cloud retarget` mid-flight

- **From:** `STATE_IN_FLIGHT`
- **To:** `STATE_IN_FLIGHT` with the existing target's `DispatchedRun`
  replaced after successful cancellation + redispatch.
- **Trigger:** `shipyard cloud retarget --pr <n> --target <lane> --provider <prov> --apply`
- **Writes:** Cancels matching live job(s) through the GitHub Actions job-cancel
  endpoint. If every active job in the run matches the target, Shipyard may
  safely fall back to cancelling the whole run. After cancellation is proven,
  it dispatches the new workflow and saves the updated `ShipState` with the
  target row replaced.
- **Bug B3 fixed.** Retarget no longer leaves stale `DispatchedRun` rows after
  a successful dispatch; the saved row carries the new provider and run id.
- **Failure modes**
  - Cancel partial success: retarget aborts **before** dispatch, reports
    `event=cancel_failed`, includes any `cancelled_job_ids`, and leaves
    `stale_old_blocker_status="unknown_cancel_failed"`.
  - Cancel total failure: retarget aborts **before** dispatch and classifies the
    failure (`auth`, `scope`, `not_found`, `unsupported`, `transient`,
    `unknown`) with manual recovery steps.
  - Whole-run fallback succeeds: retarget proceeds to dispatch and reports
    `run_cancel_fallback_used=true`; `stale_old_blocker_status="cleared"`.
  - Dispatch failure after cancel success: old job/run is cancelled, but no new
    lane is persisted; retry after inspecting GitHub Actions state.

### T10 — `cloud add-lane` mid-flight

- **From:** `STATE_IN_FLIGHT` (refuses if `_ship_terminal_verdict` is not None)
- **To:** `STATE_IN_FLIGHT` with one more `DispatchedRun` appended
- **Trigger:** `shipyard cloud add-lane --pr <n> --target <name> --apply`
- **Writes:** `workflow_dispatch`, then `append_run(DispatchedRun(..., run_id=real or f"pending-{target}"))` → `save()`. Does not refresh `pr_url` / `pr_title` / `commit_subject`.
- **Externals:** `gh api`, `gh run list`, `workflow_dispatch`
- **Failure modes**
  - `workflow_dispatch` fails → exits 1 before any `append_run`. No state change. *Recovery: retry.*
  - `find_dispatched_run` times out → `DispatchedRun` saved with sentinel `run_id="pending-<target>"` (see T2 — no backfill exists).

### T11 — Terminal archive

- **From:** `STATE_MERGED` (from T5) or **any active state** (via `shipyard ship-state discard`)
- **To:** `STATE_ARCHIVED`
- **Trigger:** `ship` end-of-flow merge-success branch; `auto-merge` merge-success branch; `ship-state discard` (works on any active state, regardless of verdict)
- **Writes:** `os.replace(<pr>.json, archive/<pr>-<timestamp>.json)` at core/ship_state.py:377. The rename is atomic inside the same filesystem store path — the source lives at `self.path / f"{pr}.json"` and the destination at `self._archive_dir / f"{pr}-<ts>.json"` (core/ship_state.py:376). Same filesystem, different subdirectory.
- **Externals:** filesystem atomic rename
- **Failure modes:** rename fails (permission, disk) → the active state file remains. Next `shipyard ship` / `shipyard auto-merge` tick recomputes the verdict — which means merge is re-attempted and hits `GhError` (see T5's archive-failure-is-not-auto-recoverable note).

### T12 — Aging prune

The default cleanup scope also manages queue-job logs independently of
`ShipState`. When a ship job reaches a structurally complete terminal state,
the queue path durably writes `logs/<job-id>/.retention.json` before queue
trimming can remove its pass/failure classification. Malformed or
timestamp-less completions remain failure/unclassified evidence. Default
cleanup retains active writers and audit pins, compresses closed logs, and
pressure-deletes only manifest-proven successful terminal directories; the
`--ship-state` flag adds the state-file pruning below. See
[`log-retention.md`](log-retention.md) for the bounded Phase 1 policy and its
continuously-active-writer Phase 2 boundary.

- **From:** any old active state (gated by the PR being closed) or any old archive
- **To:** deleted
- **Trigger:** `shipyard cleanup --ship-state --apply`
- **Rules (per `ShipStateStore.prune` at core/ship_state.py:399–446)**
  - Active state is deleted only if the PR is in the supplied `closed_prs` set AND `updated_at` is older than `active_days` (default 14). Without a `closed_prs` set, active files are never deleted.
  - Archived files are deleted when mtime is older than `archive_days` (default 30).
- **Externals:** `gh pr list --state closed` (the caller feeds the `closed_prs` set; `prune` itself doesn't call gh)
- **Failure modes:** `Path.unlink` is unguarded (both `delete` at core/ship_state.py:423 and the direct `archive_path.unlink()` at core/ship_state.py:431). A permission or I/O error interrupts the prune mid-sweep; earlier deletions remain applied, later ones are skipped. Phase B test: inject an `OSError` on the second active deletion; assert the `PruneReport` is accurate for the files that were actually removed.

### T13 — Cross-PR evidence reuse (synthesized PASS)

- **From:** `STATE_FRESH` or `STATE_IN_FLIGHT` with a target configured for `reuse_if_paths_unchanged`
- **To:** `STATE_IN_FLIGHT` with an extra passing target row, no dispatch
- **Trigger:** `_maybe_reuse_evidence` inside `_execute_job` (cli.py:4245)
- **Writes:** Returns a synthesized `TargetResult` with `backend="reused"` (cli.py:4510). `_update_ship_state_from_job` mirrors it as `evidence_snapshot[target]="pass"` and a `DispatchedRun` with `status="completed"` and `provider` = the ancestor's provider. `DispatchedRun.status="reused"` is NOT persisted — that string only appears in `watch`/`--json` envelopes as a display label.
- **Externals:** git diff vs ancestor SHA (`shipyard.ship.reuse.check_reuse_eligible`)
- **Failure modes**
  - Ancestor SHA unknown or diff check fails → falls through to normal dispatch. No false-PASS risk from the reuse path itself.
  - Stage-list drift or validation-contract drift → reuse is refused by `reuse.py`; normal dispatch runs.

### T14 — Abandon an orphaned in-flight state (opt-in, daemon)

- **From:** `STATE_IN_FLIGHT` (verdict `None`) whose owning worker is provably dead
- **To:** terminal **abandoned** — `ShipState.abandoned` is set, so
  `ship_terminal_verdict` short-circuits to `Some(false)`
- **Trigger:** the daemon's periodic reconcile pass runs an opt-in abandon sweep
  (`src/ship_resume.rs::sweep_orphaned_ship_states`), gated by
  `[ship_state] auto_resume` (default **off**). No-op — and opens no queue — when
  disabled.
- **Quantification (deliberately conservative — a *false* abandon of a live ship
  is the one catastrophic error):** abandons **only** on `queue_stale` evidence
  (a matching running job whose heartbeat is dead past the reaper's ~180s window —
  a provably dead worker). `queue_terminal` / `queue_absent` / `time_fallback`
  stay report-only (T-diagnostic below): a terminal-but-unfinalized job may be a
  success mid-write, and the weaker signals are inferences. Fail-**closed**: an
  unavailable/absent queue never abandons.
- **Writes:** the sweep snapshot only *selects candidates*; the destructive
  decision is re-made per PR under the per-PR lock. There it re-checks the state
  is still in flight (`ship_terminal_verdict` still `None`) and re-classifies it
  as a `queue_stale` orphan against a **fresh queue read** (never the sweep-wide
  snapshot), then `mark_abandoned(AbandonRecord { reason, evidence,
  stalled_minutes, job_id, abandoned_at })`. Emits a `ship_state_abandoned`
  daemon IPC event.
- **Effect:** the wait/auto-merge path sees a terminal failure and stops blocking;
  the state is **never merged**. Recovery stays operator-driven — a human
  re-ships (`shipyard ship <pr>`), which clears the `abandoned` marker (both the
  reuse and archive-and-replace paths) so the re-validated PR is no longer
  short-circuited to failure. The sweep does **not** auto-re-dispatch, so there
  is no resume→die→resume loop.
- **Idempotent:** an abandoned state is terminal, so the next sweep's
  `classify_orphan` returns `None` — it is never re-abandoned.
- **Failure modes**
  - A verdict lands between candidate selection and the per-PR lock → the
    under-lock `ship_terminal_verdict` re-check skips it (counted as `raced`).
  - A re-ship's worker starts (or a job resumes) during the sweep → the
    under-lock **fresh** queue read sees the owner live, so the live re-ship is
    never abandoned (counted as `raced`).
  - Config load fails in the daemon worker → the sweep no-ops for that pass.

### T15 — Native merge-queue handoff

- **From:** terminal passing ship-state for an OPEN PR on a base branch whose
  live merge-queue object or evaluated repository rules require `merge_queue`
- **To:** active ship-state plus a GitHub-native merge-queue entry; ultimately
  archived ship-state only after GitHub reports the PR merged
- **Trigger:** `shipyard ship` or the one-shot `shipyard auto-merge <pr>`
- **Stack boundary:** at each merge or enqueue mutation boundary, Shipyard
  queries the protected base's top-level `stacked_pr_mode` together with formal
  `PullRequest.headRefOid`, `stack`, and `stackEntry` metadata. Accepted values
  are `off`, `observe`, and the reserved `apply`; missing means `off`. Detection
  runs in every mode. `off` preserves the existing refusal. `observe` refuses
  the same mutation and emits deterministic `stacked-pr-plan=<json>` telemetry
  bound to the full head SHA plus repository, PR, stack number/size/position,
  and stack base. Its receipt explicitly records `github_mutation=false` and
  `required_checks_suppressed=false`; it is not merge evidence. `apply` is
  structurally rejected as `apply_unavailable` (NO-GO) until the asynchronous
  request UUID/lifecycle is durably modeled. A trusted machine-global top-level
  `stacked_pr_mode = "off"` forces the conservative behavior; other global
  values are invalid because fleet policy may not broaden repository policy.
  Invalid policy, incomplete metadata, or a stack observation whose head does
  not match the exact validated head fails before mutation. Unstacked PRs retain
  T15 unchanged in every mode. If classic-boundary inspection exhausts GraphQL,
  a read-only REST fallback may still classify identity, but it cannot authorize
  a merge. Observe-only pilots validate each layer and merge with
  `gh stack merge <pr> --merge`; T15 remains the unstacked merge-queue state
  machine until that lifecycle is modeled.
- **Externals:** the configured `GhClient` reads the live branch merge-queue
  object plus evaluated rules, then performs sparse GraphQL queue/PR polls and
  calls `enqueuePullRequest(expectedHeadOid: <validated-sha>)`.
- **Rules:** classic branches refuse automatic merge. Queue branches never use
  the REST direct-merge fallback. The exact live head must equal the
  validated SHA atomically on GitHub. `auto-merge` returns exit 3 while queued
  and keeps state active; `ship` supervises until merge or a terminal queue
  outcome.
- **Eviction safety:** absence is actionable only after the PR was observed in
  the queue and the enqueue settle window elapsed. Re-enqueue is allowed only
  for `invalid_merge_commit`. `failed_checks`, manual/unknown removal, a
  never-observed arm, malformed/truncated authority data, and head drift are
  terminal without mutation. Observed membership and attempt timestamps are
  durable across process restarts. HTTP 403/rate-limit responses are never
  retried.
- **Idempotency:** a later one-shot first polls the queue. An already queued PR
  is not armed again; a terminal removal newer than the current ship-state is
  not rearmed. A new validated head creates newer ship-state and may be armed.
- **Revocation authority:** an active ship-state owns native auto-merge and
  queue authority only while its validated `head_sha` still equals the live PR
  head. A same-head base retarget may revoke that authority through the audited
  mutation path. A stale state must not disable or dequeue the newer head;
  pending required, advisory, or self-hosted checks do not create revocation
  authority, regardless of their age.
- **Closure authority:** an open PR may be described as already integrated only
  from a `current-base...PR-head` GitHub comparison. `behind` or `identical`
  with `ahead_by == 0` proves commit containment. `ahead` and `diverged` do not;
  they require exact changed-path blob containment before closure. The ghapp
  close guard fails closed on missing, contradictory, or truncated evidence and
  requires `GHAPP_ALLOW_UNINTEGRATED_PR_CLOSE=1` for an explicit abandonment or
  temporary sequence lock. Reversing the compare operands reverses the meaning
  of `ahead_by` and is never valid closure evidence.
- **Fleet authority:** `[merge_queue].mutation_machine` is required in the
  trusted machine-global `config.toml` reported by `shipyard paths`; tracked
  project and checkout-local overlays are ignored. The machine-global runner
  tag must match before any GraphQL mutation is started.
  Queue writes are serialized with a machine-global lock shared by hold/resume,
  so a successful hold waits out any admitted writer and no later writer can
  pass the sentinel. Resume removes only the hold, not the authority policy.
- **Provenance:** every mutation writes `started` and `finished` JSONL records
  under `merge_queue/mutations.jsonl`, including correlation id, machine tag,
  PID, repo, base, PR, validated head, action, and outcome. A normal unwind or
  ambiguous transport result records `outcome=uncertain`; after hard
  termination, `merge-queue status` classifies an unmatched `started` row as
  uncertain. It is never silently converted into permission to retry.

## Diagnostic: orphan reporting (no transition)

`STATE_IN_FLIGHT` has no self-healing exit when the owning process dies
mid-validation (host reboot from a jetsam kill, daemon crash, `cmux` relaunch):
nothing advances the state, `ship_terminal_verdict` stays `None`, and auto-merge
reports `InFlight` forever without merging. The queue's killed-worker reaper
(#351) recovers the sibling `queue.json` `Job`, but the ship-state store has no
equivalent lifecycle.

`shipyard ship-state list` and `shipyard status` surface these as a **read-only**
diagnostic (no state transition, no write; `src/ship_liveness.rs`). A state is
reported orphaned when `ship_terminal_verdict` is `None` (in flight — the exact
predicate the auto-merge gate uses) **and** a single queue snapshot confirms — or
cannot disprove — a dead worker. The signal is source-labeled, strongest to
weakest: `queue_stale` (a matching running job whose heartbeat is dead past the
reaper's 180s window — flagged immediately), `queue_terminal` (a matching job
already terminal while the ship-state never finalized — immediate), `queue_absent`
(queue consulted, no matching job — the ship-state has no job id, so this is
time-gated), and `time_fallback` (queue unavailable — pure `updated_at`
staleness, time-gated). A live-running (fresh heartbeat) or pending job is never
flagged. The time threshold gates only the weak signals, defaults to 45 minutes,
and is configurable via `[ship_state] orphan_stale_minutes`. Human output adds an
`ORPHANED? [<evidence>]:` line; JSON adds `orphaned: [{pr, stalled_minutes,
evidence}]` (`ship-state list`) / `orphaned_ship_states` (`status`). It cannot
affect merge readiness — a flagged state is in flight, which auto-merge already
refuses. Recovery stays operator-driven (`shipyard ship <pr>` to re-validate, or
`ship-state discard`) unless the opt-in daemon abandon sweep is enabled — see T14,
which acts on the strongest (`queue_stale`) evidence this diagnostic surfaces. The
`QueueMatch` the classifier returns carries the owning `Job` so the sweep records
the dead worker's id.

Exact `queue_absent` recovery is a separate, machine-global opt-in. It is off by
default and requires both the kill switch and an explicit checkout registry:

```toml
[ship_state]
queue_absent_recovery = true

[ship_state.repo_paths]
"Generous-Corp/pulp" = "/Volumes/Workshop/Code/pulp"
```

Age is never recovery authority. Under the repository-scoped PR lock the daemon
requires the exact preserved daemon work envelope whose queue job ID is stored
on the current ship attempt. Repo/PR/SHA equality alone is insufficient because
a fresh attempt may deliberately reuse all three. The registered path identifies
the repository root; a preserved canonical submission cwd may be a proven
subdirectory beneath it. Recovery validates checkout/tree/config provenance
from that cwd, confirms the live PR is still OPEN at the stored head branch,
head SHA, and base branch, and proves that no pending/running queue owner, newer
owner, or worker receipt exists. It persists a fresh fenced generation before
an idempotent queue insert, so a crash after either commit resumes the same
generation. Legacy state without an owning queue job keeps its envelope during
retention but cannot replay it automatically. Missing auth, configuration,
checkout, provenance, ownership identity, or GitHub evidence fails closed and
emits a durable `needs_agent` receipt; absence never abandons or merges the ship
state.

## Separate recovery-worker lifecycle (not `ShipState`)

The merge steward's semantic exception worker deliberately does not add a
transition to the `ShipState` graph. Its atomically persisted records use a
separate lifecycle:

```text
pending -> running -> escalated
                   -> failed
                   -> triaged     (reserved for a future diagnostics-enabled phase)
pending/running -> superseded  (a newer exact head replaces stale work)
```

The durable identity binds repository, PR, target base, exact head,
deterministic failure fingerprint, and steward policy signature. The receipt
additionally binds the trusted machine-global worker-config signature and
allows exactly one attempt per head in phase 1. The default command only
inspects and revalidates one pending record's live base, head, and complete
failed-required-check set or recorded merge state. Each request carries the
complete structured required-check policy, so a newly failed required check
supersedes same-head work without treating advisory failures as required.
`--apply` may launch the configured read-only
triager and persist its strict JSON result; `--drain --apply` processes at most
the bounded initial snapshot, never an open-ended poll loop.
Pre-claim repository or GitHub failures preserve the unused attempt and write
a bounded deferral timestamp, rotating that request behind untouched pending
work instead of permanently blocking later repositories.
Before the steward publishes a request, its merge-queue, PR, and required-check
revalidation shares one 20-second absolute deadline with bounded output
capture. The deadline starts before the final publication lease is acquired,
so a stalled GitHub process cannot retain that lease indefinitely.

This lifecycle is advisory and cannot advance, merge, discard, or otherwise
mutate `ShipState`. It also cannot write GitHub statuses/labels, rerun checks,
change native queue state, push commits, sign, publish, or release. Timeout,
invalid output, policy/config drift, and quota/provider exhaustion terminalize
as typed failure evidence; valid classifications always terminalize as escalation so
other repositories and PRs continue through deterministic stewardship.

## Canonical work-ledger shadow (no authority yet)

`shipyard work-ledger import` scans the legacy `ShipState`, queue request and
outcome, recovery, terminal-handoff, and resume-record lifecycles into one
selected, redacted projection. The default is a deterministic dry run and does
not create storage. `--apply` writes the projection idempotently to the
machine-global `work-ledger/work-items.sqlite3`; it does not edit or delete a
legacy record, schedule work, dispatch a wake, call a model, mutate GitHub, or
project to Linear.

Every imported record starts in the closed `shadow_imported` lifecycle state;
legacy status text is evidence, not native authority. Promotion requires one
structurally complete continuation contract containing both success and failure
outcomes. Native transitions use a closed legal graph, fence work and owner
generations, and insert a deterministic audit event in the same transaction as
the state change and optional outbox wake.

The schema deliberately keeps logical goal, owner, terminal runtime,
agent/session adapter, and provider-routing adapter identities separate. PR,
product-acceptance, and continuation terminal truth are separate columns;
legacy lifecycle completion is recorded as `unknown` rather than promoted to a
stronger terminal claim. Terminal, agent, and provider adapter kinds are strings
so cmux/HerdR, Codex/Claude/agy/Qwen/Kimi, and Subrouter additions do not require
a lifecycle redesign. Private owner, goal, source, and route values are stored as
opaque SHA-256 references; raw prompts, terminal text, credentials, tokens,
provider accounts, and route identifiers are not imported. Native repair routes
use a separate integrity-bound protected registry. cmux and HerdR terminal
runtime provenance is independent of Codex/Claude/named agent sessions and
explicit Direct/Subrouter/CLIProxyAPI provider routing. Every agent/session
route, including Codex, Claude, Qwen, agy, Kimi, and future named agents, binds
an independently registered agent-adapter object. Versioned registered terminal
and provider variants allow future adapters without changing lifecycle truth or
the database schema. Route registration and wake resolution remain
nondispatchable unless every referenced adapter record is active and exactly
matches axis, name, generation, revision, and
implementation/configuration/capability digests.
Automatic work-ledger v1-to-v2 migration is limited to route-free ledgers.
Existing v1 route payloads do not contain the mandatory exact agent-adapter
binding, so a route-bearing v1 ledger is preserved unchanged and refused until
its routes receive explicit reconciliation; the migration never invents that
provenance.
The registry preserves
the native resume identity, HerdR session/workspace/tab/pane tuple, account and
model references, one canonical wrapper reference, protected session-header
reference plus digest, executable/configuration
digests, generations, revisions, and exact head. Missing, stale, malformed, or
integrity-mismatched provenance is nondispatchable and is never treated as
Direct or silently converted to a fresh-agent route.

The database opens fail-closed with `WAL`, `synchronous=FULL`, foreign keys,
integrity checking, protected filesystem permissions, and the host-global
writer-domain fence. Apply upgrades that fence to an exclusive bounded snapshot
barrier from legacy scan through SQLite commit. Schema versions newer than the binary, malformed legacy
JSON, symlinked legacy sources, corruption, truncation, and failed writes abort
the whole operation. The future transition API fences work and owner
generations and commits a state change, deterministic audit event, and optional
wake intent in one SQLite transaction. A rejected wake or event therefore rolls
back the state transition.
Wake identity is derived from the work ID, transitioned work generation, owner
generation, protected route reference, and payload digest; callers cannot
substitute an arbitrary retry identity without failing the transaction.
Schema v3 adds a durable attempt record for each wake claim; schema v4 adds
append-only consumer ownership epochs. The inert consumer holds one host-local
exclusive lease across profile resolution, claim, provider invocation, and
finalization, so another live consumer cannot masquerade as restart recovery.
The claim binds the route's protected launch-profile reference and provider
identity before invoking a provider and finalizes acknowledgement, retry,
failure, or uncertainty afterward. It passes the exact launch-profile argv
array without shell translation, reconciles only an explicitly idempotent
claimed delivery after restart once the previous consumer lease is gone, and
never retries an ambiguous non-idempotent delivery. Successful acknowledgement
and transition to agent-owned repair are one transaction. This contract remains
inaccessible to the CLI and daemon.
`activation_enabled=false` and `dispatch_enabled=false` are invariant CLI
outputs until later scheduler, adapter, and physical canary gates land.
The adjacent `work-ledger policy` surface stores per-repository primary-platform
and compatibility-blocking policy behind an exact revision fence. It requires
every repository's primary platform explicitly (Pulp, Forge, and
Vellum use macOS), defaults to independent compatibility lanes, and records the
complete compatibility-lane inventory and the subset with declared artifact
dependencies. Unknown lanes fail closed. Other cross-lane
blocking requires evidenced shared-integrity fault. The read-only shadow
observer consumes policy only for explicit repository enrollment and attaches
the exact revision to evidence; policy cannot activate, dispatch, or make a
blocking decision in this phase.

## External dependency matrix

| External                               | Transitions         | Failure class               | Symptom + audit note                                                                                                 |
|----------------------------------------|---------------------|------------------------------|----------------------------------------------------------------------------------------------------------------------|
| `ShipStateStore.save` / `archive`      | T1, T2, T3, T5, T7, T8, T10, T11 | disk full / permission / race | Uses tmp+`os.replace` (core/ship_state.py:342–357). Torn writes prevented. Orphan tmp cleaned on exception path.     |
| `EvidenceStore.record`                 | T3 (via `_record_evidence` at cli.py:4343) | disk full / race | **Does NOT use tmp+replace** — `core/evidence.py:226` writes directly. Phase B: inject disk-full; assert behavior (crash vs. half-written row). Tracked separately from #102. |
| `queue.json` writes                    | T2, T3              | disk full / kill mid-write   | On `main` today, `Queue._save` writes `queue.json` directly (core/queue.py:119). Fixed in PR #105 (`fix/102-atomic-queue-writes`). Phase B should run against main OR the fix/102 branch depending on test timing. |
| `git push`                             | T1                  | auth / network               | Return code is ignored. State can be saved for a branch whose tip isn't pushed — drift check on next resume catches it, but the first run proceeds. |
| `gh pr create` / `gh pr list`          | T1                  | auth / network / rate-limit  | GraphQL rate-limit errors fall back to REST `gh api repos/<owner>/<repo>/pulls`; only auth/network errors or REST fallback failures abort before T1 save. |
| `gh pr view` (idempotency for `auto-merge`) | T5 (no-state branch only) | auth / network | On failure the command falls through to `pr-not-found`. Not reached when the state file is present.                |
| `workflow_dispatch`                    | T2, T9, T10         | 404 / 5xx / rate-limit       | Add-lane: exits before mutation. Retarget: if dispatch fails after cancel succeeded, the old lane is gone and no new lane exists. `ship` path goes through `CloudExecutor` inside `_execute_job`. |
| `find_dispatched_run`                  | T2, T10             | timeout                      | DispatchedRun persisted with `pending-<target>` sentinel. **No backfill path exists.**                              |
| GitHub Actions cancel (retarget)       | T9                  | race / auth / scope / unsupported / not-found | If cancellation is not proven complete, retarget aborts before dispatch with `event=cancel_failed`. Partial cancellation no longer dispatches additively. Whole-run fallback is used only when every active job matches the target. |
| automatic merge governance             | T5                  | native queue unavailable / mutation-identity bypass cannot be disproved / synthetic hook requested in production | Queue-governed branches retain native admission. Classic branches and production uses of hidden synthetic merge hooks return typed `automatic-merge-refused` / `green_automatic_merge_refused`, exit 10, retain state, and perform no merge mutation. REST remains read-only identity fallback only; use a native queue or manual maintainer exact-head merge. |
| SSH backend probe                      | T2 (preflight)      | network / auth / host_key     | Pre-#100: silent hang. Post-#100: exit 3 with classified error inside 10s.                                           |
| `git rev-parse HEAD` (branch/SHA)      | T1, T7              | worktree gone                 | `ship` aborts at cli.py:2582 before drift detection if branch or SHA is unavailable.                                 |

## Bugs discovered by this audit

The Codex review pass turned up four real bugs, independent of the doc's
accuracy. All four have been fixed — regression tests live in
`tests/test_ship_state_machine.py` and run on the dedicated
`state-machine` CI lane.

| ID | Issue | Summary | Status |
|----|-------|---------|--------|
| B1 | [#108](https://github.com/danielraffel/Shipyard/issues/108) | `_ship_terminal_verdict` required coverage check — partial evidence with all-pass values no longer declares a premature verdict. | **Fixed** — `cli.py:_ship_terminal_verdict`. Test: `TestB1_PartialEvidenceCoverage`. |
| B2 | [#109](https://github.com/danielraffel/Shipyard/issues/109) | `--no-resume` now carries forward the attempt counter from `archive_and_replace` instead of resetting to 1. | **Fixed** — `cli.py` ship command `carried_attempt` thread. Test: `TestB2_NoResumeAttemptCounter`. |
| B3 | [#110](https://github.com/danielraffel/Shipyard/issues/110) | `cloud retarget --apply` now replaces the target's `DispatchedRun` row after a successful dispatch. | **Fixed** — `cli.py` `cloud_retarget`. Test: `TestB3_RetargetUpdatesState`. |
| B4 | [#111](https://github.com/danielraffel/Shipyard/issues/111) | `_cloud_runs_by_platform` filters by `requested_ref == sha`. | **Fixed** — `cli.py:_cloud_runs_by_platform`. Test: `TestB4_CloudRunsByPlatformScopesToSha`. |

The `xfail(strict=True)` markers used during Phase B have been flipped
to plain assertions. Any regression that reverts one of these fixes
will fail the `state-machine` CI lane immediately.

## Silent-failure regression tests

1. **Merge success hidden behind cleanup failure.** If `gh pr merge --delete-branch` merges on GitHub but fails while deleting a local branch checked out in another worktree, `auto-merge` must archive ship-state and exit 0 with a cleanup warning, not report `merge-failed`. Covered by `auto_merge_archives_when_merge_error_reports_already_merged`.
2. **Partial evidence PASS (Bug B1).** Dispatch 3 targets, write evidence for only 1 (all "pass"), compute verdict. Today returns True. *Test:* seed a `ShipState` with `dispatched_runs=[T1, T2, T3]` and `evidence_snapshot={"T1":"pass"}`; `_ship_terminal_verdict(state)` must return `None`, not `True`. Proposed fix: extend the function to require an evidence row for every non-advisory `DispatchedRun.target`.
3. **Ship-state tmp-write durability.** `ShipStateStore.save` already uses tmp+`os.replace` (core/ship_state.py:342). *Test:* inject an `os.replace` failure; assert the prior file is byte-identical. (Do NOT couple this test to `queue.json` — `Queue._save` uses a different write pattern on `main`; atomicity for that file lands in PR #105.)

## Phase B test plan

For each transition T1–T13, Phase B should land at least:

1. A happy-path test that writes the expected fields.
2. A failure-injection test for every external-dependency row the
   transition touches, asserting the documented recovery behavior.
3. A `touch()` / `updated_at` assertion: writes must move it forward;
   read-only helpers must not.

Plus the four bug regression tests from the table above, and the three
silent-failure regression tests from the list above. Every test should
name the transition it exercises (`test_T5_merge_on_pass_archives_state`
etc.) so failure output maps directly to this doc.

## Phase C — doc-sync hook + dedicated CI lane

Both landed in a follow-up PR:

1. **Doc-sync hook.** `scripts/doc_sync_check.py` + `scripts/doc_sync_map.json`
   enforce that changes to mapped Rust ship-state or command modules
   include an update to this doc. Runs in
   `.githooks/pre-push` (advisory; `SHIPYARD_ENFORCE_PREPUSH=1`
   upgrades to block) and in `.github/workflows/version-skill-check.yml`
   as a hard CI gate. Bypass via a `Doc-Update: skip doc=<path>
   reason="..."` trailer on any commit in the diff range.
2. **Dedicated state-machine coverage.** Rust unit tests exercise the
   state-machine transitions as part of `cargo test --all-targets --locked`.
   A failure shows up in the PR status list, so an operator can tell a
   state-machine regression from a cross-platform
   infra blip at a glance.

When you touch ship-state transition code, either update this doc in
the same PR, or record why the update is unnecessary on the tip commit
with `Doc-Update: skip doc=docs/ship-state-machine.md reason="..."`.
