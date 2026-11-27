# Applying CI's expected derived files: `shipyard apply-derived`

A derived file is regenerated from the tree, for example Pulp's
`test/ctest_script_inputs/` fragments. When a pull request changes what such a
file is derived from and does not regenerate it, a drift check fails. If that
check uploads the files it expected as a workflow artifact, the fix needs no
configure, no build and none of the repository's code: commit the artifact's
files. `shipyard apply-derived` does exactly that, and nothing else.

```sh
shipyard apply-derived --pr 1234                 # dry run: what would change, nothing written
shipyard apply-derived --pr 1234 --push          # write, commit, push the head branch
shipyard --json apply-derived --pr 1234          # shipyard.apply-derived/v1
shipyard apply-derived --pr 1234 --repo OWNER/REPO --path DIR
```

Run it in a clean checkout of the pull request's branch, at the pull
request's head. `--repo` defaults to the checkout's `origin`.

## Why

Over the 14 days to 2026-11-27, Generous-Corp/pulp had about 25
stale-derived-file red rounds a week. Each took a median of 4.2 h from red to
the next green (p25 2.2 h, p75 7.9 h), and almost all of that was waiting for
someone to notice and regenerate, not the 45-second regeneration itself. The
target is under 0.5 h.

## What it reads and checks

1. The pull request: head SHA, head branch, base. A closed pull request, one
   whose head lives in a fork, one whose base is not the repository's default
   branch, and one whose head branch is the default branch are refused.
2. The **base** branch's `.shipyard/config.toml` `[derived_files]` table. The
   pull request's own config is never read, because it could widen the
   allowlist. The `gh` client is built from machine-global config only, for
   the same reason.
3. The checkout: `HEAD` must equal the pull request's head, and
   `git status --untracked-files=all` must be empty.
4. The newest **failed** workflow run of **this pull request** at that exact
   head that uploaded an unexpired artifact named in the config: its head
   repository is the repository, and it is either a `pull_request` run whose
   `pull_requests` names this pull request or a `push` run of its head branch.
   Another pull request opened from the same commit (perhaps onto a base its
   author wrote, with its own workflow) never supplies the files. The
   artifact must report this head; one with no head or another head is
   ignored. Up to 300 runs at the head are read.
5. Each such artifact's zip, parsed in memory (never extracted by a shell
   tool):
   - Only stored or deflated regular files. Symlinks, devices, setuid,
     setgid and sticky bits, encryption, zip64, multi-disk archives,
     overlapping entries and duplicate names (compared case-insensitively)
     are refused. CRC-32 is verified.
   - Every name is normalized: each segment may hold only ASCII letters,
     digits, `.`, `_` and `-`, so absolute paths, drive letters, backslashes,
     NUL, control and bidi characters and Unicode normalization pairs are
     refused, and so are `..`, `.`, empty segments and any segment starting
     with `.git` (`.git`, `.gitattributes`, `.gitmodules`, `.github`).
   - The name, joined to its artifact's `root`, must fall inside the
     allowlist (`paths`).
   - Limits: 16 MiB per archive, 4096 entries, 2 MiB per file and 64 MiB in
     total, enforced while inflating.
6. If any file in the artifact already matches the checkout, the command
   refuses (exit 1) and names those files. `--emit-expected` writes only the
   files that differ, so a match means the artifact cannot explain this red:
   the drift needs a file removed or a regeneration, which this command
   cannot do. It never reports such a run as fixed, and nothing is written.
7. The pull request's head is read again. If it moved, the command stops.

A dry run stops here and prints each file as `new`, `modified` or
`unchanged` with its added and removed line counts.

With `--push`:

- A path the checkout's ignore rules cover, or that git tracks (or has an
  ancestor tracked) as a symlink or submodule, is refused before anything is
  written.
- Each changed file is written through a temporary file and a rename. A path
  that crosses a symlink or a file in the checkout is refused. New files get mode 0644, and existing
  files keep their mode; incoming permission bits are ignored.
