# Loss baseline, 2026-11-12 (Ledger lane, items L1 to L3)

Facts, method, coverage and limits. Produced by `planning/findings/scripts/loss_baseline.py` (stdlib only, read-only) from data the fleet already has. Nothing was changed on m3. Elapsed time is context only; the ranking below is by work.

## 1. Where the full per-PR rows are persisted (verified)

| Source (on m3) | Holds | Retention | Usable as the ledger? |
| --- | --- | --- | --- |
| `~/Library/Logs/shipyard-steward-tick/<repo>-<stamp>.json` | Raw `runner steward --json` stdout, one file per repo per tick. Every open PR has a row: `number`, `head_sha`, `blocked_on`, `blocked_detail`, `since`, `since_source`, `held_by`, `superseded`, `next_action {actor, action, stop}`, `decision`, `mutation`. | **3 days.** `shipyard-steward-tick.sh` ends with `find "$log_dir" -name '*.json*' -mtime +3 -delete`. Oldest file today: 2026-11-08T23:26Z. 4,003 files, 62 MB. | Complete, but rotated away. Nothing after the window is kept. |
| `.../shipyard-steward-tick/summary.log` | One line per repo per tick; `oldest=` lists only the 3 oldest blocked PRs (token, age). | Kept (1.4 MB, since 2026-10-30). | Biased: top 3 per repo per tick, no head, no actor. |
| `~/Library/Application Support/shipyard/steward-dwell/overdue.jsonl` | One row per overdue observation (`state`, `blocked_on`, `head`, `entered_at`, `age_minutes`, `next_action`, `act` disposition). 1,161 rows since 2026-11-09. | Unbounded append. | Only PRs past a dwell limit; repeated observations, not episodes. |
| `merge-steward.json` (`audit`, `dwell.acts`, `infra_reruns`, `terminal_handoffs`, `superseded_checks`) | Recovery acts and receipts. `audit` is capped at 1,000 rows. | Capped or overwritten. | Recovery decisions only, no blocked transitions. |
| `queue-arms.jsonl` (1,460 rows since 2026-10-27), `merge-receipts.jsonl` (2 rows), `queue-observer/*.transitions.jsonl`, `relay/shadow-deliveries.jsonl` (1,233 rows since 2026-11-09) | Arms, merge receipts, queue transitions, relay events including repair-lane `claim_taken`. | Unbounded append. | Useful joins. Not a blocked-time record. |
| `metrics/metrics.db` | Job-level samples, 23,520 jobs. Only project `pulp` has recent rows. | Unbounded. | Project level; see section 5. |

Conclusion: the full rows exist only for the last 3 days. There is no stable form, so the `shipyard metrics loss` reader is not justified yet. L1 needs a writer (section 7). This report is also the only backfill: the tick files copied to `/tmp/sy-plan-evidence/ledger/ticks` will be deleted by the rotation within 3 days. They should be archived (one tar, read-only) now.

## 2. Method

- Observations: 22,661 PR rows from 4,003 tick files, 2026-11-08T23:26Z to 2026-11-12T23:30Z, 7 repositories, 548 PRs.
- Episode: contiguous run of one (repo, PR, head, blocked_on token). A change of head or token ends it. A PR absent from a tick that ran ends it; a missing tick file does not. 22,661 rows collapse to 3,286 episodes (6.9 to 1).
- Class: `normal_wait` (next_action actor is `ci`, `steward` or `nobody`, or a system-wait token: `queued`, `admitted`, `ready`, `deferred_pass_budget`, `revalidating`, `transient_rerun`, carried-commit retry), `intentional` (`draft`), `human_stop` (`next_action.stop = true`: `excluded_label`, `direct_merge_refused`, `owner_decision`), `actionable` (author, reviewer, repair or operator must act). `required_pending` on a live run is normal wait; `required_pending` whose actor is the reviewer counts as actionable.
- Abandoned: an episode on a PR that GitHub shows closed unmerged is reported as `abandoned_pr` and is excluded from every other class. PRs still open at the end of the window are not knowable as abandoned.
- Cleared by: the next observation for the PR. A new head means a push after the block. The same head with another token means it cleared without a push. Leaving the list is resolved to merged or closed through GitHub's closed-PR lists.
- Agent work: pushes after a block (from ticks), and `repaired:` / `dispute:` comments (GitHub issue comments in the window, each assigned to exactly one episode of its PR).
- GitHub reads: 168 calls of the 400 cap. 7 closed-PR list pulls (700 pulp, 300 vellum, 300 Shipyard, others smaller), 7 comment lists (pulp 25 pages, 2,429 comments; 4,977 in all), 1 single PR lookup, 110 per-head Actions run lists (sample, stratified by repo and token), plus 2 early probe calls. `ghread` for Generous-Corp, `ghapp` for danielraffel. Cached under `/tmp/sy-plan-evidence/ledger/cache`.

