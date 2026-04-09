"""Shipyard CLI — the primary human and agent interface.

Every command supports --json for structured output. Human-readable
output is the default.
"""

from __future__ import annotations

import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import click

from shipyard import __version__
from shipyard.cloud.github import find_dispatched_run, run_view, workflow_dispatch
from shipyard.cloud.records import CloudRecordStore, CloudRunRecord
from shipyard.cloud.registry import default_workflow_key, discover_workflows, resolve_cloud_dispatch_plan
from shipyard.core.config import Config
from shipyard.core.evidence import EvidenceStore
from shipyard.core.job import Job, JobStatus, TargetResult, TargetStatus, ValidationMode
from shipyard.core.queue import Queue
from shipyard.executor.dispatch import ExecutorDispatcher
from shipyard.output.human import (
    console,
    render_doctor,
    render_error,
    render_evidence,
    render_job,
    render_message,
    render_status,
)
from shipyard.output.json_output import render_json
from shipyard.output.schema import OutputEnvelope
from shipyard.preflight import run_submission_preflight


class Context:
    """Shared CLI context."""

    def __init__(self, json_mode: bool = False) -> None:
        self.json_mode = json_mode
        self._config: Config | None = None
        self._queue: Queue | None = None
        self._evidence: EvidenceStore | None = None
        self._cloud_records: CloudRecordStore | None = None

    @property
    def config(self) -> Config:
        if self._config is None:
            self._config = Config.load_from_cwd()
        return self._config

    @property
    def queue(self) -> Queue:
        if self._queue is None:
            state_dir = self.config.state_dir / "queue"
            self._queue = Queue(state_dir=state_dir)
        return self._queue

    @property
    def evidence(self) -> EvidenceStore:
        if self._evidence is None:
            self._evidence = EvidenceStore(self.config.state_dir / "evidence")
        return self._evidence

    def output(self, command: str, data: dict[str, Any]) -> None:
        """Render output in the appropriate format."""
        if self.json_mode:
            render_json(OutputEnvelope(command=command, data=data))
        # Human output is handled by the calling command directly

    @property
    def cloud_records(self) -> CloudRecordStore:
        if self._cloud_records is None:
            self._cloud_records = CloudRecordStore(self.config.state_dir / "cloud")
        return self._cloud_records


pass_context = click.make_pass_decorator(Context, ensure=True)


@click.group()
@click.option("--json", "json_mode", is_flag=True, help="Output structured JSON")
@click.version_option(__version__, prog_name="shipyard")
@click.pass_context
def main(ctx: click.Context, json_mode: bool) -> None:
    """Shipyard — cross-platform CI coordination."""
    ctx.obj = Context(json_mode=json_mode)


@main.command()
@click.option("--targets", "-t", help="Comma-separated target names")
@click.option("--smoke", is_flag=True, help="Fast smoke validation")
@click.option(
    "--fail-fast/--continue", "fail_fast", default=True,
    help="Stop after first target failure (default) or run all",
)
@click.option(
    "--resume-from", type=click.Choice(["configure", "build", "test"]),
    help="Resume from a stage (skip earlier stages that already passed)",
)
@click.option(
    "--allow-root-mismatch",
    is_flag=True,
    help="Queue the run even if the git root does not match the Shipyard root",
)
@click.option(
    "--allow-unreachable-targets",
    is_flag=True,
    help="Queue the run even if no backend is reachable for one or more targets",
)
@click.pass_obj
def run(
    ctx: Context,
    targets: str | None,
    smoke: bool,
    fail_fast: bool,
    resume_from: str | None,
    allow_root_mismatch: bool,
    allow_unreachable_targets: bool,
) -> None:
    """Validate current HEAD on configured targets."""
    config = ctx.config
    mode = ValidationMode.SMOKE if smoke else ValidationMode.FULL

    # Get current SHA and branch
    sha = _git_sha()
    branch = _git_branch()
    if not sha or not branch:
        render_error("Not in a git repository")
        sys.exit(1)

    # Resolve targets
    target_names = targets.split(",") if targets else list(config.targets.keys())
    if not target_names:
        render_error("No targets configured. Run 'shipyard init' first.")
        sys.exit(1)

    dispatcher = _make_dispatcher(config)
    try:
        preflight = run_submission_preflight(
            config,
            target_names=target_names,
            dispatcher=dispatcher,
            allow_root_mismatch=allow_root_mismatch,
            allow_unreachable_targets=allow_unreachable_targets,
        )
    except ValueError as exc:
        render_error(str(exc))
        sys.exit(1)

    # Create and enqueue job
    job = Job.create(
        sha=sha,
        branch=branch,
        target_names=target_names,
        mode=mode,
    )
    job = ctx.queue.enqueue(job)

    if not ctx.json_mode:
        render_message(f"Queued {job.id} — {branch} @ {sha[:8]}")
        for warning in preflight.warnings:
            render_message(f"warning: {warning}", style="bold yellow")

    job = _execute_job(
        ctx=ctx,
        job=job,
        config=config,
        dispatcher=dispatcher,
        mode=mode,
        fail_fast=fail_fast,
        resume_from=resume_from,
    )

    if ctx.json_mode:
        ctx.output("run", {"run": job.to_dict(), "preflight": preflight.to_dict()})
    else:
        if job.passed:
            render_message("All green.", style="bold green")
        else:
            render_message("Failed.", style="bold red")
            sys.exit(1)


@main.command()
@click.pass_obj
def status(ctx: Context) -> None:
    """Show queue, active runs, and recent results."""
    active = ctx.queue.get_active()
    pending = ctx.queue.pending_count
    recent = ctx.queue.get_recent()

    dispatcher = _make_dispatcher(ctx.config)

    targets_info: dict[str, dict[str, Any]] = {}
    for name, tconfig in ctx.config.targets.items():
        target_config = dict(tconfig)
        target_config["name"] = name
        reachable, selected_backend = _probe_target(target_config, dispatcher)
        targets_info[name] = {
            "backend": dispatcher.backend_name(target_config),
            "reachable": reachable,
        }
        if selected_backend and selected_backend != targets_info[name]["backend"]:
            targets_info[name]["fallback"] = selected_backend

    if ctx.json_mode:
        data: dict[str, Any] = {
            "queue": {
                "pending": pending,
                "running": 1 if active else 0,
                "completed_recent": len(recent),
            },
        }
        if active:
            data["active_run"] = active.to_dict()
        data["targets"] = targets_info
        ctx.output("status", data)
    else:
        render_status(active, pending, recent, targets_info)


@main.command()
@click.argument("branch", required=False)
@click.pass_obj
def evidence(ctx: Context, branch: str | None) -> None:
    """Show last-good-SHA evidence per target."""
    branch = branch or _git_branch() or "main"
    records = ctx.evidence.get_branch(branch)

    if ctx.json_mode:
        ctx.output("evidence", {
            "branch": branch,
            "evidence": {k: v.to_dict() for k, v in records.items()},
        })
    else:
        if records:
            render_message(f"Evidence for {branch}:")
            render_evidence({k: v.to_dict() for k, v in records.items()})
        else:
            render_message(f"No evidence for {branch}", style="dim")


