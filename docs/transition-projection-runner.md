# External transition projection runner

External status projection is an optional, downstream observation of durable
Shipyard state. It never grants execution authority and cannot block handoff,
queue, merge, or continuation stewardship.

The daemon loads `[transition_projection]` only from protected machine-global
configuration. Absent or disabled policy performs no projection I/O. Enabled
policy names an exact native companion path and SHA-256, fixed token-only argv,
bounded execution limits, a repository allowlist, and optional environment
variables whose values are paths to owner-private secret files. Secret values
and transition payloads are never placed in argv.

```toml
[transition_projection]
enabled = true
executable_path = "/opt/shipyard/bin/transition-projector"
executable_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
argv = ["linear-v1"]
deadline_seconds = 30
max_stdout_bytes = 65536
max_stderr_bytes = 65536
repositories = ["owner/repository"]

[transition_projection.secret_files]
LINEAR_API_KEY_FILE = "/Users/operator/.config/shipyard/secrets/linear-api-key"
```

The companion receives strict JSON v1 on stdin and returns strict JSON v1 on
stdout. Submit and readback are separate idempotent operations. Shipyard does
not acknowledge an item until readback returns the exact transition and
evidence identities. A crash after external acceptance therefore replays the
same idempotency key after lease expiry.

Authoritative producers must call the commit-before-enqueue ingress only after
their source receipt is durable. The ingress opens that exact private receipt
without following symlinks, hashes it, compares it with the transition evidence,
and then appends to the repository's digest-named outbox. The supported kinds
are handoff, waiting, actionable, new head, merge, and configured closure.
Producer call sites are intentionally a separate integration pass so this
runner does not infer transitions from terminal labels or mutable daemon state.