## 3. Check (a): is merge_state:DIRTY still the top class?

Elapsed time: yes, verified. Reproducing the plan's table from `summary.log` gives DIRTY 114 PRs, 2,770 hours (plan: 111, 2,759). Deduplicated episodes in the 4-day tick window give 2,361 elapsed hours for DIRTY and UNSTABLE actionable episodes, rank 1 of 13.

By work, no. In the tick window, after removing abandoned PRs:

| Rank by | Result |
| --- | --- |
| Actionable episodes | `required_failed` 550, `merge_state` 167 (DIRTY 150), `no_verdict` 68 |
| Agent work (pushes after a block plus `repaired:`/`dispute:` comments) | `required_failed` 503, `merge_state` 236, `held` 65, `ejected_same_head` 27 |
| Sampled wasted run minutes per head (n of 7 to 18, an estimate) | `ejected_same_head` 410, `merge_state` 73, `required_never_ran` 26, `required_failed` 13 |
| Elapsed hours (context) | `merge_state` 2,361, `required_never_ran` 864, `required_failed` 824 |

So DIRTY is first by elapsed time and second by agent work, behind `required_failed`. `required_failed` splits as review-verdict only 235 episodes (a HOLD or missing verdict: 97 cleared by a push, 132 cleared on the same head with no push), review-verdict plus other checks 149, other checks only 166. By wasted CI, `ejected_same_head` is the largest per head but the sample is 7 heads and 23 episodes; treat as an estimate.

Why the elapsed number overstates DIRTY (verified from `summary.log` and the closed-PR lists):

- 114 DIRTY PRs: 99 merged, 14 closed unmerged (8 of them labelled superseded), 1 open. Hours on PRs closed unmerged: 127 of 2,770 (4.6%). By PR count 12%; by episode count in the tick window 49 of 216 `merge_state` episodes (23%) are on abandoned PRs, which matches the Conflicts lane's roughly 20%.
- 1,809 of the 2,770 DIRTY hours (65%) sit in four repositories whose base admits only by hand (`direct_merge_refused`): spectr 840 h (one PR, still open), cmux-capacity-watchdog 393 h, fleet-handbook 371 h, fleet-ssh 205 h. Nobody was waiting on those. The auto-admitted repositories account for about 960 h: pulp 675, Shipyard 175, vellum 93, tartci 18.
- In the tick window 119 of 166 resolved DIRTY episodes cleared with a push (a real repair), 47 with no push. Median elapsed for a pulp DIRTY episode is 199 minutes; 35 PRs had more than one DIRTY episode (it came back after a repair).

Verdict: DIRTY remains the largest source of repair pushes after review failures and the largest by elapsed hours, but it is not the top class by work, and about two thirds of its elapsed hours are an artifact of manual-merge repositories. Verified for the window and for `summary.log`; the sampled wasted-minute column is an estimate.

## 4. Check (b): agent-free resolution rate

