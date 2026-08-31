# Coding-tool integration

Shipyard works alongside coding tools such as Claude Code and Codex. The tool
does the reasoning and changes the code; Shipyard records and watches the
delivery work around it. That keeps a model from spending a long session
repeatedly asking whether a test or merge queue changed.

This integration is optional. A normal `shipyard run` or `shipyard ship` works
without a long-lived daemon. The daemon-backed continuation path is explicitly
enabled per trusted policy and handoff; it is designed to stop safely and ask
for help when its evidence is incomplete or code judgment is needed.

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

For the basic setup, a coding tool can hand a change to Shipyard:

1. You: "Implement the reverb effect and ship it"
2. Agent writes code, commits to a feature branch
3. The tool runs `shipyard ship`, which:
   - Pushes the branch
   - Creates a PR
   - Validates on all configured platforms
   - If all green, merges automatically
4. You come back to a merged change or a recorded reason it stopped

Whether a PR is merged automatically remains your repository policy. GitHub
keeps merge ordering; Shipyard only acts where its exact-head evidence and
configured authority allow it.

## Optional durable handoff

For a long-running or restart-prone task, an explicit steward handoff stores
the PR, exact commit, workstream context, and a bounded continuation route.
The trusted daemon can then receive verified GitHub webhooks, reconcile with
GitHub if an event is missed, and keep routine monitoring out of the coding
tool's context window. It escalates only when a decision or repair is needed.

This is deliberately not inferred from a terminal name or a chat transcript.
If the handoff is missing, stale, or no longer matches the current PR,
Shipyard refuses rather than guessing a replacement session. See
[launch profiles](launch-profile.md) and
[terminal and provider adapters](terminal-adapters.md) for the current
capability and recovery boundaries.

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
