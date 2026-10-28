# Agent delivery contract follow-up — authenticated metrics handoff

- **Observed:** 2026-10-28T15:27:32Z (reconciled against the live contract branch, PRs, and exact-head validation)
- **Contract:** https://github.com/danielraffel/pulp-planning/blob/codex/live-drain-refresh-20261028/2026-10-28-agent-delivery-contract.md
- **Contract branch tip at reconciliation:** `1b4df0f470d5dcce2a56d125bd0ff6068b02dc6a`
- **Contract document blob:** `e5e9b8fa791a543867276949626532844307e78b`
- **Shipyard main:** `d7dfff57de91929bd7176593caf166ad8799792d`
- **Worktree:** `/Users/danielraffel/Code/Shipyard-metrics-auth-envelope-20261028`
- **Branch:** `codex/metrics-auth-envelope-schema-20261028`
- **Source implementation/validation head:** `5114947addb5362b1d88b16aaaf5bc5a159b0bb9`
- **PR head at this receipt snapshot:** `3e483e1111052b5ec80b4eada52bc2afcd769e08`
- **PR:** https://github.com/danielraffel/Shipyard/pull/998

## Contract application

The delivery contract applies directly. GitHub owns repository, review, required-check,
and native queue state. Shipyard records governed receipts and exact-head evidence;
TartCI owns disposable jobs and fleet leases. This slice is telemetry/provenance
work only: it does not select tests, schedule jobs, relax CI, authorize merges, arm,
enqueue, or call a merge API. Legacy `shipyard-merge-receipt/v1` rows remain visible
but are not authenticated provenance.

The current Pulp checkout is superseded/merged and was not edited. This Shipyard
worktree is fresh from `origin/main`; no existing PR branch was force-pushed or
reassigned.

## Live state reconciled

- Shipyard #996 (https://github.com/danielraffel/Shipyard/pull/996) is open at
  exact head `c470ba62ff27124344c5e59aa4a18dade5138c1a`, based on
  `d7dfff57de91929bd7176593caf166ad8799792d`. It owns the stale baseline-test
  assertion repair. Its checks are green, but it has no counted review and remains
  unarmed/unqueued.
- Shipyard #997 (https://github.com/danielraffel/Shipyard/pull/997) is open at
  exact head `e59e1fd53529f441758bbb072eb0de42df36ade5`, based on the same main
  head. Its Python helper check is currently failing. It has no counted review
  and remains unarmed/unqueued.
- Shipyard review-verdict refused both heads because no allowed exact-head review
  exists. No approval, arm, enqueue, merge, or merge API call was made.
- PR #998 was open at snapshot head `3e483e11`, based on Shipyard `main`.
  Resolver, version/skill, sandbox Linux, Linux, macOS ARM64, coverage,
  and risk checks are successful; Windows was still running at this observation
  boundary; `Python helper tests [github-hosted]` failed in `Run packaging helper
  tests`. The PR has no counted review, steward handoff, or queue admission.
  This failed pass condition is owned by the related #996 baseline assertion
  repair and must be fixed or explicitly reconciled before a fresh exact-head
  review.
- Merged dependency: #950 at `cdb17438` supplies the existing v1 handoff health
  sidecar and freshness contract. Open follow-ups #989 (`6222b149`), #994
  (`6f84359c`), #996 (`c470ba62`), and #997 (`e59e1fd`) remain separate and are
  not treated as prerequisites for this telemetry slice; #989/#994/#996/#997 lack
  a counted exact-head review, #996 owns the baseline assertion repair, and #997
  currently has its own failing Python helper check.

## Work and follow-up

- The authenticated handoff schema and durable `MetricsStore` persistence are
  implemented and validated at exact head `5114947addb5362b1d88b16aaaf5bc5a159b0bb9`.
  It binds canonical receipt bytes to repository/project/base branch, producer
  transport claims, bounded freshness, and durable replay/provenance; exact
  persisted replays are idempotent even after expiry. Legacy v1 imports stay
  visible, while `authenticated_safe_merge_summary` requires a provenance link
  and exact repository/project/base binding.
