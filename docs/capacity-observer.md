# Capacity observation pass

`scripts/collect_fleet_capacity.py` is an optional, POSIX-only, one-shot observer.
It collects capacity snapshots without routing jobs or acquiring a database
lease during SSH. It invokes the existing `shipyard metrics import fleet-capacity`
validator once for the received batch. It does not install or schedule itself.

Keep an explicit roster outside the repository. Example (replace every value):

```json
{"schema":"shipyard.capacity-roster/v1","hosts":[
  {"ssh_alias":"builder-a","host_id":"builder-a","tartci":"/opt/tools/tartci",
   "repo":"example/project","target":"macos"}
]}
```

```sh
python3 scripts/collect_fleet_capacity.py \
  --roster /private/config/capacity-roster.json \
  --state-dir /private/state/shipyard --output-dir /private/state/capacity-observer
```

The roster is limited to 16 distinct hosts. Aliases are SSH configuration names;
executable paths must be absolute. The script shell-quotes remote arguments and
never supplies a replacement producer host ID. Receipt bytes pass unchanged to
Rust; Python only checks envelope completeness, expected identity, and freshness.
Exit 2 is accepted only with a complete unknown producer document; the Rust
importer must still validate it. SSH does not upgrade `untrusted_local` origin.

Defaults: four concurrent fetches, 20 seconds for the entire fetch phase,
55 seconds for the pass subprocess budget, 256 KiB stdout per producer, 16 KiB
stderr per child, five seconds allowed future skew. `--deadline-seconds` can
reduce the pass budget (maximum 55); `--fetch-seconds` must leave at least two
seconds for import/status work. Import lease contention consumes the remaining
budget. Timeouts signal only process groups created by this pass; termination
of a remote producer after SSH disconnect still requires deployment validation.
Local filesystem operations also require a responsive filesystem.

Every expected host appears in the compact attempt journal, including all-host
failure and overlap skips. Unknown, missing, and unconfirmed imports stay
distinct. Import timeout may occur after commit, so no rollback or automatic
retry is claimed. No pending receipt buffer or per-pass log files are retained.
Exit codes: 0 all hosts verified and importer acknowledged; 2 partial/unknown or
overlap; 1 collector/journal failure. A journal-lock failure is reported on stderr
and exits 1; it cannot promise a durable attempt entry.

`attempts.jsonl` rotates on each invocation to seven days and 1 MiB by default;
`--history-days` and `--history-bytes` change these bounds (up to 100 years and
16 MiB). Rotation drops oldest entries first. `last-status.json` is replaced
atomically after journal persistence; after interruption, the journal is the
historical authority and last status may lag. Both contain the roster digest,
collector source digest, collection policy, expected hosts, and receipt identity
metadata, not receipt payloads. Boot/generation changes are equality comparisons,
never lexical version ordering. Lower byte budgets may retain less than the
requested age; retained history alone cannot claim full-window reliability.

`--expected-interval-seconds` (default 60) records intended cadence without
creating a schedule. Missing invocations must be measured against that cadence
and roster; a journal records attempted passes, not proof the controller ran
continuously. Capacity remains a snapshot-derived estimate with the producer's
120-second validity, not continuously measured or authenticated capacity.

Before activation, an owner must verify the deployed producer/importer, private
roster, real timeout behavior, sample sizes and one-hour collection cost, then
approve scheduling, cleanup, and rollback. Fake-command tests do not establish
those integration or deployment gates.
