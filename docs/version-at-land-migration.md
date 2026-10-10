# Version-at-land migration

Shipyard now assigns CLI and plugin versions after a change lands on `main`.
A normal pull request must not edit `Cargo.toml`, `Cargo.lock`,
`.claude-plugin/plugin.json`, or `.claude-plugin/marketplace.json`. The PR gate
reports those edits as an error. Two branches can therefore merge back to back
without racing on a shared version line.

The post-merge `version-at-land.yml` workflow is the only writer. It computes
the highest semver level from the merged range, writes all files in each
surface, commits `Version-Bump-Applied:`, and pushes with a fast-forward-only
retry. The existing `auto-release.yml` notices the writer commit and creates
`vX.Y.Z`; `release.yml` publishes it, and `shipyard update` continues to use
the latest published tag.

## In-flight pull requests

Remove any hand-written bump commit and restore the branch to the version at
`main`. If a release PR must intentionally carry a version file change during
the migration, add this trailer to a commit in that PR:

```
Release: allow-version-files reason="pre-cutover release recovery"
```

That exception is limited to release recovery. It is not needed for ordinary
feature or fix pull requests.

## Replay acceptance

A no-version-file PR passes the gate. A PR that edits a version file fails
unless it carries the release exception. Two consecutive merged commits are
processed by one writer transaction and receive one monotonically increasing
assignment, so the second branch has no version-line conflict to replay.
