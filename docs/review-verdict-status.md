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
  else the fleet App and OWNER, MEMBER or COLLABORATOR accounts, never a
  distinguishable author;
- a repository whose base sets `arm_requires_head_approval = false` gets
  `success` with the reason.

The rebase carry is a per-host switch and is always off here.

## Trust

The workflow runs on `pull_request_target`, `pull_request_review` and
`issue_comment`, checks out a trusted copy of the rule (the base branch, or a
pinned Shipyard commit), and never checks out or runs the pull request's
code. It reads every comment and review from the API, so an APPROVE from an
account the rule does not trust counts for nothing, whoever posts it.
Permissions are `contents: read`, `pull-requests: read`, `statuses: write`.
A `pull_request_review` on a fork's pull request runs with a read-only token
and cannot post; the next comment or push re-posts the status.

## Rollout

1. Report-only: the status posts but is not required. Compare it with the
   guard's decisions on real pull requests for two or three days.
2. Required, one repository at a time, starting with Vellum. A change to
   branch protection is reviewed before it is applied.

## Adding it to another repository

Shipyard runs `.github/workflows/review-verdict.yml` from its own base
branch. Another repository runs the same script from a pinned Shipyard
commit:

```yaml
name: Review verdict

on:
  pull_request_target:
    types: [opened, reopened, synchronize, ready_for_review]
  pull_request_review:
    types: [submitted, edited, dismissed]
  issue_comment:
    types: [created, edited, deleted]

permissions:
  contents: read
  pull-requests: read
  statuses: write

concurrency:
  group: review-verdict-${{ github.event.pull_request.number || github.event.issue.number }}
  cancel-in-progress: false

jobs:
  review-verdict:
    if: github.event_name != 'issue_comment' || github.event.issue.pull_request
    runs-on: ubuntu-latest
    timeout-minutes: 5
    steps:
      - name: Check out the pinned rule
        uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          repository: danielraffel/Shipyard
          ref: <40-hex Shipyard commit on main>
          persist-credentials: false
      - name: Post review-verdict
        env:
          GH_TOKEN: ${{ github.token }}
        run: python3 scripts/review_verdict_status.py
```

Bump the pinned commit when the rule changes, like any other pin.
