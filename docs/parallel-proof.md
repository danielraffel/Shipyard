# Parallel proof invariant core

Status: schema v1, shadow only. This module cannot satisfy a protected merge
check and does not dispatch work.

Shipyard currently validates targets as whole jobs. Re-running the same 20,702
tests on M1, M3, and M5 proves three independent executions, but it serializes
the suite on each machine and rebuilds equivalent inputs. The intended end
state is different: build one exact subject once, execute an exhaustive shard
plan across eligible machines, and aggregate immutable receipts for that one
artifact.

`src/parallel_proof.rs` is the invariant boundary needed before transport or
scheduling can do that safely. It is deliberately a shadow implementation. It
records what a future production controller must prove, but it has no queue,
runner, GitHub, Check Run, or merge-authority integration. Consequently this
slice alone produces no queue-time improvement.

## Authority boundary

Every v1 manifest contains `shadow_only: true`. Construction and validation
reject `false`. Both the manifest and aggregate expose
`satisfies_merge_readiness()`, which always returns `false`; a passing v1
aggregate is only `shadow passed`. No conversion to Shipyard evidence or a
GitHub required check exists.

Moving from shadow evidence to merge authority requires a separately reviewed
schema/version and end-to-end rollout. Reinterpreting an existing v1 record as
authoritative is forbidden.

## Immutable proof identity

One manifest binds all of the following through a domain-separated SHA-256
digest:

- numeric repository identity plus canonical `owner/name`;
- subject kind and identity (pull-request head or merge-group head);
- exact commit object and exact Git tree in one consistent Git object format;
- full build-contract digest, toolchain closure, target, and profile;
- artifact payload digest, member-layout digest, byte length, source tree, and
  build-contract digest;
- producer identity, runner/VM image, sandbox policy, artifact trust class,
  network/mount policy, and required execution boundary;
- canonical test-inventory digest; and
- exhaustive shard-plan digest.

The artifact repeats its source-tree and build-contract bindings so an artifact
cannot be substituted between otherwise similar manifests. A worker report
must independently echo the artifact, build contract, head, and tree it
observed. Any mismatch is rejected before a receipt exists.

Serialization is deterministic JSON over canonical ordered records. Every
digest has a distinct domain and length-prefix framing; digests from one record
class cannot be replayed as another.

## Canonical inventory and shard topology

Inventories are bounded, sorted, unique, and validated before hashing. Each
test declares:

- exact CTest identifier;
- `DEPENDS` edges;
- fixture setup, requirement, and cleanup names;
- `RUN_SERIAL`;
- each `RESOURCE_LOCK` plus an explicit `host` or `fleet` scope; and
- required worker capabilities.

Unknown dependencies, missing fixture setup, dependency cycles, duplicate
identifiers, conflicting lock scopes, control characters, and non-canonical
ordering fail closed. Schema v1 accepts at most 100,000 tests, 4,096 shards,
1,000,000 topology relations, 256 authenticated capabilities per assignment,
32 immutable attempts per shard, 16,384 attempt records per aggregate, 512
bytes per identifier, and 16 MiB per worker, durable, or aggregate-input
corpus. Size validation streams into a bounded counter rather than first
allocating an unbounded encoded copy. Semantically valid inventories, plans,
assignments, reports, and receipts must also fit the bounded durable payload,
preventing the count and string limits from combining into an accepted record
that cannot be persisted.

A plan must assign every inventory test exactly once to a non-empty shard. It
may not add tests, omit tests, or duplicate tests. The deterministic planner
keeps the connected components induced by dependency and fixture edges in one
shard. This preserves fixture setup/cleanup and CTest dependency ordering.

Splitting a CTest process changes the meaning of `RUN_SERIAL`: separate CTest
processes would otherwise execute a serial test concurrently. V1 therefore
places each `RUN_SERIAL` test alone in a `fleet_exclusive` shard. Aggregation
rejects any receipt interval in which that shard overlaps another shard. If a
serial test is coupled to another test by a fixture or dependency, the planner
refuses to shard it instead of guessing at semantics.

