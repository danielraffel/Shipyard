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

`ShipState` lives at `<state_dir>/ship/<pr>.json` during the active ship.
The active file is archived to `<state_dir>/ship/archive/<pr>-<utc>.json`
on one of:

- `shipyard ship` success (`merge_pr` returned a merged PR)
- `shipyard auto-merge` success (same)
- `shipyard ship-state discard <pr>` (manual tombstone — works on any
  active state, not only MERGED)

Failed verdicts (`STATE_VERDICT_FAIL`), refused merges, and merge
attempts that hit a GhError all leave the active file in place for
inspection. `shipyard cleanup --ship-state` ages these out (see T12).

`ShipState` carries:

| Field               | Purpose                                                                             |
|---------------------|-------------------------------------------------------------------------------------|
| `pr`                | GitHub PR number — primary key.                                                     |
| `repo`              | Owner/name (`danielraffel/pulp`) captured at dispatch so retarget/add-lane route dispatches correctly. |
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
| `shipyard ship` (fresh)     | `ShipStateStore.get(pr)` (auto-resume decision; returns None) | Saves fresh state BEFORE preflight (cli.py:2675). Calls `_update_ship_state_from_job` once after `_execute_job` ends. `archive(pr)` on MERGED. |
| `shipyard ship --no-resume` | Same                                                | `ShipStateStore.archive_and_replace(state)` archives prior attempt; then a new `ShipState(...)` is constructed with `attempt=1` (see Bug B2). |
| `shipyard ship --resume`    | Refuses on SHA/policy drift via `_detect_ship_state_drift` | Refreshes `pr_url` / `pr_title` / `commit_subject` on the existing state and saves (cli.py:2679–2689). |
| `shipyard cloud add-lane`   | `ShipStateStore.get(pr)`; verdict check; idempotent `has_target` | `append_run` + `save`. Does NOT refresh human-context fields. |
| `shipyard cloud retarget`   | None (the command operates on the live GH Actions run; it does not load `ShipState` at all) | **None** — cancels old job, dispatches new workflow; never writes `ShipState`. See T9 + Bug B3. |
| `shipyard watch`            | `ShipStateStore.get(pr)` loop                       | Never mutates; signature-based change detection emits NDJSON. |
| `shipyard auto-merge`       | `ShipStateStore.get(pr)` + `gh pr view` fallback when state is absent | `archive(pr)` on success; no writes on failure. `_pr_is_merged` only runs on the no-state branch. |
| `shipyard ship-state list`  | `list_active()`                                     | None                                                    |
| `shipyard ship-state show`  | `get(pr)`                                           | None                                                    |
| `shipyard ship-state discard` | `get(pr)` (accepts any state, not only MERGED)    | `archive(pr)` (manual tombstone)                        |
| `shipyard cleanup --ship-state` | `prune(active_days=14, archive_days=30, closed_prs=...)` | Deletes aged-out active (only if PR is in the supplied `closed_prs` set) + archived files. Unlinks are unguarded — a failure raises. |

## Transitions — preconditions, postconditions, failure modes

### T1 — Create a fresh ship state

- **From:** no state file exists for `<pr>`
- **To:** `STATE_FRESH`
- **Trigger:** `shipyard ship` on a branch
- **Writes:** `ShipStateStore.save(ShipState(..., dispatched_runs=[], evidence_snapshot={}))` at cli.py:2675 — **before** preflight runs at cli.py:2679
- **Externals:** `git push -u origin <branch>` at cli.py:2602 (return code ignored — see "External matrix" below), `gh pr list` / `gh pr create` for PR number. The Rust implementation falls back to REST `gh api repos/<owner>/<repo>/pulls` when GitHub GraphQL is rate-limited, so PR creation can still produce a tracked ship-state record.
- **Failure modes**
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
  job so a later local macOS slot can pick it up.
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
- **Externals:** `gh pr merge` (branch protection, auth, network)
- **Failure-handling split:**
  - `shipyard ship` treats merge-command failure as "green but not merged" and leaves the state file active for retry.
  - `shipyard auto-merge` returns `merge-failed` only when the PR is still unmerged. If `gh pr merge --delete-branch` exits nonzero after GitHub has already merged the PR (for example, local branch deletion failed because another worktree has it checked out), Shipyard archives state and exits 0 with a `cleanup_warning`.
