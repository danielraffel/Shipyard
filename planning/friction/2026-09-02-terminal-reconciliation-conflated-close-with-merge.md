# Terminal reconciliation conflated a closed PR with a merged PR

**Status:** RESOLVED
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

**Outcome:** Shipyard now reconciles closed-without-merge rows as the distinct
`closed_unmerged` terminal disposition without manufacturing merge authority.
**Landed in:** PR #547, merge `e6691a6059b2f3f829552c76432a38bb47ed9b88`,
released as v0.153.0 from that exact commit and deployed identically to M1, M5,
and Studio.
**Did it work?:** Yes. On M5, the exact PR21 no-write plan classified
`closed_unmerged` with plan digest
`bacd4b7140d4d79380ff501a2eccaaf6bc0ac6a109ab6cae02ea4b4c000e7455`
and receipt digest
`7dbcc2d891082fe272417d42191c6676630397b14aeec493739d7420c6ebec3a`.
Apply terminalized work item
`wi_32b4d12bc538a693271ce531f53589c268b35e009e01e932b1812492df47997d`
at generation 7. After a daemon restart, inventory retained the exact terminal
row and the same command returned `applied=false, replay=true` with unchanged
digests.
**Still open:** Nothing for this incident. Broader custody transport work and
unrelated daemon policy/webhook diagnostics remain separate GEN-14 lanes.

Confidence: HIGH

Why: the production PR state, original refusal, typed implementation, release
ancestry, identical fleet installation, exact apply receipt, restart readback,
and write-free replay were all inspected directly.

What would raise it: no further evidence is required for this incident.
