# Push claim fence

A repair lane announces work on a pull request with a comment line:

```text
repair-claim: <owner> [(<note>)] until <UTC time>
```

The fence refuses a push to that pull request's branch while another owner's
claim is live, so two sessions do not repair the same pull request at once.

## What is fenced

| Path | Fenced | How |
|---|---|---|
| `shipyard pr` / `shipyard ship` push | yes | checked in process before the push |
| `git push` in a repository whose pre-push hook calls `shipyard claim check-push "$1" "$2"` with the hook's stdin | yes | the hook exits 1 on a refusal |
| Shipyard's own `.githooks/pre-push` | yes | it carries that line |
| `git push` in a repository whose hook does not (Pulp and Vellum until their own PRs add it) | no | |
| `git push --no-verify` | no | git skips the hook |

Agent git pushes over SSH with the owner's key, never through the fleet App, so
no `ghapp` guard or server-side rule sees a push. The fence is a client-side
coordination check, not a security boundary: every claim comment posts as the
same App, so an owner is only a name.

## How a claim is read

The comments on each open pull request whose head is the pushed branch are read
oldest first. Only a trusted author counts (repository owner, member,
collaborator, or a bot). Per line:

- `repair-claim: <owner> [(<note>)] until <time>` claims for `<owner>`. The time
  is RFC 3339 or the minute form `2026-11-16T22:00Z`. A time that parses as
  neither counts as live.
- `repair-claim-released: <owner>` ends that owner's earlier claim.
- `repair-claim-taken: <new> from <old> reason=...` moves the old owner's
  earlier claim, with its expiry, to the new owner.

A claim posted after a release or a take is live again. This grammar has one
parser, `crate::work_claim`, shared by the relay, `shipyard advance` and the
fence.

## Who you are

`SHIPYARD_CLAIM_OWNER` is the owner name you claimed with. A live claim by
that name does not block you; when it is unset, every live claim does. The
refusal says both ways forward:

```text
claim-fence: set SHIPYARD_CLAIM_OWNER=<owner> if this is your claim, or take it over with `shipyard claim take --repo <repo> --pr <n> --from <owner> --reason "..."`, then push again.
```

The owner is not inferred from the session: no environment variable carries an
agent's team name or cmux tab title (cmux exports only UUIDs, and its title
needs a socket call), and the Claude session id is shared by every teammate of
one team, so matching it would let a teammate pass its lead's claim.

## Taking a claim

`shipyard claim take --from <owner> --reason "..." [--pr N] [--repo OWNER/REPO]`
needs `SHIPYARD_CLAIM_OWNER`, checks that `<owner>` holds a live claim, posts
`repair-claim-taken: <you> from <owner> reason=...` through `ghapp`, and checks
the posted body. Push again afterwards; the fence then reads the claim as yours.

## When GitHub cannot be read

One read is retried once, two seconds later. If it still fails, the fence
prints `claim-fence: could not read claims (<reason>); pushing without the
check` and lets the push go. This differs from the autonomy contract's A11
(fail closed before a mutation) on purpose: the fence guards coordination, not
policy, `--no-verify` already bypasses it, and failing closed would block every
repair push during a GitHub incident for no protection. A host whose `shipyard`
predates `claim check-push` passes the hook the same way, with a note.

Reads go through `ghapp` for `danielraffel/*` and `ghread` elsewhere (`ghapp`
when `ghread` is missing or does not serve the repository). A branch with no
open pull request is not fenced, so the first push of a new branch is one read.

## Naming the pusher

`ghapp`'s `verdict-session-stamp` guard adds `<!-- session: <host>/<session> -->`
above the last non-blank line of any comment with a `repaired:` or `repair-claim-taken:`
line, so the next push made under someone else's claim names its session.
Commits carry no session trailer: `shipyard pr` does not create the commits it
pushes, and a pre-push hook cannot add a trailer without changing the SHA.

## Rollback

`SHIPYARD_CLAIM_FENCE=off` turns the fence off for that process (it prints that
it did). Removing the hook line turns it off for a repository.
