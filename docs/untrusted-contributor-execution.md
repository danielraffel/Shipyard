# Untrusted contributor execution

Status: policy proposed; Proxmox execution-boundary prototype deployed

## Decision

Contributor-controlled code must never execute on a maintainer host.

Shipyard may inspect GitHub metadata and publish results from a trusted control
plane, but checkout hooks, setup, configure, build, test, package, generated
tools, and produced artifacts from an untrusted revision must execute inside a
disposable VM. There is no remote or agent-accessible flag that may downgrade an
untrusted revision to the local executor.

This rule applies to direct Shipyard validation, agent-driven review, and
self-hosted GitHub Actions. A temporary worktree or a secret-free environment is
not an isolation boundary.

## Why this is necessary

Shipyard's current `local` backend runs configured commands through the host
shell in the selected checkout. The child inherits the invoking process's
environment. Project configuration is executable too: CMake, package-manager
hooks, compiler launchers, generators, and test discovery can all execute code
before an explicitly named test binary starts.

An untrusted revision running this way can attempt to:

- read the maintainer's home directory, source trees, agent state, shell files,
  SSH material, cloud configuration, browser data, or accessible Keychain items;
- modify other worktrees, persistent build caches, installed tools, launch
  agents, or the runner installation;
- use the network, local services, LAN, tailnet, or cloud metadata endpoints for
  exfiltration and lateral movement;
- persist through shared caches or generated build tools;
- consume unbounded CPU, memory, disk, processes, or wall-clock time; and
- emit terminal escapes or malicious artifacts that attack the trusted process
  which displays, parses, or later opens the output.

Withholding repository secrets prevents one class of credential theft. It does
not protect the host or make native code safe.

## Trust classification

Trust is an input to scheduling, not a property inferred by an executor after it
has started.

The simplest safe default is: only an exact revision already reachable from a
protected repository ref is trusted. Every unmerged PR head executes as
`untrusted`, regardless of author, repository role, prior contributions, or
whether the branch lives in the primary repository. This also covers a
compromised maintainer account and an agent-authored branch influenced by
hostile repository content.

At minimum, classify these other inputs as `untrusted`:

- an arbitrary SHA, bundle, archive, patch, attachment, or generated source from
  outside the protected repository; and
- any job whose source provenance cannot be established.

Author association such as `CONTRIBUTOR` is not sufficient evidence of trust.
The classification and policy must come from host-owned configuration or the
protected base revision. An untrusted PR must not be able to edit its own target,
commands, VM policy, mounts, network policy, credentials, or trust label.

Shipyard should fail closed when classification is missing or contradictory.

If no eligible VM is available, the job remains queued with a visible capacity
reason or terminates as `blocked: no eligible untrusted executor`. It never
falls back to the local host, a persistent SSH machine, a normal self-hosted
runner, or a less restrictive provider. Read-only review may continue, but it
must not claim that the revision was built or tested.

A test which needs unavailable hardware, host UI state, or a non-virtualizable
device is unsupported for an untrusted revision until a suitable isolated lane
exists. Test incompatibility is not a reason to weaken the boundary.

## Execution architecture

Use two separately privileged components.

### Trusted control plane

The control plane may:

- read PR metadata and the protected-base policy;
- resolve the exact head and base SHAs;
- create a source bundle and start an execution VM;
- provide inference through a broker which retains the model credential;
- receive bounded structured results and inert logs; and
- publish a review or check using the Shipyard GitHub App.

It must not execute files from the untrusted checkout or load the checkout's
plugins, hooks, MCP servers, skills, agent instructions, build helpers, or
generated tools.

### Disposable execution VM

The entire reviewer process tree should run in the VM when untrusted repository
content can influence agent behavior. Sandboxing only the terminal command is
insufficient: an agent may also execute through file tools, code-evaluation
tools, plugins, hooks, MCP subprocesses, or other host-side helpers.

Each job gets a fresh copy-on-write VM derived from a signed, immutable image.
The VM is destroyed after result collection whether the job passes, fails,
times out, or the agent crashes.

