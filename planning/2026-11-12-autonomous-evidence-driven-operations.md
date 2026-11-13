# Shipyard: Autonomous, Evidence-Driven Operations

Status: plan, revision 3 (2026-11-12). Approved to start: the ledger and
baseline (L), the read-only audits, conflict classification (C1 only), and the
false-HOLD fix (R6 to R8). Everything else waits for a reliable baseline.
Every behavior change ships as its own reviewed PR, default-off or shadow, and
is promoted only by the rule in "Promotion ladder".

Revision 1 was written against v0.97.0 (August). Main has since shipped most of
what it proposed to study. This revision is written against main at `e4f025b2`
and replaces "study, then recommend one recovery" with a standing improvement
loop fed by the data the fleet already produces.

The team brief that carries this out is
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
episode count once. An episode's cost is the work it caused:

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

Outcome measures tracked together, so that none is traded for another:

- agent-free resolution rate: actionable episodes cleared by the steward
  alone;
- false approvals: unapproved or defective content admitted. Hard constraint,
  zero;
- false holds: blocks later shown to name no real defect (upheld disputes,
  defect-free HOLDs, reviewer faults, wrong infra or poison verdicts);
- unnecessary work: wasted compute, duplicate reviews, repeated retries;
- human interventions per 100 PRs.

## Existing implementation pointers

The plan builds on these implementation pointers, not on unverified enforcement
claims. The entries below identify code and documentation to inspect; they do
not attach a current receipt or test result. Until that evidence is recorded,
the mode remains `Verification required`.

| Revision 1 proposed | Implementation pointer on main | Evidence status |
| --- | --- | --- |
| Typed classification set | `blocked_on` tokens with `since`, and `next_action {actor, action, stop}`, one per open PR per steward pass (`src/app/merge_steward_cmd/blocked_on.rs`, `next_action.rs`, `docs/automated-draining.md`) | Verification required |
| "Recommend one recovery loop" | Infra rerun for required checks (`infra_rerun.rs`) | Verification required |
| | Known-flake and not-head-caused same-head retry | On with a merge queue; `known_flaky_tests` empty by default |
| | Dwell-time acts: stale verdict re-run, `required_never_ran` dispatch, reorder, retarget (`[auto_merge.dwell]`) | Shadow |
| | Superseded detection (`docs/superseded-close.md`) | Shadow; closing not built |
| | Base-poison hold, fast-track, pause (`[base_health]`) | Pause dry-run, `auto_jump` off |
| | Environment requeue, queue-absent recovery | Opt-in, off by default |
| | Runner watchdog | Report-only without `--fix` |
| Failure classification | `ci.cause/v1` cause records, `FailureClass` in `src/classify.rs`, `src/diagnose/classify.rs`: three vocabularies | Verification required |
| Historical benchmark | `shipyard metrics scorecard`, `gate_cost` weekly scorecard, hourly baseline collector joining GitHub timing and TartCI exports, merge-receipt contract | Verification required |
| Review usefulness contract | `verdict: APPROVE head:<sha>` / HOLD, `review-verdict` status, reviewer claims and receipts, repair budget | Verification required |
| Low-risk fast path | `tools/fleet-pass/version_only.py` | Shadow, decides after 10 agreements |
| Risk tier | `docs/risk-tier.md` | Report-only; does not route reviewers or models |
| Model escalation receipt | fleet-pass logs the requested model only | Absent |
| Autonomy rollout | `docs/autonomy-contract.md` (A1 to A12, F1 to F8), `planning/autonomy-recovery-rollout-20261027.md` | Contract and evidence in progress, default-off |

Consequences for this plan:

- No new classification enum. Revision 1's seven states become a documented
  projection over `blocked_on` and `next_action`, not a second vocabulary.
- The benchmark is not a new corpus. It is the existing scorecard plus one
  missing table: time lost per blocked reason.
- The first job is promotion and arbitration of recoveries that already exist,
  not designing a new one.

## Evidence: where time goes

These numbers are elapsed time from a truncated log. They show where to look
first and are not the measure the loop optimizes; the baseline from L1 and L2
replaces them with episode cost.

