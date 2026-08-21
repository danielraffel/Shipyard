# Fleet release convergence

`shipyard fleet release` keeps every controller/runner host on one immutable
Shipyard build without turning an offline laptop or a busy builder into a
fleet-wide serialization point.

The identity is always both:

- the normalized release version; and
- the SHA-256 of the **installed `shipyard` binary**.

The digest is deliberately not the checksum of a DMG, archive, tag, or source
commit. Version-only checks miss a replaced/rebuilt asset, while an archive
checksum does not prove the bytes that actually reached `~/.local/bin`.

## Inventory

The default inventory is Shipyard's existing
`<global-dir>/fleet-hosts.json`. Its legacy array remains accepted:

```json
[
  {"name": "M1", "ssh": "m1"},
  {"name": "M5", "ssh": "m5"}
]
```

When that form has no local entry, the controller inserts itself as the local
canary. New inventories can make the choice explicit:

```json
{
  "schema_version": 1,
  "hosts": [
    {"name": "M3 Studio", "local": true, "canary": true},
    {"name": "M1", "ssh": "m1"},
    {"name": "M5", "ssh": "m5"}
  ]
}
```

This inventory describes machines, not repositories or job types. Capacity
remains available to any trusted repository whose queue/policy admits it.

## Audit and plan

Before applying a release, obtain the installed-binary digest from a proven
canary installation, then audit without mutation:

```sh
shipyard fleet release status \
  --to v0.97.0 \
  --sha256 <installed-binary-sha256>

shipyard fleet release plan \
  --to v0.97.0 \
  --sha256 <installed-binary-sha256>
```

Every host report includes reachability, CLI version, installed-binary digest,
daemon version (or a positive not-running observation), runner-pool
participation, and busy state. `status` and `plan` are read-only. Exit `0`
means every host is converged; exit `3` means rollout work is pending, busy,
offline, failed, or unobservable; exit `2` is invalid inventory/identity.

## Apply

An apply must include the exact rollback identity:

```sh
shipyard fleet release apply \
  --to v0.97.0 \
  --sha256 <v0.97.0-installed-binary-sha256> \
  --rollback-to v0.96.0 \
  --rollback-sha256 <v0.96.0-installed-binary-sha256>
```

The command writes durable state before the first install and installs a
controller-local `com.shipyard.fleet-release` LaunchAgent. Its executable is an
immutable copy at `~/.local/libexec/shipyard-fleet-controller`, separate from
the managed `~/.local/bin/shipyard`; rolling the fleet back cannot remove the
controller's ability to converge an offline host later. The reconciler replays
the exact state every five minutes. It does not follow `latest`.
The controller carries the checksum-aware installer source it was built with;
it never depends on an older installed CLI or a mutable branch copy of the
installer to bootstrap another host.

Rollout behavior is intentionally per-host:

1. Probe all hosts concurrently.
2. Pick a declared reachable/idle canary. If the declared canary is offline or
   busy, choose another reachable/idle fleet member instead of waiting for one
   machine.
3. Prove the canary's exact version, digest, daemon parity, and unchanged
   participation.
4. Update all other eligible hosts concurrently.
5. Retain offline, busy, and unobservable hosts as pending. They do not stop
   eligible peers.
6. On a later reconciler tick, a rejoined host enters the same convergence
   flow. Already-converged peers are not reinstalled.

Shipyard drains only the host crossing the mutation boundary: it temporarily
turns that host's runner pool off, verifies that no `Runner.Worker` won the
race, performs the exact-byte install, and restores the prior participation
before continuing. Other fleet members remain available throughout. A host
with an active worker is deferred. The participation flag is sampled before
and after installation; failure to restore it is a terminal host failure. A
daemon that was running is refreshed and must report the target version. A host
with no daemon is allowed to remain daemonless.

The durable state defaults to
`<state-dir>/fleet-release/state.json`. Apply/reconcile/rollback share a
machine-wide lock, so a manual invocation and the LaunchAgent cannot write the
same rollout concurrently.

## Rollback

Rollback never resolves `latest` and never guesses prior bytes:

```sh
shipyard fleet release rollback
```

It atomically changes the desired identity to the exact rollback pair saved by
`apply`, resets the canary proof, and uses the same staged/per-host flow.
Repeated rollback invocations are idempotent: once rollback is active they
continue reconciliation rather than toggling back to the failed forward
release. The former forward identity is retained as the next explicit recovery
identity.

## Acceptance battery

A fleet release is accepted only when all of these are proven:

1. Status rejects version-only and malformed digests.
2. A declared offline canary is replaced by an eligible canary.
3. After canary proof, independent eligible hosts occupy the same wave.
4. An offline host stays pending while peers converge, then converges on a
   later invocation without reinstalling those peers.
5. Busy hosts are deferred without blocking idle peers.
6. CLI version and binary SHA match on every host.
7. Every running daemon matches the CLI version.
8. Runner participation is unchanged on every host.
9. The reconciler plist is installed and loaded, and its receipt points to the
   durable state.
10. Rollback is exact, canaried, and idempotent.

The unit/fixture battery lives beside the implementation in
`src/app/fleet_release_cmd/tests.rs`. A production rollout additionally needs
one live `status --json` receipt from every declared host after the release is
published.
