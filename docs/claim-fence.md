# Push claim fence

A repair lane announces work on a pull request with a comment line:

```text
repair-claim: <owner> [(<note>)] until <UTC time>
```

The fence refuses a push to that pull request's branch while another owner's
claim is live, so two sessions do not repair the same pull request at once.

## Off by default

The fence runs only with `SHIPYARD_CLAIM_FENCE=on`; unset, or any other value,
is off, and the fence reads nothing. This follows the default-off rollout rule
in `docs/autonomy-contract.md`. Turn it on in this order:

1. The lanes export `SHIPYARD_CLAIM_OWNER` set to the name they write in their
   own `repair-claim:` lines (fleet-pass posts `repair-claim: $SIGNER/$TAG`).
   Without it, a lane's own claim refuses its own `shipyard pr` push.
2. The lanes post a `repair-claim-taken:` (`shipyard claim take`) before they
   repair a STEWARD-ESCALATED or OWNER-EXCLUSION-LAPSED entry, which the brief
   tells them to repair over an owner's live claim.
3. Hosts set `SHIPYARD_CLAIM_FENCE=on`.

`SHIPYARD_CLAIM_FENCE` is an owner and incident switch. Agents never set it,
the same rule as `GHAPP_ALLOW_*`; a refused agent takes the claim instead.
The refusal text names no switch.

## What is fenced

