# CLI Reference

```bash
# Setup
shipyard init                  # configure project
shipyard doctor                # check environment + suggest fixes
shipyard doctor --rate-limit   # show effective GitHub auth + REST/GraphQL buckets
shipyard auth doctor           # show Shipyard's configured GitHub auth source
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

# Ship
shipyard ship                  # PR → validate → merge on green
shipyard ship --base develop   # target a different branch
shipyard merge-queue status    # local mutation authority hold
shipyard merge-queue hold --reason "incident"
shipyard merge-queue resume

# Monitor
shipyard status                # dashboard: queue + targets + evidence
shipyard queue-observe         # one read-only queue/PR snapshot; emit on change
shipyard --json queue-observe --follow  # adaptive delta-only NDJSON monitor
shipyard watch                 # live-tail an in-flight ship
shipyard watch local --target linux-vm --command '<cmd>' --milestone-regex '<re>' --terminal-regex '<re>'
shipyard queue                 # show all jobs with priorities
shipyard logs <id>             # per-target logs
shipyard logs <id> --target windows
shipyard evidence              # last-good SHA per platform
shipyard evidence command      # latest workload-agnostic command-evidence bundle
shipyard evidence command --list

# Runner metrics
shipyard metrics record --project pulp --job linux-arm64 --step compile --duration 18.4s --target linux-arm64 --backend local --provider tart-linux --host macstudio
shipyard metrics import github --repo Generous-Corp/pulp --workflow build.yml --limit 10
tartci runtime export --repo Generous-Corp/pulp | shipyard metrics import tartci
shipyard metrics summary --project pulp --json
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
shipyard runner steward-handoff --repo OWNER/REPO --pr 123 --head "$SHA" --workstream-id GEN-7 --context-url https://linear.app/... --apply

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
Semantic blockers receive one deduplicated `shipyard:needs-agent` label and
failed `shipyard/steward-recovery` status, which are cleared after recovery.

`runner steward` is read-only unless `--apply` is present. Same-head duplicate
runs are never cancelled; cancellation authority requires an immutable PR or
merge-group head that differs from GitHub's current head. Apply mode requires
the trusted machine-global `[merge_queue].mutation_machine`, rejects the
central merge-queue `HOLD`, and serializes plus write-ahead audits every
enqueue, rerun, and cancellation through the shared mutation guard. A
repository without a GitHub-native merge queue receives a typed
`direct_merge_refused` decision; Shipyard does not issue a client-side REST
merge because that endpoint cannot atomically enforce complete check
materialization and the validated base revision.
Accepted capacity cancellations remain in the handoff ledger until an exact
run/job read proves terminal; each apply pass resumes those records with an
exact-run force-cancel before planning new work. Read failures keep the record
pending and make the pass unhealthy. Dry-run does not require mutation
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

The agent-facing queries are `summary`, `slowest`, `watch`, `advise`, and
`compare`. Human tables are available by default; `--json` returns structured
rows or findings for plugins, MCP tools, and monitoring agents.

`shipyard status` is intentionally limited to queue/target state and does not
probe GitHub quota. Use `shipyard doctor --rate-limit` when you need to confirm
whether Shipyard is using ambient `gh`, an env token, or a command helper such
as a GitHub App installation token, and to see the current REST and GraphQL
rate-limit buckets.