Run untrusted VMs on dedicated CI hosts which contain no personal login, user
data, browser profile, maintainer source-of-truth checkout, or standing
credentials. Hardware virtualization is a strong boundary, not an absolute
proof against a hypervisor vulnerability; a sterile host keeps a VM escape from
becoming immediate access to a maintainer's life. A logged-in workstation or
laptop is not an eligible untrusted-execution host, even when it can start VMs.

The guest receives only:

- an immutable source bundle for the exact PR head and protected base;
- the protected-base validation recipe;
- a writable, job-scoped scratch disk; and
- narrow broker endpoints required for the job.

It does not receive host directory mounts, the host home directory, SSH agent,
Docker socket, Tart socket, GitHub App credential, maintainer GitHub credential,
signing identity, notarization credential, package-publish token, browser
profile, clipboard, camera, microphone, or unrelated devices.

Run as an unprivileged guest user without passwordless privilege escalation.
Disable nested virtualization and host integration not required by the test.

### Proxmox default lane

A disposable VM on a dedicated Proxmox server is a good default executor for
portable contributor builds. Shipyard should use the Proxmox API from the
trusted control plane to clone a signed golden template onto a per-job
copy-on-write disk, apply CPU/memory/disk/process/time limits, attach the source
as an immutable input, collect bounded results, and destroy the VM and disks.

Put the guest on an isolated bridge or VLAN with no route to the Proxmox
management plane, LAN, tailnet, storage network, or other guests. Do not expose
the Proxmox API token, host filesystem, shared storage credentials, guest
console credential, or QEMU monitor to the job. Disable host directory sharing,
device passthrough, clipboard, and nested virtualization. Any required egress
goes through the same allowlisting broker described below.

Proxmox guests do not replace platform-specific verification. A Linux guest can
validate portable code but cannot prove macOS frameworks, vDSP, Audio Unit,
CoreGraphics, Metal, or other Apple-only behavior.

Pulp's private `pulp-macos-cross-infra` repository substantially narrows that
gap. A trusted image-builder can bake a pinned, reviewed snapshot of its
osxcross toolchain and private SDK input into the golden image. An untrusted job
can then compile and link Mach-O arm64 outputs, build `.component` and AUv3
bundle topology, and verify architecture, symbols, bundle structure, and rpaths
without receiving private-repository credentials or a network path to the
private assets store. The protected control plane injects the public PR source
as an immutable bundle.

This produces `apple-cross: compile-verified`, not macOS runtime evidence. Linux
cannot run `auval`, load an Audio Unit, launch the app, or exercise vDSP,
CoreAudio, CoreGraphics, Metal, codesign policy, Gatekeeper, or plugin-host
behavior. Shipyard must preserve those as distinct evidence lanes rather than
letting structural verification satisfy a runtime requirement.

The default Apple-specific untrusted lane should use a standard ephemeral
GitHub-hosted macOS runner, never a self-hosted label. Its workflow is controlled
by the protected base, uses the `pull_request` event rather than
`pull_request_target`, receives no repository secrets, grants the job token no
more than read-only contents access, and does not restore or publish a cache
which a later trusted job will consume. Produced artifacts remain untrusted and
must not be downloaded and executed on a maintainer host.

Before approving an external workflow, Shipyard must resolve and attest the
actual runner labels. Approval is refused if any Apple job can select a local,
self-hosted, persistent, or ambiguously resolved runner. Repository variables,
matrix output, reroute logic, and fallback chains must not be able to change that
decision for an untrusted job.

If hosted macOS execution is disabled or unavailable, the Apple-specific lane is
reported as `unverified: no eligible untrusted macOS executor`. It never falls
back to a logged-in Mac. Merging a revision does not retroactively constitute
runtime verification of that lane.

## Network policy

Default to no guest network.

Prefer a pre-baked toolchain and dependencies derived from the protected base.
When a build genuinely needs network access, use a control-plane proxy with an
explicit destination and protocol allowlist. Block LAN, tailnet, link-local,
cloud metadata, host services, arbitrary DNS, and direct Internet egress.

Do not expose model or GitHub credentials to the guest. The inference broker
should accept a job-scoped capability and hold the provider credential outside
the VM. GitHub publication happens after the VM exits through the trusted
control plane.

