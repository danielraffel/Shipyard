# Merge-receipt handoff supervision

The protected merge producer stores validated `shipyard-merge-receipt/v1`
records in its local metrics database. The baseline observer must receive an
explicit export; it must never manufacture a safe merge from GitHub metadata.

Run the producer-side handoff wrapper from the machine that owns that database:

```sh
python3 scripts/merge_receipt_handoff.py export \
  --shipyard "$(command -v shipyard)" \
  --state-dir "$HOME/Library/Application Support/shipyard" \
  --repo Generous-Corp/pulp \
  --base-branch main \
  --file "$HOME/Library/Application Support/shipyard/merge-receipts.jsonl" \
  --health-file "$HOME/Library/Application Support/shipyard/merge-receipts.health.json"
```

The wrapper calls only `shipyard metrics export merge-receipt`, which is
repository-scoped, local, atomic, and read-only with respect to GitHub. It
writes a separate atomic health sidecar containing the export time, producer identity, latest merged timestamp,
record count, byte count, and SHA-256 of the exact JSONL payload. Export failure records `status=error` and never
claims a fresh handoff or replaces a previously successful handoff.

Install a supervised per-user macOS job with a dry-run first:

```sh
scripts/install_merge_receipt_handoff.sh --repo Generous-Corp/pulp
scripts/install_merge_receipt_handoff.sh --repo Generous-Corp/pulp --install
```

The installer copies the wrapper into the managed Shipyard application-support
directory before loading launchd, so the job does not depend on the checkout
that installed it. It requires an explicit repository and absolute paths; it
does not copy credentials or alter queue/merge authorization.

## Remote producer and observer

When the protected merge database lives on another fleet host, the observer
can fetch the producer export through the managed SSH alias. Run this on the
observer (for example, m1):

```sh
python3 scripts/merge_receipt_handoff.py fetch \
  --source-host m3 \
  --source-shipyard /Users/danielraffel/.local/bin/shipyard \
  --source-state-dir '/Users/danielraffel/Library/Application Support/shipyard' \
  --repo Generous-Corp/pulp \
  --base-branch main \
  --file '/Users/danielraffel/Library/Application Support/shipyard/merge-receipts.jsonl' \
  --health-file '/Users/danielraffel/Library/Application Support/shipyard/merge-receipts.health.json'
```

`fetch` runs the existing repository-scoped export on the named producer host,
reads the producer's authenticated `imported_at` timestamp from its local
read-only metrics database, and transfers one validated JSON envelope over
SSH. The observer validates every receipt, producer identity, timestamp,
repository, and base branch before atomically replacing its local handoff.
Transport or validation failure writes an `error` health sidecar and preserves
the previous payload. It never calls GitHub, creates a receipt, changes queue
state, or authorizes a merge. The source executable and state paths must be
absolute; the host is a plain managed SSH alias.

A monitoring agent can fail closed when the producer stops exporting:

```sh
python3 scripts/merge_receipt_handoff.py check \
  --file "$HOME/Library/Application Support/shipyard/merge-receipts.jsonl" \
  --repo Generous-Corp/pulp \
  --base-branch main \
  --health-file "$HOME/Library/Application Support/shipyard/merge-receipts.health.json" \
  --stale-after 2h
```

`check` exits `0` for a valid, recent handoff and `3` for a missing,
malformed, failed, empty, stale, tampered, duplicate, or conflicting handoff.
The baseline collector requires and verifies this sidecar when
`--merge-receipt-health` is supplied, preserving an unavailable denominator
instead of importing stale data. This status is observational only;
it cannot authorize, undo, or infer a merge.

The producer and observer bind each receipt to the explicit protected base
branch (default `main`) and repository basename. Freshness requires both the
handoff export and the producer store's newest `imported_at` timestamp
(`freshness_basis=handoff_export_and_source_import`), not the age of a
historical receipt. `exported_at` is the canonical handoff-generation time,
and `stale_after_seconds` binds the producer freshness window to the observer
policy; a mismatch is rejected.
