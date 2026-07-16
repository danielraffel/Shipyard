# Fable independent review — activation slice

Verdict: **READY-PENDING-X.** X = (1) make the production poller's service-stop
teardown symmetric with the proven smoke path (3-line fix), and (2) the owner's
explicit shared-host acceptance (the known, honestly-stated gate).

**No blockers. No LAN/tailnet/Internet escape and no guest→controller
publication-influence path found.** 33/33 policy tests verified locally.

## HIGH — activation (timer/poller) path has no SIGTERM→teardown handler
`interrupt_for_teardown` is installed ONLY in `review-control.py:842` (the `smoke`
subcommand). The production path (poll.timer → comment-poller.py main/poll_once/
process_comment → lifecycle.run()) installs no signal handler, and
`shipyard-review-poll.service` is Type=oneshot with no ExecStop/KillSignal. A live
review runs minutes (288s on PR6114) inside that oneshot; a `systemctl stop`/reboot
mid-job kills Python WITHOUT running run()'s finally teardown (:671-676) →
orphaned VM200 + ISO + disk. It is FAIL-CLOSED (next admission blocks via
assert_job_slot_clean :473; sqlite keeps trigger running 3h) so NOT an escape —
but it wedges the lane until manual reconcile, and it is NOT the graceful "SIGTERM
removed all fixed resources" the evidence advertises.
FIX: install signal.signal(SIGTERM, interrupt_for_teardown) around poll_once in
comment-poller.py; add a test that stops mid-run and asserts teardown.

## MEDIUM — no-NIC template VM132 is manifest-pinned, not code-reproducible
Admission + assert_template REJECT a NIC'd template (fail-closed, good) and the v10
JSON pins every digest, but nothing in the diff BUILDS VM132 no-NIC — creation is a
manual live op. The ACL/SDN-removed state IS code-reproducible; the template
artifact is not. The "no further mechanism proof required" framing should
acknowledge template rebuild remains a manual, digest-attested step.

## MEDIUM/LOW — idempotency marker dedup scans only first 100 comments
publish_result (comment-poller.py:133-137) fetches per_page=100, no pagination. A
double-publish needs BOTH sqlite-state loss AND the marker pushed past 100 newer
comments. Primary guard (sqlite terminal-status early return :221) is solid +
tested. Bounded fix (query by marker / paginate); not a blocker.

## LOW
- Stale unused `ShipyardReviewBridge` role lingers (ACL revoked, role never
  deleted) — harmless, not reproducible-clean.
- `ShipyardReviewJob` now includes `VM.Config.Network` (scoped to /pool/$job_pool;
  controller only deletes NICs; admission rejects added NICs) — acceptable, but
  widens theoretical capability.
- `job_bridge=vmbr1` still a required key though jobs no longer attach a bridge
  (now used only for foreign-guest detection) — vestigial-but-repurposed.

## Overclaims to correct
- "graceful SIGTERM ... removed all fixed resources" (README/evidence) — MILD
  OVERCLAIM BY CONFLATION: proven only for the smoke CLI, not the activation poller
  (see HIGH). Both fail-closed, but the graceful claim does not transfer.
- "No further mechanism proof is required" (evidence:109) — slightly overclaims
  given the poller-stop teardown asymmetry is unproven.
- Shared-host residual risk — NOT an overclaim; stated honestly. Visibility
  transport — CONFIRMED absent, correctly out of scope.

## Genuinely sound
No-NIC defense-in-depth (template + live-clone admission + net0 delete + trusted
in-guest probe before untrusted code); guest→controller boundary holds
(publication renders only controller-trusted fields); publication ordering correct
(run() returns only if teardown succeeded; publish refuses unless
teardown==confirmed); two-layer idempotency; SDN/ACL removal reproducible + a
least-privilege reduction; reconcile_stranded_job adopts only provably-owned
resources; shared-host escape risk stated honestly as the operator accept/migrate
gate.
