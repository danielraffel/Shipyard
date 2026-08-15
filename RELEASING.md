# Releasing Shipyard

## Quick path: `shipyard release-bot setup`

The guided command is the primary way to wire up `RELEASE_BOT_TOKEN` — it detects current state, recommends the right PAT path (fresh per-project PAT vs reusing one you already have), opens the pre-filled creation URL, stores the secret via `gh secret set`, and then dispatches a real workflow run to prove `actions/checkout` accepts it. One command replaces the manual steps below.

```sh
shipyard release-bot status                # what's configured, is it rejected?
shipyard release-bot setup                 # guided wizard
shipyard release-bot setup --paste         # skip wizard; just (re-)paste a token
shipyard release-bot setup --reconfigure   # replace existing secret value
shipyard release-bot setup --shared-name shipyard-release-bot  # one PAT across repos
shipyard release-bot setup --siblings other/repo --siblings other/repo2  # hint existing peers
```

If the wizard can't run (headless, no `gh`, etc.), the manual steps below remain the fallback.

## Operational GitHub auth is separate

`[github.auth]` in Shipyard config controls Shipyard's own built-in
GitHub CLI calls: PR checks, wait/auto-merge probes, cloud dispatch,
governance, doctor rate-limit checks, and related operations. It can use
ambient `gh`, an env token, or a command helper, including a helper that
mints GitHub App installation tokens.

That auth is intentionally separate from `RELEASE_BOT_TOKEN`. Release-bot
setup/status remain operator actions and use ambient `gh` auth while they
create or inspect repository secrets. Changing `[github.auth]` does not
change the workflow secret or the auto-release contract.

Moving Shipyard to another Mac means moving non-secret config and
reprovisioning credentials externally. Do not place raw tokens, GitHub App
private keys, Keychain exports, or token caches in release artifacts or
Shipyard config. Use `shipyard auth export` / `shipyard auth import` only for
sanitized `[github.auth]` config movement.

## One-time setup: `RELEASE_BOT_TOKEN` secret

The auto-release workflow needs a fine-grained PAT to push tags so that downstream `release.yml` actually fires. **Without this secret, auto-release silently degrades**: tags are still created via `GITHUB_TOKEN`, but GitHub Actions deliberately does not chain workflows from `GITHUB_TOKEN`-pushed tags (anti-infinite-loop safety), so `release.yml` never runs and no binaries ship.

Run `shipyard doctor` to check whether the secret is configured. If it shows `RELEASE_BOT_TOKEN: missing`, set it up:

1. **Generate the token.** github.com → top-right avatar → Settings → Developer settings → Personal access tokens → **Fine-grained tokens** → Generate new token.
2. **Token name:** `shipyard-release-bot` (or any descriptive name).
3. **Expiration:** 1 year (mark your calendar to renew).
4. **Resource owner:** the org or user that owns this repo.
5. **Repository access:** *Only select repositories* → include the repo(s) where this PAT will be used as `RELEASE_BOT_TOKEN`. **Why this is strict:** fine-grained PATs are authorized per-repo; GitHub enforces that `actions/checkout@v5` can only use the PAT against repos explicitly listed here. If you add the PAT as a secret to a second repo that isn't listed, auto-release fails on that repo with `fatal: could not read Username`.

   **Which repos belong here?** Just the project(s) whose `auto-release.yml` will use this token — typically *your own project*, not the Shipyard repo itself. You're setting up release automation for the codebase *using* Shipyard, not for Shipyard's own releases (unless you maintain a Shipyard fork).

   **Multi-project tip:** if you run Shipyard on multiple projects and want one rotation point, list them all here and use `shipyard release-bot setup --shared-name shipyard-release-bot` on each. Easier to rotate once; wider blast radius if the token leaks. Default per-project PATs are least privilege.
6. **Permissions** (Repository permissions section): **Contents: Read and write** is required. **Metadata: Read-only** is auto-added (required baseline). Optional: **Workflows: Read and write** only if you plan to commit changes to `.github/workflows/*` under this token — Shipyard's auto-release only pushes tags, so it's not required.
7. **Generate**, copy the token (starts with `github_pat_…`). The token is shown once and cannot be retrieved later.
8. **Add to repo secrets, on every consumer repo.** The PAT's "Selected repositories" list only authorizes the token to *operate against* those repos; you still have to store the token *value* as a secret named `RELEASE_BOT_TOKEN` on each repo separately. The fastest way is one `gh` command per repo:

   ```sh
   # Paste the token once, then Ctrl-D, for each repo:
   gh secret set RELEASE_BOT_TOKEN --repo <owner>/<repo-A>
   gh secret set RELEASE_BOT_TOKEN --repo <owner>/<repo-B>
   ```

   Or via the web UI: github.com/<owner>/<repo>/settings/secrets/actions → **New repository secret** → name `RELEASE_BOT_TOKEN`, paste value.

