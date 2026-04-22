# Waiting on conditions (`shipyard wait`)

`shipyard wait` is the primitive for "I need to block until something
on GitHub is true." Three truth conditions:

- **`wait release <version>`** — release tag exists, artifacts uploaded.
- **`wait pr <N> --state {green|merged|closed}`** — PR reached a state.
- **`wait run <id> [--success]`** — workflow run reached terminal.

It replaces hand-rolled `gh`-polling loops. When a shipyard daemon is
running, waits wake in seconds on real webhook events instead of
polling cadence. When the daemon isn't running, it falls back to `gh`
polling — same result, just slower. Always safe to use.

## Invocation

```sh
shipyard wait release v0.23.0 --timeout 900 --json
shipyard wait pr 151 --state green --timeout 1800 --json
shipyard wait pr 151 --state merged --timeout 3600 --json
shipyard wait run 22345678 --success --timeout 1200 --json
```

All three subcommands accept:

| Flag | Default | Meaning |
|------|---------|---------|
| `--timeout SECONDS` | 600 (release), 1800 (pr/run) | Hard deadline. |
| `--poll-interval SECONDS` | 2 (release), 30 (pr), 15 (run) | Polling cadence when daemon unreachable. |
| `--no-fallback` | off | Exit 6 if the daemon isn't available and the snapshot doesn't already match. |
| `--json` | off | Emit a structured envelope. |

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | condition matched |
| 1 | `--timeout` elapsed |
| 4 | `wait run --success` hit a terminal-but-wrong conclusion |
| 5 | invalid input (PR/release/run not found, bad tag) |
| 6 | daemon unreachable + snapshot didn't match + `--no-fallback` |
| 7 | unsupported scope — rulesets / merge-queue detected |
| 130 | SIGINT / SIGTERM |

## Truth conditions

### `wait release <version>`

Matches when:

1. A release with `tag_name == <version>` exists and `draft == false`.
2. Every artifact named in `[release.artifacts]` in
   `.shipyard/config.toml` has `state == "uploaded"`.
3. If no manifest is configured, match as soon as `assets.length > 0`
   with at least one uploaded asset.

Example manifest:

```toml
[release]
artifacts = [
  "shipyard-x86_64-linux",
  "shipyard-aarch64-darwin",
  "shipyard-x86_64-windows.exe",
]
```

Event source: the `release.published` webhook wakes the waiter.
GitHub emits no dedicated asset-upload event, so after the wake the
waiter re-evaluates on `--poll-interval` until every manifested asset
is `uploaded` or `--timeout` expires. The only time budget is
`--timeout` — there's no hidden asset-watch sub-timeout.

### `wait pr <N> --state green`

Matches when, for PR `<N>`'s current head SHA, every classic
branch-protection required check has conclusion ∈ `{SUCCESS, NEUTRAL,
SKIPPED}`.

Conclusion mapping:

- `SUCCESS`, `NEUTRAL`, `SKIPPED` → passing
- `FAILURE`, `TIMED_OUT`, `CANCELLED`, `ACTION_REQUIRED`,
  `STARTUP_FAILURE`, `STALE` → failing (not terminal-match; waiter
  keeps reporting "not matched" until timeout, because a pending retry
  could still flip the lane)
- `QUEUED`, `IN_PROGRESS`, `PENDING` → still waiting

Re-evaluated on every `check_run` / `check_suite` / `workflow_run` /
`reconcile_healed` event that pertains to this PR.

**Classic branch protection only.** Rulesets and merge-queue required
status detection exits 7. If you're on rulesets, either wait via
`gh pr checks --watch` or fall back to the classic branch-protection
path until governance support lands.

### `wait pr <N> --state merged` / `wait pr <N> --state closed`

`merged` matches when the PR's `merged` field is true. `closed`
matches when `state ∈ {CLOSED, MERGED}`. Both are monotonic — once
matched, they stay matched, so a resume is safe.

### `wait run <id> [--success]`

