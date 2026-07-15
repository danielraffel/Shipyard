# External review Phase 1 evidence

Status: containment implementation and controlled proof in progress. The poll
timer and GitHub publication remain disabled. This file records evidence; it
does not activate the lane.

## Installed control plane

- The trusted controller is VM 121. Runtime credentials are service-owned mode
  `0600` files inside a mode `0700` directory and survived reboot without a
  password-manager invocation.
- The Proxmox token has job-pool lifecycle rights, dedicated ISO/snippet rights,
  template-clone rights, and read-only fleet VM audit. On an unrelated VM it
  has `VM.Audit` only.
- The systemd timer is `disabled` and `inactive`.
- The installed Python failure/policy suite passes 31 tests. Readiness reports
  template VM 127 ready after independent fixed-slot, storage, template,
  manifest, clean-init-policy, latch, and lock checks.
- A held kernel lock, a durable latch, an orphan job ISO, and a latch surviving
  controller reboot each blocked readiness. Removing the synthetic condition
  restored readiness; no alternate executor ran.

## Source and dependencies

- GitHub acquisition is an exact-head-SHA archive request. Repository and SHA
  validation rejects path/query injection; no controller `git`, submodule, LFS,
  package hook, or FetchContent resolution is used.
- The protected Linux dependency inventory classifies every recipe dependency.
  In a fresh no-NIC clone, the offline verifier matched all ten baked source
  directories to exact commits and `git archive` hashes and rejected unexpected
  `*-src` directories in its negative test.
- The proof clone, its volumes, and the temporary verification ISO were removed
  and independently absent afterward.

## Linux image and execution

- Protected template VM 127 is stopped and pinned by composite manifest SHA-256
  `6e891728d885669aacfd40e8bdd8208b9d202b73dc15998268e46c7afafbd4d3`.
- A fresh clone completed explicit cloud-init with `status=done`,
  `extended_status=done`, no errors, no recoverable errors, no SSH keys, and the
  expected unprivileged marker. The clone and all volumes were removed.
- An offline smoke passed privilege, host/LAN/management/public-network probes,
  compiled and ran a C++ target, returned bounded evidence, and confirmed
  teardown.
- A broad exact-head run of external PR 6115 hit the controller wall deadline
  after its offline build and failed closed with confirmed teardown. A later
  focused recipe passed selected builds/tests for the same exact head in a new
  disposable VM. Neither run enabled publication.
- Maintainer integration of PR 6114 was rebased onto current main with Matthew's
  feature authorship preserved and repository-owned version/provenance cleanup
  kept separate. Exact head
  `c2f37ca01fecdbddc6529f4646101df618a585d8` then configured, built the
  spectral/FFT/real-time-safety targets, and passed the focused CTest selection
  in a fresh disposable VM. Teardown was confirmed after 288.895 seconds and
  the fixed VM, volumes, and ISO were independently absent. The public update
  names one next action: Daniel's final review and merge decision.

## macOS cross image

- Protected template VM 131 is stopped, has no NIC or provisioning media, and
  contains no signing/notarization identity.
- It uses SDK 26.5 with the pinned osxcross infrastructure commit `f4aad34` and
  Darwin 25.5 compiler wrappers.
- The clean no-NIC candidate compiled a deterministic C source into a Mach-O
  64-bit arm64 object with SHA-256
  `32160db1e79bfb4e9d8fc94f379a52d0196864b7c9c23f2cfcf1f522d74d0d9a`.
- Complete image identity is recorded in
  `tools/proxmox-review/images/shipyard-review-macos-cross-template-v1.json`
  (manifest SHA-256
  `82cf0fc9d5a0d574add5cba26441670c7a574478204ec43b16404805b09d4ff6`).
- This proves compile structure only. Runtime, signing, notarization, and macOS
  host behavior remain explicitly outside this lane.

## Still required before activation

- The installed/live failure matrix is closed: pass, structured build failure,
  command/wall timeout, single-SIGTERM interruption, malformed/oversized/fuzzed
  and contradictory result payloads, independent result-read deadline,
  insufficient-storage refusal, teardown failure/latch, controller reboot, and
  protected stranded-resource reconciliation are covered. The live build-fail
  job returned fail with confirmed teardown; SIGTERM returned blocked and
  removed all fixed resources; and reconciliation refused then removed an
  identity-checked protected job in the actual disposable pool.
- Prove unattended exact-command polling, idempotency, and sanitized bounded
  publication in a controlled non-publishing/dry-run sequence before enabling
  either feature. One installed systemd one-shot already completed successfully
  with no exact trigger present, recorded 100 comments as ignored, created no
  job, and left the timer disabled/inactive; publication remains unit/corpus
  proven but disabled.
- Produce the complete maintainer brief for a current external PR and have the
  final exact diff plus installed evidence independently adversarially reviewed.
- Decide whether shared-host hypervisor-escape residual risk is accepted for
  this personal review lane. A dedicated node remains the higher-assurance
  posture.
- Jointly review any later one-way Harbormaster event transport. Visibility is
  downstream only and is not an activation dependency.
