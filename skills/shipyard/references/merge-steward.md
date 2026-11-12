# Merge steward

`shipyard runner steward` is the agent-neutral merge-on-green reconciler:

```bash
shipyard runner steward \
  --repo Generous-Corp/pulp \
  --repo Generous-Corp/forge \
  --repo Generous-Corp/vellum \
  --json
```

It is audit-only unless `--apply` is present. The apply path uses GitHub's
server-owned exact-head guard for queue admission, preserves existing native
merge-queue positions, does not rerun genuine failures, limits transient
reruns durably, and live-revalidates queued state plus immutable head before
cancelling a run whose PR or merge-group head is provably superseded.
Same-head duplicate runs are never cancelled because their unobserved inputs
may differ. Repositories without a server-owned
merge queue are admitted only by the server-enforced exact-head merge: a
`mergePullRequest` bound by `expectedHeadOid`, taken only when GitHub requires
an app-bound approval check (`--approval-context`, default `review-verdict`) and
the head is `CLEAN`, all-green and approved. Without that rule the steward
reports `direct_merge_refused_server_enforcement_required` and merges nothing;
the REST merge endpoint cannot atomically prove complete required-check
materialization or bind the validated base revision, so it is never used.
`shipyard auto-merge` on such a repository still returns a typed
`automatic-merge-refused` decision (exit 10). Use manual merge, add the bound
approval rule, or enable a native merge queue.

Apply mode keeps its reads cheap. The census hydrates required-check identities
only for pull requests that can still reach a check-dependent decision (not
drafts, queued, conflicting or `DIRTY` rows, except managed-labelled rows, which
the handoff status needs), four at a time. When the GraphQL list is unavailable,
the REST fallback reads `mergeable_state` per candidate from `pulls/{n}`, bound
to the listed head; an unreadable or moved head stays `UNKNOWN` and is refused.
For an arm or merge candidate, exact-head approval is checked before the live
single-pull-request revalidation, so an unapproved head costs no extra read, and
that live read requests every census field (including `author`), so a
fleet-authored pull request keeps its management classification.

The transient retry ceiling is fenced twice: the local ledger records intent
before mutation, and GitHub's durable `run_attempt` must still be within the
budget at live revalidation. Losing a controller cache after GitHub
accepted a rerun therefore cannot authorize another retry on the next pass.

A rerun is for an interruption, never for a conclusion string. Only a
`CANCELLED` required check is a candidate; `TIMED_OUT`, `STARTUP_FAILURE` and
`STALE` are results. At live revalidation the steward reads the run's
latest-attempt jobs and reruns only when every job that did not pass was
cancelled with no `runner_name`, 16 to 20 minutes after it was created (the
runner-assignment timeout), with no concurrency-supersede annotation on its
check run (`transient_interruption::classify_jobs`). A person's cancel, a
superseding push, a cancel after a runner took the job, or a job that ran and
failed is reported as `not an interruption` and spends no budget. The budget
is one rerun per PR, head and required check: `--max-transient-reruns` accepts
only 0 or 1, the code clamps any wider value, and the ledger keys the attempt
by check as well as by run, so a fresh run of the same check on the same head
cannot buy a second rerun. The carrier's `redispatch` class uses the same
classifier and the same budget of one.

### Infra reruns (`--infra-rerun`, every open pull request)

A required check that FAILED for an infrastructure reason is a separate case
from an interruption, and it is not limited to owned or armed pull requests:
an apt-mirror outage turns `drift-fast` red at `Install Linux dependencies` on
every pull request pushed in the window, and nothing re-runs them until a
person notices. `--infra-rerun` (`off`, `shadow`, `enforce`; module
`merge_steward_cmd::infra_rerun`) handles it on every repository pass:

- Candidates: a required check-run whose newest attempt on the current head is
  `COMPLETED`/`FAILURE`. Managed heads come from the census the pass already
  read; heads the census skipped (unmanaged pull requests that are not draft,
  queued or conflicting) are read in batched rollup queries of six heads, at
  most 60 per pass. `review-verdict` (and `--approval-context`) is never a
  candidate: its red means "awaiting a reviewer", and its failing step is a
  non-content one, so the classifier alone would call it infra. Pull requests
  labelled `superseded`, `owner-decision`, `PARKED/*`, the opt-out label or a
  provenance-blocking label are skipped. Cancelled and timed-out checks stay
  with the transient path above.
