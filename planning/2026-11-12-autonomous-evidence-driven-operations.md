# Shipyard: Autonomous, Evidence-Driven Operations

Status: plan, revision 4 (2026-11-12). Started: the baseline, the read-only
audits, conflict classification, and the false-HOLD fix. Their reports are in
`planning/findings/`. Acting automation outside "What is in progress" waits
for the ledger.
Every behavior change ships as its own reviewed PR, default-off or shadow, and
is promoted only by the rule in "Promotion ladder".

Revision 1 was written against v0.97.0 (August). Main has since shipped most of
what it proposed to study. This revision is written against main at `e4f025b2`
and replaces "study, then recommend one recovery" with a standing improvement
loop fed by the data the fleet already produces.

The backlog detail is in `planning/2026-11-12-autonomous-operations-backlog.md`. The team brief is
`planning/2026-11-12-autonomous-operations-team-brief.md`.

## Objective

Shipyard improves itself from its own evidence. On a schedule it measures where
changes lose time, ranks the losses, applies or promotes a bounded
deterministic recovery for the largest one, and checks on the next measurement
that the loss shrank. A human is involved only for secrets, logins, spending,
sudo, and decisions that are consequential or new.

> Shipyard makes the correct routine decision, takes the safe next action,
> proves it helped, and asks Daniel only when the decision is ambiguous,
> consequential, or novel.

Models diagnose, propose, and review. A model never authorizes a merge,
satisfies a required check, weakens a policy, or turns unknown evidence into
approval.

### The bar

Shipyard with TartCI should be the system anyone with their own Macs wants to
run: PR to verified merge on local hardware, at a high merge bar, with routine
trouble cleared by machinery and not by an agent session. Four properties
define "best", and each has a measure in this plan:

- **Clears its own routine trouble.** Headline measure: the share of blocked
  episodes cleared with no agent session and no human (agent-free resolution
  rate).
- **Never lowers the merge bar to go faster.** False approvals and unapproved
  content on a base stay at zero.
- **Does not repeat a mistake.** Every recorded lesson has a gate or detector
  in code.
- **Leaves nothing orphaned.** Every durable object has an owner, an expiry,
  and a deterministic disposition.

## What is in progress

The stated problem is complexity, so this plan must not answer it with more
machinery. The rule: anything that measures, labels, or checks may start now;
anything that acts on its own waits for evidence.

**In progress:**

1. **Stop the review lane blocking good changes.** A reviewer that reports
   its own failure posts nothing; an approval must prove the diff was read.
   Fix: fleet-handbook PR 93 (R6 to R8).
2. **Fix what is silently broken.** Changelog sync has failed 45 of its last
   50 runs since 2026-10-31. 44 of the last 100 Shipyard releases are still
   drafts. Each gets a fix and a detector, so the class cannot recur unseen.
3. **Stop losing the evidence.** The only complete record of why PRs are
   blocked is deleted after three days. Keep it (archive the steward's
   per-tick output, then replace it with the ledger). A copy of 2026-11-08
   to 11-12 is preserved outside the fleet's rotation.
4. **One blocked-episode ledger and one weekly report** (L1 to L3), with
   one outcome vocabulary across steward, wait, pr-watch and landing (L5).
5. **Shorten the HOLD and failed-check loop,** the largest class by work:
   findings with stable ids that the repair lane closes, rounds per PR
   measured, and a distinct identity per role so work can be attributed
   (R4).
6. **Score what already exists in shadow** and promote or remove it:
   superseded detection first, then dwell acts and version-only review.
7. **Cut the largest chore source:** the skill-sync path map (S4).
8. **Read-only studies** that supply ground truth: labeled cases, agent
   session review, attention quality, reuse correctness (H1 to H4), the
   lesson-to-gate table and orphan report (X1, X3).

**Not started, and why:** conflict repair by regeneration (the evidence does
not support it), the self-promoting scheduled loop (needs the ledger first),
the recovery arbiter, risk-tier routing, and the rest of the backlog. Detail
and the prior-art table are in
`planning/2026-11-12-autonomous-operations-backlog.md`.

