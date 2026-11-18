# CLAUDE.md

Guidance for Claude Code (and Codex via `AGENTS.md → CLAUDE.md`) when working in this repository.

## Autonomous architecture contract

The permanent invariants for autonomous review, CI, merge, release, recovery, and cross-host handoff are in `docs/autonomy-contract.md`. Read it before changing those paths. A detector is not a recovery system: permitted remediation or redispatch and an authoritative post-action progress read are required. Keep the rollout default-off until the contract's crash, restart, fencing, and integrated-provider evidence passes.

## What is Shipyard

Shipyard is a cross-platform CI controller: local VMs, SSH hosts, and cloud runners (Namespace / GitHub-hosted) coordinated under one queue-and-evidence model. It is designed so agents, not humans, are the primary users.

The project is a Rust CLI (`Cargo.toml`, `src/`) and ships a Claude Code plugin (`.claude-plugin/`, `commands/`, `skills/`, `agents/`, `hooks/`) delivered directly from this git repo.

## Two independently-versioned surfaces

| Surface        | Where                                            | When it moves                                  |
|----------------|--------------------------------------------------|------------------------------------------------|
| CLI binary     | `Cargo.toml` `[package].version`             | `src/`, release scripts, installer, or workflow changes that affect behavior |
| Claude plugin  | `.claude-plugin/plugin.json` `version`           | `commands/`, `skills/`, `agents/`, `hooks/`, or `.claude-plugin/` changes |

These are **decoupled by policy** (`RELEASING.md`) — plugin files are delivered from git, not the binary, so a plugin-only change is not a binary release. The gate just ensures each surface's version moves when its own code moves.

## Versioning & Skill-Sync Policy

Enforcement runs in two layers, both calling the same two scripts. (There is
deliberately no agent `PostToolUse` hook: it would execute whichever checkout's
scripts the edited file sat in.)

| Layer | Where | Mode |
|---|---|---|
| 1 (pre-push)   | `.githooks/pre-push` (install via `scripts/install-githooks.sh`) | `--mode=report` hard-fails by default; only an explicit `SHIPYARD_ENFORCE_PREPUSH=0` keeps local warnings advisory |
| 2 (CI)         | `.github/workflows/version-skill-check.yml` | `--mode=report` with `SHIPYARD_ENFORCE_PREPUSH=1` — blocks merge |

Scripts:

- `scripts/version_bump_check.py` detects which surfaces need a post-merge assignment. Pull requests may not edit version files, except dependency and feature edits in `Cargo.toml` and `Cargo.lock`; the gate has no apply mode.
- `scripts/skill_sync_check.py` — hard-fails when a mapped path is touched without the corresponding `SKILL.md` update. The map is `scripts/skill_path_map.json`; every dir under `skills/` must appear in it.

Full design: borrowed from `pulp` (upstream) — the schema and scripts are shared. See the pulp repo's `docs/guides/versioning.md` for the source of truth on the design.

### Shipping a PR

