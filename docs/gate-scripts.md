# Gate script path resolution

`shipyard pr` runs two repository gate scripts before handing off to
`shipyard ship`:

- `skill_sync_check.py` — hard-fails when a mapped path was touched
  without a `SKILL.md` update.
- `version_bump_check.py` — rewrites per-surface version files.

It also needs to read the versioning config: `versioning.json`.

Shipyard's own repo keeps these under `scripts/`. Pulp keeps them
under `tools/scripts/`. Other consumer repos may keep them elsewhere
(or outside the repo entirely).

## Resolution order

For each file, `shipyard pr` tries these sources in order and uses the
first one that exists:

1. **Environment variable.** Highest priority; absolute or
   repo-root-relative path.

   | Script                  | Env var                         |
   |-------------------------|---------------------------------|
   | `skill_sync_check.py`   | `SHIPYARD_SKILL_SYNC_SCRIPT`    |
   | `version_bump_check.py` | `SHIPYARD_VERSION_BUMP_SCRIPT`  |
   | `versioning.json`       | `SHIPYARD_VERSIONING_CONFIG`    |

2. **`.shipyard/config.toml`** (per-repo override).

   ```toml
   [validation]
   skill_sync_script   = "tools/ci/skill_sync.py"
   version_bump_script = "tools/ci/version_bump.py"
   versioning_config   = "tools/ci/versioning.json"
   ```

3. **`tools/scripts/<file>`** — common CI-tooling layout.

4. **`scripts/<file>`** — Shipyard's own default.

## Error behavior

If a file can't be resolved, `shipyard pr` exits with code 2 and
prints every path it probed plus the override knobs. An env-var or
config override pointing to a missing path is a hard error — it will
not silently fall through to the defaults, because a broken override
almost always hides a typo or a stale path rather than an intentional
"fall back."

## Repositories without these gates

A repository that has no skill-sync or versioning gates at all (for example
tartci) declares that in `.shipyard/config.toml`:

```toml
[validation]
pr_gates = false
```

`shipyard pr` then skips both gates, says so, and goes straight to
`shipyard ship`. Only an explicit boolean `false` does this; a missing key,
`true`, or a string keeps the gates, and a missing script still fails as
described below. The opt-out is a declaration, never a fallback: a repository
that has gates and has lost a script must not ship silently.

## Precedence between env var and config

Env wins. This mirrors every other Shipyard config lever so a
one-shot CI override (`SHIPYARD_SKILL_SYNC_SCRIPT=... shipyard pr`)
works without editing committed config.
