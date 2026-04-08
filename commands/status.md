---
name: status
description: Show queue, active runs, and target health
---

Show current Shipyard status.

```bash
shipyard status --json
```

Parse the JSON output. Report:
- Queue state: pending jobs, running jobs, recent completions
- Active run details if one is in progress
- Target health: which targets are configured and reachable

Keep the summary concise. Highlight any issues (unreachable targets, failed recent runs).
