"""Shipyard CLI — the primary human and agent interface.

Every command supports --json for structured output. Human-readable
output is the default.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path
from typing import Any

import click

from shipyard import __version__
from shipyard.core.config import Config
from shipyard.core.evidence import EvidenceStore
from shipyard.core.job import Job, JobStatus, TargetResult, TargetStatus, ValidationMode
from shipyard.core.queue import Queue
from shipyard.executor.local import LocalExecutor
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


class Context:
    """Shared CLI context."""

    def __init__(self, json_mode: bool = False) -> None:
        self.json_mode = json_mode
        self._config: Config | None = None
        self._queue: Queue | None = None
        self._evidence: EvidenceStore | None = None

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
@click.pass_obj
def run(ctx: Context, targets: str | None, smoke: bool, fail_fast: bool, resume_from: str | None) -> None:
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

    # Execute (for now, just local — SSH/cloud come later)
    job = job.start()
    ctx.queue.update(job)

    executor = LocalExecutor()
    validation_config = _resolve_validation(config, mode)
    had_failure = False

    for name in job.target_names:
        # Fail-fast: skip remaining targets after a failure
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

        target_config = config.targets.get(name, {})
        target_config["name"] = name

        log_path = str(config.state_dir / "logs" / job.id / f"{name}.log")

        result = executor.validate(
            sha=sha,
            branch=branch,
            target_config=target_config,
            validation_config=_resolve_target_validation(config, name, validation_config),
            log_path=log_path,
            resume_from=resume_from,
        )
        job = job.with_result(result)
        ctx.queue.update(job)

        if not result.passed:
            had_failure = True

        if not ctx.json_mode:
            render_job(job)

    # Complete the job
    job = job.complete()
    ctx.queue.update(job)

    # Record evidence
    from shipyard.core.evidence import EvidenceRecord

    for name, result in job.results.items():
        if result.is_terminal:
            ctx.evidence.record(EvidenceRecord(
                sha=sha,
                branch=branch,
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

    if ctx.json_mode:
        ctx.output("run", {"run": job.to_dict()})
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

    # Probe targets (basic for now — just report config)
    targets_info: dict[str, dict[str, Any]] = {}
    for name, tconfig in ctx.config.targets.items():
        targets_info[name] = {
            "backend": tconfig.get("backend", "?"),
            "reachable": tconfig.get("backend") == "local",  # placeholder
        }

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

    ready = all(
        info.get("ok", False)
        for info in core.values()
    )

    if ctx.json_mode:
        ctx.output("doctor", {"ready": ready, "checks": checks})
    else:
        render_doctor(checks, ready)


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


@main.command()
@click.option("--base", default="main", help="Base branch for PR")
@click.pass_obj
def ship(ctx: Context, base: str) -> None:
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

    job = Job.create(sha=sha, branch=branch, target_names=target_names)
    job = ctx.queue.enqueue(job)
    job = job.start()
    ctx.queue.update(job)

    executor = LocalExecutor()
    validation_config = _resolve_validation(config, ValidationMode.FULL)

    for name in job.target_names:
        target_config = config.targets.get(name, {})
        target_config["name"] = name
        log_path = str(config.state_dir / "logs" / job.id / f"{name}.log")
        result = executor.validate(
            sha=sha, branch=branch, target_config=target_config,
            validation_config=_resolve_target_validation(config, name, validation_config),
            log_path=log_path,
        )
        job = job.with_result(result)
        ctx.queue.update(job)
        if not ctx.json_mode:
            render_job(job)

    job = job.complete()
    ctx.queue.update(job)

    # Record evidence
    from shipyard.core.evidence import EvidenceRecord

    for name, result in job.results.items():
        if result.is_terminal:
            ctx.evidence.record(EvidenceRecord(
                sha=sha, branch=branch, target_name=name,
                platform=result.platform,
                status="pass" if result.passed else "fail",
                backend=result.backend,
                completed_at=result.completed_at or job.completed_at,  # type: ignore[arg-type]
                duration_secs=result.duration_secs,
            ))

    if job.passed:
        merged = merge_pr(pr_info.number)
        if ctx.json_mode:
            ctx.output("ship", {"pr": pr_info.number, "merged": merged, "run": job.to_dict()})
        else:
            if merged:
                render_message(f"PR #{pr_info.number} merged. All green.", style="bold green")
            else:
                render_message(f"All green but merge failed for PR #{pr_info.number}", style="bold yellow")
    else:
        if ctx.json_mode:
            ctx.output("ship", {"pr": pr_info.number, "merged": False, "run": job.to_dict()})
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
    """Get validation config for the given mode."""
    validation = config.validation
    if mode == ValidationMode.SMOKE and "smoke" in validation:
        return validation["smoke"]
    if "default" in validation:
        return validation["default"]
    return validation


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
