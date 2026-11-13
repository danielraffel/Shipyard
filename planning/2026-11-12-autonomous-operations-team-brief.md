# Team brief: autonomous, evidence-driven operations

Carries out `planning/2026-11-12-autonomous-evidence-driven-operations.md`.
Work item identifiers (L1, C3, ...) refer to that plan. Read the plan first.

Status: Ledger, Audit, Conflicts (C1 only) and Review (R6 to R8) are approved and started. The other lanes wait for the baseline.
Read-only lanes (marked) change nothing and may start first.

## Shape

One lead and eight lanes. A lane is one agent session that owns a set of files
and a queue of small PRs. Lanes do not share files, so they do not conflict.

| Lane | Items | Owns | Model | Starts |
| --- | --- | --- | --- | --- |
| Lead | sequencing, gates, the plan file | `planning/2026-11-12-*` | Fable, for planning and re-ranking only | first |
| Ledger | L1, L2, L3, L5 | `src/metrics/` new `loss` and `recovery_score` modules, `docs/rollups.md` | Sonnet | first |
| Conflicts | C1 (read-only), then C2 to C5 | new `src/conflict_repair/`, `docs/conflict-repair.md` | Sonnet; Opus for C3 push path | C1 at once; the rest after L1 |
| Promotion | ladder config, superseded close, dwell scoring, S1 arbiter | `src/app/merge_steward_cmd/` recovery modules, `docs/automated-draining.md` | Opus (steward admission code) | after L3 |
| Review | R6 to R8 first, then R1 to R5, M1, M2 | `tools/fleet-pass/`, `src/review_verdict*`, `docs/review-verdict-status.md`, `docs/risk-tier.md` | Sonnet; Opus for verdict-gate changes | first |
| Budget | B2, B3, S5 (B1 waits on Daniel) | `src/github_read*`, steward invariants, `docs/github-app-quota.md` | Sonnet | first |
| Fleet | T1 to T5, in Shipyard and TartCI | `src/metrics/` TartCI import, TartCI export and receipt schema, both repos' contract fixtures | Sonnet; Opus for anything touching leases or reaping | first (T1 is read-only joins) |
| Hygiene | X1 to X5, P1 | new `src/census/`, `docs/orphan-census.md`, lesson-to-gate table in fleet-handbook | Sonnet, Haiku for the census sweeps | first (X1 and X3 start as reports) |
| Audit | S2, S3, S4, O1, prior-art checks, T6 (all read-only studies) | `planning/` notes only | Sonnet, Haiku for counting | first |

Model rule: Fable plans and re-ranks and does no lane work. Lanes run on the
smallest model that does the job: Haiku or Codex Luna for counting, sweeps and
mechanical merges; Sonnet or Codex Terra for code, scripts and reports; Opus
or Codex Astra only for admission, verdict-gate, lease, credential and
release changes. Independent challenge and review of a lane's work goes to a
different provider than the one that wrote it. The lead sets the model when
it launches a lane and records it in the lane's tracking issue.

`src/app/merge_steward_cmd/` has one writer at a time: the Promotion lane.
Any other lane that needs a change there files it to Promotion.

The Loop item L4 goes to the Ledger lane once L2 and L3 have run for a week.
O2 and M3 are not staffed until the audit and the loop produce their inputs.

## Order and gates

The lead holds these gates. A lane does not pass a gate on its own judgment.

1. **Measure before acting.** L1 and L2 land and produce one week of real
   numbers. C1, O1, S2, S3, S4 report. No recovery is built or promoted
   before this, except work already in shadow on main.
2. **Re-rank.** The lead compares the L2 numbers with the plan's estimates.
   If the order of losses differs, the lead reorders the lanes and updates
   the plan. Expected top class: `merge_state:DIRTY`.
3. **Build in shadow.** C3, superseded close, the arbiter, R3, R4 land
   default-off or shadow with planted negatives.
4. **Promote by the ladder.** Shadow to canary on Shipyard, then enforce,
   each by its own PR carrying the L3 scorecard rows as evidence.
5. **Close the loop.** L4 runs daily and takes over steps 2 and 4.

Parallel throughout: Review lane R1 and R2, Budget lane B2 and B3, since
neither depends on the ledger.

## First PRs per lane

Each is one PR, mergeable on its own, with tests.

**Ledger**
1. L1 schema and writer: one row per `blocked_on` transition from the
   steward's per-PR rows. Backfill from retained tick JSON if it exists;
   report the retention found.
2. L2 `shipyard metrics loss` with coverage and unknown share.
3. L3 recovery scorecard reading existing shadow records (dwell
   `overdue.jsonl`, superseded shadow rows, `version_only` shadow report,
   `jump-decisions.jsonl`, infra-rerun ledger).
4. L5 projection field and doc table.

**Conflicts**
1. C1 report (read-only): conflicting paths for every `DIRTY` episode in the
   last 14 days, by repository, with the share confined to generated or
   counter files. Uses `git merge-tree` locally; no pushes, no API writes.
2. C2 per finding: the smallest change that removes a hot file from PR
   diffs. Pulp changes go through Pulp's own PR path.
3. C3 in shadow: decide and record "would repair", no push.
4. C4 route to repair at first observation, with paths.
5. C5 pre-admission test merge, shadow.

**Promotion**
1. Mode enum `off | shadow | canary | enforce` shared by all recoveries, with
   existing config values mapped onto it. No behavior change.
2. S1 arbiter in shadow: record which recovery would own each head and
   where two would have acted on the same head.
