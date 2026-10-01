# exe.dev support

Shipyard can use an exe.dev VM today through its existing POSIX SSH target.
This is the deliberately small integration: exe.dev owns VM creation and
account capacity, while Shipyard owns command execution, logs, artifacts, and
typed evidence.

```toml
[targets.exe-linux]
backend = "ssh"
host = "builder.exe.xyz"
platform = "linux-x64"
repo_path = "/home/exedev/work/project"
timeout_secs = 1800
```

The target can be used with `shipyard watch local` for a streamed workload or
`shipyard run command` for a bounded command and evidence bundle. The latter
records the caller-provided source SHA, remote host, command, exit code,
duration, log excerpt, and copied artifact metadata. Verify the remote
checkout head and compute final executable, artifact, and log hashes separately
when provenance matters. SSH identity and options can be supplied through the
normal target fields; credentials must remain in the user's SSH configuration
or Shipyard's secret-file mechanism.

## Pilot status

The September 28, 2026 pilot used a disposable LAX `exeuntu` VM named
`tree-desk` (2 vCPU, 8 GB RAM, 25 GB disk) on the exe.dev trial account.

- Account capacity: 2 vCPU / 8 GB shared, 25 GB pooled disk, 100 GB monthly
  transfer, 90 days remaining, and a one-time $20 Shelley credit. No payment
  method or invoice was present.
- SSH reached x86_64 Linux as `exedev` after the account's one-time browser
  verification and key registration.
- Shipyard `run command` passed against the VM in 0.99 seconds and pulled an
  artifact into a command-evidence bundle.
- A repeat run passed in 0.54 seconds and observed a marker left on the
  persistent remote disk, proving that persistence is available. This does
  not yet prove a useful compiler-cache hit.
- A local control run passed in 0.04 seconds on the same command-evidence
  path.

The pilot proves that exe.dev is useful as a low-friction persistent Linux
target for Pulp experiments and remote build/test work. It does not replace
native macOS, Windows, Apple GPU/audio, signing, or other physical-machine
gates.

A follow-up exact-head Pulp Linux experiment independently verified the remote
checkout at `49d4de57ebc7a501878f5f574264b4bff8540de3`. The governed
`pulp-cli` build completed on a 2-vCPU VM after dependency setup; a same-VM
warm rebuild completed in 0.23 seconds with an identical executable hash. A
GitHub-hosted Ubuntu/GCC core-library control at the same head also passed in
about 11m51s. These results support optional SSH use and cache reuse on a
retained VM, but do not establish VM cost savings or justify automatic
provider lifecycle management.

## Follow-on plan

1. Keep the existing SSH path optional and user-managed. A retained VM may be
   useful for trusted repeated Linux work, but each experiment must record
   setup, command, cache, disk, transfer, and teardown evidence within the
   trial allocation.
2. If future fresh-VM and cost measurements justify lifecycle management, add
   a typed `provider = "exe.dev"`
   lifecycle adapter. It should create or reuse by job ID, wait for SSH
   readiness, enforce CPU/RAM/disk admission, apply a bounded TTL, and delete
   only resources it owns.
3. Add lifecycle metrics for create, boot, SSH-ready, command, teardown,
   cache-hit, disk, and transfer usage. Unknown ownership or unreadable
   provider state must defer or fail closed.
4. Separately test a persistent exe.dev self-hosted GitHub Actions runner.
   Keep it optional; direct SSH execution remains the simpler evidence path.
5. Reuse the useful TARTCI ideas—weighted resource leases, image/toolchain
   manifests, setup attestations, host-health signals, JIT runner cleanup,
   and JSONL runtime metrics—but do not port Tart-local VM assumptions to a
   remote Linux service.

Provider integration is intentionally deferred until the bounded workload
shows value. The current SSH path is already supported and reviewable.