@main.command()
@click.argument("job_id")
@click.option("--target", "-t", help="Show logs for a specific target")
@click.pass_obj
def logs(ctx: Context, job_id: str, target: str | None) -> None:
    """Show logs from a run."""
    job = ctx.queue.get(job_id)
    if not job:
        render_error(f"Job {job_id} not found")
        sys.exit(1)

    if target:
        result = job.results.get(target)
        if not result or not result.log_path:
            render_error(f"No log for target {target}")
            sys.exit(1)
        log_file = Path(result.log_path)
        if log_file.exists():
            console.print(log_file.read_text())
        else:
            render_error(f"Log file not found: {result.log_path}")
    else:
        # Show all target logs
        for name in job.target_names:
            result = job.results.get(name)
            if result and result.log_path:
                log_file = Path(result.log_path)
                console.print(f"\n[bold cyan]--- {name} ---[/]")
                if log_file.exists():
                    console.print(log_file.read_text())
                else:
                    console.print(f"[dim]Log file not found: {result.log_path}[/]")


@main.command()
@click.argument("job_id")
@click.pass_obj
def cancel(ctx: Context, job_id: str) -> None:
    """Cancel a pending or running job."""
    job = ctx.queue.get(job_id)
    if not job:
        render_error(f"Job {job_id} not found")
        sys.exit(1)

    try:
        cancelled = job.cancel()
        ctx.queue.update(cancelled)
        if ctx.json_mode:
            ctx.output("cancel", {"job": cancelled.to_dict()})
        else:
            render_message(f"Cancelled {job_id}")
    except ValueError as e:
        render_error(str(e))
        sys.exit(1)


@main.command()
@click.argument("job_id")
@click.argument("priority", type=click.Choice(["low", "normal", "high"]))
@click.pass_obj
def bump(ctx: Context, job_id: str, priority: str) -> None:
    """Change the priority of a pending job."""
    from shipyard.core.job import Priority

    job = ctx.queue.get(job_id)
    if not job:
        render_error(f"Job {job_id} not found")
        sys.exit(1)

    if job.status != JobStatus.PENDING:
        render_error(f"Can only bump pending jobs (current: {job.status.value})")
        sys.exit(1)

    new_priority = Priority[priority.upper()]
    updated = job.with_priority(new_priority)
    ctx.queue.update(updated)

    if ctx.json_mode:
        ctx.output("bump", {"job": updated.to_dict()})
    else:
        render_message(f"Bumped {job_id} to {priority}")


@main.command(name="queue")
@click.pass_obj
def queue_cmd(ctx: Context) -> None:
    """Show all jobs in the queue."""
    queue = ctx.queue
    active = queue.get_active()
    pending = [j for j in queue._jobs if j.status == JobStatus.PENDING]
    queue._ensure_loaded()
    pending.sort(key=lambda j: (-j.priority.value, j.created_at))
    recent = queue.get_recent(limit=5)

    if ctx.json_mode:
        ctx.output("queue", {
            "active": active.to_dict() if active else None,
            "pending": [j.to_dict() for j in pending],
            "recent": [j.to_dict() for j in recent],
        })
    else:
        console.print()
        console.print("[bold]Queue[/]")

        if active:
            console.print("\n  [bold yellow]Running:[/]")
            console.print(f"    {active.id}  {active.branch} @ {active.sha[:8]}  [{active.priority.name.lower()}]")

        if pending:
            console.print(f"\n  [bold]Pending ({len(pending)}):[/]")
            for j in pending:
                console.print(f"    {j.id}  {j.branch} @ {j.sha[:8]}  [{j.priority.name.lower()}]")
        else:
            console.print("\n  [dim]No pending jobs[/]")

        if recent:
            console.print(f"\n  [bold]Recent ({len(recent)}):[/]")
            for j in recent:
                status = "[green]pass[/]" if j.passed else "[red]fail[/]"
                console.print(f"    {j.id}  {j.branch} @ {j.sha[:8]}  {status}")

        console.print()


@main.command()
@click.pass_obj
def doctor(ctx: Context) -> None:
    """Check environment, dependencies, and targets."""
    checks: dict[str, dict[str, Any]] = {}

    # Core tools
    core: dict[str, Any] = {}
    core["git"] = _check_command("git", "--version")
    core["ssh"] = _check_command("ssh", "-V")
    checks["Core"] = core

    # Cloud providers
    cloud: dict[str, Any] = {}
    cloud["gh"] = _check_command("gh", "--version")
    cloud["nsc"] = _check_command("nsc", "version")
    checks["Cloud providers"] = cloud

    # Governance drift — best-effort. If the repo can't be detected
    # or gh isn't authenticated, this section is skipped silently
    # rather than blocking the whole doctor command.
    governance_section = _check_governance_drift(ctx.config)
    if governance_section:
        checks["Governance"] = governance_section

    # Ready = core tools healthy AND (no governance section declared
    # OR every governance check is ok). Informational entries (the
    # immutable-releases line) set `ok=True` specifically because
    # they can't lie — they report what the API exposes and nothing
    # more — so they're allowed to contribute to the ready rollup.
    ready = all(info.get("ok", False) for info in core.values()) and all(
        info.get("ok", False) for info in (governance_section or {}).values()
    )

    if ctx.json_mode:
        ctx.output("doctor", {"ready": ready, "checks": checks})
    else:
        render_doctor(checks, ready)


def _check_governance_drift(config: Config) -> dict[str, Any] | None:
    """Produce a doctor section reporting governance drift.

    Returns None if the repo cannot be detected or the user has not
    declared a governance section. Never raises — any failure is
    translated into a single diagnostic entry so doctor stays
    informative even when gh auth is missing.
    """
    from shipyard.governance import (
        build_status,
        detect_repo_from_remote,
        load_governance_config,
    )

    section: dict[str, Any] = {}
    try:
        governance = load_governance_config(config)
    except ValueError as exc:
        return {"config": {"ok": False, "detail": f"Config error: {exc}"}}

    # Only probe GitHub if the user has explicitly declared a profile.
    # Without a profile, governance is opt-out and doctor shouldn't
    # second-guess the choice.
    declared_profile = config.get("project.profile")
    if not declared_profile:
        return None

    repo = detect_repo_from_remote()
    if repo is None:
        return {"profile": {
            "ok": True,
            "detail": f"declared={declared_profile}; no github remote detected",
        }}

    try:
        status = build_status(
            repo=repo, governance=governance, branches=("main",),
        )
    except Exception as exc:  # noqa: BLE001 — diagnostic path
        return {"profile": {
            "ok": False,
            "detail": f"could not fetch live state: {exc}",
        }}

    # Fetch errors from build_status must NOT be swallowed into a
    # false-green. If gh is unauthenticated or a branch returned
    # a 5xx, has_drift can be False even though we never actually
    # read the live state. Treat any error as a not-aligned result.
    if status.has_errors:
        section["main"] = {
            "ok": False,
            "detail": f"could not read live state — {'; '.join(status.errors)[:200]}",
        }
    elif status.has_drift:
        drifted_fields = [
            f"{r.branch}:{e.field_name}"
            for r in status.reports
            for e in r.drifted_entries
        ]
        section["main"] = {
            "ok": False,
            "detail": f"drift on {', '.join(drifted_fields[:3])}"
                     + ("..." if len(drifted_fields) > 3 else "")
                     + " — run: shipyard governance apply",
        }
    else:
        section["main"] = {
            "ok": True,
            "detail": f"aligned with {declared_profile} profile",
        }

    # Honest immutable-releases line — per Part 12 spec, never claim
    # a state Shipyard cannot verify on personal repos.
    section["immutable_releases"] = {
        "ok": True,  # info-only
        "detail": (
            "GitHub does not expose the repo-level setting via API. "
            f"Verify at https://github.com/{repo.slug}/settings"
        ),
    }
    return section


