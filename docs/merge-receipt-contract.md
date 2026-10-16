# Merge receipt contract (`shipyard-merge-receipt/v1`)

The fleet scorecard may report `safely_merged_changes` and
`wasted_compute_per_safely_merged_change` only after importing a durable
producer receipt. A PR count, `merged_at`, or `merge_commit_sha` from GitHub is
not sufficient: those values do not prove that the exact validated PR head
passed Shipyard's required gates before the protected branch accepted it.

Each JSON object (or JSONL line) has these required fields:

- `schema`: exactly `shipyard-merge-receipt/v1`.
- `receipt_id`: stable immutable identity.
- `repo`, `project`, `base_branch`, and positive `pr`.
- `head_sha`, `validated_head_sha`, and `merge_commit_sha`: 40-character SHAs;
  the first two must match exactly.
- `merged_at`: RFC3339 timestamp.
- `safe_merge`: must be `true`.
- `source`: must be `shipyard`; this records producer identity but does not cryptographically authenticate a GitHub
  export as an authority.
- `required_checks`: non-empty exact required-check names that passed for the
  validated head.
- `gate_receipt_sha256`: nonzero lowercase SHA-256 of the immutable gate
  receipt containing those results.
- `evidence`: immutable evidence hash or URL for the gate/merge proof.

`shipyard metrics import merge-receipt --file <path>` validates every record
before writing anything. Imports are transactional. Re-importing the same
`receipt_id` is idempotent only when its evidence matches; conflicting evidence
fails closed. `-` or an omitted file reads stdin.

The SQLite store keeps receipts in `merge_receipts`, keyed by `receipt_id`.
Scorecards count distinct imported safe `head_sha` values in the requested
window and join failed/cancelled job compute only by the receipt's exact
`repo`, exact `head_sha`, and PR number. Duplicate receipts for a head are deduplicated;
conflicting records for one repository and PR are rejected. Receipts
without a matching completed job identity produce `partial` or `unavailable`
coverage rather than a zero-cost claim. Distinct PR throughput is never used
as a substitute.

The current GitHub API importer does not produce these receipts because it has
no Shipyard gate-evidence provenance. A future producer must emit this schema
at the protected merge boundary, after exact-head and gate receipt checks.
Until then, the honest state is unavailable.