- A run that the rollup still shows in flight costs no read. Otherwise each
  candidate run is diagnosed once per red observation with
  `shipyard diagnose`'s own classifier (`diagnose_cmd::diagnose_runs`, which
  fails closed on an unreadable annotation or a partial jobs page): the run's
  jobs, the failing required jobs' logs, and any annotations a cancel needs.
  At most 8 runs per repository pass, newest first, inside the pass budget; a
  rate-limit error stops the sweep's reads. The ledger remembers the check id
  it judged, so a declined red is never re-read.
- A run is re-run (`POST /actions/runs/{id}/rerun-failed-jobs`) only when every
  failed, timed-out or startup-failed job in it, required or not, is a
  diagnosed infra failure of a required job (cancelled fail-fast siblings are
  tolerated), no job in it is the approval context, all its jobs are complete,
  and it is of the current head. `rerun-failed-jobs` re-runs every failed job
  and its dependents, so one real failure anywhere declines the whole run.
- Budget per (head SHA, check): one re-run; a second only after 20 minutes and
  only if the re-run failed as infra again; then `capped`. The intent is saved
  before the POST, and GitHub's `run_attempt` fences the cap too, so a crash
  or a lost ledger cannot buy a third. A red after the cap is classified once:
  infra is `capped`, anything else `declined`. A refused POST or an unreadable
  diagnosis is retried after 20 minutes, twice at most (`refused`).
- Outage finding: when a check is capped, or 3 or more open pull requests are
  still red or re-running on the same check after an infra call at the same
  step, the repository report's `infra_rerun.findings` carries one line,
  "infra outage suspected: <check> failing on N PRs at step '<step>'". It is
  one finding per episode (`first_reported_at`, `new`, remembered 40 minutes
  past its last sighting) and `tools/fleet-pass/operator-findings.py` prints
  it as `INFRA-OUTAGE-SUSPECTED`.
- Known blind spot: a head can break a "non-content" step itself (a bad
  submodule pin breaks checkout, a changed lockfile breaks `Install …
  dependencies`). Such a red is re-run at most twice, then reported.
- `enforce` is the default. A 7-day back-test over the fleet's required
  failures on pull-request heads (2026-11-04 to 11-11: 529 failures, 25
  calls) found 22 of 23 verifiable calls passed on re-run or failed again as
  infra at the same step (precision 0.957; the miss was a lost runner whose
  re-run failed as infra at another step). `shadow` reports `would_rerun` and
  writes nothing to GitHub; without `--apply`, `enforce` reports as `shadow`.

Ownership is explicit. Before the steward may mutate a PR, the submitting
agent writes an exact-head receipt and management label:

```bash
shipyard runner steward-handoff \
  --repo Generous-Corp/pulp \
  --pr 123 \
  --head "$FULL_HEAD_SHA" \
  --workstream-id GEN-7 \
  --context-url https://linear.app/example/issue/GEN-7 \
  --apply
```

The command validates the open PR and expected SHA, writes the successful
`shipyard/steward-handoff` commit status, re-reads the PR head, and only then
adds `shipyard:managed`. A status stranded on an older head is harmless: the
steward requires the receipt on the current immutable head. PRs without both
signals are reported as `unmanaged` or `handoff_missing` and receive no queue,
rerun, cancellation, or recovery mutation. Add `shipyard:no-auto-merge` to opt
an otherwise managed PR out.

The origin machine gets one private persisted identity. Mutable hostname and
launcher environment values are not replay authority. When the recorded agent
session is gone, transfer is an explicit local operation: repeat the command
with the same exact head, workstream, context, and disposition plus explicit
`--agent-provider`, `--agent-session-id`, and `--transfer-agent-owner`. Shipyard
increments the ownership generation and preserves prior immutable work facts;
without that flag, a different session or changed explicit fallback surface
fails closed. The same explicit transfer flag may update a diagnosed cmux
surface for the recorded session without changing immutable work identity.

