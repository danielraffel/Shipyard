# External review Phase 1 specification

Status: review draft; not an activation or deployment claim

This document is the sufficiency checkpoint for Shipyard's first external-
contributor review lane. It separates the intended Phase 1 contract from the
prototype evidence already collected and from capabilities that remain
deferred. The detailed execution policy lives in
`untrusted-contributor-execution.md`; the maintainer workflow lives in
`external-contributor-review.md`.

## Goal

Let maintainers understand and validate an external pull request without ever
executing contributor-controlled code on a maintainer workstation. Shipyard
should produce exact-revision build and test evidence, return concise findings
through the normal GitHub review cycle, and leave the merge decision to a
human.

Phase 1 is successful when it is useful for real reviews while failing closed:
if an eligible isolated executor is unavailable or any boundary assertion
fails, the result is `blocked` or `unverified`. It never falls back to a Mac,
local shell, persistent SSH host, ordinary self-hosted runner, or weaker target.

## Non-goals

Phase 1 does not:

- merge, approve, reject, or otherwise replace human maintainers;
- treat contributor identity, signed commits, or previous merges as permission
  to execute a new revision outside isolation;
- provide interactive agent dialogue inside the guest;
- provide signing, notarization, release, publish, deployment, or production
  credentials;
- prove macOS runtime behavior from a Linux cross-build;
- retain warm disks or caches between reviews;
- support concurrent untrusted guests; or
- make Discord an authority or a dependency of execution or teardown.

## What Phase 1 ships

Phase 1 consists of one conservative, single-job Linux validation lane:

1. A trusted controller admits only a configured repository, authorized review
   request, exact open-PR head, protected base, and maintainer-owned recipe.
2. The controller creates a fresh disposable VM from a pinned, protected image.
3. The exact source and recipe enter as hash-bound, immutable job input.
4. The guest runs fixed build and test commands as an unprivileged user with
   resource, time, output, and filesystem limits.
5. The controller collects bounded structured evidence and is designed to
   destroy the VM, job disk, and input media in all outcomes. Phase 1 may not
   claim that guarantee until the full failure matrix below is proven.
6. Only after confirmed teardown may Shipyard publish a bounded factual result.
   Actionable code or design findings remain ordinary GitHub review comments.
7. GitHub remains the system of record. A human maintainer alone decides and
   performs the merge.

The initial Pulp recipe covers portable Linux configuration, compilation, and
selected tests. Platform behavior outside that recipe is reported explicitly
as unverified.

Harbormaster is optional, downstream visibility only. In Phase 1 it may render
redacted lifecycle and review facts in one quiet activity thread and notify a
maintainer about material findings, ambiguity, infrastructure failure, or a
decision-ready review. It cannot admit work, change policy, send commands to the
executor, waive a finding, or merge. If it is unavailable, validation and
teardown continue normally.

## Explicitly deferred

The following are not required for Phase 1 and must remain disabled rather than
silently approximated:

- interactive or model-driven review inside an untrusted guest;
- an inference broker or any model credential path;
- warm per-PR build disks, shared writable caches, or cross-job state reuse;
- concurrent untrusted jobs;
- a Discord return path, buttons, signed decisions, or execution commands;
- automatic contributor feedback beyond bounded factual GitHub results;
- macOS signing, notarization, runtime tests, plugin-host tests, or release work;
  and
- automatic merge or a trust-based sandbox bypass.

A prebuilt macOS cross-compilation image may be added later as a separate
evidence lane. It may report only `apple-cross: compile-verified`; it cannot
satisfy a macOS runtime requirement. Because the compiler must read its SDK,
that lane requires a separate licensing review and permanent no-egress policy;
making the SDK unreadable to the build user is not a workable control.

## Security model

### Boundary

The operating-system VM boundary contains the entire contributor-controlled
build and test process tree. A temporary worktree, container, secret-free
environment, approval prompt, or static scan is not considered an equivalent
boundary. The trusted controller handles admission and evidence but never loads
or executes checkout-provided hooks, agent instructions, plugins, tools, or
generated programs.

Source acquisition downloads only the repository archive for the exact PR head
SHA. The controller must never check out or recursively resolve contributor-
controlled submodule URLs, Git LFS pointers, package hooks, or build-system
downloads. A dependency absent from reviewed immutable inputs makes the
networkless build fail; it does not authorize a trusted-side fetch.

