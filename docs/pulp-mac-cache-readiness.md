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
session generation, selected pinned route, sorted capabilities, persistent staging
root and reserve, adapter-verified terminal-instance digest, exact companion
executable, and expected immutable manifest. The response and carrier-origin
request/response byte counters, digests, monotonic route-probe times, fallback
classification, and exchange round-trip time are retained in the receipt.
Freshness uses the M3 controller timestamp taken after the exchange; M1's wall
clock is retained only inside the authenticated response binding and cannot
make healthy evidence stale through cross-host clock skew.
`shipyard-workstream-provider --observe-m1-cache`
implements the bounded read-only M1 endpoint and verifies its own executable
digest before opening the cache.

`StrictSshRemoteM1CacheTransport` is the production carrier from M3. It requires
explicit current-user-owned mode-0600 identity authority and locked known-host
authority, clears the environment, disables SSH config, agent, control socket,
proxy, forwarding, and host-key update paths, and carries the canonical request
only on bounded stdin. It measures the pinned direct-LAN route first and may
measure/use an independently pinned Tailnet fallback only after a classified
transport failure. It never consults ambient SSH state and never redispatches a
request interrupted after the companion boundary. The controller must construct
the authority from protected host/session/terminal receipts, never companion or
worker claims.

Tailnet evidence remains useful for cache hit/miss, byte, and latency diagnosis,
but cannot close the direct-LAN or worker-session readiness gates. Local SSH
authority errors and authenticated remote refusals never trigger fallback.
Missing, stale, insufficient-reserve, capability-mismatched, detached, or
counter-mismatched evidence fails before it can close any M1 gap.

Valid paired cache evidence closes the cache gap and the exact remote M1 gates
it actually proves. The authenticated M1 capability inventory is retained but
does not close capability readiness until a selected proof inventory supplies
the workload-specific requirements to compare. M3 session/capability authority,
complete transfer evidence, and M3-control-first shadow execution remain
independent gates, so this library tranche still cannot make the canary
eligible or execute it. It never trusts `claimed_bytes_avoided`.

The protocol, role router, and strict authenticated M3-to-M1 observation carrier
are implemented. There is no main-app execute flag, daemon activation, cache
population, replacement, deletion, release, deployment, or live host mutation
in this tranche.
