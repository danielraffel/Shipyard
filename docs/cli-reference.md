# CLI Reference

```bash
# Setup
shipyard init                  # configure project
shipyard doctor                # check environment + suggest fixes
shipyard doctor --rate-limit   # show effective GitHub auth + REST/GraphQL buckets
                               # also: "Git checkouts" reports a cherry-pick, merge, revert, rebase,
                               #   am or unmerged index left in progress over 1 h in a primary checkout
                               #   (this repo's, its pulp/Shipyard/tartci/vellum siblings, doctor.git_checkouts)
                               #   with the backup-then-abort commands; it never aborts anything
shipyard auth doctor           # show Shipyard's configured GitHub auth source
shipyard doctor --rate-limit --repo OWNER/REPO  # resolve auth for an exact repo
shipyard auth doctor --repo OWNER/REPO          # same override, auth-only
shipyard auth export --output shipyard-auth.toml        # export non-secret auth config
shipyard auth import shipyard-auth.toml --scope local   # import auth config locally
shipyard targets               # show targets + reachability
shipyard targets add <name>    # interactively add a new target
shipyard targets remove <name> # remove a target

# Validate
shipyard run                       # full validation, all targets
shipyard run --smoke               # fast smoke check
shipyard run --targets mac         # single target
shipyard run --resume-from test    # skip setup/configure/build (where supported)
shipyard run --continue            # don't stop at first failure
shipyard run command --target linux-vm --artifact 'build/linux-x64/lib/libv8.so' -- bash -lc './build-v8.py --target linux-x64'
shipyard --json changed-surface-plan --repo OWNER/REPO --pr 123 --target mac  # shadow-only exact-head test plan
shipyard --json changed-surface-trial-status --repo OWNER/REPO --pr 123 --target mac --head HEAD_SHA  # read-only shadow result verdict
shipyard --json parallel-proof-canary --request /absolute/private/invocation.json # no-execution/no-mutation plan
shipyard --json parallel-proof-canary --request /absolute/private/invocation.json --apply # exact machine-global digest-pinned canary

# Ship
shipyard ship                  # PR → validate → merge on green
shipyard ship --base develop   # target a different branch
shipyard merge-queue status    # local mutation authority hold
shipyard merge-queue hold --reason "incident"
shipyard merge-queue resume

# Immutable Pulp dependency pins (opt-in tracked policy)
shipyard dependency pulp show             # local policy + exact lock
shipyard dependency pulp update           # qualify latest/stable/fixed and open an App-authored PR
shipyard dependency pulp verify           # fresh, cache-bypassing CI verification

# Understand how a repo lands work (read-only)
shipyard landing                             # merge queue, strict, enqueue, batching strategy, check placement, backlog, base tip health (HEALTHY/RED/PENDING/UNPROVEN from the tip's merge_group required jobs)
shipyard landing --repo OWNER/REPO --base main
shipyard --json landing                      # machine-readable; exit 9 when any headline field is UNKNOWN
shipyard landing --pr 123                    # one PR. First line: the VERDICT to quote, RED / PENDING / GREEN / UNKNOWN on the
                                             #   current head's required checks (branch protection), failing tests from the job log,
                                             #   REPEAT on K heads (same test on >=2 runs of the PR) or "also failing on #a,#b —
                                             #   likely main/shared", a failed merge group on this head, and the queue suffix;
                                             #   `gap:` lines and the API-call count follow. JSON: `verdict`. Exit 9 when UNKNOWN.
                                             # Then queued / armed_not_queued / ejected / never_armed / merged, with sources;
                                             #   plus VALIDATION: head test tier (fast / full / unknown) and merge-group receipt decisions

# What does this project build, pin, run on and test with? (read-only)
shipyard inspect --profile                   # detected facts, each with its file:line or API row: build systems, lockfiles,
                                             #   pin manifests, CI commands, runner classes (as written, and as observed in a
                                             #   recent run for each computed runs-on), merge queue, required checks, test
                                             #   frameworks, release workflows; labels like desktop-native / rust-cli
shipyard --json inspect --profile --repo OWNER/REPO --base main --max-runs 8

# Is the base red? (reads the repo's own base-poison-signal/v1 annotation)
shipyard base-health                         # status; for `poisoned` + a named fix PR: "main red: <test>, fix PR #n, jump it" and the commands
shipyard base-health --workflow FILE         # detector workflow (default base_health.workflow, else main-health-detector.yml)
shipyard base-health --act                   # apply base_health.auto_jump: off (default; reads nothing) | dry-run (records "would jump PR #n"
                                             #   once per episode in <state>/base-health/jump-decisions.jsonl) | on (dequeue + enqueue jump:true)

# Is the merge queue fed? The merge train (off unless the base config sets `[train] enabled = true`)
shipyard train status [--repo R] [--base main]  # switch + each open PR's stage and why (approval is head_approval's, carry included)
shipyard train status --record                # also append this pass to <state>/train/<owner>__<repo>.jsonl (for metrics)
shipyard train metrics [--since-hours 24]     # merges/h, approved->merged p50/p95, queue-empty minutes while a PR was ready, batch sizes, heads/merged PR
shipyard train pause --reason "..."           # host kill switch: writes <guards dir>/train.pause; SHIPYARD_TRAIN=off|0|false (or off:owner/repo) also kills
shipyard train resume --reason "..."          # removes the pause file: only the operator who paused, never while SHIPYARD_TRAIN kills
                                              #   (both audited in <guards dir>/train-pauses.jsonl)
# Off (absent, false, unreadable, or killed): status makes at most the one config read (none when killed) and records nothing.
# status never writes the carry log: approvals are evaluated dry (a carried approval is reported, not logged).

# Why is it red? Bounded diagnosis instead of reading a 10 MB job log (shipyard.diagnose/v1)
shipyard diagnose 123                        # PR head: per failing REQUIRED check the step, tests, <=5 evidence groups,
                                             #   runner, class (infra | interrupted | stale_base | flake_candidate | real),
                                             #   rule, and the command for the full log; never over --max-bytes (16384)
shipyard diagnose --run ID [--required NAME] # one workflow run (required checks from the base policy unless named)
shipyard --json diagnose 123                 # the JSON document
shipyard diagnose 123 --annotate plan|post   # check-run annotations on the failing file:line (neutral "shipyard diagnose" check run);
                                             #   output.text carries each failing required job's ci.cause/v1 record, one JSON per line
                                             # config: [diagnose] stale_base_markers / fail_closed_markers = ["regex", ...]

# Name a failed job's cause (ci.cause/v1, a superset of C2's c2.cause.v0) and keep it; see docs/cause-records.md
shipyard cause extract --log FILE            # the job's record as JSON; where it ran from --repo/--run-id/--attempt/--job/
                                             #   --head-sha/--event/--pr, else GITHUB_*; --job-json FILE reads the failing step
shipyard cause extract --log FILE --job NAME --notice  # `::notice title=ci-cause::<record>`: run in an `if: failure()` step so the cause
                                             #   lands in the job's own log. --job must be the job's display name
                                             #   (default GITHUB_JOB is its key). The scorecard uses the record only when
                                             #   bound to the job (repo/head/run/attempt/job) and naming the cause it reads,
                                             #   and dates it by the API time of the step that printed it
shipyard cause extract --log FILE --format c2  # C2's raw records
shipyard cause archive --dir DIR --input F   # append records (or scorecard case rows' `cause`) to DIR/causes/YYYY-MM.jsonl;
                                             #   ids already archived are skipped, nothing is rewritten
shipyard cause record --repo OWNER/REPO --workflow FILE --dir DIR [--since TIME] [--base main]
                                             # read-only observer: a cause row per new red job (DIR/causes/YYYY-MM.jsonl)
                                             #   and a fix-link once its PR or branch goes green (DIR/fixlinks.jsonl);
shipyard cause advise --dir DIR --repo OWNER/REPO --head-sha SHA --query TEXT [--pr N]
                                             #   read-only lexical historical causes; exact-head/privacy guarded
                                             #   --since defaults to three days ago. fix_hint is an allowlisted suggestion, never run
shipyard cause audit --dir DIR               # read-only local archive integrity report; exit 1 on findings

# ghapp guards (queue-removal-guard, queue-arm-guard, branch-refresh-guard, merge-guard,
#   verdict-hold-hook, verdict-session-stamp); see docs/ghapp-guards.md
shipyard guards status                       # missing/stale/current vs this build's copies, plus whether
                                             #   review-before-arm is on (default) or OFF by opt-out; exit 1 unless all current
shipyard guards install [--dir DIR] [--dry-run]  # install this build's copies atomically
shipyard guards install --opt-out-review-before-arm "<reason>"  # turn review-before-arm off here (the opt-out file), logged
shipyard guards install --opt-in-review-before-arm               # remove the opt-out, logged
# Other per-host switches are files in the guards directory, all off by default:
#   reviewer-signers.json (an App APPROVE needs a `reviewer: <name>` signer),
#   rebase-carry (an approval carries across a pure rebase), hold-disarms (a HOLD dequeues its head)
shipyard who --pr 123 [--repo OWNER/REPO]    # which host/session (and agent, if set) armed, enqueued, merged or removed it; joins every
                                             #   host's queue-arms.jsonl + queue-removals.jsonl to the PR timeline
                                             #   (repo+PR+verb+head; time breaks ties); unexplained = unattributed
shipyard review-verdict --pr 123 [--repo OWNER/REPO] [--head SHA] [--no-rebase-carry] [--json]
                                             # may this head land, by the repository-side review rule (no host
                                             #   switch); merge-only and rebase carries apply; exit 0 allowed,
                                             #   1 refused, 2 unreadable; JSON names the approval or carry,
                                             #   excluded human approvals, and the fix
shipyard landability --repo OWNER/REPO       # can this PR's required contexts be scheduled at all
shipyard rollup plan [--repo OWNER/REPO] [--base BRANCH]
                                             # which open PRs form the next rollup (oldest ready, disjoint files,
                                             #   no excluded or shared hot paths) and why each other one does not
shipyard rollup build --members 11,12 [--base BRANCH] [--apply]
                                             # merge each member's exact head onto the base tip (merge-tree +
                                             #   commit-tree; the work tree is untouched); --apply pushes
                                             #   shipyard-train/rollup-<n> and opens its PR with a notice comment
shipyard rollup verify --pr N                # does this rollup head land, by the guard's rollup rule
shipyard rollup land --pr N [--apply]        # merge method MERGE only; merge-queue: prints the steward handoff;
                                             #   direct: required checks green, base re-read, PR merge API with
                                             #   sha= bound; closes members GitHub did not mark merged
shipyard rollup reconcile [--base BRANCH] --apply
                                             # close members of landed rollups GitHub did not mark merged, each only
                                             #   once its head is proven on the base (the steward runs this too)
shipyard rollup split --members 11,12,13,14 [--passed] [--first-passed BOOL --second-passed BOOL]
                                             # next bisect step: land, eject one member, or test each half
                                             #   (docs/rollups.md); all rollup commands exit 0/1 refused/2 unreadable

# Keep the fleet on the latest release (controller only)
shipyard runner fleet-update --to vX.Y.Z --all-hosts          # plan
shipyard runner fleet-update --to vX.Y.Z --all-hosts --apply  # roll out + verify every host; the first host is a canary smoked with read-only admission-clean
shipyard runner fleet-update --to vX.Y.Z --all-hosts --apply --skip-canary-smoke  # emergency rollback while GitHub itself cannot be observed
shipyard runner fleet-reconcile                # recent releases vs every host; --apply rolls the newest soaked tag to hosts behind it
shipyard doctor --fleet                        # each host's version vs the latest release

# Monitor
shipyard status                # dashboard: queue + targets + evidence
shipyard queue-observe         # one read-only queue/PR snapshot; emit on change
shipyard --json queue-observe --follow  # adaptive delta-only NDJSON monitor
shipyard pr-watch scan --repo Generous-Corp/pulp  # read-only flags for stuck open PRs (dry run; --post-comments / --digest opt in)
shipyard pr-watch replay --repo Generous-Corp/pulp --since 7d --expect 8933=1,3 --control merged-clean  # offline simulation
shipyard pr-watch scan --repo Generous-Corp/pulp --handback  # plan the hand-back to owning sessions (dry run; --deliver-handback sends)
shipyard pr-watch sweep-labels --repo Generous-Corp/pulp  # list closed/merged PRs still carrying shipyard:needs-agent (--apply removes it)
shipyard pr-watch wakes --repo Generous-Corp/pulp --since 24h  # hand-back wake events: sent vs seen, latencies, unanswered calls
shipyard pr-watch rules --repo Generous-Corp/pulp --repo danielraffel/tartci@ci.yml --alarm-repo danielraffel/tartci  # rules-first watcher: one JSONL event per new condition, each once
shipyard pr-watch rules-replay --repo Generous-Corp/pulp --since 2d --tick 5m --out events.jsonl  # the same rules over a past window
shipyard watch                 # live-tail an in-flight ship
shipyard watch local --target linux-vm --command '<cmd>' --milestone-regex '<re>' --terminal-regex '<re>'
shipyard queue                 # show all jobs with priorities
shipyard queue triage --repo OWNER/REPO [--apply]          # cancel PR runs that cannot merge at their head (superseded,
                                                            #   conflicting, required check failed); prioritizes the one PR
                                                            #   that says it fixes a check red on the base; records a restore ledger
shipyard queue prioritize --pr N --repo OWNER/REPO [--apply] # cancel queued, not-started PR runs ahead of N on shared runner labels
shipyard queue resume --repo OWNER/REPO [--apply]           # re-run ledger entries still open, same head, no longer doomed
shipyard logs <id>             # per-target logs
shipyard logs <id> --target windows
shipyard --json logs <id>      # typed availability/lifecycle metadata, not log content
shipyard wait job <id> --success --json  # durable queue terminal/pass wait
shipyard evidence              # last-good SHA per platform
shipyard evidence command      # latest workload-agnostic command-evidence bundle
shipyard evidence command --list

# Runner metrics
shipyard metrics record --project pulp --job linux-arm64 --step compile --duration 18.4s --target linux-arm64 --backend local --provider tart-linux --host macstudio
shipyard metrics import github --repo Generous-Corp/pulp --workflow build.yml --limit 10
tartci runtime export --repo Generous-Corp/pulp | shipyard metrics import tartci
shipyard metrics export merge-receipt --repo Generous-Corp/pulp --file "$SHIPYARD_HANDOFF_DIR/merge-receipts.jsonl"
shipyard metrics summary --project pulp --json
shipyard metrics repeated-work-cost --since 14d --limit 1000 --json
shipyard metrics summary --project pulp --group-by host --json   # fold ephemeral runners into their host
shipyard metrics scorecard --project pulp --since 30d --json
shipyard metrics gate-cost --repo Generous-Corp/pulp --workflow build.yml --gate-job macos --since 48h --json
shipyard metrics slowest --project pulp --limit 20
shipyard metrics watch --project pulp --since 14d --json
shipyard metrics watch --project Generous-Corp/pulp --fail-on-stale   # exit 3 when the store is STALE or empty
shipyard metrics watch --project pulp --since 7d --required macos   # override [governance] required_status_checks
shipyard metrics advise --project pulp --profile normal --json
shipyard metrics compare --project pulp --lane windows-arm64 --before 7d --after 7d --json
shipyard metrics compare --project pulp --basis wall-time --json   # duration verdict (load-dependent)
shipyard metrics trend --project pulp --since 14d --json

`metrics compare` splits at `now - --split-days-ago`. `--before` bounds the earlier
side to `[split-before, split)` and `--after` independently bounds the later
side to `[split, split+after)`; neither flag moves the split. Both intervals
are half-open, so a sample exactly at the split belongs to the after side and
a sample exactly at an explicit window end is excluded. Windows that cannot
be represented by the timestamp range produce no findings instead of
panicking.

# Manage
shipyard bump <id> high        # reprioritize a pending job
shipyard cancel <id>           # cancel a job
shipyard cleanup               # explain retention/compression actions (dry-run)
shipyard cleanup --apply       # apply log retention and prune artifacts
shipyard cleanup --validation-tmp           # report leftover validation TMPDIRs (GiB, why reclaimable)
shipyard cleanup --validation-tmp --apply   # delete them (owner gone or unused, older than 6h)

# Profiles & config
shipyard config profiles       # list defined profiles
shipyard config use <profile>  # switch active profile
shipyard config show           # dump effective merged config
shipyard ci profile show normal-local-fast
shipyard ci profile plan normal-local-fast --repo OWNER/REPO --json

# Governance
shipyard governance status     # declared vs live drift
shipyard governance diff       # dry-run apply
shipyard governance apply --yes  # write live state to match config (without --yes: preview only)
shipyard governance export     # snapshot to TOML
shipyard governance use <name> # switch profile (solo / multi / custom)

# Cloud
shipyard cloud workflows       # list dispatchable workflows
shipyard cloud defaults        # show current cloud dispatch plan
shipyard cloud run <wf>        # dispatch a workflow
shipyard cloud status          # tracked cloud runs

# Branch protection (one-shot)
shipyard branch apply [--create name] [--base branch] [target_branch]

# Self-hosted runner watchdog
shipyard runner status                    # one-shot health check (exit 0/1/2)
shipyard runner cleanup                   # dry-run: list stale queued runs
shipyard runner cleanup --fix             # cancel stale queued runs
shipyard runner cleanup --stale-hours 4   # override threshold for this call
shipyard runner watch                     # poll loop (default 5 min)
shipyard runner watch --fix               # auto-cancel stale queued runs each tick
shipyard runner watch --kill-hung-workers # also auto-kill hung Worker processes
shipyard runner watch --reap-stale-runs   # also cancel stale workflow runs repo-wide
shipyard runner watch --reap-stale-runs --dry-run   # preview reaper, cancel nothing
shipyard runner fleet-status --repo OWNER/REPO      # TartCI, registered runners, storage + queue liveness
shipyard runner fleet-status --json                 # periodic-monitor JSON + nonzero alerts
shipyard runner steward --repo OWNER/pulp --repo OWNER/forge --repo OWNER/vellum
shipyard runner steward --apply                     # exact-head, green-gated mutations
shipyard runner steward --no-preempt-capacity       # disable bounded preamble preemption
shipyard runner steward --recover-hosted-setup-eviction-priority --apply
shipyard runner steward --provenance-blocking-label 5·unresolved # repeatable authority blocker
shipyard runner steward --arm-unqueued              # audit green, unqueued, UNARMED PRs
shipyard runner steward --arm-unqueued --apply      # arm native auto-merge on them
shipyard runner steward-handoff --repo OWNER/REPO --pr 123 --head "$SHA" --workstream-id GEN-7 --context-url https://linear.app/... --apply
shipyard runner steward-handoff --repo OWNER/REPO --pr 123 --head "$SHA" --workstream-id GEN-7 --context-url https://linear.app/... --agent-provider codex --agent-session-id NEW_SESSION --transfer-agent-owner --apply
shipyard pr --workstream-id GEN-7 --context-url https://linear.app/...   # free-form handoff identifiers
shipyard pr --no-arm                                # validate only: no arm, no steward handoff, no merge phase;
                                                    #   conflicts with --workstream-id/--context-url
shipyard pr --arm                                   # arm when the head's review verdict allows it (the default);
                                                    #   there is no override for an unapproved head
                                                    # title/body come from origin/<base>..HEAD non-merge commits (never empty);
                                                    #   [pr.body] attribution = "..." closes every composed body once
shipyard pr --body-append "Proxy: ..."              # add text to the PR body once, after the attribution line and before the
shipyard ship --pr 123 --body-append @notes.md      #   whence block; @path reads a file; repeating the same text changes nothing
                                                    # `shipyard pr` first prints a fold suggestion (advisory) when open PRs from the
                                                    #   same whence session, opened in the last 6 h, share a directory family
                                                    #   (first two path components; pr.fold.noise_paths excluded) with this branch
shipyard pr --fold feat/a --fold feat/b             # first cherry-pick each branch's own commits over origin/<base> onto this branch
                                                    #   (merges, version bumps and patches already here skipped); clean tree required;
                                                    #   a conflict resets the branch to where it started
shipyard ship --pr 123 --no-arm                     # same, adopting an existing PR
shipyard runner recovery-worker                     # inspect/revalidate one pending exception; no model launch
shipyard runner recovery-worker --apply             # run one bounded read-only triage attempt
shipyard runner recovery-worker --drain --apply     # process one bounded pending snapshot (maximum 32)

# On the protected base branch, make every `shipyard pr` submission durable
# immediately after PR creation. A PR branch cannot opt itself in.
# Optional CLI --workstream-id/--context-url values override the fallbacks.
# In .shipyard/config.toml:
# [merge_steward]
# auto_handoff = true

`runner steward-handoff` is also dry-run by default. Apply writes a durable
successful `shipyard/steward-handoff` commit status on the expected immutable
head, revalidates that the PR still has that head, and then adds
`shipyard:managed` and removes `shipyard:unmanaged`. Apply-mode `runner steward`
adds that explanatory label to unhanded PRs, but only heads carrying both
management signals may be queued, rerun, cancelled, or recovery-signalled.
The transfer form is the explicit recovery path when the original agent
session is unavailable. It preserves the exact head/workstream/context and
increments a private ownership generation; ambient sessions cannot silently
adopt a receipt. Machine identity is persisted privately on first use rather
than recomputed from mutable host environment variables.

`--workstream-id` is a free-form string: Shipyard stores it verbatim on the
private receipt and publishes it as the `Managed handoff <id>` status
description. It is not parsed and no particular handle shape is required.
`--context-url` is likewise stored verbatim and published as the status
`target_url`; the only check is that it uses `http://` or `https://`.

