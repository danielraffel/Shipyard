# The `review-verdict` status

`queue-arm-guard` refuses an unreviewed or held head, but only for landings
sent through `ghapp`. Two kinds of landing never pass through it:

- GitHub-native auto-merge armed before a HOLD: GitHub merges by itself once
  the checks are green, and no one sends a merge request the guard could see.
- A workflow that merges with its own token (Vellum's merge-on-green steward).

The `review-verdict` commit status closes both once a repository makes it a
required check: GitHub then cannot merge a head whose status is not
`success`, whoever asked.

## What it decides

`scripts/review_verdict_status.py` calls the guard's own
`review_gate` with the live head bound, so the status and the guard cannot
disagree:

- a trusted HOLD naming the head always wins, before or after any APPROVE of
  the same head (a held head needs a new push); otherwise the newest trusted
  verdict naming the head decides;
- `success` only for an APPROVE naming the head (a comment ending
  `verdict: APPROVE head:<sha>` or `reviewed: <sha>`, or an `APPROVED`
  review on it), or one the guard's merge-only carry takes to it;
- `pending`, not `failure`, while the head only awaits its reviewer: no
  counted verdict names it, or only approvals that do not count do. The
  description says `awaiting reviewer verdict for head <sha>`, or names why an
  approval was excluded (`awaiting reviewer verdict; 1 approval excluded:
  unlisted_stamp_session`; codes include `unattributed`, `uncounted_signer`,
  `not_exact_head`). Branch protection treats `pending` as unsatisfied exactly
  like `failure`, so nothing merges without an approval; `failure` is kept for a
  counted HOLD or an unreadable review. The job (the required check run) still
  fails while awaiting, so that check cannot read as satisfied; it carries a
  notice saying there is nothing for the author to do;
- whose verdict counts is the guard's rule: the base's `reviewer_identities`,
  else an approval only from the fleet App or a human the base lists in
  `[auto_merge] human_reviewers` (a HOLD from any OWNER, MEMBER or
  COLLABORATOR still counts), never a distinguishable author;
- a repository whose base sets `arm_requires_head_approval = false` gets
  `success` with the reason.

The rebase carry is on here, whatever any host's switch says: the status is
the repository-side rule. An approval of an earlier head carries across a
rebase only when the diff keeps its `git patch-id --stable`, the files and
their statuses are the same, the base touched none of them and each keeps its
approved blob and mode, and no required check on the new head was skipped,
neutral or reused a receipt. `shipyard review-verdict --pr N --json` gives the
same answer from the command line (Rust, `crate::review_verdict`).

Signers come only from the base: the status reads no host's
`reviewer-signers.json`. An App APPROVE counts only with a `reviewer: <name>`
line naming one of the base's `[auto_merge] reviewer_signers`, and in a
repository that requires review by default with no list on its base, no App
APPROVE counts at all ("no signer list is configured on the base branch").
Every repository that relies on the status, or on `shipyard review-verdict`,
must set the list on its base branch first.

Repositories that commission more than one provider can bind each signer name
to stamped host and agent identities with
`[auto_merge.reviewer_signer_identities]`, for example
`codex-review = ["Daniels-Mac-Studio-m3 agent:codex-reviewer"]`. The status
refuses a missing or mismatched host/agent stamp; existing signer and reviewer
login settings remain unchanged until this map is configured.

A `reviewer:` line and an `agent:` id are text any lane can write; a reviewer
run's session id is minted for that run. So a base can list the sessions that
may approve, as globs on the stamp's `<session>`:
`[auto_merge] reviewer_stamp_sessions = ["fleet-pass-reviewer-*"]`. With the
list set, an App APPROVE counts only when its stamped session matches one
(`unlisted_stamp_session`); a HOLD from any session still counts. pulp#11407:
the authoring session approved its own head, signed `reviewer: pulp-review`
under its own session stamp, and the status went green; with this list only
the scheduled reviewer run's APPROVE
(`fleet-pass-reviewer-Daniels-Mac-Studio-m3-20261102T033922Z-92868`) counts.
Without the list the rule is off. The steward's own gate reads the same key.

An App APPROVE must also carry the `<!-- session: <host>/<session> -->` stamp
`ghapp` adds, and not an `unattributed` one ("it has no `ghapp` session
stamp"). Every fleet agent posts as the App, and a body posted around the
stamper (stdin, a raw API call, the web UI) names no session at all:
tartci#623's copied APPROVE, with its `unattributed` stamp removed, passed the
status before this rule. Reviewers post through a stamped path (`ghapp api
.../issues/N/comments -f body=...`). A host's own guard does not require the
stamp unless its `reviewer-signers.json` lists `sessions`.

A status belongs to a commit, but a decision belongs to a pull request, and
two open pull requests can share a head (one branch opened into two bases, a
fork pushing an approved commit). The status is `success` only when the guard
allows the head for every open pull request whose head it is
(`commits/{sha}/pulls`).

## Edited and deleted verdicts

A pull request's own workflow can edit or delete any comment in its
repository with `pull-requests: write`, and an edited comment keeps its
author. merge-queue-rehearsal#47 showed both on an App comment (PATCH 200,
DELETE 204; `issues: write` alone and a read-only token got 403). So:

- **An edited APPROVE counts for nothing.** A comment counts as an approval
  only while its REST `updated_at` equals its `created_at` (a reaction does
  not move it; an edit does, and times that cannot be compared count as
  edited), and, for the comment that decides a head, while its GraphQL
  `lastEditedAt` is null. REST times have one-second resolution, so an edit
  made in the second the comment was posted only shows in `lastEditedAt`; a
  `lastEditedAt` that cannot be read refuses the head. The status names the
  exclusion `edited`. The same rule holds in `queue-arm-guard`, `shipyard
  review-verdict`, the steward's gate (any author: the App, a listed human,
  a listed reviewer), `repair-check` (which refuses to judge an edited newest
  verdict) and the relay. A reviewer who changes their mind posts a new
  verdict.