Unresolved provenance is a separate fail-closed authority boundary. By default,
any case-insensitive `5·unresolved` label produces `provenance_blocked` and
forbids queue, rerun, cancellation, preemption, recovery-signal, and
management-label mutations. Live revalidation reads the current labels before
acting. Repeat `--provenance-blocking-label <label>` to select an explicit
repository vocabulary; removing a blocker grants authority only after a fresh
current-head observation. Provenance blocking takes precedence even when the
opt-out label is also present. If a normal cancellation was already accepted,
the final force-cancel boundary re-reads blocker, opt-out, management label,
exact-head handoff status, PR number, and base; a late authority loss leaves the
pending record and a durable rejected-revalidation audit without another POST.

## Tick-start invariants

A pass can finish with `rc=0` and no errors while a repository admits nothing,
because what is wrong is the set-up around it rather than any pull request.
Every repository pass therefore reports `repos[].invariants`, one row per
check, `{id, ok, detail, fix}` (plus `skipped`, `since`, `healed`, `followup`
when they apply). Module: `merge_steward_cmd::invariants`.

| id | fails when | incident it replays |
|---|---|---|
| `required_context_producer` | a required context (bound to GitHub Actions, or unbound) is named by no workflow on the base, carried by no open head, and not listed in `[merge_steward.invariants] external_producers` in the base's `.shipyard/config.toml` | watchdog `review-verdict` required with no workflow, 2026-11-11 |
| `approval_admission_config` | the merge path is `direct_merge_refused_server_enforcement_required`; or exact-head approval is required (ruleset, `REVIEW_REQUIRED_REPOS`, or `arm_requires_head_approval`) and `[auto_merge] reviewer_signers` or `reviewer_stamp_sessions` is unset; or the ruleset requires the approval context but the approval rule does not apply to the repository (a row's error reads "head approval is not required here") | fleet-ssh #18, #19; watchdog missing from `REVIEW_REQUIRED_REPOS` |
| `required_never_ran` | a `required_never_ran:<ctx>` row is over 60 minutes old and `<ctx>` runs on no open head | watchdog #8/#7 waiting 10+ hours |
| `reviewer_verdicts_counted` | an `approval_excluded:<reason>` row (not `stale_head`/`predates_head`) is over 60 minutes old and no head counted an approval this pass | pass-runner install made verdicts comment-only, 2026-11-12 03:42Z |

Cost: the base config is the pass's own cached read. Workflows are read from
the directory the pass runs in when it is a checkout of the repository with the
base checked out (the tick's config clone), otherwise one contents listing per
pass with blobs cached by SHA beside the ledger. Nothing else is read. A
failing invariant never changes the exit code. Under `--apply` the first failing
pass is recorded in `<ledger>.invariants.json` and reported as `since`, so a
standing incident is one row per tick, not a new event. Filing is off by
default: with `[merge_steward.invariants] followup_issues = true` in the
machine-global config, the steward opens one `fleet-followup` issue per
invariant per repository in `followup_repo` (default
`danielraffel/fleet-handbook`), and comments on it only when the finding (its
`fix`) changes. `--no-invariants` turns all of it off.

The fleet-level check runs once at tick start, outside any pass: `shipyard
runner steward-invariants --fleet-repos <fleet-repos.txt> --clones <dir>
[--heal] [--json]` reports `config_clone` for every row (a clone at
`<dir>/<name>` with a readable `.shipyard/config.toml`). `--heal` clones a
missing repository with the existing clones' remote form (`git@github.com:` stays
SSH), `--filter=blob:none --sparse --depth 1`, sparse to `.shipyard` and
`.github/workflows`, and adds `.github/workflows` to an existing sparse clone
that lacks it. Both heals are idempotent and listed in `healed`; a missing
`.shipyard/config.toml` is an incident only a change on the repository fixes.
The reference tick script is `tools/steward-tick/shipyard-steward-tick.sh`; it
writes `invariants=ok|FAIL:<id>,...|off` on each repository's summary line and
`fleet invariants=...` once per tick, and `SHIPYARD_STEWARD_INVARIANTS=0` turns
both off.

## What a pass costs GitHub (`api_calls`)

Every repository in a `runner steward --json` report carries `api_calls`:
REST and GraphQL children spawned, reads answered from the pass cache, summed
child wall time, and a per-call-site breakdown (endpoint shape with SHAs and
numbers elided). The human output prints one `github: rest= graphql=
cache_hits=` line. Read it before guessing why a pass is slow or which budget
it burns; a pass that times out reports nothing, so take the numbers from a dry
run (`runner steward --repo <r> --base main --json`, no `--apply`).

Two things keep a pass's cost tracking the pull requests that can move rather
than every open one:

- **Pass read cache** (`src/gh_pass.rs`). Within one repository's pass a
  successful answer is reused only for content-addressed reads (a commit, tree
  or blob by full SHA, a compare between two full SHAs) and the base's policy
  (a file at a named ref such as `.shipyard/config.toml?ref=main`, the base's
  evaluated rules, its branch protection). Nothing that decides whether a head
  may land now is cached: reviews, comments, check runs, statuses, queue
  state, the pull request and every GraphQL read are asked fresh each time, so
  every live revalidation before a mutation still sees GitHub's live answer.
  Failed reads are never cached. On Generous-Corp/pulp this removed ten of
  eleven reads of the base config per pass.
- **Settled recovery skips the live revalidation.** A pull request whose
  required check failed and whose needs-agent signal is already in place was
  re-proved live (merge-queue snapshot, PR read, check runs, statuses) every
  pass only to find there was nothing to write. Now, under the same
  publication lease, the steward first asks local durable state whether the
  write would change anything: the exact-head actionable-failure record with
  the census's failure facts (`actionable_failure_is_recorded`, the ledger
  merge run on a copy) and the recovery request (`RecoveryStore::settled_existing`,
  the branches of `enqueue` that answer `Existing` without saving, plus a
  current witness). Only when both are settled does it return the outcome the
  full path would (`actionable_failure_persisted` when model recovery is off,
  nothing when it is on) without a GitHub read. Any change, doubt or read error
  takes the full path, which still revalidates live before writing. A failure
  whose checks change after the census is published by the next pass, whose
  census sees them. Pulp carried 27 such pull requests on 2026-11-08, about 110
  sequential reads per apply pass.

