# Autonomous fleet architecture contract

This is the version-controlled contract for Shipyard's autonomous review, CI,
merge, release, and recovery paths. It is a set of testable invariants. A
feature is not compliant because it records a failure: it must attempt the
permitted remediation, redispatch or return work to an eligible agent, and
prove progress from an authoritative read after the action.

GitHub remains authoritative for pull requests, reviews, required checks, and
merge queues. Shipyard state is durable intent, leases, receipts, and cache;
it never substitutes for GitHub state.

## Invariants

| ID | Requirement | Blocker detector and durable state | Recovery trigger and action | Post-action proof and terminal escalation | Executable evidence |
| --- | --- | --- | --- | --- | --- |
| A1 | Accepted work is durable before an external side effect. | `queue_request`, `ship_state`, and recovery records commit intent, identity, and attempt before dispatch. | Startup/reconciliation replays the same intent; an absent or malformed record is held. | Provider readback matches the intent or records `needs_agent` with the exact missing fact. | test: src/immutable_store.rs::immutable_replay_survives_reopen_and_conflicts_fail; gap: recovery-worker lease integration receipt |
| A2 | Workers are replaceable. | Heartbeats, boot identity, leases, and generation receipts detect stale or dead workers. | A scheduled sweep retires only proven-dead work and redispatches the durable request. | A replacement receipt with a newer generation is observed; uncertain liveness remains held. | test: src/execution_supervisor.rs::dead_receipt_retirement_serializes_concurrent_replacement; test: src/execution_supervisor.rs::daemon_restart_adopts_exact_live_worker_without_replay |
| A3 | A stall causes active remediation. | Supervisor and merge-queue liveness classify stale checks, absent queue entries, and idle capacity. | The supervisor enqueues a bounded retry or redispatches to an eligible lane, then verifies the provider state. | The PR/run/receipt advances; otherwise the record carries the blocker, attempt, and next deadline. | test: src/merge_queue_liveness/tests.rs::stale_required_check_and_optional_work_expose_useful_progress_wedge; test: src/queue.rs::drain_owner_recovers_stale_running_jobs |
| A4 | Retry is bounded and classified. | Recovery records persist attempt count, deadline, policy/config signature, and terminal disposition. | Retry recoverable infrastructure failures with backoff; return code failures to an agent; stop at budget. | Terminal state names the exact actionable reason and owner; no infinite retry loop is allowed. | test: src/app/merge_steward_cmd/recovery_worker/tests.rs::oversized_hostile_worker_failure_always_terminalizes_within_store_bound; gap: freshness-budget integration receipt |
| A5 | External effects are idempotent and fenced. | Exact repo/PR/head, generation, App identity, claim epoch, and external ID guard mutations. | A lost response causes readback and deduplicated replay, never an unbounded blind retry. | One provider effect is attributable to one identity; stale or duplicate attempts are refused. | test: src/pr.rs::exact_head_match_refuses_duplicates; test: src/parallel_proof.rs::crash_points_leave_no_partial_record_and_restart_recovers |
| A6 | Controller restart is safe. | Pending outbox, claim, receipt, and queue transitions are durable before the controller can exit. | Launchd/scheduled steward restart runs reconciliation and replays pending work from durable state. | A restart test observes the same work complete once and records the authoritative result. | test: src/execution_supervisor.rs::daemon_restart_repairs_missing_terminal_outcome; test: src/immutable_store.rs::restart_reconciles_private_pending_files_and_refuses_unsafe_pending_entries |
| A7 | Code failures return to an agent with context. | Failure records retain exact head, failed checks, logs/facts, attempts, and authorization context. | A bounded recovery worker dispatches an eligible implementation agent; otherwise persists `needs_agent`. | The new head or agent receipt is read back; no failure is silently dropped. | test: src/app/merge_steward_cmd/recovery_worker/tests.rs::preclaim_repository_error_durably_rotates_to_the_next_request; gap: changed-failure freshness integration receipt |
| A8 | Host and dependency loss delays progress but does not lose work. | Durable state root holds intent; host-local leases and provider outage facts are separate. | Periodic reconciliation retries after dependency recovery or selects another eligible equivalent host; host-bound work stays fenced. | Provider health and the new lease/receipt are observed before progress is counted. | test: src/queue.rs::recovery_enqueue_fails_closed_on_unknown_active_unscoped_owner; gap: host-pool lease pruning integration receipt |
| A9 | Cross-host handoff is self-describing. | Handoff records contain objective, exact commits, state, blockers, authorization, recovery instructions, and protocol version. | A new host loads the contract, rejects stale protocol versions, and reconstructs from live provider state. | The successor records its own receipt and can continue without the original transcript. | gap: cross-host live handoff receipt; gap: two-host recovery receipt |
| A10 | Progress is proved after remediation. | Every recovery action records its target identity and expected state transition. | Reconcile, remediate/redispatch, then perform an authoritative provider read; do not close on a local log. | Exact head/generation advances or a specific terminal blocker is emitted. | gap: merge-queue liveness integration receipt; gap: authoritative post-remediation provider read |
| A11 | GitHub policy remains the security boundary. | Required checks, rulesets, App identity, exact-head approvals, and merge-queue state are re-read immediately before mutation. | Any mismatch fails closed and returns the item to reconciliation; no raw merge or forged verdict. | GitHub audit/readback proves the mutation was by the authorized App at the approved head. | gap: ghapp queue-arm guard receipt; gap: ghapp merge guard receipt; gap: exact-head negative-control receipt |
| A12 | The system converges without routine human intervention. | Scheduled steward/supervisor and material-work metrics identify every nonterminal state and deadline. | The steward retries, redispatches, or routes to a named terminal escalation without an interactive session. | Eligible work reaches a provider terminal state; unresolved work has an actionable owner and next action. | gap: scheduled-steward integrated receipt; gap: crash/recovery matrix receipt |