- **An edited HOLD still holds,** since it can only refuse more.
- **A HOLD deleted or edited away still holds, from three records no pull
  request's code can rewrite.** None of them is complete; the durable fix
  takes verdicts from an App-posted status instead of comments.
  - The poster. An edit or a delete made with a `GITHUB_TOKEN` starts no
    workflow run (GitHub creates none for events that token causes), so the
    poster never sees that edit or delete. What it has is the HOLD's own
    `created` run: that event's payload carries the HOLD text even after the
    comment is gone, and a counted HOLD naming the live head posts `failure`
    (`a HOLD naming head <sha> was removed after it was posted (...); it still
    holds`), whether the head was approved or only pending. An edit or delete
    by anyone else does start a run, and its payload (the deleted body, or
    `changes.body.from`) is judged the same way. Later runs read the head's
    status history: a `review-verdict` `failure` starting `a HOLD naming head
    <sha>`, posted by this job's own creator, keeps the head held. That
    creator is the verdict App's bot once `REVIEW_VERDICT_APP_BOT_LOGIN` names
    it, else `github-actions[bot]`, never both.
  - The steward's ledger. A head any steward pass finds held is written to
    the steward's own ledger (`held_heads`, kept 30 days), and the steward's
    gate refuses it from then on, and the native-arm backstop skips it,
    whatever the comments say. A HOLD deleted before any pass judged its head
    is not in it.
  - The host's hold record. `queue-arm-guard` and the steward's gate both
    refuse a head that the hold hook's `holds.jsonl` on their host names as
    held (it records each HOLD posted through `ghapp` there). A HOLD posted
    from another host is not in it; an unreadable file refuses.
  - A held head's approval never carries. Each record is checked for the head
    an approval is carried from as well as the live head, so deleting the
    HOLD and merging the base into the branch does not carry the held head's
    approval to the new head.

Residuals: the HOLD's `created` run can still be lost, either cancelled by a PR
job with `actions: write`, or displaced in the per-PR concurrency group (one
run runs and one waits; a newer waiting run cancels the older waiting one);
a dismissed HOLD review leaves no payload text, so only an earlier failure in
the status history or the steward ledger keeps it held; and until the App
posts, a PR workflow can post a `github-actions` failure that holds any head
in the repository until that head's pull request pushes again. Vellum does not
require `review-verdict` at all.

