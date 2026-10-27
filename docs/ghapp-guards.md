# `ghapp` queue guards — operator reference

This page is for **operators**. It is where the override and bypass mechanisms
are documented. The guards' refusal messages and the agent skills deliberately
do not name them: an agent that reads an override in a refusal treats it as the
next step, and the refusal exists because the next step is something else.

## What runs

`scripts/ghapp` runs these guards from `$SHIPYARD_GHAPP_GUARDS_DIR`
(default `~/.config/shipyard/guards`) before the native `gh`. The two queue
guards are required for the requests they judge: when `queue-arm-guard` is
missing, unreadable or not executable, `ghapp` refuses every request that can
arm, enqueue or merge (`pr merge`, `enablePullRequestAutoMerge`,
`enqueuePullRequest`, `mergePullRequest`, a non-GET REST `pulls/<n>/merge`,
and a GraphQL body it cannot read) with ``ghapp: queue-arm-guard is not
installed; run `shipyard guards install` ``, and the same holds for
`queue-removal-guard` and the requests it judges (`pr merge --disable-auto`,
`dequeuePullRequest`, `disablePullRequestAutoMerge`). There is no override.
Other requests pass, and any other guard that is not present, or not
executable, is skipped. A queue guard counts as installed when its path
resolves to a regular, readable, executable file; a symlink to such a file
counts too (`shipyard guards install` never writes one), while a dangling
symlink, a directory or a non-executable file refuses. Anything the wrapper
cannot classify for certain (a body on stdin, a FIFO, a device or a symlinked
body file, a JSON body with escapes, a method-override header) counts as a
landing and a removal, so a missing guard refuses it.

### Upgrading a host

The guards `ghapp` runs are the installed copies in
`~/.config/shipyard/guards`, and `ghapp` itself runs from the auth generation
under `~/.local/share/shipyard/auth-generations/`. Neither changes when a new
Shipyard binary lands. Right after a release that changes them, on every
host (the fleet today: m1, m3, m5 and m5s):

1. `shipyard update` (or the fleet update), which installs the new binary and
   its `ghapp` auth generation;
2. `shipyard guards install`, which places this build's guards;
3. `shipyard guards status` and `shipyard doctor` to confirm both.

Until both steps run on a host, its `ghapp` paths keep the previous guards and
wrapper behaviour, so the protections a release adds there are not in effect.
Shipyard's own review check before each of its landings runs from the copy in
its binary and needs only step 1.