The carried-commits guard and the other admission reads (exact-head approval,
deferred tier, code-owner wait) already run only for a head that is green,
`CLEAN` and approved: `mutate_pr_with_recovery` refuses an unapproved head
from its census read before any snapshot, and the `--arm-unqueued` backstop
reaches the guard only after the approval gate allows. Keep new expensive
reads behind those gates.

The census check hydration (the complete check set of each managed, unqueued,
non-draft, non-conflicting head) reads those heads six at a time through one
GraphQL `statusCheckRollup` query (`src/app/merge_steward_cmd/head_rollup.rs`),
instead of a REST `check-runs` read plus a `statuses` read per head. Check runs
come from the rollup, lowercased to the REST spelling; the rollup lists the
same latest run per suite and name as `check-runs`. Commit statuses still come
from the REST history, because the steward reads more than the newest status
per context: the handoff check asks whether any handoff status succeeded, an
app-bound rule can be satisfied by an older status from its app under a newer
one from someone else, and the rerun budget and blocked-on report walk every
observation. A head's history is kept beside the ledger, one file per
repository (`merge-steward.status-history.<owner>~<name>.json`), and reused on the next pass only while the newest status of
every context in it equals what the rollup reports now (context, state, time,
target URL, creator) and the rollup names no other context; statuses are
append-only, so that proves the history unchanged. Dry-run passes write that
file too: it is a read cache, never a decision record. A write that dies before
its rename leaves `<file>.json.<pid>.tmp`; the next load removes such files
older than 30 minutes. A rate-limited rollup read fails the census, so the pass
takes its rate-limit cooldown instead of spending two REST reads per head on
the same installation. Any other rollup failure leaves the batch to the
per-head REST reads and is counted in `api_calls.fallbacks` (reason such as
`census check rollup -> REST: graphql_errors`, count, last message); a reason
that recurs every pass is a schema change or a persistent failure. On Generous-Corp/pulp (17 hydrated heads,
2026-11-08) this took the census from 34 REST calls to 3 GraphQL calls plus one
status read; decisions matched a concurrent pass of the old binary on 28 of 29
pull requests, the 29th being a check that completed between the two reads.
`src/app/merge_steward_cmd/tests/head_rollup.rs` pins both claims, and
`src/app/merge_steward_cmd/tests/api_budget.rs` the apply-phase one: once
failures are published, ten more blocked pull requests add no GitHub call to a
pass.