Definition used: an actionable, non-abandoned episode that was resolved in the window, cleared on the same head (no push), with no `repaired:` or `dispute:` comment inside the episode.

- Denominator: 1,025 resolved actionable episodes (1,032 actionable, 7 still open at the window end). Excludes 279 episodes on abandoned PRs, 1,916 normal waits, 31 draft, 28 human stops.
- Numerator: 459 cleared on the same head with no repaired/dispute comment = **45%**, an upper bound. A stricter version, requiring the next token to be a normal wait (the system moved it along), gives 304 = 30%.
- 448 (44%) cleared by a push after the block; 89 (9%) left the list as merged (counted as not agent-free because the actor is unknown; 42 of them are `unmanaged` PRs merged by hand).
- By family: `no_verdict` 63 of 67, `required_pending` 36 of 43, `approval_excluded` 17 of 18, `carries_unapproved_commits` 24 of 28 are waiting on a reviewer or a parent PR and clear alone; `required_failed` 258 of 548, `merge_state` 32 of 167, `held` 0 of 35, `ejected_same_head` 6 of 23.
- Why it is an upper bound: a same-head clear can still be an agent or the owner rerunning a job or adding a label without a comment. The comment source is 3,777 bot comments by one app identity (`shipyard-local[bot]`) that authors, repair lane and reviewer all share, so a comment cannot be attributed to a role. `next_action.actor` is the role that must act at the time, not who acted.
- Unknown share: 0% for comment coverage (all 7 repositories read), but the attribution gap above is not counted in that figure; treat the true rate as between 30% and 45%.

Human interventions per 100 PRs: owner stops raised 4.6 per 100 (24 of 524 non-abandoned PRs; all `direct_merge_refused`, `owner_decision`, `excluded_label`). This counts stops raised, not answered: answered is **unavailable**. Comments by the owner login inside episodes: 39 (upper bound, agents using the personal token are included). `repaired:`/`dispute:` comments: 553 inside episodes over 524 PRs (105 per 100 PRs), 578 distinct in the window on 241 PRs; 21 are disputes.

False holds: 207 HOLD verdict comments on 190 distinct (PR, head) pairs. 12 candidates (6%): 3 where an APPROVE later named the same head with no push, 9 where the comment says the reviewer could not read the workspace (Vellum 385, Shipyard 1170, spectr 248 and 324, pulp 11880, 11842, 12133, 12143, fleet-handbook 52). The text match is a heuristic (estimate). Upheld disputes cannot be scored mechanically from comments alone: 21 disputes exist, and the outcome of each is not recorded in a field.

## 5. Check (c): cost components and coverage

| Component | Source today | Coverage | Note |
| --- | --- | --- | --- |
| Episode count, actor, token, head, since | tick JSON | available, 4 days only | Only 2% of episodes start at the window edge, so left censoring is small; `since` can still predate the first observation |
| Pushes after a block (fix rounds) | head changes in ticks | available, 4 days | Proxy for agent fix rounds; cannot say whether a human or an agent pushed |
| `repaired:` / `dispute:` comments | GitHub issue comments | partial | One shared bot identity; comments outside the 4-day window not read |
| Wasted and rework compute per episode | GitHub Actions runs by head (sample 110 heads; 91 attached to actionable episodes), wall time of failed/cancelled runs | partial (estimate) | Run wall time overstates job minutes; one head's runs are attributed to its first episode only |
| Scorecard `wasted_compute_minutes` | `metrics.db` on m3 | unavailable per episode | Project level, pulp only: 138.6 min over 14 days, 367 failed jobs, coverage "available" for that project. No PR identity (`distinct_pull_requests` 0), freshness `stale` (last GitHub import 2026-09-01), `wasted_compute_per_safely_merged_change` unavailable (2 merge receipts), no fleet-capacity samples. No data for Shipyard, vellum, tartci, fleet-handbook, spectr |
| Local gate runs per head | `metrics.db` `runs.sha` | partial | pulp only; 72 of 1,032 actionable episodes join (264 runs); local validation, not CI |
| `gate_cost` weekly scorecard | GitHub, cached | unavailable here | Cache last written 2026-10-30; merged-PR level, no blocked reason |
| Reruns | `run_attempt` from the sampled runs; infra-rerun ledger in `merge-steward.json` (2 entries) | partial | Sample only; the ledger holds current state, not history |
| Human interventions answered | none | unavailable | Only stops raised are recorded |
| Downstream PRs held | `carries_unapproved_commits:#N` children | partial | Stack holds only: 7 distinct child PRs carried a parent's unapproved commits; the downstream column sums children per parent episode, so a child behind a long-lived parent is counted once per parent episode (overcount). Queue entries behind an ejection and base-poison holds are not measured |
| Dispute outcomes, false approvals | none as a field | unavailable | Heuristic text match only |

