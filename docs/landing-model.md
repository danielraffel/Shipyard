# The landing model — `shipyard landing`

`shipyard landing` answers one question, measured rather than read:

> in this repository, what is the mechanism by which a pull request becomes a
> commit on the default branch?

It is read-only. It never merges, enqueues, dispatches or cancels.

```bash
shipyard landing                                   # the origin remote, its default base
shipyard landing --repo OWNER/REPO --base main
shipyard --json landing                            # machine-readable
shipyard landing --pr 123                          # one pull request's queue state
```

Exit `0` when the mechanism was fully determined, `9` when any headline field
is `UNKNOWN`.

## What it reports

| field | measured from |
|---|---|
| merge queue: present/absent, enforcement, grouping strategy, `max_entries_to_merge`, `max_entries_to_build`, `min_entries_to_merge`, merge method, check timeout | `GET /repos/{o}/{r}/rulesets` + per-ruleset detail, corroborated by GraphQL `repository.mergeQueue(branch:)` |
| strict up-to-date protection, and its consequence | `GET /repos/{o}/{r}/branches/{b}/protection` |
| how to enqueue, with the exact merge method this repo requires | the queue's own configuration |
| where each required check actually executes | a completed job's `runner_name` / `runner_group_name` |
| open-pull-request backlog by mergeability | GraphQL `pullRequests.mergeStateStatus` |

## Why branch protection is the wrong place to look for a queue

`GET /repos/{owner}/{repo}/branches/{branch}/protection` has **no field** that
can carry a merge queue. Its payload is `required_status_checks`,
`required_pull_request_reviews`, `enforce_admins`, `required_linear_history`
and friends. A queue configured as a repository **ruleset** appears in none of
them.

The endpoint does not return `false` for the queue. It says nothing — and
silence reads as `false` to anybody who only asks there. A repository with a
live, actively enforcing queue answers that call with a clean `200 OK` whose
every field is accurate and whose omission is total.

So `landing` records branch protection as a surface that **cannot vote** on
queue presence (`cannot-say` in the human output, `inexpressible` in JSON). It
supplies strict protection and the required contexts, and its silence is never
counted as a finding of absence. The queue is read from the rulesets endpoints,
which is the only REST surface that carries the `merge_queue` rule.

## Why `runs-on:` is the wrong place to look for a runner

Reading a workflow's `runs-on:` answers "what did this job ask for", not "what
answered". The value is routinely an indirection —
`fromJSON(vars.SOME_RUNS_ON_JSON)` — whose contents live in repository
variables the workflow file does not contain, and even a literal is a request.

Placement is therefore derived from a completed job's own `runner_name` and
`runner_group_name`, written by whatever machine actually picked the job up. A
job that never executed carries `runner_name: null`, and is reported as
`no-evidence` rather than as a placement — with the skipped case distinguished
from the never-found case, because only the first tells you the name is right.

## Fail closed

An unreadable surface is `UNKNOWN`, never `absent`.

Reporting "no merge queue" when the truth is "could not read rulesets" is worse
than reporting nothing, because absence is actionable and somebody acts on it.
The queue verdict settles on `absent` only when every surface that can express
a queue was read and none of them found one. One readable surface finding
nothing does not license absence while another surface is blind.

Every `UNKNOWN` carries the boundary that produced it — `permission`,
`identity`, `scope`, `grammar`, `parse`, or `transport` — so a reader is sent
to the right subsystem. A rate limit is `transport`, not `permission`, however
much its `403` resembles one.

## Reading the output

The report leads with the action, because the action is the thing that gets got
wrong:

- **Queue present + strict ON** — the action is `enqueue`. Merging one pull
  request at a time is a treadmill: each landing puts every other open pull
  request `behind`, forcing an individual full-gate revalidation. The queue
  exists to batch exactly that.
- **Queue present, non-enforcing** (`enforcement: evaluate`) — the queue
  reports but does not gate; merge directly.
- **No queue + strict ON** — land in dependency order and expect serialized
  revalidation.
- **`UNKNOWN`** — determine the mechanism before doing bulk work. Hand-rebasing
  a backlog against a live queue is wasted effort, and enabling auto-merge
  against a repository with no queue silently does nothing useful.

The merge method matters beyond the merge. `landing` names the queue's own
method rather than a convention, because a repository whose release automation
keys on a marker commit's subject breaks when a squash folds that commit away.

