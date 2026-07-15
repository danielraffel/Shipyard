# Proxmox review-lane prototype

These files reproduce the hardened infrastructure assets used by the
untrusted-contributor execution prototype described in
`docs/untrusted-contributor-execution.md`.

They are not a complete executor. In particular, this directory does not yet
contain a Proxmox API client, source-bundle transport, inference broker, GitHub
publisher, resource lease manager, or teardown attestation implementation.

Deployed resources on `nexus`:

- `vmbr1` (`10.77.0.1/24`): isolated job bridge with no uplink;
- VM 118: protected backing template, never schedule directly;
- VM 119: protected `shipyard-review-template-v3`, powered off by default; and
- VM 121: protected trusted control VM, powered off by default and currently
  credential-free.

The firewall and hardening files are intentionally versioned so the effective
boundary can be reviewed. Live credentials must never be added here. When the
control-plane token and broker credentials are introduced, store each as a
`0600` file below a `0700` directory in the control or broker VM, with a 1Password
backup. No credential belongs in a job image or source bundle.

Do not manually clone VM 119 and treat its existence as admission. A production
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

The prototype is single-job only. `vmbr1` prevents host and routed access, but
it does not by itself isolate two guests attached to the same bridge at layer 2.
Do not enable concurrent untrusted clones until the coordinator allocates a
unique VLAN or bridge per job and proves cross-guest traffic is denied.

`nexus` is also a shared home-services hypervisor. This prototype protects
against normal guest access and contributor code, but it does not make a
hypervisor escape harmless. Use a dedicated Proxmox node and isolated management
network for the high-assurance production lane.
