# Multi-Host Controller Protocol

Shipyard's multi-host mode is an opt-in controller/client model for fleets such
as an always-on Mac Studio plus laptops that can submit work or provide
overflow capacity. Standalone single-machine Shipyard remains the default.

## Implementation Status

This document is the protocol plan for the multi-host controller/client work.
The current implementation has the scheduler capacity accounting needed for
safe multi-slot host pools, `shipyard network tailscale status` for checking
private tailnet reachability, local controller registry initialization with
`shipyard controller init`, one-time invite creation with
`shipyard controller invite`, and registry inspection/revocation with
`shipyard node list` / `shipyard node remove`.

An SSH-backed first controller/client slice is implemented: `shipyard
controller join --controller ssh://host --token ...` consumes a controller
invite over SSH, writes client-local config, stores only bearer-token hashes on
the controller, `shipyard leave` removes local client config, and `shipyard
status` routes to the controller when client config is enabled. Use
`--local-state` to inspect this machine's local state instead.

HTTP controller serving, remote enqueue/ship/watch, periodic heartbeats, and
GUI consumption are still follow-on slices. Until those land, SSH-backed status
proves the trust/config/routing model but does not yet make a laptop enqueue
work to a Mac Studio controller.
When client config is enabled, stateful local commands that are not yet routed
to the controller fail closed unless `--local-state` is supplied, so a laptop
does not silently mutate a separate local queue by accident.

## Roles

- **Controller:** the only process that mutates shared Shipyard state:
  queue files, host-pool leases, ship state, warm-pool records, and cloud run
  records.
- **Client:** a machine with a local Shipyard install that can inspect the
  controller, submit work to it, and optionally register as capacity.
- **Node:** a registered machine, including the controller itself.

Clients must not write the controller's state directory directly, even over a
shared filesystem. Remote operations go through authenticated controller RPCs.

## Pairing

Pairing is explicit. The controller creates a short-lived invite:

```bash
shipyard controller invite --name m5
```

The invite prints a one-time token. Remote join is planned to accept that token
with a command such as:

```bash
shipyard controller join \
  --name m5 \
  --controller mac-studio.example.ts.net:8765 \
  --token syjoin_...
```

The join token is only for bootstrap. A successful join creates a per-node
bearer token for subsequent RPCs and writes client-local controller config.
The SSH transport invokes a controller-side accept command and then uses the
bearer token for controller RPCs; the token is sent through the encrypted SSH
channel and is never written to controller state in plaintext.

## Machine Identity

Each install has a stable generated `machine_id` stored in Shipyard state. The
controller stores registered nodes by `machine_id` and also records display
hostname, platform, architecture, capabilities, and last heartbeat.

Reinstalling or deleting local state creates a new `machine_id`. The old entry
is a ghost node until removed:

```bash
shipyard node list
shipyard node remove <machine-id>
```

Clients can remove their local controller config with:

```bash
shipyard leave
```

## Transport And Endpoints

Tailscale/MagicDNS is the preferred transport, but it is optional. The
controller advertises ordered endpoints:

```json
{
  "endpoints": [
    { "kind": "tailscale_dns", "url": "https://mac-studio.example.ts.net:8765" },
    { "kind": "tailscale_ip", "url": "https://100.64.0.1:8765" },
    { "kind": "lan_https", "url": "https://192.168.86.20:8765", "cert_sha256": "..." },
    { "kind": "ssh", "url": "ssh://mac-studio" }
  ]
}
```

Clients try endpoints in order with short timeouts and cache the last working
endpoint. After repeated failures, the client refreshes the endpoint list from
any reachable endpoint and tries the ordered list again.

Do not build active health probing for every endpoint in the first version.
Lazy retry and last-winner caching are enough.

## Security

Every cross-host RPC after pairing uses the per-node bearer token. Tokens are
revocable from the controller with `shipyard node remove <machine-id>`.

Tailscale transport is preferred because the tailnet supplies private
reachability. Pure LAN fallback must still be authenticated and protected:

- Acceptable: SSH command transport.
- Acceptable: HTTPS with the controller certificate fingerprint captured during
  join.
- Not acceptable: unauthenticated HTTP on the LAN.
- Not acceptable: bearer token over plaintext LAN HTTP.