@main.command(name="init")
@click.option("--discover-only", is_flag=True, help="Show what was detected, don't write config")
@click.pass_obj
def init_cmd(ctx: Context, discover_only: bool) -> None:
    """Configure Shipyard for this project."""
    from shipyard.init.wizard import run_init

    config = run_init(Path.cwd(), non_interactive=True)
    if ctx.json_mode:
        ctx.output("init", config.to_dict())
    elif not discover_only:
        render_message("Shipyard configured. Try: shipyard run", style="bold green")
    else:
        render_message("Detected config (not written):")
        import json as _json
        render_message(_json.dumps(config.to_dict(), indent=2))


# ── governance commands ────────────────────────────────────────────────


@main.group()
def governance() -> None:
    """Manage branch protection and governance profiles."""


@governance.command("status")
@click.option(
    "--branch", "-b", multiple=True,
    help="Override which branches to check (repeatable). Default: main.",
)
@click.pass_obj
def governance_status(ctx: Context, branch: tuple[str, ...]) -> None:
    """Report declared-vs-live governance drift per branch."""
    from shipyard.governance import (
        build_status,
        detect_repo_from_remote,
        format_status_text,
        load_governance_config,
    )

    gov = load_governance_config(ctx.config)
    repo = detect_repo_from_remote()
    if repo is None:
        render_error("Could not detect repo from git remote. Is this a GitHub repo?")
        sys.exit(1)

    branches = branch or ("main",)
    status = build_status(repo=repo, governance=gov, branches=branches)

    if ctx.json_mode:
        ctx.output("governance.status", {
            "repo": repo.slug,
            "profile": status.profile_name,
            "has_drift": status.has_drift,
            "reports": [
                {
                    "branch": r.branch,
                    "live_unprotected": r.live_unprotected,
                    "drifted_fields": [e.field_name for e in r.drifted_entries],
                    "deviated_fields": [e.field_name for e in r.deviated_entries],
                }
                for r in status.reports
            ],
            "errors": list(status.errors),
        })
    else:
        render_message(format_status_text(status))

    if status.has_drift or status.has_errors:
        sys.exit(1)


@governance.command("apply")
@click.option(
    "--branch", "-b", multiple=True,
    help="Override which branches to apply to (repeatable). Default: main.",
)
@click.option("--dry-run", is_flag=True, help="Show what would change without writing")
@click.option(
    "--from", "from_path",
    type=click.Path(exists=True, dir_okay=False),
    help="Apply rules from a snapshot file instead of the project config",
)
@click.pass_obj
def governance_apply(
    ctx: Context,
    branch: tuple[str, ...],
    dry_run: bool,
    from_path: str | None,
) -> None:
    """Apply declared governance rules to live state (idempotent).

    When --from is given, the snapshot file is the source of
    truth instead of `.shipyard/config.toml` + profile defaults.
    This is the disaster-recovery path: re-apply a known-good
    state captured via `governance export`.
    """
    from shipyard.governance import (
        build_apply_plan,
        build_status,
        compute_drift,
        detect_repo_from_remote,
        execute_apply_plan,
        get_branch_protection,
        load_governance_config,
        resolve_branch_rules,
    )

    gov = load_governance_config(ctx.config)
    repo = detect_repo_from_remote()
    if repo is None:
        render_error("Could not detect repo from git remote.")
        sys.exit(1)

    branches = branch or ("main",)

    # ── Snapshot path ──────────────────────────────────────────
    if from_path:
        from pathlib import Path

        from shipyard.governance.snapshot import GovernanceSnapshot

        try:
            snapshot = GovernanceSnapshot.from_toml(Path(from_path).read_text())
        except ValueError as exc:
            render_error(f"Could not parse snapshot: {exc}")
            sys.exit(1)

        # Refuse to apply a snapshot to a different repo — the repo
        # slug is recorded at export time specifically so a copy-
        # paste accident can't silently reconfigure the wrong repo.
        if snapshot.repo_slug != repo.slug:
            render_error(
                f"Snapshot is for '{snapshot.repo_slug}' but current repo is "
                f"'{repo.slug}'. Refusing to apply."
            )
            sys.exit(1)

        results: list[Any] = []
        errors: list[str] = []
        snapshot_branches = branches or tuple(snapshot.branches.keys())
        for branch_name in snapshot_branches:
            if branch_name not in snapshot.branches:
                errors.append(f"{branch_name}: not in snapshot")
                continue
            declared = snapshot.branches[branch_name]
            from shipyard.governance.github import GovernanceApiError
            try:
                live = get_branch_protection(repo, branch_name)
            except GovernanceApiError as exc:
                errors.append(f"{branch_name}: {exc}")
                continue
            report = compute_drift(
                branch=branch_name,
                profile_rules=declared,
                declared_rules=declared,
                live_rules=live,
            )
            plan = build_apply_plan(
                repo=repo,
                branch=branch_name,
                declared_rules=declared,
                drift_report=report,
            )
            results.append(execute_apply_plan(plan, dry_run=dry_run))

        _render_apply_results(ctx, results, errors, dry_run=dry_run, from_snapshot=True)
        if errors or any(r.error_message for r in results):
            sys.exit(1)
        return

    # ── Profile/config path ────────────────────────────────────
    status = build_status(repo=repo, governance=gov, branches=branches)

    results = []
    for report in status.reports:
        declared = resolve_branch_rules(gov, report.branch)
        plan = build_apply_plan(
            repo=repo,
            branch=report.branch,
            declared_rules=declared,
            drift_report=report,
        )
        result = execute_apply_plan(plan, dry_run=dry_run)
        results.append(result)

    _render_apply_results(
        ctx, results, list(status.errors), dry_run=dry_run, from_snapshot=False,
    )

    any_errors = any(r.error_message for r in results) or status.has_errors
    if any_errors:
        sys.exit(1)


