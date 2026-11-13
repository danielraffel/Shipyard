# Backlog: autonomous, evidence-driven operations

Detail for the work items named in
`planning/2026-11-12-autonomous-evidence-driven-operations.md`. That document
says what is in progress and why. Everything here outside the core is a
ranked candidate, not a commitment: an item starts only when the ledger shows
a repeated, attributable loss it would remove.

Statements below about what main does today (a mode, a default, a caller
that does or does not exist) come from a reading of code and documentation
on 2026-11-12. They are reported, not verified, and none describes a fleet
host's effective configuration. Whoever takes an item confirms its premise
first.

Findings from 2026-11-12 that change items below:

- **C (conflicts).** Classification is done. 18 percent of 144 conflict
  episodes are purely mechanical; 135 cleared by one merge of the base in a
  median of 51 minutes; Pulp already split its worst generated files. C3
  (repair by regeneration) is not justified by this evidence and stays
  unstarted. C2 remains for one file, TartCI `fleet/advertised-labels.json`.
  The hand-written hot files need a structural answer: in Shipyard,
  `src/app/merge_steward_cmd.rs` is in half of the conflicts.
- **S2 (carried-commit guard).** The tree-equality rule is withdrawn. It
  would have missed the one incident where held code reached a base, because
  the reviewer had approved a diff that already contained it. The item is
  now: narrow the guard by deleting the commit-count caps, probes and
  landed-elsewhere code, which caused 3 of 7 recorded incidents.
- **S4 (chores).** The cause is the skill-sync path map, which sends `src/`,
  `scripts/`, `docs/` and `planning/` to one 6,107-line `SKILL.md`. 71
  percent of ordinary PRs edit a `SKILL.md`, 35 percent need a skip trailer,
  and that file is the top conflict source. Version files caused none.
- **R9 (review machinery).** Reviewer claims and receipts show no observed
  blocks and their producer is not installed: freeze, then remove or finish.
  The per-head steward handoff blocked 62 PRs with no safety content.

## Work items

Ordered by measured loss. Each is sized to land as small PRs. Identifiers are
used by the team brief.

### L: the loop

- **L1 Blocked-episode ledger.** Persist one row per PR per `blocked_on`
  transition (repo, PR, head, token, since, until, cleared-by actor) from the
  steward's existing per-PR rows. Append-only, with a schema version, a
  stated retention, replay from the raw rows, and a completeness check that
  reports passes with no rows written. Keep it in the existing metrics
  store. This replaces the top-three summary as the source for every number
  above.
- **L2 Loss report.** `shipyard metrics loss --since 7d --json`: hours and PRs
  per token per repository, share cleared by steward, repair lane, author, or
  owner, with coverage. Add `interventions_per_100_prs` and
  `owner_stops_per_100_prs`.
- **L3 Recovery scorecard.** For each recovery: mode, decisions, correct,
  unknown, unsafe, and the target metric before and after.
- **L4 Scheduled loop.** A daily job that runs L2 and L3, files or updates one
  issue per top loss class, and opens promotion or demotion PRs per the
  ladder. Starts as report-only.
- **L5 One outcome vocabulary.** In the core. Every `blocked_on` token and
  every landing, wait and pr-watch state maps to exactly one of seven
  outcomes: `PASS`, `BLOCKED`, `STALE`, `INFRASTRUCTURE_WAIT`,
  `UNSAFE_TO_REUSE`, `HUMAN_REQUIRED`, `UNKNOWN`. The mapping is one table in
  code, exposed as one field on every surface, and covered by a test that
  fails when a token has no mapping. An unmapped or unreadable state is
  `UNKNOWN`, never `PASS`. `HUMAN_REQUIRED` is reserved for the floor in
  "Promotion ladder" and for real judgment calls; a token that says the
  owner must act is checked against what actually cleared that state in the
  ledger, and corrected when a machine or an agent cleared it (starting with
  `excluded_label:superseded`). The existing tokens stay as the reason
  detail. No new state machine.

