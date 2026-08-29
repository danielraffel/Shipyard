# Immutable artifact transport proof

Shipyard contains a pure, **default-off** artifact transport proof core in
`artifact_transport`. It does not dispatch builds, select workers, change fleet
configuration, or make sharding authoritative. Those behaviors require a
separate reviewed integration and live canary.

## Trust and identity

An artifact manifest binds all of the following before bytes can be published:

- repository, exact Git head, and exact Git tree;
- platform, architecture, build type, toolchain digest, test-inventory digest,
  and optional golden-image digest;
- encoded artifact size and SHA-256, fixed-size chunk SHA-256 values, and the
  digest of the sorted unpacked layout;
- immutable cache names, generations, and digests (for example Skia or Dawn);
- producer worker, lease, generation, and attempt.

Unknown manifest fields, unsorted or duplicate entries, unsafe relative paths,
chunk gaps/overlaps, missing required cache generations, and stale authority
fences fail closed. A cache path alone is never a cache hit: the generation and
digest must match.

## Receiver-pull and interruption recovery

The receiver constructs `rsync` argv directly, without a shell. The executable,
remote host, remote store root, local store root, session, and digest are
validated; both remote object and local partial paths are derived rather than
caller supplied. Remote roots deliberately accept only a narrow portable
character set because classic rsync still passes the remote path through a
remote shell.

Before constructing that command, the receiver acquires an exclusive OS-backed
lease for the exact digest and transfer session. The same lease value must stay
alive through the receiver-pull process and is consumed by publication. A
crashed process releases the kernel lock automatically; its lock file remains
so a pathname unlink cannot split later contenders across different lock
inodes. Publication first moves the partial to a sealed path while ownership is
held, so a second cooperating receiver cannot append during verification.

macOS `/usr/bin/rsync` (openrsync) does not provide `--append-verify`. Therefore
plain `--partial` is not considered resumable proof, and blind `--append` is
forbidden. Shipyard hashes each complete prefix chunk, truncates an incomplete
or corrupt tail to the last verified boundary, and only then permits `--append`.
The resume plan is opaque and bound to the exact manifest digest, transfer
session, leased partial path, and observed length. Applying it re-reads the
partial and refuses drift; receiver-pull command construction accepts only the
applied plan and checks the prepared length again. Callers cannot construct an
append boundary or reuse a plan from another transfer. A fresh lease with no
partial produces an authenticated restart plan; applying that plan atomically
creates the empty receiver file, so callers never need to manufacture
undocumented staging state.
After transfer, it rechecks every chunk, total size, final SHA-256, manifest
authority, and producer fence.

Verified objects move from `<root>/.incoming` to `<root>/objects` with a
same-root, atomic create-if-absent hard link. There is no existence-check/rename
window: one concurrent publisher wins, and every loser verifies the immutable
winner before reuse. Validation failure restores the resumable partial;
post-verification publication failure retains the sealed bytes for diagnosis or
retry. An existing immutable object is reused only after its size and digest
match; it is never overwritten.

Publication authenticates the encoded object, but encoded-object identity alone
does not authorize unpacking. Before extraction, `verify_archive_layout`
decodes the `tar.zst` without writing files and requires every archive member to
match the manifest's complete sorted layout. Raw tar iteration rejects extension
records before their payloads can be buffered, and also rejects traversal,
absolute or non-portable paths, links and special files, duplicates, undeclared
or missing members, hidden post-archive data, directory payloads, and
type/mode/size/digest mismatches. Portable identity is ASCII case-folded at
every component prefix, and Windows device aliases and trailing-period names
are rejected, so authenticated paths cannot collapse together on default APFS
or Windows filesystems. The entry count and zstd decoder window are bounded.
Terminal zero padding accepts the standard twenty-record tar block (10 KiB)
and rejects non-zero or larger tails, so common tar producers interoperate
without turning compressed padding into an unbounded decode path.
Schema 2 requires explicit parent-directory records. Schema-1 manifests remain
readable when they satisfy the current bounded portable-path policy, so ordinary
in-flight immutable artifacts survive an upgrade; an oversized or newly unsafe
legacy layout fails closed and must be republished rather than weakening the
receiver for backward compatibility.
`extract_verified_archive` first requires the current exact manifest/source/
producer authority fence, then repeats validation into a private
sibling staging directory, rechecks the encoded object for mutation, holds an
OS-backed parent extraction lease, and atomically renames the complete tree
into a previously absent destination. Its caller supplies the free-space
reserve policy; Shipyard checks the overflow-safe sum of declared file bytes
plus conservative per-file and per-directory allocation reserves before
staging, then rechecks live space before and after every allocation (including
schema-1 implicit parents). Concurrent disk use therefore fails closed and
discards private staging.
Restrictive directory modes are deferred until every fallible verification has
finished; atomic no-replace publication cannot overwrite a destination created
by another process, and restores traversable staging permissions before cleanup
if publication loses that race. Extraction returns an explicit durability
outcome after the rename commit point: a parent-sync failure means the complete
destination is already visible and must be reconciled, not blindly retried as
an unpublished failure. A failed, partial, or competing layout never becomes a
consumable destination.