Source: the m3 steward tick summary, 2026-10-30 to 2026-11-12, 9,532 rows
across eight repositories. Caveat: the summary lists only the three oldest
blocked PRs per repository per tick, so counts favor long waits and hours are a
floor for short classes and inflated by abandoned PRs for long ones. The
per-PR `blocked_on` rows in the tick JSON are complete; work item L1 replaces
this estimate with them.

| Blocked reason | PRs | Median longest wait | Over 4 h | Hours (sum of longest wait) |
| --- | --- | --- | --- | --- |
| `merge_state:DIRTY` (conflicts with base) | 111 | 301 min | 67 | 2,759 |
| `unmanaged` | 62 | 54 min | 3 | 472 |
| `required_failed:review-verdict` (a HOLD) | 182 | 36 min | 7 | 405 |
| `ejected_same_head:failed_checks` | 25 | 186 min | 9 | 121 |
| `merge_state:UNSTABLE` | 49 | 45 min | 6 | 71 |
| `owner_decision` | 16 | 745 min | 11 | not summed |
| `excluded_label:superseded` | 15 | 613 min | 11 | not summed |
| `required_never_ran:review-verdict` | 12 | 214 min | 5 | not summed |

Steward tick duration: median 29 s, p90 156 s, p99 401 s, maximum 1,017 s.

Other measured costs:

- **Ejections.** 59 in one week; an ejected PR took a median 231 minutes to
  merge against 31 for a first-time merge (`docs/automated-draining.md`).
- **Carried-commit and stacked-PR guard.** 43 commits in four weeks refine the
  same guard; five dated incidents; one PR blocked over nine hours
  (fleet-handbook `lessons.md`).
- **Shared GitHub read budget.** Drained several times on 2026-11-09 by
  long `shipyard wait` polls; 617 steward evaluations deferred on 2026-11-08.
  The steward and fleet-pass poll. Webhook registration is an open owner ask
  (`docs/webhook-registration-ask.md`).
- **Chore volume.** 131 of 965 commits since 2026-10-15 are
  "assign versions at land"; about 12 are skill-sync exception records.
- **Review liveness.** Reviewing and landing depended on one interactive
  session on m3 (fleet-handbook, "Today versus target"). Pulp final commit to
  enqueue: median 79 minutes, p90 6.8 hours, against 28 minutes enqueue to
  merge.
- **Silent drift.** A pinned steward binary missed a merged fix; an
  `auto_apply = false` left hosts eight releases behind for two weeks with no
  alert.

Not established, to be measured before acting on it: whether the 34
fmt/clippy/lint follow-up commits cost CI rounds. The pre-push hook already
blocks on `cargo fmt` and `cargo clippy`, so they may be local fixes.

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

A promotion or demotion along the ladder is routine: it merges on normal
reviewer approval with the scorecard rows in the PR body, with no owner
sign-off. A change that expands policy authority is not a promotion and is
labeled `owner-decision`. That covers a new act type, a wider allowlist, a
lower precision bar or evidence threshold, a new repository class, a new
credential or permission for an actor, and any change to who may approve or
merge. The promotion PR's diff is limited to the mode value; a check refuses
a promotion PR that touches anything else.

Precision bars: 0.95 for an act that re-runs or re-dispatches; 1.0 over the
scored window for an act that closes, merges, approves, pushes, or removes a
queue entry. An "unsafe decision" is one that would have merged unapproved
content, closed live work, or discarded evidence.

Scoring is automatic: a shadow decision is correct when the later observed
outcome matches it (the PR was in fact superseded, the rerun did pass on the
same head, the check did need dispatch). Decisions with no observable outcome
count as unknown and do not count toward the 20.

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

## Work items

Ordered by measured loss. Each is sized to land as small PRs. Identifiers are
used by the team brief.

### L: the loop

- **L1 Blocked-time ledger.** Persist one row per PR per `blocked_on`
  transition (repo, PR, head, token, since, until, cleared-by actor) from the
  steward's existing per-PR rows. Keep it in the existing metrics store. This
  replaces the top-three summary as the source for every number above.
- **L2 Loss report.** `shipyard metrics loss --since 7d --json`: hours and PRs
  per token per repository, share cleared by steward, repair lane, author, or
  owner, with coverage. Add `interventions_per_100_prs` and
  `owner_stops_per_100_prs`.