An allowlisted public host is still an exfiltration channel. Prefer artifact and
dependency mirrors which only permit immutable, expected downloads, and record
every allowed or denied request.

## Filesystem and cache policy

- No host bind mounts, including read-only mounts of credential-bearing trees.
- No writable cache shared with trusted or future jobs.
- Seed dependencies from a verified, content-addressed cache and expose it
  read-only; copy into job scratch if a tool requires writes.
- Cap guest disk use and artifact count/size.
- Treat every output artifact and log as untrusted data. Never execute a produced
  binary on the host. Sanitize terminal controls and do not render active HTML or
  SVG in a privileged viewer.

### Optional per-PR build disk

The safest default is fresh scratch for every dialogue turn. A job-scoped warm
build disk may be retained as an explicit performance optimization, but it does
not become trusted merely because it survived a successful turn.

A retained disk must:

- be bound to one repository and PR and never attach to another PR or a trusted
  job;
- contain only disposable build and dependency state, not control-plane state,
  credentials, or the authoritative source bundle;
- have a size quota, a last-used timestamp, and an automatic expiry of at most
  24 hours;
- be destroyed immediately when the PR closes or its lease is revoked;
- surface `workspace=warm`, the disk identifier, and the previous head SHA in
  execution evidence; and
- never satisfy a final approval gate by itself: a fresh-disk rebuild is required
  before Shipyard reports clean reproducibility.

Reusing the disk after a new push to the same PR can carry malicious or stale
state from the previous head. This is acceptable only as explicitly untrusted
incremental evidence, not as a clean build.

## Resource and lifecycle limits

Enforce limits outside the guest:

- wall-clock timeout and idle timeout;
- CPU, memory, process, open-file, and disk quotas;
- maximum stdout/stderr and artifact sizes;
- kill of the full VM, not just the immediate shell child; and
- guaranteed VM destruction and lease cleanup.

Snapshots, caches, and failed-VM retention must be opt-in forensic artifacts
with an expiry, not the default lifecycle.

## Evidence and publication

Shipyard must not publish "verified locally" without an execution attestation.
Every reported result should bind:

- repository, PR number, head SHA, and base SHA;
- VM image digest and sandbox-policy digest;
- exact commands and working directories;
- sanitized environment variable names, with values omitted;
- start/end time, exit status, resource-limit outcome, and VM teardown result;
- network policy plus allowed/denied egress summary; and
- immutable log/artifact identifiers and size hashes.

Reviews and check runs should include a short provenance footer such as:

```text
Executed as untrusted code in disposable VM
head=736f70e6 base=75966995 image=sha256:... policy=sha256:... run=...
network=broker-only standing-secrets=none capability=one-run teardown=confirmed
```

The GitHub identity which publishes the review is not execution provenance.

## Proxmox prototype deployed on 2026-07-14

The current `nexus` Proxmox node now contains a proof of the execution boundary.
This is infrastructure evidence, not yet a production Shipyard executor.

### Host boundary

- Proxmox VE 9.2.4 on x86-64, with four CPU cores and 16 GiB RAM.
- `vmbr1` is `10.77.0.1/24`, has no physical port or uplink, and is reserved for
  untrusted review guests.
- `shipyard-review-firewall.service` loads an `nftables` table before the normal
  network target. New guest-to-host connections on `vmbr1` are dropped, as is
  all routed traffic entering or leaving the bridge.
- IPv4 and IPv6 forwarding were disabled during deployment. The explicit
  forwarding drops remain the fail-closed boundary if another service enables
  forwarding later.
- The `pve/data` thin pool is monitored by `dmeventd` with the
  `shipyard-thin` profile: auto-extension begins at 70 percent and extends by 2
  percent. Disk-use admission and alerting are still required in the
  coordinator.
- The prototype admits at most one untrusted job VM at a time. Guests on the
  same Linux bridge may otherwise communicate at layer 2 without traversing
  the host's routed-traffic rules. Concurrency requires a unique VLAN or bridge
  per job plus a negative cross-guest connectivity test.

### Job image

- VM 119, `shipyard-review-template-v3`, is the protected, powered-off job
  template. VM 118 is its protected backing template and must not be scheduled.
