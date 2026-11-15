# Shipyard scorecard: end-to-end metrics per eligible PR

Scorecard definition version: `2026-11-15.1` (`SCORECARD_VERSION` in `tools/metrics/pr_metrics.py`,
printed in every report header). A change to any definition below bumps it, so a definition change
cannot pose as an improvement. The daily loss report ([rollups.md](rollups.md#daily-loss-report))
counts blocked episodes; this scorecard measures outcomes per PR. `pr_metrics.py` computes it, and
it is on only when the tick passes `--e2e-cache DIR` (the daily run does, unless
`SHIPYARD_PR_METRICS=off`).

## Cohort

- **Eligible PR.** A PR that was non-draft at some point, whose base is the repo's managed base (the
  tick's `base`, else `main`) or an integration base (`develop/*`, `release/*`), and that does not
  carry `automation` (bot release and version-bump PRs), `shipyard:unmanaged` or `superseded`. A PR
  that is still a draft is `never_ready`. The report lists how many PRs each exclusion removed.
- **Cohort day.** The UTC day the PR first became ready for review (opened non-draft, or its first
  `ready_for_review` event), not the day it merged. The cohort is every eligible PR ready that day,
  merged or not.
- **Horizon.** Every per-PR metric uses the same 48 hours from ready. A cohort day is reported only
  after its horizon closes (end of the day plus 48 hours); an open day prints as "48h horizon open:
  not reported". A day 11-18 is therefore judged on the same time as the baseline.
- Limit: the cohort lists PRs created in the window. A PR opened as a draft before the window and
  made ready inside it is not counted.

## The four metrics

| Metric | Definition |
| --- | --- |
| **North Star: autonomous merge rate** | Eligible PRs ready in the window that merged within 48h of ready with no owner action between ready and merge, over ALL eligible PRs ready in the window (a PR that never merged stays in the denominator). An owner action is a timeline event by the owner's personal account (`--owner`, default danielraffel): review, label, comment, UI merge, close. App and bot identities, and `ghapp` comments as `shipyard-local[bot]`, are not. |
| North Star outcomes | `autonomous`, `owner_action`, `unknown`, `not_merged_in_48h`. **Unknown**: an owner-identity commit with no agent trailer (`Co-Authored-By: Claude`/`Codex`, `Version-Bump:`, `Skill-Update:`, `Release:`), because agents commit under the owner's git identity; or a timeline or commit list not read in full. The unknown share is always shown; above 20% the rate prints as a range (autonomous over all, to autonomous plus unknown over all). `gh-only` is the same rate ignoring commits. |
| **Verdict proxies (mechanism, per ready PR)** | **Review rounds**: HOLD verdicts per ready PR (comments or reviews that start with `HOLD` or `verdict: HOLD`); **re-HOLD rate**: PRs with a HOLD after a repair push or `repaired:` comment over PRs with a HOLD, per PR and by size bucket (<200, 200-800, >800 changed lines). **Agent pushes**: commits after the first ready moment, one per distinct minute, split into `merge_main` (a `Merge branch 'main'` commit), `repair` (after the first HOLD or `repaired:`) and `author` (before). **Merge-queue attempts** per ready PR (`added_to_merge_queue` events) and **ejections by cause** (`removed_from_merge_queue` reasons other than `merged`). **DIRTY per 100 PRs**: PRs whose tick token held `merge_state:DIRTY`. |
| Handoff-to-merge time | Median and p90 minutes from first ready to `merged_at`. **Load-dependent context, not the verdict.** |
| **Compute per merged PR** | Verdict: gate **runs** per merged PR, and repeat runs (a run of a workflow on a sha that workflow already ran on, including a push to the base on a merge-group sha, since the queue fast-forwards the base). Runs counted: those whose `head_sha` is one of the PR's heads (its commits and force-push SHAs), its `merge_group` runs (`gh-readonly-queue/<base>/pr-<n>-...`), and pushes on its merge-group shas. Job minutes (`completed_at` minus `started_at`, split by the `self-hosted` label) are context. |
| **Correctness** | Of merged eligible PRs: a revert (a commit `Revert ...` naming the merge sha or `#<n>`) within 7 days, or a required check red on the base at the merge commit or the next push (a `base_poison` red-everywhere episode in the ticks if one began then, else check runs against the required contexts; `cancelled` is not red). Provisional while the PR is under 7 days old. **Escaped defect** (best effort): a later PR merged within 14 days whose title or body names this PR as the cause on one line (`fix ... #N`, `regression`, `broke`, `caused`, `introduced`); the detection floor is titles and bodies naming the PR only, so it is never complete. |

## Controls

A zero is printed with its positive control's count. Built-in controls: fleet-handbook #124 reverted
#119 (the revert detector must find it), #126's title names #116 (the escape detector must find
it), and PRs the ticks show as `direct_merge_refused` are owner-touched (they must not read as
autonomous). A control whose PR is outside the window is "not in window" and unchecked. If a control
that is in the window is missed, the report prints `CONTROL FAILED: nothing is reported`.

## Sample and comparison

Every rate carries a 95% Wilson interval and n. A sample is **insufficient** below 50 PRs, when the
window's interval overlaps the baseline's, or when the run was truncated. The report prints the
pooled window cohort beside the pooled Baseline cohort (default 2026-11-12 to 2026-11-14, closed
days only) and the baseline's day-against-day North Star rates and spread as the noise floor: a
later change smaller than that spread is no change. It makes no per-change attribution. Medians and
p90 carry only n.

## Cost

Merged PRs do not change, so a closed PR's timeline and commits, a completed run's jobs, a PR's
compute and a settled commit's check runs are each read once and kept under
`DIR/<owner>__<repo>/`; open PRs are read again each run. Generous-Corp repos read through `ghread
api`, others through `ghapp api`. `--read-cap` (default 100) is the most reads per run across repos,
split evenly; a repo that hits its share is `truncated` and the next run resumes from the cache. Each
report prints the read count and the `truncated` flag. A read that fails leaves a PR unread or a
field null, never zero. `--baseline FROM:TO` (default `2026-11-12:2026-11-14`, `none` to drop it).