- **L3 Recovery scorecard.** For each recovery: mode, decisions, correct,
  unknown, unsafe, and the target metric before and after.
- **L4 Scheduled loop.** A daily job that runs L2 and L3, files or updates one
  issue per top loss class, and opens promotion or demotion PRs per the
  ladder. Starts as report-only.
- **L5 Projection.** Document the mapping from `blocked_on` and `next_action`
  to the reader-facing states (`PASS`, `BLOCKED`, `STALE`,
  `INFRASTRUCTURE_WAIT`, `HUMAN_REQUIRED`, `UNKNOWN`) and expose it as one
  field. No new state machine.

### C: conflicts, the largest loss

- **C1 Classify conflicts.** For every `merge_state:DIRTY` episode in the
  ledger, record the conflicting paths. Hypothesis from the handbook: a few
  generated or counter files cause most of it (`ctest_script_inputs.json` was
  edited in 75 commits in seven days; a version counter collided at the same
  value on both sides). The classification decides C2 to C4.
- **C2 Prevent.** Remove the hot files from PR diffs: generate them at land,
  as version-at-land already does for versions, or make them append-only with
  a union merge driver.
- **C3 Mechanical repair.** Its own authority boundary, separate from every
  other recovery, because it writes to a PR branch. Not started until C1
  reports. Scope when it starts:
  - only conflicts confined entirely to an allowlist of files that a named,
    deterministic generator reproduces from the rest of the tree; the
    allowlist lives on the protected base and widening it is an
    `owner-decision`;
  - merge the base in an isolated worktree (never rebase), run the
    generator, and require that the result equals a clean regeneration and
    that no file outside the allowlist differs from the plain merge;
  - push with the expected-head fence, so a head that moved is left alone;
  - the pushed commit is a new head: prior approvals do not carry unless the
    existing patch-id rule carries them, and every required check runs in
    full on it;
  - a distinct actor identity and receipt, one attempt per head, a per-pass
    cap, its own kill switch;
  - no model in the path, and no resolution of any hand-written line.
- **C4 Prompt handoff.** For real conflicts: route to the repair lane at the
  first observation with the conflicting paths attached, instead of after
  hours. A PR whose author session is gone and whose content is on main goes
  to superseded detection.
- **C5 Predict.** Before admission, test-merge the candidate against the base
  plus the queue ahead of it and hold a certain failure out of the queue.
  This also addresses `ejected_same_head`.

### R: review cost

- **R1 Review liveness as a monitored invariant.** Reviewer and repair passes
  run headless on a timer; a tick-start invariant reports the age of the last
  completed reviewer pass per repository; no approval path depends on an
  interactive session.
- **R2 Risk tier routes the review.** Make the tier an input to fleet-pass:
  low tier to the smallest model, high tier and release paths to the
  adversary role. The tier is computed from the protected base, never from PR
  content.
- **R3 Fast path for mechanical diffs.** Promote version-only review through
  the ladder. Candidates for the same treatment, each with its own shadow
  record: changelog regeneration, skill-sync exception records, lockfile-only
  changes produced by a named workflow.
- **R4 HOLD quality.** Extend the verdict comment with a machine-readable
  block: findings each with a stable id, the file, and the test that would
  prove the fix. The repair lane closes findings by id. A second HOLD on the
  same finding id escalates instead of looping. Measure rounds per PR.
- **R6 A HOLD must name a defect.** The verdict gate accepts a HOLD only with
  at least one finding (R4). A reviewer that could not review emits
  `UNVERIFIED` with the reason; `UNVERIFIED` leaves `review-verdict` pending,
  re-dispatches the review to a different reviewer slot or provider at once,
  and raises a reviewer-health fault. Existing defect-free HOLDs at a head
  are treated the same way. Evidence: on 2026-11-12 Vellum PR 385 and PR 381
  were each held by a reviewer whose comment said it could not read the
  review workspace and named no defect; the earlier dispute took about
  2.5 hours, the same fault had a fix recorded the same day, and the report
  to the m3 orchestrator went to a surface no agent was reading.