## The improvement loop

One scheduled job, four steps. It adds no scheduler, store, or agent framework:
it reads what the steward, the metrics collector, and TartCI already write.

1. **Measure.** Compute operational cost per repository and per blocked
   reason, as defined in "What is measured". Report coverage and the unknown
   share next to every number.
2. **Rank.** Order loss classes by operational cost, not by elapsed time.
   Attach the evidence rows.
3. **Act.** For the top class:
   - a recovery exists in shadow: evaluate its promotion criteria and, if met,
     open the one-line PR that promotes it;
   - a recovery exists and is enforced: the class is a regression, open an
     issue with the rows;
   - no recovery exists: open one issue with the evidence and the smallest
     deterministic contract that would cover it.
4. **Verify.** Each promoted recovery names the metric it should move and the
   expected direction. If two consecutive measurements show no improvement, or
   any safety counter rises, the job opens the PR that returns it to shadow.

The loop itself starts read-only (steps 1 and 2 only). Steps 3 and 4 open
issues and PRs through the normal reviewed path. The loop never changes a mode
directly.

## What is measured

The loop optimizes work, not wall time. A PR that sat blocked overnight with
nobody waiting on it cost nothing; a PR that burned three macOS runs and two
agent sessions in an hour cost a lot.

The unit is the **blocked episode**: one contiguous interval in which one PR
head carried one `blocked_on` token. Repeated tick observations of the same
episode count once. Each episode is reported with the components below, kept
separate. There is no single composite score: a blended number hides which
component moved and invites gaming. If a weighting is used to rank classes,
it is published beside the ranking and the unweighted components are shown
with it.

| Component | Source | Note |
| --- | --- | --- |
| Wasted CI compute minutes | scorecard `wasted_compute_minutes`, `rework_compute_minutes`, TartCI runtime | Runs that could not have passed, or re-ran unchanged content |
| Retries and re-runs | rerun ledger, `run_attempt` | Counted per head and check |
| Agent interventions | `repaired:` and `dispute:` comments, repair-lane claims, pushes after a block | One per fix round |
| Human interventions | owner actions, `next_action.actor = owner` stops that were answered | Weighted highest |
| Downstream impact | PRs held behind this one (stack children, queue entries behind an ejection, base-poison holds) | Count of other PRs delayed |

Rules:

- An episode counts toward loss only when it was **actionable**: the system
  or someone had to do something to clear it. `required_pending` on a healthy
  run is normal waiting and is excluded.
- Episodes on PRs that were later closed unmerged or superseded are reported
  in their own class and never inflate another class.
- Elapsed time is reported as context beside the cost and is never the
  target. No recovery is justified by elapsed time alone.
- Each component carries `available`, `partial`, or `unavailable` coverage. A
  missing component is unavailable, never zero.
- An episode is an association, not proof of cause. A retry or an agent
  session during an episode is attributed to it only when a receipt links
  them (the rerun names the check, the push follows the HOLD it answers).
  Unlinked work is reported as unattributed.
- "Wasted" compute is limited to what is knowable without hindsight: runs on
  a head that was already superseded, runs after a required check had
  already failed at that head, and re-runs of unchanged content. Anything
  else is plain compute.
- Interventions are counted from receipts and pushes, not from comment text
  alone, because comments can be emitted to move a number. Out-of-band work
  is unobserved and reported as a known gap.
- Closed, abandoned, and superseded PRs are censored observations: they are
  kept with their terminal outcome and exposure, never dropped and never
  counted as zero cost.
- Downstream impact is attributed once, to the root blocker. A stack child
  held by its parent does not add a second episode's cost.
- Every rate has a stated denominator: per PR, per head, or per actionable
  episode.

Outcome measures tracked together, so that none is traded for another:

- agent-free resolution rate: actionable episodes cleared by the steward
  alone;
- false approvals: unapproved or defective content admitted. Hard constraint,
  zero;
- false holds: blocks later shown to name no real defect (upheld disputes,
  defect-free HOLDs, reviewer faults, wrong infra or poison verdicts);