| Path | Fenced | How |
|---|---|---|
| `shipyard pr` / `shipyard ship` push | yes | checked in process before the push |
| `git push` in a repository whose pre-push hook calls `shipyard claim check-push "$1" "$2"` with the hook's stdin, and whose clone sets `core.hooksPath` | yes | the hook exits 1 on a refusal ([every worktree](#making-a-repositorys-hook-run-in-every-worktree)) |
| `shipyard claim check-push origin <url> --branch <b> && git push ...` | yes | [without a hook](#checking-before-a-push-without-a-hook) |
| Shipyard's own `.githooks/pre-push` | yes | it carries the hook line |
| `git push` in a repository whose hook does not (Pulp and Vellum until their own PRs add it) | no | |
| `git push --no-verify` | no | git skips the hook |

Agent git pushes over SSH with the owner's key, never through the fleet App, so
no `ghapp` guard or server-side rule sees a push. The fence is a client-side
coordination check, not a security boundary: every claim comment posts as the
same App, so an owner is only a name.

`shipyard pr` checks once and hands its push `SHIPYARD_CLAIM_FENCE_DONE=<head>`;
the hook skips a push whose every ref pushes that exact head, so one push is not
checked twice. It is internal to that hand-off.

## Making a repository's hook run, in every worktree

A pre-push hook fences a push only when git runs it. For a repository that
opts in (Vellum and Pulp in their own pull requests):

1. Its `.githooks/pre-push` calls the fence before its other gates, passing the
   hook's stdin through:

   ```sh
   push_refs=""
   [ -t 0 ] || push_refs="$(cat)"
   if command -v shipyard >/dev/null 2>&1; then
       printf '%s\n' "$push_refs" | shipyard claim check-push "${1:-}" "${2:-}"
       case $? in 0) ;; 1) exit 1 ;; *) [ "${SHIPYARD_CLAIM_FENCE:-}" != on ] || echo "claim-fence: could not run; pushing without the check" >&2 ;; esac
   elif [ "${SHIPYARD_CLAIM_FENCE:-}" = on ]; then
       echo "claim-fence: shipyard is not on PATH; pushing without the check" >&2
   fi
   ```

   A hook that reads stdin for its own purposes must use `$push_refs`
   afterwards, since stdin can be read once.
2. The clone sets `core.hooksPath` to the relative path `.githooks`, once:
   `git config core.hooksPath .githooks` (Shipyard's `scripts/install-githooks.sh`
   does exactly this). It is stored in the clone's shared `.git/config`, so every
   worktree of that clone, including a lane worktree added later with
   `git worktree add`, runs the hook from its own checkout's `.githooks/`. Git
   resolves a relative `core.hooksPath` against the worktree being pushed from.

It does not cover:

- a branch checked out from before the hook gained the fence line (the hook
  runs from the worktree's own checkout);
- a clone where `core.hooksPath` was never set (git's default `.git/hooks` has
  no such hook), or where a worktree's own `config.worktree` sets a different
  `core.hooksPath` (`shipyard claim post` turns on `extensions.worktreeConfig`
  but writes only its claim record there);
- `git push --no-verify`.

Check a worktree with `git config core.hooksPath` (expect `.githooks`) and
`test -x .githooks/pre-push`.

## Checking before a push, without a hook

A lane that pushes with raw `git push` can call the fence itself first:

```sh
shipyard claim check-push origin "$(git remote get-url origin)" --branch "$BRANCH" && git push origin "$BRANCH"
```

`--branch` (repeatable) replaces the hook's stdin. Exit 1 is a refusal; exit 0
covers allowed, off, and GitHub unreadable (the last prints why).

## What passes without a check

With the fence on, each of these lets the push through and prints why:
GitHub unreadable within the deadline, a remote URL that is not GitHub
(`claim-fence: <url> is not a GitHub remote; not checked`; an SSH host alias
or a typo lands here), a
`shipyard` that predates `claim check-push`, and no `shipyard` on PATH (the
hook prints that one). With the fence off, all of them are silent.

## How a claim is read

The comments on each open pull request whose head is the pushed branch are read
oldest first. Only a trusted author counts (repository owner, member,
collaborator, or a bot). Per line:

- `repair-claim: <owner> [(<note>)] until <time>` claims for `<owner>`.
- `repair-claim-released: <owner>` ends that owner's earlier claim; a bare
  `repair-claim-released:` ends every earlier claim (as fleet-handbook's
  `repair_preclaim.py` reads it).
- `repair-claim-taken: <new> from <old> reason=...` moves the old owner's
  earlier claim, with its expiry, to the new owner.

A claim posted after a release or a take is live again.

The time is read in every spelling `repair_preclaim.py` reads, plus a few:
`2026-11-16T22:00Z`, a space instead of the `T`, optional seconds, a `Z`,
` UTC` or `+HH:MM` suffix or none (read as UTC), and a trailing period. One
difference remains on preclaim's side: it does not read a claim with a
`(<note>)`, which this does (the Generous-Corp/vellum#462 claim had one).

A time that reads as none of these does not block forever: for the fence it
ends four hours after its comment was posted, and the fence prints a warning
naming the comment. `shipyard advance` keeps its earlier rule, live with no
end, because a retrigger over a live claim is the riskier mistake there.

This grammar has one parser, `crate::work_claim`. The relay, `shipyard
advance` and the fence all read claims through it, and all three honour
releases and takes.

## Who you are

A session proves a claim is its own in one of two ways.

### Post the claim with `shipyard claim post` (any session)

```sh
shipyard claim post --pr 462 --owner lane-a --until 2h      # or --until 2026-11-27T22:00Z
# ... repair, then push as usual; the fence counts that claim as yours
shipyard claim release --pr 462
```

`post` posts `repair-claim: <owner> until <YYYY-MM-DDTHH:MM:SSZ>` through
`ghapp`, checks the posted body, and writes one record to this worktree's own
git config (`config.worktree`, turning on `extensions.worktreeConfig` if it is
off; as `git worktree add` does, it first moves a shared `core.bare=true` or
`core.worktree` to the main worktree's `config.worktree`, so a bare clone's
worktrees keep working):

```text
shipyard.claim.<owner>/<repo>#<pr>.record = <owner> <until> <comment-id>
```

The slug is lowercase. Git splits a config name at its first and last dot, so
the trailing `.record` is what makes it a valid name; a repository name with
dots still reads back.

`post` refuses while another owner holds a live claim on the pull request
(take it instead), refuses an `--until` in the past or more than 24h away, and
takes `--owner` only as `name` or `name/tag` of letters, digits and `._-`, the
grammar Pulp's `prepush_claim_check.py` reads. The time is always written with
seconds and a `Z`, which both checkers read; Pulp reads `+00:00` as an
unreadable time, which it counts as live for ever. Posting again under the
same owner renews the claim and replaces the record.

The fence counts a claim as yours only when all of these hold:

- the record is in this worktree's own `config.worktree` (a record in the
  clone's shared `.git/config`, a global config or a `-c` does not count, so
  one worktree's claim never covers another worktree of the same clone);
- it names this exact repository and pull request, and the pull request is the
  open one whose head is the pushed branch;
- a trusted comment with that id holds a live claim by that owner: not
  expired, not released, not taken since. A later claim under the same owner
  name is another comment, so it does not match.

Every other live claim on the pull request still refuses the push. A record
that matches no live claim prints a note naming it and counts for nothing.

`release` reads the pull request first. It posts `repair-claim-released:
<owner>` only while the record's comment still holds the claim; a stale record
(expired, released, taken, or the name claimed again by another session) posts
nothing, since a release by name would end that other session's claim. Either
way it removes the record.

A record lives as long as the worktree. A later session reusing the same
worktree inherits it until the claim expires (24h at most), so release a claim
when the work is done.

This is a coordination fence, not a trust boundary. The same user could write
the record by hand, as they could set `SHIPYARD_CLAIM_OWNER`; the record is
no weaker than that variable, and it is state the claim command produces, not
an override.

### Name the owner with `SHIPYARD_CLAIM_OWNER` (the fleet runner)

`SHIPYARD_CLAIM_OWNER` is the owner name you claimed with. A live claim by that
name does not block you; when it is unset and no record matches, every live
claim does. The fleet runner exports it; other sessions post their claim
instead.

The owner is not inferred from the session: no environment variable carries an
agent's team name or cmux tab title (cmux exports only UUIDs, and its title
needs a socket call), and the Claude session id is shared by every teammate of
one team, so matching it would let a teammate pass its lead's claim.

## Taking a claim

The refusal names one way forward:

```text
claim-fence: to take it over, run `shipyard claim take --repo <repo> --pr <n> --from <owner> --reason "..."`, naming the owner exactly as written (owner names can collide), then push again.
```

`shipyard claim take` needs `SHIPYARD_CLAIM_OWNER`, checks that `<owner>` holds
a live claim, posts `repair-claim-taken: <you> from <owner> reason=...` through
`ghapp`, and checks the posted body. Push again afterwards; the fence then reads
the claim as yours.

## When GitHub cannot be read

The whole check has one 15-second deadline. Each read gets what is left of it;
a failed read is retried once, two seconds later, if time remains. A `ghread`
timeout is not retried through `ghapp`. Past the deadline, or after the retry
fails, the fence prints `claim-fence: could not read claims (<reason>); pushing
without the check` and lets the push go.

This differs from the autonomy contract's A11 (fail closed before a mutation)
on purpose: the fence guards coordination, not policy, `--no-verify` already
bypasses it, and failing closed would block every repair push during a GitHub
incident for no protection.

Reads go through `ghapp` for `danielraffel/*` and `ghread` elsewhere (`ghapp`
when `ghread` is missing or does not serve the repository). A branch with no
open pull request is not fenced, so the first push of a new branch is one read.

## Naming the session

`ghapp`'s `verdict-session-stamp` guard stamps a comment with a
`repair-claim-taken:` line with this session, above its last non-blank line, so
a take names who took the claim.

`repaired:` comments are not stamped here. A stamp there wakes two dormant
readers in fleet-handbook's `must-review.py`, `owner_activity`'s
`fleet_pushed` and `adversary_pushed`, and changes `shipyard who --contact`'s
last stamped actor. That change gets its own follow-up.

Commits carry no session trailer: `shipyard pr` does not create the commits it
pushes, and a pre-push hook cannot add a trailer without changing the SHA.

## Rollback

Unset `SHIPYARD_CLAIM_FENCE` (or set it to anything but `on`). Removing the
hook line turns the fence off for a repository. Both are for the owner and
incidents only.
