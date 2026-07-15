# Fable independent delta review — containment slice (code + evidence)

Verdict: **disposition-CONFIRMED-with-caveats → SAFE-TO-ACTIVATE-PENDING-X.**
B1–B4 and H1–H5 are genuinely closed IN CODE (not just spec prose); the old
H4/H5 overclaims are now honestly hedged and independently backed. Two gates
before the trigger may be enabled. Lane is still disabled, so nothing here is a
live breach.

## X1 (BLOCKER before enabling) — poller auth gate FAILS OPEN
`comment-poller.py:232-240`. With `login=None`/`user_id=None` (user missing, or a
user dict lacking `id`), `policy["authorized_users"].get(login)` returns `None`
and `None != None` is False — the guard does NOT fire and the comment falls
through to admission. GitHub always populates `user.id`, so not exploitable via
the real API today, but the gate is not fail-closed — the one property this lane
must guarantee.
FIX: `isinstance(user_id, int) and login in authorized_users and authorized_users[login] == user_id`.

## X2 (correct before activation) — three checkmarks overclaim
- **L247** "a fresh real-PR run proves ... no route or DNS, bounded resources":
  the DNS/route/host/LAN negative PROBES ran in the offline-smoke run, not the
  real-PR run. Conflates two runs. Reword.
- **L258** "the permanent controller operates unattended from restricted files":
  only a one-shot poll ran; the unattended TIMER is disabled and open-risk #1
  says the unattended path still needs proof. File-perms half is real; "operates
  unattended" half is not demonstrated. Reword.
- **L266** "contributor-controlled excerpts are visibly fenced": the automated
  publisher quotes NO contributor excerpt at all — neutralization is by omission
  (stronger), so "excerpts visibly fenced" is vacuously satisfied. The fencing
  requirement really applies to human/agent review comments, which this code
  doesn't emit. Reword to claim omission, not fencing.
- **B4/L262 wording:** admission binds the manifest DESCRIPTOR + template self-
  description; it does NOT re-hash live LVM disk blocks (Proxmox limitation the
  design accepts). Soften "complete hardened-image identity" to "descriptor
  pinned; live-disk content not re-hashed at admission."

## Confirmed closed in code
- B1 exact-SHA: `comment-poller.py:143 gh_archive` tarball-by-SHA via ghapp, full-match REPO_RE/SHA_RE; `build_iso` copies verbatim, NEVER extracts on trusted side → no submodule/LFS/FetchContent/tar-traversal. Baked deps matched offline (10 sources).
- B2/B3 `acquire_admission_lock` real flock; `assert_job_slot_clean` independent Proxmox query (fixed VMID, every guest's vmbr1, both storages); `assert_admission_unlatched` durable latch; reconcile adopts only identity-matched VM. (Minor: bridge check continues past STOPPED foreign guests at other VMIDs — harmless at concurrency-1, note only.)
- B4 clean-init hard gate (rejects degraded/DEPRECATED-user); image manifest+inventory digests in protected template desc.
- H1 `validate_guest_result` strict schema + caps; 256KiB read cap; 250-case fuzz all fail Blocked-only.
- H2 independent socket deadline; collection inside try, teardown in finally.
- H3 VM131 referenced nowhere; controller pins template 127 only; stopped/no-NIC/no-signing → no early exposure.
- H4 teardown in finally; latch on TeardownBlocked; matrix pass/build-fail/timeout/SIGTERM proven.
- H5 teardown ends with independent assert_job_slot_clean re-query; teardown=confirmed only after clean run; publish refuses non-confirmed. The key improvement over the spec review — the gate is Proxmox-side, not a self-print.

## New holes the code introduced
- X1 (above) — the one that matters.
- Poller processes only first 100 comments (no pagination) — an authorized trigger past 100 is silently missed. Availability quirk, note.
- Result channel/qemu-ga parsing — sound. TOCTOU preflight→clone covered by flock. Co-tenant thin-pool DoS (old M3) still honestly unchecked (L276).

## Test totals verified here
30/30 Python suite passes; 1052 Rust unit #[test] + 5 integration — match claimed.