Semantic blockers receive one deduplicated `shipyard:needs-agent` label and
failed `shipyard/steward-recovery` status, which are cleared after recovery.
The current PR's case-insensitive `5·unresolved` label blocks every steward
mutation and reports `provenance_blocked`; repeat
`--provenance-blocking-label` to configure another explicit vocabulary. The
blocker takes precedence over opt-out, including final force-cancel
revalidation after an accepted cancellation or controller restart.

`runner steward` is read-only unless `--apply` is present. Same-head duplicate
runs are never cancelled; cancellation authority requires an immutable PR or
merge-group head that differs from GitHub's current head. Apply mode requires
the trusted machine-global `[merge_queue].mutation_machine`, rejects the
central merge-queue `HOLD`, and serializes plus write-ahead audits every
enqueue, rerun, and cancellation through the shared mutation guard. A
repository without a GitHub-native merge queue receives a typed
`automatic-merge-refused` decision (exit 10); Shipyard does not issue a client-side REST
merge because that endpoint cannot atomically enforce complete check
materialization and the validated base revision.

Queue-priority recovery is separately default-off. When
`--recover-hosted-setup-eviction-priority` is enabled on an apply pass, the
steward durably records exact managed queue-front entries. If GitHub later removes
that same PR head and unchanged base revision for `failed_checks` within two
hours, with the speculative merge-group commit directly parented by the pinned base and the
removal immediately following the witnessed admission, the steward uses
`jump: true` exactly once only when the recorded merge-group head has one failed required GitHub Actions CheckRun
run, its sole failed job ran in the `GitHub Actions` runner group, only `Set up
job` failed, and the job log contains the narrow provider-internal DNS failure
signature. It revalidates the absent queue entry, open PR, base, immutable head,
ownership, required checks, and central mutation authority before enqueueing.
Missing, stale, ambiguous, generic setup, or self-hosted evidence never grants
priority recovery; the ordinary exact-head enqueue path is unchanged.