Whenever an agent workflow needs to wait on a GitHub condition — a
release to finish uploading, a PR's required checks to go green, a
dispatched workflow run to finish — reach for `shipyard wait ...`
instead of hand-rolling a `gh`-polling loop. Daemon-backed (wakes in
seconds on real events) with a transparent polling fallback when no
daemon is running. Always set `--timeout`. See
[`skills/ci/SKILL.md#waiting-on-conditions-shipyard-wait`](https://github.com/danielraffel/Shipyard/blob/main/skills/ci/SKILL.md#waiting-on-conditions-shipyard-wait)
for patterns and exit-code reference.

When the user says "push a PR", "ship this", "ship it", "we're done", "merge this", or similar, invoke `shipyard pr` (or `shipyard ship` if that name is reserved for release today — check `skills/ci/SKILL.md`). It orchestrates:

1. `skill_sync_check.py --mode=report` — hard-fails on missing SKILL.md updates.
2. `version_bump_check.py --mode=report` refuses a PR that edits a version file (dependency and feature edits in `Cargo.toml` and `Cargo.lock` pass).
3. `git commit` + PR create + CI validate. Agents pass `--no-arm` and stop there; the steward merges the approved head (see "How a PR lands here").
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

A PR needs no `Version-Bump:` trailer. At land, version-at-land takes versions from the change and ignores `Version-Bump: <surface>=skip` (it suppressed the release for #1393); only `Release: skip reason="..."` opts a change out.

| Gate          | Trailer                                                      |
|---------------|--------------------------------------------------------------|
| Version bump  | `Version-Bump: <surface>=<patch\|minor\|major\|skip> reason="..."` |
| Skill update  | `Skill-Update: skip skill=<name> reason="..."`              |
| Auto-release  | `Release: skip reason="..."`                                 |

## Manual fallback release

`./scripts/release.sh` remains as a break-glass manual path. The default is the automatic path above (auto-release workflow on merge). Do not call `release.sh` directly unless the automatic path is genuinely blocked.

## macOS release: a stapled .dmg (arm64 only), built and published by release.yml on m5studio

macOS binaries ship as stapled **.dmg** artifacts. The `.dmg` wrapper puts the notarization ticket **inside** the artifact so Gatekeeper verifies offline. Bare Mach-O binaries depend on an online notarization check that proved unreliable: v0.42.0 and v0.43.0 shipped bare Mach-O and both SIGKILL'd on the maintainer's Mac (#219). Apple Silicon (arm64) is the only supported macOS architecture (#256); install.sh prints a clean "unsupported" message on Intel.

**How a release is published, with no human step** (repository variable `CI_MACOS_SIGNING_ENABLED=true`):

1. A merged change opens the `release/version-at-land` pull request (`version-at-land.yml`); the steward merges it once `review-verdict` is green.
2. `auto-release.yml` tags `vX.Y.Z`.
3. `release.yml` builds Linux x64 and ARM64 (and Windows only under `CI_PLATFORM_FOCUS=all`), runs the live smoke, and creates the GitHub release as a **draft**. `install.sh`'s `releases/latest` excludes drafts, so users keep the previous release until this one is complete.
4. Its `sign-and-upload-macos` job runs on the self-hosted `local-mac` runner on m5studio (Shipyard-studio-*), in the `release` environment. It builds with a pinned toolchain, imports the Developer ID identity into an ephemeral keychain, signs, notarizes, staples and attests the DMG, and mounts it and launches both binaries. Then `scripts/release-macos-local.sh --publish-only --ci-mode` checks every asset and checksum, flips the release public, and runs install.sh for the tag. If the installed binary's `--version` is not the tag, it puts the release back to draft and fails the run.
5. `release-heal.yml` re-runs a failed draft release run, up to 3 times.
6. `release-cadence-check.yml`'s `chain` step opens one `release-blocked` issue when a version is not public, complete and latest 90 minutes after it lands, or a bump pull request is open over 120 minutes. The m3 steward tick then messages the lead sessions over cmux (`docs/release-watchdog.md`).

**Load-bearing rule:** the success criterion is a working `--version` after a fresh install.sh, mount, extract and launch. That `--version` must equal the tag without its leading `v`, since a build script or a hardcoded string can make a binary report a version its tag does not name. Not `codesign --verify`, not `spctl --assess`, not the on-runner launch gate in isolation. We shipped v0.42.0 and v0.43.0 with the same breakage because we declared victory on partial verification. Don't do that again.

**Break-glass fallback:** if the CI job cannot run (signing disabled, the m5studio runner down), run `./scripts/release-macos-local.sh --tag vX.Y.Z --upload` on an Apple Silicon Mac with the signing credentials. It runs the same steps and the same install.sh `--version` check (exit 3 on a failed mount and launch, exit 4 on a failed install, which reverts the release to draft). Full flow and environment setup: `RELEASING.md`. Do not move to bare Mach-O without re-running the #219 diagnostic (#226).

## How a PR lands here

- **Open the PR and stop.** Use `ghapp pr create -R danielraffel/Shipyard --base main`. Never arm, enqueue, auto-merge or merge; admission has exactly one writer.
- **Review:** a standing reviewer posts an exact-head APPROVE or a HOLD once required checks are green. Never approve your own team's work.
- **Landing:** this repo is user-owned, so there is no merge queue. The m3 steward tick (every 5 minutes) merges the exact approved head. The ruleset requires `Test macOS ARM64` and `review-verdict`, bound to GitHub Actions.
- **On a HOLD:** fix what it names, with a test. Merge `origin/main` (never rebase or force-push). Push once and comment `repaired: ... at <sha>`; the PR re-enters review on its own.
- **macOS first.** `CI_PLATFORM_FOCUS=macos` (the default) limits Shipyard CI to macOS ARM64 and releases to macOS ARM64, Linux x64 and Linux ARM64; Windows and the Linux CI lane run only with `CI_PLATFORM_FOCUS=all`. A Windows- or Linux-only failure gets a `PARKED/<platform>` issue with evidence and never blocks a macOS landing.
- **Labels the pipeline skips:** `superseded`, `owner-decision`, `PARKED/<platform>`.
- Launch agents with `sr claude` / `sr codex` and use the smallest model that fits (Haiku or Codex Luna for mechanical work, Sonnet or Codex Terra/Sol for fixes, Opus/Fable/Codex Astra for security, signing and release diffs).
- Steward log on m3: `~/Library/Logs/shipyard-steward-tick/summary.log`.

## Development

Shipyard is a Rust crate with Python helper scripts. The authoritative list of checks is the `run:` steps in `.github/workflows/ci.yml`; if this list and that file disagree, the workflow wins. To run the main ones locally:

- `cargo fmt --all --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --all-targets --locked --features ci-test-home`
- `python3 scripts/run_unittest_strict.py -s <dir> -p 'test_*.py'` for each Python test directory the workflow names (`scripts`, `tools/metrics` and others)

CI wraps the cargo test and clippy steps in `scripts/ci-capped-run.sh`, which only caps their resource use.

Never push directly to `main`; open a PR as described above.
