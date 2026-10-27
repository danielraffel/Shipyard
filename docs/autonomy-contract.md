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

| ID | Requirement | Enforcement and evidence | Production violation action |
| --- | --- | --- | --- |
| A1 | Accepted work is durable before an external side effect. | `queue_request`, `ship_state`, `recovery_worker`, and receipt tests cover atomic intent, idempotency keys, and replay. | Reconcile the durable intent; do not issue an un-fenced retry. |
| A2 | Workers are replaceable. | Queue-worker receipts, boot identity, leases, and generation fencing reject stale workers. | Retire only a worker proven dead; redispatch from durable state. |
| A3 | A stall causes active remediation. | Supervisor ticks run reconciliation and queue-absence recovery; recovery tests must assert enqueue/redispatch, not only a report. | Classify the blocker, retry or redispatch within the bounded policy, then read back authoritative progress. |
| A4 | Retry is bounded and classified. | Recovery records persist attempts, status, policy/config signatures, and terminal `needs_agent`/escalation. | Stop at the budget and persist a specific actionable terminal reason. |
| A5 | External effects are idempotent and fenced. | Exact repo/PR/head, generation, App identity, claim epoch, and external ID are required by mutation guards. | Reconcile unknown outcomes; reject stale or duplicate effects. |
| A6 | Controller restart is safe. | Startup sweeps reconcile orphaned claims and pending outbox records; crash-injection tests cover claim, dispatch, lost response, receipt commit, and replay. | Restart the controller, replay pending durable work, and deduplicate by the same identity. |
| A7 | Code failures return to an agent with context. | Durable recovery records retain exact head, failure facts, required checks, and bounded summary. | Redispatch an eligible implementation agent or persist `needs_agent`; never silently drop work. |
| A8 | Host and dependency loss delays progress but does not lose work. | Reconciliation is event-driven with periodic sweep; state is on the durable state root; host-local claims are fenced. | Move work to an eligible host when authority permits; otherwise retain a durable blocked reason. |
| A9 | Cross-host handoff is self-describing. | Handoff records contain objective, exact commits, state, blockers, authorization, and recovery instructions; startup reads the contract and live state separately. | Reject stale protocol versions and require a fresh handoff or migration. |
| A10 | Progress is proved after remediation. | Every recovery test performs an authoritative post-action read and checks the exact head/generation. | Keep the item recoverable and escalate with evidence if progress cannot be proved. |
| A11 | GitHub policy remains the security boundary. | Required checks, branch protection/rulesets, App identity, and exact-head approvals are re-read before mutation. | Fail closed; no raw merge, forged approval, or bypass. |
| A12 | The system converges without routine human intervention. | Scheduled steward/supervisor plus bounded reconciliation and metrics for material work; no interactive session is required. | Escalate only unsafe, ambiguous, or irrecoverable states with an owner and next action. |

## Required failure tests

Each recovery implementation must include deterministic tests for: original
agent loss; coordinator loss; host reboot/offline; dependency outage and return;
claim expiry; duplicate dispatch; lost external acknowledgement; stale worker
mutation; controller kill at each durable transition; restart replay; retry
exhaustion; and an authoritative post-remediation progress read. A test that
only detects or reports a failure is insufficient.

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
