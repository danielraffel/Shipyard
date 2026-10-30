# Queue timing provenance contract

This contract defines how queue observations may acquire timing evidence. It
does not make queue scheduling depend on metrics storage.

## Identity and granularity

The queue `Job.id` is the only authoritative identity for a local queue job.
Metrics stores one authoritative `queue_timing` row keyed by `queue_job_id`. A terminal event may insert or refresh one row for one queue job; it must never create one row per validation target.

Rows written by the queue producer use a queue-owned provider identity and are
excluded from GitHub/TartCI pull-request aggregates. They are advisory timing
evidence, not a second provider observation.

That provider identity is reserved for the in-process terminal producer.
`metrics record` and import paths must reject attempts to supply it or a
`queue_job_id`; callers cannot authenticate queue provenance by choosing a
string.

## Producer boundary

The producer runs after a queue terminal state has been durably committed. It
covers ordinary completion, authenticated receiptless cancellation, and
uncertain orphan completion. A repeated terminal callback refreshes the same
identity. Metrics write failure never prevents terminalization.

Only jobs with an authoritative `started_at` and `completed_at` produce a
duration. Never-started cancellations produce no timing row. `UNCERTAIN`
completion is retained as excluded or flagged evidence and is never used as a
duration. Requeued jobs measure the final attempt only; this limitation is
explicit in reports.

Mid-run cancellations are also excluded or flagged. A later conservative
terminal state wins over an earlier clean callback, and an older callback may
never restore a duration after an uncertain or cancelled state. Refreshes must
validate immutable queue identity (creation time, SHA, branch, targets, and
mode), not only the short queue ID.

## Aggregation semantics

`repeated_work_shadow.job_id` identifies the surviving job. A matching timing
row therefore measures **avoided survivor runtime**, not runtime that was
actually wasted by a superseded job. Aggregation must use one timing row per
queue job and must not multiply duration by the number of shadow rows or
validation targets.

Unmatched, excluded, and unknown-duration observations remain visible as
coverage counts. They are never converted to zero and never treated as
evidence of savings.

If several shadow rows identify one survivor, counting that survivor once is an
intentional conservative undercount. In a supersession chain, unmatched rows
for jobs that never ran are expected and remain part of coverage reporting.

The `metrics repeated-work-cost` readout exposes this accounting as
`duration_ms` (avoided survivor runtime), together with the distinct-survivor and
excluded/unmatched coverage counts. These fields are observational evidence;
they do not authorize skipping, cancellation, routing, or merge decisions.

## Required validation

The implementation must prove migration of an old database, exact identity
round-trips, duplicate terminal-event deduplication, multi-target and
multi-shadow aggregation, never-started cancellation exclusion, uncertain
completion exclusion, best-effort failure behavior, and unchanged existing
pull-request aggregates.

Validation must also prove that ordinary record/import APIs cannot forge queue
provenance, that immutable identity mismatches are rejected, that mid-run
cancellation is excluded, and that callback ordering cannot turn an uncertain
or cancelled result back into a clean duration.