def _render_apply_results(
    ctx: Context,
    results: list[Any],
    errors: list[str],
    *,
    dry_run: bool,
    from_snapshot: bool,
) -> None:
    """Shared apply-output renderer for both the config and snapshot paths."""
    any_changes = any(
        r.executed or (dry_run and not r.plan.is_noop) for r in results
    )

    if ctx.json_mode:
        ctx.output("governance.apply", {
            "dry_run": dry_run,
            "from_snapshot": from_snapshot,
            "changed": any_changes,
            "results": [
                {
                    "branch": r.plan.branch,
                    "action": r.plan.action.value,
                    "executed": r.executed,
                    "error": r.error_message,
                }
                for r in results
            ],
            "errors": errors,
        })
        return

    if not results and not errors:
        render_message("No branches to apply to.", style="dim")
    for result in results:
        branch_name = result.plan.branch
        action = result.plan.action.value
        if result.error_message:
            render_message(
                f"  ✗ {branch_name}: {action} failed — {result.error_message}",
                style="bold red",
            )
        elif result.plan.is_noop:
            render_message(f"  ✓ {branch_name}: already aligned (no changes)")
        elif dry_run:
            drifted = [e.field_name for e in result.plan.drift_report.drifted_entries]
            render_message(
                f"  → {branch_name}: would {action}"
                + (f" (fields: {', '.join(drifted)})" if drifted else "")
            )
        else:
            render_message(f"  ✓ {branch_name}: {action} applied", style="green")

    for err in errors:
        render_message(f"  ! {err}", style="yellow")

    # Manual followups, printed every time per Part 12 spec
    if results:
        render_message("")
        render_message("Manual followups (Shipyard cannot apply these via API):")
        for followup in results[0].plan.manual_followups:
            render_message(f"  ⚠ {followup}")


@governance.command("diff")
@click.option("--branch", "-b", multiple=True, help="Branches to check")
@click.pass_obj
def governance_diff(ctx: Context, branch: tuple[str, ...]) -> None:
    """Show what `governance apply` would change, without applying."""
    # Delegate to governance_apply with --dry-run by calling the same
    # logic in-place.
    from shipyard.governance import (
        build_apply_plan,
        build_status,
        detect_repo_from_remote,
        load_governance_config,
        resolve_branch_rules,
    )

    gov = load_governance_config(ctx.config)
    repo = detect_repo_from_remote()
    if repo is None:
        render_error("Could not detect repo from git remote.")
        sys.exit(1)

    branches = branch or ("main",)
    status = build_status(repo=repo, governance=gov, branches=branches)

    any_drift = False
    for report in status.reports:
        declared = resolve_branch_rules(gov, report.branch)
        plan = build_apply_plan(
            repo=repo, branch=report.branch,
            declared_rules=declared, drift_report=report,
        )
        if plan.is_noop:
            render_message(f"  ✓ {report.branch}: no changes")
            continue
        any_drift = True
        drifted = report.drifted_entries
        if report.live_unprotected:
            render_message(
                f"  + {report.branch}: create protection "
                f"({len(report.entries)} fields)",
                style="yellow",
            )
        else:
            render_message(
                f"  ~ {report.branch}: update {len(drifted)} field(s)",
                style="yellow",
            )
            for entry in drifted:
                render_message(
                    f"      {entry.field_name}: "
                    f"{entry.live_value!r} → {entry.declared_value!r}"
                )
    if any_drift:
        render_message("")
        render_message("Run: shipyard governance apply")

    # If any branch fetch failed, surface the error and exit
    # non-zero so automation does not treat a clean diff produced
    # from incomplete reads as "nothing to apply".
    if status.has_errors:
        render_message("")
        render_error(
            "governance diff: live state could not be read for one or more branches"
        )
        for err in status.errors:
            render_message(f"  ! {err}")
        sys.exit(1)


@governance.command("export")
@click.option(
    "--branch", "-b", multiple=True,
    help="Branches to snapshot (repeatable). Default: main.",
)
@click.option(
    "--output", "-o",
    type=click.Path(dir_okay=False),
    help="Write snapshot to file instead of stdout",
)
@click.pass_obj
def governance_export(
    ctx: Context,
    branch: tuple[str, ...],
    output: str | None,
) -> None:
    """Snapshot live GitHub governance state to TOML.

    The snapshot is check-in-able and can be fed back to
    `governance apply --from <file>` for disaster recovery or
    audit-trail diffing.
    """
    from pathlib import Path

    from shipyard.governance import (
        detect_repo_from_remote,
        get_branch_protection,
    )
    from shipyard.governance.github import GovernanceApiError
    from shipyard.governance.snapshot import build_snapshot

    repo = detect_repo_from_remote()
    if repo is None:
        render_error("Could not detect repo from git remote.")
        sys.exit(1)

    branches = branch or ("main",)
    live_branches: dict[str, Any] = {}
    errors: list[str] = []
    for branch_name in branches:
        try:
            rules = get_branch_protection(repo, branch_name)
        except GovernanceApiError as exc:
            errors.append(f"{branch_name}: {exc}")
            continue
        if rules is None:
            errors.append(f"{branch_name}: no protection set")
            continue
        live_branches[branch_name] = rules

    if errors:
        for err in errors:
            render_error(err)
        sys.exit(1)

    snapshot = build_snapshot(repo=repo, live_branches=live_branches)
    toml_text = snapshot.to_toml()

    if output:
        Path(output).write_text(toml_text)
        render_message(
            f"Wrote snapshot for {repo.slug} to {output}", style="green",
        )
    else:
        # Stream straight to stdout so users can pipe to a file.
        click.echo(toml_text, nl=False)


@governance.command("use")
@click.argument("profile_name", type=click.Choice(["solo", "multi", "custom"]))
@click.option("--yes", "-y", is_flag=True, help="Skip the interactive prompt")
@click.option("--dry-run", is_flag=True, help="Show the diff without applying")
@click.pass_obj
def governance_use(
    ctx: Context,
    profile_name: str,
    yes: bool,
    dry_run: bool,
) -> None:
    """Switch governance profile + apply (interactive).

    Updates `[project].profile` in `.shipyard/config.toml`, then
    runs the existing apply path. Any explicit overrides in
    `[branch_protection.*]` are preserved.
    """
    if ctx.config.project_dir is None:
        render_error(
            "No .shipyard/config.toml found. Run `shipyard init` first."
        )
        sys.exit(1)

    config_path = ctx.config.project_dir / "config.toml"
    if not config_path.exists():
        render_error(f"Config file not found: {config_path}")
        sys.exit(1)

    current_profile = str(ctx.config.get("project.profile", "solo"))
    if current_profile == profile_name and not dry_run:
        render_message(
            f"Already on profile '{profile_name}'. Running apply to verify live state…",
        )

    # Show the diff that would land if we switched
    from shipyard.governance import (
        build_apply_plan,
        build_status,
        detect_repo_from_remote,
        load_governance_config,
        profile_for_name,
        resolve_branch_rules,
    )

    repo = detect_repo_from_remote()
    if repo is None:
        render_error("Could not detect repo from git remote.")
        sys.exit(1)

    # Clone the config dict, flip the profile, re-resolve
    hypothetical_data = dict(ctx.config.data)
    hypothetical_project = dict(hypothetical_data.get("project", {}))
    hypothetical_project["profile"] = profile_name
    hypothetical_data["project"] = hypothetical_project
    hypothetical_config = Config(data=hypothetical_data)
    hypothetical_gov = load_governance_config(hypothetical_config)

    # Use the hypothetical profile for the preview
    required = hypothetical_gov.required_status_checks
    profile_for_name(profile_name, required_status_checks=required)

    status = build_status(
        repo=repo, governance=hypothetical_gov, branches=("main",),
    )
    any_change = False
    render_message(f"Switching profile: {current_profile} → {profile_name}")
    render_message("")
    for report in status.reports:
        if report.has_drift:
            any_change = True
            declared = resolve_branch_rules(hypothetical_gov, report.branch)
            plan = build_apply_plan(
                repo=repo, branch=report.branch,
                declared_rules=declared, drift_report=report,
            )
            drifted = [e.field_name for e in report.drifted_entries]
            render_message(
                f"  {report.branch}: would {plan.action.value} "
                f"({len(drifted)} field(s): {', '.join(drifted)})"
            )
        else:
            render_message(f"  {report.branch}: no changes")

    if dry_run:
        return

    if any_change and not yes:
        render_message("")
        click.confirm("Apply these changes?", abort=True)

    # Rewrite the project config file with the new profile
    _rewrite_profile_in_config(config_path, profile_name)
    render_message(
        f"Updated {config_path} → profile = \"{profile_name}\"",
        style="green",
    )

    # And run apply using the updated config
    ctx._config = None  # force reload on next access
    from shipyard.governance import (
        build_apply_plan as _bap,
    )
    from shipyard.governance import (
        execute_apply_plan as _eap,
    )
    reloaded_gov = load_governance_config(ctx.config)
    status2 = build_status(repo=repo, governance=reloaded_gov, branches=("main",))
    results = []
    for report in status2.reports:
        declared = resolve_branch_rules(reloaded_gov, report.branch)
        plan = _bap(
            repo=repo, branch=report.branch,
            declared_rules=declared, drift_report=report,
        )
        results.append(_eap(plan, dry_run=False))
    _render_apply_results(
        ctx, results, list(status2.errors), dry_run=False, from_snapshot=False,
    )
    if any(r.error_message for r in results) or status2.has_errors:
        sys.exit(1)


