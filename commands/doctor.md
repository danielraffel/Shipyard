---
name: doctor
description: Check environment, dependencies, and target connectivity
---

Run the Shipyard doctor to verify the environment is set up correctly.

```bash
shipyard doctor --json
```

Parse the JSON output. Report:
- Which core tools are installed (git, ssh) with versions
- Which cloud providers are available (gh, nsc)
- Overall readiness status

If something is missing, explain how to install it.
