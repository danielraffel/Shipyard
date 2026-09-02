# Custody disable lacked crash-safe generation proof

**Status:** IN PROGRESS
**Filed:** 2026-09-02 by Codex@M3
**Owner:** GEN-14 Shipyard durable custody
**Class:** SYSTEMIC
**Recurrence:** first live-canary acceptance audit; every future custody host would share it

## What happened

Gate GEN-94 requires the first live M3-to-M5 durable-custody canary. Shipyard
could provision and diagnose the carrier, but returning a host to default-off
only removed `[custody_transport]`. A crash after that edit had no immutable
record of which exact policy generation was removed or whether custody history
survived unchanged. The canary therefore had no legally replayable rollback
proof.

## Evidence

```text
Inspected source base: 7e7b0ac55725499b3ab316fab12e26f9f05aa014

Original sequence:
  validate digest -> check active rows -> replace config -> read doctor result

Missing boundaries:
  no durable pre-write intent
  no immutable post-write receipt
  no exact ledger schema/history binding
  no restart recovery for the config/receipt gap
```

The first implementation review then proved three adjacent authority failures:

```text
1. dry-run ledger inspection could create writer lock/SQLite sidecar files and
   did not hold one coherent config/history/receipt snapshot;
2. an unresolved intent for policy A could be ignored while policy B was
   installed, and reinstalling an already-disabled identical policy digest had
   no new operation generation;
3. row hashing did not authenticate the custody tables, constraints, indexes,
   or append-only/state-transition triggers that give those rows meaning.
```

## Constraint discovery

### Toolchain

Shipyard is a Rust CLI with its own `.shipyard/config.toml`, GitHub Actions test
and release workflows, an owner-only machine-global configuration, an
append-only SQLite work ledger, immutable record stores, and a machine-global
writer domain. Changes ship through a PR; this isolated successor is handed to
the primary integration owner and never pushed directly to `main`.

### Existing rationale and motivating incident

The carrier is deliberately default-off. Host enrollment, SSH keys,
`known_hosts`, `authorized_keys`, sshd subsystem setup, routes, and private
readiness receipts remain protected owner operations. Shipyard validates those
inputs but must not generate, copy, delete, or infer them. Custody lifecycle
rows and terminal receipts are append-only because they are the recovery and
audit authority after a process or machine failure.

### Guarantee preserved

Current design prevents: ambient SSH enrollment, policy guessing, disabling an
unknown generation, disabling with active custody, and deletion of custody
history or owner-managed trust material.

The repair preserves those guarantees by changing only the policy cutover and
its evidence. It uses the existing protected writer domain, removes only the
exact digest-matching config table, leaves all SSH/private profile material
owner-managed, and never updates or deletes ledger history.

### Mechanism verification

| Failure | Mechanism that works | Why alternatives do not |
|---|---|---|
| Crash before config publication | Immutable intent committed first | A final doctor read has no state after process death |
| Crash after config publication | Exact intent recovery and completion receipt | Retrying a stateless edit cannot distinguish completion from a different generation |
| Concurrent config/custody write during proof | Exclusive writer-domain snapshot from first read through result | Separate shared locks permit mixed generations |
| Dry-run mutates a clean host | Noncreating existing-lock acquisition plus immutable SQLite snapshot | Ordinary SQLite read-only opens may create `-shm`; the creating lock API materializes files |
| Policy B bypasses unresolved policy A | One globally ordered intent chain and provision-time pending fence | Filtering pending intents by requested digest hides A |
| Identical policy is reinstalled | New monotonic intent sequence chained to the previous receipt | Policy digest alone cannot distinguish two installations with identical bytes |
| Append-only trigger or constraint is removed | Canonical current custody schema/topology digest | Table names plus row hashes cannot prove the semantics of the rows |

## Root cause

Disablement was initially modeled as a guarded configuration edit rather than
as a multi-surface state transition. The config file and custody ledger have
separate crash and concurrency boundaries, so a final readback without durable
intent/receipt state could not establish exact completion. The first receipt
draft then reused read and recovery helpers whose contracts were too weak for
authority: shared/creating locks, per-policy pending lookup, and generic schema
identity.

## Why it will recur

Every enrolled host has the same config/ledger split. A crash, reprovision, or
schema drift can occur on any canary or rollback, and identical policy manifests
are a normal supported sequence rather than a malformed edge case.

## The fix now (unblock)

Land the crash-safe disable state machine and its negative controls before the
first physical custody canary. Until the release is fleet-deployed, do not
treat config absence as a durable disable receipt.

## The fix forever (prevent)

- Persist one immutable, globally sequenced intent before config publication.
- Chain each later intent to the prior completion receipt and block provisioning
  while any intent is unresolved.
- Hold a type-distinct exclusive production snapshot across config, exact
  custody schema/history, receipt-store reads, publication, and readback.
- Keep dry-run strictly noncreating; refuse live/uncheckpointed or orphan SQLite
  sidecars rather than repairing them.
- Bind the exact current custody table/constraint/index/trigger topology and
  deterministic row snapshot into every intent and receipt.
- Retain crash checkpoints, cross-generation/reprovision tests, schema mutation
  controls, no-write filesystem snapshots, and writer/read races in the focused
  suite.

## Routing

- [x] Fix now in the isolated custody-disable successor
- [x] Hand the reviewed local commit to the primary #543 integration owner

## Resolution

**Outcome:** Implemented, adversarially reviewed, and locally validated for
primary-owner integration. The focused custody setup suite is 36/36 green, the
explicit read-barrier suite is 3/3 green, and strict all-target/all-feature
Clippy passes.
**Landed in:** Pending primary-owner integration into Shipyard PR #543.
**Did it work?:** Failure-capable tests now cover all three persisted crash
checkpoints, exact replay, supported same-policy reprovision, an unresolved A
blocking both same-policy A and cross-policy B provisioning, byte/inode/name-
stable dry-run, orphan WAL refusal without `-shm` creation, concurrent writer
exclusion, custody DDL/trigger drift, and canonical schema equivalence after
authentic schema 10, 11, and 12 production migrations. The independent security
re-review found the original three authority classes repaired and exposed one
remaining crate-visible unfenced provisioning primitive; that primitive is now
private. Final `autoreview --mode local` reports no accepted/actionable finding.
The broad repository run completed 3,113 passed, 34 failed, and 2 ignored; all
34 failures are outside this change and are the existing local GitHub-auth/test-
environment classes in fleet status, local Linux lease, merge stewardship, and
runner reaping. All post-library integration targets passed.
**Still open:** primary integration, release/fleet deployment, and the protected
M3-to-M5 canary. The integration owner must replace this pending line with the
exact PR head/merge and physical receipts when those gates complete.

Confidence: HIGH for the diagnosed mechanism; MEDIUM until integration and the
physical canary complete.

What would raise it: exact fleet canary receipts from both custody directions
after the successor release is deployed.
