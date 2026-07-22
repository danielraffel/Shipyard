# Agent Integration

Shipyard is designed for AI agents (Claude Code, Codex) that write code
across parallel worktrees and need real cross-platform validation before
merging.

## `shipyard init` handles this for you

When you run `shipyard init`, it detects whether you're using Claude Code
or Codex and offers to set up agent integration automatically:

```
$ shipyard init

  ...detecting project, configuring targets...

  Agent setup:
    Found: Claude Code (.claude/ directory detected)

    How should your agent handle merging?
      [1] Auto-merge — agent validates and merges to main automatically
      [2] Auto-merge to develop — agent merges to develop, you promote to main
      [3] Validate only — agent runs CI, you click merge manually
      [4] Skip agent setup

  Choice [1]: 1

  → Writing .claude/skills/ci.md
  → Adding CI instructions to CLAUDE.md

  Done. Your agent will now validate and merge automatically.
```

You don't need to copy files or edit configs. Init writes the right files
for your choice. You can re-run `shipyard init` later to change the setup.

## How it works after setup

Once configured, your agent handles CI end-to-end:

1. You: "Implement the reverb effect and ship it"
2. Agent writes code, commits to a feature branch
3. Agent runs `shipyard ship` which:
   - Pushes the branch
   - Creates a PR
   - Validates on all configured platforms
   - If all green, merges automatically
4. You come back, it's on main

This is how [Pulp](https://github.com/Generous-Corp/pulp) (the project
Shipyard was extracted from) operates daily.

## If you prefer manual merging

Option 3 during init sets up "validate only" — the agent runs
`shipyard run` to validate, but doesn't merge. You review the PR and
click squash-and-merge yourself. You still get cross-platform validation
without giving up control over what lands on main.

## Merging to develop instead of main

Option 2 during init sets up a develop branch flow. Agents merge to
`develop` automatically. You promote `develop` to `main` when ready:

```bash
git checkout develop
shipyard ship --base main    # validate develop, merge to main
```

## What init writes

Depending on your choice, init creates:

| File | What it does |
|------|-------------|
| `.claude/skills/ci.md` | Teaches Claude how to validate and ship |
| `CLAUDE.md` addition | CI instructions for Claude |
| `AGENTS.md` addition | CI instructions for Codex |

These are standard files in your repo. You can edit them, version them,
or delete them. Nothing hidden.
