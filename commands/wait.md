---
name: wait
description: Wait for a GitHub condition (release uploaded, PR green/merged/closed, run terminal) to match
---

Use `shipyard wait` any time you'd otherwise write a polling loop
around `gh` — dispatching a workflow and waiting for it, watching a
release for its artifacts to upload, or waiting for a PR's required
checks to go green. When the user runs `shipyard daemon start` the
waiter wakes in seconds on real webhook events; without a daemon it
falls back to polling, so this is always safe to use.

## Detection gate

Only use `shipyard wait` when both are true:

- `command -v shipyard` returns a path (the binary is installed).
- The current project has either `.shipyard/config.toml` or
  `tools/shipyard.toml` (i.e. the user opted in to Shipyard for this
  repo).

If either is missing, fall back to a hand-rolled `gh run watch` or
`gh pr checks --watch` loop.

## Subcommands

### `shipyard wait release <version>`

Waits for a GitHub release tagged `<version>` to exist with every
artifact in `[release.artifacts]` marked `state=uploaded`. If the
repo has no `[release.artifacts]` manifest, matches as soon as any
asset uploads.

```bash
shipyard wait release v0.23.0 --timeout 900 --json
```

### `shipyard wait pr <N> --state {green|merged|closed}`

Waits for PR `<N>` to reach the given state.

```bash
# All required checks on current HEAD pass.
shipyard wait pr 151 --state green --timeout 1800 --json

# PR is merged (branch protection passed + merge clicked).
shipyard wait pr 151 --state merged --timeout 3600 --json
```

### `shipyard wait run <run-id> [--success]`

Waits for an Actions workflow run to reach a terminal status. With
`--success` the run must end with conclusion=success; any other
terminal conclusion exits 4 immediately so you don't wait on a run
that's already decided.

```bash
shipyard wait run 22345678 --success --timeout 1200 --json
```

## Flags

| Flag | Default | Purpose |
|------|---------|---------|
| `--timeout SECONDS` | 600 / 1800 | Hard timeout. Always set one explicitly in agent workflows. |
| `--poll-interval SECONDS` | varies | Fallback polling cadence when no daemon is running. |
| `--no-fallback` | off | Exit 6 rather than poll if the daemon isn't reachable. |
| `--json` | off | Emit an `OutputEnvelope` with `matched`, `observed`, `transport`, `elapsed_seconds`. |

## Exit codes (branch on these)

- `0` — condition matched.
- `1` — `--timeout` elapsed.
- `4` — `wait run --success` reached a terminal-but-wrong state.
- `5` — invalid input (PR/run/release not found, bad tag).
- `6` — daemon unreachable + snapshot missed + `--no-fallback` set.
- `7` — unsupported scope (rulesets / merge-queue detected). Fall
  back to `gh` manually or ask the user to switch to classic
  branch protection.
- `130` — SIGINT / SIGTERM.

## JSON output shape

```json
{
  "schema_version": 1,
  "command": "wait:pr",
  "matched": true,
  "condition": {"type": "pr_green", "pr": 151, "repo": "owner/repo", "head_sha": "abc123"},
  "observed": {
    "checks": [{"name": "Linux", "conclusion": "SUCCESS", "required": true}],
    "advisory": []
  },
  "transport": "daemon",
  "fallback_used": false,
  "events_received": 3,
  "elapsed_seconds": 12.4
}
```

Agents should branch on `matched` + `transport`. `transport == "daemon"`
tells you the wait was webhook-driven (fast); `transport == "polling"`
tells you the daemon wasn't running and you got the fallback.

## When NOT to use

- The user wants a blocking live tail with progress UI — that's
  `shipyard watch`, not `wait`. `wait` exits once, without per-event
  output.
- You need ruleset / merge-queue governance evaluated — classic
  branch protection only. Exit 7 tells you to switch lanes.
- You're inside a bash script on a CI runner where `shipyard` isn't
  installed. Fall back to `gh run watch` / `gh pr checks --watch`.

Always set `--timeout`. An unbounded wait in an agent workflow is a
common way to get stuck.
