---
name: config
description: Inspect Shipyard configuration files and effective cloud defaults
---

`shipyard config` inspects configuration and switches the active profile.

Subcommands:

- `shipyard config show` — print the effective merged configuration (global →
  tracked `.shipyard/config.toml` → local `.shipyard.local/config.toml`).
- `shipyard config profiles` — list defined profiles and which one is active.
  The `--json` form also reports `active_source`
  (`local` | `tracked` | `global` | `none`) plus the resolved config paths, so a
  tool can tell whether the active profile is a per-machine override or the repo
  default.
- `shipyard config use <profile>` — switch the active profile by writing
  `[project].profile` to the **tracked** `.shipyard/config.toml` (a committed,
  everyone change).
- `shipyard config use <profile> --local` — switch it for **this machine only**
  by writing the **local overlay** `.shipyard.local/config.toml` instead; the
  tracked config is left untouched, so the switch never affects collaborators.
  In a git worktree this writes the current checkout's own overlay, not a
  borrowed main-checkout one.

If the user asks to "switch profiles", "go local", or "go cloud", use
`shipyard config profiles` to inspect options and `shipyard config use <profile>`
(add `--local` to keep the switch per-machine).

> GitHub repo Variables that route *everyone's* CI (e.g.
> `PULP_LOCAL_MACOS_RUNS_ON_JSON`) are a separate, repo-wide layer — manage those
> with `gh variable`, not `shipyard config`.

Other entry points:

- Environment and tool health: `shipyard doctor --json`
- Effective cloud workflow/provider resolution: `shipyard cloud defaults --json`
- Active job and target state: `shipyard status --json`

Examples:

**Inspect cloud defaults:**
```bash
shipyard cloud defaults --json
```

**Inspect current job and target state:**
```bash
shipyard status --json
```
