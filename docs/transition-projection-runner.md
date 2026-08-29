# External transition projection runner

External status projection is an optional, downstream observation of durable
Shipyard state. It never grants execution authority. Projection I/O and adapter
failure cannot block handoff, queue, merge, or continuation stewardship.

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

Schema v11 stages a deterministic `projection_intents` row in the same SQLite
transaction as every authenticated native producer transition. The intent owns
an immutable canonical receipt snapshot and its SHA-256; no mutable handoff path
is needed to reconstruct the draft. Its `workstream_projection_bindings` row is
populated only from the authenticated `ContinuationBootstrapV1` carried through
`NativePublicationRequest`: workstream handle, plan SHA-256, root/issue/
projection/material revisions, repository, and exact head. Titles, descriptions,
and prose are never handle or owner authority.
The binding identity is immutable. Its exact head advances only in the same
fenced transaction that accepts an authenticated agent-return receipt, so later
intents continue to read their exact head from the binding.

The daemon drains at most 32 eligible intents per pass, one oldest item per
workstream, into the repository's digest-named NDJSON outbox before calling the
existing companion. Appending precedes the SQLite projected mark, so a crash in
between replays as `AlreadyQueued`. Retryable failures remain pending with
bounded backoff; active-claim supersession waits at least one claim lease;
digest and identity contradictions are quarantined. A bad workstream does not
starve another, and repository outboxes need not have contiguous global
sequences because ordering is per workstream. Disabled policy retains pending
rows for an explicit later enablement. The supported kinds are handoff, waiting,
actionable, new head, merge, and configured closure.

Production producers cover managed handoff, waiting observation, actionable,
dispatch and acknowledged ownership handoff, returned new exact head, merged,
and configured closure. `merged` remains a merge transition; `superseded` and
`stale_head` remain separately named configured-closure receipts.
