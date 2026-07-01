# CI Runtime Measurement Plan

Date: 2026-06-12

Status update, 2026-06-13:

- Phase 1 has a first implementation on branch `codex/macos-vm-pool-phase5`:
  `shipyard metrics record/list/summary/slowest/trend/compare/watch/advise`
  backed by `metrics/metrics.db`, plus automatic best-effort metrics capture
  from `shipyard run command` command-evidence rows.
- Phase 2 has a first implementation: `shipyard metrics import github` imports
  recent GitHub Actions run jobs via `gh api` and stores them with GitHub
  `run_id/job_id/attempt` external IDs.
- Phase 3 has a first implementation: `watch`, `advise`, and `compare` emit
  JSON findings with conservative insufficient-sample behavior and no config
  mutation.
- Phase 4 is partially connected: profile policy remains explicit in
  `shipyard ci profile ...`; metrics-backed recommendations are queryable, but
  profile docs do not yet embed live recommendations.
- tartci integration is optional and pull-based: `tartci runtime export` can
  pipe into `shipyard metrics import tartci`. Shipyard does not require tartci.

Validation so far:

- `cargo test metrics:: --locked`
- `cargo test command_evidence --locked`
- isolated CLI smoke for `metrics record`, `metrics import tartci`,
  `metrics summary`, `metrics watch`, `metrics advise`
- isolated CLI smoke proving `shipyard run command` writes a metrics summary row
- merged PR #361: all GitHub checks green
- merged-code proof: imported 2 real backfilled tartci timing records and 6 live
  Pulp GitHub Actions job rows into one isolated `metrics.db`; `summary` and
  `watch` returned agent-readable JSON

Still required before calling the whole project complete:

- Run a live measured tartci VM job with `TARTCI_RUNTIME_MEASURE=1`; current
  cross-repo proof uses real historical `timing.tsv` backfill plus live GitHub
  job import, not a newly emitted VM runtime record.
- Collect enough samples from `macstudio` and `m5` for the Phase 1 acceptance
  question to be meaningful.
- Decide whether profile documentation should reference static example metrics
  queries only, or add a live recommendation command to `shipyard ci profile`.

## Goal

Service agents with enough historical runner data to validate whether our
runners are performing at a high bar consistently. Per repo, the measurement
system should show whether local hardware (`macstudio`, `m5`, Tart VMs) and
GitHub-hosted runners are fast, healthy, and reliable enough for the lanes they
are assigned.

Build duration matters, but it is raw material rather than the product. The
customer is an agent deciding whether CI/local runners are healthy, whether a
lane needs closer monitoring, and whether a change is worth investigating. This
should stay small: enough history and basic stats to guide those agent decisions
without committing Shipyard to a metrics platform. If an existing tool already
solves the storage/reporting layer cleanly, integrate with it instead of
rebuilding it.

The near-term output is an agent-readable runner performance service. It should
make it obvious when a runner, VM image, cache, or route is degrading, improving,
or failing to justify its place in a repo's profile. Longer term, the same data
can inform whether it is worth owning more of the stack, such as a private Git
server with GitHub as a backup push target, but that is an evaluation input, not
Phase 1 scope.

Keep the bar intentionally modest. We need enough data to decide whether to
tweak a repo's profiles, caches, VM sizing, or fallback timing. We do not need a
general observability system.

The primary consumer should be an agent. Humans should be able to inspect the
same data occasionally, but the default workflow is:

1. Agent imports or records recent runs.
2. Agent asks Shipyard for material changes and optimization opportunities.
3. Shipyard returns structured JSON with evidence, thresholds, and suggested
   next actions.
4. Agent files an issue, updates a plan, or recommends a config change only when
   the signal is strong enough.

## Ownership And Integration Boundary

This should be optional infrastructure, not a hard dependency on tartci.

Shipyard is the natural place for the normalized store and the agent-facing
query commands because it already coordinates local, SSH, host-pool, and cloud
targets. tartci can integrate by emitting VM-specific timing and metadata when a
lane runs inside Tart, but projects that only use GitHub-hosted runners, plain
SSH targets, local commands, or another VM manager should still be able to record
and query metrics.

The contract should be:

- Shipyard owns `metrics.db`, imports, summaries, drift detection, and stable
  JSON output for agents.
- tartci optionally emits timing events such as VM boot, readiness, setup,
  cache-restore, cache-save, and shutdown.
- Repos optionally annotate runs with profile/lane/tweak labels so agents can
  evaluate per-repo decisions.
- External tools such as Hyperfine or Bencher can import/export through JSON, but
  are not required.

