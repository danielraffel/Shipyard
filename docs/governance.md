# Security & Governance Profiles

Shipyard manages a project's GitHub-side governance settings —
branch protection on `main`, tag protection on release tags, default
workflow token permissions, release approval gates — declaratively from
`.shipyard/config.toml`. Pick a profile, run `shipyard governance apply`,
and the live GitHub state matches the profile. Drift between the declared
config and the live state is reported by `shipyard governance status`.

## Pick a profile

```toml
# .shipyard/config.toml
[project]
profile = "solo"   # one of: solo, multi, custom
```

The two presets cover the most common shapes: a single maintainer who
takes occasional third-party PRs, and a multi-contributor team with real
review requirements. `custom` lets you declare every knob explicitly.

## What each profile sets

| Setting | `solo` | `multi` | Why the difference |
|---|---|---|---|
| Branch protection: require PR | ✅ | ✅ | Catches stray pushes either way |
| Branch protection: required status checks | ✅ (configured) | ✅ (configured) | The whole point of CI |
| Branch protection: strict status checks | ❌ | ✅ | Solo doesn't need rebase coordination |
| Branch protection: required reviews | 0 | 1 | Solo can't review their own PR |
| Branch protection: enforce on admins | ❌ | ✅ | Solo needs a 3 AM hotfix path |
| Branch protection: dismiss stale reviews | ❌ | ✅ | Force re-review on rebase in multi |
| Tag protection: forbid update / delete / force | ✅ | ✅ | Trivy-style attack prevention |
| Tag protection: forbid creation by non-admins | ❌ | ✅ | Solo creates release tags directly |
| Default workflow token | read | read | Both — pure win, zero friction |
| Forbid sensitive branch patterns | ❌ | ✅ | Solo has no co-maintainers to coordinate disclosure with |
| Release approval gate | `off` (or `auto`) | `manual` | Solo doesn't gain from approving themselves |
| Sigstore release attestations | ✅ | ✅ | Free, no friction, helps downstream verifiers |
| Immutable releases | ✅ | ✅ | Free, no friction |
| Action SHA pinning (Renovate) | ✅ | ✅ | Same — managed by Renovate |
| `zizmor` workflow lint in CI | ✅ | ✅ | Same — runs automatically |
| Renovate cooldown (third-party / first-party days) | 3 / 0 | 3 / 0 | Same |

The pattern: **anything that's an "attacker-side" guardrail is on for
both profiles** (free security), and **anything that's a "process
correctness" guardrail varies** (solo doesn't gain from rules that
exist to coordinate multiple humans).

## Commands

```bash
shipyard governance status     # show declared vs live drift per branch
shipyard governance diff       # what `apply` would change (dry run)
shipyard governance apply      # bring live GitHub state in line with config
```

`status` is the rollup view that shows where things stand without
clicking through six GitHub settings pages. `diff` is the dry-run
before any mutation. `apply` is the idempotent apply — re-running
it on an aligned repo issues zero API writes.

`shipyard doctor` grows a "Governance" section that folds main-branch
drift into the same health check as git, ssh, and cloud auth, so CI
scripts and agents get a single pass/fail answer about whether the
repo is in the expected state.

Shipyard's profile table is the governance **target**. Branch
protection is enforced via the GitHub REST API today; tag protection,
rulesets, deployment environments, sigstore attestations, and the
immutable-releases row are partly API-manageable and partly
UI-only — the planning repo tracks which pieces land in which
release. Shipyard never claims a state it cannot verify: the
immutable-releases row in `doctor` and `governance status` prints
an informational line pointing at the settings URL rather than a
check or cross, because GitHub does not expose the repo-level
toggle via API on personal repos.

## Inspired by Astral

The governance profiles, the action SHA pinning workflow, the tag
protection, immutable releases, default read-only workflow tokens, and
the deployment approval pattern all follow practices documented in
[Astral's open-source security post](https://astral.sh/blog/open-source-security-at-astral).
Astral built and maintains uv, Ruff, and ty — millions of developers
depend on those tools, so they had to figure out the security baseline
for cross-platform Python tooling under real attacker pressure. The
post is the canonical reference for *why* each of these settings
matters, and Shipyard packages the *how* into a one-command profile
switch so you don't have to figure it out from first principles.

[Pulp](https://github.com/Generous-Corp/pulp) is the first project to
adopt Shipyard's governance profile system. Pulp runs on the `solo`
profile because it has a single maintainer today; switching to `multi`
would be a single-line edit to `[project].profile` in
`.shipyard/config.toml` plus a `shipyard governance apply`, with no
other config changes.
