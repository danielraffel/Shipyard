# Organic fleet baseline collection

`collect_baseline.py` is the scheduled observation wrapper for the fleet optimization baseline. It imports real GitHub job timing through Shipyard, imports real TartCI runtime exports, and does not import merge receipts. It does not create metric rows itself, infer a merge outcome from a pull request, or turn an unavailable phase into zero.

Run it from a scheduler on the observing machine with a state file that is outside the repository:

```sh
python3 scripts/collect_baseline.py \
  --db "$SHIPYARD_STATE_DIR/metrics/metrics.db" \
  --project pulp --repo danielraffel/pulp \
  --state "$SHIPYARD_STATE_DIR/baseline-status.json" \
  --status-markdown "$SHIPYARD_STATE_DIR/baseline-status.md" \
  --collect
```

The command writes an append-only observation history and reports elapsed weekdays, required activity strata, organic sample counts, and the safe-merge denominator. The baseline cannot exit while normal PR, merge-group, rerun, outage, or concurrent-agent activity is absent. Safe-merge throughput remains unavailable in this collector; failed or cancelled jobs are never treated as merged changes. Synthetic provenance is not asserted by this wrapper; use only the organic provider exports intended for the baseline.