### C: conflicts

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
    allowlist lives on the protected base and widening it follows the
    expanded-authority rule in "Promotion ladder";
  - merge the base in an isolated worktree (never rebase), run the
    generator, and require that the result equals a clean regeneration and
    that no file outside the allowlist differs from the plain merge;
  - push with the expected-head fence, so a head that moved is left alone;
  - the pushed commit is a new head: prior approvals do not carry unless the
    existing patch-id rule carries them, and every required check runs in
    full on it;
  - a distinct actor identity and receipt, one attempt per head, a per-pass
    cap, its own kill switch;
  - no model in the path, and no resolution of any hand-written line;
  - the generator, its inputs, and its toolchain are taken from the protected
    base, not from the PR; a PR that changes any of them is out of scope;
  - the generator runs twice in a hermetic environment (no network, fixed
    clock, clean checkout) and the outputs must be byte-identical;
  - an allowlisted file must match what the generator produces from the PR's
    own pre-conflict tree, which proves it carried no hand edit; if it does
    not match, the PR is out of scope;
  - approvals never carry onto a head this recovery created: a fresh review
    and every required check are needed.
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
  are treated the same way. `UNVERIFIED` fails closed: the status stays
  pending and is never success, it never reduces the approvals a head needs,
  re-dispatch is capped per head, and a head that exhausts the cap is
  quarantined to the operator role with its own reason. A PR that makes
  reviewers unable to read it is a suspect, not a beneficiary: when the
  reviewer's preflight fixture reads cleanly and the PR's workspace does
  not, that is a finding against the PR. Nothing the PR controls selects the
  fallback reviewer, provider, or model. Evidence: on 2026-11-12 Vellum PR 385 and PR 381
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

### H: history review

Carried from revision 1, which the first rewrite dropped. The ledger counts
what happened; it cannot say whether a decision was right or whether a human
was really needed. These two read-only studies supply that ground truth.

- **H1 Labeled case sample.** 30 to 50 real cases across merge-queue
  failures, stale or invalid evidence, runner and lease failures, repeated
  builds, incorrect reuse or invalidation, recovery loops, holds and
  approvals, duplicate reviews, avoidable operator requests, and
  notifications. Each case records the state snapshot available at decision
  time, the evidence, the expected classification, the safe next action, the
  terminal outcome, the source of the label, and whether human involvement
  was necessary. No hindsight: a label uses only what was knowable then.
  Unlabelable cases are `UNKNOWN` and the unknown rate is reported. This set
  is the planted-negative and scoring source for the ladder.
- **H2 Agent-session review.** Review past sessions as operational evidence,
  not as a model showcase. For each: did it understand the goal and its
  authority boundary; did it inspect current evidence before acting; did it
  take ownership of safe routine work; did it repeat findings or spend
  tokens to no effect; did it ask for a human action automation could
  perform; did it leave a durable next action and receipt; did it stop when
  evidence was unavailable or invent confidence. Output is changes to shared
  tooling, receipts, and review output. A class of mistake gets one
  deterministic contract, not a new instruction per symptom. Session
  identity is host, session, surface when available, and a durable
  transcript reference; raw cmux workspace or surface handles are not
  portable across hosts and are not identity.
- **H3 Attention quality.** Score what reached Daniel or an agent as
  consequential, redundant, or noise: owner relay messages, PR comments,
  operator findings, alerts. Report signal-to-noise and the repeated-message
  rate. Normal retries, successful recoveries, and unchanged state are not
  messages. The weekly report is the default channel; an interrupt needs a
  consequential transition, an unresolved failure, or an authorization that
  only he can give.
- **H4 Reuse correctness.** For evidence reuse and approval carry: every
  reuse and every invalidation is recorded with its reason, and the sample
  in H1 is checked for evidence that was reused when it should not have been
  and rebuilt when it need not have been. Unsafe reuse is a safety counter
  beside false approvals.

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
| Isolated command-line reviews with NDJSON output | Macroscope | Already present: reviewers run headless with stream output and Shipyard commands emit JSON envelopes. Add fields only where a receipt cannot express provenance (the review token in R6 is one) |
| Incremental review: re-review only what changed since the approved head | Macroscope | CONSTRAIN. Approval carry by patch-id exists. Extend only with the ledger showing repeat full reviews as a cost, and never across a base change that touches the approved files |
| Policy inheritance across repositories | Macroscope | DEFER. Needs protected provenance, visible resolution, conflict refusal and deny by default; R9 first decides which policies survive |
| Operating modes (fast, normal, deep, release) | Macroscope | DEFER. Risk tier (R2) already expresses this; no new modes without a shown tradeoff |
| Warm runner pools sized by demand | Actions Runner Controller, Namespace, Cirrus Runners | ADOPT as T3, decided by TartCI |
| Fleet orchestration for Tart VMs | Cirrus Orchard | Review under T6; TartCI keeps authority |
| Build provenance attestation | SLSA, GitHub artifact attestations | Present for releases; extend to the merge receipt only if a consumer needs it |
| Per-job timeline and wait breakdown | Buildkite, Datadog CI Visibility | ADOPT as T1 |
| Hosted dashboards, Slack and S3 delivery | most | DEFER. The loop's issues and the scorecard come first |
