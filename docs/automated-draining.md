# Draining pull requests automatically

If coding agents open more pull requests than you can shepherd by hand, this is
how to get them reviewed, tested, and merged without anyone babysitting the
queue. GitHub still decides what merges. Shipyard decides *when a pull request
is ready to be handed to GitHub*. TartCI supplies the machines the tests run on.

## The idea in one picture

```text
  agent opens a PR ──► CI runs on the exact head ──► reviewer posts APPROVE or HOLD
                         │                                  │
                         │ (TartCI VMs on your Macs,        │ HOLD: an agent fixes it,
                         │  or GitHub-hosted runners)        │ pushes, and it goes round again
                         ▼                                  ▼
              Shipyard steward, every few minutes: "approved at this exact head
              and green?"  ── yes ──► hands it to GitHub to merge
                                          │
                                          ├─ repo has a merge queue: enqueue it,
                                          │  GitHub tests and merges in batches
                                          └─ no merge queue: merge that exact head,
                                             GitHub still enforces the required checks
```

Nobody arms, enqueues, or merges by hand. Each step has one owner, and each
owner only acts on evidence about the exact commit in front of it.

## Who does what

- **GitHub** owns pull requests, required checks, the merge queue, and the
  merge itself. Nothing here works around it.
- **Shipyard's steward** (`shipyard runner steward --apply`) is the only
  thing that admits work. It reads every open pull request. For each one
  that has been handed to it (or was opened by the fleet App) and whose
  current head has an approving verdict and green required checks, it either
  adds it to the merge queue or merges that exact head. It checks the approval
  first, then re-reads the pull request live immediately before acting, and
  does nothing if anything is unclear.