Every submodule and build dependency used by the Linux lane must fall into
exactly one of these categories:

- it is not required by the protected Linux recipe;
- a reviewed, content-pinned copy is baked into the golden image before that
  image is approved, with its provenance included in the image identity; or
- the build attempts to resolve it inside the networkless guest and fails
  closed because it is unavailable.

The controller must never backfill missing submodule content, resolve Git LFS,
pre-populate or warm a FetchContent or package-manager cache, or make any
per-job dependency request selected by contributor-controlled files. A baked
dependency cache is immutable job input; a controller-populated warm cache is
not permitted.

Each revision is revalidated at its exact head in a fresh isolated execution.
Contributor familiarity affects communication, not revision validation.

### Secrets and privilege

The guest receives no maintainer, GitHub, hypervisor, signing, model, package-
publish, SSH-agent, browser, or private-repository credentials. It has no host
directory mounts, management socket, device passthrough, nested virtualization,
or passwordless privilege escalation. The controller's narrowly scoped
credentials remain outside the guest and cannot authorize unrelated machines,
storage, networks, or executors.

This means a malicious PR cannot obtain a credential merely by reading its
environment, filesystem, process tree, mounted host state, or build input.

### Network

The Phase 1 guest has no DNS, default route, LAN, management-plane, tailnet, or
Internet egress. Required compiler and dependency inputs are baked into the
protected image or supplied as reviewed immutable input. A future build that
requires downloads must use a separately reviewed allowlisted dependency path;
opening general egress is not a Phase 1 escape hatch.

Network denial is defense in depth. The primary exfiltration control is that
the guest contains no standing secret or other private material worth
extracting. A future lane that adds licensed or private build inputs must be
reviewed separately and must preserve permanent no-egress unless the operator
can prove that its readable inputs may be disclosed to every untrusted job.

No software boundary is a proof against every hypervisor vulnerability. Phase
1 reduces that residual risk through a sterile guest, least-privilege control
credentials, no guest network path, no host integration, finite resources, and
mandatory destruction. Whether the physical hypervisor may also host unrelated
services is a documented operator risk decision, not something a passing guest
test can erase.

### Fail-closed lifecycle

Admission checks the protected image-manifest descriptor and template
self-description (it does not re-hash live LVM blocks), then checks the runner,
guest shape, network, mounts,
privilege, hardening marker, resource headroom, and immutable input before code
runs. A mismatch blocks the job. Teardown runs in a controller `finally` path
for pass, failure, timeout, controller error, or guest crash. A run cannot be
reported as passing unless the job VM, disks, and input media are confirmed
absent. Failure to confirm teardown is an infrastructure error requiring human
attention.

Logs and artifacts remain untrusted data. Output is bounded and sanitized; the
trusted host does not execute produced binaries or open active artifacts.
Every rendered surface structurally labels quoted contributor content as
untrusted so it cannot impersonate Shipyard's recommendation or provenance.

The guest-agent result channel is an explicit untrusted-data boundary. The
controller must enforce an independent collection deadline, a strict schema
including nested command records, and a hard encoded-size limit. Missing,
malformed, contradictory, or late results block the run and cannot delay
hypervisor-side teardown.

## Review flow and ownership

1. **Admission:** bind repository, PR, protected base, exact head, contributor
   context, and trusted review policy. First-seen status may cause additional
   maintainer triage but never changes isolation.
2. **Read-only review:** explain purpose and project fit; inspect correctness,
   API and compatibility impact, duplication, maintainability, test quality,
   dependencies, licensing, provenance, security, and unusually large or
   sloppy changes before execution.
3. **Isolated validation:** execute the protected recipe and record exact-head
   evidence plus anything the available lane cannot prove.
4. **Findings:** consolidate substantive blockers and suggestions into normal,
   actionable GitHub review feedback. Avoid repetitive nits and internal
   infrastructure detail.
5. **Revision:** the contributor owns substantive design, code, test, and
   provenance fixes. A new exact head is reviewed and validated again.
6. **Integration:** maintainers own moving-base rebases, version collisions,
   generated markers, and final merge preparation when those are repository
   mechanics rather than defects in the contribution.
7. **Decision:** give the maintainer a short evidence-backed recommendation and
   exactly one named next action and owner. An authorized human handles merge.

No PR should remain in an ambiguous stalemate: a non-terminal review must say
who acts next and what would make the review terminal.

