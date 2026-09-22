---
description: Report finished validations that nobody consumed — failed or cancelled gates on still-open PRs.
---

Run `shipyard verdicts` and act on what it reports.

```sh
shipyard verdicts
```

This reads the ship-state records Shipyard already wrote and reports
validations that reached a terminal verdict whose pull request is still
open — the failures that no longer have a reader because the agent that
dispatched them finished its turn.

Exit codes: `0` nothing actionable and nothing unresolved · `1` actionable
verdicts exist · `5` the scan was partly blind.

Read the census line before drawing any conclusion:

- `passed + failed + cancelled` must equal `terminal`. If the command says
  the census does not reconcile, the scan is unreliable — do not report a
  result from it.
- `unresolved > 0` means some verdicts' pull-request state could not be
  read. Those are **unknown, not clean**. Raise `--limit` if they predate
  the lookup window, and never report an all-clear from such a scan.

For each actionable row, decide from the verdict: `failed` is investigated,
`cancelled` is re-run (`shipyard ship --pr <n> --base main`).

Use `--json` when another tool consumes the result; the envelope carries
the full unresolved list and a `supports_all_clear` judgement.

Lookups are batched per repository (`gh pr list`, paginated at 100), so a
wider `--limit` costs more pages but never more calls per record.