- **TartCI** turns Apple Silicon Macs into GitHub Actions capacity. Each job
  gets a fresh macOS, Linux, or Windows VM that is thrown away afterwards, with
  build caches kept warm on the host and a budget so CI doesn't starve the
  machine. [Tart](https://github.com/openai/tart) provides the VMs; TartCI schedules them.
- **GitHub-hosted runners** cover whatever you'd rather not run yourself, or
  run when your Macs are busy. Shipyard doesn't care where a check ran, only
  that it passed on the exact head.
- **Agents** write code, open pull requests, and fix the ones that get held.
  A *reviewer* agent posts verdicts. A *repair* agent picks up holds. Neither
  one merges anything.

## What runs, and when

Four timed jobs on one machine. Nothing else has to be running.

| Job | How often | What it does |
|---|---|---|
| **Steward** (Shipyard CLI, no AI) | every 5 to 10 minutes | Admits approved, green exact heads (queue or exact-head merge). Writes one summary line per repository, with the oldest stuck pull requests and why. |
| **Reviewer pass** | every 15 minutes or so, and on demand | A script computes the must-review queue (see below) and starts a one-shot AI session, through a pooled router such as Subrouter that falls back to another provider when one runs out. It gives the session the written reviewer brief and that queue. The session posts APPROVE or HOLD on each queued pull request in order, then exits. |
| **Eligibility tick** (no AI) | every 2 minutes | Recomputes the must-review queue. When a pull request has just become reviewable, it starts a reviewer pass now. If a pass is already running, it queues exactly one follow-up, which re-checks the queue and is skipped when nothing is left. |
| **Repair pass** | every 15 minutes or so | The same pattern: fixes one held or failing pull request, pushes once, exits. |

**The reviewer never picks its own work.** Before each pass, a script lists
every open, non-draft, unlabelled pull request whose required checks are green
and which has no verdict naming its current head. That list is the must-review
queue. The pass works through it in order. Anything still without a verdict
afterwards is logged as missed and goes first next time. A pull request
therefore keeps coming back until a verdict names its exact head; a new push
resets that. The 2-minute tick cuts pickup to about 2 minutes from the moment
checks go green, and the 15-minute timer remains as the backstop.

**No agent session needs to stay open.** Shipyard and TartCI never start AI
sessions themselves; the timer script does. GitHub runs CI (on TartCI VMs or
hosted runners) and does the merge. If the machine is off, draining pauses and
picks up again when it returns; nothing unsafe merges in the meantime.

## How a pull request moves

1. An agent opens a pull request (`shipyard pr --no-arm`, or any normal way)
   and stops.
2. CI runs on the pull request's head: on TartCI VMs, GitHub-hosted runners,
   or both.
3. Once the required checks are green, a reviewer reads the change and posts
   a verdict naming the exact commit: APPROVE, or HOLD with what to fix.
4. On a HOLD, a repair agent fixes what was named, adds a test, merges the
   base branch in (never a rebase), and pushes once. A new head means a fresh
   review. A HOLD locks its exact head: once a counted reviewer holds it, no
   approval of that head counts, from anyone, before or after. Only a new push
   can be approved. (A second reviewer session once approved a held head and it
   merged; Shipyard now refuses that.)
5. On the next steward pass (every five minutes is plenty), the approved head
   is handed to GitHub. A merge queue tests it with up to four others and
   merges the batch. Without a queue, Shipyard merges that exact head, and
   GitHub refuses unless the required checks pass.

A HOLD is only as strong as nobody stacking on it, so the steward also refuses
a head that carries another open pull request's unapproved commits. Before it
enqueues or merges pull request P, it reads P's commits that are not on the
base (the compare API `base...head`) and, once per pass, the commits of every
other open pull request in the repository. If P carries any commit of a pull
request Q whose current head is not approved (a counted HOLD, or no counted
APPROVE), P is refused with `blocked_on=carries_unapproved_commits:#Q`, and
`blocked_detail` names the shared commits and why Q is not approved. Commits
already on the base never count, nor does a shared commit whose change the base tip
already has under other shas (a non-merge commit with the patch-id of a
single-parent tip commit, or a merge whose every path differing from its first
parent has an identical mode, type and blob on the tip; proven by the steward
against the tip, all shared commits must qualify, any doubt or read error keeps
the refusal), nor does Q when it is P's own base-branch
pull request, is stacked on P (following its base branch through other open
pull requests' head branches reaches P's head branch), only inherits the
commit (a commit counts as Q's only if it is in Q's own diff against Q's own
base branch), merged P's branch without being stacked on it, or is a Shipyard
rollup that merged P's head. A shared commit counts against P only when it is
on Q's first-parent chain (`Q.base..Q.head` followed through first parents) or
is not on P's own first-parent chain (`base..P.head`): a commit on P's
first-parent chain that reached Q only through a merge's second parent is P's
own. When every shared commit is P's own, Q does not hold P, and a refusal P
still gets from another pull request ends with `#Q merged #P's branch; not
carried`. P merging held work in (the held commits are P's second parent) and
P built straight on commits on Q's own first-parent chain (they are on both
chains) are still refused, and a first-parent read that fails or is over the
cap keeps the overlap. Q's own diff and its first-parent chain come from one
read of `Q.base...Q.head`. Named residual: P built straight on commits that a
held Q itself took in through a merge's second parent (Q = merge(Q.base, X)
with P on X, or Q's tip re-hung as merge(Q.base, old tip)) is cleared, because
the commit graph cannot tell that from Q having merged P's branch; X then
lands under P's own approval, so P's reviewer reviews X as part of P's diff. Land or
approve Q first, or drop its commits from P; a branch built on another pull
request should target that pull request's branch. At most 250 commits are read
per pull request. A Q whose listing is larger (for example a stacked pull
request whose listing reaches back past its base) is not refused on that count:
the steward reads Q's own diff against Q's own base (`Q.base...Q.head`, paged),
and the cap applies to that. If Q's own diff is still over 250 commits, an
approved Q is ignored, and for an unapproved Q each of P's commits (at most 50)
is probed: it counts only if Q's head reaches it and Q's base does not. So an
over-cap Q that carries none of P's commits never blocks P. When P has more
than 50 commits to probe, the steward reads P's own commits not on the base
with their parents (`base...P.head`) and probes whether Q's head reaches each
root of that set (at most 50 roots). Every commit of P's set has its parents
in the set or on the base, so a Q head that reaches any of P's commits reaches
one of those roots; a Q head that reaches none of them shares no commit off the
base with P and does not block it. This is the same as "every merge base of P
and Q is on the base", without relying on the single merge base the compare API
reports, which a criss-cross history can have several of. Only when that
cannot be settled either (Q's head reaches a root of P's, P's graph is
incomplete or has more than 50 roots) does P fail closed
(`carried_commits_over_cap:#Q`); a failed read there fails closed as
`carried_commits_unreadable:#Q`. A P that is itself over the cap (for example a
mirror-history import of thousands of commits) cannot list its own commits, so
the check runs the other way: for every other open Q (the same exclusions
apply), the steward reads Q's own commits not on the base with their parents
(`base...Q.head`, narrowed to `Q.base...Q.head` for a Q on another branch) and
probes whether P's head reaches each root of that set (a commit none of whose
parents is in it). P carries some commit of Q's exactly when it reaches a root,
so a P that reaches none of an unapproved Q's roots is not held by Q, and one
that does is refused with `carries_unapproved_commits:#Q` naming the roots it
reaches. An unapproved Q whose own commits are over 250, with more than 50
roots, or past the 100 probes one over-cap P may make in a pass, refuses P as
`carried_commits_over_cap:#Q`. Each Q's roots are read once per pass and
reused across candidates. A paged compare whose total or merge base changes
between pages (the base moved mid-read) fails closed until the next pass.
There is no label or override that skips the check. An unreadable listing, compare, probe or
approval, or a Q pushed after the pass listed it, also fails closed
(`carried_commits_unreadable`;
the next pass re-reads it). The check applies only where the base
requires head approval. It guards every admission the steward makes: its own
enqueue (including a same-head retry or priority re-enqueue), the exact-head
merge on a base without a queue, and the `--arm-unqueued` backstop, which then
declines to arm (`skip: carried_commits`) and gives the row the same token. (On 2026-11-01 a branch
merged another branch that carried a held pull request's commits; its own
diff was approved and the held code reached main. On 2026-11-05 one
unapproved stacked pull request whose listing was over the cap refused every
approved pull request on main, though it carried none of their commits; the
own-diff read and the per-commit probe end that. On 2026-11-07 an approved
re-export carrying a 2,724-commit mirror-history import was refused only for
its own size; the reverse root probe ends that. On 2026-11-08 three ready
pull requests on main were each held by a stacked pull request that had merged
their branch; every shared commit was on the held pull request's own
first-parent chain, and the stacked pull request, whose base is not main, was
never going to be reviewed. The first-parent rule ends that.)

