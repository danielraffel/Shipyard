# Superseded pull requests: detection (shipped, shadow) and closing (proposed)

## Why

A pull request whose change already landed through another pull request sits
open until a person notices. On 2026-11-11 `danielraffel/fleet-ssh#15` stayed
open for hours, and an agent waited for the owner to merge it. Meanwhile #16
had merged byte-identical content (`fleet/advertised-labels.json` on main had
#15's blob `e2eb39ec`). Each such pull request costs a reviewer pass, a
steward row and, worst of all, an agent session that believes it is blocked.

## Detection (shipped, shadow mode)

`src/app/merge_steward_cmd/superseded.rs` adds a `superseded` object to a row
of the steward tick JSON only on deterministic evidence:

| `kind` | Evidence |
|---|---|
| `contained` | `compare <base>...<head>` is `ahead_by` 0: every commit of the head is on the base |
| `tree_equal` | every path the head changes has the head's blob on the base, every path it removes is gone, every rename's old path is gone (one REST three-dot compare `<base>...<head>` for the changed paths and their head blobs, then one GraphQL read of those paths' blobs on the base tip; a list of 300 or more files proves nothing) |
| `named_by` | a merged pull request in the same repository says `Supersedes #<n>` (GitHub cross-reference) |
| `label_successor` | labelled `superseded`, its body or a comment names `superseded by #<m>`, and `#<m>` merged |

Titles, similar diffs and authorship are never evidence. `blocked_on` and
`next_action` are unchanged. The object carries `action`, which reads "close as
superseded by #M (shadow: Shipyard does not close it)".

A pull request that is merged, in the merge queue, or has an
`added_to_merge_queue` event with no later removal is never judged. A head the
queue is landing reads `contained` for a moment before GitHub marks it merged:
pulp#12014 was queued at 07:35:47Z, removed at 08:04:26Z and merged at
08:04:27Z. `named_by` and `label_successor` need the successor merged into the
same base, ignore reverts and qualified wording ("partly supersedes", "not
superseded by"), and read a successor name only from the pull request's body
or a repository member's comment. An absent path counts as absent only when
the same read resolved the base and its root tree. Status `changed` (mode
only), `copied` and `unchanged` prove nothing.

Each row's evidence also carries `closeable`, which is true only for tree
evidence on a pull request that a fleet identity (a GitHub App) opened and
whose body closes no issue (`Fixes`/`Closes`/`Resolves #n`). Otherwise it
carries `not_closeable` with the reason. Detection still reports such pull
requests; a future close would never act on them.

Cost: each open head is checked at most once per 30 minutes, sooner only when
its head moves. A failed read is recorded as `unknown`, never as "not
superseded", so a blocked backlog costs nothing on the passes in between. Per
head and recheck there is one compare, and, unless the head is `contained`,
one small GraphQL blob read. Each head also has a share of one GraphQL mention
read per 10 heads. That comes to about two calls per open head per 30 minutes,
at most 25 heads per repository per pass. The pass budget is checked before
each read. The first rate-limit error stops the detector and is reported in
the repository's `errors`, which starts the pass's rate-limit cooldown. Only
`--apply` passes run it, because they persist the cache.
`SHIPYARD_STEWARD_SUPERSEDED=off` turns detection off.

`Superseded` is one condition the steward derives per head, beside its other
conditions such as review awaiting and owner active. New behavior should read
it from the tick row rather than re-derive it.

## Closing (proposed: owner approval required; not implemented)

This is a new write capability, so it is not in this change. The proposal is
one narrow capability, **close-superseded**, reviewed by Fable as APPROVE WITH
CHANGES under these conditions.

1. **Evidence:** only `contained` and `tree_equal` with `closeable: true` may
   close. `named_by` and `label_successor` never close; they are reported only.
2. **Proof at close time:** every close goes through the existing
   `pr-close-guard` (`scripts/ghapp_pr_close_guard.py`). The guard re-proves
   containment from Git tree entries (mode, type and blob) on the live base
   and head, and refuses if either moved during the proof. No override
   (`GHAPP_ALLOW_UNINTEGRATED_PR_CLOSE`) is ever used. If the guard is not
   installed, nothing closes.
3. **Never closed:**
   - pull requests labelled `owner-decision`, or with a HOLD at the head;
   - queued or armed pull requests, and stacked ones (base is not the default branch);
   - heads that moved in the last 2 hours;
   - pull requests with any steward act or repair claim in the last 2 hours;
   - pull requests whose body closes an issue, or that a non-fleet author opened.
   The same evidence must be seen for the same head on two passes at least 30
   minutes apart.
4. **Where:** only on the `mutation_machine` host (m3), through the Shipyard App identity.
5. **Limits:**
   - 3 closes per repository per pass, 10 per repository per day, and 20 fleet-wide per day;
   - one reopen within 7 days pauses closing for that repository;
   - a reopened head is `do-not-close` forever.
6. **Fixed comment, posted before the close:**

   ```
   Closed as superseded (shipyard steward, close-superseded).
   Evidence: <kind> against <base>@<base_sha>: <paths@blobs> [successor #M]
   Head <head_sha> is unchanged and its branch is kept; reopen to restore it,
   and the steward will not close this head again.
   ```

   The head branch is never deleted.
7. **Audit:** each close writes the ledger audit record (repository, pull
   request, head, `base_sha`, evidence, paths@blobs, successor, comment id,
   host, session), and the guard writes its own `pr-closes.jsonl` line. The
   tick names it under `mutation` as `closed_superseded`.
8. **Opt-in:** `[steward] close_superseded = true` in the base's
   `.shipyard/config.toml`, off by default. It is enabled first on fleet-ssh and
   fleet-handbook, after 7 days of shadow detection with zero false positives.
   `SHIPYARD_STEWARD_SUPERSEDED_CLOSE=off` is the kill switch.
9. **Wake the blocked:** a declared blocker on that pull request is reported as
   superseded, and the session is woken through the owner relay
   ([owner-relay.md](owner-relay.md)) with the successor's number.
10. **Retirement:** once close-superseded lands, the raw-API close path that
    fleet-handbook #53 documents for agents is retired. Closing superseded
    pull requests becomes the steward's alone.

**Owner-only, always:** closing on text evidence, closing `owner-decision` or
held pull requests, the unintegrated-close override, deleting a head branch,
and enabling the capability per repository.

### Where it sits in the privileged grammar

`docs/ghapp-guards.md` lists the guard family. Closing a pull request is
already governed by `pr-close-guard`, which allows a close only for a head
contained by its live base. close-superseded adds no guard bypass. It adds a
steward write path that calls the same close, so the guard judges it like any
other close. The rollup reconciler's `close_landed_members` already works this
way for landed rollup members.

## Prior art

- **Kubernetes triage-robot (formerly fejta-bot):** the lifecycle runs stale,
  then rotten, then closed. Every step posts a fixed, explanatory comment, and
  `/reopen` undoes the close. Adapted: a fixed comment and reopen as the undo.
  Not adopted: closing on inactivity alone, which is a heuristic. Here only
  content evidence closes.
- **Mergify `close` actions:** a declarative condition closes with a templated
  message. Adapted: the condition is declarative (evidence kind plus two
  observations), and the message is a template.
- **Renovate / Dependabot:** they close their own pull request when a newer one
  supersedes it, with a comment naming the successor. Adapted: name the
  successor and the evidence. They act only on pull requests they authored;
  this applies to any author, so it requires tree proof rather than authorship.