An ejected merge-group can currently leave unrelated children in the same
workflow run active after the hosted setup-only failure. Automatic cancellation
is deliberately not inferred from queue-priority authority. The follow-up gate
must reuse the same durable queue witness, latest `failed_checks` removal,
exact merge-group head and required-run/job/log proof, then re-read the exact
nonterminal run and absent queue entry under the central mutation guard before
one write-ahead-audited cancellation. Any ambiguity, head/base drift, new queue
entry, non-hosted job, or run-attempt change must refuse cancellation.

Accepted capacity cancellations remain in the handoff ledger until an exact
run/job read proves terminal; each apply pass resumes those records with an
exact-run force-cancel before planning new work. Read failures keep the record
pending and make the pass unhealthy. Dry-run does not require mutation
authority.

`runner recovery-worker` consumes the steward's durable semantic-blocker
requests. It is phase-1 **read-only triage**: even with `--apply`, the worker
cannot push, edit GitHub state, rerun checks, enqueue, merge, sign, publish, or
release. Without `--apply`, it only inspects pending records and revalidates
their target base, immutable PR head, and complete failed-required-check set or
recorded merge state. Each request carries the complete structured
required-check policy, so a newly failed required check supersedes same-head
work while advisory failures remain irrelevant. No model process starts.
Requests retain a base/evidence/policy-bound identity,
while the attempt gate is stricter: a repository/PR/exact-head tuple receives
at most one model call even if normalized evidence changes. A retarget, newer
head, recovered check, or changed merge state supersedes the old request
instead of spending an attempt on stale evidence.

