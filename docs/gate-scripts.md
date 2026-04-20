# Gate script path resolution

`shipyard pr` runs two Python gate scripts before handing off to
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

## Precedence between env var and config

Env wins. This mirrors every other Shipyard config lever so a
one-shot CI override (`SHIPYARD_SKILL_SYNC_SCRIPT=... shipyard pr`)
works without editing committed config.
