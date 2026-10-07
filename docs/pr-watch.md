# PR watch

`shipyard pr-watch` flags open pull requests that are stuck in a way a person
should look at, with one evidence line per flag. It is read-only on GitHub
except for two opt-in writes: one sticky comment per flagged pull request
(`--post-comments`, or `[pr_watch] post_comments = true` for the daemon), and
the [hand-back](#hand-back)'s `shipyard:needs-agent` label plus a non-input
notification to the owning agent session (`--deliver-handback`, or
`[pr_watch.handback] enabled = true`). It never rebases, dequeues, arms,
re-runs, resumes an agent, or types into a session.

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

Every request carries `GH_REPO=<repo>`, so a `ghapp` wrapper on `PATH` binds
its App installation to the repository even when the command (or the daemon)
runs outside a checkout.

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

Every `flags[]` entry keeps the v1 fields; `count`, `kinds`,
`shared_failures`, and (when the hand-back ran) `owner` (`state`, `unowned`,
`agent`, `host`, `session`, `resume`, `path`) are additive, so a v1 consumer that ignores unknown keys
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

## Hand-back

A red pull request is handed back to the session that owns it, in tiers. All of
it is behind `[pr_watch.handback] enabled` (off by default); `scan --handback`
plans it as a dry run whatever the config says, and `scan --deliver-handback`
(or the daemon, when enabled) sends through the channels the config turns on.

A flag is **owner-actionable** when its digest route is per-PR (not a
"failing on main/pre-existing" shared failure, not an ejection the attributor
pinned on a neighbour) and it is a repeated test failure, red while armed, or a
repeated ejection. A rebase treadmill (the base moving) and the split advisory
are not.

| tier | when | what |
|---|---|---|
| 0 | an owner-actionable flag holds | the sticky comment, plus the `shipyard:needs-agent` label; the label is removed when every such flag is addressed or the pull request merges or closes |
| 1 | the owner's session is live | `cmux notify --surface <uuid>` (and, with `status = true`, a `shipyard-pr-<n>` sidebar pill, cleared later) plus an inbox line on the owner's host |
| 2 | the owner is dead, unknown, or unreachable for `unowned_after_hours` | the pull request's digest line carries `owner.unowned = true` with the `whence` resume hint |

**Label.** Only added or removed, never defined: if the repository has no
`shipyard:needs-agent` label the pass reports it and adds nothing (GitHub would
otherwise create it on add). A label the pass did not add is never removed, and
one a person removed is not put back during the same episode. A merged, closed,
or no-longer-observed pull request has ours deleted even when its snapshot does
not show it yet (a 404 means it is already gone), unless a person removed it. Before each add
the pass re-reads the pull request and adds nothing if it is no longer open, or
if its state cannot be read: the scan's snapshot can be minutes old.

**Owner.** The merge steward's exact-head handoff record (on this machine's
state directory) wins; otherwise the `<!-- whence {...} -->` marker in the pull
request body (`prov.host`, `agent`, `session`, `terminal_address`, `resume`,
`path`). A malformed marker (not JSON, no session, a session or surface that
does not look like one) is reported, never guessed at. The stamped host name
routes through `[pr_watch.handback.hosts]` to an ssh alias (or `"local"`); a
host absent from the map is local only when it is this machine's `hostname -s`,
and otherwise unreachable. Nothing about the fleet is hardcoded.

**Liveness.** `cmux sessions list --json --session <id>` on the owner's host
(read-only). Live means a record for exactly that session with
`agent_lifecycle = running` and `stored_pid_exists = true`; the record's current
surface is used. No record or a stopped one is dead; unreadable output is
unknown; an ssh failure or an unmapped host is unreachable.

**Once per episode.** A delivery is recorded in the ledger (`handback.delivered`)
against the flag episode's start, so an unchanged episode is never re-sent; a
new episode (new head, or a flag that cleared and came back) is. A session gets
at most one delivery per `session_interval_minutes` (pending episodes wait) and
several pull requests for one session go out as one notification. A delivery
whose every channel failed is not recorded and is retried next pass.

**Commands.** The only processes the hand-back can start are
`cmux sessions list`, `cmux notify`, `cmux set-status`/`clear-status` (key
`shipyard-pr-<n>`), and one fixed `sh -c` inbox append, run directly or as
`ssh -o BatchMode=yes -o ConnectTimeout=10 -- <alias> <single-quoted words>`.
Every argv passes an allowlist before it runs, and tests assert `cmux send`,
`send-key`, agent CLIs (`claude --resume`, `codex exec resume`), extra ssh
options, and unquoted shell never pass. It never types into a session, resumes
or starts an agent, or arms/dequeues a pull request.

