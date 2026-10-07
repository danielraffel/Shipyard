# Real GitHub responses: `pr-watch` golden replay

Every `NNNN.json` file is one request the `pr-watch` gatherer sends and the
answer GitHub gave, as `{"argv": [...], "response": "..."}`. They were
recorded from the live API for `Generous-Corp/pulp` on 2026-09-29 with

    shipyard pr-watch replay --repo Generous-Corp/pulp --since 7d \
      --until 2026-09-29T06:15:00Z --record <dir>

and then **trimmed**, not edited: only the answers for seven pull requests
were kept, and inside each answer only the items and fields the gatherer
reads.

- #8933: repeat test failure, repeated ejection, rebase treadmill, split.
- #8970: two failed merge groups named for it (and a failed_checks ejection),
  both built from head 47db0daf; the owner pushed 56282c2b at 08:44Z, so flag 3
  holds only from 08:30Z (second failure settled) to 08:44Z.
- #9012, #9018, #9019: armed, out of the queue, `macos` red for over 30 min.
- #9026, #9035: merged cleanly (the control: no flag may be raised).

Trimming rules:

- the pull-request search is one page holding the seven PRs' nodes verbatim;
- workflow-run listings keep the `pull_request` runs on those PRs' branches
  and the `merge_group` runs named for them plus their parent groups, with
  `total_count` set to the kept count;
- check-run and run-job listings keep only required checks, with the fields
  `id name status conclusion started_at completed_at`;
- job logs keep only the CTest failure block and `##[error]` lines;
- branch protection keeps only its required contexts; compare answers are
  verbatim `--jq` output.

Nothing secret is in any answer: they are public CI metadata of the
repository. The golden test is `tests/pr_watch_golden.rs`; the answers must
match the gatherer's requests exactly, so a change to what it requests
fails loudly ("no fixture for gh ...") instead of reading as absence.
