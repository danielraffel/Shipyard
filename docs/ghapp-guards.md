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
| removed for `failed_checks` / `merge_conflict`, same head | refuse: under ALLGREEN a same-head re-enqueue fails its batch-mates; push a fix first |
| removed for any other reason (`manual`, ...), same head | refuse: confirm with whoever dequeued it |
| merged / closed | refuse |
| unreadable, or a truncated timeline window that shows no removal and no new head | refuse |

A GraphQL body read from stdin (`--input -`, `query=@-`) cannot be inspected
and is refused as ambiguous.

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

- a same-head re-enqueue after `failed_checks` or `merge_conflict` is refused.
  This is the intended refusal. It includes the older merge steward's
  queue-priority recovery, which re-enqueued the same head after a
  `failed_checks` it attributed to infrastructure; that recovery resumes once
  the host runs a marked binary.
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
