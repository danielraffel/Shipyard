# Agent delivery contract follow-up — authenticated metrics handoff

- **Observed:** 2026-10-28T14:38:35Z
- **Contract:** https://github.com/danielraffel/pulp-planning/blob/codex/live-drain-refresh-20261028/2026-10-28-agent-delivery-contract.md
- **Contract branch tip at reconciliation:** `8e87f86cdc23c3aec65c41479446ea036fbc0c49`
- **Contract document blob:** `e5e9b8fa791a543867276949626532844307e78b`
- **Shipyard main:** `d7dfff57de91929bd7176593caf166ad8799792d`
- **Worktree:** `/Users/danielraffel/Code/Shipyard-metrics-auth-envelope-20261028`
- **Branch:** `codex/metrics-auth-envelope-schema-20261028`
- **Implementation head:** `2653076e77af5bd3fc8819b5de83926b9560a2b2`

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
  head. Its Python helper check is now green. It has no counted review and
  remains unarmed/unqueued.
- Shipyard review-verdict refused both heads because no allowed exact-head review
  exists. No approval, arm, enqueue, merge, or merge API call was made.
- Merged dependency: #950 at `cdb17438` supplies the existing v1 handoff health
  sidecar and freshness contract. Open follow-ups #989 (`6222b149`), #994
  (`6f84359c`), #996 (`c470ba62`), and #997 (`e59e1fd`) remain separate and are
  not treated as prerequisites for this telemetry slice; #989/#994/#996/#997 lack
  a counted exact-head review, #996 owns the baseline assertion repair, and #997
  now has green checks.

## Work and follow-up

- The authenticated handoff schema and durable `MetricsStore` persistence are
  implemented at `2653076e77af5bd3fc8819b5de83926b9560a2b2` (parent
  `92fad47e`). It binds canonical receipt bytes to repository/project/base branch,
  producer transport claims, bounded freshness, and durable replay/provenance;
  exact persisted replays are idempotent even after expiry. Legacy v1 imports stay
  visible, while `authenticated_safe_merge_summary` requires a provenance link and
  exact repository/project/base binding.
- Validation completed on the implementation branch: `cargo fmt --all -- --check`,
  `git diff --check`, authenticated handoff tests 5/5, persistence tests 2/2,
  metrics tests 143/143, full library suite 4840 passed / 0 failed / 5 ignored,
  and `cargo clippy --locked --lib --all-features -- -D warnings` (only the
  repository's pre-existing unknown `clippy::assert_is_empty` warning).
- Remaining follow-up: wire the authenticated importer to the existing #950
  supervised handoff/strict transport and decide when the opt-in authenticated
  denominator replaces the legacy scorecard. Do not treat #989/#994/#997 as
  merged; their exact-head review/CI gates remain independent.
- Before opening a PR, use `shipyard pr --no-arm --no-steward-handoff`. Do not
  approve, arm, enqueue, or merge.

**Disposition:** `ACTIVE — contract reconciled; ownership and authorization preserved; durable authenticated persistence validated; transport/scorecard wiring remains an explicit follow-up.`
