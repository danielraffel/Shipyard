# fleet-pass

The scheduled reviewer, repair and operator passes that drain the fleet's PRs.

- `fleet-pass.sh`: the runner. One bounded pass per role (`reviewer`, `repair`,
  `operator`, `adversary`): picks a provider account via `sr status`, builds the
  prompt from the work list, runs the agent with an idle and a ceiling timeout.
- `must-review.py`: the deterministic work lists. Default mode lists green PRs with
  no counted verdict at their head; `--repair` is the rescue list (HOLD, FAILED,
  CONFLICT, STACK-BASE-MERGED); `--adversary` is the release-path second review.
- `operator-findings.py`: the stuck-PR detector the operator pass reads.
- `test_must_review.py`: offline unit tests for `must-review.py` (no network).

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