Resource locks remain explicit exclusion claims rather than hidden scheduler
behavior. Overlapping executed attempts with the same fleet lock are rejected
regardless of host. A host-scoped lock may overlap on different physical hosts
but is rejected on the same host. Fleet-exclusive and per-lock interval checks
use sorted sweeps and lock indexes rather than a quadratic all-attempts scan.
Interval timestamps are supplied through a separate controller-owned
observation, not worker JSON. They must come from the controller-comparable
clock contract used by the eventual dispatcher; until that transport exists,
these checks are shadow evidence only.

The inventory producer is not part of this slice. A production producer must
derive this metadata from the configured CTest graph without relying on
`ctest --tests-from-file` as an existence check: CTest silently ignores unknown
names there, and fixtures may add tests implicitly. The producer must compare
the executed inventory back to the canonical declared set.

## Authenticated assignment and retries

Only a controller-minted assignment authorizes a receipt. The assignment binds
the full manifest, inventory, plan, artifact, exact shard, topology-derived
mode, required execution boundary, authenticated host identity, sorted
capabilities, host-session generation, attempt, and fencing token. It carries
an HMAC-SHA256 computed with a controller key whose secret is never serialized
or printed.

The caller that constructs `AuthenticatedWorker` is the transport trust
boundary. The future transport must obtain those fields from an authenticated,
replay-protected channel; worker-supplied JSON is not sufficient. Receipt
acceptance checks that authenticated session against the signed assignment and
rejects reconnect-generation, host, identity, or capability drift.

The controller also HMAC-authenticates the complete accepted receipt. This is a
separate domain from assignment authentication. Aggregation and durable-store
writes verify it, so a worker that can read its assignment cannot bypass report
validation by constructing a receipt-shaped JSON object directly.

Attempts are immutable and contiguous from one for each shard. Fences must
increase for every retry and may not be reused. Aggregation selects only the
highest authenticated attempt. A late pass from an older attempt is retained
for audit but cannot complete the proof. Missing active-attempt receipts leave
the aggregate incomplete. Every accepted attempt, including stale attempts,
still participates in interval overlap checks so discarded results cannot hide
interference with an active exclusive or resource-locked execution. Executed
attempts for the same shard may never overlap, even when the shard has no
explicit resource lock.

Worker reports are not the lifecycle authority. Every issued assignment needs
a separately controller-authenticated terminal disposition: either `executed`
with a controller-owned interval or `fenced_before_start`. An executed attempt
can therefore constrain overlap even if its worker crashes or withholds its
report. An unclosed stale attempt keeps the aggregate incomplete; a retry does
not erase it.

## Worker input and untrusted artifacts

Worker reports are untrusted, strictly decoded, bounded, and reject unknown
fields. They must contain exactly one sorted outcome for every test in the
assigned shard, including an explicit `not_run` outcome when execution did not
complete. A complete pass requires every declared outcome to pass, the full
artifact digest to have been verified, and all immutable runtime observations
to match.

Execution interval, artifact-verification state, actual boundary, and teardown
state do not come from that JSON. `ControllerExecutionObservation` is a typed
caller boundary for controller-owned lease/transport telemetry. Production
transport must create it from trusted harness state; copying worker claims into
it would violate the contract.

An `untrusted_contributor` artifact is accepted only when the manifest declares
all of these:

- a disposable-guest execution boundary;
- no execution network; and
- no writable maintainer-host mounts.

No executed disposition or receipt is accepted until guest teardown is
confirmed. The module never executes, extracts, or inspects artifact content on
the controller host. Artifact transport and guest execution must preserve that
boundary.

## Deterministic aggregation

Aggregation validates every assignment and receipt again. It rejects unknown
attempts or dispositions, mixed manifest/plan/artifact bindings, conflicting
duplicates, non-contiguous retry histories, reused fences, topology overlap,
and structurally corrupt outcomes. Stale authorized receipts are auditable but
do not replace an active attempt. A passing result also requires a terminal
disposition for every issued attempt.

The caller must supply the complete controller-owned assignment, disposition,
and receipt corpus from durable storage. An arbitrary subset is not an
authoritative aggregation input. Production orchestration must enumerate that
corpus from its durable attempt ledger before calling this pure verifier.

The result is computed in shard-ID order regardless of arrival order:

- `passed` only when every declared shard has one active, valid, passing
  receipt;
