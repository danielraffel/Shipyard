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
After transfer, it rechecks every chunk, total size, final SHA-256, manifest
authority, and producer fence.

Verified objects move from `<root>/.incoming` to `<root>/objects` with a
same-root, atomic create-if-absent hard link. There is no existence-check/rename
window: one concurrent publisher wins, and every loser verifies the immutable
winner before reuse. Validation failure restores the resumable partial;
post-verification publication failure retains the sealed bytes for diagnosis or
retry. An existing immutable object is reused only after its size and digest
match; it is never overwritten.

The host declares the store root. Do not assume `/Volumes/Workshop` or a home
directory: a machine with a nearly-full external volume may correctly choose a
root-volume staging directory, while other machines may be root-only. Apply a
free-space watermark to the remaining transfer bytes before starting.

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

## Logs and retention

Future integration must write bounded structured records containing artifact
and manifest digests, source head/tree, producer fence, source/destination host,
LAN or tailnet route, cache hits/misses by generation, bytes total/reused/sent,
verified resume offset, setup/transfer/verification duration, free-space
observation, outcome, and reassignment reason. Never log credentials or raw
rsync environment.

Rotate transfer logs with Shipyard's bounded log-retention primitives. Keep the
terminal receipt and compact metrics longer than verbose transport logs; retain
failed partial metadata only within an explicit byte/age budget. Publication
receipts are immutable evidence and must not be rotated as ordinary debug logs.
