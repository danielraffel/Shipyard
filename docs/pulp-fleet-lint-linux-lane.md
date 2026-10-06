# Pulp fleet Linux lint lane

A self-hosted fallback for Pulp's merge-queue preamble (`resolve-provider`,
`classify` and the jobs that share their runner) when GitHub cannot assign
hosted Linux runners. Each job runs in its own disposable tart Linux VM on the
Apple-silicon fleet, launched by tartci's `providers/tart-linux` provider, with
egress limited to GitHub and no host mounts. The provider owns boot, isolation,
teardown, the JIT runner and the host resource lease; see tartci's runbook.
Shipyard owns the routing lane, the health report, and the alarm described
here.

## What routes to it, and how

The preamble reads `PULP_PREAMBLE_RUNS_ON_JSON` directly in `runs-on`:

```yaml
runs-on: ${{ fromJSON(github.event_name == 'merge_group' && vars.PULP_PREAMBLE_RUNS_ON_JSON || '"ubuntu-latest"') }}
```

So the selector reaches the fleet for `merge_group` runs only; pull-request,
push and dispatch runs stay GitHub-hosted whatever it holds. A `runs-on`
expression cannot compare a lease expiry with the clock, and the preamble is
the job that would read one, so **no health lease gates this selector**.
Setting it is an operator flip for the duration of a hosted-runner outage, and
unsetting it when hosted capacity returns is part of the same step.

## The checked-in lane

`.shipyard/ci-profiles/normal-local-fast.toml` in Pulp:

```toml
[repo."Generous-Corp/pulp".merge_group.preamble]
strategy = "ordered-fallback"
targets = ["fleet.linux-arm64-lint-vm", "github.linux-x64"]
github_variable = "PULP_PREAMBLE_RUNS_ON_JSON"
health_lease_variable = "PULP_LINT_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["merge_group"]
health_lease_runner_name_prefix = "pulp-lint-ephemeral-"
health_lease_merge_queue_branch = "main"
health_lease_admission_burst = 3
health_lease_required_capability = "pulp-lint-linux-arm64"
health_lease_forbidden_capability = "pulp-pr-safe-lint-linux-arm64"

[targets."fleet.linux-arm64-lint-vm"]
runs_on_json = ["self-hosted", "Linux", "ARM64", "pulp-lint-linux-arm64"]
ephemeral = true
```

The lane is **report-only**. No workflow reads `PULP_LINT_LINUX_LEASE_UNTIL`;
the lease lane exists so the pool's health has one place to be read, and so
the selector has an alarm. The admission burst is the merge queue's
`max_entries_to_build`. The forbidden capability reserves the namespace of a
future pull-request pool, which would be a separate lane: one lease may name
`merge_group` or `pull_request`, never both.

`shipyard ci profile apply` cannot write the selector for this lane. Its
runners are repository-scoped in the Default group, so the runner-group gate
never passes, and the target is not marked `proven`.

## Before setting the selector

Run a dry-run tick from a Pulp checkout:

```sh
shipyard runner local-linux-lease --repo Generous-Corp/pulp \
  --context merge_group --lane preamble --json
```

- `action: renew` means the pool has enough idle `pulp-lint-ephemeral-*`
  runners for the queue's build concurrency. `clear` means it does not, or the
  fleet could not be read.
- `selector_state` reports what `PULP_PREAMBLE_RUNS_ON_JSON` routes to now.
- `selector_alarm` is set when the selector routes to the pool while the tick
  clears, cannot be read while the tick clears, or is not JSON. That is a
  required check about to queue indefinitely, so unset the selector or
  restore the pool. See [Fleet health leases](fleet-lease.md#the-selector-alarm).

Set the selector only after a `renew` tick, and keep a tick running for as
long as it is set. Unset it when hosted runners recover.

## Not covered here

The two workflow_ref-pinned checks, "Enforce version & skill sync" and
"Vellum freeze", cannot reach any self-hosted Linux runner today: their
automatic route requires `workflow_ref` to be `@refs/heads/main`, which a
`merge_group` or `pull_request` run never carries. Routing them here is a
reviewed change to their `runs-on`, and this lane does not claim them.

Running pull-request content on this lane is a separate decision.
Shipyard's [untrusted-contributor execution](untrusted-contributor-execution.md)
guidance names a logged-in workstation as ineligible for untrusted execution,
so a pull-request stage needs an explicit exception from the repository owner.
