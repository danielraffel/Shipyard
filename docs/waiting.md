# Waiting on conditions (`shipyard wait`)

`shipyard wait` is the primitive for "I need to block until durable work or
something on GitHub reaches a known state." Four truth conditions:

- **`wait release <version>`** — release tag exists, artifacts uploaded.
- **`wait pr <N> --state {green|merged|closed}`** — PR reached a state.
- **`wait run <id> [--success]`** — workflow run reached terminal.
- **`wait job <sy-id> [--success]`** — durable Shipyard queue job reached terminal.

It replaces hand-rolled `gh`-polling loops. With a running daemon, relevant
webhook events trigger immediate authoritative snapshots, and periodic
reconciliation heals missed events. When the daemon isn't running or
disconnects, it falls back to `gh` polling — same truth condition, just slower.

## Invocation

```sh
shipyard wait release v0.23.0 --timeout 900 --json
shipyard wait pr 151 --state green --timeout 1800 --json
shipyard wait pr 151 --state merged --timeout 3600 --json
shipyard wait run 22345678 --success --timeout 1200 --json
shipyard wait job sy-20260901-example --success --timeout 1200 --json
```

All four subcommands accept `--timeout`, `--poll-interval`, and `--json`.
GitHub-backed release/PR/run waits also accept `--no-fallback`; queue-backed
job waits do not need a network fallback.

| Flag | Default | Meaning |
|------|---------|---------|
| `--timeout SECONDS` | 600 (release), 1800 (pr/run/job) | Hard deadline. |
| `--poll-interval SECONDS` | 2 (release/job), 30 (pr), 15 (run) | Authoritative reconciliation cadence. |
| `--no-fallback` | off | GitHub waits only: exit 6 if the daemon isn't available and the snapshot doesn't already match. |
| `--json` | off | Emit a structured envelope. |

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | condition matched |
| 1 | `--timeout` elapsed |
| 4 | A success condition became impossible: `wait run/job --success` failed, or every exact-head required PR check finished and at least one failed |
| 5 | invalid input (PR/release/run/job not found, bad tag, wrong ID class) |
| 6 | daemon unreachable + snapshot didn't match + `--no-fallback` |
| 7 | unsupported scope — rulesets / merge-queue detected |
| 130 | SIGINT / SIGTERM |

**These codes are `wait`'s, and they are not `watch`'s.** The two commands
answer different questions and number their outcomes independently:

| Code | `wait` | `watch` |
|---|---|---|
| 1 | `--timeout` elapsed | terminal verdict observed, and it failed |
| 3 | *(unused)* | still in flight when a bounded watch ended |
| 4 | a success condition became impossible | ship state archived — outcome undetermined |

So `1` means *"I stopped waiting"* to `wait` and *"it failed"* to `watch`, and a
caller that branches on a bare `$?` without knowing which command produced it
will read one of them backwards. Branch per command, or read the `--json`
envelope, which names the outcome instead of encoding it.

This is a documented difference rather than a defect in either command: each
table is internally coherent, and renumbering one to match the other would
break its own contract for no gain. The hazard is generalising across them.

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

The `release.published` webhook can trigger an immediate refresh. GitHub emits
no dedicated asset-upload event, so the waiter also re-evaluates on
`--poll-interval` until every manifested asset is `uploaded` or `--timeout`
expires. The same cadence heals a missed publication event. The only time
budget is `--timeout` — there's no hidden asset-watch sub-timeout.

### `wait pr <N> --state green`

Matches when, for PR `<N>`'s current head SHA, every classic
branch-protection required check has conclusion ∈ `{SUCCESS, NEUTRAL,
SKIPPED}`.

Conclusion mapping:

- `SUCCESS`, `NEUTRAL`, `SKIPPED` → passing
- `FAILURE`, `ERROR`, `TIMED_OUT`, `CANCELLED`, `ACTION_REQUIRED`,
  `STARTUP_FAILURE`, `STALE` → failing
- `QUEUED`, `IN_PROGRESS`, `PENDING` → still waiting

