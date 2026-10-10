# `ghapp` queue guards — operator reference

This page is for **operators**. It is where the override and bypass mechanisms
are documented. The guards' refusal messages and the agent skills deliberately
do not name them: an agent that reads an override in a refusal treats it as the
next step, and the refusal exists because the next step is something else.

## What runs

`scripts/ghapp` runs these optional guards from `$SHIPYARD_GHAPP_GUARDS_DIR`
(default `~/.config/shipyard/guards`) before the native `gh`. A guard that is not
present, or not executable, is skipped.

| installed name | source | refuses |
|---|---|---|
| `queue-removal-guard` | `scripts/ghapp_queue_removal_guard.py` | `pr merge --disable-auto`, `dequeuePullRequest`, `disablePullRequestAutoMerge` |
| `queue-arm-guard` | `scripts/ghapp_queue_arm_guard.py` | `pr merge --auto`, a plain `pr merge` whose base has a merge queue, `enablePullRequestAutoMerge`, `enqueuePullRequest`, when the PR's live state makes arming harmful (below) |
| `branch-refresh-guard` | `scripts/ghapp_branch_refresh_guard.py` | `pr update-branch`, REST `pulls/<n>/update-branch`, `updatePullRequestBranch`, only when the base's `[merge] refresh_branch` policy is `"only-if-conflicting"` and the refresh is provably pointless (below) |
| `merge-guard` | `scripts/ghapp_merge_guard.sh` | `pr merge` on a repository listed in `~/.config/shipyard/merge-guard.json` (one that cannot enforce required checks server-side) until every listed check reads `pass`, and `--auto` there outright; inert for unlisted repositories and when that file is missing or malformed |

`pr-close-guard` is a mandatory member of the authenticated `ghapp` generation
and is managed by `shipyard fleet update`, not by this page.

### Queue-arm guard decisions

The guard reads the PR's live state with the same GraphQL query and classifier
as `shipyard landing --pr <n>` (see `docs/landing-model.md`).

