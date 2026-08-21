# Log retention and rotation

This is the first bounded log-lifecycle increment. It is
deliberately lossless: it never uses `copytruncate`, and restart/reconnect paths
preserve the prior file before opening a replacement.

## Defaults and configuration

Put operational overrides in the machine-global Shipyard `config.toml` under
`[log_retention]`. Project and local overlays also affect `shipyard cleanup`;
daemon and queue-worker reopen rotation uses the trusted machine-global layer.

```toml
[log_retention]
success_days = 7
failure_days = 30
compress_after_hours = 1
max_active_file_bytes = 67108864 # 64 MiB
rotated_segments = 4
high_watermark_bytes = 1073741824 # 1 GiB
low_watermark_bytes = 805306368   # 768 MiB
```

Values are clamped to safe ranges. The low watermark cannot exceed the high
watermark.

## Cleanup receipts and pins

`shipyard cleanup` remains dry-run by default. Human and JSON output explain
each `delete` or `compress` action, every retained directory and its reason,
the observed log-tree bytes, projected bytes after planned deletions, and
explicit deleted, protected, pinned, and skipped byte totals. `skipped_bytes`
counts active-writer and audit-pinned directories that cleanup will not mutate;
`pinned_bytes` is the audit-pin subset of that total.
Use `shipyard cleanup --apply` to mutate.

Terminal jobs write `logs/<job-id>/.retention.json` before later queue trimming
can erase pass/failure classification. Failures, cancellations, and legacy
directories without a valid manifest receive the longer failure window and are
never pressure-deleted. Successful terminal logs are gzip-compressed after the
configured delay. Gzip retirement is two-pass: the first apply publishes and
verifies the gzip while retaining the source; a later apply re-verifies the
durable gzip and removes the source. `shipyard logs` reads the gzip transparently
after source retirement. If the log tree crosses the high watermark, oldest successful
terminal directories that are already compressed are selected until the low
watermark is reached. Compression happens first so Shipyard never deletes
evidence based on a guessed compression ratio; a subsequent cleanup receipt
uses the real compacted size.
Apply runs serialize with other cleanup runs, while each log mutation takes the
queue state lock only for its final disposition recheck and filesystem change.
Queue submission, cancellation, and completion are therefore not blocked by
the full directory scan or unrelated artifact cleanup; the critical section is
bounded to one candidate mutation.

Run `shipyard cleanup --pin <job-id>` to pin incident or audit evidence
indefinitely. This creates `logs/<job-id>/.shipyard-retain` while holding the
same lock as cleanup mutations, closing the check/write race. Do not create the
marker with a raw `touch` while cleanup may be running. Active queue writers and
pinned directories are never compressed or deleted. Interrupted directory
retirements are restored as a standalone, receipted transaction; run dry-run
again before any later deletion.

## Rotation boundary

Phase 1 rotates daemon and queue-worker logs when a new writer opens after the
size threshold, and preserves bounded prior segments. Target validation streams
also preserve a bounded prior file on restart instead of truncating it. This
prevents reconnects from destroying evidence and bounds repeated reopen history.
Merge-queue audit segments use the same configured bounds only while every
mutation correlation is resolved. Any unresolved/uncertain mutation pins the
audit history until explicit reconciliation, so rotation cannot erase the
authority record that blocks unsafe retries.

A single, continuously running target or daemon can still exceed the per-file
threshold before it reopens. Fully bounding those active writers is Phase 2: it
requires routing child stdout/stderr through a supervised rotating sink. Renaming
an open file would leave the child writing to the renamed inode and is not an
acceptable substitute.