- unnecessary work: wasted compute, duplicate reviews, repeated retries;
- human interventions per 100 PRs.

## What main already has

The plan must build on these, not re-create them.

| Revision 1 proposed | On main today | Mode |
| --- | --- | --- |
| Typed classification set | `blocked_on` tokens with `since`, and `next_action {actor, action, stop}`, one per open PR per steward pass (`src/app/merge_steward_cmd/blocked_on.rs`, `next_action.rs`, `docs/automated-draining.md`) | Enforced |
| "Recommend one recovery loop" | Infra rerun for required checks (`infra_rerun.rs`, back-test 22 of 23 correct) | Enforced |
| | Known-flake and not-head-caused same-head retry | On with a merge queue; `known_flaky_tests` empty by default |
| | Dwell-time acts: stale verdict re-run, `required_never_ran` dispatch, reorder, retarget (`[auto_merge.dwell]`) | Shadow |
| | Superseded detection (`docs/superseded-close.md`) | Shadow; closing not built |
| | Base-poison hold, fast-track, pause (`[base_health]`) | Pause dry-run, `auto_jump` off |
| | Environment requeue, queue-absent recovery | Opt-in, off by default |
| | Runner watchdog | Report-only without `--fix` |
| Failure classification | `ci.cause/v1` cause records, `FailureClass` in `src/classify.rs`, `src/diagnose/classify.rs`: three vocabularies | Enforced, rules only |
| Historical benchmark | `shipyard metrics scorecard`, `gate_cost` weekly scorecard, hourly baseline collector joining GitHub timing and TartCI exports, merge-receipt contract | Running; primary metric `unavailable` without merge receipts and fleet capacity |
| Review usefulness contract | `verdict: APPROVE head:<sha>` / HOLD, `review-verdict` status, reviewer claims and receipts, repair budget | Enforced |
| Low-risk fast path | `tools/fleet-pass/version_only.py` | Shadow, decides after 10 agreements |
| Risk tier | `docs/risk-tier.md` | Report-only; does not route reviewers or models |
| Model escalation receipt | fleet-pass logs the requested model only | Absent |
| Autonomy rollout | `docs/autonomy-contract.md` (A1 to A12, F1 to F8), `planning/autonomy-recovery-rollout-20261027.md` | Contract and evidence in progress, default-off |

Read the Mode column strictly. "Enforced" means the code runs in the fleet
today. Several rows are capabilities that exist but are not doing anything:
`known_flaky_tests` is empty by default, so flake retry has nothing to act
on; the verdict format is enforced but nothing requires a HOLD to carry a
finding; the scorecards run but there is no durable per-PR, per-token
transition record, which is why L1 exists. No metric in this plan is called
running until its source, retention, denominator, and completeness have been
shown.

Consequences for this plan:

- Two vocabulary layers, both kept. Revision 1's seven states (`PASS`,
  `BLOCKED`, `STALE`, `INFRASTRUCTURE_WAIT`, `UNSAFE_TO_REUSE`,
  `HUMAN_REQUIRED`, `UNKNOWN`) are the outcome every surface reports: small,
  stable, and what an agent or a person acts on. Main's `blocked_on` tokens
  are the reason beneath it. Neither replaces the other. Main's set has no
  single outcome type today (about 40 tokens, three failure vocabularies,
  several separate unknown states), and at least one token misleads:
  `excluded_label:superseded` tells an agent the owner must act when no
  human is needed. See L5.
- The benchmark is not a new corpus. It is the existing scorecard plus one
  missing table: time lost per blocked reason.
- The first job is promotion and arbitration of recoveries that already exist,
  not designing a new one.

## Findings so far