- The job shape is two virtual CPUs, 4 GiB maximum RAM with a 2 GiB balloon
  floor, and an 80 GiB sparse thin disk.
- The signed Ubuntu 24.04 release image was verified with Ubuntu's dedicated
  cloud-image signing key and the published SHA-256 manifest before import. The
  imported source image SHA-256 was
  `ffe6203da54deeb6db5d2a98a83f9ec8e55f149d3f7ba622e1abe5fa966ee3d6`.
- The template includes C/C++, Clang/LLVM, CMake, Ninja, Rust, Python, common
  archive/build tools, and the QEMU guest agent. It does not yet include the
  private macOS SDK or osxcross toolchain.
- The `shipyard` account has no supplementary groups and no passwordless sudo.
  Root/password login, SSH forwarding, unprivileged BPF, unprivileged user
  namespaces, nested virtualization, guest IPv6, DNS, and a default route are
  absent or disabled.
- Every clone must produce `/run/shipyard-review/unprivileged-ready` through the
  post-cloud-init hardening service before the coordinator may inject source or
  start an agent.

### Trusted control base

- VM 121, `shipyard-review-control`, is a protected, powered-off trusted control
  VM with one CPU, 1.5 GiB RAM, and a 20 GiB disk.
- It has only a `vmbr0` management NIC. It has no NIC on `vmbr1`, so an
  untrusted job cannot initiate a connection to the coordinator.
- Its host firewall accepts SSH only from the Proxmox node. Direct SSH from the
  maintainer Mac was verified to time out; administration works through the
  Proxmox jump host.
- Outbound policy permits the LAN resolver/NTP service, the Proxmox HTTPS API,
  and public HTTP(S), while denying other private and link-local IPv4 ranges.
- `/etc/shipyard-review` and `/var/lib/shipyard-review` exist with mode `0700`.
  They currently contain no credentials.
- The eventual coordinator daemon must run as its own non-login, non-sudo
  service account. The administrative cloud user is not an acceptable runtime
  identity for the daemon.

### Smoke evidence

A fresh linked clone from VM 119 was admitted only after its cloud-init and
hardening marker completed. Inside that clone:

- `id` reported only `uid=1000(shipyard) gid=1000(shipyard)`;
- passwordless sudo failed, `/dev/kvm` was absent, and the CPU exposed no `vmx`;
- there was no default route and the static resolver file contained no server;
- Internet and LAN probes failed, while a connection attempt to the Proxmox
  management address through `vmbr1` timed out and incremented the host firewall
  drop counter;
- CMake and Ninja built and ran the repository's C++20 smoke program, producing
  SHA-256 `5717a42c1e46fe90eb6347060915689849e6866fdc0e3caa100885d0b6440260`;
  and
- teardown removed the VM configuration and every `vm-120-*` thin volume.

### Not production-ready yet

Do not route an unmerged PR to this lane until all of these are complete:

1. Install a least-privilege Proxmox API token in VM 121 using the file-backed
   secret policy. Root SSH was acceptable for initial provisioning only.
2. Implement the deterministic lifecycle coordinator: clone, configure, admit,
   inject an immutable source bundle, start, limit, collect, stop, destroy, and
   attest teardown.
3. Add a separately isolated inference broker with a one-run capability. The
   job must receive neither the provider credential nor control-plane
   credentials.
4. Integrate source trust and fail-closed target admission into Shipyard. The
   current `local` backend remains capable of host execution and is not safe for
   an unmerged revision.
5. Implement bounded, inert result transport and GitHub publication through the
   Shipyard App.
6. Add the reviewed macOS cross toolchain to a new signed image generation. The
   current image proves Linux-native builds only.
7. Automate the optional per-PR disk lease and 24-hour expiry before enabling
   warm builds.
8. Keep concurrency at one until each job has a separate layer-2 segment; the
   shared prototype bridge is not a cross-guest isolation boundary.
9. Replace Proxmox's deprecated generated `user` cloud-init field with explicit
   protected user-data, then require a clean cloud-init result rather than the
   currently understood deprecation-only `degraded done` state.
