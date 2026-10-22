# Merge-receipt scorecard validation

Shipyard emits a `shipyard-merge-receipt/v1` record only after it owns the
merge, confirms the exact validated head is merged, and has terminal green
required-check evidence. The metrics scorecard joins those receipts to exact
PR-head job identities; it does not infer safe merges from GitHub PR counts.

A missing receipt is therefore reported as unavailable coverage rather than a
zero-cost claim. Receipts can be imported with `shipyard metrics import
merge-receipt` only when their producer provenance and gate-evidence hash pass
schema validation.