- Exact-head validation at `5114947a` completed: `cargo fmt --all -- --check`,
  `git diff --check`, handoff tests 5/5, authenticated metrics tests 4/4, full
  library suite 4840 passed / 0 failed / 5 ignored, and
  `cargo clippy --locked --lib --all-features -- -D warnings` (only the
  repository's pre-existing unknown `clippy::assert_is_empty` warning).
- Remaining follow-up: repair/reconcile the packaging-helper baseline failure via
  its owning #996 work, then wire the authenticated importer to the existing #950
  supervised handoff/strict transport and decide when the opt-in authenticated
  denominator replaces the legacy scorecard. Do not treat #989/#994/#996/#997 as
  merged; their exact-head review/CI gates remain independent. PR #998 remains
  at the review gate until its exact head is green and receives an allowed review.
- Before opening a PR, use `shipyard pr --no-arm --no-steward-handoff`. Do not
  approve, arm, enqueue, or merge.

**Disposition:** `ACTIVE — contract reconciled; ownership and authorization preserved; durable authenticated persistence validated; PR #998 is parked at the review gate pending an allowed exact-head review and completion of its current validation run.`

## Superseding exact-head reconciliation — 2026-10-28T16:35:00Z

- The delivery contract remains authoritative at branch tip
  `1b4df0f470d5dcce2a56d125bd0ff6068b02dc6a`, document blob
  `e5e9b8fa791a543867276949626532844307e78b`, and requires GitHub review,
  required checks, and native queue state to remain the landing authority.
- The current Shipyard `main` head is still
  `d7dfff57de91929bd7176593caf166ad8799792d`. The source worktree is clean at
  branch `codex/metrics-auth-envelope-schema-20261028`, exact head
  `d17125aca2cc0c66e40c8e0947fcac0cfe3186f7`, and the branch has not been
  reassigned or force-pushed.
- PR #998 is open at that exact head, based on the current `main`, and remains
  `MERGEABLE`/`UNSTABLE`. Its resolver, Linux, macOS ARM64, Windows, sandbox,
  coverage, version/skill-sync, risk-tier, and advisory cause checks succeeded
  in run `40724196095`. The Python helper check failed at
  `test_collect_allows_missing_merge_receipt_handoff`; that assertion belongs
  to the separately owned #996 repair and is not duplicated here. The
  `review-verdict` status context remains failed closed because #998 has no
  counted allowed exact-head review, even though the workflow check itself
  completed successfully.
- PR #996 remains open at exact head
  `c470ba62ff27124344c5e59aa4a18dade5138c1a`, with its required checks green,
  but it also has no counted allowed review. It remains the owner of the
  Python helper assertion; #998 must not absorb or duplicate that fix.
- No approval, reviewer verdict, steward handoff, arm, enqueue, merge, or merge
  API call was made. The branch remains parked at the review gate. The
  authenticated importer/transport slice remains a separate follow-up until
  the adversarial review preconditions and a fresh exact-head review are
  satisfied.

**Next safe action:** submit the unchanged branch through
`shipyard pr --no-arm --no-steward-handoff` for a fresh bounded validation
receipt, then leave #998 parked for an allowed reviewer. After #996 is reviewed
and lands, refresh #998 against the new `main`, rerun all required checks, and
obtain a fresh exact-head review before any queue action by the authorized
team-lead.

## Live refresh — 2026-10-28T15:57:58Z

- The contract branch is still at exact tip
  `1b4df0f470d5dcce2a56d125bd0ff6068b02dc6a`; the contract document blob is
  `e5e9b8fa791a543867276949626532844307e78b`.
- Shipyard `main` remains `d7dfff57de91929bd7176593caf166ad8799792d`. The
  worktree is clean, based on that exact head, and the source branch remains
  `codex/metrics-auth-envelope-schema-20261028`.
- PR #998 is open at exact head
  `51817438b16ab187c5a40f95a6f3f014df5e96c9`, base
  `d7dfff57de91929bd7176593caf166ad8799792d`, and is `MERGEABLE` but
  `UNSTABLE`. It has no counted review, no review request, no auto-merge
  request, and no queue admission.
- The fresh validation has successful resolver, review-verdict workflow,
  risk-tier, sandbox resolver, version/skill-sync, and risk-tier report
  checks. The `review-verdict` status context is `FAILURE` because no allowed
  exact-head approval exists; this is the expected fail-closed review gate.
  Linux, Sandbox Linux, macOS ARM64, Windows, coverage, and Python helper
  checks were still `IN_PROGRESS` at this observation boundary.
- No approval, arm, enqueue, merge, merge API call, or reviewer verdict was
  made. The separately owned #996 Python helper repair was not duplicated.

**Next safe action:** run `shipyard pr --no-arm --no-steward-handoff` from
this exact branch, then observe its bounded validation result. If checks are
green, leave the PR parked for an allowed reviewer; if a real check fails,
return the exact failure to its owning implementation work. Do not promote
the authenticated importer/transport slice from this receipt alone.

## Validation refresh — 2026-10-28T16:15:14Z

- The receipt-only validation job `40721331065` ran at exact head
  `36e6d740dc9f04e5017a0ab33576e4a7b62d9c4e`. Resolver, sandbox, version and
  skill-sync, coverage, and advisory cause checks succeeded.
- Linux, macOS ARM64, and Windows failed in the common `cargo clippy
  --all-targets --locked -- -D warnings` step because the authenticated
  persistence test in `src/metrics.rs` exceeded the repository's
  `too_many_lines` lint. This was a real regression in this branch, not a
  fleet failure. The test setup is now split into shared test helpers; local
  authenticated handoff tests and `cargo clippy --locked --lib --all-features
  -- -D warnings` pass (with only the repository's pre-existing unknown-lint
  warning).
- Python helper tests independently failed in the existing
  `test_collect_allows_missing_merge_receipt_handoff` assertion: the test
  expected an unavailable handoff event, while the collector returned a
  completed collection event. That failure belongs to the separately owned
  baseline-helper repair in PR #996 and was not duplicated here.
- The review-verdict status remains fail-closed: PR #998 has no counted
  allowed exact-head review. It has no auto-merge request or queue admission.

**Disposition:** `ACTIVE — one branch-owned clippy regression is repaired
locally; the unrelated Python helper failure remains with #996; the branch
must be committed and revalidated before the review gate can be reconsidered.`
