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
portable modes, exact byte sizes, and file SHA-256 values. Host-local root paths are kept in observation
receipts rather than the portable generation identity, allowing M3 and M1 to
prove the same contents at different persistent roots. Each cache receipt also
binds the digest of the authenticated host observation that authorized it;
detached or stale host/cache combinations cannot close the readiness gap.
Routine observation records `model_calls=0`.

`drive_pulp_mac_cache_probe` is disabled unless its request explicitly opts in.
When enabled, it proves every required M3 generation before it invokes the M1
observer, rejects stale or incomplete inventories, and publishes the paired
receipt through a crash-durable no-overwrite controller store. Repeating an
identical correlation ID returns the stored bytes; conflicting bytes fail.

Valid paired cache evidence closes only the dry-run controller's cache gap.
Authenticated session generations, the direct M3-to-M1 LAN route,
capabilities, persistent staging roots, storage reserve, transport evidence,
and M3-control-first canary execution remain independent gates. The cache
foundation cannot return an eligible canary by itself and never trusts
`claimed_bytes_avoided`.

The production observer currently owns only local read-only trees. A future
companion/transport adapter must execute the same observer on M1 and return its
typed receipt over an authenticated host/session channel. That adapter is not
implemented here: there is no app CLI, daemon activation, cache population,
replacement, deletion, release, deployment, or live host mutation in this
tranche.