## Phase 1 acceptance criteria

### Named activation blockers

No additional real-PR proof, unattended polling, or result publication may be
enabled until all of these blockers are closed:

- **Source acquisition:** prove with hostile `.gitmodules`, Git LFS, package-
  hook, and build-download fixtures that exact-SHA archive acquisition causes
  no contributor-selected request or code execution on the trusted controller.
  Inventory every submodule and FetchContent or package dependency required by
  the protected Linux recipe, bind each to `not required`, `reviewed and baked`,
  or `fails closed in the networkless guest`, and prove that neither initial nor
  repeated jobs cause controller-side backfill or cache warming.
- **Persistent teardown interlock:** any unconfirmed teardown must set a durable
  admission latch which only an operator can clear after reconciliation.
  Admission must independently prove that the fixed job identity, input media,
  job volumes, and isolated network contain no prior job resources.
- **Mechanical single-job admission:** enforce a controller lock or lease in
  addition to the fixed job identity and service serialization. Concurrent,
  restarted, and duplicate pollers must fail before resource creation.
- **Complete hardened-image identity:** produce, pin, and emit a reviewable
  digest for the complete hardened image, not only its upstream source and
  fixed guest runner.
- **Clean initialization:** replace the deprecated generated-user behavior and
  require a clean guest initialization result in addition to the hardening
  marker. A degraded initialization state is not admissible.
- **Failure and teardown matrix:** prove pass, build failure, command timeout,
  result-collection timeout, malformed and oversized guest results, controller
  interruption and restart, insufficient storage, teardown API failure, and
  stranded-resource recovery. Publication must remain impossible until an
  independent post-delete query confirms resource absence.

After those blockers close, all of the following must also be true before Phase
1 is called active or safe for routine real-PR use:

- [x] The trigger is disabled by default and recognizes only an exact bounded
  command from a host-owned allowlist; PR text cannot select commands, policy,
  credentials, recipe paths, executor, or network.
- [x] Every admitted job is bound to the open PR's exact head and protected
  base, and the effective policy comes only from trusted configuration.
- [x] Automated tests prove that untrusted work cannot resolve to local, SSH,
  persistent self-hosted, or fallback executors.
- [x] A fresh real-PR run proves unprivileged execution, no standing secrets,
  bounded resources and logs, exact-head evidence, and confirmed teardown. The
  protected template and live job have no virtual NIC or IP configuration; the
  guest reports only loopback and no non-loopback IPv4 or IPv6 routes.
- [x] Negative probes prove that guest attempts to reach host, management, LAN,
  and public network paths fail.
- [x] Pass, build failure, command timeout, controller interruption, malformed
  input, insufficient storage, and teardown-failure paths are tested. No path
  publishes a passing result before confirmed teardown.
- [x] Guest-result collection rejects unknown, malformed, oversized, deeply
  nested, contradictory, and late payloads under a hostile-payload and fuzz
  corpus; collection cannot extend the teardown deadline.
- [x] The permanent controller's restricted runtime credential files survive
  reboot and operate without invoking a password manager. Unattended timer
  operation remains a separate unchecked activation gate.
- [x] Controller and hypervisor credentials are least-privilege and cannot
  manage unrelated workloads, general storage, or networks.
- [x] The protected image-manifest descriptor, fixed guest runner, validation
  recipe, and sandbox policy have reviewable identities recorded in evidence.
  Admission pins the descriptor/template self-description; live LVM blocks are
  not re-hashed at admission.
- [x] GitHub publication is bounded, idempotent, sanitized, and contains no raw
  logs, commands, secrets, private paths, or internal security-test detail.
- [x] A hostile-output corpus proves that terminal escapes, mentions, active
  links, Markdown or code-fence injection, overflow, and forged Shipyard
  provenance are neutralized on the GitHub surface. The publisher renders no
  contributor-controlled excerpts; any future surface which does must fence
  them visibly from maintainer-owned conclusions.
- [x] An end-to-end dry run against a current external PR produces a useful
  maintainer brief with one clear next action, without asking the contributor
  to repair moving-base repository mechanics.
- [x] Operational documentation covers enable, disable, blocked-state
  diagnosis, credential rotation, image rebuild, evidence retention, and
  emergency teardown.
- [x] An unattended exact-command trigger proves restart-safe polling,
  idempotent admission, bounded sanitized publication, and confirmed teardown
  without enabling any workstation or fallback execution path.
