# CLAUDE.md

Guidance for Claude Code (and Codex via `AGENTS.md → CLAUDE.md`) when working in this repository.

## What is Shipyard

Shipyard is a cross-platform CI controller: local VMs, SSH hosts, and cloud runners (Namespace / GitHub-hosted) coordinated under one queue-and-evidence model. It is designed so agents, not humans, are the primary users.

The project is Python-packaged (`pyproject.toml`, `src/shipyard/`) and ships a Claude Code plugin (`.claude-plugin/`, `commands/`, `skills/`, `agents/`, `hooks/`) delivered directly from this git repo.

## Two independently-versioned surfaces

| Surface        | Where                                            | When it moves                                  |
|----------------|--------------------------------------------------|------------------------------------------------|
| CLI binary     | `pyproject.toml` `project.version`, `src/shipyard/__init__.py` `__version__` | `src/shipyard/` changes that affect behavior   |
| Claude plugin  | `.claude-plugin/plugin.json` `version`           | `commands/`, `skills/`, `agents/`, `hooks/`, or `.claude-plugin/` changes |

These are **decoupled by policy** (`RELEASING.md`) — plugin files are delivered from git, not the binary, so a plugin-only change is not a binary release. The gate just ensures each surface's version moves when its own code moves.

## Versioning & Skill-Sync Policy

Enforcement runs in three layers, all calling the same two scripts.

| Layer | Where | Mode |
|---|---|---|
| 1 (agent hook) | `hooks/hooks.json` PostToolUse | `--mode=hint` — advisory text only |
| 2 (pre-push)   | `.githooks/pre-push` (install via `scripts/install-githooks.sh`) | `--mode=report` advisory by default; `SHIPYARD_ENFORCE_PREPUSH=1` upgrades to hard fail |
| 3 (CI)         | `.github/workflows/version-skill-check.yml` | `--mode=report` with `SHIPYARD_ENFORCE_PREPUSH=1` — blocks merge |

Scripts:

- `scripts/version_bump_check.py` — detects which surfaces need a bump. Heuristic (public-API vs internal paths) + conventional-commit signals + explicit `Version-Bump:` trailer override. `--mode=apply` rewrites version files in place.
- `scripts/skill_sync_check.py` — hard-fails when a mapped path is touched without the corresponding `SKILL.md` update. The map is `scripts/skill_path_map.json`; every dir under `skills/` must appear in it.

Full design: borrowed from `pulp` (upstream) — the schema and scripts are shared. See the pulp repo's `docs/guides/versioning.md` for the source of truth on the design.

### Shipping a PR

Whenever an agent workflow needs to wait on a GitHub condition — a
release to finish uploading, a PR's required checks to go green, a
dispatched workflow run to finish — reach for `shipyard wait ...`
instead of hand-rolling a `gh`-polling loop. Daemon-backed (wakes in
seconds on real events) with a transparent polling fallback when no
daemon is running. Always set `--timeout`. See
[`skills/ci/SKILL.md#waiting-on-conditions-shipyard-wait`](skills/ci/SKILL.md)
for patterns and exit-code reference.

When the user says "push a PR", "ship this", "ship it", "we're done", "merge this", or similar, invoke `shipyard pr` (or `shipyard ship` if that name is reserved for release today — check `skills/ci/SKILL.md`). It orchestrates:

1. `skill_sync_check.py --mode=report` — hard-fails on missing SKILL.md updates.
2. `version_bump_check.py --mode=apply` — applies the right bump per surface.
3. `git commit` + `gh pr create` + CI validate + merge on green.
4. `.github/workflows/auto-release.yml` tags the moved CLI version on merge; the existing tag-triggered `release.yml` publishes binaries.

Never invoke `gh pr create` + release separately. Never run the version-bump or skill-sync scripts by hand.

### Ship resume

`shipyard ship` auto-resumes an interrupted ship when a per-PR state file
exists under `<state_dir>/ship/<pr>.json`. If a session dies mid-wait, the
next `shipyard ship` invocation continues from the same dispatched run
IDs. SHA or merge-policy drift refuses the resume — re-run with
`--no-resume` to discard. See `docs/ship-resume.md`.

### Unattended merge + live watch (v0.8.0+)

Two commands that decouple the usual "ship, park, merge" loop so an agent
doesn't have to stay alive for the full cycle:

- `shipyard watch [--pr <n>]` — live-tails the ship state. NDJSON events
  under `--json`. Exits 0 pass / 1 fail / 2 not-found / 3 in-flight /
  130 SIGINT.
- `shipyard auto-merge <pr>` — cron-friendly one-shot: inspect state,
  merge if all green, idempotent on re-run. Pair with `shipyard ship` on
  the dispatch side and a cron/systemd timer / GH Actions schedule on
  the merge side.

### Release-bot setup

`shipyard release-bot setup` is the guided path for wiring up
`RELEASE_BOT_TOKEN` on a fresh consumer repo. Detects current state,
opens a pre-filled PAT creation URL, stores the secret via stdin-piped
`gh secret set`, then dispatches a workflow run to prove
`actions/checkout` accepts the token. `shipyard release-bot status` for
diagnosis; `shipyard doctor --release-chain` for the live probe.

