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
`census_sha256` retain the producer's evidence digests. The importer rejects
unknown fields, bad digests, mismatched slot counts, stale/future timestamps,
and intervals longer than one hour. Re-importing a valid `sample_id` is
idempotent.

For a verified sample, `eligible_slots = free_slots + occupied_slots +
in_flight_slots` and `configured_slots = eligible_slots + unknown_slots`.
Unknown samples set all four capacity optionals to null and
`unknown_slots = configured_slots`; they never advertise capacity.

Samples are observational only. They do not route jobs, grant leases, open
admission, or authorize a merge. `verification = unknown` remains visible and
never becomes zero capacity.

The scorecard exposes `fleet_capacity` with verified and unknown sample counts
and `eligible_slot_hours`. Unknown samples make the denominator `partial`; no
overlapping receipt intervals are double-counted for a host and target. With
no overlapping samples the denominator is `unavailable`. The normal metrics
retention setting removes old capacity samples along with raw job samples;
merge receipts remain durable provenance.