Unavailable is not zero anywhere in the tables; the script prints `unavail`.

## 6. Tables

#### Episodes by class

| class | episodes | share |
|---|---|---|
| normal_wait | 1916 | 58% |
| actionable | 1032 | 31% |
| abandoned_pr | 279 | 8% |
| intentional | 31 | 1% |
| human_stop | 28 | 1% |

#### By blocked_on family (all repos)

| key | episodes | actionable | human_stop | abandoned_pr | cleared_by (actionable) | pushes_after_block | repaired/dispute cmts | reruns (sampled) | wasted run-min (sampled) | heads w/ run data | downstream PRs held | elapsed h (context) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| required_failed | 624 | 550 | 0 | 74 | new_head(push)=259; same_head->normal_wait=170; same_head->other_block=98; merged=20; still_open=2; same_head->human_stop=1 | 259 | 244 | 31 | 236 | 18/550 | 31 | 823.7 |
| merge_state | 216 | 167 | 0 | 49 | new_head(push)=110; same_head->normal_wait=25; merged=21; same_head->other_block=10; same_head->human_stop=1 | 110 | 126 | 36 | 1029 | 14/167 | 14 | 2361.2 |
| no_verdict | 70 | 68 | 0 | 2 | same_head->normal_wait=47; same_head->other_block=17; new_head(push)=3; still_open=1 | 3 | 3 | 10 | 1 | 6/68 | 0 | 25.0 |
| unmanaged | 62 | 62 | 0 | 0 | merged=42; new_head(push)=16; same_head->normal_wait=2; still_open=1; same_head->human_stop=1 | 16 | 3 | 4 | 0 | 5/62 | 0 | 470.5 |
| required_pending | 98 | 44 | 0 | 54 | same_head->other_block=21; same_head->normal_wait=17; new_head(push)=5; still_open=1 | 5 | 2 | 12 | 3 | 6/44 | 2 | 13.5 |
| held | 38 | 36 | 0 | 2 | new_head(push)=33; same_head->other_block=2; still_open=1 | 33 | 32 | 17 | 4 | 7/36 | 0 | 33.3 |
| carries_unapproved_commits | 30 | 28 | 0 | 2 | same_head->other_block=13; same_head->normal_wait=11; merged=3; new_head(push)=1 | 1 | 1 | 2 | 0 | 1/28 | 9 | 57.3 |
| required_never_ran | 29 | 26 | 0 | 3 | same_head->normal_wait=18; new_head(push)=5; same_head->other_block=2; still_open=1 | 5 | 12 | 13 | 338 | 13/26 | 0 | 863.7 |
| ejected_same_head | 27 | 23 | 0 | 4 | new_head(push)=13; same_head->other_block=5; same_head->normal_wait=3; merged=2 | 13 | 14 | 24 | 2872 | 7/23 | 0 | 65.5 |
| approval_excluded | 18 | 18 | 0 | 0 | same_head->normal_wait=16; same_head->other_block=1; merged=1 | 0 | 0 | 25 | 0 | 8/18 | 0 | 34.4 |
| steward_error | 5 | 5 | 0 | 0 | same_head->normal_wait=4; new_head(push)=1 | 1 | 0 | 2 | 0 | 2/5 | 2 | 1.1 |
| enqueue_refused | 3 | 3 | 0 | 0 | same_head->normal_wait=2; new_head(push)=1 | 1 | 1 | 5 | 55 | 3/3 | 0 | 1.4 |
| admission_missing | 2 | 2 | 0 | 0 | new_head(push)=1; same_head->normal_wait=1 | 1 | 0 | 0 | 0 | 1/2 | 1 | 1.5 |
| excluded_label | 44 | 0 | 1 | 43 |  | 0 | 0 | unavail | unavail | 0/0 | 0 | 0.0 |
| deferred_pass_budget | 26 | 0 | 0 | 26 |  | 0 | 0 | unavail | unavail | 0/0 | 0 | 0.0 |
| direct_merge_refused | 25 | 0 | 25 | 0 |  | 0 | 0 | unavail | unavail | 0/0 | 0 | 0.0 |
| owner_decision | 12 | 0 | 2 | 10 |  | 0 | 0 | unavail | unavail | 0/0 | 0 | 0.0 |
| admitted | 4 | 0 | 0 | 4 |  | 0 | 0 | unavail | unavail | 0/0 | 0 | 0.0 |
| queued | 4 | 0 | 0 | 4 |  | 0 | 0 | unavail | unavail | 0/0 | 0 | 0.0 |
| draft | 2 | 0 | 0 | 2 |  | 0 | 0 | unavail | unavail | 0/0 | 0 | 0.0 |

