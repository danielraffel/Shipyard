# The `risk-tier` status

`risk-tier` says how much review a pull request would need before it lands,
from what it changes rather than from who asks. It is item 6 of the fleet
plan of record (section 4, "Recommended architecture"). It is
**report-only**: the status is always `success`, it is not a required check,
and its description says the tier and what that tier would require.

## Tiers

| Tier | A pull request is in it when it | Would require |
|---|---|---|
| **Low** | changes nothing below: docs, tests, skill text, internal fixes, CHANGELOG, generated files | CI only |
| **Moderate** | asks for a minor or major version bump; changes steward, queue, landing or daemon code; bumps an existing dependency; changes more than 400 lines or 15 files; has the label `risk:moderate` | 1 approving review from the review App on the exact head |
| **High** | changes workflows, `.shipyard/config.toml`, `.agents/contract.toml`, `tools/ci/**`, `scripts/ghapp*`, guard or arm code, signing or notarization, the installer, release scripts, or auth and token code; adds a dependency or package source; has the label `risk:high` | 2 review-App approvals, plus an owner approval for the owner subset |

The owner subset is workflows, credential and token code, signing, and a new
dependency or package source.

Labels raise a tier, never lower it: `risk:moderate` on a workflow change
leaves it High, and there is no `risk:low`.

### How each input is read

- **Paths** come from the pull request's file list (both names of a rename).
  `**` is any number of whole path segments and `*` any run within one
  segment, as in the carry exclusions (`src/head_approval.rs`
  `glob_match`). The guard and arm defaults are `PROTECTED_CARRY_PATHS`.
- **Dependencies:** a changed `Cargo.toml`, `Cargo.lock`, `pyproject.toml` or
  `uv.lock` (at any depth) is read at the base and the head and parsed as
  TOML, never run. A name the base did not have, or a new source (a git URL,
  path, registry or index), is High; a changed version of an existing one is
  Moderate. A package of the repository itself (no source in the lockfile)
  is neither. A file that will not parse is High.
- **Version bump:** the base's `scripts/versioning.json` (or
  `tools/scripts/versioning.json`) heuristic, as `version_bump_check.py`
  applies it: a surface whose public-API paths change with more than
  comments or whitespace is a minor bump, and a `feat:`, `BREAKING` or `!:`
  commit subject or a `Version-Bump:` trailer raises it. Here a trailer can
  only raise: `cli=skip` or `cli=patch` does not take a pull request out of
  Moderate. The version files themselves (which version-at-land writes) ask
  for nothing.
- **Size:** additions plus deletions, without lockfiles; the file count is
  the pull request's own `changed_files`. A file list GitHub truncates (over
  3000 files) is High.

## Configuration

Path lists and limits come from the **base branch's**
`.shipyard/config.toml`, never the head. Every key is optional:

```toml
[review.tiers]
high_paths_extra = ["vendor/**"]        # add to the defaults
moderate_paths_extra = ["core/engine/**"]
# high_paths = [...]                    # replace the defaults instead
# owner_paths / owner_paths_extra
# size_excluded_paths / size_excluded_paths_extra
max_changed_lines = 400
max_changed_files = 15
# versioning_config = "tools/scripts/versioning.json"
```

A replacement `high_paths` cannot drop `.github/workflows/**` or
`.shipyard/config.toml`: those are the rule's own inputs. The built-in lists
are at the top of `scripts/risk_tier.py`.

## Trust

`.github/workflows/risk-tier.yml` runs on `pull_request_target`, so it is
always the default branch's definition, and it checks out the default
branch, never the pull request's code. The script reads the pull request
through the API; the only head content it reads is the dependency files it
changed, as data.

Everything it prints, posts as a status or writes to the check run escapes
control characters first (`clean` in the script): a path or a lockfile package
name can hold a newline, and a job output line starting with `::` is a
workflow command (`::error`, `::add-mask`, `::stop-commands`). In a job the
script also brackets its output with `::stop-commands::<random token>`.
Reasons in the check run are Markdown code spans sized past any backticks
they hold.

The workflow does not run on `edited`, so retargeting a pull request to a new
base is reassessed on its next push or label change. A malformed file on the
base (for example an unparseable `versioning.json`) fails the job without
posting; while the status is report-only that is a visible, harmless failure.

**A `GITHUB_TOKEN` status is not proof of who posted it.** Any workflow in the
repository can post a `risk-tier` status, and a same-repository pull request
can add one that does. That is acceptable for a report. Before a repository
requires `risk-tier`, the status (or a check run) must be posted by a
separate GitHub App whose key no pull request job holds, and the ruleset must
name that App as the check's expected source: the trust model of
`src/deferred_tier.rs`. The review App (plan item 7) is the natural poster.

## Running it locally

```bash
python3 scripts/risk_tier.py --repo danielraffel/Shipyard --pr 950 --gh ghapp
python3 scripts/risk_tier.py --repo Generous-Corp/pulp --pr 11172 --gh ghapp --json
```

It prints the assessment and posts nothing unless `--post` is given
(`--check-run` adds a neutral check run with the full list).

## Copying it to another repository

Copy `scripts/risk_tier.py` (standard library only, Python 3.11+) and
`.github/workflows/risk-tier.yml`; set `SCRIPT` in the workflow if the script
lives elsewhere (Pulp: `tools/scripts/risk_tier.py`). Add a `[review.tiers]`
table for the repository's own steward, queue or engine paths.