## Runner admission: a fence that must hold, not a fence that must never move

Runner admission (the typed verdict tartci lanes read before claiming or
cloning) plans against an *authority*: every open pull request's head and
managed state plus the merge queue's group heads. After inspecting the
superseded runs it re-reads that authority and refuses a plan whose authority
moved (`admission authority changed during active-run inspection`). On
Generous-Corp/pulp every such refusal over 48 h (63 across m1, m3 and m5)
followed a pull-request push or merge-group run within minutes; the base rate
for a random window was 38%. So `with_stable_admission_authority` discards the
plan and makes it again from fresh reads, up to three times, for both the
initial observation and the active-run revalidation. Only an authority that
never holds (`… (3 consecutive attempts)`) is an error, and every other error
returns at once. Never loosen the fence itself: a plan must still be made under
the authority it is checked against.

## Arming backstop (`--arm-unqueued`)

The steward enqueues only pull requests explicitly handed to it. Everything else
depends on somebody's `shipyard ship` reaching its merge phase — and when that
does not happen (the session died, the daemon dropped the job, the pull request
was opened by another route) the pull request is green, unqueued and unarmed,
with nothing left on GitHub's side to move it. Measured on one repository, 6 of
12 open pull requests were in exactly that state.

`--arm-unqueued` adds a pass that arms **GitHub-native auto-merge** on those.
It is audit-only without `--apply`, like everything else here.

```bash
shipyard runner steward --repo OWNER/REPO --arm-unqueued           # report
shipyard runner steward --repo OWNER/REPO --arm-unqueued --apply   # arm
```

It acts **only** on pull requests whose decision is `unmanaged` or
`handoff_missing` — the complement of what the enqueue path touches — so the two
can never contend for one pull request. That split is also why the weaker
mutation is appropriate: `enqueuePullRequest` asserts "this exact head is
admissible now", which is why it demands an ownership receipt, whereas
`enablePullRequestAutoMerge` asserts only "merge this when GitHub says it is
ready". GitHub re-checks required checks itself and refuses drafts itself, so
arming grants no authority the repository's branch protection does not hold.

Refused, never armed: a draft, a failing required check or outstanding review
(`mergeStateStatus` `BLOCKED`), a conflict (`DIRTY`), an already-armed or
already-queued pull request, a head the queue ejected with no new head since,
the opt-out label, a repository with native auto-merge disabled, and any state
that could not be read. `BEHIND` *is* armed: being out of date is exactly what a
queue absorbs, and refusing it would exclude most of a backlog under up-to-date
protection.

Candidates are selected from facts the pass already fetched, so selection costs
no extra request; only a candidate costs one, because a backlog row carries no
timeline and so cannot tell a never-armed pull request from an ejected one.

Where review before arm applies (`[auto_merge] arm_requires_head_approval`, or a
repository in `REVIEW_REQUIRED_REPOS` on a host without a valid opt-out),
Shipyard itself checks exact-head approval, including a `verdict: HOLD`, before
every arm, by the same rules the guard uses. The `ghapp`
queue-arm guard additionally judges each arm request where Shipyard's `gh`
resolves to the guarded wrapper (Shipyard does not claim its internal marker
here — see `docs/ghapp-guards.md`). A guard refusal is agreement
that there is nothing to arm: it is reported, never retried, and
`GHAPP_ALLOW_QUEUE_REARM` is never set.

Before arming anything under `--apply`, the pass takes the same central HOLD,
`[merge_queue].mutation_machine` authority and control lock as every other
merge-queue mutation, and holds them while it arms. A HOLD is reported as
`held: …` and is not a failure; a host without the authority, a contended lock,
or an uncertain earlier mutation on that repository and base fails the pass and
arms nothing. Audit mode and a pass with no candidates never take the lock.