3. Superseded close: the closing act behind the mode, with the evidence
   comment and kill switch.
4. Promotion PRs as L3 qualifies each recovery.

**Review**
0. R6 and R7 first, ahead of every gate below: a HOLD without a finding
   does not block and is re-dispatched; a reviewer slot proves it can read
   the workspace before it claims. This is a live fault (Vellum PR 385 and
   PR 381, 2026-11-12), not a study. R8 follows directly.
1. R1 reviewer-pass age invariant and alert.
2. M1 probe (read-only): does Subrouter expose the served model and
   reasoning level. Report the answer with a sample receipt.
3. R4 findings block in the verdict comment and finding-id closure in the
   repair lane; rounds-per-PR metric.
4. R2 tier input to fleet-pass, shadow first: log the model it would pick.
5. R5 `shipyard pr preview`.

**Budget**
1. B3 attribution (read-only): what the p90 and maximum tick time is spent
   on.
2. B2 shared per-head snapshot for one reader, then the rest; reads per
   merged PR reported.
3. S5 drift invariants.

**Fleet**
1. T1 join (read-only): per-job wait by stage from the existing TartCI
   import; report what the export lacks to complete the split.
2. T5 versioned export schema with golden fixtures in both repositories.
3. T2 infra rerun reads the TartCI terminal receipt, shadow: record where
   it agrees and disagrees with the log classifier.
4. T4 host-condition invariants (disk, runner version, golden age).
5. T3 demand signal, published only; TartCI's use of it is a TartCI PR.

The Fleet lane never calls a TartCI mutation (`leases`, `pool`,
`doctor --reap --fix`). TartCI's own `docs/agents/never.md` applies.

**Hygiene**
1. X1 table (report): every lesson and friction report, its gate, its
   planted test, or "open".
2. X3 census (read-only): counts, ages, owners for each object type. No
   deletion in this PR.
3. X3 dispositions for Shipyard-owned objects, one object type per PR,
   shadow first.
4. X5 refusal counter and fix-command text on existing guards.
5. P1 fresh-install canary on a scratch repository.

**Audit** (reports under `planning/`, no code)
1. S2: replay the five carry-guard incidents and named residuals against a
   tree-equality rule. Verdict: replace, keep, or narrow.
2. S3: mapping table for the three failure vocabularies.
3. S4: count and cost of version and skill-sync chore commits; options.
4. O1: every owner stop in the last 30 days, ranked.
5. For each "ADOPT" row in the plan's prior-art table: confirm the upstream
   behavior and note what differs from the plan's description.
6. R9: every mechanism between "checks green" and "admitted", the incident
   it prevents, and its cost in blocked hours.

## Rules every lane follows

These restate the fleet rules that matter most here.

- **Own PRs first.** Fix your own red check, HOLD, or conflict before
  opening the next PR. Merge `origin/main`; never rebase or force-push.
- **Open and stop.** `shipyard pr --base main --no-arm`. Never arm, enqueue,
  or merge. Never post a verdict on your own or your team's PR.
- **No waiting.** After opening a PR, move to the next item. Check once with
  `shipyard wait pr <n> --state actionable --timeout 60 --json` before
  calling anything blocked. The owner relay messages you on change.
- **Gates before push.** Run the pre-push gates locally; one push per fix
  round.
- **Small PRs.** One work item step per PR. Default-off or shadow for any
  new act. Every act ships with its planted negative.
- **Read budget.** Reads go through `ghapp` (or the steward's cached
  snapshot). No poll loops. State reads per pass in the PR body for any new
  reader.
- **Searching.** Inside your own worktree only.
- **Worktrees.** Under `$PULP_WORKTREES_ROOT` or next to the primary
  checkout, never `/tmp`.
- **Sessions.** Launch through `sr claude` or `sr codex`; check `sr status`
  first and switch provider when one is exhausted.
- **Docs in the same change.** A change to how the fleet works updates the
  matching `docs/` page, the skill file the sync gate names, and
  fleet-handbook `docs/how-we-work.md`.
- **Ask Daniel only for** secrets, logins, spending, sudo, GitHub settings,
  and the four decisions listed in the plan.

## What a lane hands back

Per item, one comment on the tracking issue:

```text
ITEM: <id>
PR: <full URL> at <full head SHA>, or REPORT: <path on main>
MODE: off | shadow | canary | enforce | n/a
EVIDENCE: <metric rows, test names, receipts>
MOVES: <metric and expected direction>
NEXT: <exact next step and who takes it>
```

## Lead duties

- Open one tracking issue per lane and one for the plan; keep the plan's
  "Current standing" table true.
- Hold the gates above. At gate 2, re-rank from data and say so in the plan.
- Watch for two lanes touching the same file and move the work, not the
  rule.
- Stop a lane that has pushed three fix rounds on one PR without progress;
  have a different model review the approach.
- Weekly: post the L2 and L3 tables to the plan issue with what was
  promoted, demoted, and why.
- Stop conditions for the whole effort: a false approval, unapproved content
  on a base, or a recovery acting outside its head fence. Return the
  recovery to shadow, file the incident, and resume only after the planted
  negative that would have caught it is in CI.

## Done

The effort is complete when L4 has run unattended for 14 days, has promoted
or demoted at least one recovery without a human, the measures table in the
plan shows the top loss class (by operational cost) down by half with no rise in the
false-approval row, the lesson table has no open rows, and the census shows
no orphan past its expiry. After that the loop continues and this brief is retired.
