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
| `merge_group_stalled` | A queued row the steward marked `merge_group_stalled:<context>`: every run on its merge group finished and a required context never reported. | Latest tick |
| `stuck_claim` | A directory under `pass-runner/repair-claims/<owner>_<repo>_<pr>/` whose `pid` file is 30 minutes or more old, while the PR's head in the latest tick still equals the head of the PR's last line in `repair-attempts.log`. It cannot see a comment without a push (no GitHub reads), and it does not check the pid, which is only meaningful on the host that runs the claim. | Claim dirs, `repair-attempts.log`, tick |
| `pass_fault` | New lines since the last run in `fleet-pass/*.log` and `*.last-output.txt` containing `PermissionError`, `Operation not permitted`, `command not found`, `getcwd`, `Unable to read current working directory`, `sr status timed out` or `Traceback`. Counted per signature and lane (the file name before the first dot, such as `repair-2`). | Offsets in the state file; a truncated or replaced file restarts at 0, a partial last line waits for its newline; the first ever run seeds at the end so history is not counted |

The `last-output` files hold agent transcripts, so a line that merely quotes a
signature counts. That is accepted noise; the evidence lines are in the issue.

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
