# Terminal reconciliation conflated a closed PR with a merged PR

**Status:** IN PROGRESS
**Filed:** 2026-09-02 by Codex@M3
**Owner:** GEN-14 Shipyard stewardship
**Class:** SYSTEMIC
**Recurrence:** first confirmed production instance

## What happened

Shipyard v0.152.2 fixed the legacy `NULL base_ref` read failure for an exact
uncertain-dispatch row, but the supported dry-run still could not reconcile it.
The native PR was closed without merging. The command accepted only a merged PR,
even though Shipyard PR #542 explicitly promised to repair this production row.

## Evidence

```text
$ shipyard work-ledger reconcile-terminal \
    --repo generous-corp/agent-workstream --pr 21 \
    --head 488ee202e91755c96515b6858d07c426992f0257 --json
terminal reconciliation merge authority is incomplete or changed

$ ghapp pr view 21 --repo Generous-Corp/agent-workstream \
    --json state,headRefOid,baseRefName,mergeCommit,mergedAt,closedAt
state=CLOSED
headRefOid=488ee202e91755c96515b6858d07c426992f0257
baseRefName=main
mergeCommit=null
mergedAt=null
closedAt=2026-09-01T22:31:54Z
```

Shipyard PR #542's published acceptance text says it supplies the supported
repair for this exact stale closed PR 21 row. Its implementation at merge
`294829a84c69430ac4472390a1a06aa591746c6f`, however, requires
`state == MERGED`, a merge commit, and a merge timestamp.

## Constraint discovery

### Toolchain

Shipyard is a Rust CLI with GitHub App authentication, an append-only SQLite
work ledger, protected receipt objects, exact-head GitHub reads, and a second
remote read inside exclusive writer custody. Release and fleet delivery use
Shipyard's own protected release and fleet-update workflows.

### Existing rationale and motivating incident

The strict merged-head proof prevents an unmerged, reopened, moved, or
ambiguously identified PR from being recorded as successfully integrated. The
production incident motivating #542 was an uncertain provider dispatch whose
native PR had already reached a terminal closed state while its durable ledger
row remained dispatching and unbound.

### Guarantee preserved

Current design prevents: treating an unmerged or moved PR as merged.

The repair preserves that guarantee by representing `merged` and
`closed_unmerged` as distinct typed dispositions. The closed disposition
requires exact repository/PR/head/base identity, `state=CLOSED`, a valid
`closedAt`, and absent `mergeCommit`/`mergedAt`. It emits distinct evidence and
never manufactures or implies merge authority. Both paths retain the second
identical GitHub read and complete local generation/wake/delivery fences.

### Mechanism verification

| Failure | Mechanism that works | Why alternatives do not |
|---|---|---|
| Closed PR rejected as not merged | Typed GitHub terminal authority parser | Retrying cannot change a deliberate state mismatch |
| Closed PR accidentally reported merged | Distinct disposition and optional merge fields | A sentinel or synthetic merge SHA would create false evidence |
| PR reopens or head/base changes during repair | Second exact GitHub read under writer custody | A single preflight cannot detect the transition |
| Response loss duplicates state | Content-bound receipt plus exact replay | Operator memory or a new repair attempt is not crash-safe |

## Root cause

The command used a single `TerminalMergeAuthority` model for every terminal PR
outcome. Tests covered `MERGED` and rejection of `OPEN`, but did not exercise
the production `CLOSED` without merge fixture named in #542's acceptance text.

## Why it will recur

Any uncertain dispatch whose PR is deliberately closed rather than merged will
produce the same stranded row. This is a workflow state, not a one-off GitHub
error.

## The fix now (unblock)

Ship a narrow successor that supports the exact closed-without-merge outcome,
then deploy it identically and rerun the PR21 dry-run before any apply.

## The fix forever (prevent)

Keep terminal outcomes typed end to end; preserve merged receipt byte
compatibility; add parser, ledger, command, replay, drift, and mixed-evidence
tests for both outcomes; document the distinction in the CLI and agent skill.

## Routing

- [x] Fix now, by GEN-14 Shipyard owner
- [ ] Hand off

## Resolution

**Outcome:** Implementation and production canary in progress.
**Landed in:** pending
**Did it work?:** Targeted tests pass locally; live PR21 dry-run/apply pending release and fleet deployment.
**Still open:** Merge, release, deploy, exact live dry-run/apply/replay, and final report update.

Confidence: HIGH

Why: the production PR state, shipped parser, #542 acceptance statement, and
exact refusal were all inspected directly. The prevention mechanism has focused
tests but has not yet completed the live fleet canary.

What would raise it: successful exact PR21 dry-run, apply, restart/readback, and
write-free replay using the released fleet binary.
