# Shipyard gate-cost baseline — seven days

- **Observed:** 2026-10-30T20:59:22Z
- **Repository:** `danielraffel/Shipyard`
- **Workflow / gate:** `ci.yml` / `Test macOS ARM64 [local]`
- **Window:** 2026-10-23T20:59:22Z through 2026-10-30T20:59:22Z
- **Collector:** `shipyard metrics gate-cost --json`
- **Collector source head:** `a130919c116a9d29dec29656f674e822f045c020`
- **Evidence manifest:** [`2026-10-30-shipyard-baseline-evidence-manifest.json`](2026-10-30-shipyard-baseline-evidence-manifest.json)
  The manifest records the exact collector command forms and states that raw API response bodies were not retained.
- **Gate-cost evidence:** [`2026-10-30-shipyard-gate-cost-baseline-7d.json`](2026-10-30-shipyard-gate-cost-baseline-7d.json)
- **Delivery scorecard:** [`2026-10-30-shipyard-gate-cost-scorecard-7d.json`](2026-10-30-shipyard-gate-cost-scorecard-7d.json)
- **Repeated-work shadow:** [`2026-10-30-repeated-work-shadow-cost.json`](2026-10-30-repeated-work-shadow-cost.json)

## Observed baseline

| Measure | Result | Evidence floor |
| --- | ---: | --- |
| Pull requests merged | 62 | n >= 5 |
| Observed gate minutes | 1,890.50 | load-dependent context |
| Wasted gate-attempt minutes | 409.58 | attempts that ran but did not succeed |
| Gate minutes per merged PR | 30.49 | n >= 5 merged PRs |
| PR-head runs per merged PR | 6.73 | n >= 5 merged PRs |
| Wasted attempts per merged PR | 1.94 | n >= 5 merged PRs |
| Push-cancelled PR-head runs | 81 / 417 (19.4%) | n >= 10 runs |
| Median queue wait per job ahead | 2 seconds | n = 403 jobs |

The placement census is partial. It reports a 100% placement-correct share for
readable assignments, but 2,775 jobs include waiting or unassigned rows and the
organisation runner census was unavailable. That percentage is therefore not a
fleet-wide readiness claim.

These count-based measures identify repeated or superseded work as a useful
experiment target. They do not measure safely merged throughput: `main` had no
protected required contexts and no merge-queue runs in this window.

## Scorecard and limitations

The delivery scorecard reports:

| ISO week | Merged PRs | Median open-to-merge | P90 |
| --- | ---: | ---: | ---: |
| 2026-W43 (partial) | 23 | 0.65 hours | 17.71 hours |
| 2026-W44 (partial) | 39 | 4.21 hours | 25.19 hours |

There were no merge-group runs, queue ejections, or required branch-protection
contexts. The scorecard cannot attribute changes in delivery time to queue
admission or required-check behavior.

The collection also had one gate job still running at the end of the window,
so its minutes are excluded. Push cancellations are not split by push type, and
queue-depth history and the organisation runner census were unavailable.

## Repeated-work shadow

The shadow store contains 21 observations: 12 `superseded_exact_head` and 9
`duplicate_exact_head_config`. None has a clean terminal timing join. All 21
durations are therefore unknown, not zero. This is observation evidence only;
it does not support a compute-savings or promotion claim.

## Promotion decision

Keep routing and scheduling unchanged. This is a descriptive baseline. Run the
same collector over an equal follow-up window after a controlled repeated-work
experiment, with canonical terminal timing IDs, and compare wasted attempts per
merged PR, wasted gate minutes, open-to-merge latency, and coordination/rework
cost. Promote only after the follow-up shows an improvement without a reliability
regression and after a trustworthy safely-merged denominator exists.
