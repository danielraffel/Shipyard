# Queue timing provenance contract

This contract defines how queue observations may acquire timing evidence. It
does not make queue scheduling depend on metrics storage.

## Identity and granularity

The queue `Job.id` is the only authoritative identity for a local queue job.
Metrics stores a nullable `jobs.queue_job_id` and a partial unique index over
non-null values. A terminal event may insert or refresh one metrics row for one
queue job; it must never create one row per validation target.

Rows written by the queue producer use a queue-owned provider identity and are
excluded from GitHub/TartCI pull-request aggregates. They are advisory timing
evidence, not a second provider observation.

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

## Aggregation semantics

`repeated_work_shadow.job_id` identifies the surviving job. A matching timing
row therefore measures **avoided survivor runtime**, not runtime that was
actually wasted by a superseded job. Aggregation must use one timing row per
queue job and must not multiply duration by the number of shadow rows or
validation targets.

Unmatched, excluded, and unknown-duration observations remain visible as
coverage counts. They are never converted to zero and never treated as
evidence of savings.

## Required validation

The implementation must prove migration of an old database, exact identity
round-trips, duplicate terminal-event deduplication, multi-target and
multi-shadow aggregation, never-started cancellation exclusion, uncertain
completion exclusion, best-effort failure behavior, and unchanged existing
pull-request aggregates.
