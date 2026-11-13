# Security & Governance Profiles

Shipyard describes a project's GitHub-side governance settings —
branch protection on `main`, tag protection on release tags, default
workflow token permissions, release approval gates, and how workflows
pin and update their actions — as a profile in `.shipyard/config.toml`. Pick a profile and run `shipyard governance apply --yes`
to make live branch protection match it. Drift between the declared
branch-protection config and the live state is reported by
`shipyard governance status`; the supply-chain rows (action refs, the
workflow lockfile, the updates bot, zizmor, workflow permissions, tags,
releases) are reported, read-only, by `shipyard governance audit`.

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

Every row below is a **profile definition**: what the profile asks of a
repository and how Shipyard checks it. It is not a claim that any
repository does it. See [Status: target and enforced](#status-target-and-enforced).

| Setting | `solo` | `multi` | Checked by | Why the difference |
|---|---|---|---|---|
| Branch protection: require PR | ✅ | ✅ | `governance status` | Catches stray pushes either way |
| Branch protection: required status checks | ✅ (configured) | ✅ (configured) | `governance status` | The whole point of CI |
| Branch protection: strict status checks | ❌ | ✅ | `governance status` | Solo doesn't need rebase coordination |
| Branch protection: required reviews | 0 | 1 | `governance status` | Solo can't review their own PR |
| Branch protection: enforce on admins | ❌ | ✅ | `governance status` | Solo needs a 3 AM hotfix path |
| Branch protection: dismiss stale reviews | ❌ | ✅ | `governance status` | Force re-review on rebase in multi |
| Tag protection on `refs/tags/v*`: forbid update / delete / force push | ✅ | ✅ | audit `tag-protection` | Trivy-style attack prevention |
| Tag protection: forbid creation by non-admins | ❌ | ✅ | audit `tag-protection` | Solo creates release tags directly |
| Default workflow token | read | read | audit `default-token` | Both — pure win, zero friction |
| Forbid sensitive branch patterns | ❌ | ✅ | not audited | Solo has no co-maintainers to coordinate disclosure with |
| Release approval gate | `off` (or `auto`) | `manual` | not audited | Solo doesn't gain from approving themselves |
| Release jobs restore no cache | ❌ | ✅ | not audited yet | A poisoned cache must not reach a release build |
| Sigstore release attestations | ✅ | ✅ | not audited yet | Free, no friction, helps downstream verifiers |
| Immutable releases | ✅ | ✅ | audit `immutable-releases` | Free, no friction |
| Action refs: every `uses:` is an exact tag (`@v7.0.1`) or a full SHA with a `# vX.Y.Z` comment; no floating major or branch | ✅ | ✅ | audit `exact-refs` | Attacker-side: a moving ref runs whatever it points to today |
| Workflow lockfile (gh-actions-lock): `.github/workflows/actions.lock` plus a `gh actions-lock --verify` CI job, extension pinned by commit | ✅ advisory → required | ✅ advisory → required | audit `lockfile` | Attacker-side: pins transitive actions and repository identity; see [Workflow lockfile](#workflow-lockfile-gh-actions-lock) |
| Dependency updates: `updates.bot` (default `dependabot`) for `github-actions`, weekly, one group, `open-pull-requests-limit: 1`, `cooldown.default-days: 7` | ✅ | ✅ | audit `updates-bot` | Same — see [Updates bot](#updates-bot) |
| `zizmor` workflow lint: report mode | ✅ | ✅ | audit `zizmor-report` | Same — findings without blocking |
| `zizmor` workflow lint: enforce (`--min-severity high --min-confidence high`, exit codes on) | after the gate | after the gate | audit `zizmor-enforce` | Enforce only once the backlog is triaged; see [zizmor](#zizmor) |
| Every workflow declares top-level `permissions:` | ✅ | ✅ | audit `workflow-permissions` | A workflow without it inherits the repository default |
| Every workflow starts from `permissions: {}` and widens per job | ✅ | ✅ | audit `permissions-empty` | Each job asks only for what it uses |
| `pull_request_target` / `workflow_run` triggers | allowed only by a per-workflow event policy | forbidden | audit `untrusted-triggers` | These run with write tokens on untrusted input |

The pattern: **anything that's an "attacker-side" guardrail is on for
both profiles** (free security), and **anything that's a "process
correctness" guardrail varies** (solo doesn't gain from rules that
exist to coordinate multiple humans). The one attacker-side row that
differs is `untrusted-triggers`: a solo repository may still need a
reviewed `pull_request_target` workflow, and GitHub lets it allow that
per workflow file.

## Status: target and enforced

Every row is a **target** for a repository until `shipyard governance
audit` (or `governance status`, for branch protection) prints PASS for it
on that repository. This document defines the profile and never says a
row is enforced for a named repository. Where a fleet records which of its
repositories meets which row, that record cites an audit run and date, and
lives with the fleet, not here. The audit's row names (`exact-refs`,
`lockfile`, ...) are the names in the "Checked by" column, so a report can
be pasted next to this table.

## Workflow lockfile (gh-actions-lock)

[gh-actions-lock](https://github.com/github/gh-actions-lock) writes
`.github/workflows/actions.lock`, which records, for every direct **and
transitive** action a workflow uses, the ref, the resolved commit, and the
action repository's `owner_id` / `repo_id`. Once a workflow is onboarded,
the runner resolves `uses:` from the lockfile and rejects a commit that is
not reachable from the recorded ref, or a repository that was renamed or
re-created behind the same name. A CI job runs `gh actions-lock --verify`
so a missing or stale entry fails on the pull request
([background](https://hugovk.dev/blog/2026/lock-github-actions/)).

It works alongside exact refs, not instead of them:

- **Exact refs are the first layer** and work in every repository today:
  `uses:` names an exact tag or a full SHA with a version comment, and the
  updates bot keeps them current. A SHA pin covers one hop only; a
  composite action's own `uses: foo@v1` still runs whatever `v1` points to.
- **The lockfile is the second layer.** It pins the transitive hops, checks
  repository identity, and is enforced by the runner rather than only by
  whoever runs a checker. `gh actions-lock` rewrites SHA pins to exact tags,
  because the commit now lives in the lockfile; the exit plan (delete the
  lockfile) converts them back to SHAs mechanically from `ref` + `commit`.

The verify job, as the profile defines it:

- runs `gh extension install github/gh-actions-lock --pin <full commit SHA>`
  (a commit, not a tag name) and `gh actions-lock --verify`, with
  {% raw %}`GH_TOKEN: ${{ github.token }}`{% endraw %}, never an App or personal token;
- runs on `pull_request`, `push` to the default branch and, where a merge
  queue exists, `merge_group`, with no `paths:` filter;
- keeps gh-actions-lock's default narrowing on (never `--no-narrow`) so no
  floating ref enters the lockfile.

**Modes.** `advisory` first: the job runs and reports but is not a
required check. A repository moves to `required` only when all of these
hold: the job is merged on the default branch and has reported its context
on the default branch's tip at least once (never require a context before
its producer is on the base); it has been green on every PR for 7 days and
5 PRs (14 days and 20 PRs for a high-traffic or merge-queue repository)
with zero reds caused by the tool or the API; the positive and negative
controls (a new unlocked `uses:`, a wrong-repository commit, a bumped tag
without relock, each failing `--verify`; the committed lockfile passing)
are recorded; and the rollback (delete `actions.lock`) is written down.
Declare the mode in config:

```toml
[governance.lockfile]
mode = "advisory"     # or "required": the audit then also checks the context is required
context = "lockfile"  # the status-check context the verify job produces
```

**Reviewing lockfile diffs.** Moving a floating ref back to an older commit
is not detected by the tool. With exact refs there is nothing floating, and
a hunk in `actions.lock` that changes `commit` without changing `ref` is a
hold unless it comes from the updates bot and the new commit is what
`git ls-remote <action-repo> refs/tags/<ref>` returns.

**Known limits.**

- Reusable workflows called from another repository (`jobs.<id>.uses`) are
  not locked.
- Container images are not locked; pin them by `@sha256:` digest with a
  dated comment.
- Local `./` actions are never followed. A local composite action that uses
  remote actions is a blocking error until you pass `--ignore-local-actions`
  (and hand-pin the actions inside the composite to full SHAs) or
  `--migrate-local-actions`, which rewrites `./` to `$/` and needs a runner
  version that accepts that syntax.
- `--verify` needs the network and a token, so an API outage reds the job;
  that is why the mode starts advisory.
- GitHub's organisation policy "Require lockfile" blocks every workflow not
  yet onboarded, including ones with no `uses:`; it is not part of the
  profile during a rollout.

## Updates bot

The `updates-bot` row is one config value:

```toml
[governance.updates]
bot = "dependabot"            # default; "renovate" is accepted
cooldown_days = 7
open_pull_requests_limit = 1
```

The semantics are fixed whichever bot is named: `github-actions`, weekly,
one group, at most one open PR, and a 7-day minimum release age. With
Dependabot that is `schedule.interval: weekly`, a `groups` entry,
`open-pull-requests-limit: 1` and `cooldown.default-days: 7`; Dependabot
does not support the semver-bump cooldown keys for GitHub Actions, and a
cooldown never delays a security update
([Dependabot options reference](https://docs.github.com/en/code-security/dependabot/working-with-dependabot/dependabot-options-reference)).
Dependabot also regenerates the matching `actions.lock` entry in the same
PR for onboarded workflows. With Renovate it is `minimumReleaseAge: "7
days"` and a `groupName`; Renovate's lockfile support is marked
experimental, which is why Dependabot is the default. Absent keys mean
Dependabot's own defaults (for example a PR limit of 5), which the audit
reports as drift.

## zizmor

[zizmor](https://docs.zizmor.sh/) lints workflows for template injection,
untrusted checkouts, excessive permissions and unpinned uses. The profile
has two rows. **Report** (`zizmor-report`): a CI job runs zizmor (a pinned
`zizmorcore/zizmor-action`, or `uvx --from zizmor==<version>`) with
`--no-exit-codes`, so findings annotate the PR without failing it.
**Enforce** (`zizmor-enforce`): `--min-severity high --min-confidence high`
with exit codes on, adopted only after the high/high backlog has been zero
for 14 days and zizmor's lockfile support is no longer marked
experimental. zizmor is not a required check before that.

## Untrusted triggers

`pull_request_target` and `workflow_run` run with the base repository's
token on input an outside contributor controls. `multi` forbids them.
`solo` allows them only where a per-workflow event policy in the
repository settings names the workflow file; the audit cannot read that
policy, so it lists every workflow using either trigger and reports the
row as drift for review.

## Commands

```bash
shipyard governance status       # show declared vs live drift per branch
shipyard governance diff         # what `apply` would change (dry run)
shipyard governance apply        # preview only: prints the plan, writes nothing, exits 2
shipyard governance apply --yes  # write branch protection to match config
shipyard governance audit --repo owner/repo [--repo ...]  # read-only supply-chain audit
```

`status` is the rollup view that shows where things stand without
clicking through six GitHub settings pages. `diff` is the dry-run
before any mutation. `apply --yes` is the idempotent apply: re-running
it on an aligned repo issues zero API writes. Without `--yes`, `apply`
prints the same plan and writes nothing, so a copied command cannot
change branch protection by accident.

`status`, `diff` and `apply` read the policy from the base branch as
fetched (`origin/<base>`, where `<base>` is `governance.base_branch`,
else `ship.base_branch`, else `main`), not from the working tree. A stale
or edited checkout otherwise reports, and applies, its own copy of the
policy. The first output line names the ref the policy came from; if
that ref cannot be read, it says so with a WARNING and falls back to the
working tree. `shipyard landability` reads its policy the same way.

`audit` reads the supply-chain rows (everything checked by an "audit"
row in the table) for any repository, with `GET` requests only: the
workflow files, `actions.lock`, the updates-bot config, rulesets, the
immutable-releases setting and the default workflow token. Each row prints
PASS, DRIFT, or UNKNOWN when the input could not be read (for example a
403 from a credential without administration read); UNKNOWN is never
counted as PASS. It exits 0 when every row passes, 1 on any drift, and 2
when nothing drifts but something could not be read. `--gh-binary` takes
any gh-compatible CLI (such as a read-only App wrapper), `--profile` and
`--updates-bot` override the config, and `--json` emits a
`governance.audit` envelope. The policy comes from the base branch's
config like the other subcommands; outside a Shipyard checkout the
profile defaults apply. Workflow checks are textual (line patterns over
the YAML), so a workflow that builds `uses:` lines dynamically is not
seen.

`shipyard doctor` grows a "Governance" section that folds main-branch
drift into the same health check as git, ssh, and cloud auth, so CI
scripts and agents get a single pass/fail answer about whether the
repo is in the expected state.

Shipyard's profile table is the governance **target**. `governance
apply` writes branch protection only; every other row is read and
reported, never written. Shipyard never claims a state it cannot verify:
a row whose input cannot be read is UNKNOWN, not PASS.

## Relation to Astral's practices

Several rows follow the goals in
[Astral's open-source security post](https://astral.sh/blog/open-source-security-at-astral):
tag protection, immutable releases, read-only default workflow tokens,
`permissions: {}` at the top of every workflow, no `pull_request_target`
or `workflow_run`, zizmor, and a deployment approval for releases. The
post is a good account of *why* each matters.

The mechanisms and numbers here are Shipyard's, not Astral's. Astral pins
actions to full SHAs under GitHub's organisation policy and checks them
with pinact and zizmor; this profile reaches the same goal (run the
commit that was reviewed) with exact refs plus the workflow lockfile,
which also covers transitive actions and is enforced by the runner.
Astral uses both Dependabot and Renovate and publishes no cooldown
values; the updates-bot values above come from GitHub's Dependabot
documentation and this project's own policy.

## Adopting a profile

Set `[project].profile` in `.shipyard/config.toml`, run
`shipyard governance audit` to see which rows are still targets, and
`shipyard governance apply --yes` for branch protection. Switching
between `solo` and `multi` is a one-line edit to `[project].profile`.
Which repositories run which profile, and which rows each meets, is
recorded by whoever operates them, from audit output with a date; this
document names no repository.