### What this interim hardening does not close

- **Comments are still the verdict authority.** Every rule above is a
  defence of comment text that any holder of the shared App, and a pull
  request's own token for edits and deletes, can touch. The fix is an
  App-posted status from the reviewer process that the steward reads, in a
  separate change.
- **A HOLD posted from another host and deleted within one steward tick.**
  It is in no host's `holds.jsonl` on the steward's host, no steward pass
  read the gate while it stood (the gate is read only on an admission
  attempt), and if its `created` poster run was cancelled or displaced it is
  in no status history either.
- **A missing steward ledger file loads as empty,** so every held head it
  recorded is forgotten (a file that does not parse is an error). Entries
  also expire after 30 days.
- **After the App bind, keep the Actions-bound rule alongside the App
  rule.** A job that fails because the App could not post a refusal does not
  remove the App's earlier `success`; until the App leg has a record of
  posting refusals, the Actions-bound rule is what keeps such a head
  unsatisfied.

## The verdict App's stale status

When `REVIEW_VERDICT_APP_ID` is set and the state to post is not `success`
(a refusal), an App post that fails is retried once; if it still fails the
job exits non-zero with an `::error`, because the App's last status (perhaps
an earlier `success`) would otherwise stand on a refused head. A `success`
the App cannot post still fails open.

## Which branch's policy

The policy (`.shipyard/config.toml`) is read from the repository's default
branch, never the pull request's base. A pull request chooses its base, and
only the default branch is protected, so a branch could carry a config that
turns review off or names any reviewer. A pull request whose base is not the
default branch is refused outright (`failure`, "its base ... is not the
default branch"), whatever it carries, so a status judged there cannot be
carried to the default branch by a retarget. `pull_request_target: edited`
re-runs the job when a pull request is retargeted. `queue-arm-guard` reads the
policy from the default branch too; `shipyard review-verdict` refuses another
base the same way.

An approval also counts only for the base it was given against. A child
stacked on a parent's branch is approved for its diff against that parent;
retargeted to the default branch with the same head, that APPROVE would land
the parent's commits. The guard reads the pull request's base changes
(`BaseRefChangedEvent`, and `AutomaticBaseChangeSucceededEvent` for GitHub's
own retarget after a parent merges) and leaves out every APPROVE posted at or
before the newest change that is not provably safe. A manual retarget is never
safe. GitHub's own retarget is safe only when exactly one pull request from
the old base into the new base merged before it and that pull request's final
head is an ancestor of, or equal to, the child's head (compare `ahead` or
`identical`): then the approved diff is exactly the child's own content on top
of what landed. Anything unreadable or ambiguous voids. If that leaves the head unapproved the status is
`pending` (cause `approval_void`, "re-review against the new base"); a new
verdict posted after the change counts as usual. A timeline it cannot read in
full is `failure`, never "no change". The Rust gate
(`src/head_approval/base_change.rs`) applies the same rule, so the steward
reports `approval_void:base_changed`.

## The verdict App leg

When `REVIEW_VERDICT_APP_ID` is set, the job posts the same status twice:
first with `REVIEW_VERDICT_APP_TOKEN` (a token for the dedicated verdict App),
then with the job's own token. For a `success` the App leg fails open: an
empty token (the mint step failed) or a refused post prints a `::warning`
and never changes the exit code. A refusal is retried and then fails the job
(see "The verdict App's stale status"). Before a ruleset binds the App this keeps a bad key or an
outage from turning every pull request red; after the bind, a missing App
status already leaves the rule unmet. The token is never printed.

Not yet wired (future rollout): `.github/workflows/review-verdict.yml` today
sets only `GH_TOKEN`, so the job posts as GitHub Actions alone and the App
leg is inactive. The planned workflow adds a token mint step with
`continue-on-error: true` and gives the poster step `REVIEW_VERDICT_APP_ID`,
`REVIEW_VERDICT_APP_BOT_LOGIN` and `REVIEW_VERDICT_APP_TOKEN`; until then the
script's App leg runs only where those variables are set by hand.

