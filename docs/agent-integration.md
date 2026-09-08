# Coding-tool integration

Shipyard works alongside coding tools such as Claude Code and Codex. The coding
tool does the reasoning and changes the code; Shipyard records and watches the
delivery work around it. That keeps a model from spending a long session
repeatedly asking whether a test or merge state changed.

This integration is optional. A normal `shipyard run` or `shipyard ship` works
without a long-lived daemon. The durable steward handoff is enabled only by
explicit policy and a specific, exact-head handoff. It stops safely when its
evidence is incomplete or code judgment is needed.

## Start with the CLI

```bash
shipyard init
# Review .shipyard/config.toml and add the targets you intend to use.
shipyard run
shipyard ship
```

`shipyard init` detects the project, writes `.shipyard/config.toml`, and adds
the local Shipyard overlay directory to `.gitignore`. It does not probe or
enroll machines, choose a merge policy, or edit `CLAUDE.md` or `AGENTS.md`.
Configure targets and repository policy deliberately after reviewing the
generated file.

For monitoring, recovery, fleet, governance, and release commands, use the
[CLI reference](cli-reference.md). Coding-tool integrations call the same CLI;
they do not change its safety rules.

## How a handoff works

For a basic delivery:

1. A person or coding tool makes the change and commits it on a branch.
2. It runs `shipyard ship`.
3. Shipyard validates the exact commit on the configured targets and follows
   the repository's approved PR and merge policy.
4. Shipyard either completes the supported routine steps or records why the
   work needs help.

GitHub remains authoritative for pull requests, required checks, and merge
order. Shipyard does not replace GitHub or the project's build system, and it
does not invent product decisions or silently edit code.

## Optional durable handoff

For long-running or restart-prone work, an explicit steward handoff records the
PR, exact commit, and workstream context on GitHub itself: a successful
`shipyard/steward-handoff` commit status on that immutable head, the
`shipyard:managed` label, and a private crash-consistent JSON receipt beside
them. `shipyard runner steward` then treats that exact head as managed, and the
trusted daemon can receive verified GitHub events, reconcile with GitHub when an
event is missed, and keep routine monitoring outside the coding tool's context
window. It asks for help when a decision or repair is needed.

The private receipt is local to the machine that wrote it. If the handoff is
missing, stale, or no longer matches the current PR head, Shipyard refuses
rather than guessing a replacement.

See [terminal and provider adapters](terminal-adapters.md) for the current
cmux, HerdR, and provider-routing boundaries.

## Coding-tool choices

- **Claude Code:** the optional Shipyard plugin provides slash commands,
  skills, and hooks around the CLI.
- **Codex and other coding tools:** install the CLI and put repository-specific
  delivery instructions in the tool's normal project guidance, such as
  `AGENTS.md`.
- **No coding-tool integration:** run the CLI directly. Shipyard's validation
  and repository-policy checks are the same.

Automatic merging is never implied by the coding tool. It remains an explicit
repository policy. A validate-only workflow can stop after `shipyard run` or
leave the final GitHub merge to a person; a queue-governed repository leaves
merge ordering to GitHub.
