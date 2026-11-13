# C1: which files cause the DIRTY merge conflicts, and how much is regenerable

Read-only analysis for the "C: conflicts" lane of the autonomous-operations plan. Window: 2026-10-30 to 2026-11-12 (steward log `summary.log`). Nothing was pushed, commented or repaired; PR branches were only fetched into local `refs/c1/pull/<n>`.

## Verdict on the hypothesis

Partly confirmed, partly refuted.

- Confirmed for Pulp, during one week. Pulp is 86 of the 144 reconstructed conflict episodes (60%). Within Pulp, 55 of 86 episodes (64%) touched at least one generated, counter or digest path. The top three paths are all agent-capability artifacts: `docs/status/agent-capability-surface.json` (40 episodes, 47% of Pulp), the `SURFACE_INVENTORY_VERSION` counter in `tools/scripts/agent_capability_manifest.py` (35 episodes touch the file, 29 of them with conflict hunks that are only counter lines, 34% of Pulp), and header digest literals in `agent_capability_catalog_signal.py` (20 episodes, 23%). The counter collision the handbook cites is real: hunks look like `SURFACE_INVENTORY_VERSION = 184` against `= 185`, with the inventory counts in the surface JSON colliding alongside.
- Refuted as the cited file. `ctest_script_inputs.json` is only 9 of 86 Pulp episodes (10%). It was the top file earlier, but Pulp already moved it to one fragment file per entry on 2026-11-08 (`5e3cbfa08e`), and the capability catalog on 2026-11-09 (`2f068cdb4c`). Of the 9 episodes, 2 predate the migration and 7 are old-layout PRs colliding with the deletion.
- Refuted as a fleet-wide claim. Strictly mechanical episodes (every conflicting path regenerable by a named deterministic generator) are 26 of 144 (18%) overall, 22 of 86 (26%) in Pulp, and zero in Shipyard (12 episodes), fleet-handbook (8), Vellum (29, strict), cmux-capacity-watchdog (1). The dominant load outside Pulp is hand-written source, docs and workflow files.
- The mitigation is already working. After 2026-11-09 15:36Z, Pulp had 22 episodes and only 7 touched a generated/counter/digest path (3 on the surface JSON, from old-layout PRs); 15 were purely hand-written. No counter conflict was observed after the cutoff. Before the cutoff: 64 episodes, 48 touching such a path. So deterministic regeneration of those files is a recovery for PRs on the old layout and a shrinking one; the residual conflict load in Pulp is hand-written (mac_platform_tests.cmake, modules.md, C++ sources).

Plain answer: a few generated/counter files did cause most Pulp conflicts through 2026-11-09, but Pulp already took them out of PR diffs structurally. A regeneration recovery would fully address 26 of 144 episodes (18%), or 38 of 144 (26%) if digest and unnamed-derived files are included, and most of those are historic. About 54% of all episodes are pure hand-written conflicts that need a different answer (smaller PRs, earlier merge of main, hot-file ownership, or a conflict-aware repair agent).

## Method and coverage

1. Episodes: from `summary.log` (9,532 tick lines, 2026-10-30T20:14Z to 2026-11-12T23:15Z), every `#<pr>:merge_state:DIRTY` token, per (repo, PR), gaps over 30 minutes split episodes. 114 distinct PRs (the brief says 111; the log parse gives 114), 180 episodes, 8 repositories.
2. Git sources: existing clones for Pulp, Shipyard, TartCI, fleet-handbook (fetched `main` and `refs/pull/<n>/head` into `refs/c1/*` only); blobless bare mirrors under `/Volumes/Atelier/Code/agent-worktrees/c1-mirrors/` for vellum, spectr, fleet-ssh, cmux-capacity-watchdog.
3. Reconstruction: `head_at_T` is the last first-parent commit of the PR on or before the first sighting, `base_at_T` the last first-parent commit of the base on or before it; `git merge-tree --write-tree --name-only --no-messages base head` lists conflicting paths. If clean, a ladder of later times (last sighting, plus 10 minutes) was tried.
4. One REST read per PR (114 calls, `ghread` for Generous-Corp, `ghapp` for danielraffel; no plain `gh`) for state, base, merged/closed time and labels.

