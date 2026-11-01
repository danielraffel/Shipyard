# Release watchdog

Shipyard releases through a chain of workflows. `version-at-land.yml` opens the
bump pull request after every landing. Merging it moves `Cargo.toml`.
`auto-release.yml` tags that version, `release.yml` publishes it, and
`release-heal.yml` re-runs a release run that left a draft. Every link can
fail without stopping `main`, and a red run on `main` is easy to miss.
This happened once: from 2026-10-31 19:56Z `version-at-land.yml` failed on every
landing with an unexplained `CalledProcessError`, twelve `feat:`/`fix:` pull
requests merged, and nothing was released.

The watchdog is a port of Pulp's release-watchdog triad (Pulp
`docs/guides/release-watchdog.md`). Each layer keeps a GitHub issue labelled
`release-stall` (plus `ci` and `blocks-release`) open while its check fails.
It finds an open issue by label and exact title, so it updates one tracker
instead of opening duplicates. Each tracker closes itself when its check passes.

| Layer | Workflow | Fires on | Tracker title |
|---|---|---|---|
| 1 | release workflow tests (`scripts/test_version_at_land.py`, `test_release_workflow.py`) | a pull request that breaks the chain | (a failing check) |
| 2 | `auto-release-watchdog.yml` | a failed `version-at-land.yml` or `auto-release.yml` run, including a run GitHub rejected with no jobs | `version-at-land workflow failed: RELEASES BLOCKED`, `auto-release workflow failed: RELEASES BLOCKED` |
| 3 | `release-cadence-check.yml`, every 30 minutes | an outcome, whatever caused it (below) | `Release cadence: merged changes but no release` |

Layer 2 copies the failed job's error lines into the tracker, including gh's
own error, which `version-at-land.yml` now prints. It also writes the fix for
errors it recognises, such as a token missing pull request rights, an expired
token, a missing secret or a ruleset refusal. The next successful run of that
workflow closes the tracker.

Layer 3 checks two invariants with the default token and no release
credentials. It still runs when the release workflows or their token are broken:

- A `feat:` or `fix:` pull request merged to `main` more than two hours ago
  has its merge commit in a `v*` tag, unless a `release/version-at-land` pull
  request is open. An open bump pull request is waiting on review, not stalled.
- A `Cargo.toml` version move on `main` is in a `v*` tag within 30 minutes.

The decisions live in `scripts/release_watchdog.py` as pure functions, tested by
`scripts/test_release_watchdog.py`. The workflow files only pass event data in.
To preview without touching an issue:

```bash
python3 scripts/release_watchdog.py --dry-run cadence
python3 scripts/release_watchdog.py --dry-run watchdog --workflow "Assign versions at land" \
  --run-id <id> --run-url <url> --conclusion failure
```

## Differences from Pulp

- Shipyard's pull requests carry no version, so layer 3 judges merged
  `feat:`/`fix:` pull requests against tags. Pulp's only invariant is
  "a version bump on main has its tag". Shipyard checks both.
- Layer 2 watches `version-at-land.yml` as well as `auto-release.yml`, with one
  tracker for each. It also copies the error and the fix into the issue, so
  nobody has to open the run log to act.
- Logic is a tested Python script, not inline shell, and every job declares
  least-privilege permissions under `permissions: {}`.
- Tracker titles use a colon instead of Pulp's em dash.
- There is no separate `workflow-lint.yml`. Layer 1 is the existing workflow
  tests that CI runs on every pull request.