- **R7 Reviewer preflight.** Before a reviewer slot takes a claim it proves
  it can read the workspace and the diff. A slot that fails preflight takes
  no claims and shows in the R1 invariant. A reviewer never learns it is
  broken by failing a real review.
- **R8 Disputes resolve mechanically.** A dispute is a re-review of the same
  head by a different reviewer, dispatched on the next pass with a deadline
  measured in minutes. It does not wait for a claim to expire or for a
  message to be read. No recovery path depends on messaging an interactive
  session.
- **R9 Fewer review parts.** Count the mechanisms between "checks green" and
  "admitted" (signers, session stamps, claims, receipts, carried approvals,
  release-path review, code owners, disputes). For each: the incident it
  prevents and the hours it has cost in the ledger. Remove or merge any
  whose cost exceeds its protection, keeping the property that unapproved
  content never reaches a base.
- **R5 Approvability preview.** `shipyard pr preview <n>`: the read-only
  answer to "what blocks this and who acts next", assembled from `blocked_on`,
  `next_action`, required checks, and the review state. Advisory.

### B: read budget and latency

- **B1 Webhook permission.** Owner ask already written. With it, the steward
  and fleet-pass consume daemon events and poll only to heal.
- **B2 One snapshot per head.** `wait`, `pr-watch`, fleet-pass, and the
  steward read the same cached per-head snapshot. Each reader reports its
  reads; the loop reports reads per merged PR.
- **B3 Tick latency.** Target p90 under 60 s. Attribute the 156 s p90 and the
  1,017 s maximum to named causes before changing anything.

### S: simplification

- **S1 Recovery arbiter.** One function decides which recovery, if any, owns a
  head this pass. It becomes the only caller of the recovery paths.
- **S2 Carried-commit guard redesign.** State the property the guard protects
  (no unapproved content reaches the base) and evaluate enforcing it at the
  tree level: the diff that lands equals the diff that was approved. Compare
  against the current ancestry rules on the five recorded incidents and the
  named residuals. Decide whether to replace or keep.
- **S3 One failure vocabulary.** Map `FailureClass` and
  `diagnose/classify.rs` onto `ci.cause/v1`; fix the `TIMEOUT` casing bug;
  retire the duplicates.
- **S4 Chore volume.** Batch version assignment per release rather than per
  land, or keep it out of branch history, and make skill-sync exceptions a
  trailer the gate reads, not a follow-up commit. Measure before choosing.
- **S5 Config and binary drift invariants.** Extend tick-start invariants:
  steward binary age against the latest release, `auto_apply` holds older
  than 48 hours, per-host opt-in switches that differ across hosts.

### O: owner steps

- **O1 Owner-stop ledger.** Every `next_action.actor = owner` and every
  handbook "tell Daniel" path becomes a ledger row with its reason. Rank by
  frequency and hours.
- **O2 Remove the top routine ones.** Likely candidates from the handbook:
  the manual macOS release step, LaunchAgent reloads for timer changes,
  copying the admission key to m5 and m5s. Each needs a narrow, logged,
  reversible mechanism. Secrets, rulesets, sudo, FileVault, and spending stay
  with Daniel.

### T: TartCI and Tart

TartCI stays the authority for VMs, runners, leases, capacity, and teardown.
Shipyard consumes its receipts and sends it demand; it never creates, reaps,
or leases on its own. The two repositories are planned and released as one
product surface.

- **T1 One timeline per job.** Join GitHub job, TartCI lease, VM, and host so
  every job's wait splits into queued at GitHub, lease wait, clone and boot,
  runner registration, execution, and teardown. The loss report gains a
  runner-wait class broken down by stage, host, and platform. Builds on the
  existing `tartci runtime export | shipyard metrics import tartci`.
- **T2 Typed infra evidence.** Infra rerun and cause records read TartCI's
  terminal receipt (boot failed, disk full, lease lost, runner deprecated) as
  the evidence for an infra classification, ahead of log matching. A Linux
  receipt never stands in for macOS.
- **T3 Demand signal.** Shipyard publishes what is about to need a runner:
  queue depth per lane, merge-group work first. TartCI decides what to warm
  (`docs/warm-vm.md` in TartCI). Measure clone-and-boot time saved.
