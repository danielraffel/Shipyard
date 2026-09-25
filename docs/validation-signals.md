# Validation signals: test tier and receipt decisions

A green required check says the job succeeded. It does not say how much the job
tested. Two common CI shapes make that gap dangerous:

- **Tiered testing.** Pull-request heads run a narrowed, fast test tier and the
  full suite runs only in the merge queue. A green head is then "green on the
  fast tier", not "fully tested".
- **Receipt reuse.** A merge group may skip re-running the suite when an
  earlier run's receipt still covers it. "The merge group was green" then says
  nothing about whether tests ran in it.

Shipyard reads a small, generic annotation contract so it can say which of
those happened. Any repository can emit it; nothing in Shipyard depends on a
repository's job names.

## The contract

A job prints a GitHub Actions workflow-command notice. GitHub turns it into a
check-run annotation (`GET repos/{o}/{r}/check-runs/{id}/annotations`). The
annotation **title** names the signal; the **message** is one compact JSON
object.

### `shipyard-test-tier`

Emitted by the test step of a required check's job.

```sh
echo '::notice title=shipyard-test-tier::{"schema":"shipyard-test-tier/v1","tier":"fast","selector":"pr-fast","full_suite_runs_in":"merge_group"}'
echo '::notice title=shipyard-test-tier::{"schema":"shipyard-test-tier/v1","tier":"full"}'
```

| field | meaning |
|---|---|
| `schema` | must be `shipyard-test-tier/v1` |
| `tier` | `full` when the whole suite ran; anything else is a narrowed tier and is displayed verbatim (unknown future values are never rejected) |
| `selector` | optional: the label/selector the narrowed tier ran |
| `full_suite_runs_in` | optional: where the full suite runs instead (for example `merge_group`) |

If a job emits more than one, the **last** one wins.

### `shipyard-receipt-decision`

Emitted by whatever job decides whether a merge group may reuse an earlier
run's receipt, one annotation per target considered. Shipyard scans every
check run of the merge group's `merge_group` workflow runs for this title; the
job name does not matter.

```sh
echo '::notice title=shipyard-receipt-decision::{"schema":"shipyard-receipt-decision/v1","target":"macos","verdict":"reuse","reason":"inputs unchanged","source_run_id":"123","selected":812,"passed":800,"skipped":12,"inventory_count":2400}'
echo '::notice title=shipyard-receipt-decision::{"schema":"shipyard-receipt-decision/v1","target":"macos","verdict":"refuse","reason":"the receipt ran a narrowed tier","source_run_id":null,"selected":null,"passed":null,"skipped":null,"inventory_count":null}'
```

| field | meaning |
|---|---|
| `schema` | must be `shipyard-receipt-decision/v1` |
| `target` | the lane/target the decision is about |
| `verdict` | `reuse` or `refuse`; other values are shown verbatim |
| `reason` | why |
| `source_run_id` | the run whose receipt was reused (string or number), or `null` |
| `selected` / `passed` / `skipped` / `inventory_count` | test counts from the reused receipt, or `null` |

Rendered as:

```
macos: reused receipt from run 123: 812 selected / 800 passed (12 skipped)
macos: validated in full: receipt refused because the receipt ran a narrowed tier
```

## Reading rules

- **No tier annotation means the tier is `unknown`.** Shipyard never infers
  `full` from absence, and says so.
- A malformed message, a missing required field, or a different `schema` is
  reported as **unparseable** with the raw text. It is never dropped silently
  and never crashes the command.
- An unreadable annotation endpoint (403, 404, transport error) is reported as
  `unknown` with the reason.
- The head's verdict collapses to the **weakest** reported tier: one required
  check on `fast` makes the head "green on the fast tier, NOT full validation"
  even if another reported `full`.

## Where Shipyard shows them

| command | what it adds |
|---|---|
| `shipyard landing --pr <n>` | a `VALIDATION` block: what the head's green means (fast tier / fully tested / tier unknown), per required check, plus the receipt decisions and tiers of the PR's merge group (its live queue entry, or its merge commit once merged). JSON: `validation.test_tier[]`, `validation.test_tier_verdict`, `validation.merge_groups[].receipt_decisions[]`, `validation.merge_groups[].test_tier[]`, `validation.api_calls`. |
| `shipyard wait pr <n> --state green` | the same `VALIDATION` block after the match line, so a green wait cannot be read as full validation. JSON: a `validation` object in the envelope. Snapshot-file replays stay offline and omit it. |
| `shipyard queue-observe` | each queue entry lists its `receipt decision:` and `test tier:` lines. JSON/state: `queue[].receipt_decisions[]`, `queue[].test_tier[]`, omitted when empty so repositories that do not emit the contract keep their state hash. |

## API cost

- `landing --pr` / `wait pr`: one GraphQL read for the head's required contexts
  (`isRequired(pullRequestNumber:)`), then one annotations read per required
  check run (bounded at 20). Per merge group: one `actions/runs?event=merge_group`
  read, up to three `commits/{sha}/check-runs` pages, and one annotations read
  per merge-group check run that GitHub reports as annotated (bounded at 30).
  The report prints the exact count it spent (`api_calls`).
- `queue-observe`: no extra calls; the existing single GraphQL snapshot asks
  for `annotations(first:20)` on merge-group check runs only.

Use check-run ids with `check-runs/{id}/...` endpoints. `actions/jobs/{id}`
handed a check-run id can return a different, wrong job without an error.
