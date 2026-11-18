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
It creates those labels (with a colour, as GitHub requires) on first use. If
a label cannot be created, the issue opens without it and the log says so. It
finds an open issue by label and exact title, or by title alone for an
unlabelled one, so it updates one tracker instead of opening duplicates. Each tracker closes itself when its check passes.

| Layer | Workflow | Fires on | Tracker title |
|---|---|---|---|
| 1 | release workflow tests (`scripts/test_version_at_land.py`, `test_release_workflow.py`) | a pull request that breaks the chain | (a failing check) |
| 2 | `auto-release-watchdog.yml` | a failed `version-at-land.yml` or `auto-release.yml` run, including a run GitHub rejected with no jobs | `version-at-land workflow failed: RELEASES BLOCKED`, `auto-release workflow failed: RELEASES BLOCKED` |
| 3 | `release-cadence-check.yml`, every 30 minutes | an outcome, whatever caused it (below) | `Release cadence: merged changes but no release` |
| 4 | `release-cadence-check.yml` `chain` step, every 30 minutes and after each `Release` or `Release Heal` run | the current version is not public, complete and installable 90 minutes after it landed, or a bump pull request is open past 60 minutes | `Release blocked: the current version is not released and installable` |

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

Layer 4 follows the current `Cargo.toml` version on `main` to a release a user
can install. Before 90 minutes have passed since the version landed it only
reports `pending`. After that, every one of these must hold:

- the `v*` tag exists;
- the tag's `release.yml` push run completed with success. Its
  `sign-and-upload-macos` job signs, notarizes and attests the DMG on the
  self-hosted Mac. It publishes the release and then runs install.sh for the
  tag, comparing `--version` with the tag. A failed install puts the release
  back to draft and fails the run, so a green run covers the install;
- the release is public and has every asset `release.yml` must attach (the
  same list `release-heal.yml` uses, per `CI_PLATFORM_FOCUS`);
- `releases/latest` is this tag, so install.sh with no version installs it.

Separately, a `release/version-at-land` pull request open longer than 60
minutes fails layer 4 at any time, with the head's `review-verdict` state as
evidence. Layer 3 treats an open bump pull request as waiting, so before
layer 4 a stuck bump stalled releases silently.

The issue body begins with a `release-blocked-key` marker, `v<version>:<failing
steps>`, and lists each failing step with its evidence and fix.

### Who is told

An issue wakes nobody, so every tracker above also carries the
`release-blocked` label. The steward tick on m3 starts
`tools/metrics/release_notify.py` detached on every tick. At most every 10
minutes it reads the open `release-blocked` issues with one `ghapp` call. It
sends a cmux agent message (`--from shipyard-release-watch`) to each target in
`tools/metrics/release-notify-targets.txt`, or in the host's
`~/.config/shipyard/release-notify-targets`, which replaces it. It sends:

- when an issue opens or its key changes (a new version or failing step);
- once when an issue it reported closes.

A remote target is reached as `ssh -o BatchMode=yes <host> cmux ... agent
message <target> -- -`, the owner relay's transport, with the text on stdin.
A failed send is retried on the next pass and logged in the tick's
`summary.log` as `release-notify ERROR`. `SHIPYARD_RELEASE_NOTIFY=off` turns it
off. Preview it with `python3 -I tools/metrics/release_notify.py --dry-run --force`.

The decisions live in `scripts/release_watchdog.py` as pure functions, tested by
`scripts/test_release_watchdog.py`. The workflow files only pass event data in.
To preview without touching an issue:

```bash
python3 scripts/release_watchdog.py --dry-run cadence
python3 scripts/release_watchdog.py --dry-run chain --ref v<known-good tag>   # a control: ok
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
