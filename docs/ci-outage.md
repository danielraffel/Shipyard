# Private outage records

The private labels archive may contain `ci.outage/v1` rows. A v1 row carries
`member_keys`, where each key is the canonical immutable job identity
`owner/repo|run|attempt|job`. These keys are the only supported join between an
outage window and another record. Branch names, display names, and timestamps
are evidence, not identity.

Rows from before `member_keys` was introduced remain in the archive, but are
reported as unavailable. Readers must not reconstruct membership from their
aggregate counts or time ranges. A v1 row is also unavailable when its keys
are duplicated or malformed, its timestamps are not RFC3339, or its collector
provenance is incomplete.

The record is private and advisory. This schema does not alter cause advice,
merge decisions, queue admission, or any CI result.
