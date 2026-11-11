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
- `version_only.py`: the version-bump-only diff class and its consistency check,
  in SHADOW. `classify` admits a diff only when every file is one of
  `CMakeLists.txt`, `CHANGELOG.md`, `.claude-plugin/plugin.json`,
  `.claude-plugin/marketplace.json`, `tools/shipyard.toml`, none is carry-excluded
  by the base, every changed line in the first four is a version line, and
  CHANGELOG.md gains exactly one release block at its top (anchor, heading, plain
  pull-request-title bullets linking this repository's pull requests) plus its one
  link reference. Anything else goes to the full reviewer. `check` applies the
  base's `versioning.json` surfaces: each surface's files agree, a moved surface goes
  to the next patch, minor or major release, a changed file no surface owns is a
  HOLD, and the new release block names the version the base carried. The runner
  calls `shadow` on the reviewer queue, which appends the script verdict to a log
  and decides nothing; `shadow-report` compares that log with the counted verdicts
  and reports `may_decide` only after ten agreements and no disagreement. `test_version_only.py` covers it offline.

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