No stacking by default: `shipyard pr` refuses a base other than the
configured one unless `--stacked-on <n>` names the open parent pull request,
and then labels the new pull request `stacked:<n>`. Long-lived integration
bases are not stacks and pass unstacked: `[ship] integration_bases` lists them
as patterns (`*` within one path segment, `**` across segments), defaulting to
`["develop/*", "release/*"]`; an explicit list replaces the defaults and `[]`
allows none.

```toml
[ship]
integration_bases = ["develop/*", "release/*"]
```

The `stacked:<n>` label records the parent of a stacked pull request and keeps
that head out of review and admission until it is retargeted. It does not waive
the carried-commit guard: an unrelated candidate is still held when it carries
commits introduced by a labelled Q whose current head has no APPROVE (or has a
HOLD). A label anyone added by hand, naming a pull request that is not the
stack's parent, has no effect on that guard. When `<n>` merges, the repair and operator passes
list the stack immediately as `STACK-BASE-MERGED parent:#<n>`; they retarget it
onto main, merge main, push once and comment `retargeted: ...`. A parent closed
without merging is `STACK-PARENT-CLOSED`, a label whose pull request's head is
not the stack's base is `STACK-LABEL-MISMATCH`, and a parent that cannot be
read (a bogus number) is `STACK-PARENT-UNREADABLE`, reported without aborting
the rest of the listing. None of those three is retargeted.

Labels keep a pull request out of admission, and the steward refuses it:
`owner-decision`, `superseded` and `PARKED/<platform>`. `PARKED/<platform>`
excuses only that platform's CI, never a HOLD that names a code defect. If only
the owner can fix it (secrets, environments), close it with a tracking issue
rather than leaving it open.

That's the whole loop. When it stalls, the reason shows up in one place: the
steward's per-pass summary (how many PRs it admitted, how many are waiting for
approval, and any errors).

## With or without a merge queue

GitHub only offers merge queues on **organisation-owned** repositories. That
decides which path a repo takes:

| | Merge queue (org repo) | No merge queue (personal repo) |
|---|---|---|
| What the steward does | enqueues the approved head | merges the approved head with `expectedHeadOid` |
| What tests the merge | the queue's merge-group run | the PR's own required checks |
| What guarantees review | the steward's exact-head check | the same, plus a required `review-verdict` check |
| Batching | up to five per run | none; keep CI per PR small |

For the no-queue path to be safe, the branch ruleset has to require your CI and
a `review-verdict` check, and **both must be bound to the app that produces
them** (for GitHub Actions, integration id `15368`). Otherwise anyone who can
post a commit status could satisfy the requirement. The `review-verdict` job
fails unless the head is allowed, so a held or unapproved head cannot be green.
While a head only awaits its reviewer, the `review-verdict` status is
`pending` (not `failure`; a HOLD or an unreadable review is the `failure`), and
the steward reads it as waiting on the reviewer: nothing for the author to do.
`pending` is unsatisfied exactly like `failure`, so nothing merges unapproved.
The steward reports this path as `server_enforced_exact_head_merge`. It merges
only a head that GitHub reports `CLEAN`, with every required check green and an
approval it has itself confirmed. On any base that has no queue and no
app-bound approval rule, the steward refuses to merge at all
(`direct_merge_refused_server_enforcement_required`). It never arms GitHub's
native auto-merge on a base without a queue, because an armed request survives
a later push, and it turns off any such request it finds on a pull request it
owns. The approval check name is `--approval-context` (default
`review-verdict`).

## Setting it up

1. **Protect the default branch.** Use a merge-queue ruleset if the repo is
   owned by an organisation. Otherwise use a required-checks ruleset with CI
   plus `review-verdict`, both bound to the GitHub Actions app.
2. **Pick one machine to run the steward on a timer.** It needs only the
   repository's `.shipyard/` config, so a sparse clone is enough:
   `shipyard runner steward --repo OWNER/REPO --base main --apply`. Leave out
   `--arm-unqueued`: it arms native auto-merge for pull requests the steward
   does not own, only on a base with a merge queue, and reports "not applicable"
   on a base without one. Run one writer per repository, never two.
3. **Give CI somewhere to run.** Install TartCI on the Macs you want in the
   pool, label your jobs for it, and keep GitHub-hosted runners as overflow or
   for platforms you don't host.
4. **Run the reviewer and repair as scheduled one-shot passes.** Each is a
   timer job plus a small runner script that picks an available provider, sets
   a unique session id, takes a lock so passes never overlap, and caps run
   time. Each pass gets a written brief scoped by a list of repositories, not
   by repo-specific instructions, so adding a repo is a one-line change. Keep
   exactly one reviewer process. Interactive reviewer sessions are optional and
   must skip heads that already carry a verdict. Compute the reviewer's
   candidates with a script (the must-review queue), never by the model. Add
   the 2-minute eligibility tick so a pull request is reviewed shortly after its
   checks go green:
   - it never interrupts a running pass;
   - at most one follow-up is pending;
   - it kicks at most once per head.
