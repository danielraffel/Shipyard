# Ship state machine (audit — Phase A)

This document is the [#101](https://github.com/danielraffel/Shipyard/issues/101)
Phase A deliverable: a hand-written map of every state, every transition,
and every external dependency in the `shipyard ship` / `shipyard watch` /
`shipyard auto-merge` flow, written by reading the code end-to-end and
reviewed by a second pass (Codex via RepoPrompt MCP) that cross-checked
each claim against `src/shipyard/**` at exact line numbers.

Keep this doc in step with `src/shipyard/core/ship_state.py`,
`src/shipyard/cli.py` (the `ship`, `watch`, `auto-merge`, `cloud add-lane`,
`cloud retarget`, and `ship-state` subcommands), and `src/shipyard/ship/*.py`.

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
| `provider`           | Dispatch channel: `namespace`, `github-hosted`, `ssh`, `ssh-windows`, or a local job id for the queue path. |
| `run_id`             | GH Actions run ID for cloud, Shipyard job id for local/ssh, or `pending-<target>` when `cloud add-lane` couldn't discover the real run id. **No code backfills this sentinel today** — `watch` is read-only with respect to ship state (cli.py:3497). |
| `status`             | Last observed lifecycle string: `queued`, `in_progress`, `completed`, `failed`, `cancelled`. `reused` is **not** a valid `DispatchedRun.status` — cross-PR evidence reuse synthesizes a `TargetStatus.PASS` with `backend="reused"` (cli.py:4510) and persists it as `status="completed"` (cli.py:4586). |
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
- **Externals:** `git push -u origin <branch>` at cli.py:2602 (return code ignored — see "External matrix" below), `gh pr list` / `gh pr create` for PR number
- **Failure modes**
  - `git push` fails silently → `find_pr_for_branch` may still find an existing PR; the local SHA may not match the remote. A fresh state is saved for a branch whose tip may not be pushed. *Recovery: none automatic — the drift check on the next resume will catch it, but between the stale push and the next resume the state claims a SHA that doesn't exist on the remote.*
  - `gh pr create` fails → `create_pr` raises `GhError`; `ship` exits without saving state because the save at cli.py:2675 runs only after the PR has been found or created. *Recovery: retry.*
  - `save` fails (disk, permission) → `save` raises; tmp file is cleaned up by the `except` branch in `core/ship_state.py`. *Recovery: resolve disk issue, retry.*

### T2 — Dispatch targets within `_execute_job`

- **From:** `STATE_FRESH` or `STATE_IN_FLIGHT`
- **To:** `STATE_IN_FLIGHT`
- **Trigger:** `_execute_job` per-target loop; or `shipyard cloud add-lane --apply`; or `shipyard cloud retarget --apply` (see T9 — retarget does NOT advance ship state)
- **Writes for the `ship` path:** `_execute_job` does NOT save `ShipState` at each target boundary. It only calls `_update_ship_state_from_job` **once** after `job.complete()` (cli.py:4345), which performs one `save()` for the whole batch (cli.py:4595). Within the loop, only the per-job `queue.update(job)` is written.
- **Writes for `cloud add-lane --apply`:** `append_run(DispatchedRun(..., run_id=discovered or f"pending-{target}"))` then `save`.
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
- **Failure-handling split (key asymmetry):**
  - `shipyard ship` at cli.py:2723 calls `merge_pr` with **no local try/except**. A `GhError` propagates up and aborts with a traceback. The state file is left active.
  - `shipyard auto-merge` at cli.py:3333–3347 catches `GhError` + `TypeError` (back-compat) and exits 1 with a `merge-failed` event. The state file is left active.
- **Archive failure is NOT auto-recoverable.** `_pr_is_merged(pr)` is only consulted when `ctx.ship_state.get(pr)` returns `None` (cli.py:3242). If `archive(pr)` fails (disk error, race) and leaves the active state file present, the next `auto-merge` tick reads the state, recomputes `STATE_VERDICT_PASS`, and attempts `merge_pr` again — which returns `GhError` from gh ("Pull request is already merged"). `auto-merge` exits 1 reporting `merge-failed`, not 0. Phase B test: inject an archive failure after a successful merge; assert the next `auto-merge` tick emits a distinct event (not a false `merge-failed`). The fix may be a new "archive or accept merged PR" probe on the state-present branch.

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
  - SHA drift (`is_sha_drift`): ship refuses to resume. *Recovery: `--no-resume`.*
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
- **To:** `STATE_IN_FLIGHT` — but the `ShipState` file is **not updated**.
- **Trigger:** `shipyard cloud retarget --pr <n> --target <lane> --provider <prov> --apply`
- **Writes:** **None.** The apply path cancels the matching live job via `gh run` (cli.py:2101) and dispatches the new workflow via `workflow_dispatch` (cli.py:2116). It never loads, mutates, or saves `ShipState`.
- **⚠ Known bug — Bug B3.** Retarget dispatches a new workflow but leaves the `DispatchedRun` rows unchanged. The watch loop sees the old `DispatchedRun(target, provider=old, run_id=old_id)` alongside a new GH Actions run for the same target but on a different provider. `_update_ship_state_from_job` won't fire for the retargeted lane unless the fresh dispatch flows through `_execute_job` (it does not). Phase B test: retarget + watch + assert that either (a) the `DispatchedRun` is updated in state, or (b) the watch surfaces the drift to the operator. Neither happens today.
- **Failure modes**
  - Cancel partial success: retarget proceeds to dispatch. Stale lane consumes cloud quota but does not block merge.
  - Cancel total failure: retarget aborts at cli.py:2108 **before** dispatching. Old job keeps running; no new lane.
  - Dispatch failure after cancel success: old job is cancelled, no new lane persisted — the target is effectively gone from the in-flight dispatch set.

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

## External dependency matrix

| External                               | Transitions         | Failure class               | Symptom + audit note                                                                                                 |
|----------------------------------------|---------------------|------------------------------|----------------------------------------------------------------------------------------------------------------------|
| `ShipStateStore.save` / `archive`      | T1, T2, T3, T5, T7, T8, T10, T11 | disk full / permission / race | Uses tmp+`os.replace` (core/ship_state.py:342–357). Torn writes prevented. Orphan tmp cleaned on exception path.     |
| `EvidenceStore.record`                 | T3 (via `_record_evidence` at cli.py:4343) | disk full / race | **Does NOT use tmp+replace** — `core/evidence.py:226` writes directly. Phase B: inject disk-full; assert behavior (crash vs. half-written row). Tracked separately from #102. |
| `queue.json` writes                    | T2, T3              | disk full / kill mid-write   | On `main` today, `Queue._save` writes `queue.json` directly (core/queue.py:119). Fixed in PR #105 (`fix/102-atomic-queue-writes`). Phase B should run against main OR the fix/102 branch depending on test timing. |
| `git push`                             | T1                  | auth / network               | Return code is ignored. State can be saved for a branch whose tip isn't pushed — drift check on next resume catches it, but the first run proceeds. |
| `gh pr create` / `gh pr list`          | T1                  | auth / network / rate-limit  | Raises `GhError`; ship aborts before T1 save.                                                                        |
| `gh pr view` (idempotency for `auto-merge`) | T5 (no-state branch only) | auth / network | On failure the command falls through to `pr-not-found`. Not reached when the state file is present.                |
| `workflow_dispatch`                    | T2, T9, T10         | 404 / 5xx / rate-limit       | Add-lane: exits before mutation. Retarget: if dispatch fails after cancel succeeded, the old lane is gone and no new lane exists. `ship` path goes through `CloudExecutor` inside `_execute_job`. |
| `find_dispatched_run`                  | T2, T10             | timeout                      | DispatchedRun persisted with `pending-<target>` sentinel. **No backfill path exists.**                              |
| `gh run cancel` (retarget)             | T9                  | race / auth                  | If cancel fails completely, retarget aborts before dispatch. Partial cancellation proceeds to dispatch → stale old job runs alongside new.                                   |
| `gh pr merge`                          | T5                  | branch protection / auth / already-merged | `ship`: unwrapped, propagates. `auto-merge`: wrapped, exits 1 with `merge-failed`. "Already merged" from a failed-archive retry is currently reported as `merge-failed`, not idempotent success (see T5). |
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

1. **Archive failure after merge is not idempotent on retry.** `_pr_is_merged` fires only when the state file is absent (cli.py:3242). If archive fails and leaves the state active, the next `auto-merge` reattempts `merge_pr`, GitHub returns "already merged" as an error, and `auto-merge` exits 1 with `merge-failed`. *Test:* archive → force an `OSError` on the rename; assert the next `auto-merge` tick distinguishes "already merged on GH" from "merge genuinely failed" and exits 0 with an event like `already-merged-but-state-not-archived`.
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

## Phase C preview

1. **Doc-sync hook.** Add `docs/ship-state-machine.md` to the skill-sync
   path map so that changes to `src/shipyard/core/ship_state.py` or the
   relevant chunks of `src/shipyard/cli.py` must update this doc in the
   same PR. Mechanism identical to `scripts/skill_path_map.json`.
2. **Dedicated state-machine CI lane.** A `pytest -m state_machine`
   marker run as its own GitHub Actions job, so a state-machine
   failure is visually distinct in the PR checks list.

Neither change lands in Phase A. This doc's role is to make Phase B
testable and Phase C's path-map additions obvious.
