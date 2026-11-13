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

## The job is the check

The job is also the `review-verdict` check run, and it exits non-zero unless
the head is allowed, so a required `review-verdict` check cannot be green on a
refused, held or unapproved head. A read or post error fails the job too (it
never passes by default). The commit status is posted exactly as before.

Only the `pull_request_target` run leaves its check run on the pull request's
head; a dispatched (`workflow_dispatch`, input `pull_request`), a comment's
(`issue_comment`) or a review's (`workflow_run`) run leaves its check run on
the default branch's tip, where it gates nothing. Those runs
post the status and then re-run the head's newest `pull_request_target` run
(`actions: write`), so an APPROVE or a HOLD changes the head's check without a
new push. A re-run that cannot be requested fails the job.

Both listings the poster reads are paginated in full: every open pull request
sharing the head is judged, and the head's own run is found on any page. A
listing that is unreadable, malformed, or shorter than its `total_count`
fails the job instead of standing in for the whole one.

## Trust

Every trigger runs the default branch's definition of the poster:

- `pull_request_target`, `workflow_dispatch` and `issue_comment` always do;
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
  workflow_dispatch:
    inputs:
      pull_request:
        description: Pull request number to evaluate
        required: true
        type: string
  pull_request_target:
    types: [opened, reopened, synchronize, ready_for_review]
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
  group: review-verdict-${{ github.event.pull_request.number || github.event.inputs.pull_request || github.event.issue.number || github.event.workflow_run.pull_requests[0].number || github.event.check_run.pull_requests[0].number || github.event.workflow_run.head_sha || github.event.check_run.head_sha }}
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