**If auto-release is failing with `fatal: could not read Username`,** there are two independent causes to check:

1. **PAT scope:** edit the existing token at https://github.com/settings/personal-access-tokens → add the failing repo to **Selected repositories** → **Update**. No secret re-set needed; the stored value stays valid.
2. **Secret value drift:** the `RELEASE_BOT_TOKEN` secret on this repo holds a *different* token than the one you expanded in step 1. This happens when the secret was seeded from an earlier PAT that was later revoked or replaced. Re-run `gh secret set RELEASE_BOT_TOKEN --repo <owner>/<repo>` with the current token value.

Both can be true at once. Verify by checking the timestamp of the secret (`gh secret list --repo <owner>/<repo>`) against the last token regeneration time on https://github.com/settings/personal-access-tokens.

That's it — no code change needed. The workflow already reads `${{ secrets.RELEASE_BOT_TOKEN || secrets.GITHUB_TOKEN }}`. `shipyard doctor` will then report `RELEASE_BOT_TOKEN: configured`.

### What if you can't or don't want to set the secret?

The chain still works but requires one manual step per release:

```bash
gh workflow run release.yml --ref v<x.y.z>
```

Run that after the auto-tag appears. The release workflow will pick up the existing tag and publish the binaries. (Pulp's first auto-released tag, `v0.4.0`, used this fallback before its `RELEASE_BOT_TOKEN` was provisioned.)

## macOS release is a locally-signed + stapled .dmg

**As of 2026-04-24 evening, macOS binaries ship as stapled `.dmg` artifacts built on the maintainer's Mac.** This supersedes the "bare Mach-O, notarized at release" path used through v0.43.0.

**What we learned** (issue [#219](https://github.com/danielraffel/Shipyard/issues/219)):

1. **CI-signed bare Mach-O binaries (v0.42.0, v0.43.0):** passed every CI check — `codesign --verify`, `spctl --assess`, an on-runner launch gate running `--version` under the signed+notarized state — and still SIGKILL'd with "Taskgated Invalid Signature" on the maintainer's Mac. We originally blamed CI signing.
2. **Locally-signed bare Mach-O (v0.43.0 re-upload):** worked the first time, then SIGKILL'd a few hours later on the same Mac with the same bytes. The delta wasn't CI vs local — it was that **bare Mach-O binaries depend on an online notarization check at launch**, and that check is unreliable on some Macs (contention, Apple CDN state, macOS 26.3+ `com.apple.provenance` enforcement, or all three).
3. **Stapled `.dmg`:** the only durable fix. A `.dmg` wrapper carries the notarization ticket **inside** the artifact. When macOS mounts the dmg, Gatekeeper verifies the ticket **offline**. The binary extracted from the mount inherits "trusted origin" provenance. No online check, no per-Mac flakiness, no dice rolls. End-to-end verified working on the same Mac that rejected every earlier artifact.

### How to cut a macOS release

Tag pushes create a draft GitHub release with Linux and Windows
artifacts. The macOS job builds and launch-smokes the Rust binary, but
does not upload an unsigned artifact unless the optional CI signing
experiment is explicitly enabled. The maintainer-local path remains the
primary macOS release path because it proves the exact DMG on the Mac
that will use it.

```bash
# One-time setup — either .env file or shell rc.
export SHIPYARD_NOTARIZE_APPLE_ID=<apple-id-email>
export SHIPYARD_NOTARIZE_TEAM_ID=<team-id>              # 10-char from developer.apple.com/account
export SHIPYARD_NOTARIZE_APP_PASSWORD=<app-specific>    # appleid.apple.com → Sign-In and Security
export SHIPYARD_SIGNING_IDENTITY=<SHA1-or-CN>           # `security find-identity -v -p codesigning`

# After release.yml creates the draft release:
./scripts/release-macos-local.sh --tag vX.Y.Z --upload --rollback-tag vPREVIOUS
```

M5 uses the existing file-backed App Store Connect API key and dedicated
signing keychain. Both dotenv files must be mode `0600`; the script maps the
existing `PULP_SIGN_*` and `PULP_NOTARY_*` names without copying secrets:

```bash
./scripts/release-macos-local.sh \
  --env-file ~/.config/pulp/secrets/keychain.env \
  --env-file ~/.config/pulp/secrets/notary.env \
  --check-auth

./scripts/release-macos-local.sh \
  --env-file ~/.config/pulp/secrets/keychain.env \
  --env-file ~/.config/pulp/secrets/notary.env \
  --tag vX.Y.Z --upload --rollback-tag vPREVIOUS
```

When both standard M5 files exist, the local release script discovers them
automatically; the explicit flags above document the portable contract and
remain useful for a nonstandard checkout. An explicit signing keychain without
the local `.p12` and its file-backed password fails before `codesign` rather
than risking a Keychain UI prompt.

`--check-auth` runs a timestamped hardened-runtime signature against a
disposable Mach-O. When a signing keychain is configured, the release path
snapshots the user keychain search list, places that keychain first, pins
`codesign` to it, and restores the original list on every exit. API-key
notarization invokes `notarytool` directly with the local key file; Apple-ID
and app-password authentication remains supported for other environments. The
full local release command runs this same check automatically before building;
failed import, partition setup, search-list preparation, or probe is terminal
and cannot fall through to artifact signing.

The script runs the full pipeline on the local Mac:

1. Fails fast if any env var is missing (before the Rust release build).
2. Builds `target/release/shipyard` unless `--skip-build --binary <path>` is supplied.
3. Signs the Mach-O with `--options runtime --timestamp` (notarytool prerequisites).
4. Packages the signed binary into a `.dmg`, signs the DMG, submits it via `xcrun notarytool submit --wait`, and staples the accepted ticket.
5. **Runs `<binary> --version` from the mounted DMG locally.** If this fails, the script exits and does NOT upload. This is the whole point — the Mac running the script is the same Mac that will need to launch the binary tomorrow.
6. On launch success: uploads via `gh release upload --clobber`, updates `checksums.sha256`, verifies public asset visibility, and runs the `install.sh` E2E.
7. With `--rollback-tag`, verifies baseline install, upgrade to the new release, and rollback to the previous release in an isolated temp install directory.

Running without `--upload` is the diagnostic mode (used to confirm the local signing path actually works on a given Mac). Script-helper tests under `scripts/test_*.py` ensure missing creds / bad flags / bash syntax errors all surface before the expensive build step.

### Optional CI macOS signing

CI signing is dormant unless the repository variable
`CI_MACOS_SIGNING_ENABLED=true` is set. When enabled, the macOS release
job imports a Developer ID identity into an ephemeral keychain, resolves
the identity by Team ID, and runs `scripts/release-macos-local.sh
--ci-mode --upload`.

Required CI secret names:

| Secret | What |
|---|---|
| `MACOS_NOTARIZE_APPLE_ID` | Apple ID email used for notarization |
| `MACOS_NOTARIZE_TEAM_ID` | 10-char Team ID from developer.apple.com/account |
| `MACOS_NOTARIZE_APP_PASSWORD` | App-specific Apple ID password |
| `MACOS_SIGN_P12_BASE64` | Base64-encoded Developer ID Application cert + private key |
| `MACOS_SIGN_P12_PASSWORD` | Export password for the `.p12` |

If the variable is unset, the macOS CI job is build-health-only. Forks
and pull requests from external contributors are not blocked by signing
credentials.

### Lesson codified

We shipped v0.42.0 and v0.43.0 with the same class of breakage because
we declared "works" based on partial verification. The release script
now enforces the actual success criterion: `install.sh` downloads the
DMG, mounts it, extracts the binary, and `shipyard --version` launches.
If any step in that chain fails, the release fails regardless of what
`codesign --verify` or `spctl --assess` said earlier.

## Preferred runner provider

Shipyard's own CI defaults to GitHub-hosted runners. Namespace remains an
explicit opt-in for accounts that have access, but it is not the safe default.
If a repo variable was previously set to Namespace, flip it back:

```sh
gh variable set DEFAULT_RUNNER_PROVIDER --repo danielraffel/Shipyard --body github-hosted
```

Every subsequent PR push, tag push, and scheduled release picks that up without
workflow edits. Per-run overrides still work via
`gh workflow run ci.yml --ref <branch> -f runner_provider=namespace` when
Namespace access is available.

**Current default:** GitHub-hosted Linux, macOS, and Windows. This is slower
than a warm paid pool, but it avoids paid Namespace capacity and keeps public
repo CI available while local/self-hosted runners are being set up.

**When to opt into Namespace:**
- Only when the account has active Namespace access.
- Only for trusted branches/repos; paid or self-hosted capacity should not run
  untrusted fork code by default.

The resolution order per target in `ci.yml` / `release.yml` is:
1. `workflow_dispatch` input (per-run override — all targets).
2. `vars.DEFAULT_RUNNER_PROVIDER` (repo-wide default).
3. Hardcoded fallback `github-hosted`.

The workflow also supports explicit `*_runner_selector_json` inputs for trusted
manual runs. There is intentionally no hidden repo-variable fallback to a local
macOS runner; if you want a self-hosted runner for one run, pass the selector
explicitly via `workflow_dispatch`.

## Default path: automatic on merge

Normal releases are automatic through draft creation. The macOS DMG
publish step still follows the signed release runbook above unless CI
macOS signing is explicitly enabled.

1. Open a PR via `shipyard pr` (or `shipyard ship`).
2. CI runs `.github/workflows/version-skill-check.yml`, which confirms the right bump(s) are present via `scripts/version_bump_check.py` + `scripts/skill_sync_check.py`. Merge on green.
3. On push to `main`, `.github/workflows/auto-release.yml` diffs `Cargo.toml`'s package version against the previous push. If it moved, the workflow creates a `v<x.y.z>` tag.
4. The existing tag-triggered `release.yml` builds Linux, Windows, and
   macOS ARM64 release candidates, creates a draft GitHub Release, and
   leaves macOS signing/upload to the runbook unless CI macOS signing is
   explicitly enabled.

Plugin-version bumps (`.claude-plugin/plugin.json`) are intentionally **not** tagged — plugin files are delivered from git, not from the binary. Bumping the plugin version still requires a PR and goes through the same gate, but it doesn't cut a binary release.

## Patch-level auto-bumps (rollup gap fix)

By default the version-bump gate only auto-applies **minor** and **major** verdicts. Patch-level changes (internal fixes, Codex-review cleanups) land as "patch-suggested" — advisory only. In a run of fix-only PRs, nothing gets auto-released until a minor-class change happens to merge.

Enable per-surface auto-patch-apply in `scripts/versioning.json`:

```jsonc
{
  "surfaces": {
    "cli": {
      "auto_apply_patch": true    // fix-only PRs now bump + release
    }
  }
}
```

When true, `apply_bumps()` treats patch verdicts like minor/major: rewrites the version files, commits, pushes. The tag-release chain fires normally.

When false (default), `shipyard doctor` surfaces the drift: latest `vN.N.N` tag vs count of CLI-surface commits since that tag on main. At or above the threshold (default 3) it reports `tag_drift` as not-ok so maintainers know to bump before the gap widens. Shipyard's own repo has `auto_apply_patch: true` on `cli`; the plugin surface stays off (git-delivered, no binary).

See issue #70 for the design discussion.

## When to go manual

Only when the automatic flow is genuinely unavailable:

- The auto-release workflow is disabled or broken.
- An emergency hotfix needs direct tag control (rare — prefer a PR even for hotfixes).

### Manual fallback

```bash
./scripts/release.sh patch    # 0.1.0 → 0.1.1 (bug fixes)
./scripts/release.sh minor    # 0.1.0 → 0.2.0 (new features)
./scripts/release.sh major    # 0.1.0 → 1.0.0 (breaking changes)
./scripts/release.sh 0.3.0    # explicit version
```

The script:

0. Runs `scripts/version_bump_check.py --mode=report` and refuses to tag if the required bumps aren't present. `RELEASE_SKIP_VERSION_CHECK=1` bypasses this for emergencies; log a reason in the commit trailer.
1. Bumps the version in `Cargo.toml` and plugin manifests
2. Commits the version bump
3. Tags the commit (`v0.1.1`)
4. Pushes the tag
5. The tag push triggers `.github/workflows/release.yml` which:
   - Builds Linux x64, Linux ARM64, Windows x64, and macOS ARM64 release candidates
   - Uploads non-macOS binaries plus SHA256 checksums to a draft GitHub Release
   - Leaves macOS signing/upload to `scripts/release-macos-local.sh` unless CI macOS signing is explicitly enabled
   - Uses the configured runner provider, with GitHub-hosted as the safe default

## Monitoring a release

```bash
# Watch the build
gh run list --repo danielraffel/Shipyard --limit 3

# Check the release page
gh release view v0.1.1 --repo danielraffel/Shipyard
```

## Using Namespace runners for a release build

```bash
gh workflow run release.yml --repo danielraffel/Shipyard -f runner_provider=namespace
```

Only use this when the Namespace account is active.

## Version locations

The binary and plugin versions live in 3 places (all updated by `release.sh`):

| File | Field |
|------|-------|
| `Cargo.toml` | `[package] version = "X.Y.Z"` |
| `.claude-plugin/plugin.json` | `"version": "X.Y.Z"` |
| `.claude-plugin/marketplace.json` | `"version": "X.Y.Z"` |

## Versioning convention

- **Patch** (0.1.x): bug fixes, small improvements
- **Minor** (0.x.0): new features, new commands, new ecosystem detectors
- **Major** (x.0.0): breaking changes to config format, CLI output schema, or behavior

The `--json` output schema has its own version (`schema_version` field) that
increments independently when the output format changes.
