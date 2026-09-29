# PR watch

`shipyard pr-watch` flags open pull requests that are stuck in a way a person
should look at, with one evidence line per flag. It is read-only on GitHub.
The only write it can make is one opt-in sticky comment per flagged pull
request, and that is off unless `--post-comments` (or `[pr_watch]
post_comments = true` for the daemon) asks for it. It never rebases,
dequeues, arms, re-runs, or messages a session.

```bash
# One pass; a dry run that prints flags and the comment/digest it would send.
shipyard pr-watch scan --repo Generous-Corp/pulp

# Same pass, also editing sticky comments and delivering the digest.
shipyard pr-watch scan --repo Generous-Corp/pulp --post-comments --digest

# Simulate the last seven days at 15-minute ticks with the same rules.
shipyard pr-watch replay --repo Generous-Corp/pulp --since 7d \
  --expect 8933=1,3,4,5 --expect 8970=3 --control merged-clean

# Build the hourly digest from the ledger; --post delivers it.
shipyard pr-watch digest --repo Generous-Corp/pulp [--post]
```

`--json` (global) prints the structured report. Run from a checkout of the
repository or pass `--repo` so GitHub credentials resolve for it.

## Flags

Only **required** checks count: the query's `[pr_watch] required_checks`, else
the base branch's protection contexts. An advisory job (Linux, Windows,
sanitizers) can never raise a flag, and a failure is attributed to the
required job by name, never to "the first failed job".

| # | kind | holds when | evidence |
|---|---|---|---|
| 1 | `repeat_test_failure` | the same required check failed with the same failing test on at least 2 runs of the PR (its heads and merge groups named for it), and the PR's latest pass-or-fail result for that check is still a failure | the check, the test, the runs. Verdict "code failure, not flake", or "failing on main/pre-existing" when the same test also failed on at least 2 *other* PRs in the last 24 h |
| 2 | `red_while_armed` | auto-merge is armed (or the PR was ejected for `failed_checks` and not re-armed), it is not in the queue, and a required check on the current head has been red for more than 30 minutes with no push since | the check, head, and red-since time |
| 3 | `repeated_ejection` | at least 2 merge groups named for the PR (`gh-readonly-queue/<base>/pr-<N>-<parent>`) failed a required job, with no passing named group since | each failed group with its parent group's status |
| 4 | `rebase_treadmill` | the head was replaced at least 3 times within 24 h, each time the previous head's gate run was cancelled and the merge base with the base branch advanced | the head chain; says "inferred" |
| 5 | `split_candidate` | open more than 3 days, or more than 60 files or 30 commits | advisory only: raised only alongside another flag on the same PR, never alone, and never alone in a digest |

Signatures: CTest summary lines are normalised to the bare test name
(`21516 - name (Failed)  labels` becomes `name`, because CTest renumbers tests
between runs). A log with no CTest summary uses its first `##[error]` line that
is not a bare "Process completed with exit code N".

Flag 3 is "named for", not "proved culprit". GitHub names a merge group after
its last entry, so a batch-mate's failure is attributed to it; the parent
group's status is shown so a reader can see a failure that was already failing
upstream. When `scan` runs inside a checkout whose config declares
`[queue.attribution] command` (and the script it names exists there), it asks
that attributor about each failed group behind a flag 3, with the same argv
Shipyard's queue-arm guard uses (`--repo R --pr N --run-id ID`, run from the
repository root; at most 8 calls per pass, decisive verdicts cached). When the
attributor clears the pull request for every failed group (`implicates_head`
exactly `false` with verdict `other_pull_request` naming another PR, or
`infrastructure`), the flag stays on the sticky comment labelled
"neighbour of #N" and leaves the digest. Without an attributor (the daemon
runs outside any checkout; `replay` does not call it) flag 3 keeps the
named-failed-groups rule. Flag 4's "main moved" is inferred from the merge base
(`compare/<base>...<head>`), which is why its evidence says so.

## Sources

Everything is read through one argv-in/JSON-out seam, so tests replay recorded
responses and nothing can write by accident:

- required checks: `repos/{repo}/branches/{base}` protection contexts;
- pull requests updated in the window, with queue timelines: one GraphQL
  search (50 per page, `timelineItems(last:100)` of force-push, queue,
  auto-merge, merged/closed/reopened events);
- gate-workflow runs (`[pr_watch] workflow`, default `build.yml`) for
  `pull_request` and `merge_group`, one day of creation time per listing;
- required check runs of every head seen (`commits/{sha}/check-runs`,
  `filter=all`);
- required jobs of every merge-group run that failed;
- the log of every failed required job, reduced to its signatures;
- merge bases, only for heads that could form a flag-4 candidate.

A head's push time is the creation time of its first gate run (or its
force-push event), never its commit date, which the author controls.
Settled answers are cached under `<state>/pr-watch/cache/<repo>`: required jobs
of a completed run attempt, signatures of a completed job's log, check runs of
a head that is no longer an open PR's head, and merge bases.

## Sticky comment

