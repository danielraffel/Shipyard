---
name: run
description: Run cross-platform validation on the current branch
---

Run Shipyard validation on configured targets.

```bash
shipyard run --json
```

If the user specifies targets, pass them:
```bash
shipyard run --targets mac,ubuntu --json
```

For smoke (fast) validation:
```bash
shipyard run --smoke --json
```

Parse the JSON output. Report:
- Job ID and branch/SHA being validated
- Per-target status (pass/fail/error) with duration
- Overall result (all green or which targets failed)

If a target fails, offer to show logs with `/shipyard:logs`.
