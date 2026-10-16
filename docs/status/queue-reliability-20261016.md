# Queue reliability audit status — 2026-10-16

Status: local scenario battery verified; documentation only; no daemon promotion.

Tested head: [`ecda199137d75f8fb79a12157f680fcc32fc6dec`](https://github.com/danielraffel/Shipyard/commit/ecda199137d75f8fb79a12157f680fcc32fc6dec).
It includes main baseline
[`ecec8359223d981751e51a943371471b441a6fe0`](https://github.com/danielraffel/Shipyard/commit/ecec8359223d981751e51a943371471b441a6fe0),
including the telemetry change at
[`dea6c79353a3769a1e1ecbeb14e99cd4ba75759f`](https://github.com/danielraffel/Shipyard/commit/dea6c79353a3769a1e1ecbeb14e99cd4ba75759f).
This status file is a subsequent documentation-only receipt; no tested code was changed.

The [control guide](../queue-reliability-control.md) records exact commands,
scenario names, foreground ownership, and kill-switch limitations. The P2b
plan is explicitly historical because current Unix local submissions already
have a separate daemon-owned execution supervisor.

| Exact-head local validation | Result |
| --- | --- |
| `execution_supervisor::tests` | 51 passed, 0 failed |
| `queue_absent_recovery::tests` | 35 passed, 0 failed |
| `ship::tests::foreground_recovery_preserves_daemon_owned_running_jobs` | 1 passed, 0 failed |
| `queue_concurrency` integration suite | 7 passed, 0 failed |

Document links and all 16 named scenario tests were checked against the source;
`git diff --check` passed. The architecture and adversarial review remained
inline and corrected the fallback guidance: only `run` and `ship` expose
`--foreground`; `pr` does not.

No runtime code, fleet config, live queue state, installed binary, launcher, or
recovery toggle changed. The foreground cooperative drain remains authoritative
for foreground envelopes. Default-off recovery controls retain their existing
values. Production restart latency, queue-age improvement, and physical host
acceptance were not measured; local fixture success cannot graduate them.

Next action: review the documentation PR without arming auto-merge or enabling
any daemon ownership expansion. A later live canary requires its own authorization,
exact workload/control receipt, and rollback preserving outstanding worker custody.