The main service this provides to agents is historical context: enough structured
data to validate whether runners are consistently performing at a high bar,
communicate trends, and spot regressions or optimization opportunities that are
hard to see from one CI run.

High-bar signals should include:

- low and stable queue time for lanes expected to run locally.
- low and stable boot/readiness time for VM lanes.
- stable p50 and p90 total duration per repo/lane/runner.
- acceptable failure rate after separating source failures from runner failures.
- cache behavior that matches expectations after a cache optimization.
- enough samples to avoid overreacting to one slow or flaky run.

Agents should use these historical baselines to choose monitoring behavior:

- poll less aggressively when lanes are within normal historical bounds.
- poll more frequently when queue time, boot time, run time, or failure rate
  starts drifting.
- escalate only when a change is material relative to that repo/lane's baseline.
- distinguish "still within normal variance" from "worth investigating" without
  requiring a human to remember prior timings.
- identify when a recent optimization is still settling versus clearly helping
  or hurting.

## Prior Art To Check First

- GitHub Actions usage metrics can show workflow and job consumption, but they
  are GitHub-side and do not compare local Tart/SSH targets with GitHub-hosted
  runners.
- Prometheus exporters such as `webdevops/github-workflow-exporter`,
  `Labbs/github-actions-exporter`, and `gravitational/gha-exporter` can collect
  GitHub workflow/job timing, but they are usually polling/exporter stacks rather
  than a lightweight per-project local decision log.
- CICDash stores GitHub Actions history beyond GitHub retention and may be useful
  as a reference if we later want a dashboard, but it is still GitHub-focused.

Conclusion for Phase 1: use Shipyard's existing command evidence and run outcome
records as inputs, but store normalized metrics in a small SQLite database first.
Keep JSON import/export so CI artifacts and third-party benchmark tools can feed
the same store without requiring a service.

## Data Model

Use SQLite as the primary store in Shipyard state, with append/import/export
commands. JSONL remains useful as a wire format and artifact format, but the
interactive product is querying trends; SQL should be the default rather than a
later migration.

Minimum tables:

```sql
CREATE TABLE machines (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  os TEXT,
  arch TEXT,
  cpu_count INTEGER,
  ram_mb INTEGER,
  labels_json TEXT,
  UNIQUE(name, kind, os, arch)
);

CREATE TABLE runs (
  id INTEGER PRIMARY KEY,
  ts TEXT NOT NULL,
  project TEXT NOT NULL,
  repo TEXT,
  branch TEXT,
  sha TEXT,
  pr INTEGER,
  workflow TEXT,
  profile TEXT,
  routing_decision TEXT,
  status TEXT NOT NULL
);

CREATE TABLE jobs (
  id INTEGER PRIMARY KEY,
  run_id INTEGER NOT NULL REFERENCES runs(id),
  machine_id INTEGER REFERENCES machines(id),
  job TEXT NOT NULL,
  target TEXT,
  platform TEXT,
  backend TEXT,
  provider TEXT,
  queued_at TEXT,
  started_at TEXT,
  completed_at TEXT,
  queue_ms INTEGER,
  boot_ms INTEGER,
  setup_ms INTEGER,
  run_ms INTEGER,
  total_ms INTEGER,
  status TEXT NOT NULL,
  exit_code INTEGER,
  failure_class TEXT,
  external_id TEXT,
  UNIQUE(provider, external_id)
);

CREATE TABLE steps (
  id INTEGER PRIMARY KEY,
  job_id INTEGER NOT NULL REFERENCES jobs(id),
  step TEXT NOT NULL,
  started_at TEXT,
  completed_at TEXT,
  duration_ms INTEGER NOT NULL,
  status TEXT NOT NULL,
  cache_key TEXT,
  cache_hit INTEGER,
  artifact_path TEXT
);
```

The CLI should also accept a single step-style record for very low-friction
instrumentation:

```sh
shipyard metrics record \
  --runner macstudio \
  --workflow build \
  --job linux-arm64 \
  --step compile \
  --duration-ms 18423
```

Normalize that into `runs/jobs/steps` rather than forcing every caller to know
the full schema.

Record these fields when available:

- `schema_version`
- `project`, `repo`, `branch`, `sha`, `pr`
- `workflow`, `job`, `target`, `platform`, `backend`, `provider`
- `host` (`macstudio`, `m5`, `github-hosted`, VM label when available)
- `profile` and routing decision (`primary`, `fallback`, `forced`)
- timestamps: `queued_at`, `started_at`, `completed_at`
- durations: `queue_secs`, `boot_secs`, `setup_secs`, `run_secs`, `total_secs`
- cache fields: `cache_mode`, `cache_hit`, `cache_key`, `cache_restore_secs`,
  `cache_save_secs`