The table deliberately separates detection, action, and proof. A detector that
only emits an alert is not evidence of self-healing. The executable names are
the current unit-level evidence; the cross-host and real-provider cases are
explicit gaps until their integrated tests produce receipts.

## Blocked-state invariants

`blocked` is a durable, safety-preserving state, not an invitation to keep
trying. A blocked record must retain the exact repository/PR/head (or queue
identity), blocker code, owner, attempt count, and next review deadline. The
following conditions are hard holds:

- a deliberate `HOLD`, owner-held PR, or unresolved exact-head approval;
- a queue-removal or merge-group guard that has not produced its required
  readback;
- missing, stale, contradictory, or unauthenticated provider evidence; and
- uncertain worker liveness, request identity, or host ownership.

Re-admission or redispatch is permitted only after a fresh authoritative read
proves that the hold is gone, the exact head and required approvals still
match, the queue-removal guard permits the action, and an eligible owner and
capacity are available. A local timeout, an absent queue row, or a successful
diagnostic alone never clears a blocked record. If any prerequisite remains
unknown, reconciliation preserves `blocked` and records the next deadline.

## Failure and liveness matrix

The following cases are the minimum integrated acceptance suite. Each test must
kill or disconnect the named component, restore sufficient capacity, and assert
both safety (no duplicate privileged effect) and liveness (the eligible work
reaches the next authoritative state). A fixture-only refusal is insufficient.

| Case | Injected failure | Recovery owner/action | Forward-progress assertion | Safety assertion |
| --- | --- | --- | --- | --- |
| F1 | Worker exits after claim | successor steward reclaims after proven-dead lease | exact request gets a replacement receipt and completes | stale generation cannot mutate |
| F2 | Controller exits after intent, dispatch, lost acknowledgement, and receipt commit | scheduled restart replays durable intent | provider reaches the expected terminal state | one idempotency key/effect |
| F3 | Two hosts reconcile the same stale claim | shared CAS/lease fence elects one owner | elected owner completes the request | loser receives a stale-epoch refusal |
| F4 | Host reboots with host-bound VM work | recovery holds the lease until liveness is proven, then redispatches when safe | eligible independent work continues and the VM request converges | no duplicate VM or release effect |
| F5 | GitHub/TartCI dependency outage then recovery | bounded backoff and periodic reconciliation | work resumes after the dependency returns | no blind retry during unknown outcome |
| F6 | Required check is stale or merge queue entry disappears | steward refreshes head/checks; re-admission is refused for a deliberate HOLD, owner-held PR, missing exact-head approval, or an uncleared queue-removal guard | eligible PR enters the queue or reaches merge only after every hold is cleared by authoritative readback | approval, head, HOLD/owner state, and queue-removal guard remain exact |
| F7 | Genuine code failure | recovery worker returns exact facts to an implementation agent | a repaired head is reviewed and progresses | failed head is never auto-merged |
| F8 | Full fleet/controller restart | every host starts a reconciliation pass from durable state | all eligible work is rediscovered and drains | no stale worker or duplicate privileged effect |

F1-F8 are rollout blockers for autonomous mode. The current branch proves many
of the underlying unit transitions, but the real configured App, GitHub
rulesets, TartCI provider, and two-host recovery receipts are still required.

## Required failure tests

Each recovery implementation must include deterministic tests for: original
agent loss; coordinator loss; host reboot/offline; dependency outage and return;
claim expiry; duplicate dispatch; lost external acknowledgement; stale worker
mutation; controller kill at each durable transition; restart replay; retry
exhaustion; and an authoritative post-remediation progress read. A test that
only detects or reports a failure is insufficient. Every safety assertion must
cover duplicate effects, including duplicate privileged effects where the
provider operation is irreversible.

The tests must use a fake provider or recorded fixture at the wire boundary,
then one integrated test against the real configured App and GitHub protections
before the default-off rollout can be enabled. Failure tests must prove both
positive recovery and planted-negative refusal.

## Knowledge and handoffs

Architecture and procedures live in this repository and the fleet handbook.
Live claims, permissions, PR status, queue entries, and service health are
always fetched from their authoritative providers. Agents must load the
contract and the handbook's current procedure at startup, then inspect live
state before acting. Handoffs are durable, discoverable, exact-head records;
conversation history is never a required dependency.

Any change to these invariants requires an accompanying enforcement/test
change or an explicit documented gap. Keep the autonomous rollout default-off
until all release blockers and integrated recovery evidence pass independent
review.
