# Pulp local Linux health lease

`shipyard runner local-linux-lease` is the external health authority for Pulp's
disposable Mac Pro Linux CI route. It does not dispatch workflows or mutate the
merge queue. It only maintains the short repository variable consumed before a
workflow creates its matrix:

```text
PULP_LOCAL_LINUX_LEASE_UNTIL=2026-08-13T18:50:00Z
```

The value is an RFC3339 UTC expiry. Pulp uses the local selector only while the
value is in the future; an absent, malformed, or expired value selects
`ubuntu-latest`.

## Checked-in policy

The operator reads the lane declaration from Pulp's checked-in
`.shipyard/ci-profiles/normal-local-fast.toml`. The Linux lane has this shape:

```toml
[repo."Generous-Corp/pulp".pr.linux]
strategy = "ordered-fallback"
targets = ["macpro.linux-x64-vm", "github.linux-x64"]
github_variable = "PULP_LOCAL_LINUX_RUNS_ON_JSON"
health_lease_variable = "PULP_LOCAL_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["merge_group"]
health_lease_runner_name_prefix = "pulp-ci-ephemeral-"
health_lease_min_idle = 1

[targets."macpro.linux-x64-vm"]
runs_on_json = ["self-hosted", "Linux", "X64", "pulp-build-linux-x64", "pulp-host-macpro", "pulp-auto-linux-x64"]
```

Required runner labels come from the first target's `runs_on_json`; they are
not duplicated in the lease declaration. Shipyard rejects TTLs outside
60–900 seconds, any event scope other than exactly `merge_group`, a
non-ephemeral runner prefix, a zero idle minimum, or a first target missing
`self-hosted`, `Linux`, `X64`, or the protected automatic opt-in label
`pulp-auto-linux-x64`.

## Operation

Run from a Pulp checkout so Shipyard resolves the checked-in profile. Dry-run is
the default:

```sh
shipyard runner local-linux-lease --repo Generous-Corp/pulp --json
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
authentication. A runner authorizes renewal only when its name has the declared
ephemeral prefix, it carries every label from the first local target, it is
online, and it is idle. Enough matching idle runners renew the variable to
`now + health_lease_ttl_seconds`. Active workflow runs are inspected before
each decision; matching queued jobs reserve idle slots, and
`health_lease_min_idle` is the minimum unreserved idle capacity. Insufficient
capacity, an offline/busy pool, malformed runner or job data, or an unreadable
fleet deletes the variable. A failed
clear, including an ambiguous HTTP 404, is reported as an error. If the
controller crashes, the last value expires without another mutation.

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
