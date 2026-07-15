# Fable independent security review — Phase-1 spec

Independent adversarial pass over `external-review-phase-1-spec.md` and its
load-bearing companion `untrusted-contributor-execution.md`. Focus, as
requested: escalation / exfiltration / teardown evasion / Phase-1 sufficiency /
overclaimed evidence / missing acceptance criteria. Blocker-first.

**Bottom line:** As specced, Phase 1 is *not yet* safe to run against a hostile
PR. B1 and B2 are live holes in the containment story; B3/B4 are load-bearing
preconditions the companion defers but the spec must promote to named
activation blockers. Recommendation: **accept with named deltas — B1–B4 are
hard blockers before the next real-PR proof.**

---

## BLOCKERS

### B1 — Source-bundle assembly resolves attacker-controlled submodule/LFS URLs on the egress-capable control plane (EXFILTRATION / SSRF)
Spec "What Phase 1 ships" L46 ("hash-bound, immutable job input"); companion
L92, L121 ("create a source bundle"); companion L337-338 (VM 121 outbound policy
permits public HTTP(S)).

Neither doc specifies **how the immutable source bundle is assembled**, and the
trusted control plane (VM 121) has public HTTP(S) egress. Pulp uses git
submodules (`planning/`, `external/*`) and FetchContent. A malicious PR ships a
`.gitmodules` (or git-LFS pointer) whose URL points at an attacker host. If
bundle assembly does `git submodule update --init --recursive` or an LFS smudge
on the trusted side, the egress-capable control plane makes an outbound request
to an attacker-chosen URL — SSRF against the LAN resolver / NTP / Proxmox-API
endpoints VM 121 can reach, and an exfil beacon to the Internet — **before the
untrusted code ever enters the VM.** The guest's no-network posture is
irrelevant; the fetch happens in the trusted context. The boundary section
(L89-96) only promises the controller won't *execute* checkout code — nothing
about *fetching* attacker-named URLs.

**Fix:** Bundle assembly fetches **only the PR head tree by SHA**
(`git archive` / GitHub tarball). Recursive submodule/LFS/FetchContent
resolution on the trusted side is prohibited; submodule content is pinned-and-
mirrored through a reviewed allowlist, or the build fails closed inside the
networkless guest. Acceptance: a PR with a hostile `.gitmodules`/LFS pointer
produces zero outbound request from the control plane.

### B2 — Teardown failure does not halt admission → next run shares layer-2 with a stranded untrusted guest (TEARDOWN EVASION; breaks the fail-closed headline)
Spec "Fail-closed lifecycle" L135-136; companion L298-301, L445-447 (vmbr1
shared bridge is NOT a cross-guest isolation boundary; guests talk at L2).

Fail-closed is asserted only for *publication* (L52: no passing result before
confirmed teardown), not for *containment*. Chain: teardown fails (thin volume
won't delete / VM won't destroy / host wedged) → poller still enabled → next
admitted job clones onto a host where a live untrusted VM from the prior run
sits on the same `vmbr1` → two untrusted guests reachable at L2 → cross-run
persistence and communication. The spec's own non-goal "support concurrent
untrusted guests" (L37) is violated by a *failure*, not a config choice.

**Fix:** Hard interlock — any unconfirmed teardown sets a persistent latch that
blocks all further admission until an operator clears it; admission must
independently verify `vmbr1` has zero other guest VMs and no orphaned `vm-*`
thin volumes before clone. Both are acceptance criteria.

### B3 — Concurrency=1 is an operational assumption, not a mechanical interlock
Spec non-goal L37; acceptance L171-203 (no interlock listed); companion L298
("admits at most one untrusted job VM at a time").

The single-job property rests on coordinator behavior with no hard gate. Any
admission race, restart-during-run, or the B2 stranded-VM case produces two
untrusted guests on a shared L2 segment the design says is not an isolation
boundary.

