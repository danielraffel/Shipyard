# CLI Reference

```bash
# Setup
shipyard init                  # configure project
shipyard doctor                # check environment + suggest fixes
shipyard doctor --rate-limit   # show effective GitHub auth + REST/GraphQL buckets
shipyard auth doctor           # show Shipyard's configured GitHub auth source
shipyard auth export --output shipyard-auth.toml        # export non-secret auth config
shipyard auth import shipyard-auth.toml --scope local   # import auth config locally
shipyard network tailscale status # show private tailnet reachability
shipyard targets               # show targets + reachability
shipyard targets add <name>    # interactively add a new target
shipyard targets remove <name> # remove a target
shipyard controller status
shipyard controller init --name mac-studio --endpoint ssh=ssh://mac-studio
shipyard controller invite --name m5
shipyard controller join --controller ssh://mac-studio --token syjoin_...
shipyard node list             # show registered controller/client/worker nodes
shipyard node remove <machine-id>
shipyard leave                 # remove local controller-client pairing

# Validate
shipyard run                       # full validation, all targets
shipyard run --smoke               # fast smoke check
shipyard run --targets mac         # single target
shipyard run --resume-from test    # skip setup/configure/build (where supported)
shipyard run --continue            # don't stop at first failure

# Ship
shipyard ship                  # PR → validate → merge on green
shipyard ship --base develop   # target a different branch

# Monitor
shipyard status                # controller-backed when joined; local otherwise
shipyard --local-state status  # force this machine's local state
shipyard queue                 # show all jobs with priorities
shipyard logs <id>             # per-target logs
shipyard logs <id> --target windows
shipyard evidence              # last-good SHA per platform

# Manage
shipyard bump <id> high        # reprioritize a pending job
shipyard cancel <id>           # cancel a job
shipyard cleanup --apply       # prune old logs and artifacts

# Profiles & config
shipyard config profiles       # list defined profiles
shipyard config use <profile>  # switch active profile
shipyard config show           # dump effective merged config
shipyard config set multi_host.controller.enabled true --scope local
shipyard config unset multi_host.controller.enabled --scope local
shipyard config export --output shipyard-setup.toml
shipyard config import shipyard-setup.toml --from local --scope local
shipyard auth export --output shipyard-auth.toml
shipyard auth import shipyard-auth.toml --scope local

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
shipyard runner watch --fix               # auto-cancel stale runs each tick

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
```

The envelope always carries `schema_version: 1` and the command name, so
agents can pin to a stable contract.

`shipyard status` is intentionally limited to queue/target state and does not
probe GitHub quota. Use `shipyard doctor --rate-limit` when you need to confirm
whether Shipyard is using ambient `gh`, an env token, or a command helper such
as a GitHub App installation token, and to see the current REST and GraphQL
rate-limit buckets.

## Setup Movement

For machine-to-machine setup movement, use `shipyard config export --output
shipyard-setup.toml` and restore a chosen layer with `shipyard config import
shipyard-setup.toml --from local --scope local`. Reprovision secrets
separately.

For GitHub App auth specifically, use `shipyard auth export` and
`shipyard auth import`; the export is allow-listed to omit private keys,
tokens, and unknown secret-bearing keys.

## Multi-Host Registry

For controller/client multi-host planning, do not share Shipyard state
directories across Macs. The controller is the only writer for shared queue,
lease, ship, warm-pool, and cloud records; clients join explicitly over an
authenticated protocol. See `docs/multi-host-protocol.md`.

The implemented SSH-backed first slice is available with `shipyard controller
init`, `shipyard controller invite`, `shipyard controller join --controller
ssh://... --token ...`, `shipyard controller status`, `shipyard status`, and
`shipyard leave`. After join, `shipyard status` asks the controller for shared
state; use `shipyard --local-state status` for the laptop-local queue. Remote
enqueue/ship/watch and HTTPS controller RPC are still planned.
