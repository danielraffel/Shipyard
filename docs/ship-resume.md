# Resuming an interrupted ship

`shipyard ship` can take a long time when remote targets are slow
(Namespace, SSH VMs, Windows ARM64 fresh builds). If your session
dies mid-wait — laptop closed, OS restart, Claude Code crash,
switching to another agent — the in-memory state of that ship used
to be lost: the next invocation had to re-dispatch every target
from scratch.

Shipyard v0.3+ persists in-flight state to a per-PR JSON file on
every material event (dispatch, poll update, evidence record). A
fresh invocation reads that file and picks up exactly where the
previous session left off.

## What's saved

One file per PR under `<state_dir>/ship/<pr>.json`:

```json
{
  "schema_version": 1,
  "pr": 224,
  "repo": "danielraffel/pulp",
  "branch": "feature/foo",
  "base_branch": "main",
  "head_sha": "a1b2c3...",
  "dispatched_runs": [
    { "target": "cloud", "provider": "namespace",
      "run_id": "24446948064", "status": "in_progress", ... }
  ],
  "evidence_snapshot": { "macos": "pass", "linux": "pending" },
  "policy_signature": "0123abcd",
  "attempt": 2,
  "created_at": "...",
  "updated_at": "..."
}
```

`state_dir` is the same directory Shipyard already uses for
`evidence/`, `queue/`, and `cloud/` records — typically
`~/Library/Application Support/shipyard` on macOS,
`~/.local/state/shipyard` on Linux,
`%APPDATA%\Local\shipyard` on Windows.

Writes are atomic (tempfile + `os.replace`). Corrupt files on read
are treated as absent, so a half-written file never blocks a
subsequent ship.

## Using resume

Default behavior: if a state file exists for the current PR,
`shipyard ship` auto-resumes.

```sh
shipyard ship              # auto-resumes when state exists, otherwise fresh
shipyard ship --resume     # explicit; same as default when a state exists
shipyard ship --no-resume  # archive the stale state and start fresh
```

On resume, Shipyard refuses to proceed when it detects drift:

- **SHA drift.** The PR head SHA has moved since the state was
  written (someone pushed a new commit). Resuming under a different
  SHA would merge work that was never validated.
- **Policy drift.** `.shipyard/config.toml` now lists different
  required platforms, target names, or validation mode than when
  the state was written. The previous run's evidence may no longer
  satisfy the new policy.

Both cases print a one-line error explaining which check failed and
tell you to re-run with `--no-resume` to archive the stale state and
start fresh.

## Inspecting ship state

```sh
shipyard ship-state list        # one line per active PR
shipyard ship-state show <pr>   # full dump for one PR
shipyard ship-state discard <pr>  # archive and move on
```

`list` is the fastest way to spot abandoned ships — e.g., a PR you
worked on last week whose state never got archived because the
merge failed.

## Cleanup

Ship-state files are small but accumulate over time. Pruning is
opt-in via the existing `shipyard cleanup`:

```sh
shipyard cleanup --ship-state            # dry-run preview
shipyard cleanup --ship-state --apply    # actually delete
```

Rules:

- Archived files older than **30 days** are deleted.
- Active files older than **14 days** whose PR is **closed or
  merged on GitHub** are deleted. Open PRs are always preserved —
  they may still be in flight.

During `--apply`, Shipyard queries `gh pr view <pr>` for each
active state file to determine PR status. A PR whose status cannot
be determined (gh missing, network error) is treated as open and
kept.

## Cross-agent hand-off

The state file is the interop point, not the agent. If you start a
ship in Claude Code, close your laptop, then reopen and use Codex
(or any other agent) to run `shipyard ship`, Codex auto-resumes
the same state. There is no Claude-specific dependency in Phase 1.

## Foreground resume versus unattended continuation

The per-PR ship-state path above is foreground resume: another invocation must
run `shipyard ship` to continue the validation loop. It remains useful without
the daemon and does not transfer process authority to another agent.

Shipyard also has a separate, explicit and default-off unattended continuation
path. An exact steward handoff may publish a protected launch profile and a
zero-wake obligation into the machine-global work ledger. Only after that
publication returns its exact receipt may the trusted daemon consumer own
monitoring and a generation-fenced wake after the submitting session exits.
The daemon revalidates the work item, repository/PR/head, protected route,
profile digest, owner generations, machine allowlist, and adapter identity
before provider I/O. An uncertain outcome is recorded and never blindly
relaunched.

Terminal transport and provider routing are independent. cmux delivery binds a
live process/surface and native checkpoint. HerdR has a registered endpoint
shape but physical delivery remains fail-closed and unproven; it never falls
back to cmux or direct Codex. Subrouter account/model/native-resume choices stay
digest-bound. See [launch profiles](launch-profile.md) and
[terminal/provider adapters](terminal-adapters.md) for the activation and
capability boundaries.
