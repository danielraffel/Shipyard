---
name: ci
description: Cross-platform CI coordination with Shipyard
---

# CI Operations with Shipyard

Shipyard coordinates validation across local, SSH, and cloud targets. Use the CLI for all operations.

## Quick reference

| Task | Command |
|------|---------|
| Validate current branch | `shipyard run` |
| Validate specific targets | `shipyard run --targets mac,ubuntu` |
| Fast smoke check | `shipyard run --smoke` |
| Check merge readiness | `shipyard evidence` |
| Full ship flow (PR + validate + merge) | `shipyard ship` |
| Show queue and status | `shipyard status` |
| Show run logs | `shipyard logs <job_id>` |
| Environment check | `shipyard doctor` |

## Ship workflow (merge on green)

1. Work on a feature branch. Commit your changes.
2. Run `shipyard run` to validate on all configured targets.
3. Run `shipyard evidence` to verify all platforms show passing proof for the current SHA.
4. Run `shipyard ship` to create a PR and merge when all platforms are green.

Shipyard will refuse to merge unless every required platform has passing evidence for the exact HEAD SHA.

## Adding --json for structured output

All commands accept `--json` for machine-readable output. Use this when parsing results programmatically:

```bash
shipyard run --json
shipyard status --json
shipyard evidence --json
```

## Target configuration

Targets are defined in `.shipyard/config.toml`:

```toml
[targets.mac]
backend = "local"
platform = "macos-arm64"

[targets.ubuntu]
backend = "ssh"
host = "ubuntu"
platform = "linux-x64"
```

## Troubleshooting

- `shipyard doctor` checks that git, ssh, gh, and nsc are installed.
- `shipyard logs <job_id> --target <name>` shows the full log for a specific target.
- If a target is unreachable, Shipyard will report it as an error in the run results.