def _rewrite_profile_in_config(config_path: Path, new_profile: str) -> None:
    """Idempotently set `[project].profile = new_profile` in a TOML file.

    Uses line-level rewriting rather than a full TOML round-trip so
    comments and ordering in the user's config are preserved.
    """
    text = config_path.read_text()
    lines = text.splitlines(keepends=True)
    in_project_section = False
    replaced = False
    out: list[str] = []
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_project_section = stripped == "[project]"
            out.append(line)
            continue
        if in_project_section and stripped.startswith("profile"):
            # Preserve the leading whitespace
            leading = line[: len(line) - len(line.lstrip())]
            out.append(f'{leading}profile   = "{new_profile}"\n')
            replaced = True
            continue
        out.append(line)

    if not replaced:
        # Project section exists but no profile line — append one
        # right after the [project] header.
        final: list[str] = []
        for line in out:
            final.append(line)
            if line.strip() == "[project]":
                final.append(f'profile   = "{new_profile}"\n')
                replaced = True
        out = final

    config_path.write_text("".join(out))


def _should_auto_create_base(base: str, flag: bool | None) -> bool:
    """Decide whether `ship --base <x>` should auto-create-base.

    Explicit --auto-create-base/--no-auto-create-base wins. When
    the flag is unset, default to on for `develop/*` and
    `release/*` patterns (the two cases where the planning doc
    explicitly wants auto-creation) and off for everything else.
    """
    if flag is not None:
        return flag
    return base.startswith("develop/") or base.startswith("release/")


def _maybe_auto_create_base_branch(ctx: Context, base: str) -> None:
    """If `base` does not exist on origin, create it + apply its rules.

    Best-effort: on any failure this prints a warning and returns.
    The caller still tries to push and create the PR, which will
    surface any hard failure from git/gh in the normal path rather
    than stopping ship() early on a transient API hiccup.
    """
    from shipyard.governance import (
        detect_repo_from_remote,
        load_governance_config,
        resolve_branch_rules,
    )
    from shipyard.governance.branch_create import (
        BranchCreateStatus,
        create_branch_and_apply_rules,
    )

    # Cheap check first: does the branch already exist on the remote?
    check = subprocess.run(
        ["git", "ls-remote", "--exit-code", "--heads", "origin", base],
        capture_output=True,
        text=True,
        timeout=15,
    )
    if check.returncode == 0:
        return  # Branch already exists, nothing to do

    # Branch is missing — try to create + protect it.
    repo = detect_repo_from_remote()
    if repo is None:
        render_message(
            f"warning: --base {base} does not exist on origin and the "
            f"repo could not be detected from the git remote; skipping "
            f"auto-create-base.",
            style="bold yellow",
        )
        return

    gov = load_governance_config(ctx.config)
    rules = resolve_branch_rules(gov, base)
    render_message(f"Creating base branch '{base}' from 'main' + applying governance rules…")
    result = create_branch_and_apply_rules(
        repo=repo, branch=base, base_branch="main", rules=rules,
    )
    if result.status == BranchCreateStatus.RULES_APPLIED or result.status == BranchCreateStatus.CREATED:
        render_message(f"  ✓ {result.message}", style="green")
    elif result.status == BranchCreateStatus.ALREADY_EXISTS:
        render_message(f"  ℹ {result.message}")
    else:
        render_message(f"  ✗ {result.message}", style="bold red")
        render_message(
            "continuing with ship flow anyway; fix the branch protection "
            "with `shipyard branch apply` after the PR is opened",
            style="yellow",
        )


# ── branch commands ───────────────────────────────────────────────────


@main.group()
def branch() -> None:
    """Manage branch protection for individual branches."""


