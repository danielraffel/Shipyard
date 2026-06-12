# CI Runtime Measurement Plan

Date: 2026-06-12

## Goal

Quantify whether a project should run each validation lane on local hardware
(`macstudio`, `m5`, Tart VMs) or GitHub-hosted runners by collecting comparable
runtime, queue, boot, and outcome data over time.

This should stay small: enough history and basic stats to guide routing profiles,
without committing Shipyard to a metrics platform. If an existing tool already
solves the storage/reporting layer cleanly, integrate with it instead of
rebuilding it.

The near-term output is operational: make it obvious how, where, and when each
lane should run. Longer term, the same data can inform whether it is worth
owning more of the stack, such as a private Git server with GitHub as a backup
push target, but that is an evaluation input, not Phase 1 scope.

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
records as the canonical local data source, then add GitHub import so the same
summary command can compare both worlds.

## Data Model

Start with append-only JSONL in Shipyard state. Add SQLite only if query speed or
retention makes JSONL painful.

Record one row per target/job attempt:

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

Extend existing Shipyard records before adding a new subsystem:

- Teach `shipyard run command` / local target evidence to emit an optional
  metrics JSONL row beside the existing evidence bundle.
- Include backend, target, host, command name, duration, exit status, and artifact
  cache annotations.
- Add `shipyard metrics list --project <name>` and `shipyard metrics summary`
  with p50/p90/min/max/count/failure-rate grouped by `project,target,backend,host`.
- Keep output available as human table and JSON so agents, plugins, and CI can
  parse the same truth.

Acceptance:

- Running the same local command on `macstudio` and `m5` creates comparable rows.
- Summary can answer: "For Pulp `linux-arm64`, which host is faster over the last
  N successful runs?"

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
- Recommend preferred location per lane when enough samples exist.
- Include confidence guardrails: minimum sample count, recent failure rate, stale
  data age, resource capacity, and queue/backlog signals.
- Emit explicit fallback reasoning such as:
  - `macstudio` primary: fastest p50 and healthy success rate.
  - `m5` fallback: slower but available when Mac Studio has no free slots.
  - GitHub fallback: slower but authoritative for Intel coverage or when local
    fleet is offline.

Acceptance:

- Advice is explainable from the metrics rows and never changes config unless a
  later explicit `--apply` mode is designed.

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

Minimum useful views:

- Last 20 runs for one lane.
- p50/p90 successful duration by target/backend/host over 7/14/30 days.
- queue time vs run time split for GitHub-hosted and self-hosted jobs.
- boot/setup/run split for Tart VM lanes.
- failure rate by lane and host.
- "Should this lane run locally?" advisory summary.

## Open Questions

- Whether Tart should emit boot/setup timing directly, or Shipyard should infer it
  from wrapper milestones.
- Whether Windows cache timing belongs in Shipyard metrics, tartci timings, or
  both with a shared field name.
- Whether a third-party dashboard is worth adopting after Phase 2. Until local
  Tart/SSH timings exist in the same shape as GitHub jobs, external dashboards
  are likely to answer only half the routing question.

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
