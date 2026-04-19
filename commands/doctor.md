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

## Optional probes

- `shipyard doctor --release-chain --json` — live-probes the
  `RELEASE_BOT_TOKEN` → auto-release → binaries chain by dispatching
  a workflow and waiting for the result.
- `shipyard doctor --runners --json` — probes every SSH-backed target
  in `.shipyard/config.toml` for reachability (5s per host, 8s
  timeout). Surfaces a dead SSH host without running a full ship.
  Skipped silently when the config has no SSH targets.
