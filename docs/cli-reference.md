# CLI Reference

```bash
# Setup
shipyard init                  # configure project
shipyard doctor                # check environment + suggest fixes
shipyard doctor --rate-limit   # show effective GitHub auth + REST/GraphQL buckets
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

# Monitor
shipyard status                # dashboard: queue + targets + evidence
shipyard queue-observe         # one read-only queue/PR snapshot; emit on change
shipyard --json queue-observe --follow  # adaptive delta-only NDJSON monitor
shipyard watch                 # live-tail an in-flight ship
shipyard watch local --target linux-vm --command '<cmd>' --milestone-regex '<re>' --terminal-regex '<re>'
shipyard queue                 # show all jobs with priorities
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
shipyard metrics summary --project pulp --json
shipyard metrics scorecard --project pulp --since 30d --json
shipyard metrics slowest --project pulp --limit 20
shipyard metrics watch --project pulp --since 14d --json
shipyard metrics advise --project pulp --profile normal --json
shipyard metrics compare --project pulp --lane windows-arm64 --before 7d --after 7d --json

# Manage
shipyard bump <id> high        # reprioritize a pending job
shipyard cancel <id>           # cancel a job
shipyard cleanup               # explain retention/compression actions (dry-run)
shipyard cleanup --apply       # apply log retention and prune artifacts

# Profiles & config
shipyard config profiles       # list defined profiles
shipyard config use <profile>  # switch active profile
shipyard config show           # dump effective merged config
shipyard ci profile show normal-local-fast
shipyard ci profile plan normal-local-fast --repo OWNER/REPO --json

# Governance
shipyard governance status     # declared vs live drift
shipyard governance diff       # dry-run apply
shipyard governance apply      # bring live state in line with config
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
shipyard runner steward-handoff --repo OWNER/REPO --pr 123 --head "$SHA" --workstream-id GEN-7 --context-url https://linear.app/... --apply
shipyard runner steward-handoff --repo OWNER/REPO --pr 123 --head "$SHA" --workstream-id GEN-7 --context-url https://linear.app/... --agent-provider codex --agent-session-id NEW_SESSION --transfer-agent-owner --apply
shipyard runner steward-handoff --repo OWNER/REPO --pr 123 --head "$SHA" --workstream-id GEN-7 --agent-provider codex --agent-session-id SESSION --launch-profile ./launch-profile.json --apply
shipyard runner steward-handoff --repo OWNER/REPO --pr 123 --head "$SHA" --workstream-id GEN-7 --agent-provider codex --agent-session-id SESSION --launch-profile ./launch-profile.json --after-handoff pause --task-graph ./task-graph.json --apply
shipyard pr --workstream-id GEN-7 --context-url https://linear.app/... --launch-profile ./launch-profile.json
shipyard runner recovery-worker                     # inspect/revalidate one pending exception; no model launch
shipyard runner recovery-worker --apply             # run one bounded read-only triage attempt
shipyard runner recovery-worker --drain --apply     # process one bounded pending snapshot (maximum 32)
shipyard work-ledger status                         # inspect canonical shadow storage; does not create it
shipyard work-ledger inventory                      # bounded immutable local-work view; does not create storage
shipyard work-ledger publish --repo OWNER/REPO --pr 123 --head "$SHA" # authentic-v11 exact-row reconciliation plan; no writes
shipyard work-ledger publish --repo OWNER/REPO --pr 123 --head "$SHA" --apply # writer-fenced migrate/bind/publication
shipyard work-ledger reconcile-terminal              # bounded redacted inventory of terminal repairs plus typed unbound handoff rows
shipyard work-ledger reconcile-terminal --repo OWNER/REPO --pr 123 --head "$SHA" # exact dry-run; verifies local and typed terminal GitHub authority
shipyard work-ledger reconcile-terminal --repo OWNER/REPO --pr 123 --head "$SHA" --apply # bind one already-terminal row; exact replay is write-free
shipyard work-ledger import                         # deterministic redacted legacy-import plan; no writes
shipyard work-ledger import --apply                 # idempotently populate shadow storage; no activation/dispatch
shipyard work-ledger policy list                    # list revision-fenced per-repository lane policy
shipyard work-ledger policy set --repo generous-corp/forge --primary-platform macos --compatibility-lane linux --compatibility-lane windows
shipyard work-ledger policy set --repo generous-corp/forge --primary-platform macos --compatibility-lane linux --declared-dependency-lane linux --expected-revision 0 --apply

# On the protected base branch, make every `shipyard pr` submission durable
# immediately after PR creation. A PR branch cannot opt itself in.
# Optional CLI --workstream-id/--context-url values override the fallbacks.
# --launch-profile atomically publishes a zero-wake daemon obligation when the trusted consumer is enabled.
# --after-handoff defaults to continue; pause also requires --task-graph proof.
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

`work-ledger status` supports an authentic schema-v11 ledger through the same
immutable, race-checked snapshot boundary as inventory; it does not migrate or
open a WAL. When native publication encounters that released schema, dry-run
returns a typed disposition for every bound row. Exactly one row must match the
authenticated work ID, repository, PR, head, and workstream. Apply reacquires
the exact snapshot under the exclusive writer domain, migrates schema 11 to
schema 14, enriches only that row with immutable repository identity, and
requires an exact reread/replay. Foreign lineage, ambiguous or changed
snapshots, coordinate drift, and any unbound row refuse the operation.

`work-ledger reconcile-terminal` is the narrow repair for a native
`terminal_handoff` that reached terminal state before its immutable workstream
projection binding was recorded. With no target it returns a bounded,
redacted, no-write inventory of exact terminal repairs and every other unbound
terminal handoff. Non-repairable rows are typed as a clean publication
precursor, managed-unbound state, or blocked, with bounded related-state counts
and explicit blockers; they are never silently omitted or made eligible for the
terminal repair. A targeted command is dry-run by default and
requires the exact repository, PR, and head. Apply additionally requires the
protected launch profile and continuation contracts, the exact route and
authoritative wake, and authenticated GitHub App reads proving one typed
terminal outcome at that head/base. A merged outcome requires its exact merge
commit and timestamp. A closed-without-merge outcome requires its exact close
timestamp and absent merge evidence, and is recorded as `closed_unmerged`
without creating or implying merge authority. Apply requires a second
identical GitHub read under exclusive writer custody. It adds only the immutable provider receipt, projection binding, and
terminal-to-terminal audit event; the binding insert also mints its
schema-required inert ownership-root identity. That root grants no authority:
the repair cannot create agent ownership, holder material, bootstrap
eligibility, or a lease, nor can it revise wakes, routes, continuations,
custody, activation, or projection intents.
Historical unrelated wakes are preserved. Ambiguity, incomplete authority,
head/base movement, an existing ownership root without a binding, or any
mismatch refuses. Repeating the exact targeted command after success is a
write-free replay.

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

`work-ledger` is a migration and inspection surface, not an activation switch.
`work-ledger inventory` returns at most 256 deterministically ordered items and
reports whether the result is complete. It opens only existing ledger storage,
never takes writer custody, and never creates or migrates a database. Each item
binds the canonical `GEN-N` workstream handle and exact work/owner generation to
the provider, immutable repository ID, canonical repository coordinate, PR,
and head. A valid migrated legacy `NULL,NULL` repository identity is retained
and makes `complete=false`; malformed or half-bound identity refuses the entire
snapshot rather than returning ambiguous data.
Its versioned SQLite database uses WAL, full synchronous durability, foreign
keys, integrity checks, protected permissions, and the machine writer-domain
fence. Import selects canonical lifecycle fields and opaque digests from the
legacy ship, queue, recovery, and steward stores; it never copies raw prompts,
terminal text, credentials, provider tokens, or private route identifiers.
Imported work remains explicitly `shadow_imported` and lacks an activation-
eligible continuation contract. Native route records keep terminal, agent, and
provider axes separate and integrity-bound; missing provider provenance never
means Direct and cannot dispatch. Native transitions are closed/typed and
commit their deterministic event with any outbox wake.
The schema also contains durable per-attempt wake delivery records. An internal
consumer contract can claim and finalize one canonical wake around an exact
argv-array provider call, reconcile idempotent restart claims, and retain
non-idempotent ambiguity as `uncertain`. It has no CLI or daemon activation
surface; it cannot run under the default-off policy.
Dry-run is byte-stable and creates no database. Apply is idempotent and leaves
every legacy record authoritative and untouched. Apply holds a bounded
exclusive production-writer snapshot barrier from legacy scan through the
SQLite commit, so it cannot materialize a mixed live-state snapshot. Both
`activation_enabled` and `dispatch_enabled` remain false in this phase.
When the daemon is running, it independently reads policy-covered native
nonterminal exact `(repo, PR, head)` projections from this ledger; inert
`shadow_imported` history is never scheduled. It coalesces relevant
webhooks for two seconds (with a ten-second maximum coalescing age) and performs
an eight-target round-robin catch-up every five minutes, even with zero IPC
subscribers. Webhook overflow is requeued, the same target has a 30-second
cooldown, at most four reads run concurrently, and a rolling-hour 240-request
ceiling bounds passive cost by reserving each selected target's worst-case page
budget durably before a pass, reconciling it to actual cost afterward, and
conservatively restoring in-flight or recent usage after restart.
A shared one-minute deadline covers auth preparation and reads so one slow batch
cannot starve later triggers. Each target uses a read-only, producer-
provenanced head/check snapshot through its exact repository App route and the
daemon's trusted machine-global configuration only; repository and local
overlays cannot replace unattended auth. Non-App command credentials fail
closed; one repository-scoped App installation token is pinned for the complete
paginated target observation. The App token is attached only to the
configured, validated native privileged `gh` executable under a cleared child
environment. Rollups paginate at most 1,000 contexts
and fail closed beyond that bound; request evidence counts every page. Unchanged observations and
initial baselines emit no event. Auth preparation is bounded and is not counted
as a GitHub request when it fails before the command boundary.
A changed snapshot emits `shadow_observation_transition` to IPC and the
daemon's retained supervised stderr log with request count,
wall-clock latency, exact-head verdict, policy revision, and zero model calls.
Fetch failure and recovery also emit once per state change; repeated identical
failures stay quiet and expose only a stable error class, never command output.
The observer cannot update the ledger, GitHub, an outbox, Linear, or an agent.
Its failed-check count is observation evidence, not rerun authority. A later
active phase must resolve exact failed Actions job IDs, preview the complete
dependency closure, estimate worker-minutes against a revision-fenced per-repo
ceiling, and refuse `gh run rerun --failed` whenever closure is unknown or
larger than the classified failed-job set. This shadow phase never reruns CI and
does not invent closure or cost data from check-rollup names.
Legacy import is currently supported only on Unix hosts, where the configured
state directory and every relative component are opened through pinned
no-follow handles. Windows import is explicitly deferred and does not block
the macOS stewardship rollout; status and policy surfaces remain available.
Repository policy is independently revision-fenced. `policy set` requires an
explicit primary lane (Pulp, Forge, and Vellum use `macos`), an explicit
repeatable compatibility-lane inventory, and defaults to `independent`
compatibility scheduling; Linux/Windows may
block another lane only through the default
`declared_dependency_or_shared_integrity` rule. Repeat
`--declared-dependency-lane` only for a real artifact dependency; otherwise a
compatibility lane can block only with evidenced shared-integrity failure.
Dry-run is the default and an
apply with a stale expected revision refuses rather than overwriting a newer
decision. The shadow observer consumes the policy only as an explicit
repository-enrollment and evidence seam; it does not make a blocking decision.
Pulp, Forge, and Vellum can revise their macOS-first and compatibility rules
independently without enabling dispatch. A repository without a policy is not
observed.
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
[the merge-steward reference](../skills/shipyard/references/merge-steward.md).
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
