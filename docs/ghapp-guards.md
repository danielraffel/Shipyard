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
| removed for `failed_checks` / `merge_conflict`, same head | refuse: under ALLGREEN a same-head re-enqueue fails its batch-mates; push a fix first |
| removed for any other reason (`manual`, ...), same head | refuse: confirm with whoever dequeued it |
| merged / closed | refuse |
| unreadable, or a truncated timeline window that shows no removal and no new head | refuse |

A GraphQL body read from stdin (`--input -`, `query=@-`) cannot be inspected
and is refused as ambiguous.

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

Which of a repository's steps can be influenced by repository content is
knowledge the repository has and Shipyard does not. So Shipyard asks.

## Overrides and bypasses

| variable | who sets it | effect |
|---|---|---|
| `SHIPYARD_INTERNAL_QUEUE_MUTATION=1` | Shipyard itself, on its own exact-head, audited queue commands (enqueue arm, classic merge, merge-steward enqueue, disable/dequeue revocation) | both queue guards step aside; Shipyard's admission rules already made the decision |
| `GHAPP_ALLOW_QUEUE_REARM=1` | an operator, deliberately, for one command | the arm guard allows a refused arm and prints a `WARNING` naming what it overrode |
| `GHAPP_ALLOW_QUEUE_REMOVAL=1` | an operator, deliberately | the removal guard allows a dequeue/disable and prints a `WARNING` |

Setting `SHIPYARD_INTERNAL_QUEUE_MUTATION` by hand claims Shipyard's authority
for a command Shipyard did not audit. Do not.

## Installing, and fleet-wide ordering

```sh
shipyard guards status    # exit 1 unless both guards are installed and match this build
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
unaffected. To avoid the transition entirely, update every Shipyard binary on
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
