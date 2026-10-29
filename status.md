# Scorecard provenance slice status

**Observed:** 2026-10-29 UTC
**Branch:** `codex/scorecard-provenance-20261029`
**Base:** `origin/main` at `d7dfff57de91929bd717659c3af166ad8799792d`
**Implementation head:** `e997c9dee8ae74982e87d7938da9ff0c70019ef5`

## Completed

- Added `coordination_coverage` to the JSON scorecard. It reports whether
  authoritative queue timing covers all completed jobs.
- Added `rework_compute_coverage`. It reports whether failed attempts have
  valid measured duration.
- Human cost output now prints both coverage states.
- Documented that cost totals remain proxies and that coverage is explicit.
- Added a regression test proving missing timing yields `unavailable` coverage
  rather than an apparently complete zero.

## Validation

- `cargo fmt --check`
- `cargo test stewardship_scorecard --lib` — 10 passed
- `git diff --check`

## Current external state

Shipyard `main` is `d7dfff57de91929bd717659c3af166ad8799792d`. The live scorecard
still has no provenance-qualified samples and no joined safely-merged denominator;
this patch does not promote proxy data or infer a merge denominator. Open PRs
#1000/#1002 and later cause/measurement PRs remain governed by their own owners;
no review, approval, arm, enqueue, merge, or queue mutation was performed.

The earlier Python failure was an ordering assertion in the collector, not a
missing Mise installation. Shipyard invokes `python3` directly and has no Mise
manifest. TartCI requires pinned Python 3.11+ with `tomllib`; its pinned Mise
binary is optional and does not change lane PATH or interpreter selection.

## Next action

Open this branch as an unarmed PR with
`shipyard pr --no-arm --no-steward-handoff`, then obtain an authorized exact-head
review. Do not merge or arm from this workstream.
