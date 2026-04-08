---
name: check
description: Check if a branch is ready to merge based on evidence
---

Check merge readiness for the current branch by examining evidence.

```bash
shipyard evidence --json
```

Parse the JSON output. Report:
- Branch name and current SHA
- Per-target evidence: which platforms have passing proof, which are missing
- Whether the branch meets the merge gate requirements

If not ready, explain what validation is still needed and suggest `/shipyard:run`.