#### By repo

| key | episodes | actionable | human_stop | abandoned_pr | cleared_by (actionable) | pushes_after_block | repaired/dispute cmts | reruns (sampled) | wasted run-min (sampled) | heads w/ run data | downstream PRs held | elapsed h (context) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Generous-Corp/pulp | 804 | 620 | 0 | 184 | new_head(push)=264; same_head->normal_wait=165; same_head->other_block=108; merged=81; still_open=2 | 264 | 250 | 45 | 4214 | 21/620 | 55 | 1397.7 |
| Generous-Corp/vellum | 276 | 181 | 2 | 93 | same_head->normal_wait=85; new_head(push)=67; same_head->other_block=21; merged=6; same_head->human_stop=1; still_open=1 | 67 | 72 | 46 | 214 | 19/181 | 4 | 306.4 |
| danielraffel/fleet-handbook | 96 | 86 | 8 | 2 | new_head(push)=34; same_head->normal_wait=30; same_head->other_block=18; merged=2; same_head->human_stop=1; still_open=1 | 34 | 32 | 31 | 2 | 15/86 | 0 | 589.4 |
| danielraffel/Shipyard | 74 | 74 | 0 | 0 | new_head(push)=44; same_head->other_block=15; same_head->normal_wait=15 | 44 | 40 | 35 | 15 | 15/74 | 0 | 101.7 |
| danielraffel/tartci | 47 | 47 | 0 | 0 | new_head(push)=27; same_head->normal_wait=15; same_head->other_block=5 | 27 | 27 | 23 | 45 | 6/47 | 0 | 62.0 |
| danielraffel/spectr | 22 | 13 | 9 | 0 | same_head->normal_wait=6; new_head(push)=4; still_open=2; same_head->other_block=1 | 4 | 9 | 0 | 48 | 7/13 | 0 | 1673.1 |
| danielraffel/fleet-ssh | 20 | 11 | 9 | 0 | new_head(push)=8; same_head->human_stop=1; same_head->other_block=1; still_open=1 | 8 | 8 | 1 | 1 | 8/11 | 0 | 621.8 |

#### Ranking of actionable, non-abandoned episodes: elapsed time versus work

