# Ship state machine (audit — Phase A)

This document is the [#101](https://github.com/danielraffel/Shipyard/issues/101)
Phase A deliverable: a hand-written map of every state, every transition,
and every external dependency in the `shipyard ship` / `shipyard watch` /
`shipyard auto-merge` flow. It was written by reading the code end-to-end,
not by executing it, and is therefore a fixed-in-time snapshot. Keep it in
step with `src/shipyard/core/ship_state.py`, `src/shipyard/cli.py` (the
`ship`, `watch`, `auto-merge`, `cloud add-lane`, `cloud retarget`, and
`ship-state` subcommands), and `src/shipyard/ship/*.py`.

**Phase B** (transition tests) and **Phase C** (pre-merge doc-sync hook,
dedicated CI lane) land in follow-up PRs. This audit's goal is to make
Phase B mechanical: each edge below becomes a test.

## The core persisted object: `ShipState`

`ShipState` lives at `<state_dir>/ship/<pr>.json` during the active ship.
On terminal verdict it is moved to `<state_dir>/ship/archive/<pr>-<utc>.json`
so the active directory only lists live work. The object carries:

| Field               | Purpose                                                                             |
|---------------------|-------------------------------------------------------------------------------------|
| `pr`                | GitHub PR number — primary key.                                                     |
| `repo`              | Owner/name (`danielraffel/pulp`), captured at dispatch so retarget/add-lane routes dispatches correctly. |
| `branch`            | PR head branch.                                                                     |
| `base_branch`       | Merge target.                                                                       |
| `head_sha`          | PR head SHA at dispatch. Drift vs this value refuses resume.                        |
| `policy_signature`  | SHA-256[:16] of (required_platforms, target_names, mode) at dispatch. Drift refuses resume. |
| `dispatched_runs`   | One `DispatchedRun` per lane (target + provider + GitHub Actions run id / queue job id + live status). |
| `evidence_snapshot` | `{target: "pass" | "fail" | "pending" | ...}` — source-of-truth for the verdict computer. |
| `attempt`           | Monotonic counter bumped by `archive_and_replace` on `--no-resume`.                 |
| `pr_url`, `pr_title`, `commit_subject` | Human context. Refreshed on every save so a force-push is visible to `ship-state show` without a new attempt. |
| `created_at`        | Attempt-scoped: stable for the life of an attempt.                                  |
| `updated_at`        | Last `touch()` — bumped after every mutation helper.                                |
| `schema_version`    | Currently 1.                                                                        |

`DispatchedRun` is the per-lane record:

| Field                | Purpose                                                                            |
|----------------------|------------------------------------------------------------------------------------|
| `target`             | Lane name (`macos`, `ubuntu`, …) — matches `[targets.<name>]` in `.shipyard/config.toml`. |
| `provider`           | Dispatch channel: `namespace`, `github-hosted`, `ssh`, `ssh-windows`, or a local job id for the queue path. |
| `run_id`             | GH Actions run ID for cloud, Shipyard job id for local/ssh, or `pending-<target>` while a `cloud add-lane` discovery is in flight. |
| `status`             | Last observed lifecycle string (`queued`, `in_progress`, `completed`, `failed`, `cancelled`, `reused`). |
| `attempt`            | `ShipState.attempt` at dispatch time. Survives resume so old attempts don't reattach. |
| `last_heartbeat_at`  | Additive liveness signal — written by the poller, used by `watch` to mark `stale` runs. |
| `phase`              | Additive validation-phase tag (setup/configure/build/test), same source as `last_heartbeat_at`. |
| `required`           | Lane policy at dispatch. `False` = advisory; advisory failures land in the merge check's `advisory` bucket instead of `failing`. |

## State diagram (textual)

```
                          ┌─────────────────────────────────────────┐
                          │   No state file exists for this PR      │
                          └───────────────────┬─────────────────────┘
                                              │
                                              ▼  shipyard ship (first run)
                                   ┌──────────────────────┐
                                   │   STATE_FRESH        │
                                   │   (state created,    │
                                   │    no runs yet)      │
                                   └───────────┬──────────┘
                                               │  dispatcher.validate_target(...) / workflow_dispatch
                                               ▼
                                   ┌──────────────────────┐
                                   │   STATE_IN_FLIGHT    │◀────┐
                                   │   evidence_snapshot  │     │ cloud add-lane / retarget
                                   │   has "pending"      │─────┘ (adds or replaces a
                                   │   entries for some   │        DispatchedRun while
                                   │   targets            │        the ship is still live)
                                   └───────────┬──────────┘
                                               │
                       ┌───────────────────────┼───────────────────────┐
                       │                       │                       │
                       ▼                       ▼                       ▼
              ┌────────────────┐      ┌────────────────┐      ┌────────────────┐
              │ STATE_VERDICT  │      │ STATE_VERDICT  │      │ STATE_STALE    │
              │ _PASS          │      │ _FAIL          │      │ (session died; │
              │                │      │                │      │  --no-resume   │
              │ all required   │      │ any required   │      │  or drift      │
              │ targets pass   │      │ target failed  │      │  refuses resume│
              │ (advisory fails│      │                │      └──────┬─────────┘
              │  tolerated)    │      │                │             │
              └──────┬─────────┘      └──────┬─────────┘             │ archive_and_replace
                     │                       │                       │   (bumps attempt)
          shipyard   │                       │                       │
          auto-merge │                       │                       ▼
          or ship    │                       │              ┌────────────────┐
                     │                       │              │ STATE_FRESH    │
                     ▼                       ▼              │  (new attempt) │
              ┌────────────────┐      ┌────────────────┐    └────────────────┘
              │ STATE_MERGE    │      │ STATE_MERGE_   │
              │  _ATTEMPTING   │      │  REFUSED       │
              │                │      │ (target failed;│
              │ merge_pr(...)  │      │  no merge call │
              │                │      │  is issued)    │
              └──────┬─────────┘      └──────┬─────────┘
                     │                       │
          ┌──────────┴──────────┐             │
          ▼                     ▼             │
  ┌────────────────┐  ┌────────────────┐      │
  │ STATE_MERGED   │  │ STATE_MERGE_   │      │
  │                │  │  FAILED        │      │
  │ gh pr merge    │  │ (gh error,     │      │
  │ returned ok    │  │  branch prot,  │      │
  │                │  │  auth, etc.)   │      │
  └──────┬─────────┘  └──────┬─────────┘      │
         │                   │                │
         │ archive()         │ (no archive:   │ (no archive:
         │ (terminal)        │  retry-able,   │  final verdict
         ▼                   │  state lives)  │  retained)
  ┌────────────────┐         │                │
  │ STATE_ARCHIVED │         ▼                ▼
  └────────────────┘   [stays STATE_    [stays STATE_
                       VERDICT_PASS]    VERDICT_FAIL]
```

**Note.** Shipyard does not emit or name these state labels at runtime —
they are derived from the `(evidence_snapshot, dispatched_runs)` pair
and the presence/archival of the state file. The names exist so Phase B
tests can reference each edge unambiguously.

## Entry points and which states they read/write

| CLI command                 | Reads                                               | Writes                                                  |
|-----------------------------|-----------------------------------------------------|---------------------------------------------------------|
| `shipyard ship`             | `ShipStateStore.get(pr)` (auto-resume decision)     | Creates or refreshes state; calls `_update_ship_state_from_job`; `archive(pr)` on `STATE_MERGED` |
| `shipyard ship --no-resume` | Same, forces `archive_and_replace`                  | Archives prior attempt and writes new FRESH attempt     |
| `shipyard ship --resume`    | Refuses on SHA/policy drift via `_detect_ship_state_drift` | Refreshes pr_url / title / commit_subject, touches    |
| `shipyard cloud add-lane`   | `ShipStateStore.get(pr)`; verdict check; idempotent `has_target` | `append_run` + `save`                                   |
| `shipyard cloud retarget`   | Same; dry-run default                               | Replaces one lane's DispatchedRun (target+provider) and the backing cloud workflow run |
| `shipyard watch`            | `ShipStateStore.get(pr)` loop                       | Never mutates; signature-based change detection emits NDJSON |
| `shipyard auto-merge`       | `ShipStateStore.get(pr)` + `gh pr view` fallback    | `archive(pr)` on success; never mutates on failure     |
| `shipyard ship-state list`  | `list_active()`                                     | None                                                    |
| `shipyard ship-state show`  | `get(pr)`                                           | None                                                    |
| `shipyard ship-state discard` | `get(pr)`                                         | `archive(pr)` (manual tombstone)                        |
| `shipyard cleanup --ship-state` | `prune(active_days=14, archive_days=30, closed_prs=...)` | Deletes aged-out active + archived state               |

## Transitions — preconditions, postconditions, failure modes

### T1 — Create a fresh ship state

- **From:** no state file exists for `<pr>`
- **To:** `STATE_FRESH`
- **Trigger:** `shipyard ship` on a branch that either has no open PR or an open PR without a saved state
- **Writes:** `ShipStateStore.save(ShipState(..., dispatched_runs=[], evidence_snapshot={}))`
- **Externals:** `git push`, `gh pr create` / `gh pr list` (for PR number)
- **Failure modes**
  - `git push` fails → ship aborts before state is written (no state file created). *Recovery: retry.*
  - `gh pr create` fails → `create_pr` raises `GhError`; ship exits without writing state. *Recovery: retry.*
  - `state_path.write` fails (disk full, permission) → `ShipStateStore.save` raises; tmp file is cleaned up by the `except` in `core/ship_state.py:save`. *Recovery: resolve disk issue, retry.*

### T2 — Add a dispatched run to an in-flight ship

- **From:** `STATE_FRESH` or `STATE_IN_FLIGHT`
- **To:** `STATE_IN_FLIGHT`
- **Trigger:** `shipyard ship` per-target loop in `_execute_job`; `shipyard cloud add-lane`; `shipyard cloud retarget`
- **Writes:** `ship_state.upsert_run(...)` (ship loop) or `append_run(...)` (add-lane, after `has_target` guard) → `save()`
- **Externals:** `workflow_dispatch` (for cloud), the per-lane executor (`CloudExecutor`, `SSHExecutor`, `LocalExecutor`), `ExecutorDispatcher.probe/diagnose`
- **Failure modes**
  - `workflow_dispatch` raises (404, auth, transient GH 5xx) → CLI exits 1 before saving the DispatchedRun. The ship state stays at the previous snapshot. *Recovery: retry; the ship continues from the prior snapshot.*
  - Cloud dispatch succeeds but `find_dispatched_run` times out → DispatchedRun is still saved with `run_id="pending-<target>"`. *Recovery: watch/poller backfills the real run id on next tick. Known latent bug: if backfill also fails, the lane is invisible to `auto-merge`.*
  - Preflight raises `BackendUnreachableError` / `ValueError` → ship exits 3/1; no DispatchedRun is written. *Recovery: fix backend or use `--skip-target` / `--allow-unreachable-targets`.*

### T3 — Record a terminal target outcome

- **From:** `STATE_IN_FLIGHT`
- **To:** `STATE_IN_FLIGHT` (with one more target in `evidence_snapshot`) or `STATE_VERDICT_*` once every terminal slot is filled
- **Trigger:** `_update_ship_state_from_job` after `_execute_job` completes; cloud poller updates via the watch path
- **Writes:** `update_evidence(target, "pass"|"fail")`, `upsert_run(...)`, `save()`
- **Externals:** `CloudRecordStore.list_recent` (maps platform → cloud run_id), tip-commit parsing (lane-policy trailer)
- **Failure modes**
  - Evidence saved before DispatchedRun fully resolved → next `watch` tick just sees an in-flight lane with evidence present; no divergence.
  - Process dies between the per-target mutation and `save()` — the prior `save()` already covered the earlier targets. At most one target-outcome event is lost. *Recovery: re-run the target (resume path).*
  - Known historical bug pattern (Pulp consumer report): a PR landed merged but the post-merge evidence row was never saved. Represent as a Phase B test: inject `save` failure after `merge_pr` returns True.

### T4 — Compute the terminal verdict

- **From:** `STATE_IN_FLIGHT` with a full `evidence_snapshot`
- **To:** `STATE_VERDICT_PASS` or `STATE_VERDICT_FAIL`
- **Computation:** `_ship_terminal_verdict(state)` in `cli.py` — returns `True` only if every non-advisory target has `"pass"` evidence, `False` if any non-advisory target failed, `None` while any evidence value is missing or non-terminal
- **Externals:** none — pure function over the state
- **Failure modes**
  - Partial evidence (e.g. evidence present for macos but missing for ubuntu) → verdict stays `None` and `auto-merge` exits 3 for retry. *Recovery: the cloud poller or next `shipyard ship` pass fills the gap.*
  - An advisory lane reports `"fail"` → verdict ignores it because `required=False`. Reviewers still see the failure in `watch`.

### T5 — Merge on PASS

- **From:** `STATE_VERDICT_PASS`
- **To:** `STATE_MERGED` → `STATE_ARCHIVED`
- **Trigger:** `shipyard ship` (end of `_execute_job`) or `shipyard auto-merge <pr>`
- **Writes:** `merge_pr(...)` (gh), on success `ctx.ship_state.archive(pr)`
- **Externals:** `gh pr merge` (branch protection, auth, network)
- **Failure modes**
  - `GhError` (branch protection rejects admin=false, auth, conflicts) → `STATE_MERGE_FAILED`. The state file is NOT archived so a retry can pick up. *Recovery: `shipyard auto-merge <pr>` re-attempts without re-dispatching.*
  - `merge_pr` returns True but `archive(pr)` fails (disk error) → the next `auto-merge` tick will see the state file, verdict PASS, and attempt to merge again. `_pr_is_merged(pr)` catches the double-merge: gh returns "already merged"; we treat that as idempotent success per #64 P2. *Recovery: auto.*
  - Network flake between gh API and our poller: `gh pr merge` succeeded server-side, but our local call timed out; we exit 1 without archiving. Same safe path: next tick sees `STATE_VERDICT_PASS` + a MERGED PR → `_pr_is_merged` returns True → exit 0 + archive.

### T6 — Refuse to merge on FAIL

- **From:** `STATE_VERDICT_FAIL`
- **To:** `STATE_MERGE_REFUSED` (terminal; state retained for inspection)
- **Trigger:** `shipyard ship` or `shipyard auto-merge <pr>`
- **Writes:** none. The state file is intentionally NOT archived — a reviewer may want to inspect `evidence_snapshot` / `dispatched_runs` after the fact. Eventually aged out by `shipyard cleanup --ship-state`.
- **Externals:** none
- **Failure modes**: N/A (no action to fail)

### T7 — Resume an interrupted ship

- **From:** state file exists + no drift
- **To:** `STATE_IN_FLIGHT` (continues where it left off)
- **Trigger:** `shipyard ship` (auto-resume default) or `shipyard ship --resume`
- **Writes:** `save()` after refreshing PR metadata; continues `_execute_job` which writes further target outcomes
- **Externals:** `git rev-parse HEAD` (drift check), `.shipyard/config.toml` (policy signature recomputation)
- **Failure modes**
  - SHA drift (`is_sha_drift`): ship refuses to resume. *Recovery: `--no-resume` to archive + fresh attempt.*
  - Policy drift: required-platforms or target-list changed since the state file was written → refuse. *Recovery: same.*
  - State file is corrupt (truncated JSON, post-crash): `ShipStateStore.get` catches `JSONDecodeError`/`KeyError` and returns None. Caller sees "no state" and creates a fresh one, overwriting the corrupt file. *This is the ship-state equivalent of the queue-file fix in #102.*

### T8 — Force-restart via `--no-resume`

- **From:** any existing state for `<pr>` (FRESH / IN_FLIGHT / VERDICT_*)
- **To:** `STATE_STALE` → archived → new `STATE_FRESH` with `attempt += 1`
- **Trigger:** `shipyard ship --no-resume`
- **Writes:** `ShipStateStore.archive_and_replace(state)` → `save(replaced)`
- **Externals:** none beyond the archive rename
- **Failure modes**: archive rename fails (disk/permission) → exits with OSError, previous state untouched. *Recovery: fix disk, retry.*

### T9 — `cloud retarget` mid-flight

- **From:** `STATE_IN_FLIGHT`
- **To:** `STATE_IN_FLIGHT` with one lane's `DispatchedRun` replaced (new provider + new run_id) and the stale GH Actions job cancelled
- **Trigger:** `shipyard cloud retarget --pr <n> --target <lane> --provider <prov> --apply`
- **Writes:** GH API: cancel old job, dispatch new workflow; `ShipState.upsert_run` → `save()`
- **Externals:** `gh api`, `gh run list`, `workflow_dispatch`
- **Failure modes**
  - Old-job cancel fails → new dispatch proceeds; old job still consumes cloud quota until it finishes. State is consistent.
  - New dispatch fails → neither state nor old job is touched. Caller gets an error.

### T10 — `cloud add-lane` mid-flight

- **From:** `STATE_IN_FLIGHT` (refuses if already past dispatch per `_ship_terminal_verdict`)
- **To:** `STATE_IN_FLIGHT` with one more `DispatchedRun` appended
- **Trigger:** `shipyard cloud add-lane --pr <n> --target <name> --apply`
- **Writes:** `workflow_dispatch`, then `append_run` → `save()`
- **Externals:** same as T9
- **Failure modes**: `workflow_dispatch` failure → no state change; idempotent retry OK.

### T11 — Terminal archive

- **From:** `STATE_MERGED`
- **To:** `STATE_ARCHIVED`
- **Trigger:** `ship` end-of-flow, `auto-merge` on success, `ship-state discard` (manual)
- **Writes:** `os.replace(<pr>.json, archive/<pr>-<timestamp>.json)` — atomic same-directory rename
- **Externals:** filesystem atomic rename
- **Failure modes**: rename fails → state file is untouched; next invocation sees it and hits the `_pr_is_merged` idempotency branch (which treats "already merged" as success).

### T12 — Aging prune

- **From:** `STATE_VERDICT_FAIL` or old archived file
- **To:** deleted
- **Trigger:** `shipyard cleanup --ship-state --apply`
- **Rules (per `ShipStateStore.prune`)**
  - Active state is deleted only if the PR is in the supplied `closed_prs` set AND `updated_at` is older than `active_days` (default 14)
  - Archived files are deleted when their filesystem mtime is older than `archive_days` (default 30)
- **Externals:** `gh pr list --state closed` feeds `closed_prs`
- **Failure modes**: prune is best-effort; a failed `unlink` logs but doesn't abort the cleanup.

## External dependency matrix

Every transition that hits an external system is a potential silent-
failure site. Phase B tests must inject failure at each of these points.

| External                       | Transitions                         | Failure class            | Silent-mode symptom                                                                 |
|--------------------------------|-------------------------------------|--------------------------|-------------------------------------------------------------------------------------|
| Filesystem (`save`/`archive`)  | All (T1, T2, T3, T4-save, T5, T7, T8, T11) | disk full / permission / race | Pre-#102 style truncation; stale tmp files; half-archived state. Fix from core/ship_state.py already uses tmp+replace, but the assumption deserves an explicit test. |
| `git push`                     | T1                                  | auth / network           | State written without a pushed branch → PR create fails; no state written. Benign. |
| `gh pr create`                 | T1                                  | auth / network / rate-limit | Ship aborts with a GhError; no state written.                                       |
| `gh pr list` (find_pr_for_branch) | T1                               | auth / network           | Falls through to create a new PR which may duplicate. Present risk.                |
| `gh pr view` (auto-merge idempotency) | T5 recovery                   | auth / network           | Auto-merge incorrectly reports "pr-not-found" after a successful merge; operator noise, no data loss. |
| `workflow_dispatch` (cloud)    | T2, T9, T10                         | 404 / 5xx / rate-limit   | DispatchedRun saved with `pending-<target>` run_id → `auto-merge` blind to that lane's verdict. |
| `find_dispatched_run`          | T2                                  | timeout                  | DispatchedRun never gets a real run_id; watch cannot tail.                          |
| `gh pr merge`                  | T5                                  | branch protection / auth | `STATE_MERGE_FAILED`; state retained for retry. Retry path works for all known variants. |
| `gh run cancel`                | T9                                  | race                     | Old lane keeps running (cost only). New lane proceeds. State stays consistent.      |
| SSH backend probe              | T2 (preflight)                      | network / auth / host_key | Pre-#100: silent 10-min hang. Post-#100: exit 3 with classified error inside 10s.   |
| `git rev-parse HEAD` / config read | T7                              | worktree gone            | `_detect_ship_state_drift` falls through to "no drift" (conservative). State resumes. |

## Known silent-failure modes from consumer experience

Pulp's PR history surfaced three real bugs that Phase B regression tests
must cover:

1. **Post-merge evidence blackhole.** Shipyard merged the PR, returned
   success, and exited — but the post-merge evidence row for one lane was
   never saved. Agents could not tell if the ship completed because
   `evidence_snapshot` didn't match the merge outcome. *Test design: drop
   `save()` after `merge_pr` returns True and assert the next `auto-merge`
   tick hits the `_pr_is_merged` idempotency branch, not the "target
   failed" branch.*
2. **Resume re-runs a succeeded validation lane.** The resume path trusted
   the DispatchedRun status string without cross-checking `evidence_snapshot`,
   re-dispatching a lane that had already posted PASS evidence. *Test
   design: seed a state with `status="in_progress"` + `evidence="pass"`,
   assert resume skips dispatch for that lane.*
3. **Queue state diverged from filesystem after unclean shutdown.**
   Addressed by #102 for `queue.json`; `ship/<pr>.json` uses the same
   tmp+replace pattern and should get the same test coverage. *Test
   design: kill `save()` mid-write via monkeypatching `os.replace` to
   raise; assert the previous state file is byte-identical.*

## Phase B test plan (preview — not implemented here)

For each transition T1–T12, write at least one test that:

1. Exercises the happy path.
2. Injects failure at each external-dependency row from the matrix above
   and asserts the documented recovery behavior.
3. Asserts that `updated_at` moves forward on every write and stays pinned
   when the transition is read-only.

Plus the three consumer-experience regression tests under "Known silent-
failure modes" above.

## Phase C preview

1. **Doc-sync hook.** Add `docs/ship-state-machine.md` to the skill-sync
   path map so that changes to `src/shipyard/core/ship_state.py` or the
   relevant chunks of `src/shipyard/cli.py` must update this doc in the
   same PR. Mechanism is identical to `scripts/skill_path_map.json` and
   pairs with Pulp's proposed `doc_sync_check.py` (pulp#567).
2. **Dedicated state-machine CI lane.** A single `pytest -m state_machine`
   marker run as its own GitHub Actions job, separate from the full
   suite, so a state-machine failure is visually distinct in the PR
   checks list.

Neither change lands in Phase A. This doc's role is to make Phase B
testable and Phase C's path-map additions obvious.
