# Drain watch

`tools/metrics/drain_watch.py` catches stalled drains and runner faults without
anyone reading a log. The steward tick starts it detached (same pattern as the
fleet deploy and the loss report), from the deploy clone at `origin/main`. It
reads local files only, takes its own lock, and exits 0 whatever happens;
`SHIPYARD_DRAIN_WATCH=off` turns it off.

## What it checks

| Kind | Fires when | Reads |
| --- | --- | --- |
| `drain_stall` | A repository has an actionable PR (same classification as the loss report: `required_pending` and other waits are normal) waiting 2 hours or more, and no PR of that repository went from `queued`, `admitted` or `ready` to gone in the last 2 hours. Needs about 1h45m of tick history and a tick in the last 30 minutes, else no verdict. | Live tick JSON |
| `stale_hold` | A `held` / `held:*` row whose `since` is 45 minutes or more old, so no new head. | Latest tick |
| `stuck_claim` | A directory under `pass-runner/repair-claims/<owner>_<repo>_<pr>/` whose `pid` file is 30 minutes or more old, while the PR's head in the latest tick still equals the head of the PR's last line in `repair-attempts.log`. It cannot see a comment without a push (no GitHub reads), and it does not check the pid, which is only meaningful on the host that runs the claim. | Claim dirs, `repair-attempts.log`, tick |
| `pass_fault` | New lines since the last run in `fleet-pass`, in the runner's own logs (`<lane>.log` and `<lane>.launchd.log`; the `*.last-output.txt` agent transcripts are not scanned) containing `PermissionError`, `Operation not permitted`, `command not found`, `getcwd`, `Unable to read current working directory`, `sr status timed out` or `Traceback`. Counted per signature and lane (the file name before the first dot, such as `repair-2`). | Offsets in the state file; a truncated or replaced file restarts at 0, a partial last line waits for its newline; the first ever run seeds at the end so history is not counted |

A line with a leading ISO timestamp is attributed to that time, any other line
to the preceding timestamped line (remembered per file across runs). A line
attributed to a time before the last run (minus 15 minutes; 2 hours before now on a
first run) is old content and is not counted, so a quoted old error never counts as
new. A line with no timestamp before it cannot be proven old and is counted.

## Output

- One line in `summary.log` the first time a finding is seen:
  `drain-watch kind=<kind> repo=<r> lane=<l> detail=<...> first_seen=<ts> count=<runs>`.
  After that it is counted in `drain-watch-state.json`, not announced again.
- On the second consecutive run in which the finding persists, one issue in
  `danielraffel/Shipyard` through `ghapp api`, labelled `drain-watch`, carrying
  `<!-- drain-watch:<key> -->` and the evidence. The marker finds the issue again
  if the state is lost, so there are no duplicates. The body is refreshed at most
  hourly.
- After 3 consecutive runs without the finding the issue is commented on and
  closed, and the state entry is dropped.
- A failed issue call is one `drain-watch ERROR` line; the finding is retried next run.

`python3 -I tools/metrics/drain_watch.py --dry-run [--from-start]` prints what
would be announced and filed and changes nothing. `--from-start` counts log
lines from the beginning on a first run.

## Idle gate-slot minutes

One number for the daily report: gate-VM slots that sat idle while PRs waited.
Each watch run (every steward tick) takes one sample and appends it to
`idle-slots.jsonl` in the tick log directory (35 days kept):

- **Idle slots:** `tartci fleet capacity --repo R --target T --json` on this host and, for each
  host in `SHIPYARD_IDLE_SLOT_HOSTS` (space separated, over `ssh -o BatchMode=yes`), for
  each target in `SHIPYARD_IDLE_SLOT_TARGETS` (default `macos`). The probe is
  observation-only and reports one host at a time: `free_slots`, `eligible_slots`,
  `verification` and `pool_state`. A fleet total is the sum of `free_slots` over the
  probes whose `verification` is `verified`; a probe that failed or is `unknown` adds
  nothing and makes the day `partial`. Hosts are not hard-coded: set the variable on the
  steward host. `SHIPYARD_IDLE_SLOT_REPO` (default `danielraffel/Shipyard`) only selects the
  profile the probe hashes. `SHIPYARD_IDLE_SLOTS=off` disables sampling.
- **PRs waiting:** open PRs whose `loop.state` is anything except `running` or `queued`. Until
  the tick JSON carries `loop`, the `blocked_on` token is mapped onto the same names
  (`merge_state:DIRTY` is `dirty`, `BEHIND` is `behind`, `required_failed` and
  `required_never_ran` are `red`, `held*` and carried commits are `hold`, `ejected_same_head`
  is `ejected`, `no_verdict`, `approval_excluded` and a reviewer-owned `required_pending` are
  `awaiting_review`; running checks, queue states, drafts and human stops do not wait). The
  report says which source a day used (`reasons_source`).
- **Minutes:** idle slots x seconds since the previous sample (capped at 10 minutes, so a gap
  in sampling is not billed), counted only while at least one PR waited and at least one
  probe was verified. The first sample ever has no interval and is not counted.
- **Reasons:** each counted sample's minutes are split over the waiting states in proportion
  to the PRs in each; the top 3 by minutes are reported with their share.

The slot count is not matched to the repository whose PRs wait (a macOS slot idle while only
Linux PRs wait still counts). That makes the figure an upper bound on avoidable idleness;
`--gate-target` and the probe hosts narrow it.

The loss report (`tools/metrics/loss_report.py`) reads the file: `idle_gate_slots` per day in
its JSON and markdown, and its daily run logs one line for the previous UTC day:

```
<stamp> idle-slots day=2026-11-12 idle_slot_minutes=20.0 top=dirty:47.5%,ejected:20.0%,red:12.5% coverage=available samples=288 reasons=blocked_on_mapping
```

A day with no samples is `unavailable`, never 0. Coverage is `available` when every probe was
verified and every interval measured, `partial` otherwise, `unavailable` when no probe was verified.