The steward maps a commit status to the App that posted it by its creator.
GitHub Actions maps always. The verdict App maps only when the steward host's
machine-global config names it:

```toml
[merge_steward.review_verdict_app]
id = 1234567                     # the App's id, as the ruleset binds it
bot_login = "fleet-verdict[bot]"
bot_user_id = 7654321            # the bot user's id
```

Login and user id must both match. A setting naming GitHub Actions or
malformed is refused, and `shipyard runner ...` stops instead of silently not
recognizing an App a ruleset may require. Before a ruleset is bound, check
which observation the steward selects: each pull request row in `shipyard
runner steward --repo <repo> --json` carries `required_checks`, one entry per
rule with `required_app_id`, `selected_source` (`check_run` or `status`),
`selected_app_id` and `selected_state`.

## The job is the check

The job is also the `review-verdict` check run, and it exits non-zero unless
the head is allowed, so a required `review-verdict` check cannot be green on a
refused, held or unapproved head. A read or post error fails the job too (it
never passes by default). The commit status is posted exactly as before.

Only the `pull_request_target` run leaves its check run on the pull request's
head; a comment's (`issue_comment`) or a review's (`workflow_run`) run leaves
its check run on the default branch's tip. Those runs
post the status and then re-run the head's newest `pull_request_target` run
(`actions: write`), so an APPROVE or a HOLD changes the head's check without a
new push. A re-run that cannot be requested fails the job.

Both listings the poster reads are paginated in full: every open pull request
sharing the head is judged, and the head's own run is found on any page. A
listing that is unreadable, malformed, or shorter than its `total_count`
fails the job instead of standing in for the whole one.

## Trust

- `pull_request_target` and `issue_comment` use the default branch's definition;
- a review is different: `pull_request_review` runs the pull request's own
  copy of the workflow file, so a same-repository pull request could rewrite
  a job that holds `statuses: write`. Reviews therefore start
  `review-verdict-trigger.yml`, which does nothing and holds no permissions,
  and its completion starts the poster through `workflow_run`, which runs the
  default branch's definition with the head read from the API.

The poster checks out the default branch, never the pull request's code, and
reads every comment and review from the API, so an APPROVE from an account
the rule does not trust counts for nothing, whoever posts it.

**A `GITHUB_TOKEN` status is not proof of who posted it.** Every workflow in
the repository posts as `github-actions`, and anyone who can push a branch can
add a `pull_request` workflow that posts `review-verdict=success` with its own
token. Report-only, that only misleads a reader; as a required check it would
be forgeable. Stage 2 therefore posts from a dedicated GitHub App and requires
that App as the status's expected source in branch protection, so a
`github-actions` status of the same name does not satisfy it.

Pins: the job runs on `ubuntu-24.04`, uses `actions/checkout` and
`actions/setup-python` by commit SHA, and runs the rule on Python 3.12.12 (the
guard needs `tomllib`, 3.11 or later; an older interpreter would fail every
status closed). It calls only `gh api`, from the image's `gh`.

## Guard mode: authorization from the fleet admission check

A verdict comment is text posted through the shared fleet App, so anyone who
runs an agent can post one, signer line and session stamp included. A base
can instead take authorization from the fleet trust root's admission check
run, posted by an App no agent holds:

```toml
[auto_merge]
approval_source = "guard"            # "comments" (the default) is the fallback
guard_app_id = 1234567               # the admission App's numeric id
guard_admission_check = "fleet-guard/admission"   # the default
```

Then this status, `shipyard review-verdict`, the steward (enqueue, exact-head
merge, native arm, carrier) and the `ghapp` queue-arm guard all read the same
thing: the check runs named `guard_admission_check` on the exact head whose
`app.id` is `guard_app_id`. GitHub sets `app.id`; a run or status of that name
from any other App, including `github-actions` and `shipyard-local`, is
reported and ignored. The newest counting run decides:

| Newest counting run | Result |
|---|---|
| none, queued or in progress | `pending` (waiting for admission) |
| completed `success` whose `external_id` binds this head | `success` |
| completed `success` whose `external_id` is missing, not JSON, names another head, or lacks a full `policy_sha` or a `classification` | `pending`: counts as absent |
| completed with any other conclusion | `failure` |

