# Organic fleet baseline collection

`collect_baseline.py` is the scheduled observation wrapper for the fleet optimization baseline. It imports real GitHub job timing through Shipyard and real TartCI runtime exports. When the protected merge producer publishes a `shipyard-merge-receipt/v1` JSON/JSONL export, pass it explicitly with `--merge-receipts`; the wrapper imports it through Shipyard's transactional validator. It does not create metric rows itself, infer a merge outcome from a pull request, or turn an unavailable phase into zero.

Run it from a scheduler on the observing machine with a state file that is outside the repository:

```sh
python3 scripts/collect_baseline.py \
  --db "$SHIPYARD_STATE_DIR/metrics/metrics.db" \
  --project pulp --repo Generous-Corp/pulp \
  --state "$SHIPYARD_STATE_DIR/baseline-status.json" \
  --status-markdown "$SHIPYARD_STATE_DIR/baseline-status.md" \
  --tartci-host m5s \
  --merge-receipts "$SHIPYARD_STATE_DIR/merge-receipts.jsonl" \
  --merge-receipt-health "$SHIPYARD_STATE_DIR/merge-receipts.health.json" \
  --collect
```

`--tartci-host` reads the runtime export from the named fleet host over the
managed SSH aliases. Attempts without a GitHub external id are retained by
TartCI but excluded from Shipyard's joinable baseline with an explicit count;
they cannot contribute to safely merged throughput or wasted-compute metrics.

On the observation controller, install the supervised hourly schedule with
`scripts/install_baseline_collector.sh --install`. The installer is dry-run by
default, validates the collector, handoff checker, wrapper, and
`baseline_pass_history.py` dependency before loading launchd, and copies them
into the managed Shipyard state directory (`~/Library/Application
Support/shipyard`). The launchd job executes that managed copy, so removing or
replacing the checkout that performed the installation cannot silently break
future runs. Before each collection the wrapper checks the producer handoff;
when it is fresh and valid, receipt import is enabled. When it is missing,
empty, stale, or failed, timing and queue proxies still collect and an explicit
`merge-receipt: unavailable` event records the reason. The scorecard keeps the
safe-merge denominator unavailable and never turns it into zero.

Provider reads retry only recognized transport failures (by default, two
bounded attempts with 5s then 10s backoff). Authentication, schema, malformed
data, and handoff validation failures remain terminal and record a failed pass;
the retry cannot manufacture a successful observation or a safe-merge value.
Override the bounded policy explicitly with `--provider-retries` and
`--provider-retry-delay-seconds` when diagnosing a host.

The producer-side handoff is explicit and read-only. On the Shipyard state
directory that records protected merges, install the supervised exporter (dry-run
first):

```sh
scripts/install_merge_receipt_handoff.sh \
  --repo Generous-Corp/pulp \
  --state-dir "$PRODUCER_STATE_DIR" \
  --file "$SHIPYARD_HANDOFF_DIR/merge-receipts.jsonl" \
  --health-file "$SHIPYARD_HANDOFF_DIR/merge-receipts.health.json"
scripts/install_merge_receipt_handoff.sh \
  --repo Generous-Corp/pulp \
  --state-dir "$PRODUCER_STATE_DIR" \
  --file "$SHIPYARD_HANDOFF_DIR/merge-receipts.jsonl" \
  --health-file "$SHIPYARD_HANDOFF_DIR/merge-receipts.health.json" \
  --install
```

The managed job invokes the same explicit export command:

```sh
shipyard --state-dir "$PRODUCER_STATE_DIR" metrics export merge-receipt \
  --repo Generous-Corp/pulp \
  --file "$SHIPYARD_HANDOFF_DIR/merge-receipts.jsonl"
```

Its health sidecar is checked with `python3 scripts/merge_receipt_handoff.py
check ... --stale-after 2h`; a stale, malformed, failed, or empty handoff is
unavailable, never zero.

The observer then passes that file as `--merge-receipts`. The export reads only
validated receipts already persisted by Shipyard, scopes them to the exact
repository, and atomically replaces the handoff file when at least one receipt
exists. An empty export fails without replacing the previous handoff. It never
asks GitHub for merge state or creates a receipt.

The command writes an append-only observation history and reports continuous
elapsed 24-hour days, required activity strata, organic sample counts, and the
safe-merge denominator. The baseline cannot exit while normal PR, merge-group,
rerun, outage, or concurrent-agent activity is absent. Safe-merge throughput
remains unavailable without `--merge-receipts`; failed or cancelled jobs are
never treated as merged changes. A receipt import failure fails the collection
pass. Synthetic provenance is not asserted by this wrapper; use only the
organic provider exports intended for the baseline.