**Inbox.** One JSON line per episode (`shipyard.pr-watch.handback/v1`: `id`,
`pr`, `url`, `title`, `kind`, `key`, `verdict`, `evidence`, `head_sha`,
`first_seen_at`, `delivered_at`) appended to
`~/.local/state/shipyard/inbox/<session-id>.jsonl` (`$SHIPYARD_INBOX_DIR`
overrides locally). The Shipyard Claude plugin's `hooks/handback-inbox.py` runs
at SessionStart and UserPromptSubmit: it is silent when the inbox is absent or
empty; otherwise it claims the file (rename), prints at most five entries
(2,000 characters, each line 300) as agent context, and moves them to
`<session-id>.shown.jsonl` so they show once. Codex reads the same hook
contract from `~/.codex/hooks.json`; add the script there to cover Codex
sessions:

```json
{"hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "timeout": 5,
  "command": "python3 /path/to/Shipyard/hooks/handback-inbox.py"}]}]}}
```

```toml
[pr_watch.handback]
enabled = false          # daemon delivers only when true
label = true             # tier 0 label add/remove
notify = false           # tier 1 cmux notify
status = false           # tier 1 sidebar pill (with notify)
inbox = false            # tier 1 inbox line
session_interval_minutes = 30
unowned_after_hours = 1
timeout_seconds = 20
# cmux_path = "/Applications/cmux.app/Contents/Resources/bin/cmux"

[pr_watch.handback.hosts]   # stamped host name -> ssh alias, or "local"
m3 = "m3"
Daniels-Mac-Studio-m3 = "m3"
```

**Measuring it.** A delivering pass appends a `wake.*` event for every
transition of an owner-actionable episode to the ledger's event log
(`<ledger>.events.jsonl`, beside `opened`/`addressed:*`), with a structured
`detail` (`pr`, `episode`, and per kind `rung`, `channels`, `session`, `host`,
`how`):

| event | when |
|---|---|
| `wake.raised` | a delivering pass first sees the episode owner-actionable on an open pull request |
| `wake.sent` | a tier-1 channel accepted it (`rung`, `channels`, `session`, `host`) |
| `wake.failed` | every tier-1 channel of a delivery failed |
| `wake.seen` | the owner's session displayed it inside an agent turn |
| `wake.escalated` | the next rung fired because the previous one was not seen |
| `wake.resolved` | it stopped being actionable (`how`: `addressed`, `closed`, `gone`, `new_episode`, `not_actionable`) |

Delivery is not receipt: `sent` is a channel's result, `seen` is the agent's.
Plan mode writes no events. `shipyard pr-watch wakes [--since 24h]
[--unseen-after 30m] [--json]` reports raised, sent, seen (and the seen rate),
open, failed, escalated, resolved by how, p50/p90 of raised-to-sent,
sent-to-seen and raised-to-resolved, and every open episode sent longer ago
than `--unseen-after` and not seen: the calls nobody answered.

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
comment_author = "shipyard-local[bot]"

[pr_watch.thresholds]    # spec defaults
red_minutes = 30
failed_groups = 2
replacements = 3

[pr_watch.digest]
enabled = true           # daemon digest; off when absent
command = ["/path/to/python3", "/path/to/harbormaster_digest.py", "--post"]
interval_minutes = 60
min_age_minutes = 120
```

The digest toggle is `[pr_watch.digest] enabled`. TOML cannot hold
`pr_watch.digest` as both a boolean and a table, so `digest = true` under
`[pr_watch]` next to a `[pr_watch.digest]` table is a parse error. A bare
`[pr_watch] digest = true` with no table is still accepted, but it cannot carry
the command, so use the table form. A `[pr_watch.digest]` table without
`enabled` keeps the digest off and every pass reports a warning (in the
`pr_watch_pass` event's `warnings`, and on stderr for `shipyard pr-watch`).
This block is parsed by a unit test, so it stays valid TOML.

Each pass publishes a `pr_watch_pass` IPC event with per-repository flag
counts, errors and config warnings. The digest needs the command's host (the Harbormaster token
lives on one machine); the scan itself is host-agnostic.
