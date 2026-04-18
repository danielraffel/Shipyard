---
name: changelog
description: Regenerate CHANGELOG.md from git tags, check for drift, or initialize the post-release docs sync config. Wraps `shipyard changelog`.
---

Forwards to the `shipyard changelog` CLI group:

- `regenerate` — walks every tag matching `release.changelog.tag_filter` in reverse semver order, extracts merged PRs per tag range, and writes the file atomically.
- `check` — exits non-zero if the file is out of date. Wired into CI as a drift gate.
- `init` — scaffolds `[release.changelog]` + `[release.post_tag_hook]` into `.shipyard/config.toml` and warns once if `CHANGELOG.md` already exists.
- `regenerate --release-notes <TAG>` — prints per-release markdown to stdout for feeding into `softprops/action-gh-release` or a GitHub Release body.

Opt-in. If `[release.changelog]` is absent from `.shipyard/config.toml`, every subcommand refuses rather than silently modifying files. Run `shipyard changelog init` first.

```bash
shipyard changelog $ARGUMENTS
```

## When to run regen

- Just after a tag lands on main and the hook didn't fire (manual recovery).
- Locally, to preview the next release's CHANGELOG entry before merging.
- CI drift gate: `shipyard changelog check` in a PR workflow catches humans hand-editing the wrong block.

## Post-tag sync workflow

After `shipyard changelog init`, run `shipyard release-bot hook install` to drop `.github/workflows/post-tag-sync.yml`. That workflow fires on tag push, installs shipyard, and runs `shipyard release-bot hook run` — which regenerates the CHANGELOG, commits with `[skip ci]` + the three skip trailers, and pushes back to main with a rebase-retry loop.

The config block is the opt-in signal. No HTML marker, no state file, no `--adopt` flag. If you ran `init`, you opted in.