- [ ] Guest disk allocation and host admission prevent one job from exhausting
  shared storage. The operator explicitly accepts or removes both resource-
  exhaustion and hypervisor-escape risks when unrelated workloads share the
  physical host.
- [x] Independent adversarial review finds no unresolved blocker, and all
  documentation labels unimplemented or unverified behavior honestly.

Harbormaster is not an activation dependency. If its Phase 1 renderer is used,
acceptance additionally requires proving that it receives only bounded,
redacted facts; treats every field as untrusted text; cannot mention users or
activate links or formatting from hostile content; has no execution authority;
and cannot delay validation or teardown. Event emission must be asynchronous
and occur strictly outside the teardown critical section.

## Evidence collected so far

The prototype has already established meaningful but incomplete evidence:

- End-to-end offline smoke runs passed the privilege, host-state, management-
  path, LAN, and public-network negative probes, built and ran a small C++
  target, returned bounded evidence, and confirmed removal of the job VM, disk,
  and input media.
- A broad validation of a real external Pulp revision reached its controller
  wall limit after the offline build completed and tests began. It was recorded
  as blocked and teardown was confirmed. This proves fail-closed timeout
  behavior, not a passing review.
- A focused maintainer-owned recipe then validated the same exact PR head in a
  fresh isolated run. Configuration, selected signal builds, and selected tests
  passed; the controller reported no guest network or standing secrets and
  confirmed teardown. This is focused Linux evidence only, not whole-project or
  macOS evidence.
- The controller's normal runtime credential files work without invoking or
  installing a password-manager client. Backup/bootstrap access remains a
  separate operator concern.

These results do not activate the lane. The remaining unchecked acceptance
criteria above are explicit activation gates.

## Open risks and remaining proof

1. **Activation posture:** a controlled timer run admitted one exact authorized
   comment, validated the exact PR head in a no-NIC guest, published one bounded
   result only after teardown, and remained idempotent across a later poll. The
   timer and publication were then disabled again pending the host-risk decision.
2. **Failure matrix:** the installed suite covers malformed, oversized, and
   fuzzed result data, the independent collection deadline, storage refusal,
   teardown failure, lock/latch behavior, and reconciliation. Live controlled
   jobs prove pass, build failure, wall timeout, one-signal interruption, and
   confirmed cleanup. A protected stranded fixed-ID job was refused and
   latched, then identity-checked, unprotected, destroyed, and independently
   proven absent after placement in the actual disposable pool.
3. **Image identity:** complete logical disk, stable configuration, and
   protected user-data identities are pinned for no-NIC Linux template 132 and offline
   macOS-cross template 131. Template creation is currently a manual,
   digest-attested maintenance procedure rather than a code-reproducible image
   build.
4. **Host residual risk:** the current prototype host is not a dedicated sterile
   CI machine. Ordinary guest paths are blocked, but a hypervisor escape could
   affect unrelated workloads. Production use requires an explicit acceptance
   or migration decision.
5. **Single-job boundary:** job VMs have no NIC, but the fixed VMID, ISO name,
   storage reservation, and admission lock still intentionally enforce one job.
6. **Cloud-init state:** explicit protected user data is installed, and clean
   `done` state with no errors is an admission requirement.
7. **macOS coverage:** the protected no-NIC cross image can compile arm64
   Mach-O objects with SDK 26.5. It remains inactive and cannot provide macOS
   runtime, signing, notarization, or host-integration evidence.
8. **Review usefulness:** the focused execution proof must be paired with a
   complete maintainer decision brief and a normal contributor feedback cycle;
   build success alone does not prove correctness, quality, provenance, legal
   suitability, or project alignment.
9. **Visibility transport:** Harbormaster has only a static renderer prototype.
   No live transport should be built until this execution contract and its
   redaction boundary are accepted.

## Review decision requested

Reviewers should answer three questions:

1. Is the Phase 1 scope useful enough to justify activation without any
   deferred capability?
2. Do the acceptance criteria cover every claim required to call the lane
   fail-closed and safe for routine external-PR validation?
3. Which unchecked criterion, if any, is a blocker before the next controlled
   real-PR proof?

The outcome should be `accept`, `accept with named deltas`, or `reject`, with
each required delta assigned to an owner. Publication of this draft does not
authorize activation.
