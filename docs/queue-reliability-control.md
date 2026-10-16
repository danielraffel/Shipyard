# Queue reliability control

This audit covers the queue implementation at
[`79c1399da1203ea94f68db4fb78fd53d7f6b2724`](https://github.com/danielraffel/Shipyard/commit/79c1399da1203ea94f68db4fb78fd53d7f6b2724).
It changes documentation only. It grants no host permission, enables no recovery
policy, and does not graduate a daemon execution or routing canary.

## Ownership baseline

The [P2b concurrency plan](../planning/queue-concurrency.md) describes the
foreground cooperative scheduler. It remains the authority for explicitly
foreground requests. Current Unix local submissions normally use the separate
daemon execution supervisor, already implemented in
[`src/execution_supervisor.rs`](../src/execution_supervisor.rs); this is not a
proposal to move the cooperative scheduler into the daemon.

An envelope's `execution_owner` selects that boundary. The daemon skips
foreground pending requests and preserves foreground running jobs without a
worker receipt. The cooperative scheduler preserves daemon-owned running jobs.
Keep these controls when changing either scheduler. A daemon being reachable
over IPC proves neither queue progress nor a live validation worker.

## Control and scenario battery

Run these in an isolated checkout with test-home isolation and bounded Cargo
parallelism. They use temporary fixture state and fake workers, never the live
queue. The stack setting matches CI. Keep the complete command output and the
tested commit in the evidence bundle; a test filter that runs zero tests is
not a receipt.

```sh
RUST_MIN_STACK=8388608 CARGO_BUILD_JOBS=2 cargo test --lib --locked \
  --features ci-test-home execution_supervisor::tests -- --test-threads=2
RUST_MIN_STACK=8388608 CARGO_BUILD_JOBS=2 cargo test --lib --locked \
  --features ci-test-home queue_absent_recovery::tests -- --test-threads=2
RUST_MIN_STACK=8388608 CARGO_BUILD_JOBS=2 cargo test --lib --locked \
  --features ci-test-home ship::tests::foreground_recovery_preserves_daemon_owned_running_jobs \
  -- --exact
RUST_MIN_STACK=8388608 CARGO_BUILD_JOBS=2 cargo test --test queue_concurrency \
  --locked --features ci-test-home -- --test-threads=2
```

| Scenario | Existing control and expected observation |
| --- | --- |
| Foreground baseline | `daemon_does_not_admit_or_cancel_foreground_pending_request` and `daemon_does_not_terminalize_foreground_running_without_worker_receipt`: the daemon leaves ownership with the foreground drain. The reciprocal `foreground_recovery_preserves_daemon_owned_running_jobs` preserves daemon ownership. |
| Cooperative takeover | `losing_submitter_waits_without_dispatching_targets` and `abandoned_drain_after_start_is_recovered_with_outcome`: a waiting submitter does not duplicate execution, and a later drain recovers abandoned work with an outcome. |
| Same-boot daemon restart | `daemon_restart_adopts_exact_live_worker_without_replay`: an exact PID/job/generation is adopted; no replacement child is spawned. `pid_reuse_without_exact_worker_identity_is_rejected` is its negative control. |
| Lost or ambiguous worker | `stale_running_without_worker_becomes_uncertain_and_durable` records `UNCERTAIN`; `unknown_worker_probe_preserves_running_and_blocks_admission` preserves ambiguous ownership and blocks a second worker. |
| Outcome repair | `daemon_restart_repairs_missing_terminal_outcome` reconstructs typed output from terminal queue state without rerunning validation. |
| Reboot replay disabled | `a_host_that_has_not_opted_in_never_requeues` proves the default-off control. No boot proof, a second reboot, or a cancellation request also prevents requeue. |
| Reboot replay opted in | `a_job_whose_worker_started_before_this_boot_is_requeued_exactly_once` and the intent/receipt-removal crash tests prove the write-ahead marker resumes one interrupted requeue. This fixture evidence does not enable the option on a host. |
| Queue-absence recovery disabled | `disabled_kill_switch_is_a_no_op` proves no recovery or GitHub read occurs when disabled. |
| Queue-absence recovery opted in | `exact_absent_work_is_reenqueued_once_and_duplicate_ticks_do_not_duplicate`, `head_drift_and_github_offline_fail_closed`, and `receipt_published_at_final_admission_blocks_recovery_enqueue` cover one replacement, exact-head refusal, and the final ownership race. |

These are local correctness controls. They establish no production restart
latency, queue-age improvement, fleet throughput, or physical host acceptance.
A live experiment needs separate, exact source/binary/configuration identities,
before/after queue witnesses, worker and process-group identities, observed
outcome, elapsed recovery time, and a same-workload foreground control. Do not
infer performance from fixture duration.

## Kill switches and rollback boundaries

| Scope | Existing control | Limitation |
| --- | --- | --- |
| New interactive submission | `--foreground` on `run`, `ship`, or `pr` | Selects cooperative execution for that submission; does not transfer an existing daemon job. |
| Reboot requeue | Machine-global `[queue.boot_requeue] enabled = false` (default) | Read on the supervisor's startup boot pass. It prevents the optional replay path; it cannot undo an already completed requeue. |
| Queue-absence recovery | Machine-global `[ship_state] queue_absent_recovery = false` (default) | Prevents subsequent recovery sweeps. It does not revoke a replacement already committed or an in-flight sweep holding its loaded configuration. |
| Orphan abandonment | `[ship_state] auto_resume = false` (default) | Disables abandonment; it is independent of queue-absence replacement and is not a general queue replay switch. |
| Daemon admission | `shipyard daemon stop` | Stops supervisor ticks. Existing independently owned workers continue; this is not worker cancellation. On a launcher host, the clean IPC stop exits successfully and stays down. |
| One existing daemon worker | `shipyard cancel <job-id>` | Requests termination through worker custody. Keep its slot reserved until process-tree death is proved. |

Changing a switch is an operator action outside this audit. To stop new daemon
admission immediately, stop the daemon; changing a periodic recovery setting
alone does not fence a sweep already running. Do not rewrite envelope ownership,
delete receipts or locks, edit `queue.json`, or treat a daemon restart as proof
that independent workers died. Unreadable state remains a blocker.

## Promotion gate

Keep foreground cooperative drain authoritative for foreground envelopes.
Any later ownership expansion needs an explicitly authorized isolated canary,
the control and failure scenarios above, a durable restart/orphan outcome
receipt, and a tested rollback that preserves outstanding worker custody.
There is no new promotion flag in this documentation change.
