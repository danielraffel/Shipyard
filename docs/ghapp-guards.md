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