If every required check for the observed exact head is terminal and at least
one failed, the waiter exits 4 immediately. It does not remain subscribed in
case an external actor reruns a check. If any required check is still active,
the waiter continues normally; if the PR head moves while it is waiting, the
next authoritative snapshot is evaluated solely for the new head.

Re-evaluated immediately on every `check_run` / `check_suite` /
`workflow_run` / `reconcile_healed` event that pertains to this PR, and
periodically on `--poll-interval` if no relevant event arrives.

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

`wait run` accepts GitHub numeric run IDs only. A `sy-*` identifier is rejected
before repository or GitHub lookup with guidance to use `wait job`; this keeps
a durable queue ID from being misread as an absent GitHub run.

### `wait job <sy-id> [--success]`

Reads the durable Shipyard queue under `--state-dir` until the job is
`completed` or `cancelled`. Without `--success`, either terminal state matches.
With `--success`, only `completed` with every configured target passed matches;
a failed or cancelled terminal job exits 4 immediately.

Pending and running jobs remain nonterminal even when their log is absent or a
log lookup cannot find a GitHub run. A missing queue record exits 5 with
`terminal:null` and `passed:null` in JSON. This waiter uses
`transport:"queue"`; it makes no GitHub request and survives a CLI restart
because the queue is durable.

## GitHub transport model

The subscription-open / snapshot / fallback order is fixed:

1. **Open subscription.** Connect to the daemon socket + send
   `{"type":"subscribe"}`. Start buffering every incoming event. Do
   *not* evaluate yet. If the daemon is unreachable, skip this step
   and record `transport: "polling"`.
2. **Initial authoritative snapshot.** Fetch GitHub state to evaluate the truth
   condition. This always runs, regardless of daemon state or
   `--no-fallback`. A transient token-helper/network preparation failure is
   retried inside the same process with bounded backoff and the same overall
   `--timeout`; permanent configuration or credential errors still fail
   immediately.
3. **Matched?** Exit 0 with the observed snapshot. Drain and discard
   the event queue; close the subscription cleanly.
4. **Not matched + daemon available:** process buffered events in
   arrival order, then live events. Each relevant event triggers an immediate
   authoritative `gh` re-evaluation. If no relevant event arrives, reconcile
   from an authoritative snapshot every `--poll-interval`; this heals missed
   webhooks without leaving daemon transport or counting as fallback.
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
  "transient_errors": 1,
  "elapsed_seconds": 12.4
}
```

Fields:

- `matched` — bool, `true` when the condition is satisfied.
- `condition` — echo of the inputs (normalized).
- `observed` — shape varies per subcommand. See truth-condition
  sections above.
- `transport` — `"daemon"` when the daemon subscription remained available;
  this is transport evidence, not proof that an event caused the match.
  `"polling"` means the daemon was unavailable or disconnected and fallback
  polling was used.
- `fallback_used` — `true` if the waiter started on the daemon and
  fell through to polling mid-wait (e.g. daemon exited).
- `events_received` — count of relevant events that triggered an immediate
  re-evaluation. A value greater than zero is event evidence; zero on daemon
  transport means the initial or periodic authoritative snapshot may have
  produced the result. It is also zero on pure-polling transport.
- `transient_errors` — count of recoverable snapshot/auth failures retried
  without dropping the waiter. Permanent auth/configuration failures are not
  counted because they terminate immediately. The count is preserved when
  `wait run --success` exits 4 for a terminal non-success conclusion.
- `elapsed_seconds` — wall-clock since the CLI was invoked.

Credential preparation and each `gh` snapshot attempt share the waiter's
remaining overall timeout. A retry is never started after that deadline, and a
snapshot subprocess that consumes the remaining budget is terminated.

## MVP tradeoffs

- Multiple waiters on the same condition each do their own authoritative `gh`
  fetch for every relevant event and every `--poll-interval`. This periodic
  per-waiter REST/GraphQL cost is the correctness tradeoff that heals missed
  events; use a practical interval and avoid redundant waiters.
- The daemon's ring buffer holds 100 events. A waiter that reconnects
  after a long gap may miss history older than a few minutes. Not a
  correctness issue — periodic authoritative reconciliation still runs.
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