An apply-mode repository or GitHub preflight error remains pending without
spending the attempt, but is durably moved behind untouched pending work so a
persistently unavailable repository cannot block the machine-global queue.
Required-check policy entries and failure facts store the literal context and
optional GitHub App ID as separate fields, so display-like text cannot change
producer identity. Clearing the deterministic recovery signal supersedes
active receipts and removes only the matching exact-head witness under the same
lease used by final worker completion; a newer-head witness published during
the GitHub-operation gap remains intact.

Worker policy is loaded only from the trusted machine-global `config.toml`
reported by `shipyard paths`; project config and checkout-local overlays cannot
enable or redirect it. Alternate runtime modes and `--global-dir`/`--state-dir`
overrides are rejected so neither policy nor the one-attempt ledger can fork.
Example:

```toml
[merge_steward.recovery_worker]
enabled = true
provider = "codex"
codex_binary = "/Users/you/.local/bin/codex"
codex_home = "/Users/you/.codex"
# Optional; this is also the built-in default.
first_line_model = "gpt-5.3-codex-spark"
timeout_seconds = 120          # hard maximum: 300
max_attempts_per_head = 1      # phase 1 permits exactly one
max_log_tail_bytes = 16384     # hard maximum: 65536
allowed_repositories = [
  "Generous-Corp/pulp",
  "Generous-Corp/forge",
  "Generous-Corp/vellum",
]

[merge_steward.recovery_worker.repo_paths]
"Generous-Corp/pulp" = "/Volumes/Workshop/Code/pulp"
"Generous-Corp/forge" = "/Volumes/Workshop/Code/forge"
"Generous-Corp/vellum" = "/Volumes/Workshop/Code/vellum"
```

