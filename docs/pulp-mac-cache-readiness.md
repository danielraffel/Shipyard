# Pulp macOS cache-readiness foundation

The default-off Pulp macOS canary treats a cache generation as immutable
content, not as a mutable directory name or a claimed byte saving. Shipyard
produces a portable manifest by opening a configured local cache root through
no-follow directory handles, hashing every regular file, and observing the
complete tree a second time. Links, special files, hard links, concurrent
changes, temporary roots, malformed paths, and unsupported platforms fail
closed.

The manifest binds the stable cache name and policy generation to a sorted
relative-path inventory covering the cache root and descendant directories,
portable modes, exact byte sizes, and file SHA-256 values. Host-local root
paths are kept in observation receipts rather than the portable generation
identity, allowing M3 and M1 to
prove the same contents at different persistent roots. Each cache receipt also
binds the digest of the authenticated host observation that authorized it;
detached or stale host/cache combinations cannot close the readiness gap.
Routine observation records `model_calls=0`.

`drive_pulp_mac_cache_probe` is disabled unless its request explicitly opts in.
When enabled, it proves every required M3 generation before it invokes the M1
observer, rejects stale or incomplete inventories, and publishes the paired
receipt through a crash-durable no-overwrite controller store. Repeating an
identical correlation ID returns the stored bytes; conflicting bytes fail.

An M1 receipt is accepted only through `RemoteM1CacheTransport`. Its strict
companion request binds the already authenticated host observation, nonzero
session generation, direct-LAN route, sorted capabilities, persistent staging
root and reserve, adapter-verified terminal-instance digest, exact companion
executable, and expected immutable manifest. The response and carrier-origin
request/response byte counters, digests, and monotonic round-trip time are
retained in the receipt. `shipyard-workstream-provider --observe-m1-cache`
implements the bounded read-only M1 endpoint and verifies its own executable
digest before opening the cache.

There is deliberately no direct SSH or ambient-configuration implementation of
that transport. A deployed controller adapter must carry the request over the
existing authenticated companion/terminal channel and construct authority from
protected controller receipts, never companion or worker claims. Missing,
stale, Tailnet, insufficient-reserve, capability-mismatched, detached, or
counter-mismatched evidence fails before it can close the M1 gaps.

Valid paired cache evidence closes the cache gap and the exact remote M1 gates
it actually proves. The authenticated M1 capability inventory is retained but
does not close capability readiness until a selected proof inventory supplies
the workload-specific requirements to compare. M3 session/capability authority,
complete transfer evidence, and M3-control-first shadow execution remain
independent gates, so this library tranche still cannot make the canary
eligible or execute it. It never trusts `claimed_bytes_avoided`.

The protocol and role router are implemented, but the physical authenticated
M3-to-M1 carrier is a separately deployed adapter and remains required. There
is no main-app execute flag, daemon activation, cache population, replacement,
deletion, release, deployment, or live host mutation in this tranche.