@branch.command("apply")
@click.option(
    "--create", "create_name",
    help="Create the branch from --base if it doesn't exist, then apply rules",
)
@click.option(
    "--base", "base_branch",
    default="main",
    help="Base branch to create from when --create is given (default: main)",
)
@click.argument("target_branch", required=False)
@click.pass_obj
def branch_apply(
    ctx: Context,
    create_name: str | None,
    base_branch: str,
    target_branch: str | None,
) -> None:
    """Apply declared governance rules to a single branch.

    Two modes:

      shipyard branch apply <name>
          Apply rules to an existing branch (same as
          `governance apply --branch <name>`, provided here for
          symmetry with the create flow).

      shipyard branch apply --create develop/foo
          Create `develop/foo` from --base (default: main) and
          apply the matching `[branch_protection."<glob>"]` rules
          in one shot. Prevents the "new branch is unprotected
          until someone runs apply later" failure mode.
    """
    from shipyard.governance import (
        detect_repo_from_remote,
        load_governance_config,
        resolve_branch_rules,
    )
    from shipyard.governance.branch_create import (
        BranchCreateStatus,
        create_branch_and_apply_rules,
    )

    repo = detect_repo_from_remote()
    if repo is None:
        render_error("Could not detect repo from git remote.")
        sys.exit(1)

    # Figure out which branch we're acting on
    name_arg = create_name or target_branch
    if not name_arg:
        render_error("Specify a branch name (positional) or --create <name>")
        sys.exit(1)

    gov = load_governance_config(ctx.config)
    rules = resolve_branch_rules(gov, name_arg)

    if create_name:
        result = create_branch_and_apply_rules(
            repo=repo,
            branch=create_name,
            base_branch=base_branch,
            rules=rules,
        )
        if ctx.json_mode:
            ctx.output("branch.apply", {
                "branch": result.branch,
                "status": result.status.value,
                "message": result.message,
                "ok": result.ok,
            })
        else:
            if result.status == BranchCreateStatus.RULES_APPLIED or result.status == BranchCreateStatus.CREATED:
                render_message(f"  ✓ {result.message}", style="green")
            elif result.status == BranchCreateStatus.ALREADY_EXISTS:
                render_message(f"  ℹ {result.message}", style="yellow")
            else:
                render_message(f"  ✗ {result.message}", style="bold red")
        if not result.ok:
            sys.exit(1)
        return

    # Existing-branch apply path — delegate to the governance
    # apply flow for a single branch.
    from shipyard.governance import (
        build_apply_plan,
        build_status,
        execute_apply_plan,
    )

    status = build_status(
        repo=repo, governance=gov, branches=(name_arg,),
    )
    results = []
    for report in status.reports:
        declared = resolve_branch_rules(gov, report.branch)
        plan = build_apply_plan(
            repo=repo, branch=report.branch,
            declared_rules=declared, drift_report=report,
        )
        results.append(execute_apply_plan(plan, dry_run=False))
    _render_apply_results(
        ctx, results, list(status.errors), dry_run=False, from_snapshot=False,
    )
    if any(r.error_message for r in results) or status.has_errors:
        sys.exit(1)


@main.group()
def cloud() -> None:
    """Dispatch and inspect GitHub Actions workflows."""


@cloud.command("workflows")
@click.pass_obj
def cloud_workflows(ctx: Context) -> None:
    workflows = discover_workflows()
    data = {
        "workflows": {key: workflow.to_dict() for key, workflow in workflows.items()},
        "default": default_workflow_key(ctx.config, workflows),
    }
    if ctx.json_mode:
        ctx.output("cloud.workflows", data)
        return

    if not workflows:
        render_message("No GitHub workflows discovered.", style="dim")
        return
    render_message("Discovered workflows:")
    for key, workflow in workflows.items():
        render_message(f"  {key}: {workflow.description}")


@cloud.command("defaults")
@click.pass_obj
def cloud_defaults(ctx: Context) -> None:
    workflows = discover_workflows()
    default_key = default_workflow_key(ctx.config, workflows)
    provider = ctx.config.get("cloud.provider", "github-hosted")
    ref = _git_branch() or "main"
    resolved_plans: dict[str, dict[str, Any]] = {}
    for key in workflows:
        try:
            resolved_plans[key] = resolve_cloud_dispatch_plan(
                config=ctx.config,
                workflows=workflows,
                workflow_key=key,
                ref=ref,
            ).to_dict()
        except ValueError:
            continue
    data = {
        "repository": ctx.config.get("cloud.repository"),
        "default_workflow": default_key,
        "default_provider": provider,
        "workflows": {key: workflow.to_dict() for key, workflow in workflows.items()},
        "resolved": resolved_plans,
    }
    if ctx.json_mode:
        ctx.output("cloud.defaults", data)
        return

    render_message(f"repository: {ctx.config.get('cloud.repository') or 'current repo'}")
    render_message(f"default workflow: {default_key or 'none'}")
    render_message(f"default provider: {provider}")
    if workflows:
        render_message("workflows:")
        for key, workflow in workflows.items():
            resolved = resolved_plans.get(key, {})
            fields = resolved.get("dispatch_fields", {})
            field_summary = ", ".join(f"{name}={value}" for name, value in fields.items()) or "no dispatch fields"
            render_message(f"  {key}: {workflow.file} ({field_summary})")


@cloud.command("run")
@click.argument("workflow_key", required=False)
@click.argument("ref", required=False)
@click.option("--provider", help="Runner provider override")
@click.option("--wait/--no-wait", default=False, help="Wait for the dispatched workflow to complete")
@click.option("--runner-selector", help="Generic runner selector input")
@click.option("--linux-runner-selector", help="Linux runner selector override")
@click.option("--windows-runner-selector", help="Windows runner selector override")
@click.option("--macos-runner-selector", help="macOS runner selector override")
@click.pass_obj
def cloud_run(
    ctx: Context,
    workflow_key: str | None,
    ref: str | None,
    provider: str | None,
    wait: bool,
    runner_selector: str | None,
    linux_runner_selector: str | None,
    windows_runner_selector: str | None,
    macos_runner_selector: str | None,
) -> None:
    workflows = discover_workflows()
    workflow_key = workflow_key or default_workflow_key(ctx.config, workflows)
    if not workflow_key:
        render_error("No workflows discovered")
        sys.exit(1)

    resolved_ref = ref or _git_branch()
    if not resolved_ref:
        render_error("Not in a git repository")
        sys.exit(1)

    try:
        plan = resolve_cloud_dispatch_plan(
            config=ctx.config,
            workflows=workflows,
            workflow_key=workflow_key,
            ref=resolved_ref,
            provider_override=provider,
            runner_selector=runner_selector,
            linux_runner_selector=linux_runner_selector,
            windows_runner_selector=windows_runner_selector,
            macos_runner_selector=macos_runner_selector,
        )
    except ValueError as exc:
        render_error(str(exc))
        sys.exit(1)

    dispatch_id = ctx.cloud_records.new_dispatch_id()
    record = CloudRunRecord(
        dispatch_id=dispatch_id,
        workflow_key=plan.workflow.key,
        workflow_file=plan.workflow.file,
        workflow_name=plan.workflow.name,
        repository=plan.repository,
        requested_ref=plan.ref,
        provider=plan.provider,
        dispatch_fields=plan.dispatch_fields,
        status="dispatched",
        dispatched_at=datetime.now(timezone.utc),
        updated_at=datetime.now(timezone.utc),
    )

    try:
        workflow_dispatch(
            repository=plan.repository,
            workflow_file=plan.workflow.file,
            ref=plan.ref,
            fields=plan.dispatch_fields,
        )
        discovered = find_dispatched_run(
            repository=plan.repository,
            workflow_file=plan.workflow.file,
            ref=plan.ref,
        )
        record = CloudRunRecord(
            **{
                **record.__dict__,
                "status": str(discovered.get("status") or "queued"),
                "run_id": str(discovered["databaseId"]),
                "url": discovered.get("url"),
                "updated_at": datetime.now(timezone.utc),
            }
        )
        if wait and record.run_id:
            view = _wait_for_cloud_completion(record.repository, record.run_id)
            record = CloudRunRecord(
                **{
                    **record.__dict__,
                    "status": str(view.get("status") or "unknown"),
                    "conclusion": view.get("conclusion"),
                    "url": view.get("url") or record.url,
                    "started_at": record.started_at or datetime.now(timezone.utc),
                    "completed_at": datetime.now(timezone.utc) if view.get("status") == "completed" else None,
                    "updated_at": datetime.now(timezone.utc),
                }
            )
    except (subprocess.CalledProcessError, TimeoutError) as exc:
        record = CloudRunRecord(
            **{
                **record.__dict__,
                "status": "error",
                "conclusion": "error",
                "updated_at": datetime.now(timezone.utc),
            }
        )
        ctx.cloud_records.save(record)
        render_error(str(exc))
        sys.exit(1)

    ctx.cloud_records.save(record)
    data = {"record": record.to_dict(), "plan": plan.to_dict()}
    if ctx.json_mode:
        ctx.output("cloud.run", data)
        return

    render_message(f"Dispatched {record.workflow_key} to {record.provider} ({record.dispatch_id})")
    if record.run_id:
        render_message(f"run id: {record.run_id}")
    if record.url:
        render_message(f"url: {record.url}")