`external_id` is JSON: `{"base_sha", "head_sha", "policy_sha",
"classification"}`. No comment or review is read for authorization in guard
mode, and no approval carries to a new head (merge-only, rebase, or rollup):
each head needs its own admission. The steward's merge-queue ejection cap
still applies: it is a loop-breaker, not an approval rule, so an admitted
head the queue removed twice is not enqueued again, and a later admission
run does not lift it (the trust root re-posts admission every tick); only a
new head does. `approval_source = "guard"` also turns
`arm_requires_head_approval` on, so leaving that key out cannot switch the
gate off. A guard source with no usable `guard_app_id`, an unknown
`approval_source`, or `guard_app_id = 15368` (GitHub Actions) refuses every
head instead of falling back to comments.

A host can refuse the comment fallback outright: a
`require-guard-admission` file in its guards directory
(`$SHIPYARD_GHAPP_GUARDS_DIR`, else `~/.config/shipyard/guards`) makes every
comment-mode base fail closed on that host, so a later base-config edit
cannot quietly put it back on comments. This file is the host's
machine-global downgrade guard; there is no `config.toml` key for it. Like
every file in that directory it is an accident guard: the agent user can
delete it. The boundary is the ruleset that requires the admission check
bound to the admission App's integration id.

The `check_run` trigger re-judges a head when its admission run completes,
so the status follows admission without a new push. Only runs named
`fleet-guard/admission` start the job; a base that renames the check also
edits that `if:`.

## Rollout

1. Report-only: the status posts but is not required. Compare it with the
   guard's decisions on real pull requests for two or three days.
2. Required, one repository at a time, starting with Vellum: posted by the
   dedicated App, with that App required as the source. A change to branch
   protection is reviewed before it is applied.

## Adding it to another repository

Shipyard runs `.github/workflows/review-verdict.yml` and
`review-verdict-trigger.yml` from its own default branch. Another repository
adds both, the poster running the script from a pinned Shipyard commit:

```yaml
# .github/workflows/review-verdict-trigger.yml
name: Review verdict trigger

on:
  pull_request_review:
    types: [submitted, edited, dismissed]

permissions: {}

jobs:
  trigger:
    runs-on: ubuntu-24.04
    timeout-minutes: 1
    steps:
      - name: Hand the review to Review verdict
        run: 'true'
```

{% raw %}
```yaml
# .github/workflows/review-verdict.yml
name: Review verdict

on:
  pull_request_target:
    types: [opened, reopened, synchronize, ready_for_review, edited]
  issue_comment:
    types: [created, edited, deleted]
  workflow_run:
    workflows: [Review verdict trigger]
    types: [completed]
  check_run:
    types: [completed]

permissions:
  contents: read
  pull-requests: read
  checks: read
  statuses: write
  actions: write

concurrency:
  group: review-verdict-${{ github.event.pull_request.number || github.event.issue.number || github.event.workflow_run.pull_requests[0].number || github.event.check_run.pull_requests[0].number || github.event.workflow_run.head_sha || github.event.check_run.head_sha }}
  cancel-in-progress: false

jobs:
  review-verdict:
    if: >-
      (github.event_name != 'issue_comment' || github.event.issue.pull_request)
      && (github.event_name != 'check_run' || github.event.check_run.name == 'fleet-guard/admission')
    runs-on: ubuntu-24.04
    timeout-minutes: 5
    steps:
      - name: Check out the pinned rule
        uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          repository: danielraffel/Shipyard
          ref: <40-hex Shipyard commit on main>
          persist-credentials: false
      - name: Set up Python
        id: python
        uses: actions/setup-python@ece7cb06caefa5fff74198d8649806c4678c61a1 # v6.3.0
        with:
          python-version: '3.12.12'
      - name: Post review-verdict
        env:
          GH_TOKEN: ${{ github.token }}
        run: '"${{ steps.python.outputs.python-path }}" scripts/review_verdict_status.py'
```
{% endraw %}

Bump the pinned commit when the rule changes, like any other pin.