| family | episodes | rank by episodes | elapsed h | rank by elapsed | fix pushes + repaired/dispute cmts | rank by agent work | wasted run-min per sampled head | downstream PRs |
|---|---|---|---|---|---|---|---|---|
| required_failed | 550 | 1 | 824 | 3 | 503 | 1 | 13 (n=18) | 31 |
| merge_state | 167 | 2 | 2361 | 1 | 236 | 2 | 73 (n=14) | 14 |
| held | 36 | 6 | 33 | 8 | 65 | 3 | 1 (n=7) | 0 |
| ejected_same_head | 23 | 9 | 66 | 5 | 27 | 4 | 410 (n=7) | 0 |
| unmanaged | 62 | 4 | 471 | 4 | 19 | 5 | 0 (n=5) | 0 |
| required_never_ran | 26 | 8 | 864 | 2 | 17 | 6 | 26 (n=13) | 0 |
| required_pending | 44 | 5 | 14 | 10 | 7 | 7 | 0 (n=6) | 2 |
| no_verdict | 68 | 3 | 25 | 9 | 6 | 8 | 0 (n=6) | 0 |
| carries_unapproved_commits | 28 | 7 | 57 | 6 | 2 | 9 | 0 (n=1) | 9 |
| enqueue_refused | 3 | 12 | 1 | 12 | 2 | 10 | 18 (n=3) | 0 |
| steward_error | 5 | 11 | 1 | 13 | 1 | 11 | 0 (n=2) | 2 |
| admission_missing | 2 | 13 | 1 | 11 | 1 | 12 | 0 (n=1) | 1 |
| approval_excluded | 18 | 10 | 34 | 7 | 0 | 13 | 0 (n=8) | 0 |

DIRTY (actionable, non-abandoned) by repo, tick window:

| repo | episodes | elapsed h |
|---|---|---|
| danielraffel/spectr | 1 | 840 |
| Generous-Corp/pulp | 101 | 636 |
| danielraffel/fleet-handbook | 8 | 496 |
| danielraffel/fleet-ssh | 4 | 206 |
| Generous-Corp/vellum | 24 | 112 |
| danielraffel/tartci | 7 | 19 |
| danielraffel/Shipyard | 5 | 7 |

#### Human-stop episodes (next_action.stop = true)

| repo / family | episodes | PRs | elapsed h (context) |
|---|---|---|---|
| danielraffel/spectr / direct_merge_refused | 9 | 8 | 821 |
| danielraffel/fleet-handbook / direct_merge_refused | 8 | 8 | 261 |
| danielraffel/fleet-ssh / direct_merge_refused | 8 | 6 | 60 |
| Generous-Corp/vellum / owner_decision | 2 | 1 | 13 |
| danielraffel/fleet-ssh / excluded_label | 1 | 1 | 13 |

Owner stops raised per 100 PRs (non-abandoned): 4.6 (24 of 524). This counts stops raised, not stops answered; answered is unavailable.

#### merge_state:DIRTY in the tick window

- DIRTY episodes 199 on 87 PRs; PR outcomes: {'merged': 73, 'closed_unmerged': 11, 'open': 3}
- how DIRTY episodes cleared: {'same_head->normal_wait': 30, 'new_head(push)': 119, 'same_head->other_block': 14, 'merged': 21, 'closed_unmerged': 12, 'same_head->human_stop': 3}
- cleared by a push (a real repair): 119; cleared with no push 47

#### Candidate false holds (HOLD with no defect)