@cloud.command("status")
@click.argument("identifier", required=False)
@click.option("--limit", default=10, show_default=True, help="Number of records to show")
@click.option("--refresh/--no-refresh", default=False, help="Refresh run state from GitHub before rendering")
@click.pass_obj
def cloud_status(ctx: Context, identifier: str | None, limit: int, refresh: bool) -> None:
    records = ctx.cloud_records.list(limit=limit)
    if identifier and identifier not in {"latest", ""}:
        selected = ctx.cloud_records.get(identifier)
        records = [selected] if selected else []
    elif identifier == "latest" and records:
        records = [records[0]]

    refreshed: list[CloudRunRecord] = []
    for record in records:
        if record is None:
            continue
        updated = record
        if refresh and record.run_id:
            try:
                view = run_view(repository=record.repository, run_id=record.run_id)
                updated = CloudRunRecord(
                    **{
                        **record.__dict__,
                        "status": str(view.get("status") or record.status),
                        "conclusion": view.get("conclusion"),
                        "url": view.get("url") or record.url,
                        "updated_at": datetime.now(timezone.utc),
                        "completed_at": datetime.now(timezone.utc)
                        if view.get("status") == "completed"
                        else record.completed_at,
                    }
                )
                ctx.cloud_records.save(updated)
            except subprocess.CalledProcessError:
                updated = record
        refreshed.append(updated)

    data = {"records": [record.to_dict() for record in refreshed]}
    if ctx.json_mode:
        ctx.output("cloud.status", data)
        return

    if not refreshed:
        render_message("No tracked cloud runs yet.", style="dim")
        return
    for record in refreshed:
        render_message(
            f"{record.dispatch_id}: {record.workflow_key} ref={record.requested_ref} "
            f"provider={record.provider} status={record.status} conclusion={record.conclusion or '-'}"
        )


@main.command()
@click.option("--base", default="main", help="Base branch for PR")
@click.option(
    "--allow-root-mismatch",
    is_flag=True,
    help="Queue the run even if the git root does not match the Shipyard root",
)
@click.option(
    "--allow-unreachable-targets",
    is_flag=True,
    help="Queue the run even if no backend is reachable for one or more targets",
)
@click.option(
    "--auto-create-base/--no-auto-create-base",
    default=None,
    help=(
        "If the --base branch does not exist on the remote, create it from "
        "main and apply matching branch_protection rules before opening the "
        "PR. Default: on for develop/* and release/* patterns; off for "
        "everything else."
    ),
)
@click.pass_obj
def ship(
    ctx: Context,
    base: str,
    allow_root_mismatch: bool,
    allow_unreachable_targets: bool,
    auto_create_base: bool | None,
) -> None:
    """Branch -> PR -> validate -> merge on green."""
    from shipyard.ship.pr import create_pr, find_pr_for_branch, merge_pr

    branch = _git_branch()
    sha = _git_sha()
    if not branch or not sha:
        render_error("Not in a git repository")
        sys.exit(1)
    if branch == base:
        render_error(f"Already on {base}. Switch to a feature branch first.")
        sys.exit(1)

    # ── Auto-create missing base branch ─────────────────────────
    # If the user is shipping into a `develop/*` or `release/*`
    # branch that doesn't exist yet, create it from main and apply
    # its matching governance rules before opening the PR. This
    # closes the "new develop branch exists unprotected until
    # someone remembers to run branch apply" gap.
    if _should_auto_create_base(base, auto_create_base):
        _maybe_auto_create_base_branch(ctx, base)

    # Push branch
    subprocess.run(["git", "push", "-u", "origin", branch], capture_output=True)

    # Find or create PR
    existing = find_pr_for_branch(branch)
    if existing:
        pr_info = existing
        if not ctx.json_mode:
            render_message(f"Found existing PR #{pr_info.number}")
    else:
        pr_info = create_pr(branch, base, f"Ship {branch}", "Automated by Shipyard")
        if not ctx.json_mode:
            render_message(f"Created PR #{pr_info.number}")

    if not pr_info:
        render_error("Failed to create or find PR")
        sys.exit(1)

    # Run validation
    config = ctx.config
    target_names = list(config.targets.keys())
    if not target_names:
        render_error("No targets configured")
        sys.exit(1)

    dispatcher = _make_dispatcher(config)
    try:
        preflight = run_submission_preflight(
            config,
            target_names=target_names,
            dispatcher=dispatcher,
            allow_root_mismatch=allow_root_mismatch,
            allow_unreachable_targets=allow_unreachable_targets,
        )
    except ValueError as exc:
        render_error(str(exc))
        sys.exit(1)

    if not ctx.json_mode:
        for warning in preflight.warnings:
            render_message(f"warning: {warning}", style="bold yellow")

    job = Job.create(sha=sha, branch=branch, target_names=target_names)
    job = ctx.queue.enqueue(job)
    job = _execute_job(
        ctx=ctx,
        job=job,
        config=config,
        dispatcher=dispatcher,
        mode=ValidationMode.FULL,
        fail_fast=False,
        resume_from=None,
    )

    if job.passed:
        merged = merge_pr(pr_info.number)
        if ctx.json_mode:
            ctx.output(
                "ship",
                {"pr": pr_info.number, "merged": merged, "run": job.to_dict(), "preflight": preflight.to_dict()},
            )
        else:
            if merged:
                render_message(f"PR #{pr_info.number} merged. All green.", style="bold green")
            else:
                render_message(f"All green but merge failed for PR #{pr_info.number}", style="bold yellow")
    else:
        if ctx.json_mode:
            ctx.output(
                "ship",
                {"pr": pr_info.number, "merged": False, "run": job.to_dict(), "preflight": preflight.to_dict()},
            )
        else:
            render_message(f"Validation failed. PR #{pr_info.number} not merged.", style="bold red")
            sys.exit(1)


@main.command()
@click.option("--dry-run", is_flag=True, default=True, help="Show what would be cleaned up")
@click.option("--apply", is_flag=True, help="Actually delete files")
@click.pass_obj
def cleanup(ctx: Context, dry_run: bool, apply: bool) -> None:
    """Clean up old logs, results, and bundles."""
    from shipyard.cleanup.retention import cleanup as do_cleanup

    state_dir = ctx.config.state_dir
    if apply:
        dry_run = False

    result = do_cleanup(state_dir, dry_run=dry_run)
    if ctx.json_mode:
        ctx.output("cleanup", result.to_dict())
    else:
        if not result.items:
            render_message("Nothing to clean up.", style="dim")
        else:
            for item in result.items:
                action = "would delete" if dry_run else "deleted"
                render_message(f"  {action}: {item.path} ({item.size_bytes} bytes)")
            if dry_run:
                render_message("\nRun with --apply to delete.", style="dim")