- outcome: `status`, `exit_code`, `failure_class`, `timed_out`
- resource hints when cheap: CPU count, RAM cap, Tart VM name/tag, runner labels

Do not store secrets, full logs, or unbounded command output in this metrics log.
Link to existing evidence/log paths instead.

## Phase 1: Local Timing Baseline

Build the tiny Shipyard metrics subsystem before doing routing advice:

- Add `shipyard metrics record` for explicit step/job timing writes.
- Teach `shipyard run command` / local target evidence to emit normalized rows
  into `metrics.db`.
- Include backend, target, host, command name, duration, exit status, and artifact
  cache annotations.
- Add `shipyard metrics list --project <name>`, `shipyard metrics summary`,
  `shipyard metrics compare`, `shipyard metrics slowest`, and
  `shipyard metrics trend`.
- Summary should expose p50/p90/min/max/count/failure-rate grouped by
  `project,target,backend,host`.
- Keep output available as human table and JSON so agents, plugins, and CI can
  parse the same truth.

Acceptance:

- Running the same local command on `macstudio` and `m5` creates comparable rows.
- Summary can answer: "For Pulp `linux-arm64`, which host is faster over the last
  N successful runs?"
- A basic SQL query can answer average compile time by runner without bespoke
  report code.

## Phase 2: GitHub Import

Import GitHub Actions job timing into the same shape:

- Use `gh api` to fetch workflow runs and jobs for a repo/ref.
- Capture GitHub queue time, run time, runner name/labels when exposed, conclusion,
  and workflow/job names.
- Store imported rows with `backend=cloud`, `provider=github-hosted` or
  `provider=self-hosted` when labels make that clear.
- Deduplicate by GitHub `run_id/job_id/attempt`.

Acceptance:

- `shipyard metrics import github --repo danielraffel/pulp --workflow build.yml`
  imports recent Pulp jobs without touching routing config.
- Summary compares GitHub-hosted Windows/Linux against local Tart VM attempts.

## Phase 3: Routing Advice

Use observed metrics to inform but not automatically rewrite routing profiles:

- Add `shipyard metrics advise --project pulp --profile normal`.
- Add `shipyard metrics watch --project pulp --json` for agent-oriented drift
  detection.
- Recommend preferred location per lane when enough samples exist.
- Include confidence guardrails: minimum sample count, recent failure rate, stale
  data age, resource capacity, and queue/backlog signals.
- Compare before/after windows for a named tweak, for example `windows-sccache`,
  `linux-vm-cpu12`, or `macstudio-primary`.
- Mark findings by materiality:
  - `info`: visible trend, no action.
  - `watch`: continue monitoring; sample size or magnitude is borderline.
  - `investigate`: likely regression, broken cache, queue issue, or runner drift.
  - `optimize`: clear opportunity where a different target/profile/cache appears
    materially better.
- Emit explicit fallback reasoning such as:
  - `macstudio` primary: fastest p50 and healthy success rate.
  - `m5` fallback: slower but available when Mac Studio has no free slots.
  - GitHub fallback: slower but authoritative for Intel coverage or when local
    fleet is offline.

Acceptance:

- Advice is explainable from the metrics rows and never changes config unless a
  later explicit `--apply` mode is designed.
- A repo owner can answer: "Did this optimization make Pulp PR validation
  materially faster, or should we revert/retune it?"
- An agent can answer: "Is anything materially worse or better this week, and
  what should be investigated?"

## Phase 4: Project Profiles

Feed the same measurements into the CI profile work:

- Allow profile docs to reference metrics-backed recommendations.
- Keep repo-specific policy explicit: Pulp may use local ARM macOS/Linux/Windows
  by default, while smaller repos may stay GitHub-first.
- Support scheduled Intel checks as separate lanes rather than treating them as
  fallbacks for ARM local validation.

Acceptance:

- A profile can express "local ARM fast path, GitHub Intel nightly" and the
  metrics summary can show whether that remains rational.

## Reporting Views

Minimum useful views for humans and agents:

- Last 20 runs for one lane.
- p50/p90 successful duration by target/backend/host over 7/14/30 days.
- queue time vs run time split for GitHub-hosted and self-hosted jobs.
- boot/setup/run split for Tart VM lanes.
- failure rate by lane and host.
- before/after comparison for a repo-specific optimization label.
- "Should this lane run locally?" advisory summary.