The host declares the store root. Do not assume `/Volumes/Workshop` or a home
directory: a machine with a nearly-full external volume may correctly choose a
root-volume staging directory, while other machines may be root-only. Apply a
free-space watermark to both the remaining encoded transfer bytes and the
declared unpacked extraction bytes before starting, and preserve that watermark
through extraction.

## Promotion boundary

Before any scheduler or shard integration is enabled:

1. Run a receiver-pull canary only while the governor reports free capacity.
2. Measure LAN and tailnet route, setup time, effective throughput, resumed
   bytes, final digest, and transfer-to-shard-time ratio.
3. Prefer exact cache-generation references (zero copy), then basis-aware Git
   object fetch for source, then compressed immutable artifacts. Never copy a
   whole repository, build tree, or heavyweight shared cache by default.
4. Admit transfer only when expected benefit is at least 120 seconds or 10%,
   and transfer time is no more than 15% of expected shard work.
5. Exclude or reassign a roaming worker before dispatch when it is offline,
   not on an approved route, lacks an exact cache generation, violates the
   space watermark, or cannot finish transfer inside the benefit budget. Loss
   after dispatch must expire its lease and reassign unfinished shards; another
   worker may reuse only immutable, fully verified objects.

M5 must never be a required shard while roaming. Its availability is additive,
not part of the minimum completion set.

## Measurements, logs, and retention

`parallel_proof_canary_receipt` defines the compact, shadow-only measurement
record. It binds the complete proof-manifest digest, numeric repository ID and
slug, Shipyard target and build target triple, repository head/tree, encoded
artifact and layout digests, exact builder
and worker observations plus session generations, authenticated LAN route, full/resumed/object-
reuse byte accounting, and exact cache generations. The legacy untrusted
avoided-byte field is canonically zero; it is never accepted as measurement. Setup/
transfer/verification/dispatch/shard/worker timings, submit-to-receipt wall
clock, and the digest of a separately validated same-proof single-host control
receipt. Routine canaries require
`model_calls=0`. The receipt reports the 120-second-and-10-percent speed floor
and 15-percent transport-overhead ceiling without becoming merge authority.
The speed gate consumes only controller timing and transport byte counters.

`parallel_proof_canary_driver` is the default-off controller execution seam. It
first authenticates the exact configured builder/worker observations, completes the builder control,
rechecks both session fences and the storage reserve, then permits transfer and
distributed shadow execution. It rechecks the fences and reserve again before
publishing schema-v1 driver evidence. The exact pre-execution and final host
observations are retained alongside recomputable digests. Before any transfer
or shard work the store publishes an immutable `distributed_started` record;
post-start failures publish a separate immutable failure record, and an
unreconciled started/failed correlation is never retried automatically. Resume evidence includes partial and
verified-prefix digests plus pre-interruption, retained-prefix, and suffix-byte
counters. Exact cache generations and use are recorded, while avoided-byte
claims are not representable at the adapter boundary. `model_calls` is supplied
by neither policy nor adapter and is always zero.

