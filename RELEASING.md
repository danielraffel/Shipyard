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

## Optional: sign + notarize the macOS CLI binary

macOS 26.3+ refuses to execute ad-hoc-signed binaries that carry the `com.apple.provenance` xattr GitHub stamps on every release download — the OS SIGKILLs with "Taskgated Invalid Signature" and the user sees a bare `zsh: killed` with no context.

`install.sh` works around this locally by stripping the xattr and re-applying an ad-hoc signature. Anyone installing via `gh release download` or a manual browser click still hits the crash. The proper fix is Developer-ID-signed + Apple-notarized binaries produced by the release workflow itself.

Five secrets on the repo (all `gh secret set NAME`):

| Secret | What |
|---|---|
| `APPLE_ID` | Apple ID email (same one used for Developer ID certificate) |
| `TEAM_ID` | 10-char Team ID from [developer.apple.com/account](https://developer.apple.com/account) |
| `APP_SPECIFIC_PASSWORD` | App-specific password generated at [appleid.apple.com](https://appleid.apple.com) → Sign-In and Security → App-Specific Passwords |
| `SIGNING_CERT_P12_BASE64` | `base64 -i DeveloperID.p12` of your exported Developer ID Application cert + private key |
| `SIGNING_CERT_PASSWORD` | Export password for the .p12 |

When **all five** are set, the release workflow's macOS matrix jobs import the cert into a temp keychain, resolve the Developer ID Application identity by **Team ID** (via `security find-identity -v -p codesigning | grep "(TEAM_ID)"` — not by subject CN, so forks / org-owned certs / maintainer rotation all work), pass the resulting SHA-1 fingerprint to PyInstaller's `--codesign-identity` so every embedded dylib (notably `Python.framework`) gets signed at collection time, then re-sign the outer Mach-O with `--options runtime --timestamp` and submit to `xcrun notarytool submit --wait`. Users downloading a signed binary get clean execution on macOS 26.3+ with no xattr dance.

The gate is all-five-present (not just `APPLE_ID`). A partial rotation — say a new `APPLE_ID` pasted in but the new `SIGNING_CERT_P12_BASE64` not yet uploaded — used to run the signing path and fail mid-release on `base64 -d`. Now the step cleanly no-ops in that state and the ad-hoc fallback publishes normally.

**Why PyInstaller has to see the identity, not just the post-build step.** An earlier iteration signed only the outer Mach-O after PyInstaller had already bundled an ad-hoc-signed `Python.framework` inside. dyld's "same Team ID" check then rejected the load at first launch:

```
code signature … not valid for use in process: mapping process and mapped file
(non-platform) have different Team IDs
```

Passing `--codesign-identity` during the PyInstaller build is what makes the inner and outer Team IDs match. The outer re-sign step remains because PyInstaller doesn't add `--options runtime` / `--timestamp` itself, and notarytool requires both.

When the secrets aren't set, the sign+notarize step no-ops and the workflow continues to publish ad-hoc-signed binaries (the current default). Forks and pull requests from external contributors aren't blocked.

A bare Mach-O can't be `stapler staple`'d — Gatekeeper verifies notarization online at first launch instead. That requires network, which every CI / user machine has. Accepted tradeoff vs wrapping the CLI in a no-op `.app` bundle just for stapling.

## Preferred runner provider

Shipyard's own CI defaults to the runner provider configured in the repo variable `DEFAULT_RUNNER_PROVIDER`. Set it once:

```sh
gh variable set DEFAULT_RUNNER_PROVIDER --repo danielraffel/Shipyard --body namespace
```

Every subsequent PR push, tag push, and scheduled release picks that up without workflow edits. Per-run overrides still work via `gh workflow run ci.yml --ref <branch> -f runner_provider=github-hosted`.

**Why Namespace is the preferred default:** GitHub-hosted `macos-15` queues routinely stall shipyard PRs for 10+ minutes during business hours. Namespace's cloud pool (profiles `namespace-profile-generouscorp`, `-macos`, `-windows`) has near-zero queue time and faster macOS ARM machines. The trade-off is per-minute cost — Namespace is not free like GitHub-hosted on public repos — but for an active project the wall-clock-time savings are worth it.

**When to flip back to `github-hosted`:**
- Forks / external contributors whose repos don't have the variable set will naturally fall through to `github-hosted` (safe default, no paid surface exposed).
- If Namespace has an outage: `gh variable delete DEFAULT_RUNNER_PROVIDER` restores `github-hosted` for all future runs without a workflow PR.

The resolution order per target in `ci.yml` / `release.yml` is:
1. `workflow_dispatch` input (per-run override — all targets).
2. `vars.DEFAULT_RUNNER_PROVIDER_LINUX` / `_MACOS` / `_WINDOWS` (per-target repo variable).
3. `vars.DEFAULT_RUNNER_PROVIDER` (repo-wide default).
4. Hardcoded fallback `github-hosted`.

**Per-target override use case:** If one Namespace profile goes down (e.g. `generouscorp-windows` saturated 2026-04-23, #193) you can route just that platform to `github-hosted` without losing the speed benefit on the healthy profiles:

```sh
gh variable set DEFAULT_RUNNER_PROVIDER_WINDOWS --repo danielraffel/Shipyard --body github-hosted
```

Every subsequent run builds Linux + macOS on Namespace and Windows on `windows-latest`. Delete the variable when the outage clears:

```sh
gh variable delete DEFAULT_RUNNER_PROVIDER_WINDOWS --repo danielraffel/Shipyard
```

## Default path: automatic on merge

Normal releases are automatic. You don't call any script.

1. Open a PR via `shipyard pr` (or `shipyard ship`).
2. CI runs `.github/workflows/version-skill-check.yml`, which confirms the right bump(s) are present via `scripts/version_bump_check.py` + `scripts/skill_sync_check.py`. Merge on green.
3. On push to `main`, `.github/workflows/auto-release.yml` diffs `pyproject.toml`'s version against the previous push. If it moved, the workflow creates a `v<x.y.z>` tag.
4. The existing tag-triggered `release.yml` builds binaries on 5 platforms and publishes the GitHub Release.

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
1. Bumps the version in `pyproject.toml`, `__init__.py`, and plugin manifests
2. Commits the version bump
3. Tags the commit (`v0.1.1`)
4. Pushes the tag
5. The tag push triggers `.github/workflows/release.yml` which:
   - Builds binaries on 5 platforms (macOS ARM64/x64, Windows x64, Linux x64/ARM64)
   - Creates a GitHub Release with binaries + SHA256 checksums
   - Uses GitHub-hosted runners by default; Namespace via manual dispatch

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

## Version locations

The version lives in 4 places (all updated by `release.sh`):

| File | Field |
|------|-------|
| `pyproject.toml` | `version = "X.Y.Z"` |
| `src/shipyard/__init__.py` | `__version__ = "X.Y.Z"` |
| `.claude-plugin/plugin.json` | `"version": "X.Y.Z"` |
| `.claude-plugin/marketplace.json` | `"version": "X.Y.Z"` |

## Versioning convention

- **Patch** (0.1.x): bug fixes, small improvements
- **Minor** (0.x.0): new features, new commands, new ecosystem detectors
- **Major** (x.0.0): breaking changes to config format, CLI output schema, or behavior

The `--json` output schema has its own version (`schema_version` field) that
increments independently when the output format changes.