### Post-release docs sync (v0.9.0+)

`shipyard changelog` + `shipyard release-bot hook` close the "tag
landed but CHANGELOG stayed stale" gap. Opt-in via
`[release.changelog]` and `[release.post_tag_hook]` in
`.shipyard/config.toml`; absent sections are a no-op.

- `shipyard changelog init` scaffolds both sections and backs up any
  existing CHANGELOG.md to `CHANGELOG.md.pre-shipyard.bak` (shipyard
  owns the file after opt-in).
- `shipyard changelog regenerate` walks the tag graph and writes
  `CHANGELOG.md`. `--check` is a drift gate (exit 1). `--release-notes
  <TAG>` prints per-release markdown for `softprops/action-gh-release`
  bodies.
- `shipyard release-bot hook install` drops
  `.github/workflows/post-tag-sync.yml`. Shipyard owns the file — re-
  install overwrites; uninstall is a plain `rm`. The workflow installs
  shipyard via `curl … install.sh | sh` (pinned) and runs `shipyard
  release-bot hook run`.

Agents should invoke `/shipyard:changelog regenerate` when a release
just landed and the commit-back didn't fire, or `/shipyard:changelog
check` when CI flags drift. See `docs/post-release-sync.md` for the
full flow + pulp migration notes.

### Mid-flight runner switch

`shipyard cloud retarget --pr <n> --target <lane> --provider <prov>`
switches one in-flight lane (e.g. macOS local → Namespace) without
tearing down the other target jobs on the PR. Dry-run by default; add
`--apply` to execute.

`shipyard cloud add-lane --pr <n> --target <lane> [--provider <prov>]`
is retarget's sibling: append a *new* lane to an in-flight PR without
re-dispatching the full matrix. Idempotent (no-op if target already
tracked); refuses if the ship is past dispatch phase. Dry-run by
default; add `--apply` to execute.

### Cross-PR evidence reuse

Opt-in per target via `reuse_if_paths_unchanged = ["src/backend/**"]`
in `[targets.<name>]`. When HEAD's diff since an ancestor SHA doesn't
touch any matching path, Shipyard borrows that ancestor's passing
evidence and skips dispatch. Safety: refuses across non-fast-forward
lineage, validation-contract drift, or stage-list drift. Surfaces as
`status: "reused"` in `shipyard watch --json` (and `✓ reused (from
a1b2c3)` in human mode). See `skills/ci/SKILL.md` for details.

### Trailer shortcuts

`shipyard pr --skip-bump <surface> --bump-reason "..."` and
`--skip-skill-update <skill> --skill-reason "..."` auto-append the
matching `Version-Bump:` / `Skill-Update:` trailer onto the tip commit
before the gates run. Refuses when the index is dirty (would fold in
staged changes) and replaces stale per-surface/per-skill trailers
rather than stacking them.

### Bypass trailers (tip commit, never PR body)

| Gate          | Trailer                                                      |
|---------------|--------------------------------------------------------------|
| Version bump  | `Version-Bump: <surface>=<patch\|minor\|major\|skip> reason="..."` |
| Skill update  | `Skill-Update: skip skill=<name> reason="..."`              |
| Auto-release  | `Release: skip reason="..."`                                 |

## Manual fallback release

`./scripts/release.sh` remains as a break-glass manual path. The default is the automatic path above (auto-release workflow on merge). Do not call `release.sh` directly unless the automatic path is genuinely blocked.

## macOS release is a stapled .dmg, built LOCALLY

As of #219 (2026-04-24 evening), macOS binaries ship as stapled **.dmg** artifacts built on the maintainer's Mac. CI handles Linux + Windows. The `.dmg` wrapper puts the notarization ticket **inside** the artifact so Gatekeeper verifies offline — no online notarization check, no per-Mac taskgated flakiness. Bare Mach-O binaries depend on an online check that proved unreliable (v0.42.0 and v0.43.0 both shipped bare Mach-O and both SIGKILL'd on the maintainer's Mac).

After any new tag lands and the release workflow publishes non-macOS assets, run:

```bash
./scripts/release-macos-local.sh --tag vX.Y.Z --upload
```

The script is 8 steps: build, re-sign, package-to-dmg, sign-dmg, notarize+staple, **mount + launch test** (exit 3 if it fails), upload, **end-to-end install.sh → launch test** (exit 4 if it fails). Refuses to upload a dmg that doesn't mount-and-launch cleanly; refuses to declare success after upload if the download+install chain doesn't end in a working `--version`.

**Load-bearing rule:** the success criterion is a working `--version` after fresh install.sh → mount → extract → launch. Not `codesign --verify`, not `spctl --assess`, not the on-runner launch gate in isolation. We shipped v0.42.0 and v0.43.0 with the same breakage because we declared victory on partial verification. Don't do that again.

Full flow + env-var setup in `RELEASING.md` § "macOS release is a locally-signed + stapled .dmg". Do not revert to CI signing or bare Mach-O without re-running the #219 diagnostic and proving the online-notarization-check issue is resolved (tracked in #226).

## Development

- `uv sync` to install.
- `pytest -x` for the test suite.
- Never push directly to `main` — always PR → CI → merge.