- HOLD verdict comments seen: 207 (comment coverage: {'Generous-Corp/pulp': 'available', 'Generous-Corp/vellum': 'available', 'danielraffel/Shipyard': 'available', 'danielraffel/fleet-handbook': 'available', 'danielraffel/fleet-ssh': 'available', 'danielraffel/spectr': 'available', 'danielraffel/tartci': 'available'})
- candidate false holds: 12 (same-head APPROVE after HOLD, or comment text says the reviewer could not read the workspace)
| repo | PR | head | signal 1 | signal 2 |
|---|---|---|---|---|
| Generous-Corp/pulp | 11733 | edc9467 | approved same head later |  |
| Generous-Corp/pulp | 11880 | 6dd0255 |  | reviewer could not read |
| Generous-Corp/pulp | 11767 | 362457e | approved same head later |  |
| Generous-Corp/pulp | 11842 | a18d4c3 |  | reviewer could not read |
| Generous-Corp/pulp | 12133 | 975f95f |  | reviewer could not read |
| Generous-Corp/pulp | 12143 | 7c46565 |  | reviewer could not read |
| Generous-Corp/vellum | 385 | cb636fa |  | reviewer could not read |
| danielraffel/Shipyard | 1170 | 266a2c2 |  | reviewer could not read |
| danielraffel/fleet-handbook | 43 | aa933a3 | approved same head later |  |
| danielraffel/fleet-handbook | 52 | eb06b5f |  | reviewer could not read |
| danielraffel/spectr | 248 | 4dd2889 |  | reviewer could not read |
| danielraffel/spectr | 324 | 2c7ada7 |  | reviewer could not read |

- distinct `repaired:`/`dispute:` comments in cache: 578 on 241 PRs; assigned to an episode: 553; outside any episode: {'owner': 105, 'interv': 25}
- comments by the owner login inside episodes: 39 (human and agents using the personal token are not separable: upper bound)
- distinct (PR, head) pairs with a HOLD: 190; candidate false holds above are 12 of them

- `repaired:`/`dispute:` comments inside actionable episodes: 553 over 524 PRs = 105.5 per 100 PRs (coverage per repo: {'Generous-Corp/pulp': 'available', 'Generous-Corp/vellum': 'available', 'danielraffel/Shipyard': 'available', 'danielraffel/fleet-handbook': 'available', 'danielraffel/fleet-ssh': 'available', 'danielraffel/spectr': 'available', 'danielraffel/tartci': 'available'})

#### Long horizon from summary.log (2026-10-30 to 2026-11-12, biased: top 3 per repo per tick)

```
summary.log tick rows parsed: 5208; distinct (repo, PR, token): 1361

Reproduction of the plan's elapsed-time table (sum of longest wait per PR and token):

| token | PRs | median longest wait (min) | over 4h | hours |
|---|---|---|---|---|
| merge_state:DIRTY | 114 | 300 | 68 | 2770 |
| direct_merge_refused | 9 | 1119 | 8 | 1086 |
| required_never_ran | 24 | 139 | 8 | 960 |
| required_failed | 290 | 47 | 19 | 723 |
| unmanaged | 66 | 55 | 3 | 476 |
| required_pending | 477 | 21 | 11 | 374 |
| owner_decision | 17 | 441 | 11 | 185 |
| excluded_label | 17 | 613 | 12 | 183 |
| ejected_same_head | 27 | 165 | 9 | 124 |
| merge_state:UNSTABLE | 49 | 45 | 6 | 71 |
| carries_unapproved_commits | 13 | 125 | 2 | 54 |
| held | 25 | 44 | 1 | 36 |
| approval_excluded | 15 | 34 | 3 | 27 |
| no_verdict | 69 | 9 | 1 | 27 |

DIRTY PRs: 114; final state: {'merged': 99, 'closed_unmerged': 14, 'open': 1}

| final state of PR | DIRTY PRs | hours (longest wait) | over 4h |
|---|---|---|---|
| merged | 99 | 1803 | 58 |
| open | 1 | 840 | 1 |
| closed_unmerged | 6 | 78 | 4 |
| closed_unmerged (labelled superseded) | 8 | 50 | 5 |

total DIRTY hours 2770; on PRs that were closed unmerged: 127

Merged PRs with the longest DIRTY waits (hours, repo, PR): [(380, 'cmux-capacity-watchdog', 3), (191, 'fleet-ssh', 10), (123, 'fleet-handbook', 11), (123, 'fleet-handbook', 21), (121, 'fleet-handbook', 13), (106, 'Shipyard', 960), (48, 'Shipyard', 1033), (37, 'pulp', 11787)]
```