**Fix:** Mechanical mutex (lease/lock) enforced at admission, plus the pre-clone
"no other guest on vmbr1" check from B2. Until per-job L2 isolation exists
(companion #5), the interlock — not operator discipline — is the safety
mechanism, and must be an acceptance criterion.

### B4 — Load-bearing deferred items are mis-filed as ordinary acceptance checkboxes (PHASE-1 SUFFICIENCY)
Spec acceptance L190-191; companion open-risks #2/#3/#6 and disabled-gates #6/#8.

Three items listed as flat checkboxes are currently false/untested AND
load-bearing for containment:
- **Full hardened-image digest (companion #3/#8):** not yet produced — only the
  upstream Ubuntu source image and guest-*runner* digest are pinned. "Hash-bound
  immutable job input" (L46) / "reviewable immutable identities" (L190) are only
  partially true for the image; image drift/substitution is undetectable.
- **Clean cloud-init as admission gate (companion #6):** deprecated generated
  `user` field still in use; a `degraded done` cloud-init state is *currently
  accepted*. The marker `/run/shipyard-review/unprivileged-ready` can be emitted
  while cloud-init only partially applied — admission accepts a guest whose
  claimed hardening may be incompletely applied.
- **Full failure/teardown matrix (companion #2):** controller-crash, host-wedge,
  malformed-input, insufficient-storage, teardown-API-failure paths untested.
  Cannot call a system "fail-closed" while its failure paths are unexercised.

**Fix:** Reclassify image-digest, clean-cloud-init-admission, the concurrency
interlock (B3), and the full failure/teardown matrix as **named activation
blockers** at the top of the acceptance section, distinct from general criteria.
No real-PR run until all four are green.

---

## HIGH

### H1 — QEMU Guest Agent result channel is untrusted data crossing into the trust/teardown decision, absent from the boundary analysis
Spec "Secrets and privilege" L106 ("no management socket"); boundary L89-96;
companion L370-371, L400-413 (results returned through QEMU Guest Agent).

The guest returns build/test JSON, exit codes, byte counts, hashes over the
qemu-ga virtio-serial channel, and the controller parses that attacker-
influenced payload to decide pass/fail and record `teardown=confirmed`. The
boundary section promises the controller "never loads or executes generated
programs" but says nothing about ingesting guest-produced structured data into
its control flow. qemu-ga is itself a guest→host integration surface the "no
management socket" claim glosses over.

**Fix:** Name qemu-ga as the result-transport trust boundary; controller treats
the payload as hostile (strict schema, hard size cap independent of the 1 MiB
log cap, reject-on-parse-failure → blocked); add a fuzz/adversarial-payload
acceptance test on the collector.

### H2 — Malicious guest can wedge result collection; no independent collection deadline
Spec "Fail-closed lifecycle" L128-136; companion L243, L370.

Lifecycle is run → collect via qemu-ga → destroy. A hostile build can kill
qemu-ga or block the virtio-serial channel so *collection* hangs. The proven
timeout evidence (L219-223) fired during *test execution*, not during a
maliciously wedged collection.

**Fix:** Result-collection has its own hard wall deadline; on expiry, abandon
evidence → force `qm destroy` → report `blocked`. Add to the tested failure
matrix (B4).

### H3 — Future baked private macOS SDK is readable by the untrusted guest; only no-network protects it, while the spec reserves the right to add egress (EXFILTRATION)
Spec "Network" L116-119; deferred L83-85; companion L152-159 (osxcross + private
SDK baked into the golden image).

Once the apple-cross lane lands, the private Apple SDK sits inside the guest
filesystem, readable by the unprivileged build user. The only barrier to
exfiltrating licensed SDK material is the no-network posture — and the same
Network section reserves the right to add "a separately reviewed allowlisted
dependency path" (L118-119). Baked-in secret + any future egress = SDK exfil.

**Fix:** Hard precondition on the deferred apple-cross lane: the unprivileged
guest user must not be able to read the baked private SDK (separate mount,
stripped/permissioned before the build user runs), and that lane is permanently
no-egress. Add to acceptance for the deferred lane so it can't be quietly
enabled.

### H4 — "Destroys ... in all outcomes" overstates what is tested (OVERCLAIMED EVIDENCE)
Spec "What Phase 1 ships" L51.

"Destroys the VM, job disk, and input media in all outcomes, and confirms
teardown" is a guarantee. Teardown is demonstrated only on the happy path and
one timeout (companion L400-413, L219-223); controller-crash, host-wedge,
teardown-API-failure are untested (companion #2). L51 asserts as-shipped exactly
what acceptance L184 concedes is unproven.

**Fix:** Soften to "is designed to destroy ... in all outcomes; teardown is
proven only on pass and timeout — the full failure matrix is unverified (see
acceptance B4)."

### H5 — Attestation is a controller self-report, not tamper-evident, yet it gates publication
Spec L52, L190; companion attestation footer L268-271; companion guardrail #8
L485 ("signed or tamper-evident attestation").

Publication is gated on `teardown=confirmed` and `standing-secrets=none`, but
these are the controller *printing about itself*. The companion's own guardrail
calls for a tamper-evident attestation; the current evidence isn't one. A
controller bug (or a corrupted collection payload, cf. H1) could emit
`teardown=confirmed` without teardown, and the gate would pass.

**Fix:** The teardown/no-secret assertions must be tamper-evident (independent
Proxmox-side confirmation of VM+volume absence, not a controller print) before
they may gate publication. Add as acceptance criterion.

---

## MEDIUM

### M1 — Sanitization is asserted, never proven by an adversarial fixture corpus
Spec L138, L192-193. "Output is bounded and sanitized" / "no raw logs" have no
negative test. A malicious PR seeds ANSI/terminal escapes, a Discord
`@everyone`, markdown links, a 1 MiB overflow, or a forged provenance footer
(`teardown=confirmed image=sha256:...`) into test names, compiler output, or
branch names.
**Fix:** Acceptance criterion — a curated hostile-output corpus (escapes,
mentions, markdown, overflow, fake footer strings) proven stripped/neutralized
in every rendered surface (GitHub, Harbormaster).

### M2 — Quoted untrusted content in findings/notifications isn't provenance-fenced → forgeable "recommendation"
Spec L60-61, L152-154, L208. The read-only review quotes attacker-controlled
code/strings; if those reach the maintainer's Discord/GitHub without a
structural label separating Shipyard's assessment from contributor-supplied
text, an attacker can craft code comments that read as Shipyard's own conclusion
("Shipyard: safe to merge").
**Fix:** Every rendered surface fences and labels quoted contributor content as
untrusted, structurally distinct from Shipyard's own text.

### M3 — Shared-host thin-pool DoS to co-tenant VMs is unaddressed
Spec L124, L200-201; companion L294-297 (dmeventd auto-extend on shared
`pve/data`), L451-454 (nexus hosts unrelated VMs). Admission checks headroom
pre-clone, but during a run a malicious guest can fill its 80 GiB sparse disk,
driving thin-pool auto-extension and pressuring unrelated home-service VMs. The
operator-acceptance criterion covers hypervisor *escape*, not resource *DoS*.
**Fix:** Hard-cap the guest's thick/quota'd disk so a full guest disk cannot
consume shared thin-pool space; extend operator-risk criterion to cover
co-tenant resource exhaustion, or require a sterile dedicated host (companion
#7) before activation.

### M4 — Harbormaster event emission ordering vs teardown critical section not explicit
Spec L66, L205-209. Intent ("if unavailable, teardown continues normally") is
well stated, but the spec never mandates emission is fire-and-forget *after*
teardown, never inside the teardown finally-path. A blocking emit in the wrong
place could stall teardown.
**Fix:** Specify emission is asynchronous and strictly outside the teardown
critical section; make the proof (gestured at L209) a firm criterion. Phase-1
keeping Harbormaster static (companion #9) makes this low-urgency now, but it
must land before any live transport.

### M5 — Covert/timing channel via published facts — state the real mitigation
Spec L110-111, L114-119. An attacker controls pass/fail counts, timing, exit
codes, which named tests ran; some legitimately appears in the published result.
Bandwidth is low. The honest point: exfil is closed **not by network posture but
by secret-absence** — there is nothing in the guest to exfiltrate. True today,
false the moment the private macOS SDK is baked in (H3).
**Fix:** State plainly that network posture is defense-in-depth and
secret-absence is the actual exfil control; flag that H3 breaks this invariant.

---

## Sound as written
- Executor fail-closed (no local/SSH/self-hosted fallback), Rust policy module
  with negative tests required (spec L22-23, L178-179; companion L380-381, L478).
- Teardown via hypervisor API `finally` path, not guest-cooperative (spec
  L132-134) — correctly defeats in-guest SIGTERM/fork-bomb/mount-wedge; residual
  holes are collection (H2) and next-run leak (B2), not the destroy itself.
- Guest network posture (no DNS/route/LAN, nftables drops, forwarding disabled)
  genuinely closes FetchContent/DNS/socket exfil *from the guest* (spec L114-119;
  companion L289-320) — the real exfil surface is the trusted plane (B1) and
  future baked secrets (H3).
- Recipe as protected argv arrays with no comment-supplied shell string
  (companion L366-368).
- Non-goals and "Evidence collected so far" are mostly honestly hedged; the
  exceptions are H4 (L51) and H5 (L226).
