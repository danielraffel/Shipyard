# fleet-pass

The scheduled reviewer, repair and operator passes that drain the fleet's PRs.

- `fleet-pass.sh`: the runner. One bounded pass per role (`reviewer`, `repair`,
  `operator`, `adversary`): picks a provider account via `sr status`, builds the
  prompt from the work list, runs the agent with an idle and a ceiling timeout.
- `must-review.py`: the deterministic work lists. Default mode lists green PRs with
  no counted verdict at their head; `--repair` is the rescue list (HOLD, FAILED,
  CONFLICT, STACK-BASE-MERGED); `--adversary` is the release-path second review.
- `operator-findings.py`: the stuck-PR detector the operator pass reads.

API use: each run reads every open PR of a repo (labels, head commit date, check
rollup, newest 100 comments, merge state) with one paginated GraphQL query per
30 PRs, plus two REST reads for the required checks. Per-PR REST reads would
spend the App's REST budget (a shared 15000/hour) several times per PR per run.
- `test_must_review.py`: offline unit tests for `must-review.py` (no network).
- `repair_budget.py`: holds a rescue entry once its exact line (`<repo>#<n> <sha>
  <REASON>`) has been attempted `--max` times (default 3) inside `--window` seconds
  (default 6 h). The line carries every input a repair pass acts on, so an unchanged
  line means another pass would see what the last ones saw. The runner filters the
  rescue list through `repair_budget.py filter` before claiming and records each
  started pass with `repair_budget.py record`; a held entry is logged `HELD` and
  comes back when a push, a check result or the reason changes it.
  `test_repair_budget.py` covers it offline.

## Install (m3)

Copy the three scripts to `~/.local/share/shipyard-steward/pass-runner/`. They run
from launchd jobs `com.danielraffel.fleet-pass.reviewer` and
`com.danielraffel.fleet-pass.repair`. Paths are overridable with
`FLEET_PASS_STATE`, `FLEET_PASS_LOGDIR`, `FLEET_PASS_ADD_DIR`,
`FLEET_PASS_WORKTREES_DIR` and `FLEET_PASS_REVIEWER_SIGNERS`.

## Verify

```sh
tools/fleet-pass/fleet-pass.sh --selftest   # fake provider, temp state
tools/fleet-pass/fleet-pass.sh reviewer --dry-run
python3 scripts/run_unittest_strict.py -s tools/fleet-pass -p 'test_*.py'
```