10. Decide whether this risk level is acceptable on `nexus`. It currently hosts
    unrelated home-service VMs and containers, so it is not the sterile
    dedicated CI host recommended by this design. A hypervisor escape could
    threaten those workloads even though ordinary guest networking is blocked.
    The high-assurance production lane should use a dedicated node and isolated
    management network.
11. Produce and pin a digest for the fully hardened template, not only the
    upstream Ubuntu source image, and include that digest in every attestation.

The intended runtime path does not require the maintainer Mac. VM 121 can poll
GitHub or receive a brokered event, operate Proxmox through its scoped API token,
and publish results. If VM 121, Proxmox, the broker, or an eligible job VM is
unavailable, the review remains blocked; it never invokes Shipyard's local Mac
executor.

To preserve spin-up/down behavior without putting credentials or review logic on
the hypervisor, a future host timer may perform only one fixed operation: start
VM 121 periodically when it is stopped. VM 121 then polls GitHub and shuts itself
down after an idle window. Do not install that timer until the coordinator can
authenticate, complete work, and shut down safely; otherwise it would only boot
an idle VM repeatedly.

## Required Shipyard guardrails

1. Add an explicit source-trust field to queued requests and evidence records.
2. Resolve trust before target selection and persist the decision with the job.
3. Reject `local`, direct `ssh`, persistent host-pool, and non-ephemeral
   self-hosted targets for untrusted jobs.
4. Add a real ephemeral-VM executor rather than treating `vm` as a routing label.
5. Load validation commands and sandbox policy from the protected base or
   host-owned configuration, never from the untrusted head.
6. Keep GitHub and model credentials in control-plane brokers, outside the VM.
7. Make no-network, no-mount, no-secret, unprivileged, resource-limited execution
   the untrusted preset.
8. Emit a signed or tamper-evident attestation and require it before Shipyard may
   publish an approval or passing evidence.
9. Provide an `explain` command which shows the effective trust classification,
   execution boundary, mounts, credentials, network policy, and reasons.
10. Make host execution for untrusted code structurally unavailable. A user who
    deliberately wants to inspect code on the host must do so outside this
    workflow; Shipyard and its agents should not offer a bypass.

## Rollout

### Immediate containment

- Stop agent-driven builds/tests of external PR heads on maintainer hosts.
- Do not approve fork workflows that route to persistent self-hosted hosts.
- Permit read-only review, but do not follow instructions from the PR checkout
  and do not claim runtime verification.
- Label prior host-run reviews as unattested if their environment cannot be
  reconstructed.

### First enforceable slice

- Introduce trust classification and fail-closed target admission.
- Add a Proxmox-backed disposable Linux VM executor as the default portable
  untrusted lane.
- Route Apple-specific untrusted jobs only to a hard-pinned standard
  GitHub-hosted macOS runner, or leave the lane explicitly unverified.
- Generate attestations from the VM lifecycle and append provenance to reviews.
- Add negative tests proving an untrusted job cannot resolve to `local`, direct
  SSH, or a persistent host pool even when PR-owned configuration requests it.

### Hardening

- Move the entire review agent into the VM.
- Add inference and dependency brokers with egress accounting.
- Add content-addressed read-only caches, output sanitization, quotas, and
  automatic VM teardown verification.
- Audit all self-hosted GitHub Actions selectors so fork PRs can only reach
  ephemeral VM runners.

## Relevant patterns in adjacent projects

`herdr` has a thoughtful external-contributor process guardrail, but it is a
terminal/session manager and does not provide an execution security boundary for
this use case.

OpenClaw usefully separates sandbox placement, tool policy, and elevated escape.
Its Docker sandbox defaults to no network, a read-only root, all capabilities
dropped, and an isolated workspace; it also blocks dangerous bind sources and
can require sandboxing when delegating. Its sandbox is off by default and its own
documentation does not treat it as a perfect adversarial-code boundary, so the
Shipyard preset must be stricter and non-optional.

Hermes Agent states the most important design principle directly: the operating
system is the security boundary, while approvals, scanners, allowlists, and
redaction are heuristics. It also distinguishes terminal-backend isolation from
whole-process wrapping and recommends whole-process isolation for untrusted
input. Shipyard should adopt that model, using a VM rather than a same-kernel
container for native contributor builds.
