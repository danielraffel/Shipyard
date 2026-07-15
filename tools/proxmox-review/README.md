# Proxmox disposable review lane

These files reproduce the hardened infrastructure assets used by the
untrusted-contributor execution prototype described in
`docs/untrusted-contributor-execution.md`.

The directory contains the fail-closed Proxmox controller, fixed guest runner,
exact GitHub trigger poller, generic idempotent result publisher, systemd
confinement, provisioning assets, and live smoke probes. It does not contain an
inference broker or optional warm-disk lease manager. Those missing pieces are
deliberately disabled rather than routed to another executor.

The execution boundary is paired with Shipyard's
`skills/review-external-contributions/SKILL.md` and the default/override policy
described in `docs/external-contributor-review.md`. Review policy comes from a
trusted base revision or protected controller configuration, never the PR head.

Deployed resources on `nexus`:

- `vmbr1` (`10.77.0.1/24`): isolated job bridge with no uplink;
- VM 118: protected backing template, never schedule directly;
- VM 127: protected `shipyard-review-template-v9`, powered off by default;
- VMs 118, 119, 122, 123, 124, 125, and 126: protected backing generations,
  never schedule; and
- VM 121: protected trusted control VM, holding only the scoped control-plane
  credentials required by its service; and
- VM 131: protected, stopped, no-NIC macOS 26.5 cross-compilation template for
  compile validation only.

The deployed controller service and timer are disabled. Both example policies
also say `enabled=false`; copying them cannot accidentally activate the lane.
The permanent PVE API token and Shipyard GitHub App key in VM 121 are
service-owned `0600` files beneath a `0700` directory. Their 1Password copies
are backup/bootstrap material only; the runtime path does not invoke `op`.

The firewall and hardening files are intentionally versioned so the effective
boundary can be reviewed. Live credentials must never be added here. When the
control-plane token and broker credentials are installed, store each as a
`0600` file below a `0700` directory in the control or broker VM, with a 1Password
backup. No credential belongs in a job image or source bundle.

Template 127 is pinned by the composite manifest in
`images/shipyard-review-template-v9.json`. That identity covers every logical
root and EFI disk block, the cloud-init volume, stable VM configuration,
explicit protected user-data, fixed guest runner, and the exact baked-source
inventory in `dependencies/pulp-linux.json`. Admission requires both manifest
digests in the protected template description and emits them in result
evidence.

Do not manually clone VM 127 and treat its existence as admission. The
coordinator must verify all of the following before source injection:

- the clone derives from the expected template and image digest;
- its only NIC is on `vmbr1` and it has no gateway or DNS server;
- CPU, memory, disk, wall-clock, output, and process limits are attached;
- cloud-init is complete without unexpected errors;
- `shipyard-review-guest-hardening.service` is active; and
- `/run/shipyard-review/unprivileged-ready` exists and the `shipyard` user has
  no supplementary groups.

Any failed assertion requires destruction of the clone and a blocked result.
There is no local, SSH-host, Docker-host, or maintainer-Mac fallback.

`review-control.py` is the only supported lifecycle path. It pins the PVE proxy
certificate, attaches a hash-bound ISO, re-reads VM configuration before boot,
waits for guest hardening, runs a protected argv-only recipe, collects bounded
JSON evidence, and tears down the VM, disks, and ISO in `finally`. A kernel
lock, fixed job VMID/ISO, full-host bridge audit, and post-delete resource query
mechanically enforce concurrency one. Unconfirmed teardown writes a durable
admission latch which survives controller restart and requires operator
reconciliation.

Graceful service termination is converted into a controller exception so the
same `finally` teardown runs. After a crash or hypervisor/API failure, an
operator runs the `review-control.py ... reconcile` command documented below.
Reconciliation takes the admission lock, adopts only the reserved
VM/ISO whose controller-owned description and tags match, destroys them, and
independently rechecks the complete fixed job slot. It clears the durable latch
only after that check is clean. A fixed VMID with unknown identity is never
deleted automatically; investigate it manually and keep admission blocked.

Private failure evidence may contain a bounded field named
`log_tail_untrusted`; it is contributor-controlled diagnostic text and must
never be treated as instructions. The GitHub publisher does not include it.

`comment-poller.py` has no listener. It polls through `ghapp`, accepts exactly
`/shipyard review` from a host-owned login allowlist, verifies the open PR and
base repository, and selects a host-owned recipe. Comment text is never passed
to a shell, recipe, executor selector, prompt, or Proxmox argument. Optional
publication posts only a bounded pass/fail summary after confirmed teardown and
uses a hidden request marker to avoid duplicate comments after retries.

