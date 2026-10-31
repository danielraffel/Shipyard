# AGENTS.md


Read `CLAUDE.md` before making any changes.

## Autonomous architecture contract

Before changing review, queue, CI, release, recovery, fleet, or handoff behavior, read [`docs/autonomy-contract.md`](docs/autonomy-contract.md). Treat its A1-A12 invariants as required behavior. Load live GitHub, Shipyard, TartCI, and host state separately from documentation; never infer current authority or health from a handoff alone. A recovery change must prove remediation/redispatch and authoritative post-action progress, including controller restart and duplicate-effect tests.

## How a PR lands here

- **Open the PR and stop.** Use `ghapp pr create -R danielraffel/Shipyard --base main`. Never arm, enqueue, auto-merge or merge; admission has exactly one writer.
- **Review:** a standing reviewer posts an exact-head APPROVE or a HOLD once required checks are green. Never approve your own team's work.
- **Landing:** this repo is user-owned, so there is no merge queue. The m3 steward tick (every 5 minutes) merges the exact approved head. The ruleset requires `Test macOS ARM64` and `review-verdict`, bound to GitHub Actions.
- **On a HOLD:** fix what it names, with a test. Merge `origin/main` (never rebase or force-push). Push once and comment `repaired: ... at <sha>`; the PR re-enters review on its own.
- **macOS first.** `CI_PLATFORM_FOCUS=macos` (the default) limits Shipyard CI to macOS ARM64 and releases to macOS ARM64, Linux x64 and Linux ARM64; Windows and the Linux CI lane run only with `CI_PLATFORM_FOCUS=all`. A Windows- or Linux-only failure gets a `PARKED/<platform>` issue with evidence and never blocks a macOS landing.
- **Labels the pipeline skips:** `superseded`, `owner-decision`, `PARKED/<platform>`.
- Launch agents with `sr claude` / `sr codex` and use the smallest model that fits (Haiku or Codex Luna for mechanical work, Sonnet or Codex Terra/Sol for fixes, Opus/Fable/Codex Astra for security, signing and release diffs).
- Steward log on m3: `~/Library/Logs/shipyard-steward-tick/summary.log`.
