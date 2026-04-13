# Releasing Shipyard

## Default path: automatic on merge

Normal releases are automatic. You don't call any script.

1. Open a PR via `shipyard pr` (or `shipyard ship`).
2. CI runs `.github/workflows/version-skill-check.yml`, which confirms the right bump(s) are present via `scripts/version_bump_check.py` + `scripts/skill_sync_check.py`. Merge on green.
3. On push to `main`, `.github/workflows/auto-release.yml` diffs `pyproject.toml`'s version against the previous push. If it moved, the workflow creates a `v<x.y.z>` tag.
4. The existing tag-triggered `release.yml` builds binaries on 5 platforms and publishes the GitHub Release.

Plugin-version bumps (`.claude-plugin/plugin.json`) are intentionally **not** tagged — plugin files are delivered from git, not from the binary. Bumping the plugin version still requires a PR and goes through the same gate, but it doesn't cut a binary release.

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