- **T4 Host conditions as invariants.** Disk headroom, runner version against
  GitHub's deprecation window, golden image age, and Tart version become
  tick-start invariants that warn before they stop a lane. Each of these
  caused an outage in November.
- **T5 Shared contract tests.** A versioned schema for the runtime export and
  receipts, golden fixtures in both repositories, and one cross-repository
  canary that runs on a release of either.
- **T6 Track Tart upstream.** Tart and TartCI releases are reviewed on a
  schedule for capabilities worth adopting (suspend and snapshot, image
  caching, network isolation).

### X: mistakes and orphans

- **X1 Lesson to gate.** Every entry in fleet-handbook `lessons.md` and every
  friction report gets a row: the mistake, the gate or detector that now
  prevents it, and the test that plants it. A lesson with no gate is open
  work, and the loop reports the count. A rule that lives only in an agent
  brief does not close a lesson.
- **X2 Mistake recurrence.** The loop counts recurrences of each known
  mistake class from evidence (a force-push to a reviewed branch, a self
  approval, a long poll, a wait on a superseded PR, a script edited in
  place, a pinned binary behind main). A recurrence after its gate shipped is
  a regression issue.
- **X3 Orphan census.** One report lists every durable object type with
  count, age, and owner:

  | Object | Owner signal | Disposition |
  | --- | --- | --- |
  | Open PR | owner registry, author session alive | adopt by repair lane, or superseded close |
  | Branch without a PR | last commit age, merged content | delete when content is on base; else report |
  | Worktree | branch state, session | `git worktree remove` when merged and clean |
  | Draft release | tag age | release-heal; alert past 24 h |
  | Ship-state and claim files | PR state, lease | existing compare-and-delete cleanup |
  | Queued run with no runner | TartCI demand | cancel when superseded; else demand signal |
  | VM, overlay, runner registration, lease | TartCI ownership | `tartci doctor --reap` under TartCI's rules |
  | Recovery in `shadow` | age, decisions scored | promote, fix, or remove within 30 days |
  | `PARKED/*` and `owner-decision` items | age, next step named | re-check on schedule; report stale ones |

  Shipyard acts only on the objects it owns. The rest go to the owning tool.
- **X4 Finish or remove.** A default-off feature without a promotion decision
  after 30 days appears in the census. The answer is promote, fix, or delete
  the code. Main carries several today: `fleet_selfheal` (decide-only, no
  caller), `auto_rescue` (classifier only), parallel proof (shadow), route
  shadow, changed-surface selection (shadow).
- **X5 Refusals that teach.** Every guard refusal prints the one command that
  fixes it and is counted. A guard that refuses the same mistake often is
  evidence the tool should do the right thing by default.

### P: usable by others

- **P1 Fresh-install canary.** Weekly, a scratch repository on one Mac goes
  from install to a verified merge unattended, with TartCI providing the
  runner. Any step that needs fleet-specific knowledge is a defect.
- **P2 Product defaults apart from fleet policy.** Repository names, host
  names, reviewer session globs, and Pulp-specific policies move out of code
  into configuration, with defaults that are safe on a one-Mac install.

### M: model use

- **M1 Served-model receipt.** Establish whether Subrouter returns the served
  model and reasoning level. If yes, fleet-pass records requested and served
  model on every verdict stamp and refuses a mismatch. If no, record
  `VERIFICATION_UNAVAILABLE` and stop. This is a field on the existing
  reviewer receipt, not a wrapper. The probe must show that it prevents
  misattribution, not only that a field exists:
  - the served model comes from the provider's response metadata, not from
    the request echoed back or from the model describing itself;
  - a planted mismatch (alias requested, different model served, including a
    provider failover and a fallback path) produces `MODEL_MISMATCH` and no
    verdict stamp;
  - a verdict with no receipt is attributed to no model.
  If the metadata is produced by the same layer that could misroute, the
  probe reports that the receipt is not independent evidence.
- **M2 Cost per verdict.** Tokens and wall time per review and per repair, by
  tier, from Subrouter usage. Feeds R2.
- **M3 Diagnosis proposals.** Only for loss classes the loop marks as having
  no recovery and no named cause: a model reads the evidence rows and writes
  a proposal into the issue. It acts on nothing.

