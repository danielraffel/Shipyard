# shipyard versioning-sync — acceptance transcript

Captured on branch `versioning-sync` ahead of the merge PR to `main`.

---

## 1. Both gate scripts exist and work against Shipyard's layout

Config-driven — the same scripts pulp ships work unchanged against Shipyard's `pyproject.toml` + `src/shipyard/__init__.py` (CLI surface) and `.claude-plugin/plugin.json` (plugin surface).

```
$ python3 scripts/skill_sync_check.py --base origin/main --config scripts/versioning.json --mode=report
[ci] ✓ SKILL.md updated
exit=0

$ python3 scripts/version_bump_check.py --base origin/main --config scripts/versioning.json --mode=report
[cli] Shipyard CLI: no bump needed
[plugin] Shipyard Claude plugin: heuristic=patch final=patch current=0.1.1 ? bump suggested (patch)
exit=0
```

The plugin surface shows `? bump suggested (patch)` — advisory only, not a hard fail (patch-level verdicts don't block merge). The touched paths are `commands/`, `hooks/`, `skills/` additions that are internal-only per Shipyard's `internal_only_paths` config (`hooks/*.sh`); the rest of the change adds new plugin policy material that could reasonably be called either patch or minor. Left at "suggested" so the human author (or the PR reviewer) can bump if they want; not required.

## 2. Manual shipyard pr exercise

`shipyard pr --help` renders the full synopsis and passes the flags through to `shipyard ship`:

```
$ uv run shipyard pr --help
Usage: shipyard pr [OPTIONS]

  One-shot push-a-PR: skill-sync + version-bump + ship.

  Mirrors pulp's `pulp pr` for parity with the ci skill's natural- language
  triggers ("push a PR", "ship this"). Internally:

      1. scripts/skill_sync_check.py --mode=report
      2. scripts/version_bump_check.py --mode=report
      3. invokes `shipyard ship` for push + PR + validate + merge

Options:
  --base TEXT                     Base branch to ship into (default: main)
  Version files are assigned after merge; no apply flags exist.
```

## 3. pytest green

```
$ uv run pytest -x
...
tests/test_cli.py ....                                                   [ 91%]
tests/test_cli_codex_regressions.py ......                               [ 92%]
tests/test_cli_governance_diff.py .                                      [ 92%]
tests/test_codex_review_p1_batch.py ..........                           [ 94%]
tests/test_governance_use_rewrite.py .....                               [ 95%]
tests/test_preflight.py .....                                            [ 96%]
tests/test_ship_auto_create_base.py .........                            [ 98%]
tests/test_windows_false_green.py ........                               [100%]

======================= 494 passed in 275.07s (0:04:35) ========================
```

All 494 tests pass. The new `shipyard pr` command didn't introduce any regression — it's additive and delegates to the existing `ship` command for push/PR/validate/merge.

## 4. No existing drift to fix

`git diff --name-only v0.2.0..origin/main` is empty — Shipyard's main is exactly at the v0.2.0 release tag, so there's no prior-work drift to reconcile. Any new bumps originate with this PR (the plugin-surface patch-suggested above).

## 5. End-to-end auto-release — deferred until after merge

Same rationale as the pulp acceptance doc: the full loop (PR → merge → auto-release.yml fires → tag → release.yml builds + publishes) only proves itself once the PR lands on `main`. That evidence is captured in a follow-up commit (or the release page) after this PR merges.

## Known follow-ups (not blockers for merge)

- Plugin-version patch-suggested: reviewer decides whether to bump `plugin.json` from 0.1.1 → 0.1.2 in this PR or leave it for a follow-up. Gate is advisory at patch-level; not blocking.
- `shipyard pr` currently always delegates to `ship` with the default ctx.invoke — when Shipyard adds per-stage JSON output flags to `ship`, `pr` should forward those through too.
- `scripts/install-githooks.sh`, `scripts/skill_sync_check.py`, and `scripts/version_bump_check.py` are copied verbatim from pulp. When pulp updates the scripts, re-vendor here (or revisit Open Question 1 by publishing to PyPI / adding a submodule).
