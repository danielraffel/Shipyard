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
  whose current head has an approving verdict and green required checks, it
  either adds it to the merge queue or merges that exact head. It re-reads the
  approval immediately before acting, and does nothing if anything is unclear.
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

## How a pull request moves

1. An agent opens a pull request (`shipyard pr --no-arm`, or any normal way)
   and stops.
2. CI runs on the pull request's head: on TartCI VMs, GitHub-hosted runners,
   or both.
3. Once the required checks are green, a reviewer reads the change and posts
   a verdict naming the exact commit: APPROVE, or HOLD with what to fix.
4. On a HOLD, a repair agent fixes what was named, adds a test, merges the
   base branch in (never a rebase), and pushes once. A new head means a fresh
   review.
5. On the next steward pass (every five minutes is plenty), the approved head
   is handed to GitHub. A merge queue tests it with up to four others and
   merges the batch. Without a queue, Shipyard merges that exact head, and
   GitHub refuses unless the required checks pass.

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
must also *fail* when the verdict is a refusal. A job that posts a refusal and
still exits green would pass a held head.

## Setting it up

1. **Protect the default branch.** Use a merge-queue ruleset if the repo is
   owned by an organisation. Otherwise use a required-checks ruleset with CI
   plus `review-verdict`, both bound to the GitHub Actions app.
2. **Pick one machine to run the steward on a timer.** It needs only the
   repository's `.shipyard/` config, so a sparse clone is enough:
   `shipyard runner steward --repo OWNER/REPO --base main --arm-unqueued --apply`.
   Run one writer per repository, never two.
3. **Give CI somewhere to run.** Install TartCI on the Macs you want in the
   pool, label your jobs for it, and keep GitHub-hosted runners as overflow or
   for platforms you don't host.
4. **Start a reviewer and a repair agent** as long-running sessions with
   written briefs. Scope them by a list of repositories, not by repo-specific
   instructions, so adding a repo is a one-line change.
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

## Things that bite

These came from running it for real:

- **Long steward passes can block VM boots.** The steward holds a state lock
  while it works, and TartCI's VM admission takes the same lock. Keep passes
  short (Shipyard hydrates only pull requests that could actually be
  admitted), and run repositories one after another, not in parallel.
- **A timer job that touches an external volume can hang.** On macOS a
  background job opening a path on `/Volumes` can wait forever on a privacy
  prompt nobody sees. Run the steward from the internal disk.
- **Rebasing loses approved work.** A rebase once silently dropped hardening
  that a reviewer had already approved. Merge the base branch in instead.
- **One burst of merges can queue a release per merge.** Give the release
  workflow a single concurrency group, so a newer tag replaces any release
  still waiting.
- **Version-bump pull requests are cheap to make expensive.** If a bot opens
  one per merge, each costs a full CI run. Hold bumps while the queue is busy,
  and fast-path a bump whose diff touches only version files.

## Related

- [`shipyard landing`](landing-model.md) shows how a repository actually lands
  pull requests, measured rather than read from config.
- [TartCI](https://github.com/danielraffel/tartci) covers VM pools, images,
  leases and runner lifecycle.