The backlog counts separate the two blockers that call for opposite responses:
`dirty` needs conflict resolution one pull request at a time and no amount of
queue capacity moves it, while `behind` and `blocked` are gate and capacity
questions a queue absorbs.

## Budget

Cold, on a repository with two rulesets and five required contexts: one
rulesets list, two ruleset details, one branch protection, one GraphQL round
trip (default branch + base-ref existence + queue + backlog), up to six
check-run reads to find which runs produced the required contexts, and a job
read per candidate run. Both loops stop as soon as every context is placed.
The report prints the number of API calls it actually spent rather than an
estimate: roughly 5 on a small repository, roughly 30 on one with five
required contexts and a large backlog.

`--run-sample` and `--max-job-reads` bound the fallback sweep used when the
check-run route does not cover every context.

## One pull request: `shipyard landing --pr <n>`

With `--pr`, `landing` skips the repository model and classifies one pull
request's merge-queue state from a single GraphQL read. Every line of output
names the response field it came from, and the report always opens with:

> REST pulls/<n>.auto_merge is null for every queued PR (GitHub consumes
> auto-merge on enqueue) — never read it as 'unarmed'.

| class | meaning | next action |
|---|---|---|
| `queued` | `isInMergeQueue` is true (position from `mergeQueueEntry`) | nothing; do not re-arm |
| `armed_not_queued` | `autoMergeRequest` set, not yet admitted | nothing; the queue admits it when checks pass |
| `never_armed` | open, not armed, no removal in the timeline window | `shipyard ship --pr <n>` |
| `ejected` | last `RemovedFromMergeQueueEvent` was not `merged`, not queued, not armed | new head since, or `invalid_merge_commit`: `shipyard ship --pr <n>`; same head after `failed_checks`/`merge_conflict`: push a fix first; same head after any other reason (`manual`, ...): confirm with whoever dequeued it |
| `merged` / `closed` | PR state | nothing |
| `unknown` | the read failed or was malformed, or the timeline window is truncated and shows no removal and no new head; exit `9` | do not act |

History is read in **timeline order**, not by commit date and not from
`RemovedFromMergeQueueEvent.beforeCommit` (unreliable): a new head is a
`PullRequestCommit` or `HeadRefForcePushedEvent` after the last removal.
`requeues_without_new_head` counts `AddedToMergeQueueEvent`s that directly
follow a `failed_checks`/`merge_conflict` removal with no new head between.
Under `ALLGREEN` grouping each such re-add fails every batch-mate with it. A
`merged` removal (emitted right after `MergedEvent`) is never an ejection.

The query reads `timelineItems(last: 100)`. When `pageInfo.hasPreviousPage`
is true the report says `TRUNCATED` and the counts are lower bounds. Actor
identity is not consulted: every queue mutation is attributed to the same App
actor whether Shipyard or an agent issued it.

### What the head's green means, and whether the merge group ran tests

The report also carries a `VALIDATION` block (JSON: `validation`). It reads the
`shipyard-test-tier` annotation on each **required** check run of the head
(`isRequired(pullRequestNumber:)`, newest run per name) and says whether the
head is green on a narrowed tier ("NOT full validation; the full suite runs in
merge_group"), fully tested, or of **unknown** tier (no annotation: never read
as full). For the PR's merge group, the live queue entry's head or, once
merged, its merge commit, it lists every `shipyard-receipt-decision` annotation
published by that SHA's `merge_group` workflow runs: "reused receipt from run
N: X selected / Y passed (Z skipped)" or "validated in full: receipt refused
because ...". A malformed annotation is shown as unparseable; an unreadable
endpoint as unknown. Neither changes the exit code. Contract, emission recipe
and API cost: [`validation-signals.md`](validation-signals.md).

Operator overrides for the guard are in `docs/ghapp-guards.md`. The same
classifier guards the `ghapp` wrapper: `ghapp_queue_arm_guard.py`
refuses to arm or enqueue a PR in any class whose next action above is not
"land it". Its Python twin and this Rust classifier assert against the same
real-response corpus in `tests/fixtures/github/`.

## Relationship to `shipyard landability`

They are siblings and answer adjacent halves of the same question:

- `landability` — can this pull request's required contexts be **scheduled**
  onto a runner that exists?
- `landing` — what happens to a pull request whose contexts are green?

`shipyard status` is Shipyard's own state feed: queue, targets, evidence.
Everything in it is a fact about Shipyard, which is what makes it trustworthy;
a GitHub-policy reading belongs beside it, not inside it.
