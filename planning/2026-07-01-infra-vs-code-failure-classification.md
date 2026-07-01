# Infra-vs-code Failure Classification (host-health Part 2)

Date: 2026-07-01

Status: shipped (this PR). Part of the host-health proposal (#363); Part 1
(pre-dispatch gate) shipped in #364.

## Motivation

When a self-hosted runner co-located with heavy interactive work sheds load
(macOS jetsam / `WindowServer` crash) mid-validation, a leg can fail for an
**infra** reason while its `failure_class` reads `TEST` — so the author is sent
to debug a green tree. This slice makes that label honest.

## Scope decision: classify, not retry

The proposal's Part 2 says "auto-classify as infra and **retry once**." Two
findings narrowed this to classification-only:

- `is_retryable` (`classify.rs`) is defined + unit-tested but has **no production
  caller** — nothing drives a same-leg retry from it.
- Dispatch "failover" (`executor/dispatch.rs`) tries the *next backend* in a
  fallback chain; it is not a re-run of the same failed leg.

So "retry once" would mean building a new same-leg retry mechanism — a real
behavior change to the ship path. That is deferred. This slice delivers the
foundational, low-risk half: an honest `INFRA`-vs-`TEST` label.

## Safety invariant (verified)

Reclassifying `TEST → INFRA` is a **pure label**. `EvidenceStore::is_merge_ready`
requires a `passed()` (status `"pass"`) record per required platform and never
inspects `failure_class`; no code branches on `failure_class == "INFRA"` for
merge/ship-pass behavior (only an SSH-specific `"ssh_unreachable"` string is
matched, elsewhere). A failed leg therefore still blocks merge after
reclassification — the change cannot mask a real failure into a merge.

## Design

- **Signal reuse, not a new scan.** Reads the same `host_vitals` file as Part 1.
  Incident time is reconstructed as `file_mtime − age_s` (the JSON carries
  `jetsam_age_s` / `windowserver_age_s` but no absolute timestamp) and compared
  to the leg's `[started_at, completed_at]` with a small 2 s rounding tolerance.
  **No broad grace** — a wide window could turn an unrelated code failure into a
  masked "infra" label, the one direction we must not err toward. No native
  DiagnosticReports scan, so it stays a cross-platform no-op when the file is
  absent.
- **Conservative eligibility** (`classify::reclassify_on_host_incident`): only a
  `TEST` class is promotable. `CONTRACT` / `TIMEOUT` / `TREE_DRIFT` / `INFRA` /
  `UNKNOWN` are authoritative and kept, so a genuine validation-contract
  violation is never hidden.
- **Local-only.** SSH/cloud legs run on another host whose DiagnosticReports we
  can't read, so only `backend == "local"` results are considered.
- **Seam** (`ship::maybe_reclassify_on_host_incident`): applied in
  `execute_targets_with_options` after `dispatcher.validate` and **before** the
  durable `job.with_result` / `queue.update`, so the persisted queue, evidence,
  and outcome all carry the reclassified value consistently (a command-layer
  post-process would disagree with durable state — evidence persists the class
  before the command sees it).
- **Config threading, minimal.** `[host_health] classify_local_failures` (default
  off) resolves to an `Option<PathBuf>` via `host_health::incident_reclassify_path`
  at the worker (`ShipStores`/`RunStores` already carry `&LoadedConfig`), passed
  as one param to `execute_targets_with_options`. No request-struct or queue-
  persistence changes — config is read fresh at execution, so a queued-then-
  resumed ship honours the current setting.

## Fails open

Absent / stale / unreadable signal, no overlapping incident, non-local leg, or a
non-`TEST` class → the original class is untouched.

## Tests

- `classify.rs`: eligibility (only `TEST` promotes; all authoritative classes and
  `None` kept).
- `host_health.rs`: overlap core (inside / before / after window, `WindowServer`
  vs jetsam labelling, no-ages, negative-age guard, boundary-within-tolerance),
  path resolution gate on/off, `incident_from_path` fail-open.
- `ship.rs`: the seam end-to-end (promote local TEST on overlapping jetsam;
  no-op without a path; skip remote backends; never mask `CONTRACT`; skip when
  no incident overlaps; skip a passed result).

## Follow-ups

- Same-leg **retry-once** on a confirmed infra reclassification (needs a retry
  mechanism `is_retryable` anticipates but nothing yet drives).
- `shipyard run` currently inherits the same worker path, so run legs are covered
  too; if that proves noisy for local dev it can be gated separately.

## Note (unrelated pre-existing bug spotted)

`FailureClass::Timeout` serialises as `TIMEOUT` (`classify.rs`) but ship
diagnostics compare lowercase `"timeout"` (`app/ship_cmd.rs`). Out of scope for
this PR; noted for a separate fix.