- **GraphQL/App-token fallback:** if `gh pr merge` fails because GraphQL is rate-limited or because the App installation token cannot access `mergePullRequest`, Shipyard falls through to `merge_pr_rest` with the same configured `GhClient`. This preserves App-token quota and avoids silently switching to ambient user auth. If GitHub rejects the REST merge endpoint too, the merge remains a normal T5 failure for retry or manual/user-auth merge.
- **Superseded-SHA preflight (#321).** Before `merge_pr` runs, `execute_auto_merge` reads the live PR head via `fetch_live_head_sha` (snapshot or fresh `gh`/REST; accepts `headRefOid` or `head.sha`) and compares it with `shas_match` against `state.head_sha` (the SHA Shipyard actually validated). If they differ, it returns `AutoMergeOutcome::SupersededSha { validated, current }` and does **not** merge — `ship_cmd`'s `post_run_merge_state` records this as `GreenNotMerged` (state file stays active for a re-validated retry, not `STATE_MERGED`). Fail-closed: an unreadable live head does not assume safety. This client-side guard sits in front of the server-side `--match-head-commit` / `-f sha=<oid>` race-guard on the PUT, because GraphQL auto-merge can otherwise land a commit pushed *after* validation completed (the regression that merged pulp #3128 at a pre-fix SHA).
- **Archive failure remains a store error.** If `archive(pr)` itself fails after GitHub merge succeeds, the active state file remains and the command exits nonzero. A later retry can still recover if `gh pr merge` reports "already merged" or PR-state lookup confirms `MERGED`.
- **Flaky-required-leg classification of a `MergeFailed` (`auto_rescue`).** When `execute_auto_merge` returns `AutoMergeOutcome::MergeFailed`, `ship_cmd`'s `classify_merge_failure` inspects whether the block is a *flaky required leg*: it fetches the live `headRefOid` + `statusCheckRollup` in one `gh pr view`, and only if the head still matches `state.head_sha` runs the pure `auto_rescue::classify_wedge`. If a required check is RED and **every** red required check maps (exactly, or via `[targets.<t>].required_check_context`) to a target Shipyard validated green, `post_run_merge_state` records `GreenNotMergedFlakyRequired` (still not `STATE_MERGED`; state stays active) and the hand-back prints the `shipyard rescue <pr> --rerun-failed` recovery. Fail-closed to plain `GreenNotMerged` on any ambiguity — a red/pending check with absent `isRequired`, an unmapped red required check, an unreadable ship-state, a failed rollup fetch, or a head that advanced past the validated SHA. This is diagnostic only: it changes the hand-back text and adds a `flaky_required_recovery` JSON field, never the merge action.

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
  evaluated repository rules contain `merge_queue`
- **To:** active ship-state plus a GitHub-native merge-queue entry; ultimately
  archived ship-state only after GitHub reports the PR merged
- **Trigger:** `shipyard ship` or the one-shot `shipyard auto-merge <pr>`
- **Externals:** the configured `GhClient` reads evaluated branch rules, then
  performs one sparse GraphQL queue/PR poll and arms
  `gh pr merge --merge --auto --match-head-commit <validated-sha>`.
- **Rules:** classic branches retain the direct merge path. Queue branches
  never use the REST direct-merge fallback. The exact live head must equal the
  validated SHA. `auto-merge` returns exit 3 while queued and keeps state
  active; `ship` supervises until merge or a terminal queue outcome.
- **Eviction safety:** absence is actionable only after the PR was observed in
  the queue and the enqueue settle window elapsed. Re-enqueue is allowed only
  for `invalid_merge_commit`. `failed_checks`, manual/unknown removal, a
  never-observed arm, malformed/truncated authority data, and head drift are
  terminal without mutation. HTTP 403/rate-limit responses are never retried.
- **Idempotency:** a later one-shot first polls the queue. An already queued PR
  is not armed again; a terminal removal newer than the current ship-state is
  not rearmed. A new validated head creates newer ship-state and may be armed.

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
| `gh pr merge`                          | T5                  | branch protection / auth / already-merged / cleanup failure / GraphQL rate-limit / App-token GraphQL denial / 405 base-modified | `auto-merge` treats "already merged" or post-error `MERGED` PR state as success with `cleanup_warning`; genuine unmerged failures still return `merge-failed`. GraphQL-rate-limit failures and App-token `mergePullRequest` denial fall through to `merge_pr_rest` (`src/app/auto_merge_cmd.rs`) which issues `PUT /repos/:r/pulls/:n/merge` with `sha=<head_sha>` as a server-side race-guard. On a `405 Base branch was modified` response, `merge_pr_rest` refetches head info and retries exactly once iff `headRefOid` is unchanged (a real new commit on the head refuses the retry and surfaces a `head moved from <abc> to <def>` error). Issue #266 / PR #302. |
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