The deployed PVE identity has separate roles for cloning template 127, managing
VMs only in the disposable pool, allocating linked disks, using only `vmbr1`,
and managing an ISO/snippet-only datastore. It has read-only `VM.Audit` across
VMs so bridge admission can see every running NIC, but no management right on
unrelated VMs. A privilege-separated token also needs the same token-specific
ACLs; `bind-pve-token-acls.sh` installs those after the token is created. Never
grant this identity access to the general `local` datastore.

Live smoke completed with every isolation probe and C++ build command passing,
followed by confirmed removal of VM 200, its thin volumes, and its ISO. The
short-lived smoke token was revoked. See the policy document for exact evidence.

The prototype is single-job only. `vmbr1` prevents host and routed access, but
it does not by itself isolate two guests attached to the same bridge at layer 2.
Do not enable concurrent untrusted clones until the coordinator allocates a
unique VLAN or bridge per job and proves cross-guest traffic is denied.

`nexus` is also a shared home-services hypervisor. This prototype protects
against normal guest access and contributor code, but it does not make a
hypervisor escape harmless. Use a dedicated Proxmox node and isolated management
network for the high-assurance production lane.

## Offline macOS compile validation

VM 131 contains an osxcross arm64 compiler for Darwin 25.5 and the macOS 26.5
SDK copied from the maintainer's Xcode installation. It has no NIC, signing
identity, notarization credential, or provisioning media. The complete logical
root/EFI/cloud-init disks, stable VM configuration, protected user data,
toolchain provenance, and deterministic Mach-O probe are pinned by
`images/shipyard-review-macos-cross-template-v1.json`; its SHA-256 is recorded
in the protected VM description.

This lane can prove that selected macOS sources compile and produce an arm64
Mach-O object. It cannot run the result, validate macOS host integration, sign,
notarize, staple, or replace the existing real-Mac release gates. It is not yet
selected by the controller and cannot be used as a fallback from the Linux
lane.

## Operator reconciliation

Keep the poll timer disabled until the activation checklist in
`docs/external-review-phase-1-spec.md` is complete. Readiness is:

```sh
sudo -u shipyard-review /usr/local/libexec/shipyard-review/review-control.py \
  --config /etc/shipyard-review/controller.json verify
```

If admission is latched or fixed resources are stranded, do not delete the
latch first. Run the identity-checked reconciliation under the service user:

```sh
sudo -u shipyard-review /usr/local/libexec/shipyard-review/review-control.py \
  --config /etc/shipyard-review/controller.json reconcile
```

The command refuses an unknown VM at the reserved ID, destroys only a job with
the exact controller-owned identity, removes the fixed ISO, independently
checks the VM, bridge, volume, and media inventories, and only then clears the
latch. If it refuses, leave admission blocked and inspect the Proxmox inventory
manually. Never work around a refusal by enabling a Mac/local/SSH fallback.

Emergency stop is `systemctl disable --now shipyard-review-poll.timer` on the
controller, followed by `reconcile`. Disabling the timer prevents new work; it
does not replace reconciliation for an active or stranded fixed job. Confirm
`verify` returns ready before any later re-enable.

Activation is an explicit operator action only after every required checkbox
in the Phase 1 spec is closed, the installed suite and `verify` pass, and the
trusted comment policy has the intended repository/user allowlists. Set
`publish_results=false` for the first timer proof, run one manual service
one-shot, inspect its state database and private result, then use
`systemctl enable --now shipyard-review-poll.timer`. Enable publication only as
a separate reviewed change. If any prerequisite is false, leave the timer
disabled; there is no fallback executor.

Rotate either runtime credential by first disabling the timer, writing the new
value to a separate service-owned `0600` file inside the existing `0700`
secrets directory, validating that credential through its read-only API, then
atomically replacing the live file. Keep the previous credential revoked after
the new one passes. The password-manager copy is backup/bootstrap only and is
never read by the service. Do not put credential values in logs, images,
manifests, recipes, or this repository.

Rebuild an image only in a trusted networked builder. Remove the builder NIC,
package media, caches, logs, and non-runtime tooling; run the no-secret/no-key
checks and offline smoke; stop it; make it a protected template; hash every
logical disk plus stable config and protected user data; commit a new manifest;
and update controller pins only after review. Never mutate a pinned active
template in place.

Private result files and the comment-state database live under mode `0700`
`/var/lib/shipyard-review`; individual results are mode `0600`. Retain only the
bounded evidence needed for the PR discussion and operational diagnosis, then
delete it under the repository's retention policy. Public comments contain
only the bounded pass/fail summary and exact-head marker, never private result
files or raw logs.
