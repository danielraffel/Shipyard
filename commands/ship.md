---
name: ship
description: Push, create PR, validate, and merge on green
---

Run the full ship flow: push to remote, create or find a PR, validate on all required platforms, and merge if all green.

```bash
shipyard ship --json
```

If the repo needs to override preflight for a one-off run:
```bash
shipyard ship --allow-root-mismatch --allow-unreachable-targets --json
```

Parse the JSON output. Report:
- PR number and URL
- Preflight warnings when present
- Validation status per platform (passing, missing, failing)
- Whether the merge happened or what is still needed

If not all platforms are green yet, explain which ones are missing and suggest running `/shipyard:run` to validate them.

If the merge succeeded, confirm it and note the PR was merged.
