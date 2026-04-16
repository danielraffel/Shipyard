# Changelog

All notable user-facing changes to the Shipyard CLI are recorded here.
Plugin changes track in `.claude-plugin/plugin.json`; the two surfaces
are independently versioned.

## [0.5.0]

## [0.4.0]

## [0.3.0] — 2026-04-15

### Added

- **`shipyard ship --resume` / `--no-resume`.** An interrupted ship (laptop
  closed, OS restart, agent crash) now persists durable state to
  `<state_dir>/ship/<pr>.json` on every material event — dispatched run
  IDs, evidence snapshot, merge-policy signature, PR context. A fresh
  session automatically picks up from the same run IDs without
  re-dispatching. Refuses to resume on SHA or merge-policy drift with a
  clear message. Works across agents — the state file is the interop
  point, so a ship you started in Claude Code can resume in Codex (or
  any shell) and vice versa.
- **`shipyard ship-state list | show <pr> | discard <pr>`.** Inspect and
  manage in-flight ship state. `list` is a one-line-per-PR summary with
  title and GitHub URL; `show` dumps every field including dispatched
  runs and evidence snapshot; `discard` archives a stale entry.
- **`shipyard cleanup --ship-state`.** Opt-in pruning of ship state:
  archived files older than 30 days are deleted; active files older
  than 14 days whose PR is closed/merged on GitHub are deleted. Open
  PRs are always preserved. Dry-run by default; `--apply` to delete.
- **PR context in ship state.** `pr_url`, `pr_title`, and `commit_subject`
  are recorded so `ship-state list/show` is self-describing — come back
  to a week-old state file and immediately see what you were shipping.
- Full documentation: [`docs/ship-resume.md`](docs/ship-resume.md).

### Fixed

- `RepoRef` objects are now serialized via their `.slug` property when
  populating the ship-state `repo` field. Prior to this, a ship could
  raise `TypeError: Object of type RepoRef is not JSON serializable`.
- Import ordering in `_preview_ship_state_prune` fixed so ruff passes.

### Phase 2 (deferred)

Cloud-hosted hand-off via Claude Code Routines remains deferred to
issue #41. Phase 1 covers the close-and-reopen-same-day case
completely with zero external dependencies; Phase 2 is only uniquely
valuable when the user is genuinely away for hours (overnight, plane,
weekend). Revisit after two weeks of real-world Phase 1 usage.