5. **Tell every agent the rules** (next section).

## Accounts and identities

Keep each job on its own identity, so a retry or a compromised piece can't
quietly take on another's authority.

| Identity | What it's for | Notes |
|---|---|---|
| A **GitHub App** installed on the repos | The steward reads pull requests and checks, enqueues or merges, and posts statuses | Grant only what it uses: contents, pull requests, merge queues and statuses (write); checks and actions (read). Its installation token is short-lived; don't use a personal token for admission. |
| **Reviewer signers** | The names a verdict is signed with, such as `repo-review` | List them in Shipyard's reviewer allowlist and pin them to the machine they run on. Use a different signer from the agent that wrote or repaired the change. |
| A **release bot token** (only if a bot opens version-bump PRs) | Lets the bump PR trigger workflows | A pull request created with the default `GITHUB_TOKEN` doesn't trigger CI. Keep this token out of the admission path. |
| **Model accounts** for the agents | The reviewer, repair and authoring sessions | Any provider works. A pooled router such as Subrouter (`sr claude`, `sr codex`) lets sessions fall over to another account or provider when one runs out. |

None of these can bypass a branch rule. If a step needs a bypass actor on the
default branch, change the workflow instead; a bypass applies to every path,
not just the one you meant.

## Telling your agents

Agents follow what's in the repository's `AGENTS.md` (Codex) and `CLAUDE.md`
(Claude Code), plus any global versions in their home directory. A short,
explicit section stops them improvising: arming their own pull requests,
rebasing over someone's approved fix, or waiting forever for a merge that
someone else owns. Something like this is enough:

```markdown
## How a PR lands here
- Open the PR (`shipyard pr --no-arm`) and stop. Never arm, enqueue, or merge.
- A reviewer posts an exact-head APPROVE or HOLD once required checks are green.
- The Shipyard steward admits approved heads on its own every few minutes.
- On a HOLD: fix what it names with a test, merge main (never rebase), push once.
- A HOLD sticks to its head: fix it and push; don't ask for a re-approval of the same head.
- Before declaring yourself blocked, run `shipyard wait pr <n> --state actionable --timeout 1800 --json`
  and do what `next_action` says. You may stop only when it says `stop: true`.
- Platform-only failures you can't fix now: open an issue labelled
  `PARKED/<platform>` with evidence and a next step; don't block the merge.
- Use the smallest model that does the job; escalate only on a named blocker.
```

Put the rules in the repo so they travel with the code. Put machine-specific
details (which host runs the steward, local paths) in the global file on each
machine.

## When not to use it

- **You want a human to read every change before it lands.** Keep the review
  step human; the rest still helps.
- **Your required checks don't test what matters.** Automated draining merges
  exactly what your checks allow, faster. Fix the checks first.
- **One repository, a handful of pull requests a week.** A person clicking
  merge is simpler.
- **Two things are already admitting work.** Turn one off before starting the
  steward, or they will race each other.

## Why a pull request is not merging

Every steward pass (`shipyard runner steward --json`) gives each open pull
request on the base one `blocked_on` token and a `since` time, so a stall has
a name and an age. Nothing is left out of the pass: a draft, an unmanaged pull
request and one whose checks never ran each get a row.

Per row, beside `decision`, `mutation` and `error`:

- `blocked_on`: one stable token (below).
- `since`: when that state began, UTC, or `null` when nothing dates it.
- `since_source`: which GitHub time it came from: `checks_green` (the newest
  required check finished), `check_started`, `check_completed`,
  `approval_posted`, `hold_posted`, `queue_removed`, `merge_queue_enqueued`,
  `label_added` or `head_commit`. `head_commit` is the head commit's
  committer date: a best-effort stand-in that can predate the push (and a
  `BEHIND`/`DIRTY` state, which starts when the base moves): the state may
  have begun later, so the age it implies is an upper bound.
- `blocked_detail`: the contexts, reason or record id behind the token.
- `held_by`: `{holder, record, id}` whenever a HOLD names the head, even when a
  label or another state outranks it in `blocked_on`.

Per repository, `blocked_summary` holds `counts` (rows per token) and
`oldest`: the three longest-blocked rows as `{number, blocked_on,
age_minutes}`. `admitted`, `queued`, `ready` and `draft` rows are counted but
not listed as blocked.