Coverage limits, stated plainly:

- The log lists only the 3 oldest blocked PRs per repo per tick. 2,886 of 9,532 ticks (30%) listed 3 entries, so on those ticks other blocked PRs were hidden. Newer DIRTY PRs are under-represented, so 144 episodes is a floor, and short-lived conflicts (resolved before becoming one of the 3 oldest) are mostly missing. The sample is biased toward long-lived conflicts.
- Reconstructed: 144 of 180 episodes (80%), 143 of them on the first attempt, which shows the method matches GitHub's DIRTY signal well.
- Not reconstructed: 30 episodes were clean at reconstruction (22 Pulp, 1 Vellum, 1 Shipyard, 3 cmux-capacity-watchdog, 1 fleet-ssh, 2 TartCI; likely a stale mergeability value, a head the PR had force-replaced, or the state GitHub saw at merge-queue time); 6 episodes (2 Pulp PRs, #11936 and #11937) were stacked on `feature/preset-program-wiring`, a base branch that no longer exists on origin, so the base cannot be rebuilt. The 36 unreconstructed episodes are unknowns, not evidence of no conflict.
- Head/base are chosen by committer date, not push time. A force-pushed or rewritten PR would show its rewritten history. `first sighting` can lag the real conflict by up to one tick (about 5 minutes) plus the 3-oldest filter.
- "Ended" uses the PR's final state from the API and git history after the episode head, so a later episode of the same PR is not separate from the first (25 of 95 conflicting PRs had more than one episode).

## Episode categories

"mechanical" = every conflicting path is a generated file, a counter literal, or a lockfile-style artifact with a named deterministic generator found in the repo. "lenient" adds paths that are deterministic digests of other content but need a review step (`--accept-fingerprint`) or whose generator I could not locate. "mixed" = some generated path plus at least one hand-written path. Paths are classified at hunk level where it matters (the counter in `agent_capability_manifest.py` counts as a counter only when every conflict hunk is a counter line).

| repo | conflict episodes | mechanical (strict) | +digest/derived (lenient) | mixed | hand-written | distinct PRs |
|---|---|---|---|---|---|---|
| Generous-Corp/pulp | 86 | 22 (26%) | 10 (12%) | 23 (27%) | 31 (36%) | 50 |
| Generous-Corp/vellum | 29 | 0 | 2 (7%) | 2 (7%) | 25 (86%) | 22 |
| danielraffel/Shipyard | 12 | 0 | 0 | 0 | 12 (100%) | 9 |
| danielraffel/fleet-handbook | 8 | 0 | 0 | 0 | 8 (100%) | 6 |
| danielraffel/tartci | 5 | 2 (40%) | 0 | 2 (40%) | 1 (20%) | 4 |
| danielraffel/fleet-ssh | 2 | 2 (100%) | 0 | 0 | 0 | 2 |
| danielraffel/spectr | 1 | 0 | 0 | 1 | 0 | 1 |
| danielraffel/cmux-capacity-watchdog | 1 | 0 | 0 | 0 | 1 | 1 |
| ALL | 144 | 26 (18%) | 12 (8%) | 28 (19%) | 78 (54%) | 95 |

Path occurrences across all episodes (338): source 33%, generated 22%, docs 19%, config 9%, counter 9%, digest 7%, derived-unnamed 1%. No path matching a changelog, `Cargo.*`, lockfile, `VERSION` or `plugin.json` conflicted in any episode: version bumps and changelogs are already applied at land (`chore: bump versions`) and never reached a conflict in this sample.

If every generated, counter, digest and derived path were absent from PR diffs, 106 of 144 episodes (74%) would still conflict on hand-written paths (206 of 338 path occurrences remain). 38 episodes (26%) would disappear entirely.

## Top conflicting paths per repository

Share is of that repository's conflict episodes.

### Pulp (86 episodes)

| path | episodes | share | class and generator |
|---|---|---|---|
| `docs/status/agent-capability-surface.json` | 40 | 47% | generated: `python3 tools/scripts/agent_capability_manifest.py --write` (named in `tools/scripts/hotspot_edit_guard.json`); removed from main 2026-11-09 |
| `tools/scripts/agent_capability_manifest.py` | 35 | 41% | counter (29 of 35 are counter-only hunks): `SURFACE_INVENTORY_VERSION` / `MANIFEST_REVISION`, assigned at land by `--assign-pending` (CI step added 2026-10-22, `f9f873bb7a`); the remaining 6 touch real code |
| `tools/scripts/agent_capability_catalog_signal.py` | 20 | 23% | digest: `header_fingerprint='sha256:...'` literals; `--accept-fingerprint` / `--migrate-fingerprints`; fragmentised under `tools/agent-capabilities/fingerprints/` 2026-11-09 |
| `tools/agent-capabilities/contract-history.json` | 13 | 15% | generated: `agent_capability_manifest.py --write` (append-only tail; now `tools/agent-capabilities/history/`) |
| `test/cmake/mac_platform_tests.cmake` | 10 | 12% | hand-written test registration (adjacent `if(APPLE)` blocks appended at the same spot) |
| `test/ctest_script_inputs.json` | 9 | 10% | generated: `script_test_inputs.py --build-dir <dir> --write` (needs a configured build); fragmentised 2026-11-08 |
| `core/runtime/src/durable_file_replacement.cpp` | 6 | 7% | source |
| `docs/reference/modules.md` | 6 | 7% | docs; the table rows are hand-edited by each PR that adds a platform header |
| `tools/scripts/agent_capability_registry.py` | 5 | 6% | digest, plus hand registry rows |

### Vellum (29 episodes, 29 distinct paths, no hot generated file)

`graphics/src/skia_dawn_surface.mm` 6 (21%), `CMakeLists.txt` 4, `.github/workflows/gpu-macos.yml` 4, `docs/architecture/gpu-boundary.md` 4, `graphics/CMakeLists.txt` 3. Derived but unnamed-generator: `scripts/INSTALLER_SHA256SUMS` 2 (sha256 of `install_core.py` and `install.sh`; commit history says "Regenerate INSTALLER_SHA256SUMS", no script located), `provenance/pulp-observatory/reports/current.json` 2 ("chore(provenance): coalesce Pulp observatory evidence"). Conflicts are spread over a long tail of one-off source files: a GPU-surface stack of long-lived feature branches, not a hot-file problem.

### Shipyard (12 episodes)

`src/app/merge_steward_cmd.rs` 6 (50%), `skills/ci/SKILL.md` 3, `skills/shipyard/SKILL.md` 3, `src/cloud.rs` 2, `src/app/merge_steward_cmd/cancellation.rs` 2, `tools/fleet-pass/README.md` 2. All hand-written. `merge_steward_cmd.rs` is the real hot file here (a single large module being edited by concurrent PRs); the SKILL.md conflicts come from the skill-sync policy forcing PRs to touch the same two skill files.

### TartCI (5), fleet-ssh (2)

`fleet/advertised-labels.json` is the only hot file: 4 of 5 TartCI episodes and both fleet-ssh episodes. The conflict is a single line, `"profiles_sha256"`, and the file is a pure function of `profiles/*-macos-fleet.toml`. Generator: `tartci fleet-macos advertised-labels --publish` (`scripts/macos_fleet_lanes.py`), with `--check FILE` as verifier. For fleet-ssh the file has the same `tartci.advertised-labels/v1` schema, so the same generator applies; I did not verify that fleet-ssh runs it from its own checkout.

### fleet-handbook (8), spectr (1), cmux-capacity-watchdog (1)

fleet-handbook: prose docs (`README.md`, `docs/how-we-work.md`, `docs/monitoring.md`, `docs/owner-steps.md`, 2 each) plus pass-runner scripts. All hand-written, which matches its role as a plan of record that every PR edits. spectr: nine paths in one episode including `native-ui/materialized/materialized-document.runtime.json` (materialized output, generator not located). cmux-capacity-watchdog: `README.md` and the main script.

## Mechanical paths and their named generators

| path (class) | episodes | generator | preconditions |
|---|---|---|---|
| `docs/status/agent-capability-surface.json` (generated) | 40 | `agent_capability_manifest.py --write`; verify `--check` | none beyond a Python env; old-layout PRs only now |
| `tools/agent-capabilities/contract-history.json` (generated) | 13 | same | same |
| `tools/scripts/agent_capability_manifest.py` counter lines | 29 | take main's value; CI `--assign-pending` assigns at land | only when every hunk is a counter line |
| `tools/scripts/agent_capability_catalog_*.py`, `agent_capability_registry.py` digest literals | 25 | `--accept-fingerprint <include>` / `--migrate-fingerprints` | acceptance records a reviewer judgment: recompute is deterministic, accepting is not |
| `test/ctest_script_inputs.json` (generated) | 9 | `script_test_inputs.py --build-dir <dir> --write` | needs a configured build directory |
| `docs/status/consumption-profiles.json` (generated) | 2 | `consumption_census.py --build-dir <dir> --write`, a "serialized" hotspot | needs a build; commit carries a `Serialized-Hotspot:` line |
| fragment trees (`docs/status/agent-capability-surface/**`, `tools/agent-capabilities/fingerprints/**`, `test/ctest_script_inputs/**`) | 3 | the same writers | fragment name is a pure function of the entry name |
| `fleet/advertised-labels.json` (TartCI, fleet-ssh) | 6 | `tartci fleet-macos advertised-labels --publish` | `profiles/*.toml` must already be the final text |
| `scripts/INSTALLER_SHA256SUMS`, `provenance/.../current.json`, spectr `materialized-document.runtime.json` | 5 | not located | not allowlisted |

## How episodes ended

- PR final state, per episode: merged 113 (78%), closed unmerged 29 (20%; 24 carry the `superseded` label), still open 2 (1%). Per distinct conflicting PR (95): merged 81, closed unmerged 12 (13%), open 2.
- Fraction on PRs later abandoned: 20% of episodes, 13% of PRs. By repo: Pulp 8 of 50 PRs, Vellum 4 of 22, the rest 0. Abandoned episodes by category: hand 9, mixed 9, lenient 7, strict 4. Mechanical-looking conflicts were abandoned at a higher rate (11 of 38) than hand-only ones (9 of 78), which suggests several were stacked or superseded PRs that were cheaper to replace than to repair.
- Clearing: estimated by walking each PR's first-parent commits after the episode head and finding the first that merges cleanly with main as of its own time. 135 of 144 episodes clear this way; in 135 of 135 the clearing commit is a merge commit (a merge of main), median 1 first-parent step. Median time from first sighting to the clearing commit is 51 minutes (p90 222), by category: strict 44.5, lenient 35, mixed 37, hand 53. Mechanical conflicts were not repaired meaningfully faster than hand-written ones, so the time cost is dominated by someone noticing and merging, not by resolution difficulty. 9 episodes did not clear within the first 80 first-parent commits or at all (closed PRs, force-pushes).
- Merged episodes: median 5.7 hours from first DIRTY sighting to merge (p90 24.4).
- Recurrence: 25 of 95 conflicting PRs had more than one separate episode, so a one-time repair does not hold on a fast-moving base.
- PRs whose every conflict episode was mechanical: 22 of 95 (16 strictly).

## Candidate allowlist for a later deterministic-regeneration recovery

Start only with paths that have a named generator, a verifier, and no judgment step:

1. Pulp `docs/status/agent-capability-surface.json` and `tools/agent-capabilities/contract-history.json` (old layout) and the fragment trees: resolve with main's side, apply the PR's source change, run `agent_capability_manifest.py --write`, verify with `--check`.
2. Pulp counter lines in `agent_capability_manifest.py`: take main's value (counters are assigned at land). Only when every hunk matches `^(SURFACE_INVENTORY_VERSION|MANIFEST_REVISION) = N$`; otherwise stop.
3. TartCI and fleet-ssh `fleet/advertised-labels.json`: take main's side, regenerate with `tartci fleet-macos advertised-labels --publish`, verify with `--check`.
4. Pulp `test/ctest_script_inputs/**` and the old single file: only when a configured build dir exists; otherwise stop and report.

Not on the allowlist: header digest literals (acceptance is a review decision), `consumption-profiles.json` (serialized, needs a build), unnamed derived files, and every path classified source, docs or config. Rule of thumb for the recovery: attempt it only when EVERY conflicting path is allowlisted; a mixed episode should go to a hand-conflict repair, which still gets the generated files regenerated afterwards.

## Better lever: take hot files out of PR diffs entirely

Ranked by observed conflict share, with what is already done:

1. Capability surface and history (47% and 15% of Pulp episodes): already fragmentised (2026-11-09), and the old files are gone from main. Remaining step: make PRs not contain the generated aggregate at all, with the fragment writer run at land. Post-cutoff Pulp episodes touching these: 3 of 22.
2. Counters (`SURFACE_INVENTORY_VERSION`, `MANIFEST_REVISION`; 29 episodes, 20% of all): `--assign-pending` already exists as a land-time step but a literal still lives inside a Python source file that PRs also edit. Moving each counter to a land-time generated file (or deriving it from history) removes the single most frequent hand-conflict-looking line. Zero counter conflicts after the cutoff in this sample, so check that PRs no longer bump it before spending effort.
3. Digest literals (25 path occurrences): already moved to `fingerprints/` fragments on 2026-11-09.
4. `ctest_script_inputs.json`: already fragmentised (2026-11-08); the same pattern (one file per entry, name a pure function of the entry) is the template for the rest.
5. TartCI and fleet-ssh `fleet/advertised-labels.json` (6 episodes, one-line `profiles_sha256`): remove it from PR diffs and publish at land (or from a post-merge job), since it is a pure function of the profile files. This is the only remaining hot generated file outside Pulp in this sample.
6. Hand-written hot files that need an ownership or fragment answer instead of regeneration: Pulp `test/cmake/mac_platform_tests.cmake` (append-only registrations at the same spot, 10 episodes), Pulp `docs/reference/modules.md` (table rows), Shipyard `src/app/merge_steward_cmd.rs` (6 of 12) and the two skill files, fleet-handbook prose, Vellum `skia_dawn_surface.mm`. The same "one file per entry" idea (cmake `file(GLOB)`-style registration, per-entry doc rows) applies to the first two.

## What I could not verify

- Whether `agent_capability_manifest.py --assign-pending` actually runs at land on every PR (read from the script and commit message `f9f873bb7a`, not exercised).
- The generator for Vellum `INSTALLER_SHA256SUMS` and `current.json`, and spectr's `materialized-document.runtime.json` (greps on blobless mirrors were too slow, so I read specific files only).
- That fleet-ssh publishes `advertised-labels.json` with TartCI's tool (same schema, header names the fleet-ssh repo).
- The 30 clean-at-reconstruction episodes: cause not determined.
- True conflict duration (the log's 3-oldest filter and 5-minute tick give lower bounds); first-sighting times for PRs outside the 3 oldest are missing.
- Whether regenerating would have produced a passing PR (merge-tree shows conflicts, not post-regeneration CI results).

## Reproduce

All scripts are read-only against PR branches. Data cache: `/Volumes/Atelier/Code/agent-worktrees/c1-mirrors/data/`.

```
python3 -I planning/findings/scripts/c1_conflicts_stage1.py /tmp/sy-plan-evidence/summary.log DATA   # episodes + 114 REST reads
python3 -I planning/findings/scripts/c1_conflicts_stage2.py DATA                                      # merge-tree per episode
python3 -I planning/findings/scripts/c1_conflicts_stage3.py DATA                                      # conflict hunks
python3 -I planning/findings/scripts/c1_conflicts_stage4.py DATA /tmp/sy-plan-evidence/summary.log    # classification and tables
python3 -I planning/findings/scripts/c1_conflicts_stage5.py DATA                                      # clearing-commit estimate
python3 -I planning/findings/scripts/c1_conflicts_stage6.py DATA                                      # repair and per-PR summary
```

Per-episode rows: `planning/findings/2026-11-12-conflict-classification-episodes.tsv`.
