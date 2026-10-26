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
  --file "$HOME/Library/Application Support/shipyard/merge-receipts.jsonl" \
  --health-file "$HOME/Library/Application Support/shipyard/merge-receipts.health.json"
```

The wrapper calls only `shipyard metrics export merge-receipt`, which is
repository-scoped, local, atomic, and read-only with respect to GitHub. It
writes a separate atomic health sidecar containing the export time, count, and
latest merged timestamp. Export failure records `status=error` and never
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

A monitoring agent can fail closed when the producer stops exporting:

```sh
python3 scripts/merge_receipt_handoff.py check \
  --file "$HOME/Library/Application Support/shipyard/merge-receipts.jsonl" \
  --health-file "$HOME/Library/Application Support/shipyard/merge-receipts.health.json" \
  --stale-after 2h
```

`check` exits `0` for a valid, recent handoff and `3` for a missing,
malformed, failed, empty, or stale handoff. This status is observational only;
it cannot authorize, undo, or infer a merge.
