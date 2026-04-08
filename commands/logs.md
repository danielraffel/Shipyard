---
name: logs
description: Show logs from a validation run
---

Show logs from a Shipyard validation run. The user should provide a job ID.

If the user provides a job ID:
```bash
shipyard logs <job_id>
```

If the user also specifies a target:
```bash
shipyard logs <job_id> --target <target_name>
```

If no job ID is given, first run `shipyard status --json` to find the most recent job, then show its logs.

Summarize the log output. Highlight errors, failures, and test results. Do not dump the entire log verbatim unless the user asks for it.