| Token | Meaning | Who moves it |
|---|---|---|
| `no_verdict` | Green, and no reviewer verdict names this head: "awaiting the standing reviewer; nothing for the author to do" | reviewer |
| `held` | A counting HOLD names this head, so it is un-approvable and any APPROVE of it is ignored; `blocked_detail` reads "held head: needs a new head" | author: fix and push a new head (if the HOLD is wrong, push a new head anyway and comment `dispute: <reason>`) |
| `approval_excluded:<reason>` | An APPROVE names this head but does not count. Reasons: `unattributed`, `no_stamp`, `noncanonical_stamp`, `session_not_allowed`, `unlisted_stamp_session`, `no_signer_list`, `no_signer_line`, `uncounted_signer`, `unbound_identity`, `pr_author`, `impersonated_app`, `untrusted_reviewer`, `unlisted_reviewer`, `unlisted_human`, `predates_head` (posted before the head commit), `stale_head` (only an earlier head is approved), `self_approved` | reviewer or fleet config |
| `ejection_cap` | The queue removed this head too often since its last approval | reviewer, or a new push |
| `approval_unread` | The approval gate could not be read this pass | next pass |
| `owner_decision` | Labelled `owner-decision` | owner |
| `excluded_label:<label>` | `superseded`, `PARKED/<platform>` or the opt-out label | whoever owns the label |
| `provenance_blocked:<labels>` | A provenance-blocking label | owner |
| `required_never_ran:<contexts>` | A required context has no run on this head and nothing from its source is still queued or running (often a renamed check: merge the base to re-run CI) | repair lane |
| `required_pending` | Required checks are queued or running, or have not appeared yet behind a running job, and none has failed. A `pending` `review-verdict` status (head awaiting its reviewer) also reads here, with the reviewer as actor, never as `required_failed` | CI, or the reviewer |
| `required_failed:<contexts>` | A required check failed. On a merge-queue base (`red_check_outranks_pending`, see [Ejection guards](#ejection-guards)) this outranks `required_pending`: a red head reads failed while its other checks still run. No red required check is ever admitted, whichever token it reads. When the only red context is the approval context (`review-verdict`), the row names the approval token instead; an apply pass that did not read the gate for that head keeps this token and says so in `blocked_detail` | author or repair lane |
| `transient_rerun` | The steward is rerunning an interrupted check | steward |
| `merge_state:<STATE>` | GitHub says `DIRTY`, `BEHIND`, `UNSTABLE`, ... | repair lane: merge the base |
| `ejected_same_head:<cause>` | The queue ejected this exact head and the steward will not re-enqueue it: the failure is the head's, could not be placed, or the head's one retry is spent. `blocked_detail` carries the classifier's reason (the failing job and whether the retry was spent). Read from the same queue state in a dry run and an apply pass; `since` is the removal | repair lane |
| `ejected_same_head:<cause>:needs_new_head` | The same, under the retry discipline ([Ejection guards](#ejection-guards)): `<cause>` is `failed_checks` with no infrastructure signature, `merge_conflict`, or `invalid_merge_commit`. Only a new head moves it; for `invalid_merge_commit` or `merge_conflict`, merge the base once and push once | author or repair lane |
| `generated_path_batch:#<n>` | Approved and green, but this head changes a generated path (`[auto_merge] generated_paths`) that queued entry `#<n>` also changes; `blocked_detail` names the shared paths. Not an error: the steward admits it on the first pass after `#<n>` leaves the queue | steward (nothing to do) |
| `waiting_full_test_tier`, `waiting_enqueue_requirements` | Admission waits on a deferred check tier or a GitHub requirement | CI |
| `carries_unapproved_commits:#Q` | Approved and green, but carries commits of open pull request #Q, whose head is not approved; `blocked_detail` names the commits | Q's reviewer, or the author: drop Q's commits |
| `carried_commits_over_cap:#N`, `carried_commits_unreadable` | Whether the head carries unapproved commits could not be proven (an unapproved #N whose own diff is over 250 commits while P has more than 50 commits to probe and #N's head reaches a root of P's commits, or P's commits have more than 50 roots; for a P over 250 commits, an unapproved #N whose own commits are over 250, have more than 50 roots, or exceed P's 100 probes; or a failed read) | author, or next pass |
| `revalidating` | The head or queue changed during the pass | next pass |
| `waiting_code_owner:<owners>` | Approved and green, but the base's ruleset requires a code-owner review and no owner `CODEOWNERS` (at the base) assigns to a changed file has approved this exact head. The steward asks GitHub nothing and records no mutation; it posts one comment per head mentioning the owners (`+`-joined in the token). It never reads `reviewDecision`, which is `null` on a base that requires zero approvals even after the owner approves. A team or e-mail owner, or a file the PR author owns, is left to GitHub | code owner |
| `code_owner_review` | GitHub refused the enqueue: a code-owner review is outstanding. Recorded `rejected` with GitHub's message, never left uncertain | code owner |
| `enqueue_refused:<message>` | GitHub definitively refused the enqueue for another reason, quoted | author or owner |
| `uncertain_mutation` | An earlier enqueue of this pull request is still uncertain because GitHub's state could not be read; only this pull request waits | next pass, or operator |
| `steward_error` | Admission failed; see `error` | operator |
| `deferred_pass_budget` | The pass spent its time budget (or hit the API rate limit) before this row and did nothing for it; the next pass takes it first. See "Things that bite" | next pass |
| `admission_pending` | Admission returned an outcome with no token of its own; see `blocked_detail` | next pass |
| `queued` | In the merge queue | queue |
| `admitted` | Enqueued or merged this pass (outranks any label) | nobody |
| `ready` | Approved and green; a dry run would admit it | steward |
| `draft`, `unmanaged`, `handoff_missing`, `invalid_head`, `direct_merge_refused`, `required_policy_unknown` | The steward does not own or cannot admit it yet | author or config |

The approval tokens come from the same review and comment reads the gate made
to refuse the head, so naming them costs nothing extra. Only an undated state
(a conflict, a draft, a check that never ran) reads the head commit's time,
and a skip label reads its `labeled` event; a failed read leaves `since` null
and never fails the pass.

## What to do next, for one pull request

`shipyard wait pr <n> --state actionable --timeout <seconds> [--json]` runs the
same dry-run classification as a steward pass over that one pull request and
returns the moment the answer is yours: exit 0 with `blocked_on` and
`next_action` (`actor`, `action`, `stop`), or exit 1 on timeout, which means
nothing is yours to do yet. A merged, closed, stacked (base is not the default
branch) or empty-diff pull request is answered before any admission read.
Every steward pass row carries the same `next_action`.

| `actor` | Meaning |
|---|---|
| `author` | Act now: the action names the HOLD (with its link), the failed required check, the conflict, or the dropped change. |
| `owner` | `stop: true`: `owner-decision`, `superseded`, `PARKED/*`, or no automatic admission. Report and stop. |
| `nobody` | `stop: true`: merged or closed. A merged answer says to retarget stacked children to the base. |
| `reviewer`, `ci`, `steward`, `operator` | Nothing to do; keep waiting. Includes green-with-no-verdict, checks running, queued, and BEHIND on a merge-queue base (never merge main for that). |

Reads are a full admission pass, so the default `--poll-interval` is 120
seconds for this state. The ledger used for transient-rerun budgets is the
local one; on the steward host it is the steward's own.

## Ejection guards

An ejected pull request costs the batch it broke a rebuild and costs itself
hours: on Generous-Corp/pulp over one week (2026-11-02..09) an ejected pull
request took a median of 231 minutes from first entry to merge, against 31
minutes for one that merged first time. Of 59 ejections, 19 were two batch-mates
changing the same generated file, 18 were flakes or infrastructure, and 4 were
`invalid_merge_commit` (one head re-enqueued three times in 21 minutes). The
steward runs three guards against what it can see coming. Each is a key in the
**base** branch's `.shipyard/config.toml` `[auto_merge]` table (never read from a
head) and each defaults **on** for a base with a merge queue, off otherwise:

```toml
[auto_merge]
generated_paths = [                       # globs: `**` whole segments, `*` within one
  "test/ctest_script_inputs.json",
  "test/ctest_script_inputs/**",
]
generated_path_batch_guard = true         # default: on with a merge queue
red_check_outranks_pending = true         # default: on with a merge queue
infra_only_same_head_retry = true         # default: on with a merge queue
known_flaky_tests = []                    # names or `*` globs; empty by default
```

- **Generated-path batch guard.** Before an enqueue the steward re-reads the
  queue live; when another entry is queued it reads the candidate's changed
  files (one GraphQL `files(first:100)` page, cached for the pass; paged to
  GitHub's 3000-file limit only for a larger pull request). When the candidate
  changes a generated path it reads each queued entry's files; the first entry that changes the same path defers
  the candidate as `generated_path_batch:#<n>`, with no mutation and no error.
  It is admitted on the first pass after `#<n>` leaves the queue. A file list
  that cannot be read never admits (`steward_error`); one that is still
  incomplete after paging is compared by the other side's generated paths, so
  it can only defer. Two different files under one glob (two shards) do not
  collide; only the same path does. With no `generated_paths` the guard never
  defers. What it prevents: the two heads sharing one batch, so a textual
  conflict or drift no longer ejects the batch-mate, and the deferred head
  meets the other's change as a `DIRTY` merge state (merge the base) instead
  of in the queue. A deferred head that merges textually but whose generated
  file drifts against the merged tree can still be ejected once; it then
  needs a regenerate commit either way. A deferred head gets no priority over
  a fresh one in the next pass.
- **A red required check outranks a pending one.** A head with a failed
  required check reports `blocked_on=required_failed:<contexts>` (the running
  contexts in `blocked_detail`) even while another required check is still
  running, instead of `required_pending`. Only the report changes: admission
  still waits, and the recovery worker still waits for a settled head before
  it asks an agent. Every enqueue already re-reads the head's
  checks live and refuses any red required check; Generous-Corp/pulp#11698's
  red version check (cited as "armed red") went red at 03:21, after its last
  enqueue at 03:05. What re-enqueued it was the next guard's gap.
- **Retry discipline.** The one free same-head re-enqueue after a
  `failed_checks` ejection is spent only on an infrastructure signature: a
  runner conclusion (`timed_out`, `startup_failure`, `stale`) or `cancelled`, a
  lost-runner annotation, a network or interruption signature, or, at the
  failing step's own error, GitHub's step-timeout line (`has timed out after`),
  an HTTP 5xx or `rate limit exceeded` download answer, or the runner's own
  shutdown lines (`INFRA_STEP_SIGNATURES` in `src/environment_requeue.rs`). A
  check that merely passed on the head, or also fails on the base tip, is not
  retried: on Pulp every merge-group-only failure passed on its head; of 23
  same-head retries 12 merged and 9 failed again the same way. The discipline
  trades some of those 12 flake recoveries (which now need a new head) for
  the 9 wasted group builds; turn it off per base if flakes dominate. The
  widened readings (a step-timeout, HTTP 5xx or rate-limit line, a
  `cancelled` conclusion) clear the head only when the same check passed on
  the head itself. A ctest-level `***Timeout` is not an infrastructure line;
  a step or job timeout is, deliberately, so a hang only the merged tree
  exercises can still spend the head's one retry.

  **Known flakes** (`known_flaky_tests`): under the discipline, a failing
  required check also earns the one same-head retry when its name is listed,
  or when every failing step's ctest `The following tests FAILED:` block names
  only listed tests. The same check must have passed on the head, the retry is
  still one per head, and `invalid_merge_commit` / `merge_conflict` are never
  retried. A step that names no test (a compile error, a crash before the
  summary) is never a known flake. Every FAILED block in the step counts (a
  rerun's block cannot hide the first run's failure), each must end on a blank
  line, `Errors while running CTest` or the step's `##[error]`, and the last
  must sit within 60 lines of the error. Entries are exact test names or globs
  where `*` matches anything; matching is case-sensitive, so copy names from
  the ctest output. A check name is listed only by its exact name, never by a
  glob, and a bare `*` lists nothing. A listed test that failed by `(Timeout)`
  is accepted on purpose: flakes often time out, and listing a test is a
  deliberate, issue-tracked decision. Keep the list to tests with a tracking issue, and remove
  an entry when its issue closes. Pulp's tracked flakes at the time of
  writing, as a starting list:

  ```toml
  known_flaky_tests = [
    "ui-script-plugin-editor-au",          # #11861; the evidence suggests a real crash, review before listing
    "a public WaveNet node runs its GPU path*",  # #11862
    "AsyncBlockWorker: a fast pull without an offline flag waits in Auto mode",  # #11863
    "WakeSignal: racing publish and park never lose a wakeup",  # #11864
    "Staged GpuConvolver*",                # #11865
    "inject-worktree-lineage-hook",        # #11866
  ]
  ``` `invalid_merge_commit` is never re-enqueued on the same
  head (before, it reset the refusal: #11698 went `failed_checks`, then three
  immediate `invalid_merge_commit` removals, each re-enqueued). Every refusal
  reads `ejected_same_head:<cause>:needs_new_head`.

Turn one off by setting its key to `false` in the base's config. If the
pass-start config read fails, the guards keep their merge-queue defaults and
the batch guard re-reads the config live before each enqueue; a second failure
refuses the enqueue (`steward_error`) rather than admitting unguarded.

Scope: the guards apply to the steward's own enqueue and its `blocked_on`
report. The `--arm-unqueued` native-auto-merge backstop (for pull requests the
steward does not own), `shipyard auto-merge`, `shipyard ship` and the `ghapp`
arm guard keep their earlier same-head rules, including re-arming after
`invalid_merge_commit`.

## Things that bite

These came from running it for real:

- **Long steward passes can block VM boots.** The steward holds a state lock
  while it works, and TartCI's VM admission takes the same lock. Keep passes
  short (Shipyard hydrates only pull requests that could actually be
  admitted), and run repositories one after another, not in parallel.
- **A pass that is killed reports nothing.** A pass reads GitHub per open
  pull request, so a deep backlog on a slow token path can outrun a
  scheduler's timeout, and a killed pass prints no report. Past
  `--pass-budget-secs` (default 240; 0 disables) an apply pass starts no new
  per-PR work, and it does the same once GitHub reports the token's API rate
  limit exceeded. The budget is per repository: each repository's clock starts
  when its observation starts, so an earlier repository never starves a later
  one. A rate limit while observing a repository leaves the remaining
  repositories of the invocation unread rather than erroring through them. Each remaining row is reported `deferred_pass_budget` with
  its census decision, is never admitted, armed, enqueued or dequeued that
  pass, and is evaluated first on the next pass, after admission candidates
  and queued or armed rows. Deferred rows are taken oldest deferral first (a
  deferred admission candidate also goes ahead of a fresh one), so a row
  evaluated last pass goes to the back instead of re-taking the newest rows
  every pass and starving the oldest. Keep the budget a few minutes under the
  scheduler's timeout so the in-flight pull request and the report fit.
- **A timer job that touches an external volume can hang.** On macOS a
  background job opening a path on `/Volumes` can wait forever on a privacy
  prompt nobody sees. Run the steward from the internal disk.
- **A flaky or broken runner ejects good heads.** A merge-group failure that
  is not the head's fault used to strand an approved head until someone
  pushed. With `[queue.environment_requeue] enabled = true` the steward now
  re-enqueues the same head once, through its exact-head path with the
  approval read live, when every failing required check on the ejecting merge
  group is a runner failure (`timed_out`, `startup_failure`, `stale`, a
  lost-runner annotation, or a network signature). On a base without the retry
  discipline it also retries a check that is failing on the base tip or passed
  on the head itself; on a merge-queue base the discipline is on by default and
  those stay `ejected_same_head:failed_checks:needs_new_head` (see
  [Ejection guards](#ejection-guards)). A check that passed on the base tip and
  not on the head, an unplaceable one, or a second ejection of the same head
  (counted from GitHub's own removal events) stays `ejected_same_head:<cause>`
  for repair.
- **A refused enqueue is not an uncertain one.** When GitHub answers an
  enqueue with a typed refusal (for example "Waiting on code owner review"),
  the steward records it `rejected` with GitHub's message and reports its own
  token. Only a timeout or transport failure is recorded as uncertain.
- **Uncertain enqueues settle themselves.** Each pass re-reads any pull
  request with an uncertain enqueue. Merged, or queued at the recorded head,
  resolves it `accepted`; no queue entry, no auto-merge and not merged
  resolves it `rejected`. Each settlement is written to the mutations ledger
  with `resolver: steward-auto-reconcile`. An unreadable state stays uncertain,
  marks the pass unhealthy, and holds only that pull request: other admissions
  and `--arm-unqueued` carry on.
- **Advisory checks do not hold the backstop.** `--arm-unqueued` judges green
  by the base's required checks, like admission. A `BLOCKED` pull request
  whose required checks all passed is still armed when the only red is a
  cancelled, skipped or neutral advisory check. A failed or missing required
  check still stops it.
- **Rebasing loses approved work.** A rebase once silently dropped hardening
  that a reviewer had already approved. Merge the base branch in instead.
- **One burst of merges can queue a release per merge.** Give the release
  workflow a single concurrency group, so a newer tag replaces any release
  still waiting.
- **Version-bump pull requests are cheap to make expensive.** If a bot opens
  one per merge, each costs a full CI run. Hold bumps while the queue is busy,
  and fast-path a bump whose diff touches only version files. Shipyard itself
  keeps exactly one: `version-at-land.yml` force-updates the bot-owned
  `release/version-at-land` branch with the cumulative bump after each landing
  and hands that one pull request to the steward (`shipyard:managed` plus an
  exact-head `shipyard/steward-handoff` status); it still needs green required
  checks and an exact-head approval.
- **A rule in an agent's brief is not a gate.** A scheduled reviewer was told
  to review only heads with no verdict, ignored it, and approved held heads.
  Anything that posts verdicts needs a mechanical check before it goes live;
  Shipyard now enforces held heads itself.
- **One reviewer process per signer, with an attributed session.**
  Approvals from an unattributed session don't count (a HOLD still does), and
  two processes sharing a signer can't be told apart. Scheduled (headless) runs
  must set a session id explicitly.
- **Only a reviewer run's session approves.** A `reviewer:` line and an
  agent id are text any lane can write; a reviewer run's session id is minted
  for that run. List those sessions on the base as globs on the `ghapp` stamp,
  `[auto_merge] reviewer_stamp_sessions = ["fleet-pass-reviewer-*"]`, and an
  APPROVE from any other session is excluded (`unlisted_stamp_session`); a
  HOLD from any session still counts. Example: on pulp#11407 the authoring
  session approved its own head, signed `reviewer: pulp-review`, and the
  `review-verdict` status went green; with the list, only the scheduled
  reviewer's APPROVE (session `fleet-pass-reviewer-<host>-<stamp>-<pid>`)
  counts.
- **A model choosing its own candidates skips work.** Given only a brief, a
  reviewer pass reviewed other pull requests and twice passed over a repaired,
  green one. Compute the candidate list mechanically and hand it to the pass.
  Log anything it leaves unreviewed, and put that first next time.
- **A timer alone adds up to a full interval of latency, and a pass can just
  miss a head.** A pass builds its queue when it starts. A pull request whose
  last check turns green seconds later waits for the next pass. A cheap
  eligibility tick that starts a pass when a pull request becomes reviewable
  removes that wait. Run it with an explicit `PATH`: under a timer job the
  login profile may not load, and a helper such as `ghapp` silently goes
  missing.
- **Fail over between providers.** A headless review pass should retry on
  another provider when one returns auth or quota errors, and remember a recent
  failure instead of paying for it on every run.
- **Version bumps on a protected branch.** If required checks block direct
  pushes, route bumps through one coalesced bump pull request that the writer
  keeps updating. Don't add a ruleset bypass: it applies to every path, and a
  token readable by branch workflows could use it. Give the bump commit the
  skip trailers its own sync checks need, and give the bot token permission to
  open and update pull requests, add labels and post statuses, not just push.
- **Keep release-signing keys off the machines agents run on.** If a fast path
  trusts "signed by the release bot", the key should live only in CI secrets,
  with an offline backup for recovery. The verifier should pin the key's
  fingerprint from the protected branch rather than trust author names.

## Related

- [`shipyard landing`](landing-model.md) shows how a repository actually lands
  pull requests, measured rather than read from config.
- [TartCI](https://github.com/danielraffel/tartci) covers VM pools, images,
  leases and runner lifecycle.
- The scheduled reviewer, repair and operator pass-runner used on m3 is versioned in [`tools/fleet-pass/`](../tools/fleet-pass/README.md).
