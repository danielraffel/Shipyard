# End-to-end metrics per merged PR

The daily loss report ([rollups.md](rollups.md#daily-loss-report)) counts blocked episodes. These four measure Shipyard end to end,
per repo, per UTC day of the merge. `tools/metrics/pr_metrics.py` computes them;
they are on only when the tick passes `--e2e-cache DIR` (the daily run does, unless
`SHIPYARD_PR_METRICS=off`).

| Metric | Definition |
| --- | --- |
| Autonomous merge rate | Merged PRs with no owner action from opening to merge, over merged PRs where that is determinable. An owner action is a commit, review, label, comment, merge or close by the owner's personal account (`--owner`, default danielraffel) in a human session. App and bot identities, and comments posted through `ghapp` as `shipyard-local[bot]`, are not. Reported as autonomous / owner action / unknown. |
| Autonomy, GitHub-only (`gh-only`) | The same rate with commits ignored: only GitHub-side actions by the owner account (review, label, UI merge, close, comment) count, since agents commit under the owner's git identity and act on GitHub through the App. Reported beside the strict figure. |
| Unknown (autonomy) | An owner-account commit with no agent trailer (`Co-Authored-By: Claude` or `Codex`, `Version-Bump:`, `Skill-Update:`, `Release:`), because agents commit under the owner's git identity and a human commit looks the same; or a timeline or commit list that was not read in full. A definite owner action (any timeline event by the owner) outranks an ambiguous commit. |
| Handoff-to-merge time | Median and p90 minutes, per merged PR, from the first ready-for-review moment to `merged_at`. A PR opened as a draft is ready at its first `ready_for_review` event; any other PR was ready when opened. |
| Compute per merged PR | Actions job minutes (`completed_at` minus `started_at`, skipped jobs excluded) over every run whose `head_sha` is one of the PR's heads (its commits and force-push SHAs) plus its `merge_group` runs (`gh-readonly-queue/<base>/pr-<n>-...`), split into self-hosted (job label `self-hosted`) and hosted. |
| Correctness | Share of merged PRs where, within 7 days of the merge, a commit named `Revert ...` that names the merge sha or `#<n>` landed on the base, or a required check went red on the base at the merge commit or the next push. The red signal is a `base_poison` red-everywhere episode in the ticks if one began in that interval, else the check runs at those commits against the base's required contexts (the tick's `required_contexts`, else the ruleset). A PR under 7 days old is `provisional`: its window is still open, so its rate can only rise. |

Cost is bounded. Merged PRs do not change, so the timeline and commits of a PR, the
jobs of a run, the compute of a PR and the check runs of a settled commit are each
read once and kept under `DIR/<owner>__<repo>/`. Generous-Corp repos read through
`ghread api`, every other repo through `ghapp api`. `--read-cap` (default 100) is the
most reads per run across all repos, split evenly per repo; when a repo hits its
share the report says `truncated` and the next run continues from the cache. Each run
still reads, per repo, the closed-PR listing, one run listing for the window and one
base commit listing; a PR's correctness is re-evaluated from that listing until its
7 days are over. A read that fails leaves a PR unread or a field null, never zero.

`--baseline FROM:TO` (default `2026-11-12:2026-11-14`, `none` to drop it) adds a
Baseline section that pools those merge days per repo, for comparing a later day.
| `review_verdict_same_head_no_push` | Actionable `required_failed` episodes naming `review-verdict` that cleared on the same head with no push. |
| `capacity_queued`, superseded | Episodes with that token; superseded counts the `excluded_label:superseded` label and, apart, rows carrying the steward's `superseded` detection. |
| `red_everywhere` | Episodes whose token, detail or decision names it. `unavailable` when it never appears in the window, since silence cannot tell "none occurred" from "not emitted". |
| Per-day series | Counted on the UTC day an episode was first seen. A day with no tick files is `null`, never `0`. |

Coverage is `available`, `partial` or `unavailable` for the window (days with
ticks, gaps, unreadable files), GitHub reads, agent work, CI compute and owner
stops answered. CI compute per episode and owner stops answered are always
`unavailable` today: tick JSON does not carry them. Elapsed time appears only as
context in the JSON; no number in the summary is a duration.

## Cohort, interventions, escapes and comparison

The four metrics are the Shipyard scorecard; the autonomous merge rate is the headline. They are
measured over a cohort of PRs, not over blocked episodes.

- **Eligible PR.** A merged PR that was non-draft at some point (every merged PR was), whose base
  is the repo's managed base (the tick's `base`, else `main`) or an integration base
  (`develop/*`, `release/*`), and which does not carry `automation` (bot release and
  version-bump PRs), `shipyard:unmanaged` or `superseded`. Closed-unmerged PRs are not in this
  report. The report lists how many PRs each exclusion removed.
- **Cohort day.** The UTC day the PR first became ready for review, not the day it merged. A
  cohort is therefore only complete once its PRs have merged; a recent day undercounts.
- **Human interventions and repair churn are separate.** Per PR: owner actions (timeline events by
  the owner account) versus repair cycles (`repaired:` comments, the repair lane's report that a
  HOLD or red check was fixed and pushed) and HOLD verdicts (comments or reviews that start with
  `HOLD`). Autonomy counts only the first; churn is the second, so a PR that needed three agent
  repairs and no human is still autonomous.
- **Escaped defect (best effort).** A later PR merged within 14 days whose title or body names this
  PR as the cause on one line (`fix ... #N`, `regression from #N`, `broke`, `caused`,
  `introduced`). Text matching can miss or over-match, so it is reported beside revert and
  base-red correctness and never folded into them. Provisional until the PR is 14 days old.
- **Comparison.** The report prints the pooled window cohort and the pooled Baseline cohort
  (default 2026-11-12 to 2026-11-14) next to each other, with sample sizes and a 95% Wilson interval
  on every rate. Medians and p90 carry only n. It makes no per-change attribution: a difference is
  a difference, not evidence of which change caused it.