The by-repo-and-family table (49 rows) is produced by `python3 planning/findings/scripts/loss_baseline.py analyze ...`; it is omitted here for length.

## 7. What L1 must add (writer outline; nothing under src/app/merge_steward_cmd/ was changed)

Today's gap, in order of harm:

1. **Retention.** Persist, do not rotate. Smallest change that needs no Rust: in the m3 tick script, replace the `-mtime +3 -delete` with a step that appends one compact line per PR transition to a JSONL (or gzips the tick JSON into a monthly archive). This unblocks measurement now and is an ops change, not a steward change.
2. **A transition row.** The steward holds the previous pass's rows in memory only through its own `status-history` file. The writer belongs in the Promotion lane's modules: at the end of a pass, compare each PR's `(head_sha, blocked_on)` with the last persisted row and, on change, append `{ts, repo, pr, base, head, token, since, since_source, actor, stop, detail, held_by, prev_token, prev_head, cleared_by}` to `<state>/blocked-episodes.jsonl`, and close the open row when the PR leaves the list with the terminal state (merged, closed). The reader is then `shipyard metrics loss --since <dur> --json` over that file.
3. **Fields missing today.**
   - `cleared_by` actor: who acted (steward act kind, infra rerun, repair-lane claim id, reviewer verdict comment id, author push). Today it is inferred from the next token.
   - Terminal PR state and time (merged / closed unmerged / superseded) on the closing row.
   - Per-head CI cost: failed and cancelled run minutes and rerun count, read from the data the steward already fetches (`actions/runs`, `rollup_batch`); store counts, not logs.
   - Actor identity on comments and pushes: authors, repair lane and reviewers all post as `shipyard-local[bot]`. A trailer or `role:` field in `repaired:` / `dispute:` / verdict comments, or distinct App identities, would make agent-free resolution measurable.
   - Dispute outcome: a `dispute:` followed by a recorded resolution (upheld, rejected, re-reviewed head).
   - Owner stop answered: the time and kind of the action that cleared a `human_stop` row.
   - Downstream count at the time of the episode (children behind a parent, queue entries behind an ejection).
4. **Read budget.** The writer adds no GitHub reads: it only diffs rows already in memory. Terminal state arrives with the existing next-pass absence.
5. **Backfill.** Only the 3 days of tick JSON. Copy them before they rotate; after that the backfill is `summary.log` at top-3 resolution.

## 8. Limits and unverified items

- The window is 4 days and left-censored. Rankings by elapsed time use `since` and are stable; rankings by work have small counts in the small repositories.
- Wasted-minute figures are from 91 sampled heads using run wall time (estimate). Do not use the numbers for a decision; use the shape.
- Abandonment is known only for PRs that closed before the fetch; open PRs (1 DIRTY PR in the long-horizon set, 3 in the tick window) may later be abandoned. A PR that merged is classed `merged` even if it was only merged after a long wait.
- `since` is trusted as the start of an episode; `since_source` varies (head commit, check completed, label added) and the steward may reset it on a new head.
- The agent-free rate depends on the comment heuristic and a 10-minute grace after an episode ends; see section 4. Between 30% and 45%.
- The tartci and Shipyard repositories had too few episodes (47 and 74) for per-token rates.
- Not verified: whether the `summary.log` rows before 2026-11-08 reflect the same tokens as today (the token vocabulary has grown; `carries_unapproved_commits` and `deferred_pass_budget` are recent).

## 9. Reproduce

```
python3 planning/findings/scripts/loss_baseline.py fetch   --ticks DIR --cache DIR --budget 398
python3 planning/findings/scripts/loss_baseline.py analyze --ticks DIR --cache DIR --metrics-db metrics.db --json-out episodes.json
python3 planning/findings/scripts/loss_baseline.py summary --cache DIR --summary-log summary.log
```
