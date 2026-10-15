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

- the newest trusted verdict naming the head decides; a HOLD with no readable
  time always wins;
- `success` only for an APPROVE naming the head (a comment ending
  `verdict: APPROVE head:<sha>` or `reviewed: <sha>`, or an `APPROVED`
  review on it), or one the guard's merge-only carry takes to it;
- whose verdict counts is the guard's rule: the base's `reviewer_identities`,
  else an approval only from the fleet App or a human the base lists in
  `[auto_merge] human_reviewers` (a HOLD from any OWNER, MEMBER or
  COLLABORATOR still counts), never a distinguishable author;
- a repository whose base sets `arm_requires_head_approval = false` gets
  `success` with the reason.

The rebase carry is a per-host switch and is always off here.

Signers: when the base's `.shipyard/config.toml` sets `[auto_merge]
reviewer_signers`, an App APPROVE counts for the status only with a
`reviewer: <name>` line naming one of them. The status reads no host's
`reviewer-signers.json`.

A status belongs to a commit, but a decision belongs to a pull request, and
two open pull requests can share a head (one branch opened into two bases, a
fork pushing an approved commit). The status is `success` only when the guard
allows the head for every open pull request whose head it is
(`commits/{sha}/pulls`).

## Trust

Every trigger runs the default branch's definition of the poster:

- `pull_request_target` and `issue_comment` always do;
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
    types: [opened, reopened, synchronize, ready_for_review]
  issue_comment:
    types: [created, edited, deleted]
  workflow_run:
    workflows: [Review verdict trigger]
    types: [completed]

permissions:
  contents: read
  pull-requests: read
  statuses: write

concurrency:
  group: review-verdict-${{ github.event.pull_request.number || github.event.issue.number || github.event.workflow_run.pull_requests[0].number || github.event.workflow_run.head_sha }}
  cancel-in-progress: false

jobs:
  review-verdict:
    if: github.event_name != 'issue_comment' || github.event.issue.pull_request
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