The execution driver now has a production-callable, shell-free adapter
protocol. `shipyard parallel-proof-canary --request <absolute-private-json>` is
a read-only plan with no adapter execution or state mutation by default; `--apply` additionally requires both
`activation_enabled=true` and `apply_enabled=true` in the trusted
machine-global `[parallel_proof_canary]` table. The table pins one normalized
absolute executable path, its exact SHA-256, deadline and output limits, plus
the exact repository numeric ID/slug, target/triple, and builder/worker IDs.
It also pins `invocation_authority_sha256`, the domain-separated digest of the
complete reviewed policy, timing thresholds, manifest, inventory, and plan;
changing any execution or safety input requires a new reviewed activation.
Project and checkout-local configuration cannot enable or widen it.

Every adapter invocation uses cleared environment, no shell or arguments, a
native-only executable snapshot rehashed from a no-follow source descriptor
and kept in a fresh owner-private directory with descriptor/path identity
checked immediately before spawn, and
bounded stdin/stdout/stderr under a process-tree deadline. Strict JSON requests
bind the exact correlation ID, proof-manifest digest, complete configured
scope, complete operation payload digest, payload-derived idempotency key, and
zero model calls. Strict responses must echo that authority, payload, and operation and return typed host,
same-host control, or distributed transfer/execution evidence. The controller
driver still validates every receipt, records `distributed_started` before
physical work, refuses unreconciled retries, and owns immutable publication.

The separate cache-observation path has a production-callable strict-SSH
builder-to-worker carrier whose host identities are explicit constructor inputs,
but it is read-only, requires protected controller authority, rejects
ambient SSH state, and cannot execute transfer/shard work. The general adapter
executable must implement the physical host-specific operations; Shipyard core
contains no Pulp commands or personal host defaults. Therefore setting
`policy.enabled=true` alone still cannot mutate a host, cache, or staging root.

Successful driver evidence is published through
`PulpMacCanaryEvidenceStore`, a controller-owned crash-durable no-overwrite
store. The canary and cache evidence stores share one descriptor-pinned,
no-follow, owner-private 0700/0600, single-link, atomic no-replace publication
primitive with file and directory fsync. Byte-identical replay is idempotent
and a conflicting correlation id is refused. Never
log credentials, private paths, or raw rsync environment. Failed and reassigned
attempts still need bounded transition records; they must not be rewritten into
a successful compact receipt.

### Physical canary prerequisites and command boundary

The command boundary is present, but an installed digest-pinned adapter and
reviewed machine-global policy are still required before a physical canary.
That executable must supply all of the following from authenticated APIs, not
operator-entered JSON: exact configured host identities and nonzero session
generations; current online/LAN route observations; canonical persistent staging
roots; filesystem free-byte observations that retain the configured reserve;
exact cache generation digests; monotonic phase timings; transport byte
counters; authenticated partial/prefix digests; and compact execution receipts.

Plan without adapter execution or state mutation:

```sh
shipyard --json parallel-proof-canary --request /absolute/private/invocation.json
```

Apply only after installing and digest-pinning a reviewed adapter:

```sh
shipyard --json parallel-proof-canary --request /absolute/private/invocation.json --apply
```

Focused proof commands are:

```sh
cargo test --locked --lib parallel_proof_canary_driver
cargo test --locked --lib parallel_proof_canary_receipt
cargo clippy --locked --all-targets --all-features -- -D warnings
```

Do not substitute ad-hoc `ssh`, `rsync`, claimed avoided bytes, or
model-generated monitoring for the protected adapter.

Terminal success or actionable failure may later be offered to the separate
transactional wake-delivery subsystem using the immutable correlation and
receipt digests. This command does not type into cmux/HerdR, guess a target by
label, or implement a chat bus. Cross-machine wake custody requires its own
source outbox, destination atomically persisted inbox, exact target
incarnation/delivery fence, single CAS/lease owner, acknowledgements,
expiry/revalidation, successor/rebind proof, duplicate suppression, and
restart/offline-rejoin canaries. A busy or nonempty composer is never an
authorized delivery target.

Rotate transfer logs with Shipyard's bounded log-retention primitives. Keep the
terminal receipt and compact metrics longer than verbose transport logs; retain
failed partial metadata only within an explicit byte/age budget. Publication
receipts are immutable evidence and must not be rotated as ordinary debug logs.
