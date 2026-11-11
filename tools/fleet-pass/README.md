# fleet-pass

The scheduled reviewer, repair and operator passes that drain the fleet's PRs.

- `fleet-pass.sh`: the runner. One bounded pass per role (`reviewer`, `repair`,
  `operator`, `adversary`): picks a provider account via `sr status`, builds the
  prompt from the work list, runs the agent with an idle and a ceiling timeout.
- `must-review.py`: the deterministic work lists. Default mode lists green PRs with
  no counted verdict at their head; `--repair` is the rescue list (HOLD, FAILED,
  CONFLICT, STACK-BASE-MERGED); `--adversary` is the release-path second review.
- `operator-findings.py`: the stuck-PR detector the operator pass reads. It also prints
  the steward's infra-rerun outage findings (`INFRA-OUTAGE-SUSPECTED`, from the latest
  tick JSON's `infra_rerun.findings`); `test_operator_findings.py` covers that reader.

API use: each run reads every open PR of a repo (labels, head commit date, check
rollup, newest 100 comments, merge state) with one paginated GraphQL query per
30 PRs, plus two REST reads for the required checks. Per-PR REST reads would
spend the App's REST budget (a shared 15000/hour) several times per PR per run.
- `test_must_review.py`: offline unit tests for `must-review.py` (no network).
- `repair_budget.py`: holds a rescue entry once its exact line (`<repo>#<n> <sha>
  <REASON>`) has been attempted `--max` times (default 3) inside `--window` seconds
  (default 6 h). The line carries every input a repair pass acts on, so an unchanged
  line means another pass would see what the last ones saw. The runner filters the
  rescue list through `repair_budget.py filter` before claiming and records only the
  first line when a model pass starts with `repair_budget.py record`; current held
  entries are written to `repair.held` for `operator-findings.py` and come back when
  a push, a check result or the reason changes it.
  `test_repair_budget.py` covers it offline.
- `review_queue.py`: deterministic filters that keep model passes off heads that
  need no new review. `gate_carried` drops a head only when its newest
  `review-verdict` status says the gate carried an earlier approval to that exact
  head (`approved: the approval of <12 hex> carries to|across the rebase to
  <head>`), that approval is a counted APPROVE in the PR's comments, and the status
  is under 24 h old. A plain `success` (for example "does not require a review")
  never drops a head. `claim` gives each reviewer slot only the entries it holds an
  exclusive claim on (`review_queue.py claim --dir <claims> --pid $$ --cap N`;
  claims are published by rename with their pid inside), so two slots never review
  one head; a slot holds at most N heads for its whole pass. Install it beside
  `must-review.py`, before it, since `must-review.py` imports it. `test_review_queue.py` covers it offline.
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

Install the tracked runner and all of its sibling helpers as one deployment unit:

```sh
tools/fleet-pass/install-pass-runner.sh
```

The installer targets `~/.local/share/shipyard-steward/pass-runner/` by default
(`FLEET_PASS_INSTALL_DIR` overrides it), and is the deployment path used by the
launchd jobs `com.danielraffel.fleet-pass.reviewer` and
`com.danielraffel.fleet-pass.repair`. Paths are overridable with
`FLEET_PASS_STATE`, `FLEET_PASS_LOGDIR`, `FLEET_PASS_ADD_DIR`,
`FLEET_PASS_WORKTREES_DIR` and `FLEET_PASS_REVIEWER_SIGNERS`.

## Verify

```sh
tools/fleet-pass/fleet-pass.sh --selftest   # fake provider, temp state
tools/fleet-pass/fleet-pass.sh reviewer --dry-run
python3 scripts/run_unittest_strict.py -s tools/fleet-pass -p 'test_*.py'
```