- `git status` must then show only the planned paths.
- Any refusal after the first write restores the checkout: the written files
  are reset and checked out again, new files and new directories removed.
- The commit message is fixed: `chore: apply CI's expected derived files from
  run <id>`. Its trailers are the previous tip commit's trailers whose key is
  in `carry_trailers`, then `commit_trailers`. They are carried because the
  trailer gates (version bump, skill sync) read the tip commit, and the new
  commit becomes the tip.
- The head is read a third time, then the commit is pushed to the head branch
  on the remote whose configured URL names the repository. A non-fast-forward
  push fails; it is never forced.

Every git call runs with `core.hooksPath=/dev/null` and
`core.fsmonitor=false`, and commit and push pass `--no-verify`. No repository
code runs.

## Configuration

In the base branch's `.shipyard/config.toml`. This is Pulp's example:

```toml
[derived_files]
# A trailing `/` admits everything under a directory; anything else is one
# exact file.
paths = ["test/ctest_script_inputs/", ".shipyard/changed-surface-families.toml"]
# The ctest names of the drift checks, for the steward's advice.
drift_tests = ["script-test-inputs-drift", "changed-surface-script-families-drift"]
# Trailer keys copied from the previous tip commit.
carry_trailers = ["Version-Bump", "Skill-Update", "Release"]
# Trailer lines added to every apply commit.
commit_trailers = []

[[derived_files.artifacts]]
name = "script-inputs-expected"   # tests/<slug>.json and executables/<slug>.json
root = "test/ctest_script_inputs"
```

A malformed table counts as unconfigured: the command refuses and the steward
gives no advice.

## Steward advice

A `required_failed:<contexts>` row keeps its token. Its next action becomes

> Derived-file drift only: run `shipyard apply-derived --pr N --push` ...

only when all of these hold, each read spent only if the previous one passed:

1. The base config's `[derived_files]` names artifacts and `drift_tests`.
2. Every required check other than the approval context has completed (one
   still running withholds the advice), and each failing one is a check run
   with an id.
3. A failed run of this pull request at the exact head (the same binding as
   the command, newest five) uploaded a configured, unexpired artifact for
   that head.
4. The head commit is not itself an apply-derived commit. If it is, the last
   apply did not clear the drift, and naming the command again would loop.
5. Each failing check's job (`actions/jobs/<id>`) has exactly one failed step,
   and that step's `The following tests FAILED:` blocks list at least one
   test, every one of them in `drift_tests`.

A drift step that fails stops the steps behind it, so tests later in that job
are unknown until the next round.

Any read that fails, a failure that names no test (a compile error, a crashed
step), or any other failing test or check leaves the ordinary
`required_failed` advice. `shipyard wait pr N --state actionable` and the
steward's own rows both carry the advice.

## Known gaps in Pulp (2026-11-27)

The Shipyard side is complete, but in Pulp the advice cannot fire yet:

- **No artifact where the drift fails.** Of the 14 days' stale-file reds, the
  script-inputs reds (27) and the families reds (11) all failed in the
  `macos` job of Build and Test, which uploads no expected files. Only the
  hosted `drift-fast` job uploads `script-inputs-expected`, and
  `script-test-inputs-drift` was skipped there in every recent failed run
  sampled. Pulp's macOS job needs an `if: failure()` upload of
  `$RUNNER_TEMP/script-inputs-expected`, the directory the check already
  writes.
- **No families artifact.** `changed_surface_script_families.py` emits no
  expected file, so `.shipyard/changed-surface-families.toml` drift has
  nothing to apply until it gains an emit option and an upload.
- **Deletions are not emitted.** `script_test_inputs.py --emit-expected`
  writes added and changed fragments only, so a drift that needs a fragment
  removed is not repaired by this command.
- **No config yet.** Pulp's `.shipyard/config.toml` needs the
  `[derived_files]` table above.

The expected files come from the pull request's merge ref (base plus head),
so on a base that moved they can include the base's newer entries. The drift
check on the next push verifies the result either way.
