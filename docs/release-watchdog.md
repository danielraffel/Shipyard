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
| 4 | `release-cadence-check.yml` `chain` step, every 30 minutes and after each `Release` run | a version is not public, complete and latest 90 minutes after it landed, or a bump pull request is open past 120 minutes | `Release blocked: the current version is not released and installable` |

Layer 2 copies the failed job's error lines into the tracker, including gh's
own error, which `version-at-land.yml` now prints. It also writes the fix for
errors it recognises, such as a token missing pull request rights, an expired
token, a missing secret or a ruleset refusal. The next successful run of that
workflow closes the tracker.

Layer 3 checks two invariants with the default token and no release
credentials. It still runs when the release workflows or their token are broken:

- A `feat:` or `fix:` pull request merged to `main` more than two hours ago
  has its merge commit in a `v*` tag or in the history of a merged
  `release/version-at-land` pull request, unless a bump pull request is open.
  The bump is version-at-land's decision: a plugin-only bump is the release
  (the plugin ships from git) and needs no tag, and a bump that moved the CLI
  must still be tagged (the next invariant). An open bump pull request is
  waiting on review, not stalled.
- A `Cargo.toml` version move on `main` is in a `v*` tag within 30 minutes.

Layer 4 follows the `Cargo.toml` version on `main` to a release a user can
install. A version is `ok` as soon as it, or a newer version, is the public
`releases/latest` with every asset `release.yml` must attach (the list
`release-heal.yml` uses, per `CI_PLATFORM_FOCUS`). Until then it is `pending`
for 90 minutes after it landed, and failing after that. Versions often land
less than 90 minutes apart, so while the current version is pending the check
judges the newest version already past its bound: a release that stays broken
cannot hide behind back-to-back bumps.

Publication is the install proof. `release.yml`'s `sign-and-upload-macos` job
signs, notarizes and attests the DMG on the self-hosted Mac, publishes the
release, and then runs install.sh for the tag and compares `--version` with
the tag. A failed install puts the release back to draft and fails the run. A
failing version reports the first missing link:

- `tag_missing`: no `v*` tag;
- `release_run_missing`, `release_run_incomplete`, `release_run_failed`: why
  the release is not public and complete, from the tag's `release.yml` run (a
  successful run outranks a later failed one, and a run dispatched on the tag
  counts). The two run states share one key token, so a run seen mid-rerun
  does not message anyone twice;
- `release_unpublished`, `release_assets_missing`;
- `latest_mismatch`: `releases/latest` is an older tag, so install.sh with no
  version installs that.

A public, complete and latest release is `ok` even when its run failed later
(the keychain restore, or a recovery by hand); the run stays in the evidence.

Separately, a `release/version-at-land` pull request open longer than 120
minutes fails layer 4 at any time, with the head's `review-verdict` state as
evidence. Between 2026-10-31 and 11-18, 8 of 82 healthy bump pull requests
were open over 60 minutes and 2 over 120. Layer 3 treats an open bump pull
request as waiting, so before layer 4 a stuck bump stalled releases silently.
A pending pass leaves the tracker alone, except one whose only failure was a
bump pull request that has since closed.

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
- once when an issue it reported closes (fixed or superseded).

Targets are stable ids: the m3 orchestrator's agent id as a `local` line and
the Shipyard lead's surface UUID on m5s. A `surface:N` ref changes after a
reboot, so the targets file should never hold one. A remote
target is reached as `ssh -o BatchMode=yes -- <host> cmux ... agent message
<target> -- -`, the owner relay's transport, with the text on stdin. Fields may
not start with `-`. A failed send is retried on the next pass. After three
consecutive failures to one target the notifier posts the message as a
comment on the issue instead, logs one `release-notify ERROR ... unreachable`
line in the tick's `summary.log`, and stops retrying it; if the comment cannot
be posted either, it gives up after 24 hours. `SHIPYARD_RELEASE_NOTIFY=off` turns it
off. Preview it with `python3 -I tools/metrics/release_notify.py --dry-run --force`.

The decisions live in `scripts/release_watchdog.py` as pure functions, tested by
`scripts/test_release_watchdog.py`. The workflow files only pass event data in.
To preview without touching an issue:

```bash
python3 scripts/release_watchdog.py --dry-run cadence
python3 scripts/release_watchdog.py --dry-run chain --ref <newest published tag>   # a control: ok
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
