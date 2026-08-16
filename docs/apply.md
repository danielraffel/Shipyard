# Applying a routing profile

`shipyard ci profile apply` is where a CI routing profile stops being a
document and starts deciding where real jobs land. It writes the GitHub
repository variables that workflows read for their `runs-on` value.

```bash
# Dry run. Prints every gate and what would be written, then stops.
shipyard ci profile apply normal-local-fast \
  --repo Generous-Corp/pulp --context pr

# Actually write the variables.
shipyard ci profile apply normal-local-fast \
  --repo Generous-Corp/pulp --context pr --apply
```

## Why it is gated at all

GitHub gives no safety net here, in two specific ways:

**A bad route is silent.** A job pointed at a label set no runner carries
does not error. It queues. Forever. The workflow looks pending, GitHub
reports nothing wrong, and the only symptom is that a check never starts.

**A bad route cannot be repaired in flight.** `runs-on` is fixed at the
moment a job is queued. There is no changing it afterwards. Whatever the
variable said when the job was created is what that job will wait on.

Together those mean the route has to be right *before* dispatch, and nothing
downstream will tell you if it was not. So every lane is proved first.

## The gates

Each lane is evaluated against seven gates. Any `FAIL` blocks that lane's
write, with or without `--apply`.

| Gate | What it proves |
|---|---|
| `target-resolves` | The head of the fallback chain is actually defined under `[targets]` |
| `hosted-fallback` | The chain *ends* at a GitHub-hosted target |
| `target-proven` | Self-managed targets carry `proven = true` |
| `runner-group-access` | The declared runner group exists and grants this repository workflow access |
| `dispatch-evidence` | A real job matching `evidence_job_pattern` dispatched within the age limit |
| `topology-check` | `runner_topology_check.py` passes — declared routes agree with live runner state |
| `health-lease-live` | If the lane declares a health lease, that lease is published and fresh |

`hosted-fallback` deserves its own note. Because `runs-on` is frozen at queue
time, the fallback to hosted runners has to already be in the chain when the
job is created. A chain of only self-managed targets has no floor: if those
hosts are down, jobs queue indefinitely instead of degrading to hosted.

Gates that do not apply report `n/a` rather than being omitted — a
GitHub-hosted lane has no self-managed proof to give. Keeping them visible
means the dry-run output is a complete ledger, not a filtered one.

## Reading the output

```
pr.macos -> macstudio.macos-arm64-vm
    target-resolves          PASS  chain head macstudio.macos-arm64-vm is defined
    hosted-fallback          PASS  chain terminates at hosted target github.macos-arm64
    target-proven            FAIL  macstudio.macos-arm64-vm is not marked proven = true; ...
    runner-group-access      FAIL  macstudio.macos-arm64-vm declares no runner_group; ...
    dispatch-evidence        FAIL  macstudio.macos-arm64-vm declares no evidence_job_pattern; ...
    topology-check           PASS  runner topology check passed
    health-lease-live        n/a   lane declares no health lease
  PULP_LOCAL_MACOS_RUNS_ON_JSON NOT written [blocked]
```

Three outcomes, kept distinct:

- **written** — every gate passed and the variable was set (`--apply` only).
- **blocked** — at least one gate failed. Nothing was written.
- **skipped** — every gate passed, but the lane declares no
  `github_variable`. There is nothing to write; this is not a failure.

Exit code is `1` when any lane is blocked, `0` otherwise. Skipped lanes do
not affect it — treating "nothing to write" as a failure would make the exit
code report a problem that does not exist.

## Fields the gates read

```toml
[targets."macstudio.macos-arm64-vm"]
runs_on_json = ["self-hosted", "macOS", "ARM64", "pulp-build-vm"]
proven = true                          # target-proven
runner_group = "pulp-macos"            # runner-group-access
evidence_job_pattern = "macos"         # dispatch-evidence
ephemeral = true
```

## Options

| Flag | Default | Effect |
|---|---|---|
| `--apply` | off | Write the variables. Without it nothing is mutated. |
| `--max-evidence-age-days` | `7` | How stale dispatch evidence may be. |
| `--topology-check` | `tools/scripts/runner_topology_check.py` | Checker to run. |
| `--profile-file` | search path | Explicit profile TOML. |

## Fail-closed observation

Every observation the command makes is best effort, and a read that fails
leaves its gate `false` rather than passing. A missing topology checker, an
unreachable runner-group API, an unparseable variable — all block.

This is intentional and it is the same principle as the fleet epoch check:
"I could not verify this" is not evidence that it is fine. Given that the
failure being prevented is an invisible one, a gate that passes on absent
evidence would be worse than no gate at all.
