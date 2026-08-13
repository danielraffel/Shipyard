# Pulp local Linux health lease

`shipyard runner local-linux-lease` is the external health authority for Pulp's
disposable Mac Pro Linux CI routes. It does not dispatch workflows or mutate
the merge queue. It only maintains a short repository variable consumed before
a workflow creates its matrix:

```text
PULP_LOCAL_LINUX_LEASE_UNTIL=2026-08-13T18:50:00Z
PULP_PR_SAFE_LINUX_LEASE_UNTIL=2026-08-13T18:50:00Z
```

The value is an RFC3339 UTC expiry. Pulp uses the local selector only while the
value is in the future; an absent, malformed, or expired value selects
`ubuntu-latest`.

The merge-group lane is the currently checked-in Pulp contract. PR-safe support
is dormant until Pulp separately provisions the named ephemeral pool and adds
the PR profile lane, lease variable, and advisory workflow consumer shown below.

## Checked-in policy

The operator reads the lane declaration from Pulp's checked-in
`.shipyard/ci-profiles/normal-local-fast.toml`. The Linux lane has this shape:

```toml
[repo."Generous-Corp/pulp".merge_group.linux]
strategy = "ordered-fallback"
targets = ["macpro.linux-x64-vm", "github.linux-x64"]
github_variable = "PULP_LOCAL_LINUX_RUNS_ON_JSON"
health_lease_variable = "PULP_LOCAL_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["merge_group"]
health_lease_runner_name_prefix = "pulp-ci-ephemeral-"
health_lease_merge_queue_branch = "main"
health_lease_admission_burst = 5

[targets."macpro.linux-x64-vm"]
runs_on_json = ["self-hosted", "Linux", "X64", "pulp-build-linux-x64", "pulp-host-macpro", "pulp-auto-linux-x64"]

[repo."Generous-Corp/pulp".pr.linux]
targets = ["macpro.linux-x64-pr-safe-vm", "github.linux-x64"]
health_lease_variable = "PULP_PR_SAFE_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["pull_request"]
health_lease_runner_name_prefix = "pulp-pr-safe-ephemeral-"
health_lease_merge_queue_branch = "main"
health_lease_admission_burst = 2

[targets."macpro.linux-x64-pr-safe-vm"]
runs_on_json = ["self-hosted", "Linux", "X64", "pulp-build-linux-x64", "pulp-host-macpro", "pulp-pr-safe-linux-x64"]
```

Required runner labels come from the first target's `runs_on_json`; they are
not duplicated in the lease declaration. Shipyard accepts only two complete
namespaces: context `merge_group` with `PULP_LOCAL_LINUX_LEASE_UNTIL`,
`pulp-ci-ephemeral-`, and `pulp-auto-linux-x64`; or PR-safe `pull_request` with
profile context `pr`, `PULP_PR_SAFE_LINUX_LEASE_UNTIL`,
`pulp-pr-safe-ephemeral-`, and
`pulp-pr-safe-linux-x64`. Mixed control tuples or target selectors carrying both
capability labels fail closed. TTLs must be 60–900 seconds, the branch must be
`main`, and admission burst must be positive.

## Operation

Run from a Pulp checkout so Shipyard resolves the checked-in profile. Dry-run is
the default:

```sh
shipyard runner local-linux-lease --repo Generous-Corp/pulp --json

# Separate same-repository PR-safe lane
shipyard runner local-linux-lease --repo Generous-Corp/pulp \
  --context pr --lane linux --json
```

One applied health tick:

```sh
shipyard runner local-linux-lease --repo Generous-Corp/pulp --apply --json
```

Long-running controller mode, suitable for a trusted launchd/systemd service:

```sh
shipyard runner local-linux-lease \
  --repo Generous-Corp/pulp \
  --apply --watch --interval-secs 60 --json
```

Each tick lists all registered runners through Shipyard's configured GitHub
authentication. A runner authorizes renewal only when its name has the exact
lane-specific prefix, it carries every label from the first local target,
it is online, and it is idle. Renewal also fails closed if any registered runner
eligible for the selector sits outside that prefix or carries the opposite
trusted/PR-safe capability label; GitHub schedules by labels, not runner names.
Shipyard also reads the live rules applying to `main`
and extracts the merge queue's `max_entries_to_build` for the merge-group lane.
The PR-safe lane uses its own declared capacity budget because it does not
consume merge-queue admission and GitHub exposes no equivalent repository-wide
PR materialization cap. It must remain advisory: the TTL snapshot is not atomic
admission control and cannot guarantee hosted fallback after a job is assigned.
For the merge-group lane, the declared admission burst must be at least the live
value. For either lane, that many matching runners must
remain idle after already-queued jobs reserve their slots. This covers the
reviewed admission budget for one shared lease snapshot; a TTL is not treated as
atomic admission control.

The renewal expiry is computed only after the runner, job, and branch-rule
observations finish, then written as `observation_completed_at +
health_lease_ttl_seconds`. Insufficient
capacity, an offline/busy pool, malformed runner or job data, or an unreadable
fleet/ruleset deletes the variable. A failed
clear, including an ambiguous HTTP 404, is reported as an error. If the
controller crashes, the last value expires without another mutation.

Pulp's live merge queue admits five entries while its current disposable Linux
fleet has two runners. Therefore the checked-in five-job burst deliberately
keeps this route disarmed: the operator clears the lease and new Linux jobs stay
hosted. Lowering GitHub's build concurrency or arming the selector is a separate
reviewed operation; this command never mutates either setting.

GitHub's repository-runner API does not expose whether registration used the
ephemeral flag. The configured name prefix is therefore a controller-owned
namespace: no persistent runner may register under it. Exact profile labels
remain an independent second requirement.

The credential needs repository Actions-runner read and Actions-variable write
for the target repository. Use Shipyard's configured GitHub App token helper;
never place a token or App private key in the profile, service arguments, logs,
or runner guest.

## Security boundary

This lease authorizes only the generic unprivileged Linux route declared by the
profile. It must not be consumed by `pull_request_target`, Vellum trusted gates,
WebCLAP/deploy jobs, signing, release control, or any other secret-bearing job.
Those jobs retain their separately reviewed hosted or isolated execution path.
The operator never changes required contexts, cancels runs, admits a PR to the
protected queue, or writes `PULP_LOCAL_LINUX_RUNS_ON_JSON`.
