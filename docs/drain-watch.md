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

One number for the daily report: gate-VM slots that sat idle while PRs waited. It follows the
m3 coordinator's `idle_gate_slot_minutes_while_waiting` (pulp-planning,
`architecture/2026-11-09-continuous-supply-data/idle_gate.py`, commit 1200a0775) so the two
numbers can be compared.

Same as the coordinator:

- **Waiting** is the coordinator's `waiting_state`, vendored unchanged into `drain_watch.py`
  (checked row for row on 8,853 real tick rows, no difference): not `queued` or `admitted`,
  not a draft, not label-excluded, not blocked only on its own running required checks;
  `capacity_queued` waits as `awaiting_slot`. State names are theirs: `dirty`, `behind`,
  `held`, `ejected`, `quarantined`, `awaiting_review`, `never_ran`, `red`, `awaiting_slot`,
  otherwise the first part of the `blocked_on` token. When the tick row carries `loop.state`
  it is used instead (`running` and `queued` do not wait; `hold` is renamed `held`).
- **Per repository**: a sample counts for a repository only while that repository has a
  waiting PR. `all_repos` counts a sample once when any repository waits (slots are shared).
- **Minutes**: idle slots times the time to the next sample, capped at 30 minutes; the last
  sample weighs nothing. The top 3 waiting states are PR-minutes over the same idle minutes.

Different, and why:

- **Idle slots are measured, not inferred.** Each watch run (every steward tick) probes
  `tartci fleet capacity --repo R --target T --json` on this host and on each host in
  `SHIPYARD_IDLE_SLOT_HOSTS` (space separated, over `ssh -o BatchMode=yes`) for each target in
  `SHIPYARD_IDLE_SLOT_TARGETS` (default `macos`), and sums `free_slots` over the probes whose
  `verification` is `verified`. The coordinator assumes 2 slots on each of m3, m5, m5s and m1
  and subtracts the VMs alive in lane data. The probe reflects tartci's own pool state, so a
  host with its pool off or no configured slots (m3 reports `configured_slots` 0 for macOS)
  adds nothing here but 2 slots there. Expect the coordinator's figure to be the larger one
  until that is reconciled. A failed or unverified probe adds nothing and makes the day
  `partial`; a day with no verified probe is `unavailable`, never 0.
- Samples are written to `idle-slots.jsonl` in the tick log directory (35 days kept) by
  `drain_watch.py`; `SHIPYARD_IDLE_SLOTS=off` disables sampling. `SHIPYARD_IDLE_SLOT_REPO`
  (default `danielraffel/Shipyard`) only selects the profile the probe hashes. Hosts are not
  hard-coded: set the variable on the steward host, for this fleet `m5 m5s m1` (m3 is local),
  with `SHIPYARD_IDLE_SLOT_TARGETS="macos linux"`.

The loss report (`tools/metrics/loss_report.py`) reads the file: `idle_gate_slots` per day, per
repository and for `all_repos`, in its JSON and markdown. Its daily run logs, for the previous UTC
day, one line per repository and one for all:

```
<stamp> idle-slots day=2026-11-12 repo=Generous-Corp/pulp idle_slot_minutes=1666.0 top=held:2382,red:2024,unmanaged:606 coverage=available samples=288 waiting_source=blocked_on
```

Known weaknesses shared with the coordinator's definition: `ready` (approved and green, admitted
next pass), `deferred_pass_budget` and `direct_merge_refused` (a manual-merge repository) count as
waiting, although an idle slot beside them is not a gate defect, and `unmanaged` counts although
the steward does not own the PR. They inflate the figure; they are left as is so the numbers agree.