Deferred until the loop shows a need: a general escalation entry point,
operating modes, policy inheritance, hosted dashboards, Slack or S3 delivery.

## Prior art

Ideas taken from other CI and merge systems, each mapped to a work item. The
descriptions are from general knowledge of those systems; whoever picks up an
item checks the current upstream behavior first. A scheduled prior-art review
(the fleet's `check-for-inspiration` routine) feeds new candidates into the
loop as issues, with the same `ADOPT`, `CONSTRAIN`, `DEFER`, `REJECT` decision.

| Idea | Seen in | Treatment |
| --- | --- | --- |
| Speculative merge testing with batch bisection on failure | Zuul, bors, GitLab merge trains, Mergify, Trunk, Aviator | CONSTRAIN. GitHub's queue already batches; use bisection results to attribute an ejection to one head (C5, ejection retry) |
| Predict conflicts and failures before admission | Uber SubmitQueue, Trunk | ADOPT as C5 |
| Exonerate a failure by re-running it on the base | Chromium CQ "without patch" | ADOPT. Strengthens infra and flake attribution with positive evidence (T2, known-flake retry) |
| Flaky test detection from history, quarantine with an owner and expiry | Buildkite Test Engine, Trunk Flaky Tests, BuildPulse | ADOPT. Populate `known_flaky_tests` from cause records, each entry with an expiry and an issue (X3 covers expiry) |
| Culprit finding and automatic revert when the base goes red | Chromium Findit, Google TAP | CONSTRAIN. Base-poison detection must reach its precision bar first; then propose the revert PR, never push it |
| Test impact selection from changed paths | Bazel, Nx, Develocity, Launchable | Already in shadow (changed-surface). Score and promote through the ladder |
| Content-addressed reuse of results | Bazel remote cache | Already present as evidence reuse; report the reuse rate and every invalidation reason |
| Typed prerequisites (`needs`, `waitsFor`, `Depends-On` across repositories) | Zuul, Macroscope | DEFER until S2 decides the stacking model; cross-repository dependencies matter for Pulp and Vellum |
| Approvability preview | Macroscope | ADOPT as R5 |
| Review benchmark from real history with planted negatives | MacroscopeBench | ADOPT. The ladder's scoring and planted negatives are this |
| Warm runner pools sized by demand | Actions Runner Controller, Namespace, Cirrus Runners | ADOPT as T3, decided by TartCI |
| Fleet orchestration for Tart VMs | Cirrus Orchard | Review under T6; TartCI keeps authority |
| Build provenance attestation | SLSA, GitHub artifact attestations | Present for releases; extend to the merge receipt only if a consumer needs it |
| Per-job timeline and wait breakdown | Buildkite, Datadog CI Visibility | ADOPT as T1 |
| Hosted dashboards, Slack and S3 delivery | most | DEFER. The loop's issues and the scorecard come first |

## Measures

Baselines come from the first L2 run; the numbers below are the current
estimates and the direction required.

| Measure | Now (estimate) | Direction |
| --- | --- | --- |
| Agent-free resolution rate (blocked episodes cleared by steward alone) | not measured | Reported, then up every month |
| Self-inflicted blocked hours (HOLD with no defect, reviewer fault, steward error, unread handoff) | not measured | Reported, then zero |
| Dispute to resolution, median | about 2.5 h (one case) | Under 15 min |
| Lessons without a gate | not measured | Reported, then zero |
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
| Promotions without sign-off | Allowed for mode changes along the ladder only | Diff limited to the mode value and enforced by a check; scorecard rows in the PR; normal reviewer approval; automatic demotion; anything that expands policy authority is `owner-decision` |
| Bot pushes to PR branches | Off. When C3 starts: Shipyard repository first, allowlist empty until C1 names files | The C3 boundary above: generator-reproducible files only, exact-head fence, new head fully validated, approvals not carried, one attempt per head, kill switch |
| Owner-step automation | None by default. O1 reports the ranked list first; each candidate is proposed singly | Never secrets, rulesets, sudo, FileVault, spending, or signing identity; each mechanism is scoped to one command, logged, and reversible |

## Open decisions

1. Accept or amend the four defaults above.

Everything else proceeds without Daniel.