# ---- Helpers ----


def _git_sha() -> str | None:
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD"], text=True, stderr=subprocess.DEVNULL
        ).strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None


def _git_branch() -> str | None:
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "--abbrev-ref", "HEAD"],
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None


def _check_command(name: str, *args: str) -> dict[str, Any]:
    try:
        result = subprocess.run(
            [name, *args],
            capture_output=True,
            text=True,
            timeout=5,
        )
        version = result.stdout.strip().split("\n")[0] if result.stdout else ""
        if not version and result.stderr:
            version = result.stderr.strip().split("\n")[0]
        return {"ok": True, "version": version}
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return {"ok": False, "error": "not installed"}


def _resolve_validation(config: Config, mode: ValidationMode) -> dict[str, Any]:
    """Get validation config for the given mode.

    `[validation.contract]` and `[validation.prepared_state]` are
    declared alongside `[validation.default]`/`[validation.smoke]`,
    not inside them, so they live at the top of the `validation`
    subtable. Merge them into the returned mode-specific dict so
    downstream executors see a single unified view and don't have
    to know about the split.
    """
    validation = config.validation
    if mode == ValidationMode.SMOKE and "smoke" in validation:
        result = dict(validation["smoke"])
    elif "default" in validation:
        result = dict(validation["default"])
    else:
        result = dict(validation)

    # Lift top-level peers into the resolved dict. Mode-specific
    # overrides (if present inside `default`/`smoke`) still win.
    for peer_key in ("contract", "prepared_state"):
        if peer_key in validation and peer_key not in result:
            result[peer_key] = validation[peer_key]

    return result


def _resolve_target_validation(
    config: Config, target_name: str, base: dict[str, Any]
) -> dict[str, Any]:
    """Merge target-specific and platform-specific overrides into base validation."""
    result = dict(base)

    # Platform override
    target_config = config.targets.get(target_name, {})
    platform = target_config.get("platform", "")
    platform_os = platform.split("-")[0] if platform else ""
    overrides = config.validation.get("overrides", {})
    if platform_os in overrides:
        result.update(overrides[platform_os])

    # Target-specific override
    target_validation = target_config.get("validation", {})
    if target_validation:
        result.update(target_validation)

    return result


def _make_dispatcher(config: Config) -> ExecutorDispatcher:
    return ExecutorDispatcher(
        cloud_workflow=str(config.get("cloud.workflow", "ci.yml")),
        cloud_repo=config.get("cloud.repository"),
        cloud_poll_interval=float(config.get("cloud.poll_interval_secs", 15.0)),
        cloud_dispatch_settle_secs=float(config.get("cloud.dispatch_settle_secs", 30.0)),
    )


def _probe_target(
    target_config: dict[str, Any],
    dispatcher: ExecutorDispatcher,
) -> tuple[bool, str | None]:
    primary_backend = dispatcher.backend_name(target_config)
    if dispatcher.probe(target_config):
        return True, primary_backend

    for fallback in target_config.get("fallback", []):
        merged_config = {**target_config, **fallback}
        if dispatcher.probe(merged_config):
            return True, dispatcher.backend_name(merged_config)

    return False, None


def _execute_job(
    *,
    ctx: Context,
    job: Job,
    config: Config,
    dispatcher: ExecutorDispatcher,
    mode: ValidationMode,
    fail_fast: bool,
    resume_from: str | None,
) -> Job:
    job = job.start()
    ctx.queue.update(job)

    validation_config = _resolve_validation(config, mode)
    had_failure = False

    for name in job.target_names:
        if had_failure and fail_fast:
            job = job.with_result(TargetResult(
                target_name=name,
                platform=config.targets.get(name, {}).get("platform", "unknown"),
                status=TargetStatus.CANCELLED,
                backend="skipped",
                error_message="Skipped (earlier target failed, --fail-fast)",
            ))
            ctx.queue.update(job)
            continue

        target_config = dict(config.targets.get(name, {}))
        target_config["name"] = name
        log_path = str(config.state_dir / "logs" / job.id / f"{name}.log")
        backend_name = dispatcher.backend_name(target_config)

        running = TargetResult(
            target_name=name,
            platform=target_config.get("platform", "unknown"),
            status=TargetStatus.RUNNING,
            backend=backend_name,
            started_at=job.started_at,
            log_path=log_path,
        )
        job = job.with_result(running)
        ctx.queue.update(job)

        state: dict[str, Any] = {"job": job}

        def progress_callback(
            fields: dict[str, Any],
            *,
            target_name: str = name,
            default_running: TargetResult = running,
            progress_state: dict[str, Any] = state,
        ) -> None:
            current = progress_state["job"].results.get(target_name, default_running)
            progress_state["job"] = progress_state["job"].with_result(
                current.with_updates(
                    status=TargetStatus.RUNNING,
                    phase=fields.get("phase", current.phase),
                    last_output_at=fields.get("last_output_at", current.last_output_at),
                    last_heartbeat_at=fields.get("last_heartbeat_at", current.last_heartbeat_at),
                    quiet_for_secs=fields.get("quiet_for_secs", current.quiet_for_secs),
                    liveness=fields.get("liveness", current.liveness),
                )
            )
            ctx.queue.update(progress_state["job"])

        result = dispatcher.validate_target(
            sha=job.sha,
            branch=job.branch,
            target_config=target_config,
            validation_config=_resolve_target_validation(config, name, validation_config),
            log_path=log_path,
            progress_callback=progress_callback,
            resume_from=resume_from,
        )
        job = state["job"].with_result(result)
        ctx.queue.update(job)

        if not result.passed:
            had_failure = True

        if not ctx.json_mode:
            render_job(job)

    job = job.complete()
    ctx.queue.update(job)
    _record_evidence(ctx, job)
    return job


def _record_evidence(ctx: Context, job: Job) -> None:
    from shipyard.core.evidence import EvidenceRecord

    for name, result in job.results.items():
        if result.is_terminal:
            ctx.evidence.record(EvidenceRecord(
                sha=job.sha,
                branch=job.branch,
                target_name=name,
                platform=result.platform,
                status="pass" if result.passed else "fail",
                backend=result.backend,
                completed_at=result.completed_at or job.completed_at,  # type: ignore[arg-type]
                duration_secs=result.duration_secs,
                primary_backend=result.primary_backend,
                failover_reason=result.failover_reason,
                provider=result.provider,
                runner_profile=result.runner_profile,
            ))


def _wait_for_cloud_completion(repository: str | None, run_id: str) -> dict[str, Any]:
    while True:
        view = run_view(repository=repository, run_id=run_id)
        if view.get("status") == "completed":
            return view
        render_message(f"waiting for cloud run {run_id}...", style="dim")
        import time

        time.sleep(5)


if __name__ == "__main__":
    main()