- `failed` when any complete active shard failed;
- `incomplete` when an active receipt is absent; or
- `incomplete_and_failed` when both facts are present, so neither a known
  failure nor an unknown execution interval is hidden.

The aggregate binds sorted active-assignment, every terminal-disposition, and
active-receipt digest. Extra or conflicting receipts are errors, never ignored
success. A passing aggregate still has no merge authority in schema v1.

## Crash durability and idempotence

The controller-local store uses a hashed filename for every validated logical
key, so an input key cannot traverse directories. Each record is wrapped with
its kind, logical identity, payload, and payload digest. Reads are bounded and
revalidate the envelope and record semantics.

The store root's parent must already exist as a real directory. Shipyard creates
only the final root component and syncs that parent, avoiding a false durability
claim for unsynced ancestors created recursively. Kind directories are likewise
created one level at a time and their parent is synced.

Writes follow this sequence under a per-record exclusive lock:

1. encode the complete immutable envelope;
2. write a temporary file in the destination directory;
3. `sync_all` the temporary file;
4. publish without clobbering an existing destination; and
5. sync the destination directory on platforms that expose directory fsync.

The same logical key plus identical bytes is idempotent. Different bytes at
the same logical key are an immutable conflict and are never overwritten.
Crash-injection tests cover the points after temporary-file sync and after
publication but before directory sync; restart observes either no record or a
complete digest-valid record and an identical retry completes the durability
barrier.

The store root must be controller-owned and not writable by workers. The core
rejects non-regular destination and lock paths, but operating-system ownership,
disk isolation, backup, retention, and replication are deployment concerns.

## Acceptance coverage in this slice

Unit tests exercise:

- deterministic exhaustive/disjoint partitioning of exactly 20,702 tests;
- missing, duplicate, unknown, and digest-tampered plan members;
- dependency cycles and cross-shard dependency/fixture rejection;
- `RUN_SERIAL` isolation and overlap rejection;
- host- and fleet-scoped resource-lock behavior;
- immutable source/build/artifact/trust binding and untrusted guest teardown;
- HMAC tampering, capability mismatch, host-session drift, and report tampering;
- bounded/strict JSON input;
- retry ordering, stale-attempt fencing, and attempt gaps;
- bounded retry/corpus cardinality and non-quadratic overlap validation;
- missing-report attempt dispositions and stale-execution overlap;
- order-independent aggregation plus incomplete, failed, unknown, mixed, and
  conflicting receipt cases;
- concurrent identical and conflicting immutable writes;
- crash injection, reopen/retry, and on-disk tamper detection; and
- single-level crash-durable store-root creation; and
- the invariant that neither a manifest nor aggregate can satisfy merge
  readiness.

## Required production follow-ups

This slice is a prerequisite, not the performance feature. Queue time improves
only after all of the following are implemented and measured:

1. A canonical inventory producer from the configured CTest graph, including
   explicit classification for every resource lock and runtime capability.
2. Build-once production on an isolated trusted builder, content-addressed
   artifact publication, and safe guest-only transfer for untrusted artifacts.
3. Authenticated, replay-protected transport for controller sessions,
   assignments, logs, and reports, plus durable controller-key retention and
   rotation rules for the complete lifetime of a proof.
4. Durable universal job dispatch with leases, cancellation, fencing, retry,
   host-loss recovery, and scheduling that enforces exclusive/lock constraints
   before execution rather than merely detecting violations afterward.
5. Dynamic placement across M1/M3/M5 based on measured shard cost and declared
   capabilities, with backpressure so parallel proof does not starve unrelated
   queue work.
6. Reuse rules that prove repository, exact subject head/tree, build contract,
   artifact, inventory, plan, trust policy, and runtime identity are unchanged.
7. Merge-group orchestration that creates a new proof for the exact
   `merge_group` head SHA and publishes a bounded GitHub Check Run only after a
   production-authoritative aggregate.
8. Shadow comparison against the existing full-suite gates, including failure
   injection, host loss, power loss, artifact tampering, and enough history to
   establish equal-or-better false-green behavior.

The largest latency wins should come from one full build per exact subject,
cost-balanced test shards running concurrently on available machines, and
durable jobs that resume/reassign only missing attempts. None of those wins may
be claimed from the invariant module by itself.
