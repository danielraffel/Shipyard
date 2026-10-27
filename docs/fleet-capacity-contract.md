# Fleet-capacity receipt contract (`tartci.fleet-capacity/v1`)

`shipyard metrics import fleet-capacity --file <path>` imports TartCI's
read-only capacity observations. The input may be one JSON object, a JSON
array, or JSONL. Every record is validated before any row is written.

The receipt binds `repo`, `target`, `host_id`, `host_boot_id`, `generation`,
and `profile_sha256` to one observation. `evidence` is the required
`tartci.gate-supply/v1` object; its host, repository, target, and profile
digest must match the enclosing receipt. `auth_method` is the producer's
explicit `untrusted_local` observation marker. `sample_id` is the SHA-256 of
the canonical record without `sample_id`; `evidence_sha256` and
`census_sha256` retain the producer's evidence digests. Canonical bytes are
compact, recursively key-sorted UTF-8 JSON with matching number spelling in
the Python producer and Rust importer. The importer rejects unknown fields,
bad digests, mismatched slot counts, future timestamps, and intervals longer
than one hour. Historical observations remain importable for backfills.
Re-importing a valid `sample_id` is idempotent.

For a verified sample, `eligible_slots = free_slots + occupied_slots +
in_flight_slots` and `configured_slots = eligible_slots + unknown_slots`.
Unknown samples set all four capacity optionals to null and
`unknown_slots = configured_slots`; they never advertise capacity. A producer
failure is still an explicit unknown receipt with the required identity,
evidence, and digests, so it can be imported and counted as an observation gap.

Verified samples require evidence with `verdict = "ok"` and `pool = "on"`.
The embedded pool, lane count, and free/occupied/in-flight counts must match
the enclosing receipt.

Samples are observational only. They do not route jobs, grant leases, open
admission, or authorize a merge. `verification = unknown` remains visible and
never becomes zero capacity.

The scorecard exposes `fleet_capacity` with verified and unknown sample counts
and `eligible_slot_hours`. Unknown samples make the denominator `partial`; no
overlapping receipt intervals are double-counted for a repository, host, and
target. Coverage gaps also make the denominator `partial`. With no overlapping
samples the denominator is `unavailable`. The normal metrics
retention setting removes old capacity samples along with raw job samples;
merge receipts remain durable provenance.

The scorecard's primary throughput metric,
`safely_merged_changes_per_eligible_slot_hour`, divides distinct safe merge
receipt heads by `eligible_slot_hours`. It is unavailable without both durable
safe merge receipts and positive measured capacity. A partial capacity
denominator produces a partial result and an explicit upper-bound reason;
unknown capacity is never treated as zero or silently omitted.
