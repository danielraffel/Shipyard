# Shipyard scorecard: end-to-end metrics per eligible PR

Scorecard definition version: `2026-11-17.2` (`SCORECARD_VERSION` in `tools/metrics/pr_metrics.py`,
printed in every report header). A change to any definition below bumps it, so a definition change
cannot pose as an improvement. The daily loss report ([below](#daily-loss-report))
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
| **Compute per merged PR** | Verdict: gate **runs** per merged PR, and repeat runs (a run of a workflow on a sha that workflow already ran on, including a push to the base on a merge-group sha, since the queue fast-forwards the base). Runs counted: those whose `head_sha` is one of the PR's heads (its commits and force-push SHAs), its `merge_group` runs (`gh-readonly-queue/<base>/pr-<n>-...`), and pushes on its merge-group shas. `gate-cost` has no repeat-of-merge-group-sha counter; the scorecard's repeat runs come from `tools/metrics/pr_metrics.py`. Job minutes (`completed_at` minus `started_at`, split by the `self-hosted` label) are context. |
| **Correctness** | Of merged eligible PRs: a revert (a commit `Revert ...` naming the merge sha or `#<n>`) within 7 days, or a required check red on the base at the merge commit or the next push (a `base_poison` red-everywhere episode in the ticks if one began then, else check runs against the required contexts; `cancelled` is not red). Provisional while the PR is under 7 days old. **Escaped defect** (best effort): a later PR merged within 14 days whose title or body names this PR as the cause on one line (`fix ... #N`, `regression`, `broke`, `caused`, `introduced`); the detection floor is titles and bodies naming the PR only, so it is never complete. |

## Controls

**Synthetic control (every report).** `tools/metrics/synthetic_control.py` holds fifteen invented PRs, each
built to have one known outcome (autonomous, owner label, steward merge, unbound owner merge, unsigned owner
PR, never merged, reverted, red base, escaped, twice held with repair and merge-main pushes, ejected from the
queue, a gate-run fixture with repeats). `expected()` derives 63 numbers from that table with plain arithmetic
and never calls the scorecard; the real `pr_metrics.build` then runs over the same PRs, served from memory (no
GitHub read, a throwaway cache), and every number must match. The report prints one line, `CONTROL PASS` with the
count and the largest delta, or `CONTROL FAIL` with each missed number, its expected value, its value and the delta.
On a failure the report withholds every outcome number (the per-day tables, the window and baseline pooled rows,
and the per-PR rows in the JSON) and says so: an instrument that cannot reproduce numbers built into it says
nothing about live data. The live positive controls below stay as a second check on real PRs.

A zero is printed with its positive control's count. Built-in controls: fleet-handbook #124 reverted
#119 (the revert detector must find it) and #126's title names #116 (the escape detector must find
it). Pulp has two escape controls, both checked by hand against main-red traces: #12375 fixed
#12219 (Workflow lint) and #12384 fixed #12338 (the WAM link). Neither fix names the PR it fixes, so
the escape detector misses them and Pulp's escaped metric reads `withheld` until it can find them.
`DEFAULT_CONTROLS` can also pin `"owner": [pr, ...]`, PRs with a hand-verified owner action that
must read `owner_action`. PRs the ticks show as `direct_merge_refused` are counted for context only:
that stop is a ruleset config gap, not an owner action, so they can legitimately read autonomous.
A control whose PR is outside the window is "not in window" and unchecked. A control in the window
that is missed withholds only its own metric (revert: correctness; escape: escaped; owner: North
Star); every other number is still reported.

**Read everything or withhold.** PRs are read oldest cohort day first, whole days at a time, and cached. A repo
with any unread PR in its window, or a listing cut short, prints no row at all (the per-day tables, the pooled
window and baseline rows, and the per-PR rows in the JSON are empty) and says how many PRs were unread; the next
run resumes from the oldest unread day. A repo fully read is reported normally even when another repo was cut.
A repo whose read cap ran out only on later reads (compute, checks) is still labelled `partial`.

## Proofs, macOS minutes, queue wait and the acceptance check

The compute block keeps, per merged PR, runs by workflow and level (the PR's own heads, its merge group,
pushes to the base), and the minutes on jobs labelled macOS. `PROOF_WORKFLOWS` names the gate-proof workflow per
repo (Vellum: `Locked macOS GPU proof`); a repo without one shows its busiest workflows. Enqueue to merge is the
minutes from the last `added_to_merge_queue` before the merge to `merged_at`. The run list is read in slices:
the Actions API serves at most 1000 runs for one `created` range, so a busy range is halved until each part fits
(before this, Pulp and Vellum compute reads were never complete and the block was always empty).

`tools/metrics/acceptance.py --from 2026-11-14 --to 2026-11-16 --cache DIR` reads the PRs merged in a window and
compares six numbers measured by hand on 2026-11-15 (Vellum and Pulp ready-to-merge medians, Vellum PR-level and
merge-group GPU proofs per merged PR, Pulp macOS minutes per merged PR, Vellum enqueue-to-merge median) with the
scorecard's own values, within 15%. The synthetic control covers these fields too.

## Clean and slow paths

An ejection is a removal from the merge queue other than the merge itself. GitHub records a removal when a PR merges
and the REST timeline names no reason, so a removal within two minutes before `merged_at` (or one that says
`merged`) is the merge, not an ejection (the rule the m3 fleet-metrics parser uses). Without it every merged PR
read as ejected and as slow.

Each merged PR is `slow` when it had any HOLD, a failed run on one of its heads, an ejection from the merge queue
or a DIRTY tick; `clean` when none of these and its compute was read in full; otherwise `unknown` (a failed run
cannot be ruled out without the run list), and unknown PRs are in neither path. The report gives the slow share
(slow over slow plus clean), the clean median and the slow median and p90 lead time (first ready to merge), each
with its n, and the minutes per merged PR that each cause held the slow PRs up: HOLD (each HOLD to the next push
or the merge), red (each failed head run to the next push or the merge), queue (each enqueue to its removal or the
merge), runner (queued job time from the jobs' `created_at` to `started_at`) and DIRTY (not measured: the ticks
record a state, not a duration, so its cell is empty and never zero). The causes overlap and are not additive. The per-run jobs reads are most of a compute read; a measurement run can set `pr_metrics.READ_JOB_MINUTES = False` to classify paths from the run list alone (the runner cause is then empty and nothing is cached).
The top cause is the largest of the measured ones.

## Main-red episodes (change failure)

The detector reads the checks of every push to the base in the window (each context's latest run decides it; a push
with unfinished runs or an unread page says nothing and is skipped; an episode still open there ends `end unknown`, with no fixer and no duration, since the base may have gone green and red again unseen). Each context's latest run decides a push, so a red that a rerun on the same commit turned green is a flake, not an episode. A direct push's time is its commit date (no push time is recorded), floored at the push before it, so a fix by direct push can read as a shorter time to green. An episode starts at the first push where a
context is red that was not red before and is attributed to the PR whose merge commit that push is (a direct push,
or a first usable push that is already red, has no PR). It ends at the push where its last context turns green; that
push's PR is the fixer. A run still queued or running three hours after its push is stuck (GitHub keeps phantom queued runs for days): it is neither red nor green and does not hold the push open. A context seen on one push only (a per-version or per-event name) can never turn green again and is not tracked, unless that push is the newest one: a check new on the latest push is followed until a later push shows whether it recurs. The head's runs are read with `filter=all`, so a rerun after the merge cannot hide the run that decided it. A context that turns red (from green or absent) on a tenth or more of the pushes it appears on (judged over at least 20 pushes) is a PR gate that flaps on a merge-queue base (`review-verdict`, the reviewer `judge`), not a check a merge breaks, and is not tracked either; one sustained break that stays red for many pushes turns red once and is still followed. The report lists both kinds. A context's newest run is the one with the highest id, so a queued rerun holds its push open. Pushes are the base's first-parent chain from its tip (when the listing stops short of the tip, from the newest merged PR's merge commit in it, since a cut listing can hold a feature commit whose merge fell outside it), ordered by those links, not commit dates (a merge queue fast-forwards the base, so each commit keeps its own older date; listing a base that takes merge commits also returns every merged branch's own commits, which were never base pushes), and a PR's push time is its `merged_at`. The listing is read up to 40 pages (Pulp's main lists about 2,900 commits over 8 days, its merged branches' commits included) and starts a day before the first merge in the window, so a queue's merge commit written before its `merged_at` is still read and the first PR has a push before it. A PR is attributed only when its merge commit is a settled push right after another settled push: a merge the listing misses, an unsettled one, or one right after a skipped push has `unknown` attribution (an episode that starts right after a skipped push has no PR), never a known zero. Limits: in a cut listing (a historical window), pushes after the newest listed PR merge are not read, so an episode open there reads unresolved; a rerun on a base push more than a day old is not seen once that push's state is cached. A push's state is cached only when all its runs finished and the push is a day old, and a run counts as stuck only when both the push and the run itself are three hours old (a run with no start time yet is pending), so a rerun queued minutes ago on an old push is pending. `head red` takes each context's newest run started by `merged_at` (pending if it had not finished by then, or if a run of that context has not started at all: it may have been queued before the merge), so a rerun after the merge does not change it; an episode whose head was not red in any of its contexts and not green in all of them (pending, absent or unread) is `head unknown`, neither a change failure nor other post-merge red. The report prints counts and the raw minutes to green, with no rate and no median, plus
`head red`: episodes whose PR's own final head was red in the same context when it merged. A known-incident control
(`DEFAULT_CONTROLS[repo]["incident"] = [(escaped PR, fixer PR[, head red at merge])]`; Shipyard pins #1459, #1462 and a green head, verified against their check runs) must be found, with the head verdict when one is pinned: the report prints recall k/n, and a
miss fails the control and withholds the change-failure cells only. `base_red` and the text-matched escape stay as
they were and will be retired once the episodes are audited by hand.

## Merge-queue ejections and their cause

The REST timeline's `removed_from_merge_queue` event names no reason, so every removal read as `unknown`, including
the removal that happens when a PR merges (781 of 781 in the 2026-11-09 to 11-15 cache). For a PR with such a removal
the scorecard now asks GraphQL once (`RemovedFromMergeQueueEvent.reason`, lowercased, matched to the REST event by
time; ported from the m3 fleet-metrics parser). A reason of `merged` or `already_merged` is the merge, not an
ejection. A PR whose cause could not be read keeps `unknown`, is not cached and is read again next run. The report
prints ejections, ejections per merged PR, the top cause and the unknown-cause count. GraphQL goes through `ghapp`
(the read-only reader has no GraphQL route) and counts against the read cap like any read.

## Sample and comparison

Every rate carries a 95% Wilson interval and n. A sample is **insufficient** below 50 PRs, when the
window's interval overlaps the baseline's, or when that repo was not fully read (cap hit, read error, unread PRs or a cut-short listing; it is then labelled `partial`). Sufficiency is per repository: a fully read repo is unaffected by another repo's truncation. The report prints the
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

## Baseline cohort

The baseline cohort is the PRs ready 2026-11-12 to 2026-11-14. A day closes 48 hours after it ends
(11-12 at 11-15 00:00 UTC, 11-14 at 11-17 00:00 UTC), so the first comparable later cohort day
closes 48 hours after that day. The frozen baseline is recomputed after 2026-11-17 00:05 UTC from
the cache.

The Vellum positive control for repeat runs is `tools/metrics/fixtures/vellum-gpu-runs-20261113-15.json`:
of 44 push runs of the GPU proof, 42 are on a sha a merge-group run already ran.

## Daily loss report

`tools/metrics/loss_report.py` turns the steward's per-tick JSON into a daily
account of what blocked pull requests cost. It is the one definitions table for
the loss numbers: the plan, the scorecard and the summary line use these terms.

The steward tick keeps each tick's JSON live for three days, then bundles it by
UTC day under `archive/ticks-YYYYMMDD.tar.gz` for 180 days. The report reads
both, once per file, for a window (`--since`, default `7d`). It reads no
GitHub data unless `--github DIR` names a directory of cached REST answers, and
then it still reads only that directory.

Once per UTC day the tick starts it detached (same pattern as fleet-deploy), from
the deploy clone at `origin/main`. It writes
`~/Library/Logs/shipyard-steward-tick/loss-report-YYYYMMDD.{json,md}` and one
`loss-report` line to `summary.log`. It takes its own lock, marks the day when it
starts, and exits 0 whatever happens; a failure is one `loss-report ERROR` line
and a retry the next UTC day. `SHIPYARD_LOSS_REPORT=off` turns it off.

```
python3 -I tools/metrics/loss_report.py --log-dir DIR [--since 7d] [--github CACHE] [--json-out F] [--md-out F]
```

| Term | Definition |
| --- | --- |
| Episode | One contiguous interval in which one PR head carried one `blocked_on` token. Repeated tick observations count once. A change of head or token ends it; so does a gap of more than 120 minutes between a repository's ticks (the episode is then neither cleared nor counted resolved). A repository tick that reports errors or no PR list is an unobserved interval, not an empty one: open episodes stay open across it and the window coverage drops to `partial`. |
| Class | `actionable`: `next_action.actor` is author or repair (the actors `next_action` treats as having something to do), or the actor is missing, unless the token is a wait. `normal_wait`: actor `ci`, `reviewer`, `steward`, `operator` or `nobody`, or a waiting token (`queued`, `capacity_queued`, `required_pending`, and the other steward-side waits), whatever the actor. `human_stop`: `next_action.stop`, or `excluded_label`, `direct_merge_refused`, `owner_decision`. `intentional`: `draft`. `closed_unmerged`: the PR was later closed unmerged (needs `--github`). |
| Resolved | An actionable episode that ended by a push, a same-head token change, or the PR leaving the list. |
| Agent-free resolution, upper | Resolved episodes that cleared on the same head with no `repaired:` or `dispute:` comment inside the episode, over resolved episodes. An upper bound: a rerun or label with no comment still counts. |
| Agent-free resolution, strict | The same, and the next token is a normal wait (the system moved it along). |
| Owner stops per 100 PRs | PRs with a `human_stop` episode over PRs seen, excluding closed-unmerged ones when known. Stops raised, not answered. |
| Fix push | An episode of a token that ended with a new head. |
| `review_verdict_same_head_no_push` | Actionable `required_failed` episodes naming `review-verdict` that cleared on the same head with no push. |
| `capacity_queued`, superseded | Episodes with that token; superseded counts the `excluded_label:superseded` label and, apart, rows carrying the steward's `superseded` detection. |
| `red_everywhere` | Episodes whose token, detail or decision names it. `unavailable` when it never appears in the window, since silence cannot tell "none occurred" from "not emitted". |
| Per-day series | Counted on the UTC day an episode was first seen. A day with no tick files is `null`, never `0`. |

Coverage is `available`, `partial` or `unavailable` for the window (days with
ticks, gaps, unreadable files), GitHub reads, agent work, CI compute and owner
stops answered. CI compute per episode and owner stops answered are always
`unavailable` today: tick JSON does not carry them. Elapsed time appears only as
context in the JSON; no number in the summary is a duration.

The four end-to-end per-merged-PR metrics (autonomous merge rate, handoff-to-merge time, compute, correctness) are the scorecard above.
