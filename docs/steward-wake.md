# Durable merge-steward wakeups

The Shipyard daemon is the repo-neutral wake source for managed pull requests.
It receives signed GitHub `workflow_run`, `check_suite`, and `pull_request`
webhooks and coalesces terminal transitions into one `runner steward` pass per
repository. The submitting agent can stop after the exact-head steward handoff;
no model session needs to remain alive.

The worker discovers and caches each repository's GitHub default branch for
the daemon lifetime; it does not assume that every repository uses `main`.

Only the host whose runner tag matches the trusted machine-global
`merge_queue.mutation_machine` starts the worker. Other Shipyard daemons remain
passive event consumers, and the steward's ledger lock plus exact-head mutation
guard remain the final single-writer boundary. A worker performs routine queue
admission only: transient reruns, queued-run coalescing, and capacity preemption
stay disabled until their separate durable pilots graduate.

Events are debounced for two seconds. The authority also reconciles once at
daemon start and every 30 minutes, which covers a host that was offline when a
webhook was delivered without turning GitHub Actions schedules or an agent into
a polling controller. Output is bounded in
`<state-dir>/daemon/steward-wake.log`; the latest completion receipt is
`<state-dir>/daemon/steward-wake-status.json`.

Daemon registration must name every repository the authority owns. For the
current shared policy that is:

```sh
shipyard daemon refresh \
  --repo Generous-Corp/pulp \
  --repo Generous-Corp/forge \
  --repo Generous-Corp/vellum
```

Forge Modular and Forge Sequencer share `Generous-Corp/forge`; they therefore
inherit the same steward wake without duplicate repository hooks or controllers.

Webhook registration state is recoverable. If local `registrations.json` is
lost but GitHub still has the exact callback URL, Shipyard adopts and patches
that server-side hook (including event subscriptions and the rotated secret).
It refuses multiple exact-URL matches rather than guessing and leaving a
duplicate live.
