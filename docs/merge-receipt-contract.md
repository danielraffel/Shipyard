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
window. A receipt joins only when completed jobs exist for its exact `repo`,
exact `head_sha`, and PR number. A joined change's wasted compute is every
failed or cancelled job of that pull request, earlier pushes included, so
`wasted_compute_per_safely_merged_change` is per change, not per merged head.
Runs are matched by PR number, so merge-group runs, which carry none, are not
counted, and the waste query is not limited to the scorecard window: a change
merged inside the window counts every failed or cancelled job of its pull
request, including jobs before the window began. Duplicate receipts for a head
(different `receipt_id`s) count once; conflicting records for one repository
and PR are rejected. Receipts without a matching completed job identity
produce `partial` or `unavailable` coverage rather than a zero-cost claim, and
an `unavailable` row carries no value. Distinct PR throughput is never used as
a substitute.

Shipyard emits this schema at the protected merge boundary after exact-head and
gate receipt checks and a follow-up GitHub read confirms that the PR merged.
Native merge-queue merges and manual maintainer merges are eligible when
durable ship state records the exact validated head, a live validation attempt,
and terminal-green required-gate evidence. A merge with no such evidence
receives no safe-merge receipt. The producer is observational: a receipt-store
failure is surfaced as a cleanup warning after the merge and never authorizes
or undoes the merge.
The GitHub API importer remains read-only and does not manufacture receipts.

For the Pulp merge-group reuse path, `shipyard metrics import
github-merge-receipt` consumes a normalized `job.json`, one workflow-run and
pull-request snapshot, and a complete check-runs snapshot (`complete: true`).
The command requires the observed run URL and exact queue-ref identity to bind
the artifact to the PR; it accepts GitHub's normal empty `pull_requests` array
and accepts the artifact's `refs/heads/` spelling when the normalized run uses
the corresponding bare branch name. The run projection also carries the exact
merge-group commit's ordered parent SHAs. The importer requires exactly two
parents, with the protected base SHA first and the artifact PR head second, so
batched groups with a synthetic second parent remain unavailable.
and requires each selected check to carry non-null `run_id`/`run_attempt`
matching the observed workflow run. A commit-wide check-runs response with
null per-check identity is rejected unless an upstream collector first
enriches it from the authoritative run jobs. The normalized check snapshot
also carries a positive `snapshot_run_id` and `snapshot_run_attempt` matching
the observed workflow run; that binding is included in the gate evidence hash.
Partial pages, duplicate required checks, ambiguous multi-PR arrays, mismatched
URLs, and unknown snapshot fields remain unavailable rather than becoming
receipts.

The run projection must include `pull_requests`, even when GitHub supplies the
legitimate empty array for a merge-group run. The check projection must include
`total_count`, accepted only when it equals the captured `check_runs` length
after pagination. These are normalized, schema-defined inputs; the importer
intentionally rejects raw GitHub objects with unrelated fields. The artifact
URL is a caller-provided locator and is not proof of artifact object identity;
the SHA-256 of the supplied `job.json` bytes is the content binding recorded in
the receipt. The Pulp `job.json` projection is the one exception for producer
metadata: extra producer fields are ignored after the nine binding fields are
strictly normalized, including a decimal-string `run_id`.

The required-check list is not trusted merely because it was supplied on the
command line. The caller must provide a normalized
`shipyard.required-check-policy/v1` branch-protection/ruleset snapshot whose
repository, protected base branch, and exact ordered check list match the
reconciliation arguments. `--policy-sha256` is the SHA-256 of that canonical
snapshot. A missing, mismatched, or unverifiable policy refuses the receipt;
safe-merge metrics never use a caller-selected subset of checks.