| live state | decision |
|---|---|
| never armed, not queued | allow |
| removed from the queue, new commit or force-push since | allow |
| removed for `invalid_merge_commit`, same head | allow (GitHub failed to build the merge commit; nothing against the head, and the one reason Shipyard's own admission re-arms after) |
| already queued | refuse: nothing to do; REST `auto_merge` is `null` for every queued PR |
| auto-merge armed, not yet queued | refuse: the queue will pick it up |
| removed for `failed_checks`, same head, batch attributor certifies the head | allow (the repository ruled the ejecting batch's failure not this head's; see [Batch attribution](#batch-attribution)) |
| removed for `failed_checks`, same head, first ejection of that head, every failing required check failed on the network, repository opted in | allow, once (see [Environment re-enqueue](#environment-re-enqueue)) |
| removed for `failed_checks` / `merge_conflict`, same head | refuse: under ALLGREEN a same-head re-enqueue fails its batch-mates; push a fix first |
| removed for any other reason (`manual`, ...), same head | refuse: confirm with whoever dequeued it |
| merged / closed | refuse |
| unreadable, or a truncated timeline window that shows no removal and no new head | refuse |

A GraphQL body read from stdin (`--input -`, `query=@-`) cannot be inspected
and is refused as ambiguous.

## Environment re-enqueue

A batch that died because the network did (a package relay answered 403, a
download host did not resolve, an upload connection was reset) says nothing
against the head, yet the same-head refusal made the only way back into the
queue a new push, and a whole required-gate cycle for a commit that changes
nothing. Over 2026-09-15..29 on `Generous-Corp/pulp`, 15 queue ejections were
environment failures and 12 of them were followed by exactly such a push.

A repository opts in with:

```toml
# .shipyard/config.toml
[queue.environment_requeue]
enabled = true
```

Then, for a same-head `failed_checks` ejection only, the guard (and
`shipyard landing --pr`, and `ship`'s arm-on-open, which share
`src/environment_requeue.rs`) allows **one** re-enqueue when all of these hold:

| requirement | why |
|---|---|
| the timeline window is complete and shows exactly one `failed_checks`/`merge_conflict` removal of the current head | the allowance is one retry per head; a head ejected twice has had it, and a truncated window cannot prove it has not |
| the removal names its merge-group commit (`beforeCommit`) | that commit's check runs are the ones that ejected it; no run-resolution heuristic |
| the base's required checks (rulesets plus classic protection) can be read and are non-empty | only a **required** failure ejects; an advisory lane failing a real test is not what removed the head |
| every failing required check on the merge-group commit is a GitHub Actions job concluded `failure` (or `cancelled` and starved, below), and no required commit status failed | a `timed_out` job or a status has no log that could prove anything |
| every failing step of each such job (jobs API `conclusion == failure`) printed an environment signature within 60 output lines of that step's first `##[error]` | positive evidence at the failure, not anywhere in a long log |

The signatures are the network-transport spellings shared with Shipyard's infra
classifier (`Could not resolve host`, `Network is unreachable`, `No route to
host`, `Connection reset by peer`) plus `ENOTFOUND`, `getaddrinfo`,
`EAI_AGAIN`, `ECONNRESET`, `Tunnel connection failed`, `Proxy CONNECT aborted`,
`Temporary failure in name resolution`, `curl: (6)` and `curl: (56)`. Broad
words such as `timeout`, `rate limit` or `Connection refused` are deliberately
absent: a test the head broke prints them too.

Two traps shaped the reader. **A step's own script can contain the signature**:
Pulp's `Install visual-analysis Python dependencies` step carries a comment
quoting `Tunnel connection failed: 403 Forbidden`, and GitHub echoes a `run:`
script into the log inside `##[group]Run ... ##[endgroup]`. Those lines are never
read as output. **The first `##[error]` in a log is not the failing step**: an
`if: always()` / `continue-on-error` step after it prints its own. The failing
step comes from the jobs API and its output from the first `Run` segment at or
after the step's `started_at`, through that segment's first `##[error]`.

### Interruptions: a starved job, or an upload that stalled after green

Two more ejection causes say nothing against the head, and the same reader
(Rust and the guard's Python twin) treats them as **interruptions**:

| cause | evidence required |
|---|---|
| a required job **starved** of a runner | the required check concluded `cancelled`, its Actions job has an empty `runner_name` (`gate_cost/proxy.rs` `ejection_cause()` says `starved`), and it waited at least 10 minutes from `created_at` to `completed_at`. A shorter no-runner cancel is a superseding push or a concurrency-group cancel. Missing times refuse. |
| an **upload that stalled** after the work passed | the failing step's own `##[error]` line, or the line before it, reads `Upload progress stalled`. Any earlier failing step, such as a red test step, has no signature and refuses the whole verdict; a stall that recovered earlier in the step explains nothing. |

An interruption allows a same-head re-enqueue on each of a head's first **two**
ejections (a network failure allows the first only), and an allowed
interruption does not count toward the head-approval `EJECTION_CAP`. Every
same-head re-arm, from `ship`'s arm-on-open or from the steward's
`--arm-unqueued` backstop, is sent with `expectedHeadOid` bound to the head the
classifier read, so GitHub refuses it if the head moved. The steward's backstop
reads the opt-in from the protected base's `.shipyard/config.toml`, never from
the head. A real `failure` with a runner, `merge_conflict`, or a moved head is
never re-armed.

### Why this is not the inference refused above

[Batch attribution](#why-the-guard-does-not-rule-for-itself) refuses to read
*absence* ("no test failed") as innocence. This reads *presence*: the step that
failed printed, at its failure, a line only the network produces. The residual
risk is a head whose own content names an unreachable host, and a head broken
in a way the dead batch never reached (`#8811`). Both are why the allowance is
bounded to one retry per head and off unless the repository opts in: the worst
a wrong allowance costs is the one batch the retry joins, and the second
ejection is refused with the ordinary "push a fix first".

### Relationship to a declared attributor

The guard asks the repository's [batch attributor](#batch-attribution) first.
When it certifies, that allow stands and the environment reader is not
consulted. When it does not certify, including a verdict of
`implicates_head: true`, the environment reader still runs. That is deliberate:
Pulp's attributor reads chain ancestry (the batch's parent passed, so this head
is the culprit), which compares outcomes rather than causes. A parent that
passed on a host that could reach PyPI says nothing about a batch that died on
a host that could not, and `--certify` returns `implicates_head: true` for both
`pulp#8678`'s pip-relay ejection and `pulp#8911`'s cargo-DNS ejection. The
environment verdict rests on the failing step's own output instead, which a
content-level implication (the head owns a failing ctest case) cannot share:
such a step failed on a test, not on a network signature at its failure.

An allowed re-enqueue is not silent: the guard prints
`queue-arm-guard: note: ...` with the verdict and each step's matching log line.
A refused one appends `Environment re-enqueue refused: <reason>` to the
ordinary refusal. `shipyard landing --pr <n>` prints the same verdict and
evidence under `ENVIRONMENT RE-ENQUEUE`.

## Batch attribution

A pull request ejected for `failed_checks` is refused a same-head re-enqueue
because under ALLGREEN a head that failed its batch will fail the next one too,
taking innocent batch-mates with it. That reasoning assumes the head is what
failed. Sometimes it is not: the batch failed on infrastructure, or on a
breakage already present on the base, or on a different entry in the batch.

The guard does not decide that for itself. It asks the repository, and only a
positive certification turns the refusal into an allow.

### Declaring an attributor

```toml
# .shipyard/config.toml
[queue.attribution]
command = ["python3", "tools/scripts/queue_batch_attribute.py"]
```

`command` must be an argv list; a shell string is rejected rather than quoted,
so the repository's config never becomes a shell-injection surface for a command
that runs on an operator's machine. The config is found by searching upward from
the working directory `ghapp` was invoked in, and it is used only when that
checkout is the pull request's own repository.

**With no `[queue.attribution]` declared the guard makes no extra API reads and
behaves exactly as it did before.** Nothing changes for a repository that does
not opt in.

### What the guard asks

When, and only when, a target is a same-head ejection for `failed_checks`, the
guard resolves the ejecting `merge_group` run (the most recent failed
`merge_group` run created no later than the removal, whose batch contains this
head, proven by the read-only queue branch naming `pr-<n>-` or by commit
ancestry), collects each failing job and the names of its failing steps, and
runs:

```
<command...> --repo <owner/name> --pr <number> --run-id <id>
```

The run is searched by creation time, `created=<removal-6h>..<removal+5m>`,
100 per page and at most 3 pages, never by recency. Every workflow of every
failed group is its own run, so on `Generous-Corp/pulp` "the latest 20 failed
runs" covered about an hour, and a certified ejection (`pulp#9048`) could not be
re-admitted once that hour passed. A window with nothing matching, or busier
than the read cap, refuses and says which.

### The attributor's environment

The command runs from the repository root with its output captured and a
120-second timeout, inside the guard's environment rather than the operator's
shell:

- `PATH` is the wrapper's trusted system path
  (`/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin`). `ghapp`
  itself, usually installed under `~/.local/bin`, is **not** on it, and neither
  is a shell function or alias. An attributor that shells out to a bare `ghapp`
  fails with "not found" here even though the same command works interactively.
- `GHAPP_REAL_GH` names the real `gh` binary, `GH_TOKEN` holds the App token
  the wrapper minted for the command being guarded, and `GH_REPO` is bound to
  the pull request's repository. `"$GHAPP_REAL_GH" api ...` makes a read under
  the same identity the guard used.

A non-zero exit, or a verdict that is not JSON, refuses with the last few lines
of the attributor's stderr (control characters removed) in the message, so a
crash is diagnosable from the refusal alone.

### What certifies, and what does not

The attributor must print one JSON object on stdout:

```json
{
  "run_id": 36093055057,
  "implicates_head": false,
  "verdict": "infrastructure",
  "evidence": "macos failed at `Install ccache (macOS)`, before any repository content built"
}
```

Certification requires **all** of:

| requirement | why |
|---|---|
| exit status 0 | a crashed attributor has not ruled |
| stdout is a JSON object | an unparsable verdict is not a verdict |
| `run_id` equals the run the guard resolved | a ruling about another run does not apply here |
| `implicates_head` is exactly `false` | `null`, missing, or a string is not a ruling |
| `verdict` is `infrastructure` or `other_pull_request` | it must name *why*, not what it failed to find |
| for `other_pull_request`, `implicated_pr` is an integer other than this PR | blaming this PR is not blaming another one |

Everything else refuses, and the refusal now carries the batch's failing jobs
and steps so the attribution can be made in one step instead of hunted for.

A certified allow is not silent. The guard exits 0 and prints
`queue-arm-guard: note: ...` to stderr naming the batch, the certification and
the evidence, because lifting a protective refusal should leave a trace.

**`merge_conflict` is never attributable.** A conflict is a property of the head
against its base, so it implicates the head whatever the batch's checks did.

### Why the guard does not rule for itself

It is tempting to let Shipyard decide this generically: resolve the ejecting
run, and if nothing in it looks like a test failure, call the head innocent.
That rule is unsound, and the incident that motivated this feature is the
counter-example.

`Generous-Corp/pulp#8811` was ejected for `failed_checks` at
2026-09-25T04:12:51Z by `merge_group` run 36093055057. Two jobs failed: `macos`
at `Install ccache (macOS)`, and `Linux (x64) [github-hosted]` at `Build`.
Neither is a test failure, and the repository's own tooling reported exactly
that: "no ctest failure block (failure is not a test failure)".

But **a compile error is the most common way a head breaks a batch, and it
produces no test-failure evidence at all.** A rule that reads "no test failure"
as "innocent head" allows a re-enqueue of a head that cannot possibly pass:
the precise wrong-allow this guard exists to prevent, and one whose cost is
ejecting innocent batch-mates.

A step-name signature is no better. In that same batch the `Build` failure was
CMake test discovery for `pulp-test-group-canvas`, registered at the batch's
base commit and untouched by the pull request. The neighbouring batch for
`#8807` (run 36090859921) failed the same job at the same step for the same
pre-existing cause, so `(job, step)` does corroborate across batches, but
`(macos, Install ccache (macOS))` corroborates nowhere that day, and a rule
requiring every failing job to be corroborated refuses this incident anyway.

The same incident also shows that "un-implicated by the ejecting batch" is
strictly weaker than "will pass next time". `#8811`'s visible batch failure was
not its own, but its head was broken anyway, by the same class of defect: a
grouped member spec whose only case compiles on macOS, so discovery matched
nothing on Linux and Windows. The build aborted on the base's `group-canvas`
failure before reaching it, so that defect appears nowhere in the ejecting run,
and the pull request needed a real fix three hours later. A same-head
re-enqueue at 04:13 would have been justified by every piece of evidence that
batch contained, and would still have failed and taken its batch-mates with it.

So certification is deliberately narrow and repository-owned. Only the
repository can reason about whether a failed build even reached the parts a diff
touches, and which of its steps repository content can influence at all.
Shipyard asks rather than guessing.

### Shipyard's own arming path is deliberately judged by this guard

`shipyard pr` / `ship` / `ship --pr` arm native auto-merge once the pull request
is known, and `runner steward --arm-unqueued` does the same periodically for
pull requests the steward declines to own. Both issue
`enablePullRequestAutoMerge` through the ordinary `gh` path **without**
`SHIPYARD_INTERNAL_QUEUE_MUTATION`, so this guard judges them.

That is on purpose. The internal marker exists for requests bound to a head
Shipyard has *validated*; an arm-on-open request is not, so the guard's live
read is the only thing standing between it and re-arming a queued or ejected
head. Every state the guard refuses, Shipyard's own policy
(`src/auto_arm.rs`) refuses too, so in practice the guard agrees — and when it
refuses, Shipyard reports the refusal and carries on rather than failing the
ship. Shipyard never sets `GHAPP_ALLOW_QUEUE_REARM`.

### Branch-refresh guard decisions

The policy is `[merge] refresh_branch` in `.shipyard/config.toml`, read from the
pull request's **base branch** through the App token, so a PR branch cannot
relax it for itself:

```toml
[merge]
refresh_branch = "only-if-conflicting"   # default: "always"
```

| policy / live state | decision |
|---|---|
| `"always"`, key or file absent, or an unrecognized value (reported) | allow: Shipyard's historical behaviour |
| `"only-if-conflicting"`, base has no merge queue | allow: strict protection may need the up-to-date head to merge |
| PR conflicts (`mergeable: CONFLICTING` or `mergeStateStatus: DIRTY`) | allow: resolving it needs a new head |
| a required check on the head is failing (`FAILURE`, `TIMED_OUT`, `CANCELLED`, `ACTION_REQUIRED`, `STARTUP_FAILURE`, `STALE`; status `ERROR`/`FAILURE`) | allow: the queue would reject the PR, and a fresh merge ref can clear a failure at a step the current workflow no longer runs |
| `mergeable` still `UNKNOWN` after two re-reads, required checks truncated past 100, PR not open, or anything unreadable | allow (with a note): the guard never refuses blind |
| removed from the merge queue at its **current** head for a reason `queue-arm-guard` refuses to re-enqueue (`failed_checks`, `merge_conflict`, `manual`, ...) | allow (with a note): the arm guard demands a new head, and a refresh is one |
| the queue state (timeline) cannot be read or classified, or `queue-arm-guard` is installed but will not load | allow (with a note): not refusing blind |
| base has a merge queue, PR `MERGEABLE`, no required check failing (pending is not failing), and not removed from the queue at its current head | **refuse**: the queue validates the merge result itself, and the refresh push would cancel and restart the required gate |

The queue-state row is read only when the guard is about to refuse, with
`queue-arm-guard`'s own query and classifier (the Python twin of
`shipyard landing --pr <n>`), loaded from the arm guard installed next to it. An
ejected pull request's head checks are usually green, because the failure
happened in its `merge_group` run, so without that row an infrastructure
ejection would be refused by both guards at once. When no arm guard is
installed nothing refuses a same-head re-arm, so the row is skipped.

There is deliberately no `"never"`: a refresh that resolves a conflict is
needed for correctness, and no policy value can refuse one. The guard sees only
App-authenticated refreshes; a local `git merge origin/main && git push` does
not pass through `ghapp`, so agent guidance (the `ci` and `shipyard` skills)
carries the same rule.

### How the two guards interact

A guard that refuses must leave a path another guard allows, or the pull request
is stuck until an operator override. Under `refresh_branch = "only-if-conflicting"`
on a base with a merge queue:

| live state | `queue-arm-guard` (re-arm this head) | `branch-refresh-guard` (new head via refresh) | way forward |
|---|---|---|---|
| never armed, not queued, mergeable, green | allow | refuse | arm it (`shipyard ship --pr <n>`) |
| queued, or armed and not yet queued | refuse (nothing to do) | refuse | wait for the queue |
| removed for `invalid_merge_commit`, same head | allow | refuse | re-arm as is |
| removed for `failed_checks` / `merge_conflict`, same head | refuse (unless the repository's attributor certifies the batch, or the one [environment re-enqueue](#environment-re-enqueue) applies) | allow | push a fix, or refresh if the batch failed on infrastructure; then `shipyard ship --pr <n>` |
| removed for `manual` (or any other reason), same head | refuse: confirm with whoever dequeued it | allow | confirm, then fix or refresh, then `shipyard ship --pr <n>` |
| removed, new head since | allow | normal policy (refuse when mergeable and green) | re-arm |
| conflicting (`CONFLICTING` / `DIRTY`) | as its queue state says | allow | refresh or resolve locally |
| a required check on the head failing | as its queue state says | allow | refresh or fix |
| queue state unreadable | refuse (fail closed) | allow (fail open) | refresh, or read it with `shipyard landing --pr <n>` |
| merged / closed | refuse | allow (not open) | nothing to do |

"Same head" and "new head since" compare SHAs: the current `headRefOid` against
the head the queue removed (the second parent of the removal's `beforeCommit`
merge-group commit). Commit dates and timeline position are not consulted,
because GitHub sorts a fix committed before an ejection but pushed after it
*before* the removal. A removal that names no head (`merge_conflict`) falls back
to push-time evidence. That is a push of the current head after it in timeline
order, or the current head's earliest check suite created after the removal
event, wherever GitHub sorted its commit (basis `suite_after_removal`). The
suite stands in for the push time because `Commit.pushedDate` is null. A head
with neither, including one with no check suite, is treated as unchanged by
both guards. A commit that first reached GitHub on another branch before the
removal carries that earlier suite and also reads as unchanged. If a live push
is ever misread, the repository activity API
(`repos/{owner}/{repo}/activity`, push events with timestamps) is the
authoritative source.

The two unreadable rows are asymmetric on purpose: arming blind can fail
innocent batch-mates, while a blind refresh costs at most one gate run. Every
row has a path that at least one guard allows without an override. Each guard's
refusal names the other's allowed path: the arm guard's same-head refusal points
at `gh pr update-branch <n>`, and the refresh guard's refusal says a same-head
queue removal is refreshable.

## Overrides and bypasses

| variable | who sets it | effect |
|---|---|---|
| `SHIPYARD_INTERNAL_QUEUE_MUTATION=1` | Shipyard itself, on its own exact-head, audited queue commands (enqueue arm, classic merge, merge-steward enqueue, disable/dequeue revocation) | both queue guards step aside; Shipyard's admission rules already made the decision |
| `GHAPP_ALLOW_QUEUE_REARM=1` | an operator, deliberately, for one command | the arm guard allows a refused arm and prints a `WARNING` naming what it overrode |
| `GHAPP_ALLOW_QUEUE_REMOVAL=1` | an operator, deliberately | the removal guard allows a dequeue/disable and prints a `WARNING` |
| `GHAPP_ALLOW_BRANCH_REFRESH=1` | an operator, deliberately, for one command | the branch-refresh guard allows a refused refresh and prints a `WARNING` |

Setting `SHIPYARD_INTERNAL_QUEUE_MUTATION` by hand claims Shipyard's authority
for a command Shipyard did not audit. Do not.

## Installing, and fleet-wide ordering

```sh
shipyard guards status    # exit 1 unless every managed guard is installed and matches this build
shipyard guards install   # atomic; never overwrites a symlink
```

`shipyard doctor` reports the same state under "ghapp guards", and
`shipyard update` refreshes the guards with the newly verified binary when the
guards directory already exists.

**The guards directory is shared by every Shipyard binary on the host.**
Installing the arm guard changes the behaviour of older Shipyard binaries too
(a pinned project version, a daemon not yet refreshed, another checkout's
build). Binaries from before the arm guard do not set
`SHIPYARD_INTERNAL_QUEUE_MUTATION` on their enqueue, so their enqueues are
judged like anyone else's. In practice their admission logic already skips
queued and armed PRs, so what changes for them is:

- a same-head re-enqueue after `failed_checks` or `merge_conflict` is refused
  unless the repository declares a batch attributor that certifies the head
  (see [Batch attribution](#batch-attribution)). This is the intended refusal.
  It includes the older merge steward's queue-priority recovery, which
  re-enqueued the same head after a `failed_checks` it attributed to
  infrastructure; that recovery resumes once the host runs a marked binary, or
  once the repository's attributor can certify such a batch.
- a same-head re-enqueue after a `manual` (or other non-`invalid_merge_commit`)
  removal is refused, asking for confirmation from whoever dequeued it.
- an enqueue whose live state the guard cannot read is refused (fail closed);
  that Shipyard reports the enqueue as failed or uncertain rather than
  proceeding.

Re-enqueues after `invalid_merge_commit`, and anything after a new head, are
unaffected.

Marked binaries do not simply step around the guard: `shipyard auto-merge`
admission and the merge steward's ordinary enqueue read the same
`PR_QUEUE_STATE_QUERY` timeline and refuse a same-head re-enqueue after
`failed_checks` / `merge_conflict` themselves, whatever the age of the local
ship-state. They differ from the guard on `manual` removals, which the
attempt-scoped admission rules handle, and they do not consult the batch
attributor. To avoid the transition entirely, update every Shipyard binary on
the host (and refresh its daemon) before running `shipyard guards install`.

## Growing the real-response corpus

The classifier and the guard are pinned to real GitHub responses in
`tests/fixtures/github/` (see its README). A synthetic fixture can only encode
what we already believed, so prefer a live capture whenever one exists.

When a live PR is observed in a state the corpus only covers by truncation or
synthesis (for example ejected for `failed_checks` and not yet re-enqueued),
capture it verbatim, read-only:

1. From inside a checkout of the PR's repository (so `ghapp` can resolve it),
   run the query in `tests/fixtures/github/README.md` with
   `-f owner=... -f name=... -F number=<n>`. If `pageInfo.hasPreviousPage` is
   true, page with `timelineItems(first:100, after:$cursor)` until
   `hasNextPage` is false and concatenate the nodes in order.
2. Save the response unedited as `tests/fixtures/github/pr_<state>.json`.
3. Add its expected classification to `expected_classifications.json` and a
   README row with the PR number and capture time.
4. Run `cargo test pr_queue_state` and
   `python3 -m unittest scripts/test_ghapp_queue_arm_guard.py`: both
   implementations must agree with the new entry.

A real capture of a state supersedes a truncated or synthetic fixture for the
same state; replace it rather than keeping both.