Pulp dependency channels require an explicit tracked `[dependencies.pulp]`
declaration; there is no floating-ref or unrelated-repository default. See
[Pulp dependency channels](dependency-channels.md) for the active-first-party,
production, and frozen templates and the consumer build-authority boundary.

The model command is not configurable. Shipyard constructs the complete
ephemeral/read-only `codex exec` argv, disables Codex shell, browser, app, MCP,
computer-use, image, and search tool surfaces, and delivers the request only on
stdin. The child runs from isolated scratch rather than the mapped repository,
ignores user/project rules, and starts after `env_clear` with only explicit
`CODEX_HOME`, scratch-local `HOME`/`TMPDIR`, a minimal `PATH`, and the Windows
system root where required. GitHub preflights, the model, and final validation
share one overall deadline; stdout/stderr and durable failure details are
bounded. Each provenance read scans at most four explicit 100-status pages. A
machine-global file lease permits one model invocation at a time.
Shipyard accepts only the strict versioned JSON result
documented in
[the merge-steward reference](https://github.com/danielraffel/Shipyard/blob/main/skills/shipyard/references/merge-steward.md).
Phase 1 is escalation routing only: `bounded_repair`, `no_change`, evidence,
candidate paths, and focused tests are rejected. Every accepted result
explicitly escalates; category and confidence are routing metadata only.
This universal rule does not rely on check names to infer protected paths, so
generic check labels cannot downgrade agent-instruction, workflow/CI, signing,
release, environment, credential, or other high-risk changes.
Current deterministic evidence and steward-policy witnesses are checked before
claim and after model output; pending same-head drift supersedes and replaces
stale work. Failures do not block unrelated stewardship or grant model
authority.

# Self-hosted runner provisioning (register / list / remove on this machine)
shipyard runner tag --set studio          # set this box's machine tag (m1, m5, …)
shipyard runner tag                       # print the stored machine tag
shipyard runner register --repo OWNER/REPO --count 3 --ci-root /path/ci/repo
shipyard runner register --repo OWNER/REPO --count 3 --dry-run   # plan only
shipyard runner list --repo OWNER/REPO    # live pool, grouped by machine
shipyard runner list                      # discover repos from local runner dirs
shipyard runner remove --name repo-studio-03 --yes   # add --purge-dir to delete the dir

# Explicit Worker termination (snapshot + SIGTERM grace + SIGKILL + quarantine)
shipyard runner kill --pid 59996 --reason "wedged on agentB/81"
shipyard runner kill --pid 59996 --reason "..." --retrigger     # re-queue CI
shipyard runner kill --pid 59996 --reason "..." --yes           # skip prompt
shipyard runner kill --history                                  # review past kills
shipyard runner kill --history --last 5
shipyard runner kill --recover kill-59996-deadbeef              # restore quarantine
```

Cleanup protects active, failed, unclassified, and explicitly audit-pinned log
evidence; it pressure-deletes only successful terminal jobs. See
[Log retention and rotation](log-retention.md) for defaults, configuration, and
the active-writer Phase 2 boundary.
Use `shipyard cleanup --pin <job-id>` for serialized indefinite audit pinning.

See `docs/runner-watchdog.md` for the full reference.

## JSON output

Every command supports `--json` for structured output with a versioned
schema, intended for AI agent consumption:

```bash
shipyard run --json
shipyard ship --json
shipyard status --json
shipyard ci profile plan normal-local-fast --repo OWNER/REPO --json
```

The envelope always carries `schema_version: 1` and the command name, so
agents can pin to a stable contract.

## Merge Queue Control

`shipyard merge-queue hold` writes a durable machine-global sentinel and every
Shipyard enqueue/dequeue path checks it before invoking GitHub. `resume`
removes only that sentinel. It does not bypass
`[merge_queue].mutation_machine` from the trusted machine-global `config.toml`
reported by `shipyard paths`, which binds queue writes to the host whose stored
runner tag matches the configured authority. Tracked project config and local
checkout overlays cannot grant or redirect mutation authority.

Every attempted write is serialized process-wide and recorded in
`$SHIPYARD_STATE_DIR/merge_queue/mutations.jsonl`. A `started` record without a
definitive `finished` result is classified under `uncertain_mutations` by
`merge-queue status --json`, preserving the fail-closed retry boundary after a
hard crash or transport ambiguity.

An unresolved row blocks another mutation for the same repository, base, and
PR. After checking GitHub's authoritative queue/timeline state, resolve it with
`shipyard merge-queue resolve <correlation-id> --outcome accepted|rejected
--reason "<evidence>"`.

`shipyard ci profile plan` is provider-neutral and read-only. It parses a
repo-owned TOML profile from `.tartci/`, `.shipyard/ci-profiles/`, or
`ci-profiles/`, then reports the concrete GitHub runner variables/selectors
that would be used for each lane. It does not require tartci to be installed.

## Inspect

`shipyard inspect` reads a repository's CI and says what to change. It is
read-only and runs only when asked: nothing on the path of `ship`, `pr`, the
guards or the daemon calls it.

`shipyard inspect --profile` prints the facts every inspect check applies to,
read from the repository rather than declared:

- build systems, from the marker files `shipyard init` detects (plus
  `pyproject.toml`, and Python sources with no manifest);
- lockfiles and pin manifests;
- the build, install, test and setup commands workflows run, each with its
  `path:line`;
- runner classes: every job's `runs-on:` as written, and for each workflow
  whose `runs-on:` is computed (a routing variable or a job output), the
  runner its latest completed run's jobs actually used;
- the base branch's merge queue (from its active rules; branch protection
  cannot express one) and its required checks (rules plus branch protection);
- test frameworks, from test files and from the commands CI runs;
- workflows triggered by tags or releases.

Every kind reports what it searched. A kind whose source could not be read is
`unavailable` (or `partial`) with the error, and is never reported as absent.
Paths under fixtures, tests, examples, vendored or experimental directories
are kept but marked incidental. Labels (`desktop-native`, `rust-cli`,
`python-tool`, `web`) are names for bundles of these facts; `[inspect]
profile` in config shows a chosen label beside the detected ones without
adding any fact.

The read is bounded: five settings and file reads, then two per sampled
workflow, at most `--max-runs` (default 8) of them, the ones producing a
required check first; the rest are named as not read. The token is the one
for the repository you run it in, so reading another organisation's
repository can leave branch protection unreadable (reported `partial`): run
it inside that repository's clone.

## Runner Metrics

`shipyard metrics` stores normalized runner timing rows in
`$SHIPYARD_STATE_DIR/metrics/metrics.db` using SQLite. The feature is optional
and provider-neutral:

- `shipyard metrics record` writes one explicit job/step sample.
- `shipyard run command` writes a best-effort metrics row automatically when it
  stores command evidence.
- `shipyard metrics import github` imports recent GitHub Actions job timings
  through `gh api`.
- `shipyard metrics import tartci` imports JSON/JSONL from
  `tartci runtime export` when a project uses tartci VM lanes.
- `shipyard metrics import fleet-capacity --file <path>` imports validated
  `tartci.fleet-capacity/v1` observation receipts; see
  [`fleet-capacity-contract.md`](fleet-capacity-contract.md).
- `shipyard metrics import merge-receipt --file <path>` imports validated `shipyard-merge-receipt/v1` JSON/JSONL; invalid or unavailable provenance stays fail-closed.
- `shipyard metrics export merge-receipt --repo <owner/repo> --file <path>` writes the
  producer's already-validated receipts for exactly that repository as an atomic
  JSONL handoff. It reads only the local `merge_receipts` store; it never queries
  GitHub or manufactures receipts. An empty export fails closed without
  replacing the destination, so the downstream cannot record a false
  successful handoff.
- `scripts/merge_receipt_handoff.py export ...` supervises that producer export
  and writes an atomic `shipyard-merge-receipt-handoff/v1` health sidecar;
  `... check --repo <owner/repo> --base-branch main --stale-after 2h` exits `3` for a missing, malformed, failed,
  empty, stale, or tampered handoff. It verifies the sidecar's payload digest,
  byte/record counts, exact receipt schema, producer identity, and duplicate/
  conflict rules. An empty producer store remains unavailable; the baseline
  collector uses `--merge-receipt-health` to preserve that unavailable state. `scripts/install_merge_receipt_handoff.sh` installs
  the producer export as a managed per-user launchd job. The sidecar also binds
  the producer DB's newest `imported_at` for the exact repository/base branch;
  missing or stale source metadata is unavailable. The sidecar records the
  producer `stale_after_seconds` policy and observers reject a mismatched
  window. These helpers are
  observational and never authorize or infer a merge.

GitHub imports retain the workflow event, provider run ID, run attempt, and
authoritative head SHA on the SQLite run row. These dimensions are independent:
an event can have multiple attempts, and an attempt can be observed alongside
other event dimensions. Missing provider fields remain `NULL`. The importer
does not infer organic or synthetic provenance from event names or pull-request
shape. When a workflow-run response omits its pull-request list, a
`pull_request`/`pull_request_target` run may resolve its exact head through
GitHub's commit-to-pull-requests endpoint; ambiguous or unavailable
associations remain `NULL`. Only an authenticated `production` or `synthetic` label is eligible for
provenance-qualified scorecard populations; unknown rows remain visible in the
`scorecard.provenance` counts and are excluded from promotion decisions.

The current GitHub importer does not assign authenticated `production` or
`synthetic` provenance, so imported GitHub rows normally remain `unknown` and
`qualified_jobs` is expected to be zero. That is an honest unavailable
measurement, not evidence that no production jobs exist. A future producer
must attach verified provenance at the producer boundary; the importer does
not infer it from event names, repository, workflow, or runner identity.

The agent-facing queries are `summary`, `scorecard`, `slowest`, `watch`,
`advise`, and `compare`. `scorecard` returns one bounded project-level view of
job outcomes (success, failure, and other terminal outcomes), worker-minutes,
duration and queue percentiles, distinct-PR throughput, and cache reuse. It
labels incomplete PR identity coverage as `partial` or `unavailable`, and
does the same when worker-minute coverage lacks measured durations. It reports
submit-to-receipt latency and model-token
use as `unavailable` until those values have durable source telemetry; it never
infers them from job duration. Human output is available by default; `--json`
returns structured rows, findings, or the scorecard for plugins, MCP tools, and
monitoring agents.

Scorecard JSON includes an authenticated strata receipt under `strata` using
schema `shipyard-metrics-strata/v1`. It counts jobs and includes `repo`,
`window_since`, `window_until`, `source_head`, `generated_at`, `unit`, the
`normal_pr`, `merge_group`, `rerun`, and `unclassified` strata, and an
`aggregate`. Each stratum has `observed_jobs`, `qualified_jobs`, `provenance`,
`coverage`, and `reason`. Only verified production rows with valid
provider/run/head/attempt identity qualify; merge-group rows do not require a
pull-request number. Unknown, synthetic, and unverified rows remain visible
with zero qualified jobs. Observed windows are `partial` because recent-N
import completeness is unproven. The receipt does not set a minimum sample
threshold or establish safe-merge authority. When repository binding is
unavailable, an explicit unavailable envelope is emitted.

The scorecard also reports cost proxies needed for the fleet baseline:
`wasted_compute_minutes` sums measured duration for failed or cancelled jobs,
`coordination_minutes` sums authoritative queue wait, and
`rework_compute_minutes` counts failed attempts only. The combined
`coordination_rework_minutes` is queue wait plus failed-attempt compute. These
values are costs, not promotion decisions. `wasted_compute_per_safely_merged_change`
is explicitly `unavailable` until a durable merge receipt joins a PR head to a
successfully merged change; distinct PR count is never treated as a safe-merge
denominator. A baseline receipt must therefore carry that merge join before
reporting the per-change value.

### Project keys

`--project` accepts either the GitHub slug (`Generous-Corp/pulp`) or the short
repository name (`pulp`), case-insensitively, on every metrics query.
`metrics import github` has always stored the short name as the project and the
slug in the row's `repo`; a slug lookup now finds those rows. A slug also
matches rows stored under the slug itself, and a short name matches rows under
any `<owner>/<name>`. A slug never matches a short-keyed row whose recorded
`repo` names a different owner, so two forks sharing a name stay apart. Writes
normalise going forward: a slug passed to `metrics record --project`,
`metrics import github --project`, or a tartci record is stored as the short
name plus `repo`. Existing rows are never rewritten.

### Freshness (`STALE`)

The store only changes when something imports into it. `summary`, `watch`,
`advise`, and `scorecard` therefore report how old their data is before any
verdict: a leading `STALE: last github import <ts> (<age> ago; threshold 24h)`
line in human output, and a `freshness` object in `--json` (`status` =
`fresh` | `stale` | `empty`, `newest_sample_at`, `threshold_secs`,
`stale_sources`, and per-source `sources[]`). `watch` and `advise` also lead
their findings with a `stale_data` / `empty_data` finding, and append to every
`insufficient_*` finding that the shortage is likely the missing import, not
the lane.

Rows are attributed to a source: `github` (written by `metrics import
github`) or `recorded` (`metrics record`, `run command`, tartci, governed
builds). The store is `stale` when any imported source is older than the
threshold — a fresh locally recorded row does not hide a dead import — or,
when a project has no imported source, when its newest row is. `empty` means
no row matches the project, which is also what a mistyped key looks like.

The threshold is `--stale-after <N>[s|m|h|d]`, else `[metrics] stale_after`,
else `24h`. Exit codes: `0` whether fresh or stale (staleness is reported, not
failed); `3` with `--fail-on-stale` when the status is `stale` or `empty`; `5`
for an invalid `--stale-after`.

### Scheduled GitHub import (`[metrics.import]`)

The Shipyard daemon can run `metrics import github` on a cadence. It is off by
default and reads the machine-global config (`~/Library/Application
Support/shipyard/config.toml` on macOS), re-read on every check so a toggle
needs no daemon restart:

```toml
[metrics.import]
enabled = true                 # default false: nothing is read or written
interval_minutes = 60          # per-repository cadence, minimum 5
repos = ["Generous-Corp/pulp"] # default: the repositories the daemon serves
workflow = "build.yml"         # optional; every workflow when unset
limit = 20                     # recent runs per pass, 1-100
```

The daemon checks every 5 minutes (first check 5 minutes after start) and
imports a repository only when its last attempt is at least `interval_minutes`
old. Attempts, successes, counts, and the last error are kept in
`$SHIPYARD_STATE_DIR/metrics/import-schedule.json`; a failed attempt waits a
full interval before retrying. Each pass runs on its own thread through the
daemon's configured GitHub client (`GH_REPO` scoped), one read for the run list
plus one per run, each bounded at 30 s; a pass that imported or failed is
broadcast as a `metrics_import_pass` daemon event. Editing the machine-global
config is an operator step: do it deliberately, not from an agent session.

### Proxy-first verdicts

Wall-clock duration mostly measures how busy the shared hosts were: the same
change timed at 1am and at peak can differ by more than any real effect. So
`compare`, `watch`, `trend`, and `scorecard` judge "better or worse" by
load-independent proxies by default, and show p50/p90 duration only as
`context (load-dependent)`. `--basis wall-time` restores the duration verdict.
Every comparison reports both `proxy_verdict` and `wall_time_verdict`; the
headline `verdict` follows `--basis`. Verdicts are `improved`, `regressed`,
`mixed`, `unchanged`, or `insufficient_sample`.

Store proxies (per job class; wall-time lanes also split by backend and host,
but a job starved before any runner has no host):

| Proxy | Definition | Floor |
|---|---|---|
| `failure_share` | failed / (success + failed) jobs | n>=10 per window; >2 pooled SE and 5 points |
| `cancelled_share` | cancelled / completed jobs | same |
| `starvation_share` | cancelled with no runner ever assigned / jobs with known assignment | same; rows without `runner_assigned` are excluded |
| `attempts_per_pr` | jobs / distinct pull requests | n>=5 PRs; 15% and 0.25 attempts |
| `queue_wait_per_job_ahead_ms` | median of wait / (1 + same-lane jobs queued earlier and still waiting) | n>=10 queued jobs; 25% and 1s |
| `cache_hit_rate` | jobs whose every reported step hit / jobs reporting | n>=10; >2 pooled SE and 5 points |

Every share is over job rows, each judged by that job's own conclusion; a
workflow run's conclusion (which an advisory job elsewhere in the run can turn
red) is never used. Each proxy names its unit in the text (`failure_share
regressed 0.078->0.470 (n=154/336 jobs (success+failure))`) and in JSON
(`proxies[].unit`).

`watch` also attaches to each finding a `denominator` (`unit: "jobs"`, job rows
and success/failure jobs per window, and the job names in the lane) and a
`gate` class: `required` when the lane holds a required status check,
`advisory` otherwise, `unclassified` when no required set is known. The
required set is `--required <check>` (repeatable) or else `[governance]
required_status_checks`; the JSON `required` block says which (`flag`,
`config`, `none`). Required gates are listed first, under their own heading in
human output. A lane is keyed by target, so a gate the importer splits into
several targets (for example native runs and hosted placeholders) is reported
per target, each with its own denominator.

`summary --group-by host` folds machine names into the physical host: a
`*-host-<id>` runner label, else the `shipyard runner tag` in a just-in-time
runner name (`m5studio-pulp-gate-01-42746-25` -> `m5studio`) or in a
registered `<repo>-<tag>-NN` name; `GitHub Actions ...` runners fold into
`github-hosted`. The default `--group-by runner` keeps the recorded name, which
for `metrics import github` rows is the ephemeral runner.

`advise` keys lanes by resolved job name and host the same way, so a gate
served by throwaway runners accumulates samples. A job GitHub reported with an
unevaluated name (a job skipped by its `if:` keeps its {% raw %}`${{ }}`{% endraw %} `name:`
expression, for example `... && 'macos' || 'macos-pr-unused'`) files under the
name it evaluates to when it runs. Only success and failure count: skipped,
cancelled, and unfinished jobs say nothing about a lane's health. Signals:
`preferred_lane` (fastest p50 among lanes with >=3 decided jobs and <=10%
failures), `no_healthy_lane` (sampled, but every lane is over the failure
ceiling; the message lists per-host failure rates and `sample_count` is the
decided total), and `insufficient_healthy_samples` (no lane has 3 decided jobs
yet).

Recording rows that feed the proxies: `shipyard metrics import github` stores
the queue time and whether a runner was assigned automatically. `shipyard
metrics record` accepts `--queued-at`, `--runner-assigned true|false`, and a
repeatable `--label`; without them the queue and starvation proxies read
`insufficient_sample` rather than a guessed value.

`gate-cost` leads with count-based proxies, then minutes as context: gate runs
per merged PR (PR-head, merge-group, wasted attempts), starvation (gate jobs
cancelled before any runner was assigned, with the share withdrawn by a push
to the same PR broken out), placement (jobs whose exact label
set no registered runner advertises and no job in the window was served on,
with a placement-correct share per job class), queue wait per job ahead,
merge-queue attempts per merged PR with ejections by cause (`gate_failed`,
`starved`, `cancelled_after_start`), push cancellations (PR-head gate runs
cancelled after a newer commit on the same branch started a run), receipt
reuse, and batch fullness. Each block carries `evidence.sample` and
`evidence.min_sample`; below the minimum, draw no verdict. Answers that can no
longer change (jobs of a completed run attempt, annotations of a completed
check run, commit parents) are cached under the state directory and reused by
later runs; in-flight runs are always read live. `reads` reports the requests
sent to GitHub and the answers served from the cache; `--no-cache` reads
everything live.

Two listing checks guard the gate-run population. The `created=` window
listing is cross-checked against the plain event listing (newest first,
walked back to the window start, at most 10 pages): GitHub has answered the
filtered listing short with a `total_count` that agreed with the short list
(11 `pull_request` runs where 624 existed), which a page-count check cannot
see. Runs the walk finds are counted and the disagreement is a `run_listing`
gap. And when a run has no job named exactly `--gate-job`, one job per run
attempt whose reported name is an unevaluated expression or {% raw %}`${{ matrix.* }}`{% endraw %}
placeholder resolving to it stands for the gate (the one that got a runner,
else the first that was not skipped). GitHub leaves every `name:` unevaluated
in a run cancelled before its jobs started, so such a run counts one unstarted
gate attempt rather than one per alternate job. The `gate_job_name` gap splits
those runs into cancelled-before-start, ran-under-an-unevaluated-name (the
workflow's gate `name:` no longer evaluates to the gate: make it a literal),
and no gate job at all, so a workflow rename cannot silently shrink the
PR-head count.

`shipyard status` is intentionally limited to queue/target state and does not
probe GitHub quota. Use `shipyard doctor --rate-limit` when you need to confirm
whether Shipyard is using ambient `gh`, an env token, or a command helper such
as a GitHub App installation token, and to see the current REST and GraphQL
rate-limit buckets.

If a checkout has multiple GitHub remotes and no `gh` default remote,
Shipyard refuses to guess which installation a `{repo_slug}` token helper
should use. Pass `--repo OWNER/REPO` to either doctor surface to select the
exact repository for that diagnostic. The value must be a canonical slug;
invalid values fail closed. Without `--repo`, ambiguity remains an error.

### Baseline telemetry identity and phases

Metric observations may carry `--correlation-id` to bind partial, terminal, and
phase observations for one job. When it is omitted, Shipyard derives a stable
fallback from `provider` and `external-id` (`<provider>:<external-id>`). The
identity is stored on the job row and is returned by `metrics list` JSON.

Phase measurements are rows in `steps`: each row names a measured `step` (the
phase), its duration and status, and optional start/completion timestamps. A
terminal refresh updates the existing row only when the phase name matches;
new phase names append rows under the same correlation ID. No phase duration is
inferred from a job total.

### Baseline telemetry identity and phases

Metric observations may carry `--correlation-id` to bind partial, terminal, and
phase observations for one job. When it is omitted, Shipyard derives a stable
fallback from `provider` and `external-id` (`<provider>:<external-id>`). The
identity is stored on the job row and is returned by `metrics list` JSON.

Phase measurements are rows in `steps`: each row names a measured `step` (the
phase), its duration and status, and optional start/completion timestamps. A
terminal refresh updates the existing row only when the phase name matches;
new phase names append rows under the same correlation ID. No phase duration is
inferred from a job total.

`metrics repeated-work-cost` is read-only shadow accounting bounded by
`--since` (default `14d`). It reports measured runtime from distinct
**superseded queue jobs**, never the surviving job: only clean queue timings
count. Uncertain, cancelled, and unmatched superseded jobs remain visible as
coverage counts and are never treated as zero. It never infers cost from
numeric-looking IDs or changes queue authority. In JSON, `joined_durations` and
`unknown_durations` count distinct superseded jobs, while `observations` and
`shadow_rows` preserve raw shadow-row coverage.
