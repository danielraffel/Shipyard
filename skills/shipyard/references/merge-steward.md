# Merge steward

`shipyard runner steward` is the agent-neutral merge-on-green reconciler:

```bash
shipyard runner steward \
  --repo Generous-Corp/pulp \
  --repo Generous-Corp/forge \
  --repo Generous-Corp/vellum \
  --json
```

It is audit-only unless `--apply` is present. The apply path uses GitHub's
server-owned exact-head guard for queue admission, preserves existing native
merge-queue positions, does not rerun genuine failures, limits transient
reruns durably, and live-revalidates queued state plus immutable head before
cancelling a run whose PR or merge-group head is provably superseded.
Same-head duplicate runs are never cancelled because their unobserved inputs
may differ. Repositories without a server-owned
merge queue receive a typed `direct_merge_refused` decision: the REST merge
endpoint cannot atomically prove complete required-check materialization or
bind the validated base revision. Use manual merge or enable a native merge
queue.

Ownership is explicit. Before the steward may mutate a PR, the submitting
agent writes an exact-head receipt and management label:

```bash
shipyard runner steward-handoff \
  --repo Generous-Corp/pulp \
  --pr 123 \
  --head "$FULL_HEAD_SHA" \
  --workstream-id GEN-7 \
  --context-url https://linear.app/example/issue/GEN-7 \
  --apply
```

The command validates the open PR and expected SHA, writes the successful
`shipyard/steward-handoff` commit status, re-reads the PR head, and only then
adds `shipyard:managed`. A status stranded on an older head is harmless: the
steward requires the receipt on the current immutable head. PRs without both
signals are reported as `unmanaged` or `handoff_missing` and receive no queue,
rerun, cancellation, or recovery mutation. Add `shipyard:no-auto-merge` to opt
an otherwise managed PR out.

Semantic blockers (`required_failed` or a conflicting/dirty `needs_update`)
produce a failed `shipyard/steward-recovery` status and the
`shipyard:needs-agent` label. This pair is deduplicated on the immutable head;
normal waiting does not summon an agent. When deterministic reconciliation
observes recovery, it marks the recovery status successful and clears the
label. A cheap routed agent can consume this durable exception signal without
polling every healthy PR.

For unattended operation, the configured App or workflow token needs Commit
statuses and Issues read/write. A local read-oriented App that returns the exact
`Resource not accessible by integration` rejection falls back visibly to
ambient `gh` for these low-volume status/label mutations only. High-volume
observation remains on configured auth; a controller should not rely on ambient
fallback.

Capacity preemption is an explicit repository policy. The built-in Pulp policy
may preempt at most one in-progress PR workflow per pass when an exact
merge-group front has waited at least 15 minutes. The candidate must be an
allow-listed advisory workflow actively holding a `pulp-preamble` runner while
every `pulp-build` or `pulp-build-*` leg remains queued or skipped. Required
workflows, including `Build and Test`, are never capacity-preemption
candidates. Unknown repositories have preemption disabled.

The steward fetches the live run and all jobs immediately before cancellation.
Pushes, merge groups, required workflows, unknown workflows/jobs, and any
advisory run already observed with a started or completed expensive leg are
not selected. GitHub does not offer a conditional cancellation tied to that
job snapshot, so correctness rests on the whole selected workflow being
explicitly advisory; the final job read is a waste-avoidance check, not
required-work protection. The hard cross-repository pass cap is one; no CLI
option can raise it. `--no-preempt-capacity` disables this behavior. The
per-head attempt budget and write-ahead audit live in the `handoff_ledger`
path emitted by JSON.

Apply mode holds an exclusive sibling lock for the whole reconciliation and
routes every enqueue, rerun, and cancellation through the
machine-global merge-queue mutation guard. The configured authority machine,
central `HOLD`, process-wide mutation lock, and durable uncertainty audit all
apply before GitHub is mutated; dry-run remains independent of those controls.
This prevents multiple agents on the authority host from racing or losing
attempt/audit state and rejects apply on any other host.

Immediately before a preemption the steward re-reads the native queue and the
front run's jobs, requiring the same exact speculative front SHA and at least
one recognized pool job still queued, waiting, or pending. Front advancement
or a front job starting turns the action into a reported no-op. This also
covers an organization scheduler active-workflow cap: an exact-front
`resolve-provider` job queued on `pulp-preamble` is pressure even when a
matching runner is idle.

This policy never creates VMs or changes governor leases, host caps, or Tart
capacity, so a governor denial remains authoritative and cannot be bypassed.
After GitHub accepts a cancellation, the steward polls the exact run and all
job attempts for up to 15 seconds. Capacity is considered released only after
the run is completed and no job remains queued or in progress. A nonterminal
run records `cancel_not_terminal`, sends GitHub's exact-run `force-cancel`, and
polls once more. If that still does not terminalize, the pass is unhealthy with
an auditable `job@runner` handoff target. The steward never remotely kills or
restarts a runner.

The ledger keeps a pending-cancellation record keyed by canonical repository,
run ID, immutable candidate head, and queue-front head until exact run and job
reads prove terminal. Every apply pass resumes these records before candidate
filtering, using a durable mutation intent to reconcile the exact correlation
without exposing audit-log internals. A transient observation failure leaves
the record pending and makes the pass unhealthy for a later retry.
