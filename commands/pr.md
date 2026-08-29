---
name: pr
description: One-shot push-a-PR — skill-sync gate, version-bump apply, commit, then hand off to `shipyard ship`. The canonical way to ship a branch when you want the versioning gates to run.
---

Run `shipyard pr` to orchestrate the full push-a-PR flow:

1. `scripts/skill_sync_check.py --mode=report` — hard-fails if a mapped path was touched without updating the corresponding `SKILL.md` (or without a `Skill-Update: skip skill=<name> reason="..."` trailer on a commit in the PR range).
2. `scripts/version_bump_check.py --mode=apply` — rewrites `Cargo.toml` and `.claude-plugin/plugin.json` for any surface whose code moved. `--no-apply-bumps` switches to report mode so missing bumps hard-fail instead of auto-applying.
3. `git commit` — single `chore: bump versions` commit using `--only` to pick up exactly the files the bump script touched (pre-staged files the user had in progress for other reasons stay in the index).
4. Hands off to `shipyard ship` for push + `gh pr create` + cross-platform validate + merge on green.
5. When the protected base branch has `[merge_steward].auto_handoff = true` (or `--workstream-id`/`--launch-profile` is supplied), writes the exact-head `shipyard/steward-handoff` receipt and managed label immediately after PR creation. With a valid private launch profile, it publishes a zero-wake durable daemon obligation before reporting monitoring transfer. Provider delivery is not part of this boundary. The PR branch cannot enable this project default for itself.

On merge, `.github/workflows/auto-release.yml` detects the `Cargo.toml` package-version move and creates a `v<x.y.z>` tag. The existing tag-triggered `release.yml` then builds + publishes the binaries.

Do NOT run `gh pr create` + `shipyard ship` separately. Do NOT run the skill-sync or version-bump scripts by hand — `shipyard pr` invokes them in the right order with the right flags.

If GitHub GraphQL quota is exhausted, Shipyard falls back from `gh pr list/create/view` to the REST Pulls API. That keeps the PR in Shipyard's tracker instead of forcing a manual `gh api` escape hatch that the GUI cannot see.

```bash
shipyard pr $ARGUMENTS
```

## Flags

- `--base <ref>` — base branch to ship into (default `main`).
- `--apply-bumps` / `--no-apply-bumps` — default is apply. `--no-apply-bumps` switches to report mode so the command refuses rather than auto-rewriting.
- `--allow-unreachable-targets` — forwarded to `shipyard ship`.
- `--workstream-id <id>` — opt into an atomic steward receipt and use this durable work item ID.
- `--context-url <url>` — receipt link; defaults to the newly-created PR URL.
- `--launch-profile <private-json>` — bind and publish a protected continuation; requires `--no-apply-bumps` because the profile binds the final exact head. Apply and commit required bumps before generating it. Never put secrets in this file.
- `--after-handoff <continue|pause>` — disposition after monitoring transfer; defaults to `continue`.
- `--task-graph <private-json>` — required with `pause`; proves that no independent runnable task remains.
- `--no-steward-handoff` — explicitly override a project-configured automatic handoff.

## Bypass trailers (tip commit, never PR body)

| Gate          | Trailer                                                      |
|---------------|--------------------------------------------------------------|
| Version bump  | `Version-Bump: <surface>=<patch\|minor\|major\|skip> reason="..."` |
| Skill update  | `Skill-Update: skip skill=<name> reason="..."`              |
| Auto-release  | `Release: skip reason="..."`                                 |

Run `shipyard doctor` first if the repo lacks a `RELEASE_BOT_TOKEN` secret — the release workflow chain won't fire on tag push until the secret is configured. `shipyard pr` prints a yellow heads-up when the secret is missing.