Matches when the Actions workflow run with id `<id>` reaches
`status == "completed"`. With `--success`, the match additionally
requires `conclusion == "success"`. Any other terminal conclusion
(failure, cancelled, timed_out) raises exit 4 rather than waiting
out the timeout — there's no point waiting on a run that's already
decided.

## Transport model

The subscription-open / snapshot / fallback order is fixed:

1. **Open subscription.** Connect to the daemon socket + send
   `{"type":"subscribe"}`. Start buffering every incoming event. Do
   *not* evaluate yet. If the daemon is unreachable, skip this step
   and record `transport: "polling"`.
2. **Authoritative snapshot.** One `gh` call to evaluate the truth
   condition. Always runs, regardless of daemon state or
   `--no-fallback`.
3. **Matched?** Exit 0 with the observed snapshot. Drain and discard
   the event queue; close the subscription cleanly.
4. **Not matched + daemon available:** process buffered events in
   arrival order, then live events. Each event triggers a fresh
   authoritative `gh` re-evaluation. Ring-buffer replays and live
   events are indistinguishable by design.
5. **Not matched + daemon unavailable + fallback allowed:** poll `gh`
   on `--poll-interval`.
6. **Not matched + daemon unavailable + `--no-fallback`:** exit 6.

Because the subscription opens *before* the snapshot and buffering
starts immediately, any event that happened in the gap between
subscribe-open and snapshot-completion is captured in the buffer. If
the snapshot already reflects that transition, step 3 exits 0 and the
buffer is discarded. If it doesn't, step 4 drains the buffer and
catches the transition. No cursor semantics required.

## JSON output

```json
{
  "schema_version": 1,
  "command": "wait:pr",
  "matched": true,
  "condition": {"type": "pr_green", "pr": 151, "repo": "owner/repo", "head_sha": "f521fa9b"},
  "observed": {
    "pr": 151,
    "head_sha": "f521fa9b",
    "merge_state_status": "CLEAN",
    "checks": [
      {"name": "Linux", "state": "COMPLETED", "conclusion": "SUCCESS", "required": true}
    ],
    "advisory": [
      {"name": "Coverage", "state": "COMPLETED", "conclusion": "FAILURE", "required": false}
    ]
  },
  "transport": "daemon",
  "fallback_used": false,
  "events_received": 3,
  "elapsed_seconds": 12.4
}
```

Fields:

- `matched` — bool, `true` when the condition is satisfied.
- `condition` — echo of the inputs (normalized).
- `observed` — shape varies per subcommand. See truth-condition
  sections above.
- `transport` — `"daemon"` when a subscription was open,
  `"polling"` when the daemon was unreachable.
- `fallback_used` — `true` if the waiter started on the daemon and
  fell through to polling mid-wait (e.g. daemon exited).
- `events_received` — count of events that triggered a re-evaluation.
  Zero on pure-polling transport.
- `elapsed_seconds` — wall-clock since the CLI was invoked.

## MVP tradeoffs

- Multiple waiters on the same PR each do their own authoritative
  `gh` re-fetch per event. Practical max ~5–10 per machine; within
  the reconcile budget shipyard already assumes.
- The daemon's ring buffer holds 100 events. A waiter that reconnects
  after a long gap may miss history older than a few minutes. Not a
  correctness issue — the snapshot still runs.
- Rulesets / merge-queue governance → exit 7. Classic branch
  protection only.
- No cross-invocation singleton. Each `shipyard wait` is its own
  subscription + process.

## Detection gate (for agents)

Only invoke `shipyard wait` when:

- `command -v shipyard` returns a path, and
- the project has `.shipyard/config.toml` **or** `tools/shipyard.toml`.

Otherwise fall back to `gh run watch` / `gh pr checks --watch`.

## Always set `--timeout`

An unbounded wait in an agent workflow is how sessions hang. Pick
something realistic: 10–30 minutes for most checks, longer for a
full release. The defaults are intentionally modest so an agent
that forgets to set one fails fast rather than blocking forever.
