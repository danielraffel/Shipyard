# review_before_arm fixtures

Live captures (2026-10-12, through `ghapp`) of what the queue-arm guard reads to
decide whether a head is reviewed: the guard's own `PR_BY_NUMBER_QUERY`
GraphQL answer, `repos/<r>/pulls/<n>`, and the paginated (`--slurp`)
`issues/<n>/comments` and `pulls/<n>/reviews`.

| Files | Pull request | What it shows |
|---|---|---|
| `tartci-513-*` | danielraffel/tartci#513, open, unarmed | no verdict at all |
| `tartci-511-*` | danielraffel/tartci#511, open, unarmed | an APPROVE naming an older head only |
| `tartci-490-*` | danielraffel/tartci#490, open, unarmed | a HOLD naming the current head |
| `shipyard-784-*` | danielraffel/Shipyard#784, merged | HOLD on an older head, then APPROVE naming the final head |

`shipyard-784-graphql.json` is the response after the merge. The tests that
use it as an open pull request set `state` to `OPEN` and drop the
`MergedEvent`; they say so where they do. The HOLD-after-APPROVE case appends
one comment copied from the real APPROVE comment, with a later time and a HOLD
line. No fleet pull request in these repositories has a review (614 of 614
Shipyard PRs read `review:none`), so the review path is tested with GitHub's
documented review fields.
