# Shipyard gate-cost baseline — seven days

- **Observed:** 2026-10-28T17:11:13Z
- **Repository:** `danielraffel/Shipyard`
- **Workflow / gate:** `ci.yml` / `Test macOS ARM64 [local]`
- **Window:** 2026-10-21T17:11:13Z through 2026-10-28T17:11:13Z
- **Collector:** `shipyard metrics gate-cost --json`
- **Evidence:** [`2026-10-28-shipyard-gate-cost-baseline-7d.json`](2026-10-28-shipyard-gate-cost-baseline-7d.json)
- **Delivery scorecard:** [`2026-10-28-shipyard-gate-cost-scorecard-7d.json`](2026-10-28-shipyard-gate-cost-scorecard-7d.json)
- **Repeated-work shadow:** [`2026-10-28-repeated-work-shadow-cost.json`](2026-10-28-repeated-work-shadow-cost.json)

## Observed baseline

| Measure | Result | Evidence floor |
| --- | ---: | --- |
| Pull requests merged | 70 | n >= 5 |
| Required-gate minutes | 1,562.80 | load-dependent context |
| Wasted gate minutes | 354.02 | attempts that ran but did not succeed |
| Gate minutes per merged PR | 22.33 | n >= 5 merged PRs |
| PR-head runs per merged PR | 4.76 | n >= 5 merged PRs |
| Wasted attempts per merged PR | 1.57 | n >= 5 merged PRs |
| Push-cancelled PR-head runs | 72 / 333 (21.6%) | n >= 10 runs |
| Starvation share | 4 / 342 (1.2%) | n >= 10 gate attempts |
| Median queue wait per job ahead | 2 seconds | n = 332 jobs |
| Placement-correct share | 100% of 2,224 jobs with readable placement | organisation census partial |

The count-based proxies are sufficient for comparison. The 354.02 wasted
minutes and 1.57 wasted attempts per merged PR make repeated or superseded work
the first measured optimization target. The 21.6% push-cancellation share is a
second signal that work is often invalidated before it can contribute to a
merged change. The 2-second median queue wait does not support queue capacity as
the first bottleneck in this window.

## Gaps and promotion decision

This is a baseline, not a promotion decision. There were no merge-group runs or
merge-queue batches in the window, so merge-queue fullness, ejection rate, and
receipt reuse are not estimable. The organisation runner census was unavailable,
and 12 runs lacked the exact gate job name because three were cancelled before
the gate started and nine carried no gate job. Those rows remain visible in the
JSON and are not treated as zero-cost successes.

Keep routing and scheduling unchanged. Run the same collector over the next
equal seven-day window after one controlled repeated-work or supersession
experiment, then compare the count-based proxies. Promote only if wasted
attempts per merged PR and wasted gate minutes fall without a regression in
successful merged PRs or required-gate reliability.

## Delivery outcome baseline

The companion scorecard reports open-to-merge latency by ISO week:

| Week | Merged PRs | Median | P90 |
| --- | ---: | ---: | ---: |
| 2026-W43 | 41 | 1.23 hours | 17.80 hours |
| 2026-W44 (partial) | 29 | 6.37 hours | 26.35 hours |

The week-over-week latency increase is an outcome signal to preserve in the
next comparison. The scorecard found no merge-queue runs, no queue ejections,
and no required branch-protection contexts, so it cannot attribute that change
to queue admission or required-check regressions.

## Repeated-work shadow observation

The local repeated-work shadow store currently contains 14 observations: 3
`superseded_exact_head` and 11 `duplicate_exact_head_config`. None joins to a
survivor duration yet, so avoided compute is currently unknown rather than
reported as zero. This is useful evidence that the shadow path is observing
work, but it is not a promotion result. The next experiment must preserve the
shadow classifications and add a joinable terminal timing witness before
claiming compute savings.

