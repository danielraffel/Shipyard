# Fleet health leases

A health lease is a short-lived repository variable that says "this
self-managed runner pool is healthy right now." A workflow reads it before
choosing its `runs-on` value: while the lease is in the future, jobs route to
the local pool; when it is absent, malformed, or expired, they fall back to
GitHub-hosted runners.

The point is that the fallback is *automatic and bounded*. If the publisher
crashes, the lease expires on its own and traffic returns to hosted runners
without anyone noticing the outage. Nothing has to succeed for the system to
degrade safely.

```bash
shipyard runner local-linux-lease \
  --repo Generous-Corp/pulp --context pr --lane linux \
  --profile normal-local-fast --apply --watch --interval-secs 60
```

Despite the command's name, nothing about it is Linux-specific or specific to
any one repository. The repository, context, lane, published variable, runner
pool, and capability namespace all come from the routing profile.

> Historical note: this command was previously hardcoded to one repository's
> two Linux lanes. `docs/pulp-local-linux-lease.md` documents that deployment;
> this page documents the general contract.

## Declaring a lease

A lease lives on a lane in the routing profile. All six core keys are
required together:

```toml
[repo."Generous-Corp/vellum".pr.macos]
strategy = "ordered-fallback"
targets = ["m5.macos-arm64-vm", "github.macos-arm64"]
github_variable = "VELLUM_LOCAL_MACOS_RUNS_ON_JSON"

health_lease_variable            = "VELLUM_LOCAL_MACOS_LEASE_UNTIL"
health_lease_ttl_seconds         = 300
health_lease_events              = ["pull_request"]
health_lease_runner_name_prefix  = "vellum-pr-safe-ephemeral-"
health_lease_merge_queue_branch  = "main"
health_lease_admission_burst     = 2

# Optional
health_lease_min_idle            = 3
health_lease_required_capability = "vellum-pr-safe-macos-arm64"
health_lease_forbidden_capability = "vellum-auto-macos-arm64"

[targets."m5.macos-arm64-vm"]
runs_on_json = ["self-hosted", "macOS", "ARM64", "vellum-pr-safe-macos-arm64"]
proven = true
ephemeral = true
```

| Key | Meaning |
|---|---|
| `health_lease_variable` | Variable the expiry is published into |
| `health_lease_ttl_seconds` | Lease lifetime. Must be 60–900. |
| `health_lease_events` | Workflow events this lease authorizes |
| `health_lease_runner_name_prefix` | Name prefix every eligible runner must carry |
| `health_lease_merge_queue_branch` | Branch whose merge-queue concurrency bounds the burst |
| `health_lease_admission_burst` | How many runners must be simultaneously admissible |
| `health_lease_min_idle` | Idle floor for renewal. Defaults to the admission burst. |
| `health_lease_required_capability` | Capability label an eligible runner must advertise |
| `health_lease_forbidden_capability` | Capability label that disqualifies a runner |

A **partial** declaration is an error, not a silent skip. A lane that looks
leased but publishes nothing is precisely the failure a lease exists to
prevent, so it fails at load rather than quietly doing nothing.

`health_lease_min_idle` defaults to the admission burst rather than to zero.
A zero default would let an undeclared floor arm a lease over a fully busy
fleet.

## Namespace safety

Trusted merge-queue capacity and untrusted pull-request capacity are
different trust domains. A runner that will execute unreviewed PR code must
never satisfy a lease meant for post-review merge-queue work. Four generic
rules enforce the separation:

1. **`health_lease_events` may name `pull_request` or `merge_group`, never
   both.** One lease cannot authorize both trust domains.

2. **`health_lease_runner_name_prefix` must end in `-` or `_` and be at least
   4 characters.** Without a trailing delimiter a prefix matches every longer
   sibling namespace: `acme-ci-ephemeral` silently admits every
   `acme-ci-ephemeral-prod-*` runner.

3. **`required_capability` and `forbidden_capability` must differ.** A
   degenerate pair separates nothing.

4. **The lane's first target must carry the required capability and must not
   carry the forbidden one.** This is checked against the target's declared
   `runs_on_json` labels at load time, and again against every live runner at
   observation time.

Violating any of these fails the load. The command will not publish a lease it
cannot prove is scoped to one pool.

## How a tick decides

Each tick observes the fleet and then either renews or clears:

1. Find every registered runner whose labels satisfy the target's selector.
2. If any of them sits **outside** the approved namespace — wrong name prefix,
   or carrying the forbidden capability — **clear**. A contaminated pool is
   never leased.
3. If the live merge-queue concurrency exceeds the declared admission burst,
   **clear**. The profile is under-declared for real demand.
4. Count online, idle, unreserved runners. Queued matching jobs are subtracted,
   because they will consume that capacity.
5. If available capacity meets both the admission burst and the idle floor,
   **renew** for `ttl_seconds`. Otherwise **clear**.

Any unreadable observation clears. As with every gate in this system, "I could
not check" is never treated as "healthy".

Lease time starts only after every read completes, so a slow observation can
delay a renewal but can never publish an already-aged one.

## Operating notes

- `--apply` is required to mutate anything. Without it every tick is a dry run.
- `--interval-secs` must be at least 15 (API hot-loop guard) and shorter than
  the TTL, or the lease expires between ticks.
- Exit code is `0` only when the tick renewed and the write succeeded.
- Keep the TTL at or under 20 minutes. GitHub's queue visibility is seconds;
  a longer lease risks admitting jobs onto a route whose runners are gone.
