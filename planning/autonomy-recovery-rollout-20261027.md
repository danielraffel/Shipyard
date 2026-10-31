# Autonomous recovery rollout plan

Status: **Phase 0 — contract and evidence in progress; rollout remains default-off.**
Owner: Shipyard steward with Ultra independent review.

This plan is deliberately phased so restoring material merges does not wait for
all future hardening. Every phase leaves GitHub's native queue and Shipyard's
landing authority intact; it adds no competing merge ledger or interactive
session dependency.

## Phase 0: establish the contract and baseline

- Keep `docs/autonomy-contract.md` referenced by `AGENTS.md` and `CLAUDE.md`.
- Record current queue depth, material merges/hour, queue-empty-while-ready,
  approval-to-merge latency, recovery latency, and batch size. Exclude chore and
  draft PRs.
- Reconcile the live GitHub App identity, branch protection/rulesets, exact-head
  approval policy, and fleet service health. Documentation is not live state.
- Keep reviewer claims and autonomous rollout disabled until the NO-GO findings
  have integrated evidence.

Exit: the baseline and every open blocker have a named owner and exact next
check; a new agent can discover the plan without conversation history.

## Phase 1: durable remediation and redispatch

- Persist intent and idempotency/fencing identity before any external mutation.
- On stalled or interrupted work, classify the blocker and automatically retry,
  reconcile, enqueue, redispatch, or return a complete exact-head repair record
  to an eligible agent. Detection-only reports do not pass.
- After remediation, read GitHub/Shipyard/TartCI authoritatively and record
  exact-head progress. Bound retries and persist terminal escalation.
- Run failure-injection tests with the original agent, coordinator, and host
  absent. Include lost acknowledgements and dependency outage/recovery.

Exit: an accepted material PR either advances or has an explicit durable
blocker; no work is lost when a worker/session disappears.

## Phase 2: controller and host recovery

- Kill the controller at each intent, dispatch, acknowledgement, receipt, and
  post-action boundary; restart it and replay the same durable work.
- Prove stale generations cannot mutate after reassignment and duplicate
  dispatches collapse to one external effect.
- Reboot or remove the coordinating host; recover from a second eligible host
  only where shared durable authority and App permissions prove ownership.
  Otherwise retain a safe durable hold rather than guessing that a foreign host
  is dead.
- Verify launchd/scheduled steward recovery and a fresh health/readiness signal.

Exit: controller restart and host loss converge without duplicate privileged
operations, and recovery itself has a tested recovery path.

## Phase 3: integrated canary and throughput

- Layer the full reviewer-claim guard, Rust/Python fixture round-trip, durable
  publication/rebind/revoke path, and real App create/read/revoke probe.
- Deploy to one repository/class in shadow mode, then one material canary. Use
  the native merge queue and rollups where configured; route independent work in
  parallel and select available fleet capacity rather than a fixed slow gate.
- Measure material merge throughput and queue wait against Phase 0; roll back on
  stale-authority, duplicate-effect, lost-work, or queue-stall evidence.

Exit: a material PR lands through the native queue after a witnessed recovery,
with no interactive team-lead dependency and independent Ultra review of the
evidence.

## Required recovery matrix

The implementation and evidence must cover queue-admission loss, queue removal
or merge-group failure, infrastructure versus code CI failure, stale reviewer,
merge conflict, agent/session/capacity loss, host loss, dependency outage,
lost external acknowledgement, expired authorization, repeated
non-convergence, downstream dispatch loss, controller crash, and full fleet
restart. Each case records detection, durable state, recovery owner, automatic
action, retry/repair choice, fencing, post-action proof, and recovery-of-
recovery behavior.

Cross-host handoffs carry objective, exact commits, current state, completed
work, blockers, authorization context, protocol version, and recovery steps.
Live claims, permissions, queue entries, PR status, and service health are
re-fetched from their authoritative providers before mutation.
