---
name: cloud
description: Dispatch and inspect GitHub Actions workflows from Shipyard
---

Use Shipyard's cloud subcommands to discover workflows, inspect dispatch defaults,
trigger workflow_dispatch runs, and inspect tracked cloud runs.

```bash
shipyard cloud workflows --json
shipyard cloud defaults --json
shipyard cloud run build --json
shipyard cloud status --json
```

When dispatching a workflow:
- Prefer the configured default workflow when the user does not specify one.
- Pass `--provider <name>` when the user wants to override the default runner provider.
- Pass `--wait` when the user wants the command to block until the GitHub run completes.

When summarizing JSON output, report:
- Workflow key/file/name
- Resolved provider and dispatch fields
- Tracked run ID, status, conclusion, and URL when available