| installed name | source | refuses |
|---|---|---|
| `queue-removal-guard` | `scripts/ghapp_queue_removal_guard.py` | `pr merge --disable-auto`, `dequeuePullRequest`, `disablePullRequestAutoMerge` |
| `queue-arm-guard` | `scripts/ghapp_queue_arm_guard.py` | `pr merge --auto`, a plain `pr merge` whose base has a merge queue, `enablePullRequestAutoMerge`, `enqueuePullRequest`, when the PR's live state makes arming harmful (below); and those plus every direct merge (`pr merge`, `--admin`, REST `PUT pulls/<n>/merge`, `mergePullRequest`) when the head has no reviewer approval or the request does not bind it ([Review before arm or merge](#review-before-arm-or-merge)) |
| `branch-refresh-guard` | `scripts/ghapp_branch_refresh_guard.py` | `pr update-branch`, REST `pulls/<n>/update-branch`, `updatePullRequestBranch`, only when the base's `[merge] refresh_branch` policy is `"only-if-conflicting"` and the refresh is provably pointless (below) |
| `merge-guard` | `scripts/ghapp_merge_guard.sh` | `pr merge` on a repository listed in `~/.config/shipyard/merge-guard.json` (one that cannot enforce required checks server-side) until every listed check reads `pass`, and `--auto` there outright; inert for unlisted repositories and when that file is missing or malformed |
| `verdict-hold-hook` | `scripts/ghapp_verdict_hold_hook.py` | nothing: before a HOLD verdict posts, it dequeues or disarms that pull request when it is queued or armed at exactly the HOLD's head ([A HOLD takes its head out of the landing path](#a-hold-takes-its-head-out-of-the-landing-path)) |
| `verdict-session-stamp` | `scripts/ghapp_verdict_session_stamp.py` | nothing: it adds a `<!-- session: <host>/<session> -->` stamp to a verdict comment before it posts, so the arm guard can tell who approved ([An agent cannot approve its own landing](#review-before-arm-or-merge)) |
| `reviewer-claim-guard` | `scripts/ghapp_reviewer_claim_guard.py` | a verdict when the protected base enables reviewer-claim receipts and the posting reviewer has no current exact-head claim |

`pr-close-guard` is a mandatory member of the authenticated `ghapp` generation
and is managed by `shipyard runner fleet-update`, not by this page.

## Install the plain `gh` wrapper

To make direct `gh` calls use the same queue-removal policy, run
`scripts/install-guarded-gh.sh`. It installs only the `gh` wrapper in
`~/.local/bin`; guard files are owned and installed by `shipyard guards install`.
The obsolete `--guards-dir` option is refused. Put `~/.local/bin` before
Homebrew or `/usr/local/bin` in `PATH`; launchd, cmux, and Codex must inherit
that ordering. The wrapper falls back to `~/.config/shipyard/guards` when
`SHIPYARD_GHAPP_GUARDS_DIR` is unset. Verify with `command -v gh` and confirm
it names `~/.local/bin/gh` before using a plain `gh api` mutation.

### Queue-arm guard decisions

The guard reads the PR's live state with the same GraphQL query and classifier
as `shipyard landing --pr <n>` (see `docs/landing-model.md`).

| live state | decision |
|---|---|
| never armed, not queued | allow |
| removed from the queue, new commit or force-push since | allow |
| removed for `invalid_merge_commit`, same head | allow (GitHub failed to build the merge commit; nothing against the head, and the one reason Shipyard's own admission re-arms after) |
| already queued | refuse: nothing to do; REST `auto_merge` is `null` for every queued PR |
| auto-merge armed, not yet queued | refuse: the queue will pick it up |
| removed for `failed_checks`, same head, batch attributor certifies the head | allow (the repository ruled the ejecting batch's failure not this head's; see [Batch attribution](#batch-attribution)) |
| removed for `failed_checks`, same head, first ejection of that head, every failing required check failed on the network, repository opted in | allow, once (see [Environment re-enqueue](#environment-re-enqueue)) |
| removed for `failed_checks` / `merge_conflict`, same head | refuse: under ALLGREEN a same-head re-enqueue fails its batch-mates; push a fix first |
| removed for any other reason (`manual`, ...), same head | refuse: confirm with whoever dequeued it |
| merged / closed | refuse |
| unreadable, or a truncated timeline window that shows no removal and no new head | refuse |

A GraphQL body read from stdin (`--input -`, `query=@-`) cannot be inspected
and is refused as ambiguous.

### A tiered required check must prove its full lane

A repository can gate an expensive lane behind a fleet-App dispatch. On a
pull request run, its required check runs only a cheap contract lane and
concludes `success`; the full lane runs when the fleet App dispatches the
workflow on the head. GitHub cannot tell that success from the full lane's,
so native auto-merge would enqueue the head as soon as it is armed.

**Opt in** by naming the checks, and the one App whose tier records count, in
the base branch's `.shipyard/config.toml` (read through the API from the base,
never from the head):

```toml
[queue]
tiered_required_checks = ["macos"]
tier_record_app = "pulp-tier-records"
```

A repository without `tiered_required_checks` pays one config read per landing
and is otherwise unaffected. One that names tiered checks without
`tier_record_app`, or names `github-actions` or the fleet App
`shipyard-local`, refuses every landing until the config is fixed.

**Where the tier comes from: a tier record, never an annotation.** Any step of
a job, including the code under test, can print a `::notice`, so a later
annotation could claim a wider tier, and enough annotations push the real one
past GitHub's caps. The guard ignores tier annotations. The tier is read from
a *tier record*: a check run on the pull request's head named
`shipyard-test-tier/<check>` (for example `shipyard-test-tier/macos`),
created by the `tier_record_app` App through the Checks API, whose
`output.summary` is one JSON object:

```json
{"schema":"shipyard-test-tier/v1","check":"macos","tier":"deferred",
 "run_id":123,"run_attempt":1,"trigger":"pull_request","actor":"<author>",
 "selector":"pr-fast+pr-guard","full_lane_workflow":"build.yml",
 "tested_ref":"refs/pull/42/merge","tested_sha":"<40-hex merge commit>",
 "merged_base":"<its base parent>",
 "full_suite_runs_in":"merge_group"}
```

`run_id` and `run_attempt` are the workflow run and attempt whose job is the
check, so a record from an earlier run or attempt never speaks for a rerun.
`trigger` and `actor` name the event and account that started the run; a
full lane is a `workflow_dispatch` by `shipyard-local[bot]`. The guard reports
them but does not need them: the record App already decided the tier.
`full_lane_workflow` names the workflow whose dispatch on the head, by the
fleet App, runs the full lane. A label is not a full-lane path: a `labeled`
run of the workflow shadows the required check the merge queue reads (Pulp
#11044 removed `labeled` from `build.yml` for that reason). The record carries
no label.

`tested_ref`, `tested_sha` and `merged_base` say what the lane checked out: the
ref (expected `refs/pull/<n>/merge`), the merge commit actually tested, and its
base parent (GitHub's merge ref puts the base first and the head second). A
full lane counts only when all of these hold:

- `tested_ref` is `refs/pull/<n>/merge` for this pull request;
- `repos/<o>/<r>/commits/<tested_sha>` has parents exactly
  `[merged_base, head]`, in that order (GitHub's), with `head` the pull
  request's current head;
- `merged_base` is on the base branch (`compare <merged_base>...<base>` is
  `identical` or `ahead`).

A lane that checked out the bare branch (`tested_sha` is the head itself)
tested the head alone, not what the queue would merge; a merge whose second
parent is an older head is stale; a merge with the head first is not GitHub's
merge ref. All refuse, as does a full record missing `tested_ref`,
`tested_sha` or `merged_base`. A dispatch runs on a branch ref,
so the full-lane dispatch names the pull request with the `pr_number` input
and checks out `refs/pull/<n>/merge`.

**Why a dedicated App, not the workflow's own token.** GitHub lets a check run
be updated only by the app that created it. Every job's `GITHUB_TOKEN` acts as
`github-actions`, and a pull request job can hold `checks: write` (the default
under permissive token settings), so a record `github-actions` created could be
PATCHed from `deferred` to `full` by the code under test. A check run carries
no update time, so nothing would show it. Records from `github-actions` never
count. A record created by a dedicated App cannot be touched by pull request
code at all, provided only a workflow pull requests cannot edit holds the
App's key. The fleet App is refused for the same reason: every agent posts as
it.

**Publishing the record.** Use a `workflow_run` workflow on the default
branch, which runs the default branch's definition (a pull request cannot
edit it) and is the only holder of the record App's private key. The record
must be decided from facts pull request code cannot write, never from the
run's outputs, artifacts or logs.

**Custody of the record App's key** is what the whole rule rests on: whoever
can mint the App's token can forge a record.

- Keep it in an **Actions Environment secret** whose deployment-branch policy
  allows only the default branch, and give only the `workflow_run` publisher
  job that `environment:`. A **repository-level secret is not enough**: a
  same-repository pull request's runs can read it.
- Never keep the key, or a token minted from it, on a host where agent
  sessions run as the same OS user. A same-user agent could read it, mint a
  token and post a record. This is the same limit as any same-user secret on
  an agent host.
- Where a host must hold it, hold it under a separate OS user that agent
  sessions cannot act as, and have that user publish.

**The tiered job may not hold `checks: write` either (defence in depth).** A
same-repo pull request can grant its jobs `checks: write` in its own workflow
file, whatever the repository default is, and a check run carries no update
time to show a rewrite. So before a head is allowed, the guard also reads the
tiered job's log (`repos/<o>/<r>/actions/jobs/<id>/logs`) for the
`GITHUB_TOKEN Permissions` group the runner prints in "Set up job". `Checks`
must be `read` or `none` (not listed is `none`). A log that cannot be read, or
a group that is missing, never closes or holds a line that is not
`<Scope>: read|write|none`, refuses. The log is read only when the records
would otherwise allow, so a refusal costs nothing extra.

Publisher sketch:

{% raw %}
```yaml
on:
  workflow_run:
    workflows: [Build]          # the workflow whose job is the tiered check
    types: [completed]
jobs:
  tier-record:
    runs-on: ubuntu-latest
    # The key is an Environment secret; the environment's deployment-branch
    # policy admits only the default branch, so no pull request run reads it.
    environment: tier-record
    permissions: {}
    steps:
      - id: app
        uses: actions/create-github-app-token@v1
        with:
          app-id: ${{ vars.TIER_RECORD_APP_ID }}
          private-key: ${{ secrets.TIER_RECORD_APP_KEY }}
      - env:
          GH_TOKEN: ${{ steps.app.outputs.token }}
          RUN_ID: ${{ github.event.workflow_run.id }}
          RUN_ATTEMPT: ${{ github.event.workflow_run.run_attempt }}
          HEAD_SHA: ${{ github.event.workflow_run.head_sha }}
          CREATED_AT: ${{ github.event.workflow_run.created_at }}
        # Decides the tier from facts the runner or GitHub recorded, never
        # from a label timeline: `fast` or `full` only for a
        # `workflow_dispatch` run whose triggering actor is the fleet App
        # (`shipyard-local[bot]`) and whose head_sha is HEAD_SHA, and whose
        # `macos` job's step conclusions (jobs API, exactly this attempt) show
        # the full lane ran. A `pull_request` run, a dispatch by anyone else,
        # or anything it cannot place publishes `deferred` or nothing. Then
        # creates the shipyard-test-tier/macos check run on HEAD_SHA,
        # completed, with output.title = tier and output.summary = the JSON
        # above: `full_lane_workflow`, plus the `tested_ref`, `tested_sha` and
        # `merged_base` the lane checked out (the runner-recorded checkout,
        # never the head unless that is what it tested).
        run: ./.github/scripts/publish-tier-record.sh
```
{% endraw %}

**The rule.** For each tiered check, the guard takes the latest check run of
that name on the exact head (highest id):

1. Missing or not `completed`: refuse. Arm-on-open runs before the checks do,
   and a head armed then would be enqueued the moment a deferred lane went
   green. The steward's native-arm backstop (`shipyard runner steward
   --arm-unqueued`) arms it once it is green and its tier is full.
2. Not a GitHub Actions job, or its job (`repos/<o>/<r>/actions/jobs/<id>`)
   cannot be read: refuse.
3. Of the tier records bound to that job's `run_id` and `run_attempt` (name,
   app `tier_record_app`, `completed`, a valid `shipyard-test-tier/v1` summary
   naming the check): any `deferred` refuses, and none at all refuses. A later
   record cannot widen an earlier `deferred` one, so a forged record can only
   refuse.
4. Every remaining record must have tested this pull request's merge ref at
   the current head (`tested_ref`, `tested_sha` parents `[merged_base, head]`,
   `merged_base` on the base, all above): a bare branch, a stale merge, the
   reversed order or a missing field refuses.
5. The tiered job's token held `checks: write`, or its `GITHUB_TOKEN
   Permissions` log group cannot be read or parsed: refuse.

An unreadable config or a partial check-run listing refuses. The refusal
prints a `queue-arm-guard: fix:` line: for a deferred tier or an untested
merge, "dispatch `build.yml` with `pr_number=<n>` as the fleet App and wait
for the full lane" (the record's
`full_lane_workflow`; without one, "dispatch the workflow of `<check>`").

No override applies: `GHAPP_ALLOW_QUEUE_REARM=1` does not land such a head.

Shipyard applies the same rule itself (`src/deferred_tier.rs`, mirrored from
`tier_blockers` in the guard) on every path that arms, enqueues or merges:
arm-on-open in `shipyard ship`/`shipyard pr`, the validated arm and classic
merge of `shipyard auto-merge` (`head_approval_verdict`), the steward's
native-arm backstop (`ArmSkip::DeferredTier`) and managed enqueue
(`waiting_full_test_tier`), the runner carrier's exact-head arm (left
disarmed), and `base-health`'s jump enqueue (`refused: ...`). Arm-on-open and the backstop also go through this guard;
the others carry the internal marker and go past it, so each checks for
itself.

### Review before arm or merge

An arm or enqueue the table above allows, and any direct merge, is still
refused when the repository requires review and nothing approves the PR's
current head. The direct merges are `pr merge` on a base with no queue, `pr
merge --admin`, REST `PUT pulls/<n>/merge` and GraphQL `mergePullRequest`; on
a queue-less repository (tartci, Shipyard) they are the landing path.

**Which hosts and repositories.** The rule is on by default, in both twins
(this guard and Shipyard's own arm paths). An opt-in host switch left most of
the fleet unguarded: on 2026-10-14 an agent on a host without the switch file
queued an unapproved pull request and one on HOLD. The only way off is a file,
`review-before-arm.opt-out` in the guards directory:
`shipyard guards install --opt-out-review-before-arm "<reason>"` writes it
with the reason, time and binary and appends to `switches.jsonl`, and
`--opt-in-review-before-arm` removes it, logged too. Only that record counts:
a regular file holding JSON with a non-empty `reason`, `at` and `by`. Anything
else at the path (an empty or hand-made file, a directory) keeps the rule on,
and `guards status`, `doctor` and fleet-update verification fail on it until
it is fixed or removed, so a stray file cannot opt a host out. Nothing in the
environment turns it off (`SHIPYARD_REVIEW_BEFORE_ARM=1` only forces it on).
While a host is opted out, every landing prints that review is OFF and why,
and the arms ledger records `review: off: <reason>`; `shipyard guards status`
and `shipyard doctor` show the host as OFF. Off, the guard reads nothing for
this rule (no pull request, no config, no comments), so it is inert whatever
the base config says and on any Python.

**An agent cannot approve its own landing.** `verdict-session-stamp` (run by
`ghapp` before native `gh`; `ghapp` waits up to 30 seconds for its answer,
and refuses a verdict it did not answer for, "verdict NOT posted, retry",
rather than post it unstamped) adds
`<!-- session: <host>/<session> agent: <id> -->` above the marker line of a
verdict comment, posted with `--body`, `--body-file <file>`, an `api`
`body=` field (a typed `-F body=@<file>` is read), or `--input`; `ghapp`
first spools a body read from stdin (`--body-file -`, `--input -`,
`-F body=@-`) to a private file, so stdin is covered too. What counts as a
verdict is the arm guard's own `marker_verdict`, loaded from the installed
`queue-arm-guard`, so a body ending in `\r\n`, a lone `\r` or U+2028 is a
verdict to the stamper exactly when it is to the guards (`head_approval`
splits lines the same way). Every stamp already in any comment body, verdict
or not, whether written by hand or copied with the rest of a verdict, is
removed, repeatedly until none is left, since one pass can join the pieces
around a removed stamp into a new one; only a verdict then gets this
session's stamp. Bodies are read byte for byte (`surrogateescape`), so a
stray non-UTF-8 byte cannot crash the stamper. Comments it stamps or strips:
`pr comment`, `issue comment` and `pr review` bodies, `pr close` and `issue
close` comments, and `api` calls to an issue-comment endpoint however it is
written (a query, a trailing slash or an API host is dropped first), every
`body=` field and an `--input` body among them.

The stamp is optional; other stamp text is not. After the stamper, whatever
it answered, `ghapp` checks the final request and every body source gh will
read (arguments, fields, `--input`, `-F k=@file`, body files and spooled
stdin) on every command except `pr create`, `pr edit`, `issue create` and
`issue edit` (descriptions, which no review guard reads), and refuses it if
any carries stamp text other than the one stamp line the stamper reports
writing (`SHIPYARD-STAMP-OWN` in its answer). Stamp text is matched wider
than any guard reads it: `!--` then `session` in any case before the next
`>`, whatever bytes lie between (Unicode whitespace), with a `\u` escape
(GitHub decodes one in a GraphQL string or `--input` JSON) or a `&#`/`&lt`
entity decoded first; text that cannot be decoded is refused.

The guards count a stamp only in the exact ASCII form the stamper writes,
`<!-- session: <host>/<session>[ agent: <id>] -->` with single spaces, and an
App APPROVE carrying any other stamp-like text (`<!--`, whitespace,
`session:` in another spacing, case or Unicode whitespace) is refused as
"non-canonical", never read as stamped. A body file
that is not a regular file (a FIFO) cannot be checked and is refused; an
`api` output filter (`--jq`, `--template`) is never sent and is not checked.
A request without stamp text posts as before. The session is `CLAUDE_CODE_SESSION_ID`, else
`CODEX_COMPANION_SESSION_ID`, else `unattributed`; `agent: <id>` appears only
when `SHIPYARD_AGENT_ID` is set. The marker stays the last line, so every
verdict reader is unchanged. The arm guard, and `head_approval` for Shipyard's
own paths, refuse an arm only when the deciding approval is stamped with an
agent id and the arming process carries the same `SHIPYARD_AGENT_ID` (cause
`self_approval`). The session never refuses on its own: every agent of one
Claude Code team (implementer, reviewer, lead) shares `CLAUDE_CODE_SESSION_ID`,
so it cannot tell them apart. When only the session matches, the arm guard
prints a warning and flags the ledger entry `same_session_approval: true`. An
unstamped verdict (older comments, the web UI, a review), or either side
without an agent id, behaves exactly as before. The stamp is not a signature:
any writer can type one; it stops an agent approving its own work by habit,
not a determined forger.

**The arms ledger.** Every arm, enqueue or merge the guard judges, allowed or
refused, appends one line to `queue-arms.jsonl` in
`${XDG_STATE_HOME:-~/.local/state}/shipyard/` (`SHIPYARD_QUEUE_ARMS_LOG`
overrides): `at`, `host`, `session`, `repo`, `pr`, `head`, `verb` (`arm` or
`merge`), `verdict` (`allowed` or `refused`) and `review`, plus `agent` when
`SHIPYARD_AGENT_ID` is set and `same_session_approval` when the approval
carried the arming session. Override removals in `queue-removals.jsonl` now
carry `host`, `session` and `agent` too. A missing session
is recorded as `unattributed` and never refuses anything; a ledger that cannot
be written is reported and does not block. Shipyard's own marked mutations
(`SHIPYARD_INTERNAL_QUEUE_MUTATION=1` with that call's one-shot nonce, below)
are not judged again, but each is
recorded with `review: shipyard internal` from what the request itself binds,
with no GitHub call: repository, pull request (or, for a GraphQL landing, the
`node` id and no number), head and verb. An entry also records
`excluded_approvals` when it ignored a human approval (below). `shipyard who --pr N` joins both ledgers,
read from every `[host_class.*]` host over SSH, to the pull request's
timeline by repository, pull request, verb and head, with time only breaking
ties, and prints its own time to answer. It names `host/session`, plus the
agent when the ledger recorded one; without an agent id that is the team
session, not which agent within it. An event no ledger explains (a
landing through native `gh`, which passes no guard) is `unattributed`, never
guessed.

Off differs in one place between the two twins, deliberately. Shipyard's own
arming and merge paths (Rust) still honour a repository that sets
`[auto_merge] arm_requires_head_approval = true` itself, on every host: that
key is the repository's own opt-in and predates the host switch. The guard
cannot: off means it reads nothing, which is what keeps it inert on a Python
without `tomllib`. No fleet repository sets the key today, so off hosts behave
the same through both paths; a repository that sets it should expect raw
`ghapp` landings on off hosts to pass the guard.

With it on, the guard reads `.shipyard/config.toml` on the PR's base:
`[auto_merge] arm_requires_head_approval` decides when set, either way. When
the base is silent, the repositories in `REVIEW_REQUIRED_REPOS`
(`Generous-Corp/pulp`, `Generous-Corp/vellum`, `danielraffel/tartci`,
`danielraffel/Shipyard`) require review. A config that exists but cannot be
read or parsed (invalid TOML, or a Python without `tomllib`) refuses rather
than loosening. Shipyard's own arming and merge paths use the same switch for
the list default; a repository's own config key applies to them on any host.

**What approves.** The newest verdict naming the head decides:

* a comment whose last non-blank line is `verdict: APPROVE head:<40-hex>` or
  `reviewed: <40-hex>`, or
* an `APPROVED` review whose `commit_id` is the head.

`verdict: HOLD head:<40-hex>` or a `CHANGES_REQUESTED` review on the head
blocks an earlier approval. A HOLD whose time cannot be read always blocks.

**Whose verdict counts.**

* With `reviewer_identities` listed: only those identities, never the PR's
  author and never the fleet App `shipyard-local[bot]`.
* With none listed (the default for the four repositories): an approval
  counts only from the fleet App, which every agent posts as through
  `ghapp`, or from a human the base lists in `[auto_merge] human_reviewers`
  (empty by default) whose `author_association` is `OWNER`, `MEMBER` or
  `COLLABORATOR`. A human approval outside that list does not count: an agent
  holding the owner's own `gh` login posted `APPROVED` reviews a minute
  before arming its own pull requests (pulp#10293, #10291). Each one ignored
  is printed (`ignored the approval of <login>`) and recorded in the arms
  ledger's `excluded_approvals`, and Shipyard's own paths report it as
  `excluded_human_approval:<login>`. A HOLD from an `OWNER`, `MEMBER` or
  `COLLABORATOR` still counts: it can only stop a landing. Nobody else: the
  repositories are public, and a drive-by account's comment must not land a
  head. A distinguishable PR author never approves their own head, listed or
  not. List `reviewer_identities` to make it strict.
* The fleet App is recognised by its login **and** its type: a verdict counts
  as the App's only when the author is a `Bot`. GitHub's GraphQL names the
  App `shipyard-local`, a login anyone can register as a user, so a user
  account of that name counts for nothing.
* The App's identity says nothing about which agent reviewed, since every
  agent posts as it. So an App APPROVE does not count when `ghapp` stamped it
  with an `unattributed` session (pulp#10315: a bare APPROVE from a session
  with no identity armed the pull request), and the repository-side
  `review-verdict` rule also refuses one with no stamp at all, since a body
  posted around the stamper names no session (tartci#623's copy, stamp
  removed). On a host whose guards directory
  holds `reviewer-signers.json`, an App APPROVE counts only with a
  `reviewer: <name>` line naming an allowed signer (pulp#10320: an APPROVE
  with no reviewer of record). The file is
  `{"default": ["pulp-review", "infra-review2"], "repos": {"OWNER/REPO": [...]}}`;
  a repository's own list replaces the default, and a file that cannot be
  read admits nobody. A protected base can name its own signers instead, with
  `[auto_merge] reviewer_signers = [...]` in `.shipyard/config.toml`; that
  list takes precedence over the host file, and it is the one the server-side
  `review-verdict` status applies (the status reads no host's file). Without
  either, the signer rule is off on that host;
  roll it out one host at a time.
* A signer line can be copied, so a host can also list the exact `session:`
  lines its reviewers write, under a `sessions` key in the same file:
  `{"sessions": ["session: pulp-review (team-lead team, agent pulp-review, host m3)"]}`
  (the `session:` prefix is optional in an entry). With the list present, an App
  APPROVE counts only when it carries at least one `session:` line and every
  one of them is listed; the `<!-- session: ... -->` stamp `ghapp` adds is not
  such a line (pulp#10563: a `Daniels-M5-Studio` session signed its APPROVE
  `reviewer: pulp-review`). A `session:` line can be copied too (tartci#623
  carried infra-review2's line, `host m3`, under a `Daniels-M5-Studio` stamp),
  so with the list present the APPROVE must also carry exactly one `ghapp`
  stamp, not `unattributed`, whose hostname is one the host named in each
  `session:` line maps to under a `session_hosts` key:
  `{"session_hosts": {"m3": ["Daniels-Mac-Studio-m3"], "m5": ["Daniels-M5-Studio"]}}`.
  Hostnames compare without case; list every name a machine has stamped
  under, since macOS can rename one (`-2`, `-4`) on a clash. A missing stamp,
  two stamps, a `session:` line with no `host <name>`, a host with no entry,
  a missing or malformed `session_hosts` value, or a stamp from another
  machine refuses the APPROVE. An empty list, a `sessions` value that is not a list
  of strings, or a file that cannot be read admits no App APPROVE. With no file,
  or a file without the key, the rule is off and the guard prints a warning
  once. The list is host-local: the server-side `review-verdict` status never
  reads it. Each refusal is printed and recorded in
  `excluded_approvals`, and Shipyard's own paths report
  `excluded_app_approval:<why>`. A HOLD never needs a signer.

**The request must bind the head.** `gh pr merge` needs
`--match-head-commit <sha>`, a GraphQL mutation `expectedHeadOid`, and the
REST merge `sha=`, equal to the head the verdict approved. An unbound request
is refused with the flag to add, so a push between the check and the
mutation cannot land a head nobody reviewed.

**Merge-only updates.** Updating an approved head with its base makes a new
head no verdict names. The approval carries to it only when all of these hold:

* the new head descends from the approved one;
* every commit on the first-parent chain back to the approved head is a
  two-parent merge whose merged parent is on the base (the other commits in
  the range arrived through those merges);
* every file the PR changes has the same blob at the new head as at the
  approved head (`files[].sha` of `compare/<base>...<head>`), so a merge that
  moves a line or swaps content cannot pass;
* every file the PR changes keeps its mode. The comparison lists no mode, so
  the guard reads each file's directory tree at both heads (GraphQL
  `object(expression: "<sha>:<dir>")` entries' `mode`); a merge that only
  flips an executable bit keeps the blob but not the mode;
* no file the PR changes was also changed by the base between the two merge
  bases, a rename counting under both names on either side (so a base edit to
  a file the PR renames from refuses), and each renamed file keeps the source
  it was approved with;
* the PR changes no binary file. A file GitHub shows no patch for is read as
  a blob and is binary only if a NUL byte appears in its first 8000 bytes,
  git's rule: GitHub also omits the patch of a text file too large to show,
  such as a one-line JSON fixture of hundreds of KB. In a public repository
  the guard reads just those bytes with a ranged request to
  raw.githubusercontent.com; a private repository (gh sends no credentials
  there) and the Rust twin read the whole blob and require its exact size.
  A blob that cannot be read refuses as unknown, never as binary.

Anything else, or anything the guard cannot read (including a comparison at
GitHub's 300-file cap), needs a new verdict. A HOLD naming the new head is
never overridden by a carry.

Three guardrails apply to every carry, merge-only or across a rebase:

* **Excluded paths never carry.** A PR that changes (or renames from) a path
  matching `[auto_merge] carry_excluded_paths` in the base's
  `.shipyard/config.toml` needs a verdict naming its exact head. Without the
  key, the defaults are:
  * `.github/workflows/**`, `.agents/contract.toml`, `tools/ci/**`, and
    `.shipyard/**`, whose config is the review, carry and train policy itself;
  * the guards and Shipyard's own arm code: `scripts/ghapp*`,
    `src/head_approval*` and `src/head_approval/**`, `src/queue_arm_gate*`,
    `src/auto_arm*` and `src/auto_arm/**`, `src/ghapp_guards*`,
    `src/app/ship_cmd/auto_arm*` and `src/app/merge_steward_cmd/native_arm*`;
  * signing and release automation: `release-please-config.json`,
    `.release-please-manifest.json`, `**/*codesign*`, `**/*notariz*` and
    `**/*signing*`.

  The key replaces the list, so `[]` excludes nothing; a list that replaces
  must restate every default it keeps. Two more keys adjust the list without
  restating it:
  * `carry_excluded_paths_extra` adds globs (to the defaults, or to the
    replacing list when `carry_excluded_paths` is set);
  * `carry_excluded_paths_allow` re-includes paths the defaults exclude. Each
    entry is a repository path or `<directory>/**` that falls under a default
    glob, and never one overlapping `.github/workflows/**`,
    `.agents/contract.toml`, `.shipyard/config.toml`, `tools/ci/**`, the
    guards and arm code, or a signing name (`**/*codesign*`, `**/*notariz*`,
    `**/*signing*`). A signing name under an allowed directory still never
    carries, and an allow never re-includes a rollup's own `exclude_paths`.
    For example, `carry_excluded_paths_allow = [".shipyard/notes/**"]` lets
    a PR changing only `.shipyard/notes/` carry while `.shipyard/config.toml`
    still needs an exact-head verdict.

  A value that is not a list of globs, a `!` entry in the first two keys, or
  an allow entry refused by the rule above means no carry at all; the guard
  names the refused entry. In a glob, `**` is any number of whole path
  segments, `*` any run of characters within one segment, and `?` one
  character.
* **`carry: no`.** An APPROVE (comment or review body) with a line
  `carry: no` approves its exact head only.
* **A cap.** An approval carries across at most 3 base merges, and for at most
  48 hours after its verdict. Past either, the head needs a new verdict.

Every merge-only carry a landing accepts is appended to the guards directory's
`carries.jsonl`, as rebase carries already were, with `kind: merge`:
`approved` and `head`, both merge bases (`old_base`, `new_base`), the number
of `merges`, and the deciding verdict's `verdict_comment_id` (or
`verdict_review_id`) and `signer`. The Rust twin (`head_approval`) applies the
same three guardrails and logs its merge carries the same way. Only landings
log: a read-only evaluation judges the carry without appending a row, rebase
or merge. Such evaluations include the `review-verdict` status, through
`review_gate(..., dry_run=True)` in Python and `ApprovalPolicy::dry_run` in
Rust, and `shipyard train status`. So the log holds only carries that
actually armed or merged.

**Rebases (separate switch, off by default).** A rebased head does not descend
from the approved one, so the rule above never carries to it. On a host that
turns the rebase carry on (`SHIPYARD_REBASE_CARRY=1`, `0` forces it off, or a
file named `rebase-carry` in the guards directory), the approval carries
across a rebase only when all four hold, and the refusal names every one that
fails:

* the pull request's diff (each head against its own merge base, read as
  `application/vnd.github.diff`) has the same `git patch-id --stable`;
* it changes the same files, with the same status and, for a rename, the same
  source;
* re-applying the approved diff onto the new merge base gives exactly the new
  head's tree. That is proved rather than computed: the new merge base
  descends from the old one, the base touched none of the pull request's
  files between the two merge bases (changed, renamed or deleted: a rename
  counts under both names, on the base's side and on the pull request's, so a
  base edit to a file the pull request renames from refuses), and every file
  keeps its approved blob and mode,
  so a conflict resolved by hand never carries. A binary change, or a base
  comparison at GitHub's 300-file cap, refuses. Conditions 1 and 3 must agree:
  a changed patch-id refuses even when condition 3 holds;
* no required check on the new head has concluded `skipped` or `neutral`, or
  carries a `shipyard-receipt-decision` annotation with verdict `reuse` (see
  [validation signals](validation-signals.md)). The carry covers the review
  only; required checks run on the new head as on any other. A rebase always
  makes a new tree, so a receipt bound to the approved head's exact tree never
  applies to the carried head.

Every carry appends one JSON line (repository, pull request, both heads, both
merge bases, the patch-id, the file count, argv) to `carries.jsonl` in the
guards directory and prints a line saying so. A HOLD naming the new head still
refuses. Roll it out one host at a time; roll back by removing the file.
Shipyard's own arm paths read the same switch and apply the same four
conditions (`head_approval::rebase_carry`, the Rust twin); their carries are
logged to the same file with `"by": "shipyard"`.

The refusal names the head, the exact marker it needs, and the latest verdict
when it approves a different head. Each refusal is followed by one
`queue-arm-guard: fix:` line of JSON with `what`, `evidence` (the pull
request's URL), `cause` (`unreviewed`, `hold`, `approval_does_not_carry`,
`unbound`, `head_moved` or `unreadable`), `command`, `safe_to_auto_apply` and
`verify`:

```
queue-arm-guard: refusing to arm or merge an unreviewed head: PR #511 (danielraffel/tartci): no reviewer verdict names head 52eda5490844 ... Each `queue-arm-guard: fix:` line below says what to do next.
queue-arm-guard: fix: {"cause": "unreviewed", "command": "a reviewer agent reviews head 52eda549... and posts, through `ghapp` (the fleet App), a comment ending `verdict: APPROVE head:52eda549...` on https://github.com/danielraffel/tartci/pull/511; a human approval counts only for a login the base lists in `[auto_merge] human_reviewers`", "evidence": "https://github.com/danielraffel/tartci/pull/511", "safe_to_auto_apply": false, ...}
```

Only a re-read (`head_moved`, `unreadable`) is safe to apply unattended.
Binding a request (`unbound`) is still a landing, and approving is a
reviewer's act: an agent landing its own head must not post the verdict.

An unreadable pull request, policy or comment list refuses.

**No override.** This rule has none. `GHAPP_ALLOW_UNREVIEWED_ARM=1` is
refused: the guard says it is not honoured, appends one JSON line (`at`,
`override`, `refusal`, `argv`, `cwd`, `honoured: false`, `declined`) to
`overrides.jsonl` in the guards directory, and the refusal stands.
`GHAPP_ALLOW_QUEUE_REARM` does not stand in for a review: a queue refusal it
overrides still needs the verdict, and a request that names no head (a
GraphQL body on stdin) is refused whatever is set.
Shipyard's own validated enqueue and classic merge carry
`SHIPYARD_INTERNAL_QUEUE_MUTATION` and apply the same rule in Rust
(`crate::head_approval`). They no longer
rely on that twin alone: before each arm, enqueue or merge (`ship`,
`auto-merge`, the merge steward, the carrier, a base-health jump) Shipyard runs
this guard itself, from the copy it bundles, with
`SHIPYARD_QUEUE_ARM_REVIEW_ONLY=1`. In that mode the guard applies this review
rule and nothing else (Shipyard judges queue state), ignores the internal
marker and records every attempt, allowed or refused, with `caller`,
`verdict_comment_id` and `signer`, to the ledger `SHIPYARD_QUEUE_ARMS_LOG`
names: `queue-arms.jsonl` in Shipyard's state directory. That mode, like the
internal marker, is honoured only with the one-shot nonce Shipyard issues for
the call (Environment switches, below); without one the guard applies every
rule. `ship --arm` and `pr --arm` ask only for an arm the verdict allows.

### A rollup lands on its members' own approvals

A rollup head (branch `shipyard-train/rollup-*`, on a base whose
`.shipyard/config.toml` sets `[train] enabled = true` and `mode = "rollup"`)
has no verdict of its own. When no verdict names it, the guard
(`rollup_acceptance`) and Shipyard's gate (`crate::rollup::accept`) accept it
only when every merge on its first-parent chain merges the exact head of an
open, non-draft member pull request whose own review counts at that head (a carried
approval does not, by default), each merge changes exactly that member's
files to its approved blobs and modes, the members change disjoint files,
the base touched none of them since, and no member changes an excluded path.
A HOLD naming the rollup head stops it. `SHIPYARD_TRAIN=off` or a
`train.pause` file in the guards directory refuses every rollup on the host.
The notice comment on the rollup pull request is never read. See
[rollups.md](rollups.md).

### A HOLD takes its head out of the landing path

A HOLD posted on a head that is already armed or queued did not stop it: on
2026-10-12/13 six Generous-Corp/pulp pull requests merged minutes after a
`verdict: HOLD` naming their merge head, because the review rule judges only
the arm. `verdict-hold-hook` makes the verdict act.

**What it recognises:** a HOLD being posted on a pull request through `ghapp`,
in any of these forms:

* `pr comment <selector>` (`--body`, `--body-file`), the selector anywhere
  among the arguments: a number, `#N`, a pull request URL, or none (the
  current branch's pull request);
* `pr review <selector> --request-changes` (a HOLD on the current head
  unless the body names one), or a `pr review` whose body is a HOLD;
* `api repos/O/R/issues/N/comments` (`--input` JSON `body`, or `-f/-F body=`,
  `body=@FILE`);
* `api -X PATCH repos/O/R/issues/comments/ID`, editing a comment into a HOLD:
  the pull request comes from that comment's `issue_url`;
* `api repos/O/R/pulls/N/reviews` with `event: REQUEST_CHANGES` (on its
  `commit_id`, else the current head) or a HOLD body;
* `api graphql` `addComment` with a HOLD body on a pull request, and
  `addPullRequestReview` with `REQUEST_CHANGES` or a HOLD body.

A body is a HOLD when its last non-blank line is `verdict: HOLD head:<sha>`.

**What it does:** it reads the pull request; only when its current head is
exactly that sha, it dequeues it (`dequeuePullRequest`) if queued, else
disables auto-merge (`disablePullRequestAutoMerge`) if armed. Another pull
request, another head, or one neither armed nor queued is left alone. Every
action and every failure is a line in `holds.jsonl` in the guards directory.
The head lands again only once a newer verdict approves it, which
`queue-arm-guard` requires on hosts with review before arm.

**The one sanctioned route around `queue-removal-guard`.** The hook's mutation
goes to the real `gh` with the exported App token, not back through `ghapp`, so
`queue-removal-guard` keeps refusing agents' own dequeues and disarms. On a
host with the switch on, posting a HOLD that names a pull request's head is
therefore the one way to dequeue or disarm it without the removal override,
and the public HOLD is its trace. Do not add another.

**It never refuses, and a failure is public:** `ghapp` runs it with `|| true`,
after the refusing guards, and it exits 0. When it recognises a HOLD but the
read or the dequeue/disarm fails, it posts a follow-up comment on the pull
request ("this HOLD could not dequeue/disarm head <sha>: <error>; it may still
land"), so GitHub never shows an ineffective HOLD as an effective one. When it
cannot read a comment's or review's body (`--input -`, `body=@-`,
`--body-file -`), or cannot tell which pull request it is on, it prints that a
HOLD in it was NOT acted on.

**It is the fast path, not the guarantee.** It sees only HOLDs posted through
`ghapp` on a host that turned it on: a reviewer on another host, a Codex
session, or the web UI bypasses it. A repository with a merge queue needs the
server-side backstop as well: a cheap merge-group preflight that fails the
group when a queued pull request's latest trusted verdict names its head as
HOLD. In Generous-Corp/pulp that is `tools/ci/merge_group_hold_check.py` in the
`classify` job, which runs the protected base branch's copy of the check and
fails `macos` closed before any VM.

The `review-verdict` status (see [review-verdict-status.md](review-verdict-status.md))
is the repository-side twin of the review-before-arm rule: once required, it
also stops GitHub-native auto-merge armed before a HOLD and merges a workflow
makes with its own token.

**Which hosts:** off unless the host turns it on, with `SHIPYARD_HOLD_DISARMS=1`
(`0` forces it off) or a file named `hold-disarms` in the guards directory.
Off, it reads nothing. Roll it out one host at a time; roll back by removing
the file.

## Environment re-enqueue

A batch that died because the network did (a package relay answered 403, a
download host did not resolve, an upload connection was reset) says nothing
against the head, yet the same-head refusal made the only way back into the
queue a new push, and a whole required-gate cycle for a commit that changes
nothing. Over 2026-09-15..29 on `Generous-Corp/pulp`, 15 queue ejections were
environment failures and 12 of them were followed by exactly such a push.

A repository opts in with:

```toml
# .shipyard/config.toml
[queue.environment_requeue]
enabled = true
```

Then, for a same-head `failed_checks` ejection only, the guard (and
`shipyard landing --pr`, and `ship`'s arm-on-open, which share
`src/environment_requeue.rs`) allows **one** re-enqueue when all of these hold:

| requirement | why |
|---|---|
| the timeline window is complete and shows exactly one `failed_checks`/`merge_conflict` removal of the current head | the allowance is one retry per head; a head ejected twice has had it, and a truncated window cannot prove it has not |
| the removal names its merge-group commit (`beforeCommit`) | that commit's check runs are the ones that ejected it; no run-resolution heuristic |
| the base's required checks (rulesets plus classic protection) can be read and are non-empty | only a **required** failure ejects; an advisory lane failing a real test is not what removed the head |
| every failing required check on the merge-group commit is a GitHub Actions job concluded `failure` (or `cancelled` and starved, below), and no required commit status failed | a `timed_out` job or a status has no log that could prove anything |
| every failing step of each such job (jobs API `conclusion == failure`) printed an environment signature within 60 output lines of that step's first `##[error]` | positive evidence at the failure, not anywhere in a long log |

The signatures are the network-transport spellings shared with Shipyard's infra
classifier (`Could not resolve host`, `Network is unreachable`, `No route to
host`, `Connection reset by peer`) plus `ENOTFOUND`, `getaddrinfo`,
`EAI_AGAIN`, `ECONNRESET`, `Tunnel connection failed`, `Proxy CONNECT aborted`,
`Temporary failure in name resolution`, `curl: (6)` and `curl: (56)`. Broad
words such as `timeout`, `rate limit` or `Connection refused` are deliberately
absent: a test the head broke prints them too.

Two traps shaped the reader. **A step's own script can contain the signature**:
Pulp's `Install visual-analysis Python dependencies` step carries a comment
quoting `Tunnel connection failed: 403 Forbidden`, and GitHub echoes a `run:`
script into the log inside `##[group]Run ... ##[endgroup]`. Those lines are never
read as output. **The first `##[error]` in a log is not the failing step**: an
`if: always()` / `continue-on-error` step after it prints its own. The failing
step comes from the jobs API and its output from the first `Run` segment at or
after the step's `started_at`, through that segment's first `##[error]`.

### Interruptions: a starved job, or an upload that stalled after green

Two more ejection causes say nothing against the head, and the same reader
(Rust and the guard's Python twin) treats them as **interruptions**:

| cause | evidence required |
|---|---|
| a required job **starved** of a runner | the required check concluded `cancelled`, its Actions job has an empty `runner_name` (`gate_cost/proxy.rs` `ejection_cause()` says `starved`), and it waited at least 10 minutes from `created_at` to `completed_at`. A shorter no-runner cancel is a superseding push or a concurrency-group cancel. A check run whose annotations say `Canceling since a higher priority waiting request` is a concurrency supersede at any wait (runs 37890997376 and 37891885987 waited 11.6 and 16.5 minutes), so it is never starvation. Missing times, or annotations that cannot be read, refuse. |
| an **upload that stalled** after the work passed | the failing step's own `##[error]` line, or the line before it, reads `Upload progress stalled`. Any earlier failing step, such as a red test step, has no signature and refuses the whole verdict; a stall that recovered earlier in the step explains nothing. |

An interruption allows a same-head re-enqueue on each of a head's first **two**
ejections (a network failure allows the first only), and an allowed
interruption does not count toward the head-approval `EJECTION_CAP`. Every
same-head re-arm, from `ship`'s arm-on-open or from the steward's
`--arm-unqueued` backstop, is sent with `expectedHeadOid` bound to the head the
classifier read, so GitHub refuses it if the head moved. The steward's backstop
reads the opt-in from the protected base's `.shipyard/config.toml`, never from
the head, and re-arms only a head someone armed after it arrived: an
`AutoMergeEnabledEvent` at or after the head's own force-push or its first
check suite (`merge_carrier::head_arm_time`, the steward carrier's rule).
Without that, a head pushed after an earlier head was armed, auto-queued and
starved would be re-armed with nobody having armed it. A window that cannot
place the arrival refuses. A real `failure` with a runner, `merge_conflict`, or a moved head is
never re-armed.

### Why this is not the inference refused above

[Batch attribution](#why-the-guard-does-not-rule-for-itself) refuses to read
*absence* ("no test failed") as innocence. This reads *presence*: the step that
failed printed, at its failure, a line only the network produces. The residual
risk is a head whose own content names an unreachable host, and a head broken
in a way the dead batch never reached (`#8811`). Both are why the allowance is
bounded to one retry per head and off unless the repository opts in: the worst
a wrong allowance costs is the one batch the retry joins, and the second
ejection is refused with the ordinary "push a fix first".

### Relationship to a declared attributor

The guard asks the repository's [batch attributor](#batch-attribution) first.
When it certifies, that allow stands and the environment reader is not
consulted. When it does not certify, including a verdict of
`implicates_head: true`, the environment reader still runs. That is deliberate:
Pulp's attributor reads chain ancestry (the batch's parent passed, so this head
is the culprit), which compares outcomes rather than causes. A parent that
passed on a host that could reach PyPI says nothing about a batch that died on
a host that could not, and `--certify` returns `implicates_head: true` for both
`pulp#8678`'s pip-relay ejection and `pulp#8911`'s cargo-DNS ejection. The
environment verdict rests on the failing step's own output instead, which a
content-level implication (the head owns a failing ctest case) cannot share:
such a step failed on a test, not on a network signature at its failure.

An allowed re-enqueue is not silent: the guard prints
`queue-arm-guard: note: ...` with the verdict and each step's matching log line.
A refused one appends `Environment re-enqueue refused: <reason>` to the
ordinary refusal. `shipyard landing --pr <n>` prints the same verdict and
evidence under `ENVIRONMENT RE-ENQUEUE`.

## Batch attribution

A pull request ejected for `failed_checks` is refused a same-head re-enqueue
because under ALLGREEN a head that failed its batch will fail the next one too,
taking innocent batch-mates with it. That reasoning assumes the head is what
failed. Sometimes it is not: the batch failed on infrastructure, or on a
breakage already present on the base, or on a different entry in the batch.

The guard does not decide that for itself. It asks the repository, and only a
positive certification turns the refusal into an allow.

### Declaring an attributor

```toml
# .shipyard/config.toml
[queue.attribution]
command = ["python3", "tools/scripts/queue_batch_attribute.py"]
```

`command` must be an argv list; a shell string is rejected rather than quoted,
so the repository's config never becomes a shell-injection surface for a command
that runs on an operator's machine. The config is found by searching upward from
the working directory `ghapp` was invoked in, and it is used only when that
checkout is the pull request's own repository.

**With no `[queue.attribution]` declared the guard makes no extra API reads and
behaves exactly as it did before.** Nothing changes for a repository that does
not opt in.

### What the guard asks

When, and only when, a target is a same-head ejection for `failed_checks`, the
guard resolves the ejecting `merge_group` run (the most recent failed
`merge_group` run created no later than the removal, whose batch contains this
head, proven by the read-only queue branch naming `pr-<n>-` or by commit
ancestry), collects each failing job and the names of its failing steps, and
runs:

```
<command...> --repo <owner/name> --pr <number> --run-id <id>
```

The run is searched by creation time, `created=<removal-6h>..<removal+5m>`,
100 per page and at most 3 pages, never by recency. Every workflow of every
failed group is its own run, so on `Generous-Corp/pulp` "the latest 20 failed
runs" covered about an hour, and a certified ejection (`pulp#9048`) could not be
re-admitted once that hour passed. A window with nothing matching, or busier
than the read cap, refuses and says which.

### The attributor's environment

The command runs from the repository root with its output captured and a
120-second timeout, inside the guard's environment rather than the operator's
shell:

- `PATH` is the wrapper's trusted system path
  (`/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin`). `ghapp`
  itself, usually installed under `~/.local/bin`, is **not** on it, and neither
  is a shell function or alias. An attributor that shells out to a bare `ghapp`
  fails with "not found" here even though the same command works interactively.
- `GHAPP_REAL_GH` names the real `gh` binary, `GH_TOKEN` holds the App token
  the wrapper minted for the command being guarded, and `GH_REPO` is bound to
  the pull request's repository. `"$GHAPP_REAL_GH" api ...` makes a read under
  the same identity the guard used.

A non-zero exit, or a verdict that is not JSON, refuses with the last few lines
of the attributor's stderr (control characters removed) in the message, so a
crash is diagnosable from the refusal alone.

### What certifies, and what does not

The attributor must print one JSON object on stdout:

```json
{
  "run_id": 36093055057,
  "implicates_head": false,
  "verdict": "infrastructure",
  "evidence": "macos failed at `Install ccache (macOS)`, before any repository content built"
}
```

Certification requires **all** of:

| requirement | why |
|---|---|
| exit status 0 | a crashed attributor has not ruled |
| stdout is a JSON object | an unparsable verdict is not a verdict |
| `run_id` equals the run the guard resolved | a ruling about another run does not apply here |
| `implicates_head` is exactly `false` | `null`, missing, or a string is not a ruling |
| `verdict` is `infrastructure` or `other_pull_request` | it must name *why*, not what it failed to find |
| for `other_pull_request`, `implicated_pr` is an integer other than this PR | blaming this PR is not blaming another one |

Everything else refuses, and the refusal now carries the batch's failing jobs
and steps so the attribution can be made in one step instead of hunted for.

A certified allow is not silent. The guard exits 0 and prints
`queue-arm-guard: note: ...` to stderr naming the batch, the certification and
the evidence, because lifting a protective refusal should leave a trace.

**`merge_conflict` is never attributable.** A conflict is a property of the head
against its base, so it implicates the head whatever the batch's checks did.

### Why the guard does not rule for itself

It is tempting to let Shipyard decide this generically: resolve the ejecting
run, and if nothing in it looks like a test failure, call the head innocent.
That rule is unsound, and the incident that motivated this feature is the
counter-example.

`Generous-Corp/pulp#8811` was ejected for `failed_checks` at
2026-09-25T04:12:51Z by `merge_group` run 36093055057. Two jobs failed: `macos`
at `Install ccache (macOS)`, and `Linux (x64) [github-hosted]` at `Build`.
Neither is a test failure, and the repository's own tooling reported exactly
that: "no ctest failure block (failure is not a test failure)".

But **a compile error is the most common way a head breaks a batch, and it
produces no test-failure evidence at all.** A rule that reads "no test failure"
as "innocent head" allows a re-enqueue of a head that cannot possibly pass:
the precise wrong-allow this guard exists to prevent, and one whose cost is
ejecting innocent batch-mates.

A step-name signature is no better. In that same batch the `Build` failure was
CMake test discovery for `pulp-test-group-canvas`, registered at the batch's
base commit and untouched by the pull request. The neighbouring batch for
`#8807` (run 36090859921) failed the same job at the same step for the same
pre-existing cause, so `(job, step)` does corroborate across batches, but
`(macos, Install ccache (macOS))` corroborates nowhere that day, and a rule
requiring every failing job to be corroborated refuses this incident anyway.

The same incident also shows that "un-implicated by the ejecting batch" is
strictly weaker than "will pass next time". `#8811`'s visible batch failure was
not its own, but its head was broken anyway, by the same class of defect: a
grouped member spec whose only case compiles on macOS, so discovery matched
nothing on Linux and Windows. The build aborted on the base's `group-canvas`
failure before reaching it, so that defect appears nowhere in the ejecting run,
and the pull request needed a real fix three hours later. A same-head
re-enqueue at 04:13 would have been justified by every piece of evidence that
batch contained, and would still have failed and taken its batch-mates with it.

So certification is deliberately narrow and repository-owned. Only the
repository can reason about whether a failed build even reached the parts a diff
touches, and which of its steps repository content can influence at all.
Shipyard asks rather than guessing.

### Shipyard's own arming path is deliberately judged by this guard

`shipyard pr` / `ship` / `ship --pr` arm native auto-merge once the pull request
is known, and `runner steward --arm-unqueued` does the same periodically for
pull requests the steward declines to own. Both issue
`enablePullRequestAutoMerge` through the ordinary `gh` path **without**
`SHIPYARD_INTERNAL_QUEUE_MUTATION`, so this guard judges them.

That is on purpose. The internal marker exists for requests bound to a head
Shipyard has *validated*; an arm-on-open request is not, so the guard's live
read is the only thing standing between it and re-arming a queued or ejected
head. Every state the guard refuses, Shipyard's own policy
(`src/auto_arm.rs`) refuses too, so in practice the guard agrees — and when it
refuses, Shipyard reports the refusal and carries on rather than failing the
ship. Shipyard never sets `GHAPP_ALLOW_QUEUE_REARM`.

### Branch-refresh guard decisions

The policy is `[merge] refresh_branch` in `.shipyard/config.toml`, read from the
pull request's **base branch** through the App token, so a PR branch cannot
relax it for itself:

```toml
[merge]
refresh_branch = "only-if-conflicting"   # default: "always"
```

| policy / live state | decision |
|---|---|
| `"always"`, key or file absent, or an unrecognized value (reported) | allow: Shipyard's historical behaviour |
| `"only-if-conflicting"`, base has no merge queue | allow: strict protection may need the up-to-date head to merge |
| PR conflicts (`mergeable: CONFLICTING` or `mergeStateStatus: DIRTY`) | allow: resolving it needs a new head |
| a required check on the head is failing (`FAILURE`, `TIMED_OUT`, `CANCELLED`, `ACTION_REQUIRED`, `STARTUP_FAILURE`, `STALE`; status `ERROR`/`FAILURE`) | allow: the queue would reject the PR, and a fresh merge ref can clear a failure at a step the current workflow no longer runs |
| `mergeable` still `UNKNOWN` after two re-reads, required checks truncated past 100, PR not open, or anything unreadable | allow (with a note): the guard never refuses blind |
| removed from the merge queue at its **current** head for a reason `queue-arm-guard` refuses to re-enqueue (`failed_checks`, `merge_conflict`, `manual`, ...) | allow (with a note): the arm guard demands a new head, and a refresh is one |
| the queue state (timeline) cannot be read or classified, or `queue-arm-guard` is installed but will not load | allow (with a note): not refusing blind |
| base has a merge queue, PR `MERGEABLE`, no required check failing (pending is not failing), and not removed from the queue at its current head | **refuse**: the queue validates the merge result itself, and the refresh push would cancel and restart the required gate |

The queue-state row is read only when the guard is about to refuse, with
`queue-arm-guard`'s own query and classifier (the Python twin of
`shipyard landing --pr <n>`), loaded from the arm guard installed next to it. An
ejected pull request's head checks are usually green, because the failure
happened in its `merge_group` run, so without that row an infrastructure
ejection would be refused by both guards at once. When no arm guard is
installed nothing refuses a same-head re-arm, so the row is skipped.

There is deliberately no `"never"`: a refresh that resolves a conflict is
needed for correctness, and no policy value can refuse one. The guard sees only
App-authenticated refreshes; a local `git merge origin/main && git push` does
not pass through `ghapp`, so agent guidance (the `ci` and `shipyard` skills)
carries the same rule.

### How the two guards interact

A guard that refuses must leave a path another guard allows, or the pull request
is stuck until an operator override. Under `refresh_branch = "only-if-conflicting"`
on a base with a merge queue:

| live state | `queue-arm-guard` (re-arm this head) | `branch-refresh-guard` (new head via refresh) | way forward |
|---|---|---|---|
| never armed, not queued, mergeable, green | allow | refuse | arm it (`shipyard ship --pr <n>`) |
| queued, or armed and not yet queued | refuse (nothing to do) | refuse | wait for the queue |
| removed for `invalid_merge_commit`, same head | allow | refuse | re-arm as is |
| removed for `failed_checks` / `merge_conflict`, same head | refuse (unless the repository's attributor certifies the batch, or the one [environment re-enqueue](#environment-re-enqueue) applies) | allow | push a fix, or refresh if the batch failed on infrastructure; then `shipyard ship --pr <n>` |
| removed for `manual` (or any other reason), same head | refuse: confirm with whoever dequeued it | allow | confirm, then fix or refresh, then `shipyard ship --pr <n>` |
| removed, new head since | allow | normal policy (refuse when mergeable and green) | re-arm |
| conflicting (`CONFLICTING` / `DIRTY`) | as its queue state says | allow | refresh or resolve locally |
| a required check on the head failing | as its queue state says | allow | refresh or fix |
| queue state unreadable | refuse (fail closed) | allow (fail open) | refresh, or read it with `shipyard landing --pr <n>` |
| merged / closed | refuse | allow (not open) | nothing to do |

"Same head" and "new head since" compare SHAs: the current `headRefOid` against
the head the queue removed (the second parent of the removal's `beforeCommit`
merge-group commit). Commit dates and timeline position are not consulted,
because GitHub sorts a fix committed before an ejection but pushed after it
*before* the removal. A removal that names no head (`merge_conflict`) falls back
to push-time evidence. That is a push of the current head after it in timeline
order, or the current head's earliest check suite created after the removal
event, wherever GitHub sorted its commit (basis `suite_after_removal`). The
suite stands in for the push time because `Commit.pushedDate` is null. A head
with neither, including one with no check suite, is treated as unchanged by
both guards. A commit that first reached GitHub on another branch before the
removal carries that earlier suite and also reads as unchanged. If a live push
is ever misread, the repository activity API
(`repos/{owner}/{repo}/activity`, push events with timestamps) is the
authoritative source.

The two unreadable rows are asymmetric on purpose: arming blind can fail
innocent batch-mates, while a blind refresh costs at most one gate run. Every
row has a path that at least one guard allows without an override. Each guard's
refusal names the other's allowed path: the arm guard's same-head refusal points
at `gh pr update-branch <n>`, and the refresh guard's refusal says a same-head
queue removal is refreshable.

## Overrides and bypasses

| variable | who sets it | effect |
|---|---|---|
| `SHIPYARD_INTERNAL_QUEUE_MUTATION=1` | Shipyard itself, on its own exact-head, audited queue commands (enqueue arm, classic merge, merge-steward enqueue, disable/dequeue revocation), with that call's one-shot nonce | both queue guards step aside, once per nonce; Shipyard's admission rules already made the decision, and Shipyard ran this guard's review rule itself first (`SHIPYARD_QUEUE_ARM_REVIEW_ONLY=1`). Without a valid nonce it is ignored |
| `SHIPYARD_QUEUE_ARM_REVIEW_ONLY=1` | Shipyard itself, just before one of its own arms, enqueues or merges, with that run's one-shot nonce | the arm guard runs only the review rule, whatever the internal marker says, and records the attempt. Without a valid nonce it is ignored and every rule applies |
| `SHIPYARD_QUEUE_MUTATION_NONCE`, `SHIPYARD_QUEUE_MUTATION_NONCE_DIR` | Shipyard itself, only on the child it built for one call | name the one-shot record that makes the two switches above count (below) |
| `GHAPP_ALLOW_UNREVIEWED_ARM=1` | nobody | refused and audited; the review rule has no override |
| `GHAPP_ALLOW_QUEUE_REARM=1` | an operator, deliberately, for one command | the arm guard allows a refused arm and prints a `WARNING` naming what it overrode; it never lands a head whose [tiered check](#a-tiered-required-check-must-prove-its-full-lane) has not proven its full lane |
| `GHAPP_ALLOW_QUEUE_REMOVAL=1` | an operator, deliberately, with a stated reason | the removal guard allows the dequeue/disable, with a `WARNING` and an audit record, only for a `defect-fix` or the main-red fix PR itself (below) |
| `GHAPP_ALLOW_BRANCH_REFRESH=1` | an operator, deliberately, for one command | the branch-refresh guard allows a refused refresh and prints a `WARNING` |

Setting `SHIPYARD_INTERNAL_QUEUE_MUTATION` by hand claims Shipyard's authority
for a command Shipyard did not audit, and does not work. Both queue guards
honour the marker, and the arm guard honours `SHIPYARD_QUEUE_ARM_REVIEW_ONLY`,
only for the one call Shipyard built it for. For each such call Shipyard draws
a fresh 32-byte random nonce and puts it only in that child's environment
(`SHIPYARD_QUEUE_MUTATION_NONCE`, with `SHIPYARD_QUEUE_MUTATION_NONCE_DIR`
naming the directory). It writes one record per guard purpose (`internal-arm`,
`internal-removal`, `review-only`): a file named by the SHA-256 of
`<nonce>:<purpose>`, holding the purpose, mode 0600, in
`queue-mutation-nonces/` beside the arms ledger, a directory it keeps at 0700.
A guard consumes its record (the removal guard's `consume_nonce`): opened
without following links, it must be a regular file owned by the same user,
readable by no one else, at most ten minutes old and naming that purpose,
and it is unlinked before the switch takes effect. Unlinking is the claim, so
each record works once. A marker or switch with no nonce, a wrong one, a
reused one, or a record that fails any check is ignored, with a note, and the
request is judged in full. Records nobody consumed (a native `gh` runs no
guard) are dropped by Shipyard after ten minutes.

Shipyard itself never passes these on: a Shipyard process that starts with
the marker, the review-only switch, a nonce variable or any `GHAPP_ALLOW_*`
override in its environment re-executes itself without them, so no runner,
validation command or hook it spawns inherits one, and it strips them from
every `gh` and guard child except the ones it marks.

The nonce ties the marker to a call Shipyard made instead of to whoever
exported it or to the process tree. It is not a boundary against a process
running as the same user that deliberately forges Shipyard's own state
directory.

### Queue-removal override decisions

`GHAPP_ALLOW_QUEUE_REMOVAL=1` alone is refused. The override must say why, in
two more variables:

| variable | meaning |
|---|---|
| `GHAPP_QUEUE_REMOVAL_REASON` | one reason class, below |
| `GHAPP_QUEUE_REMOVAL_NOTE` | what is wrong, in words; required for the allowed classes |
| `GHAPP_QUEUE_REMOVAL_FIX_PR` | for `reorder-main-red-fix`, the number of the PR that fixes `main` |

| reason | decision |
|---|---|
| `rebase` | refuse: a queued PR does not need a rebase; the queue merges it on top of current main |
| `reorder` | refuse: dequeuing other PRs discards their merge-group runs; enqueue the PR that must land first with the queue's jump option |
| `defect-fix` | allow, with a `WARNING`, and append a record |
| `reorder-main-red-fix` | allow only when every PR removed is the one `GHAPP_QUEUE_REMOVAL_FIX_PR` names (it leaves the queue to jump back in); never another PR |
| missing or anything else | refuse |

An allowed removal reads each target PR through the App token and appends one
JSON line (time, reason, note, fix PR, each target's repository, number, head
and queue state, and the argv) to `$GHAPP_QUEUE_REMOVAL_LOG`, default
`~/.local/state/shipyard/queue-removals.jsonl`. The log is per host;
`shipyard who --pr N` reads it from every configured host. A target that cannot be read, or a log that cannot be written,
refuses the removal: an allowance nobody can
audit later is not given. A request body read from stdin is refused too,
because the guard cannot see which PR it removes.

Why intent and not queue health: of 46 queue removals agents made on
Generous-Corp/pulp between 10-03 and 10-09, 34 dequeued a healthy PR to rebase
it because `main` moved and 2 dequeued other PRs to reorder the queue, each
discarding the merge-group run it rode in. The 7 legitimate ones were defect
fixes found locally before any required check went red, which a health rule
would have blocked. `scripts/test_ghapp_queue_removal_guard.py` replays all 46
(`scripts/fixtures/queue_removals_classified_2026-10.jsonl`): the 36 avoidable
ones are refused and the 7 defect fixes allowed.

## Installing, and fleet-wide ordering

```sh
shipyard guards status    # exit 1 unless every managed guard is installed and matches this build
shipyard guards install   # atomic; never overwrites a symlink
```

Per-host switches, all in the guards directory:

| switch | default | turn it on | turn it off |
|---|---|---|---|
| review before arm or merge | on | `shipyard guards install --opt-in-review-before-arm` removes an opt-out | `shipyard guards install --opt-out-review-before-arm "<reason>"` |
| signer rule for App approvals | off | a `reviewer-signers.json` file | remove the file |
| session allowlist for App approvals | off (warns) | a `sessions` list in `reviewer-signers.json` (with a `session_hosts` map, or nothing is admitted) | remove the key |
| rebase carry | off | a `rebase-carry` file, or `SHIPYARD_REBASE_CARRY=1` | remove the file; `SHIPYARD_REBASE_CARRY=0` forces it off |
| HOLD dequeues or disarms | off | a `hold-disarms` file, or `SHIPYARD_HOLD_DISARMS=1` | remove the file; `SHIPYARD_HOLD_DISARMS=0` forces it off |

Turn the off-by-default switches on one host at a time.

`shipyard doctor` reports the same state under "ghapp guards", and
`shipyard update` refreshes the guards with the newly verified binary when the
guards directory already exists.

**The guards directory is shared by every Shipyard binary on the host.**
Installing the arm guard changes the behaviour of older Shipyard binaries too
(a pinned project version, a daemon not yet refreshed, another checkout's
build). Binaries from before the arm guard do not set
`SHIPYARD_INTERNAL_QUEUE_MUTATION` on their enqueue, so their enqueues are
judged like anyone else's. In practice their admission logic already skips
queued and armed PRs, so what changes for them is:

- a same-head re-enqueue after `failed_checks` or `merge_conflict` is refused
  unless the repository declares a batch attributor that certifies the head
  (see [Batch attribution](#batch-attribution)). This is the intended refusal.
  It includes the older merge steward's queue-priority recovery, which
  re-enqueued the same head after a `failed_checks` it attributed to
  infrastructure; that recovery resumes once the host runs a marked binary, or
  once the repository's attributor can certify such a batch.
- a same-head re-enqueue after a `manual` (or other non-`invalid_merge_commit`)
  removal is refused, asking for confirmation from whoever dequeued it.
- an enqueue whose live state the guard cannot read is refused (fail closed);
  that Shipyard reports the enqueue as failed or uncertain rather than
  proceeding.

Re-enqueues after `invalid_merge_commit`, and anything after a new head, are
unaffected.

Marked binaries do not simply step around the guard: `shipyard auto-merge`
admission and the merge steward's ordinary enqueue read the same
`PR_QUEUE_STATE_QUERY` timeline and refuse a same-head re-enqueue after
`failed_checks` / `merge_conflict` themselves, whatever the age of the local
ship-state. They differ from the guard on `manual` removals, which the
attempt-scoped admission rules handle, and they do not consult the batch
attributor. To avoid the transition entirely, update every Shipyard binary on
the host (and refresh its daemon) before running `shipyard guards install`.

## Growing the real-response corpus

The classifier and the guard are pinned to real GitHub responses in
`tests/fixtures/github/` (see its README). A synthetic fixture can only encode
what we already believed, so prefer a live capture whenever one exists.

When a live PR is observed in a state the corpus only covers by truncation or
synthesis (for example ejected for `failed_checks` and not yet re-enqueued),
capture it verbatim, read-only:

1. From inside a checkout of the PR's repository (so `ghapp` can resolve it),
   run the query in `tests/fixtures/github/README.md` with
   `-f owner=... -f name=... -F number=<n>`. If `pageInfo.hasPreviousPage` is
   true, page with `timelineItems(first:100, after:$cursor)` until
   `hasNextPage` is false and concatenate the nodes in order.
2. Save the response unedited as `tests/fixtures/github/pr_<state>.json`.
3. Add its expected classification to `expected_classifications.json` and a
   README row with the PR number and capture time.
4. Run `cargo test pr_queue_state` and
   `python3 -m unittest scripts/test_ghapp_queue_arm_guard.py`: both
   implementations must agree with the new entry.

A real capture of a state supersedes a truncated or synthetic fixture for the
same state; replace it rather than keeping both.