Marker `<!-- shipyard-pr-watch v1 -->`. A comment is ours when its id is in
the ledger, or it carries the marker and its author is `[pr_watch]
comment_author` (for the Shipyard App, `shipyard-local[bot]`). A marker comment
written by anyone else is never edited, and an unreadable comment list means
"unknown, do not create". The comment is created once, patched only when the
rendered body changes, and rewritten to "resolved" when every flag clears. It
is never deleted. The only requests are `POST repos/{repo}/issues/{n}/comments`
and `PATCH repos/{repo}/issues/comments/{id}`, and a test asserts no other
mutating argv can be produced.

## Ledger and digest

Per repository and base, `<state>/pr-watch/<repo>-<digest>.json` (atomic
writes, an exclusive `.lock`, and an append-only `.events.jsonl`) records each
flag episode: first seen, last seen, head, and when and why it was addressed
(`cleared`, `new_head`, `merged`, `closed`, `ack_label`). A new head restarts
the clock. The `shipyard:ack/pr-watch` label (read only; never created)
acknowledges a PR's flags.

The digest runs at most once per `interval_minutes` (60), carries only flags
unaddressed on the same head for at least `min_age_minutes` (120) that no
earlier digest carried, and is skipped when empty. It has one line per pull
request: its highest-severity flag (red while armed, then repeated ejection,
repeated test failure, rebase treadmill, split) with the count and kinds of the
flags it has. A repeated test failure whose verdict is "failing on
main/pre-existing" is a main-health signal, not an owner action, so it gets no
per-PR line; instead the digest carries at most one shared-failure line per
test ("`test` failing across #a, #b, #c — likely main/cross-PR"), announced
again no sooner than 24 hours later. Flags routed comment-only (a neighbour's
ejection) never reach the digest. The sticky comment still shows every flag. It is claim-then-send: the
claim is persisted, then the configured argv runs with the
`shipyard.pr-watch.digest/v1` JSON on stdin (exit 0 = delivered). A failure
rolls the claim back so the next pass retries; a claim found at start (the
process died mid-send) counts as delivered, so a lost write never double-posts.

```json
{
  "schema": "shipyard.pr-watch.digest/v1",
  "repo": "Generous-Corp/pulp",
  "generated_at": "2026-09-29T07:00:00Z",
  "window": {"min_age_minutes": 120},
  "flags": [
    {"pr": 8933, "title": "...", "url": "https://github.com/Generous-Corp/pulp/pull/8933",
     "kind": "repeated_ejection", "verdict": "repeatedly ejected from the merge queue",
     "evidence": "...", "first_seen_at": "2026-09-29T03:10:00Z",
     "age_minutes": 230, "head_sha": "fc399ea64...",
     "count": 3, "kinds": ["repeated_ejection", "repeat_test_failure", "split_candidate"]}
  ],
  "shared_failures": [
    {"check": "macos", "test": "consumption-census-drift", "prs": [8933, 8986, 9034],
     "verdict": "likely main/cross-PR", "first_seen_at": "2026-09-29T02:40:00Z",
     "evidence": "`consumption-census-drift` (`macos`) failing across #8933, #8986, #9034 — likely main/cross-PR"}
  ]
}
```

Every `flags[]` entry keeps the v1 fields; `count`, `kinds` and
`shared_failures` are additive, so a v1 consumer that ignores unknown keys
keeps working. A digest with no `flags` and no `shared_failures` is never sent.

## Replay

`replay` gathers the window once, then calls the same `evaluate(history, at)`
the live scan uses at every tick (default 15 minutes), folding each tick into a
throwaway in-memory ledger and simulating the digest. It never touches the live
ledger; `--state-file` writes the simulated end state to an explicit path and
refuses the live one. It exits 1 when an `--expect PR=FLAGS` is not met or,
with `--control merged-clean`, when any flag was raised on a PR that merged in
the window with zero failed required jobs and zero `failed_checks` removals.
`--until` pins the window end so a replay stays reproducible. The report's
`digest_stats` counts the simulated digests, the pull requests that got a
line, per-PR and shared-failure lines, and the most lines in one digest.

The history reflects today's pull-request metadata (changed files, commits,
labels) and timelines with at most 100 queue events per PR (a longer timeline
is reported as a gap).

## Daemon job and config

The daemon runs one pass every 15 minutes, the first one interval after start.
Each pass re-reads machine-global config, so toggling needs no restart:

```toml
[pr_watch]
enabled = false          # daemon job off by default
repos = ["Generous-Corp/pulp"]   # else the daemon's --repo list
base = "main"
workflow = "build.yml"
# required_checks = ["macos", ...]   # else branch protection
lookback = "7d"
post_comments = false
digest = false
comment_author = "shipyard-local[bot]"

[pr_watch.thresholds]    # spec defaults
red_minutes = 30
failed_groups = 2
replacements = 3

[pr_watch.digest]
command = ["/path/to/python3", "/path/to/harbormaster_digest.py", "--post"]
interval_minutes = 60
min_age_minutes = 120
```

Each pass publishes a `pr_watch_pass` IPC event with per-repository flag
counts and errors. The digest needs the command's host (the Harbormaster token
lives on one machine); the scan itself is host-agnostic.