Human-readable tables are convenience output. JSON findings, confidence, and
suggested next actions are the product surface.

## Agent Contract

Agent-facing commands should be stable and terse:

```sh
shipyard metrics watch --project pulp --since 14d --json
shipyard metrics advise --project pulp --profile normal --json
shipyard metrics compare --project pulp --lane windows-arm64 --before 7d --after 7d --json
```

`metrics watch --json` should return:

- `project`
- `window`
- `summary`
- `findings[]`
- `confidence`
- `suggested_poll_interval_secs`
- `recommended_actions[]`
- `evidence[]` with query parameters, sample counts, and representative run ids

Example finding shape:

```json
{
  "severity": "investigate",
  "lane": "windows-arm64",
  "signal": "p90_total_ms_regression",
  "message": "Windows ARM64 p90 increased 42% after the latest golden image tag.",
  "baseline": {"window": "previous_14d", "samples": 12, "p90_ms": 1840000},
  "current": {"window": "last_14d", "samples": 10, "p90_ms": 2610000},
  "suggested_poll_interval_secs": 300,
  "recommended_actions": [
    "Check tartci boot/setup timing split.",
    "Verify sccache hit rate and cache path.",
    "Compare against GitHub-hosted Windows x64 for the same PR set."
  ]
}
```

Default thresholds should be conservative and repo-configurable:

- alert on p50 or p90 duration regression >= 25% with enough samples.
- alert on failure-rate increase >= 10 percentage points.
- alert on queue-time increase when local capacity exists.
- suggest optimization when another configured runner is >= 20% faster with
  comparable or better failure rate.
- suppress findings when sample count is too low unless the change is extreme.

Example queries the CLI should make easy:

```sql
SELECT machines.name, AVG(steps.duration_ms)
FROM steps
JOIN jobs ON jobs.id = steps.job_id
JOIN machines ON machines.id = jobs.machine_id
WHERE steps.step = 'compile' AND steps.status = 'pass'
GROUP BY machines.name;
```

```sql
SELECT target, backend, provider, COUNT(*), AVG(total_ms), MAX(total_ms)
FROM jobs
WHERE status = 'pass'
GROUP BY target, backend, provider;
```

## Optional Tool Integrations

- `hyperfine`: use for controlled local benchmarks and import
  `--export-json` results into `metrics.db`. This is good for comparing a single
  command across Mac Studio, M5, and VM configurations.
- Bencher: evaluate for benchmark regression detection and trend charts when the
  measured thing is a real benchmark. It should be an exporter/importer target,
  not a dependency for answering runner-routing questions.
- BuildPulse: useful reference for CI time/flakiness analysis, but it is
  SaaS-oriented and should not be required for the local-first measurement loop.
- OpenTelemetry: defer. Spans map well to configure/compile/link/test, but the
  collector/exporter stack is more surface area than Phase 1 needs.

## Open Questions

- Whether Tart should emit boot/setup timing directly, or Shipyard should infer it
  from wrapper milestones. Prefer direct Tart fields if tartci can emit them.
- Whether Windows cache timing belongs in Shipyard metrics, tartci timings, or
  both with a shared field name.
- Whether agent consumption should be only CLI JSON at first, or whether a small
  read-only local API/MCP surface is worthwhile later. Start with CLI JSON unless
  an agent integration needs more.
- Whether Bencher is worth adopting for benchmark-style measurements after the
  local/GitHub runner timing data exists in one store.

## Initial Pulp Evaluation Matrix

Track these lanes first:

- macOS ARM64 on Mac Studio.
- macOS ARM64 on M5.
- Linux ARM64 Tart VM on Mac Studio.
- Windows ARM64 Tart/QEMU VM on Mac Studio.
- GitHub-hosted Linux x64.
- GitHub-hosted Windows x64.
- Scheduled/nightly Intel Linux and Windows jobs for architecture-specific drift.

The first decision target is simple: identify which PR lanes are actually faster
locally, which should stay GitHub-hosted, and which belong in scheduled Intel
coverage rather than every PR.

## Future Strategy Questions This Data Should Inform

- Are local runners consistently faster enough to justify making them the normal
  PR path?
- Which lanes should fail over to GitHub immediately versus queue locally for a
  bounded window?
- Which workloads only need scheduled Intel validation instead of every-PR Intel
  validation?
- Do GitHub queue/runtime costs or reliability become large enough that hosting
  more of the Git/runner loop locally is worth planning?
- If a private Git server is ever considered, can we keep GitHub as the backup
  remote and public integration point while preserving the measured local speed
  benefit?
