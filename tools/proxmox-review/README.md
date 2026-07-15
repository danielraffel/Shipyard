# Proxmox disposable review lane

These files reproduce the hardened infrastructure assets used by the
untrusted-contributor execution prototype described in
`docs/untrusted-contributor-execution.md`.

The directory contains the fail-closed Proxmox controller, fixed guest runner,
exact GitHub trigger poller, generic idempotent result publisher, systemd
confinement, provisioning assets, and live smoke probes. It does not yet contain
the inference broker, macOS cross image, or optional warm-disk lease manager.
Those missing pieces are deliberately disabled rather than routed to another
executor.

Deployed resources on `nexus`:

- `vmbr1` (`10.77.0.1/24`): isolated job bridge with no uplink;
- VM 118: protected backing template, never schedule directly;
- VM 124: protected `shipyard-review-template-v6`, powered off by default;
- VMs 118, 119, 122, and 123: protected backing generations, never schedule; and
- VM 121: protected trusted control VM, powered off by default and currently
  credential-free.

The deployed controller service and timer are disabled. Both example policies
also say `enabled=false`; copying them cannot accidentally activate the lane.
There is no permanent PVE API token or GitHub credential in VM 121.

The firewall and hardening files are intentionally versioned so the effective
boundary can be reviewed. Live credentials must never be added here. When the
control-plane token and broker credentials are introduced, store each as a
`0600` file below a `0700` directory in the control or broker VM, with a 1Password
backup. No credential belongs in a job image or source bundle.

Do not manually clone VM 124 and treat its existence as admission. The
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
JSON evidence, and tears down the VM, disks, and ISO in `finally`. The sole job
VMID and shared `vmbr1` intentionally enforce concurrency one.

`comment-poller.py` has no listener. It polls through `ghapp`, accepts exactly
`/shipyard review` from a host-owned login allowlist, verifies the open PR and
base repository, and selects a host-owned recipe. Comment text is never passed
to a shell, recipe, executor selector, prompt, or Proxmox argument. Optional
publication posts only a bounded pass/fail summary after confirmed teardown and
uses a hidden request marker to avoid duplicate comments after retries.

The deployed PVE identity has separate roles for cloning template 124, managing
VMs only in the disposable pool, allocating linked disks, using only `vmbr1`,
and managing an ISO-only datastore. A privilege-separated token also needs the
same token-specific ACLs; `bind-pve-token-acls.sh` installs those after the token
is created. Never grant this identity access to the general `local` datastore.

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