Each line is marked **verified** (checked against the source by a second
means), **reported** (one lane's result, not re-checked), or **estimate**.

- **Verified.** The false HOLDs on Vellum PR 385 came from the Codex reviewer
  being given Claude tool names; it read nothing and its HOLD was posted. Two
  sessions found this independently.
- **Verified.** `post-tag-sync` failed 45 of its last 50 runs; `CHANGELOG.md`
  last changed 2026-10-31. 44 of the last 100 releases are drafts.
- **Verified.** The skill-sync gate refuses a change that only adds files
  under `planning/`.
- **Verified.** The served model appears in the Claude reviewer's output
  stream (requested `haiku` and `sonnet`, received `claude-haiku-5-5` and
  `claude-sonnet-5-5`). Not verified: that a mismatch is refused, the Codex
  path, and independence from the proxy.
- **Reported, baseline.** Four days of complete steward rows (2026-11-08 to
  11-12, 7 repositories, 548 PRs): 22,661 rows collapse to 3,286 episodes.
  1,032 are actionable, 1,916 are normal waiting, 279 are on PRs closed
  unmerged, 31 drafts, 28 human stops. Fifty-eight percent of episodes are
  not loss at all.
- **Reported, baseline.** Ranked by work, failed required checks lead: 550
  actionable episodes and 503 fix pushes or `repaired:` and `dispute:`
  comments. Conflicts are second with 167 episodes and 236. By elapsed time
  conflicts still look first, but 65 percent of those hours sit in four
  repositories that merge by hand, where nobody was waiting.
- **Reported, baseline.** Agent-free resolution rate is between 30 and 45
  percent of 1,025 resolved actionable episodes. The range is wide because
  authors, the repair lane and reviewers all comment as the same App
  identity, so a comment cannot be attributed to a role.
- **Reported, baseline.** 12 false-hold candidates among 190 distinct held
  heads; nine say the reviewer could not read its workspace. Owner stops
  raised: 4.6 per 100 PRs.
- **Verified by the lane on m3.** The complete per-PR rows, including the
  next actor, exist only as raw steward output that the tick script deletes
  after three days. Nothing durable holds them. Wasted compute cannot be
  attributed to a PR: the metrics database holds project-level Pulp data
  with PR identity on 4 percent of rows.
- **Reported.** 144 conflict episodes: 18 percent purely mechanical, 54
  percent purely hand-written, 135 cleared by one merge of the base, 20
  percent on PRs later closed unmerged.
- **Reported.** Admission and approval code took 124 of the fix commits
  since 2026-10-15 (34 percent). Reviewer claims and receipts have blocked
  nothing. 71 percent of ordinary PRs edit a `SKILL.md`.
- **Reported.** 59 queue ejections in one week; an ejected PR took a median
  231 minutes to merge against 31 (`docs/automated-draining.md`).
- **Estimate, known to be biased.** The steward summary log (2026-10-30 to
  2026-11-12) lists only the three oldest blocked PRs per tick and measures
  elapsed time. It put conflicts first by a wide margin. The conflict
  classification shows that overstated them. This log is not used to rank
  anything; the ledger replaces it.
- **Corrected.** Earlier drafts said lint follow-up commits cost CI rounds
  (the pre-push hook already blocks on them, so unproven), proposed
  replacing the carried-commit guard with a tree rule (it would miss the
  real incident), and proposed inferring whether a HOLD was genuine from its
  content (rejected in review three times).

## Constraints

Revision 1's constraints stand:

- Bind evidence and decisions to repository, PR, base SHA, exact head SHA, and
  policy revision.
- Fail closed on missing, stale, malformed, or contradictory required
  evidence. Unknown stays unknown.
- Model output never authorizes a merge, bypasses a required check, or
  redefines policy.
- No duplicate scheduling, execution, retry, caching, policy, or model-routing
  authority. TartCI owns VMs and leases. Subrouter owns provider routing and
  failover. GitHub owns PR, check, and merge state. The steward is the one
  admission writer.
- Untrusted PR content, logs, and comments are input to a proposal, never
  authority.
- Platform and provider evidence stay separate.
- Bounded retries, deadlines, and budgets; durable receipts; explicit terminal
  outcomes.

Added in this revision:

- **One recovery owner per PR head.** Several recovery paths now act
  independently (infra rerun, transient rerun, flake retry, dwell acts,
  environment requeue, repair lane). An arbiter picks at most one per head per
  pass and records why the others stood down.
- **Complexity budget.** A change that adds a guard, mode, or config key names
  what it removes or why nothing can be removed. A guard that has needed more
  than five fix commits in a month is a candidate for redesign, not another
  patch.
- **Every recovery carries its own measure.** No recovery ships without the
  metric it moves, a planted negative, a kill switch, and a rollback rule.
- **Read budget is a first-class cost.** Any new reader states its GitHub
  reads per pass and reuses the shared per-head snapshot.
- **A rule in a brief is not a gate.** A behavior the fleet depends on is
  enforced in code or a required check, not in agent instructions.
- **Help, not block.** A block is valid only when it names a defect in the
  change, an owner, and the condition that clears it. A failure of Shipyard's
  own machinery (a reviewer that could not read its workspace, an unread
  message, an expired claim, a steward error) is never a verdict on the
  change. It is an infrastructure fault in the review lane: retried
  elsewhere, counted as self-inflicted loss, and never left for the author or
  Daniel to argue away.

## Promotion ladder

Every recovery moves through the same four modes. The config value names the
mode, and the loop reports which mode each recovery is in.

| Mode | Meaning | Leaves when |
| --- | --- | --- |
| `off` | No evaluation | A proposal with evidence is accepted |
| `shadow` | Decides and records what it would do; no side effect | At least 20 decisions over at least 7 days, each scored against what happened; precision at or above the bar; zero unsafe decisions; planted negative refused |
| `canary` | Enforced on one repository (Shipyard first) with a per-pass cap | 7 days with no safety counter raised and the target metric moving the right way |
| `enforce` | On for every repository in `fleet-repos.txt` | Returns to `shadow` on a safety regression or two flat measurements |

Evidence must be representative: decisions from at least three repositories,
covering both merge-queue and direct-merge bases, with the planted negatives
included in the scored set. Twenty decisions from one repository on one day do
not qualify.

Owner direction (2026-11-12): the fleet works autonomously, including
expanding its own policy authority, and involves Daniel only in rare
exceptions. So no step on the ladder waits for him. Human sign-off is
replaced by stronger machine evidence, in three tiers:

| Change | Lands on |
| --- | --- |
| Promotion or demotion of a reversible act | Normal reviewer approval, scorecard rows in the PR body |
| Promotion of an irreversible or content-bearing act to `enforce` | Two approvals from different signers on different providers, one in the adversary role; a canary of at least 14 days; the planted negative refused live during canary |
| Expanded policy authority: a new act type, a wider allowlist, a new repository class, a lower bar or threshold | The same two-approval rule, plus the ledger rows that justify it in the PR body, entry at `shadow` (never straight to `enforce`), and a line in the weekly report so Daniel sees it after the fact |

Every such change is reversible by one config value and is announced in the
weekly report. Nothing asks first.

The floor is the short list that stays with Daniel, because it is what makes
the rest safe to automate and a system should not be able to remove its own
brakes:

- secrets, logins, spending, sudo, signing identity, GitHub rulesets and
  settings;
- removing or weakening the circuit breaker, the planted-negative
  requirement, or the kill switches;
- changing who counts as an independent reviewer, or letting an author's
  session approve its own work;
- relaxing the false-approval constraint or the fail-closed rule for unknown
  evidence.

A check labels a PR `owner-decision` only when it touches the floor. The
promotion PR's diff is otherwise limited to the mode value.

Precision bars: 0.95 for an act that re-runs or re-dispatches; 1.0 over the
scored window for an act that closes, merges, approves, pushes, or removes a
queue entry. An "unsafe decision" is one that would have merged unapproved
content, closed live work, or discarded evidence.

Scoring rules:

- A decision is scored against the snapshot recorded when it was made, which
  is immutable. The later outcome confirms or refutes it.
- A shadow decision is not confirmed by an outcome that someone else
  produced. If a human or another automation performed the act, the row is
  scored as confounded, not correct.
- Unknown and confounded rows stay in the denominator. They never make
  promotion easier.
- Twenty decisions is a floor for reversible acts, not a proof. The scorecard
  reports a confidence bound for each act type, stratified by repository,
  base kind, and provider, and promotion requires the lower bound to clear
  the bar.
- Shadow does not exercise contention, API failure, or the consequences of
  acting. That is what canary is for: canary runs with a per-pass cap of one
  and its own planted negative, and a canary result is never inferred from
  shadow.

Reversible and irreversible acts are treated differently:

- **Reversible** (re-run, re-dispatch, reorder, retarget, re-review): the
  ladder applies as written and promotion is routine.
- **Irreversible or content-bearing** (close a PR, remove a queue entry,
  merge, push to a branch, approve): shadow and canary by the ladder, with
  the stronger evidence in the table above for the step to `enforce`. Rare
  catastrophic errors do not show up in a short window, so these get the
  longer canary, the two-provider review, and a permanent per-pass cap that
  is raised only by the same rule.

Demotion is independent of promotion. Each acting recovery has a runtime
circuit breaker inside the steward: a safety counter above zero, or a
planted negative that was not refused, sets the recovery to `shadow` at the
next pass with no PR and no involvement from the loop that promoted it. The
loop may propose promotions; it cannot be the only thing able to stop one.

A promotion check verifies behavior, not paths: it compares the effective
policy (resolved config, defaults, and mode lookup) at the PR head against
its parent and fails unless the only difference is the one mode value. A
change to a default, a parser alias, or a lookup that alters behavior without
touching the mode line fails the check.

A proposal for any new acting recovery states, before code: its historical
frequency and cost from the ledger; preconditions and the typed evidence it
requires; the actions it may take and under whose authority; retry, deadline
and resource bounds; idempotency and concurrency protection; circuit breaker
and quarantine behavior; the receipt it writes; terminal states and
escalation conditions; the expected reduction in interventions; and its
negative tests with a planted negative.

Current standing against the ladder:

| Recovery | Now | Next step |
| --- | --- | --- |
| Infra rerun | `enforce` | Add to loop verification |
| Superseded close | `shadow`, close not built | Build close; score; canary |
| Dwell acts | `shadow` | Score each act type separately; canary the two re-run acts first |
| Version-only review | `shadow` | Score; canary on Shipyard |
| Base-poison pause | `dry-run` | Stays shadow: replay showed the verdict was rarely right. Fix the detector before any promotion |
| Environment requeue, queue-absent recovery | opt-in off | Enter shadow with scoring |
| Conflict repair | does not exist | See C1 |

## Confidence

What this plan rests on, and how each weak point is being hardened.

| Claim | Confidence | Hardening |
| --- | --- | --- |
| The main cost is Shipyard's own machinery, not CI failures | High: steward log, audit, and independent challenge agree | Ledger confirms or refutes with episode cost |
| Measure before building | High: evidence already overturned two proposals | Kept as the gate on every backlog item |
| The three-rule false-HOLD design | Medium: fourth design round | Cross-provider review to a clean verdict; a live run of both providers through preflight and the token check with nothing posted; a fixture test of Shipyard's gate showing the status stays pending |
| Order of the backlog | Medium: one four-day baseline, with compute and role attribution missing | Core items reordered from it. Nothing outside the core starts on it. Recomputed weekly once the ledger writes durable rows |
| The promotion ladder's thresholds | Medium: reasoned, not tested | First used on superseded close, a reversible act on Shipyard only; thresholds revised from that run before any irreversible act uses them |
| Prior-art descriptions | Low: from general knowledge and the first draft | Each adopted row checked against upstream before work starts |
| The vocabulary: seven outcomes over main's reason tokens | Medium: the mapping is asserted, not yet written or tested | L5: a total mapping with a test that fails on any unmapped token, and a check of every `next_action` actor against what actually cleared that state in the ledger |
| Lane reports marked "reported" | Medium | Spot-checked by a second means before a decision rests on one |

A claim does not move a work item out of the backlog until it is verified.

## Measures

Baselines come from the first L2 run; the numbers below are the current
estimates and the direction required.

| Measure | Now (estimate) | Direction |
| --- | --- | --- |
| Agent-free resolution rate (blocked episodes cleared by steward alone) | not measured | Reported, then up every month |
| Self-inflicted blocked hours (HOLD with no defect, reviewer fault, steward error, unread handoff) | not measured | Reported, then zero |
| Dispute to resolution, median | about 2.5 h (one case) | Under 15 min |
| Lessons without a gate | not measured | Reported, then zero |
| Unsafe evidence reuse | not measured | Hard constraint, zero |
| Unnecessary rebuilds of reusable evidence | not measured | Reported, then down |
| Messages to Daniel: share that were consequential | not measured | Reported, then up; repeats to zero |
| Orphans past their expiry, by object type | not measured | Reported, then zero |
| Runner wait per job by stage (lease, boot, register) | not measured | Reported, then down |
| Fresh-install canary: unattended to first verified merge | not run | Passes weekly |
| Actionable episodes and their cost, by blocked reason | not measured | Reported; top class down by half |
| Conflict episodes needing an agent session | not measured (111 PRs seen conflicting in two weeks) | Down after C2; reported per conflicting path |
| False holds | not measured (2 known on 2026-11-12) | Reported, then zero |
| Wasted and rework CI compute minutes per merged PR | scorecard, coverage partial | Down |
| Review rounds per PR | not measured | Reported, then down |
| Ejections per week | 59 | Down after C5 |
| Superseded PRs open over 4 h | 11 of 15 | Zero |
| Steward tick p90 | 156 s | Under 60 s |
| GitHub reads per merged PR | not measured | Reported, then down |
| Owner stops per 100 PRs | not measured | Reported, then down |
| Recoveries in `shadow` longer than 30 days without a decision | not measured | Zero |
| False approvals, unapproved content on base | 1 known (2026-11-01) | Zero |
| Share of blocked time classified unknown | not measured | Reported honestly |

A gain in any row never offsets a regression in the false-approval row.
Elapsed-time figures appear in reports as context only.

## Safety

- The autonomy contract's adversarial cases come first: restart mid-act,
  fencing, duplicate effects, provider disagreement, and a compromised or
  failing reviewer each have an integrated test before any recovery reaches
  canary. Every automatic act names its owner, its fail-safe, and its
  quarantine state.

- Every acting recovery has a planted-negative test in CI and one live
  planted negative during canary.
- Kill switches are machine-global and read at each pass.
- Acts are intent-logged before the side effect and fenced by exact head.
- The loop's own PRs go through the same review and admission as any other.
  A promotion PR is never in a fast-path class.
- Autonomy-contract blockers F1 to F8 still gate "autonomous mode" as that
  document defines it. This plan promotes individual bounded recoveries; it
  does not declare autonomous mode.

## Permission defaults

Proposed narrow defaults for the four open permissions. Each stays off until
Daniel accepts it; each is reversible by one config value.

| Permission | Proposed default | Safeguards |
| --- | --- | --- |
| Webhook registration | Grant "Repository hooks: read and write" to the Shipyard App on the five repositories already named in `docs/webhook-registration-ask.md`, nothing wider | Events only trigger a re-read of GitHub state and are never evidence themselves; signature verified; polling stays as the healer; removing the hook returns to today's behavior |
| Promotions and expanded authority without sign-off | Allowed, by the three tiers in "Promotion ladder" | Two-provider approval and longer canary for irreversible acts and for expanded authority; entry at `shadow`; independent circuit breaker; reported after the fact; only the floor is `owner-decision` |
| Bot pushes to PR branches | Off. When C3 starts: Shipyard repository first, allowlist empty until C1 names files | The C3 boundary above: generator-reproducible files only, exact-head fence, new head fully validated, approvals not carried, one attempt per head, kill switch |
| Owner-step automation | Allowed for any routine step outside the floor, taken in the order O1 ranks them | Never secrets, rulesets, sudo, FileVault, spending, or signing identity; each mechanism is scoped to one command, logged, reversible, and reported after the fact |

## Open decisions

1. Accept or amend the four defaults above.

Everything else proceeds without Daniel.
