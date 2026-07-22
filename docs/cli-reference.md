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

# Ship
shipyard ship                  # PR → validate → merge on green
shipyard ship --base develop   # target a different branch

# Monitor
shipyard status                # dashboard: queue + targets + evidence
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
shipyard cleanup --apply       # prune old logs and artifacts

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
