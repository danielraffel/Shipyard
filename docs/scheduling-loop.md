# The steward's scheduling loop

Every steward tick gives each open pull request the loop covers one `loop`
object on its row in the tick JSON (`repos[].prs[].loop`). The review,
runner and ledger lanes, and the escalation ladder, read it instead of
deriving state themselves. The steward is the single writer, so it is also
the single scheduler.

The loop adds no GitHub read. It comes from what the pass already read:

- the row's `blocked_on` token, which counts required contexts only (rulesets
  and branch protection, from the app each rule names) and is already one
  token per row in the steward's own precedence;
- the row's decision (merge-queue membership);
- the dwell entry, which dates when the state began at this head;
- the pass-runner's review quarantine file on this host, when one is named.

## Schema

```json
"loop": {
  "state": "required_red",
  "since": "2026-11-13T15:00:00+00:00",
  "action": "claim and fix the red required check",
  "actor": "repair",
  "deadline_at": "2026-11-13T16:00:00+00:00",
  "level": 1
}
```

| Field | Meaning |
|---|---|
| `state` | Exactly one of the states below. |
| `since` | When the pull request entered this state at this head (RFC 3339). A push starts it over. |
| `action` | The act, a short verb phrase. |
| `actor` | Who acts at the current level: `steward`, `queue` (the merge queue or CI; nothing for anyone to do), `reviewer`, `adversary`, `repair`, `stronger_model`, `coordinator`. |
| `deadline_at` | When the current level's deadline passes; `null` while the queue or CI acts, and at the last level. |
| `level` | 0 at entry; one more for each missed deadline, at most 3. |

A row has no `loop` when the loop does not cover it: a draft, a pull request
the steward does not own (`unmanaged`, `handoff_missing`, `invalid_head`,
`direct_merge_refused`), or one carrying a label the pipeline skips by
design (`owner-decision`, `superseded`, `PARKED/*`, the opt-out label, a
provenance label).

## States, in precedence

The first match wins:

1. `stuck_group`: the row reads `merge_group_stalled:<context>`, so its merge group can never finish.
2. `queued`: the pull request is in the merge queue (or was enqueued this pass). A queued pull request is never `required_red` or `hold`, whatever a check on its head says.
3. `quarantined`: the quarantine file lists this pull request at its current head.
4. Otherwise the state the `blocked_on` token names:

| `blocked_on` | `state` |
|---|---|
| `ready`, `merge_state:BEHIND` (the queue tests on top of main, so behind is never an act), `enqueue_refused:*`, `generated_path_batch:*`, `defect_fix_removed:*` | `ready` |
| `no_verdict`, `waiting_head_approval`, `approval_excluded:*`, `waiting_code_owner:*`, `code_owner_review`, `carries_unapproved_commits:*` | `awaiting_review` |
| `required_pending`, `waiting_full_test_tier`, `waiting_enqueue_requirements`, `merge_state:` other than `DIRTY`/`BEHIND` | `check_running` |
| `capacity_queued` | `capacity_queued` |
| `required_failed:*` | `required_red` |
| `held:*` | `hold` |
| `merge_state:DIRTY` | `dirty` |
| `ejected_same_head:*`, `ejection_cap` | `ejected` |
| anything else: `transient_rerun`, `required_never_ran:*`, `approval_unread`, `steward_error`, `uncertain_mutation`, `revalidating`, `deferred_pass_budget`, `admission_pending`, `required_policy_unknown`, `carried_commits_*`, and any token added later | `tooling` |

## Acts and deadlines

| `state` | `action` | Level at entry, actor | First deadline | Then |
|---|---|---|---|---|
| `ready` | enqueue the exact head | 0, `steward` | 2 min | +30, +30 |
| `awaiting_review` | review the exact head | 0, `reviewer` | 30 min | `adversary`, +30, +30 |
| `required_red` | claim and fix the red required check | 0, `repair` | 10 min (a repair claim) | takeover at 60 min without a push, +60 |
| `hold` | claim and address the HOLD | 0, `repair` | 10 min | takeover at 60 min without a push, +60 |
| `dirty` | merge origin/main and regenerate | 0, `repair` | 5 min | +30, +30 |
| `quarantined` | review the exact head with a stronger model | 2, `stronger_model` (at once) | 30 min | `coordinator` |
| `ejected` | re-queue the same head or push a new one | 0, `steward` | 10 min | +30, +30 |
| `stuck_group` | dequeue as defect-fix | 0, `steward` | 30 min | +30, +30 |
| `tooling` | re-run on another slot | 0, `steward` | 10 min (the next tick or two) | +30, +30 |
| `queued`, `check_running`, `capacity_queued` | wait (the host is watched for capacity) | 0, `queue` | none | none |

Each missed deadline raises the level by one rung. After the state's own
actor at L0, L1 is `repair` (the `adversary` after a reviewer), L2 is
`stronger_model` on the exact head, and L3 is `coordinator`
(`FLEET_PASS_ESCALATION_TARGET`). It never goes to the fleet's owner.

## What the steward cannot see

- **Repair claims.** The steward reads no PR comments. A `required_red` or
  `hold` row reaches level 1 at 10 minutes whether or not a repair claim was
  posted. A consumer that reads claims (the escalation ladder, `shipyard
  relay`) holds level 1 back while a claim is fresh, and acts on level 2
  (60 minutes with no push) regardless.
- **Ejection causes.** The flake (re-queue once within 10 minutes) and
  main-red (re-queue when main is green) branches need the ejection cause.
  The row token carries it as `ejected_same_head:<cause>`. Both branches
  start at the same 10-minute deadline here; the act that tells them apart
  ships with the ejected re-queue.

## The quarantine file

The pass-runner appends `<repo>#<pr> <head> <reason> [since=<epoch>]` to
`$STATE/review-quarantine.txt`. The steward reads the file the environment
variable `SHIPYARD_REVIEW_QUARANTINE` names, else the machine-global key
`[loop] review_quarantine_file`. With neither set, or the file absent or
unreadable, no pull request is ever `quarantined`. Only the head the line
names is quarantined; a new head is classified normally.

## Records

An applying steward pass keeps, per pull request, the state and head it
last classified and when it entered them (`loop_seen` in the steward
ledger), so `since` survives ticks for states no read dates. A report pass
keeps nothing.