Each `--repo` mints its own token (`{repo_slug}` in `token_command` expands to
that repository, not to the working directory's remote), so the pass runs from
any directory, including launchd's `$HOME`.

### Scheduled arming on the queue-authority host

`scripts/install_arm_unqueued.sh` installs this pass as a launchd agent
(`com.danielraffel.shipyard.arm-unqueued`). It prints a plan by default;
`--install` rehearses the exact pass in audit mode under the agent's
environment, refuses to load on a failed rehearsal, then loads the agent.
Each run is bounded by `/usr/bin/timeout` (default: interval minus 60 seconds),
because launchd never starts a tick while the previous one is still running.

```bash
scripts/install_arm_unqueued.sh --repo OWNER/REPO --interval 600 --shipyard "$(command -v shipyard)"  # plan
scripts/install_arm_unqueued.sh --repo OWNER/REPO --interval 600 --shipyard "$(command -v shipyard)" --install
scripts/install_arm_unqueued.sh --status      # loaded?, last exit, last 20 log lines
scripts/install_arm_unqueued.sh --uninstall   # unload and remove the plist; keeps the log
```

Install it only on the `[merge_queue].mutation_machine` host: anywhere else
every armed pass fails on authority by design. The log is
`~/Library/Logs/shipyard-arm-unqueued.log`.

Per-repository results appear under `native_auto_merge_backstop` in the JSON
report. A repository whose settings could not be read reports
`not evaluated`, which is deliberately not the same as having no candidates.

To run it periodically, install the launchd agent (plan-by-default):

```bash
scripts/install_arm_unqueued.sh --repo OWNER/REPO            # print the plan
scripts/install_arm_unqueued.sh --repo OWNER/REPO --install  # load it
```

It rehearses the pass in audit mode under the agent's own environment and
refuses to load if that fails, and it refuses a Shipyard without the flag (which
would otherwise run the steward's much broader `--apply` pass).

Semantic blockers (`required_failed` or a conflicting/dirty `needs_update`)
produce a failed `shipyard/steward-recovery` status and the
`shipyard:needs-agent` label. This pair is deduplicated on the immutable head;
normal waiting does not summon an agent. When deterministic reconciliation
observes recovery, it marks the recovery status successful and clears the
label. A cheap routed agent can consume this durable exception signal without
polling every healthy PR.

## First-line recovery worker (phase 1)

`shipyard runner recovery-worker` turns that exception signal into a durable,
bounded triage request. The request identity covers canonical repository, PR,
target base, exact head SHA, deterministic failure fingerprint, and steward
policy signature. An identical request is a no-op, policy/config drift fails
closed, and a retarget, newer head, recovered check, or changed merge state
supersedes stale work. The default invocation inspects and revalidates the live
base, head, and complete failed-required-check set (or recorded merge state)
for at most one pending request without starting a model. Each request carries
the complete required-check policy snapshot; both policy entries and failed
facts keep the literal context and optional GitHub App ID in separate fields.
That makes a newly failed required check supersede stale same-head work while
advisory failures remain irrelevant. When deterministic stewardship clears the
recovery signal, it supersedes active receipts and removes only the matching
exact-head witness under the final completion lease. A newer-head witness
published during the GitHub-operation gap remains intact, while an in-flight
old-head advisory result cannot commit afterward.
On a base without a merge queue, the steward's own failed
`shipyard/steward-recovery` status is a red non-required context, so GitHub
reports the head UNSTABLE until it is cleared. When a pass clears that signal
from a PR whose only wait was that UNSTABLE merge state, it re-reads the exact
head live, re-classifies it, and acts on the new decision in the same pass, so
a fixed PR merges without waiting a tick. The re-read is an ordinary live
classification: the merge still needs CLEAN, green required checks, and the
exact-head approval under the merge path's own guard and final revalidation.
If GitHub still reports UNSTABLE (it recomputes the state asynchronously,
so the steward looks once more after a short pause) or the re-read fails, the
PR waits for the next pass as before.
`--apply` runs one worker attempt and persists its terminal receipt;
`--drain --apply` processes only the initial bounded snapshot (at most 32), not
an unbounded watch loop. Even if normalized evidence or policy changes, a
repository/PR/exact-head tuple receives at most one model attempt.
An operational failure before claim consumes no attempt and durably rotates
that request behind untouched pending work, preventing a broken repository or
GitHub preflight from starving unrelated requests.
The steward's final merge-queue, PR, and required-check revalidation uses one
45-second absolute deadline with bounded output capture. That budget begins
before it acquires the publication lease, preventing a stalled GitHub process
from retaining the lease indefinitely. Each read inside it is capped at 15
seconds per attempt and, when the first attempt times out or GitHub answers
with a 5xx or a dropped connection, gets one more attempt inside the same
deadline; a 4xx answer is final, and a retry never extends the deadline. A
revalidation read failure leaves that PR's decision for the next pass and is
reported as the PR's error. The live single-PR read reports itself as
`live pull-request read` whichever step asked for it.

Configure it only in the trusted machine-global `config.toml` reported by
`shipyard paths`. Repository `.shipyard/config.toml` and checkout-local
overlays are intentionally ignored. Alternate runtime modes and explicit
global/state path overrides are rejected so policy and attempt accounting stay
machine-global:

```toml
[merge_steward.recovery_worker]
enabled = true
provider = "codex"
codex_binary = "/Users/you/.local/bin/codex"
codex_home = "/Users/you/.codex"
# Defaults to gpt-5.3-codex-spark when omitted.
first_line_model = "gpt-5.3-codex-spark"
timeout_seconds = 120
max_attempts_per_head = 1
max_log_tail_bytes = 16384
allowed_repositories = ["Generous-Corp/pulp", "Generous-Corp/forge", "Generous-Corp/vellum"]

[merge_steward.recovery_worker.repo_paths]
"Generous-Corp/pulp" = "/Volumes/Workshop/Code/pulp"
"Generous-Corp/forge" = "/Volumes/Workshop/Code/forge"
"Generous-Corp/vellum" = "/Volumes/Workshop/Code/vellum"
```

The command is not configurable. Shipyard constructs the exact `codex exec`
argv, disables all Codex tool surfaces used for shell, browser, app, MCP,
computer-use, image, and search access, and delivers JSON only on stdin. The
child runs in scratch rather than a repository checkout, ignores user/project
rules, and starts from an empty environment with only explicit Codex auth,
scratch HOME/TMPDIR, a minimal PATH, and Windows system root where required.
GitHub reads, model execution, and final validation are time-bounded under one
record deadline; each provenance read scans at most four explicit 100-status
pages. A machine-global durable lease allows one model call at a time. The
current label/status provenance predicate is
sufficient only for advisory triage and must not authorize a future editing or
GitHub-mutation phase without issuer-bound hardening.

The worker must emit one JSON object and no prose:

```json
{
  "schema_version": 1,
  "verdict": "escalate",
  "category": "compile",
  "confidence": "low",
  "evidence": [],
  "candidate_paths": [],
  "focused_tests": []
}
```

The schema reserves `bounded_repair`, `escalate`, and `no_change`; categories
are `compile`, `test`, `conflict`, `security`, `workflow`, `infra`, and
`unknown`; confidence is `low`, `medium`, or `high`. In evidence-free phase 1,
both `bounded_repair` and `no_change` are always rejected: every accepted result
must explicitly escalate, and the category is routing metadata only.
`evidence`, `candidate_paths`, and `focused_tests` must be empty. Free-form
summaries, explanations, and causal claims are not part of the schema: the
worker sees normalized merge/check facts, not logs or repository contents, and
may return only routing enums. There is deliberately no check-name or
changed-path heuristic in this evidence-free phase: universal escalation means
`.env`, agent-instruction, workflow/CI, signing, release, credential, and every
other surface receives the same fail-closed route even when its check has a
generic display name.

This result remains advice. Phase 1 never edits a worktree and has no GitHub,
queue, rerun, merge, release, signing, or publishing authority. Invalid JSON,
timeout, process failure, provider/quota exhaustion, config drift, and attempts
already spent are recorded as typed escalation/failure outcomes so the steward
continues managing unrelated PRs. Autonomous bounded repair is a later phase;
do not claim or depend on it yet.

For unattended operation, the configured App or workflow token needs Commit
statuses and Issues read/write. A local read-oriented App that returns the exact
`Resource not accessible by integration` rejection falls back visibly to
ambient `gh` for these low-volume status/label mutations only. High-volume
observation remains on configured auth; a controller should not rely on ambient
fallback.

Capacity preemption is an explicit repository policy. The built-in Pulp policy
may preempt at most one in-progress PR workflow per pass when an exact
merge-group front has waited at least 15 minutes. The candidate must be an
allow-listed advisory workflow actively holding a `pulp-preamble` runner while
every `pulp-build` or `pulp-build-*` leg remains queued or skipped. Required
workflows, including `Build and Test`, are never capacity-preemption
candidates. Unknown repositories have preemption disabled.

The steward fetches the live run and all jobs immediately before cancellation.
Pushes, merge groups, required workflows, unknown workflows/jobs, and any
advisory run already observed with a started or completed expensive leg are
not selected. GitHub does not offer a conditional cancellation tied to that
job snapshot, so correctness rests on the whole selected workflow being
explicitly advisory; the final job read is a waste-avoidance check, not
required-work protection. The hard cross-repository pass cap is one; no CLI
option can raise it. `--no-preempt-capacity` disables this behavior. The
per-head attempt budget and write-ahead audit live in the `handoff_ledger`
path emitted by JSON.

Apply mode holds an exclusive sibling lock for the whole reconciliation and
routes every enqueue, rerun, and cancellation through the
machine-global merge-queue mutation guard. The configured authority machine,
central `HOLD`, process-wide mutation lock, and durable uncertainty audit all
apply before GitHub is mutated; dry-run remains independent of those controls.
This prevents multiple agents on the authority host from racing or losing
attempt/audit state and rejects apply on any other host.

Immediately before a preemption the steward re-reads the native queue and the
front run's jobs, requiring the same exact speculative front SHA and at least
one recognized pool job still queued, waiting, or pending. Front advancement
or a front job starting turns the action into a reported no-op. This also
covers an organization scheduler active-workflow cap: an exact-front
`resolve-provider` job queued on `pulp-preamble` is pressure even when a
matching runner is idle.

This policy never creates VMs or changes governor leases, host caps, or Tart
capacity, so a governor denial remains authoritative and cannot be bypassed.
After GitHub accepts a cancellation, the steward polls the exact run and all
job attempts for up to 15 seconds. Capacity is considered released only after
the run is completed and no job remains queued or in progress. A nonterminal
run records `cancel_not_terminal`, sends GitHub's exact-run `force-cancel`, and
polls once more. If that still does not terminalize, the pass is unhealthy with
an auditable `job@runner` handoff target. The steward never remotely kills or
restarts a runner.

The ledger keeps a pending-cancellation record keyed by canonical repository,
run ID, immutable candidate head, and queue-front head until exact run and job
reads prove terminal. Every apply pass resumes these records before candidate
filtering, using a durable mutation intent to reconcile the exact correlation
without exposing audit-log internals. A transient observation failure leaves
the record pending and makes the pass unhealthy for a later retry.
Queue order remains immutable by default. Operators may explicitly enable
`--recover-hosted-setup-eviction-priority` for a narrow GitHub-infrastructure
exception. Apply mode first persists the queue-front managed PR head, speculative
merge-group head, base name and revision, enqueue time, and position. A later `jump: true`
enqueue is permitted once per removal/run only after the latest queue lifecycle
event is `failed_checks` within two hours, the speculative commit has the pinned base as a parent,
the same speculative head has exactly one failed required GitHub Actions CheckRun,
that removal immediately follows the witnessed admission, the
base revision and required-check governance remain unchanged, and the run has
exactly one failed GitHub-hosted `Set up job` whose log proves the
provider-internal DNS signature. The exact PR head, base,
ownership, absence from the queue, required checks, HOLD state, and configured
mutation machine are re-read before the write; the receipt is durable before
the GraphQL mutation. Generic setup failures, incomplete histories, multiple
candidate runs/jobs, self-hosted runners, and drift preserve the ordinary
non-jump behavior.

This policy does not yet cancel still-running children of an ejected
merge-group run. That fast-follow may ship only with the identical durable
witness and failure proof plus a final exact nonterminal-run and absent-queue
revalidation through the existing write-ahead cancellation guard. Queue
removal alone is not cancellation authority.