SSH is useful for recovery and explicit fallback. It should not be the hot path
for high-volume controller RPCs unless no HTTP transport is configured.

## RPC Shape

Every RPC request includes:

- protocol version
- controller id
- client `machine_id`
- bearer token
- request id
- idempotency key for mutating operations

Every RPC response includes:

- protocol version
- status
- request id
- controller clock timestamp
- structured error code when applicable

Mutating RPCs are idempotent. Re-sending the same idempotency key returns the
same controller-side result and never creates duplicate queue jobs.

## Controller-Owned GitHub Access

For shared fleet state, the controller should mediate GitHub API polling and
status aggregation. This avoids each paired Mac burning the same GitHub App
installation bucket with duplicate watchers.

Each node may still configure GitHub App auth for local-only work or degraded
fallback, but shared status and controller-owned work should use controller-side
GitHub access by default.

If GitHub is briefly unavailable, the controller preserves local intent and
backs off. It must not discard queued work, ship intent, run records, or merge
intent just because GitHub is temporarily down.

## Heartbeats And Capacity

Nodes heartbeat to the controller. Host-pool leases use controller-observed
time for staleness decisions. Client clocks are informational only.

When a laptop sleeps or loses network:

1. Its heartbeat stops.
2. Active leases become stale after the configured grace period.
3. The controller marks the node unavailable.
4. Running jobs on that node are failed or requeued with a clear reason such as
   `member heartbeat lost`.

If the laptop wakes after the controller has already failed or requeued the
job, it must not resume the old job silently. Future job kinds may declare
`resumable_on_reconnect`, but validation jobs should restart unless explicitly
designed otherwise.

Leases should record both local process ownership and node ownership. A local
PID is not globally meaningful across machines.

## Controller Outage

The first version has no automatic controller failover. If the controller is
offline because the Mac Studio is rebooting or the network is down:

- shared orchestration pauses
- clients retry with backoff
- local-only work can continue with an explicit local-state mode
- no client writes controller state directly

Controller state is persisted locally, so a reboot should not lose the queue,
ship state, leases, or cloud records. On restart, stale leases and abandoned
running jobs are recovered through the normal recovery path.

Webhook-backed live waits have bounded recovery. GitHub retries webhook
deliveries for a limited time; Shipyard must not imply infinite event replay.
Polling fallback remains required.

## Resume And Watch

When a client submits work to the controller, resume and watch also operate
against controller state. A laptop-local resume file is not authoritative for a
controller-owned ship.

## Error Codes

The protocol should distinguish these cases:

- `controller_unreachable`
- `auth_denied`
- `node_revoked`
- `stale_endpoint`
- `duplicate_idempotency_key`
- `job_rejected`
- `github_unavailable`
- `node_heartbeat_lost`
- `controller_version_mismatch`

Human output should be short and actionable. JSON output should use stable
codes so the GUI and agents can react without parsing prose.

## Configuration And Backup

Controller and client setup should write normal Shipyard config rather than
requiring users to hand-edit files. The expected UX is guided commands:

```bash
shipyard controller init
shipyard controller init --name mac-studio --endpoint ssh=ssh://mac-studio
shipyard controller init --endpoint tailscale-dns=https://mac-studio.example.ts.net:8765
shipyard controller init --endpoint lan-https=https://192.168.86.20:8765#sha256=<fingerprint>
shipyard controller invite --name m5
shipyard controller join --controller ssh://mac-studio --token ...
shipyard controller status
shipyard leave
```

`controller join` currently supports `ssh://` endpoints. HTTPS endpoints remain
validated data-model entries until a pinned-TLS controller server is available.
Low-level settings can also be written through the config CLI:

```bash
shipyard config set multi_host.controller.enabled true --scope local
shipyard config set multi_host.controller.name mac-studio --scope local
```

Backup/export should be explicit about secrets:

- non-secret config can be exported to a portable file with
  `shipyard config export --output shipyard-setup.toml`
- a selected config layer can be restored with
  `shipyard config import shipyard-setup.toml --from local --scope local`
- per-node bearer tokens are secrets
- GitHub App private keys are secrets
- token caches and Keychain/1Password sessions are not exported

A future fleet backup command may collect online node configs through the
controller, but the first portable unit is one export file per Shipyard install.
The bundle records the install's `machine_id` for operator context but import
does not overwrite the destination machine identity.
