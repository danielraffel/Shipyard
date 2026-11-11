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
| `tree_equal` | every path the head changes has the head's blob on the base, every path it removes is gone, every rename's old path is gone (two three-dot compares from the shared merge base; a list of 300 or more files proves nothing) |
| `named_by` | a merged pull request in the same repository says `Supersedes #<n>` (GitHub cross-reference) |
| `label_successor` | labelled `superseded`, its body or a comment names `superseded by #<m>`, and `#<m>` merged |

Titles, similar diffs and authorship are never evidence. `blocked_on` and
`next_action` are unchanged. The object carries `action`, which reads "close as
superseded by #M (shadow: Shipyard does not close it)".

Cost: each open head is checked once, then again only when the head moves or
30 minutes have passed, whatever the result, so a blocked backlog costs nothing
on the passes in between. A pass reads at most 25 heads per repository: one
compare and one small blob read (GraphQL `object(expression: "<base>:<path>")`)
each, plus one batched GraphQL read of mentions. Only `--apply` passes run it,
because they persist the ledger cache. A pass that deferred rows for its budget
reads nothing. `SHIPYARD_STEWARD_SUPERSEDED=off` turns detection off.

A failed read records `unknown`, never "not superseded". If the mention read
failed and the tree shows nothing, the result is unknown too, so a merged
`Supersedes #n` cannot be missed silently.

## Closing (proposed: needs the owner's approval)

This is a new write capability, so it is not in this change. The proposal is
one narrow capability, **close-superseded**, and nothing broader.

1. **Who:** the steward, through the Shipyard App identity, on an `--apply`
   pass. It never acts as an agent's own token.
2. **When:** the row has carried the same `superseded` evidence for the same
   head on two passes at least 30 minutes apart, the pull request is not
   queued or armed, and its head has not moved in the last 2 hours. The
   evidence `kind` must be `contained` or `tree_equal`. `named_by` and
   `label_successor` alone never close; they are reported only. The tree is
   the one fact that makes closing lossless.
3. **Proof at close time:** the close goes through the existing
   `pr-close-guard` (`scripts/ghapp_pr_close_guard.py`). It re-reads the live
   base and head, proves containment from Git tree entries (mode, type and
   blob), and refuses if the head or base moved during the proof. Detection is
   advisory; the guard is the gate. No guard override (no
   `GHAPP_ALLOW_UNINTEGRATED_PR_CLOSE`).
4. **Fixed comment, posted before the close:**

   ```
   Closed as superseded (shipyard steward, close-superseded).
   Evidence: <kind> against <base>@<base_sha>: <paths@blobs> [successor #M]
   Head <head_sha> is unchanged; reopen to restore it ("Reopen pull request"),
   and the steward will not close this head again.
   ```

   Reopening is the undo. A reopened pull request's head is written to the
   ledger as `do-not-close`, and the steward never closes that head again.
5. **Audit:** each close appends a ledger audit record (`LedgerAudit`) with the
   repository, pull request, head, base SHA, evidence and comment id. The
   steward's tick JSON names it under `mutation` as `closed_superseded`.
6. **Limits:** at most 3 closes per repository per pass, and 10 per day per
   repository. Repositories opt in through `[steward] close_superseded = true`
   in the base's `.shipyard/config.toml`, off by default. There is a kill
   switch (`SHIPYARD_STEWARD_SUPERSEDED_CLOSE=off`).
7. **Wake the blocked:** the close is a pull-request state change, so any
   session whose declared blocker is that pull request is woken through the
   owner relay ([owner-relay.md](owner-relay.md)) with the successor's number.
   A blocker declared on a superseded pull request is also reported as
   superseded before the close.

### Where it sits in the privileged grammar

`docs/ghapp-guards.md` lists the guard family. Closing a pull request is
already governed there by `pr-close-guard`, which allows a close only for a
head contained by its live base. close-superseded adds no new guard bypass. It
adds a steward write path (as the rollup reconciler's `close_landed_members`
already does for landed rollup members) that calls the same close, so the
guard judges it like any other close. The owner approves:

- the capability (the steward may close, under the rules above);
- the per-repository opt-in in each base config;
- the evidence kinds that may close (proposed: `contained` and `tree_equal` only).

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
