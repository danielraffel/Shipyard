# Real GitHub responses: merge-queue PR states

Every JSON file in this directory except `expected_classifications.json` is an
**unedited response captured from the live GitHub API for
`Generous-Corp/pulp`** on **2026-09-22** (between 14:23 and 14:38 PDT). None of
them is synthetic. They exist because the synthetic fixtures elsewhere in
`tests/fixtures/` encode what we *believed* GitHub returns, and the belief was
wrong in at least one way that mattered (see "The trap" below).

## Files

| File | PR | Captured state |
|---|---|---|
| `pr_queued.json` | 8669 | In the merge queue, position 1. `autoMergeRequest` is `null`. |
| `pr_never_armed.json` | 8672 | Open, never armed, never queued. |
| `pr_armed_not_queued.json` | 8678 | Auto-merge enabled, not yet in the queue. |
| `pr_ejected_requeued.json` | 8702 | Added, Removed(`failed_checks`), Added again with **no commit between**: a re-enqueue of the same head, which under `ALLGREEN` grouping fails its batch-mates. Currently queued at position 3. |
| `pr_ejected_history.json` | 8638 | One `merge_conflict` removal fixed by a commit, then four `failed_checks` removals each followed by a re-add with no commit between. Later a new commit and a fresh `AutoMergeEnabledEvent`: currently armed after a new head, which is legitimate. |
| `pr_merged.json` | 8721 | `MergedEvent` then `RemovedFromMergeQueueEvent(reason: merged)`. The removal is **not** an ejection. |
| `rest_pull_queued.json` | 8669 | `GET repos/Generous-Corp/pulp/pulls/8669` while the PR was queued. `auto_merge` is `null`. |
| `ruleset_merge_queue.json` | - | `GET repos/Generous-Corp/pulp/rulesets/19431100` (`MERGE`, `ALLGREEN`, merge 5, build 3). |
| `classic_protection.json` | - | `GET repos/Generous-Corp/pulp/branches/main/protection`. It has no field that can express a merge queue. |

`expected_classifications.json` is the hand-verified ground truth for the
`pr_*.json` files. Both classifier implementations assert against it:

- Rust: `src/pr_queue_state.rs` (`shipyard landing --pr`)
- Python: `scripts/ghapp_queue_arm_guard.py` (the `ghapp` queue-arm guard)

A disagreement between them fails one of the two test suites.

## Source query

The `pr_*.json` files are shaped by this GraphQL query (variables
`owner`, `name`, `number`). Re-running it against PR 8721 on the capture date
reproduced `pr_merged.json` byte-for-byte apart from `pageInfo`, which the
capture did not request:

```graphql
query($owner:String!,$name:String!,$number:Int!){
  repository(owner:$owner,name:$name){
    pullRequest(number:$number){
      number state headRefOid isInMergeQueue
      mergeQueueEntry{state position}
      autoMergeRequest{enabledAt}
      timelineItems(last:100,itemTypes:[PULL_REQUEST_COMMIT,HEAD_REF_FORCE_PUSHED_EVENT,
          ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT,
          AUTO_MERGE_ENABLED_EVENT,AUTO_MERGE_DISABLED_EVENT,MERGED_EVENT]){
        pageInfo{hasPreviousPage}
        nodes{__typename
          ... on PullRequestCommit{commit{oid pushedDate}}
          ... on HeadRefForcePushedEvent{createdAt afterCommit{oid}}
          ... on AddedToMergeQueueEvent{createdAt actor{login}}
          ... on RemovedFromMergeQueueEvent{createdAt reason actor{login}}
          ... on AutoMergeEnabledEvent{createdAt actor{login}}
          ... on AutoMergeDisabledEvent{createdAt reason actor{login}}
          ... on MergedEvent{createdAt}}}}}}
```

## The trap these files pin

`REST pulls/<n>.auto_merge` (and GraphQL `autoMergeRequest`) is `null` for
**every** queued PR: GitHub consumes the auto-merge request when it enqueues the
PR, and emits no `AutoMergeDisabledEvent` when it does. A reader that treats
`auto_merge == null` as "unarmed" will re-arm a queued PR. Queue membership is
`isInMergeQueue` / `mergeQueueEntry`, nothing else.

## Other facts learned from these captures

- Every queue mutation is attributed to the same App actor
  (`shipyard-local`), whether Shipyard or an agent driving `ghapp` issued it.
  Actor identity cannot distinguish them.
- `RemovedFromMergeQueueEvent.beforeCommit` is unreliable for "was a new head
  pushed after the ejection"; the classifier instead looks for a
  `PullRequestCommit` or `HeadRefForcePushedEvent` item **after** the removal in
  timeline order.
- `timelineItems(last: 100)` is a window, not the whole history. When
  `pageInfo.hasPreviousPage` is `true` the counts derived from it are lower
  bounds; the classifier reports that rather than hiding it.
