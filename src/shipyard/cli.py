"""Shipyard CLI — the primary human and agent interface.

Every command supports --json for structured output. Human-readable
output is the default.
"""

from __future__ import annotations

# PyInstaller only bundles stdlib modules it can detect via static
# import analysis. Python's http.server -> socket.getfqdn() pulls in
# the IDNA codec lazily when the server binds to a hostname; if that
# codec isn't in the bundle the shipyard daemon crashes on start with
#   LookupError: unknown encoding: idna
# and the binary exits before writing any useful error to stderr.
# Explicit import forces PyInstaller to include encodings.idna in the
# --onefile binary. Keep this even if it looks unused.
import encodings.idna  # noqa: F401 — PyInstaller bundle primer
import json
import subprocess
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import TYPE_CHECKING, Any

import click

from shipyard import __version__
from shipyard.cloud.github import find_dispatched_run, run_view, workflow_dispatch
from shipyard.cloud.records import CloudRecordStore, CloudRunRecord
from shipyard.cloud.registry import default_workflow_key, discover_workflows, resolve_cloud_dispatch_plan
from shipyard.core.config import Config
from shipyard.core.evidence import EvidenceStore
from shipyard.core.job import Job, JobStatus, TargetResult, TargetStatus, ValidationMode
from shipyard.core.queue import Queue
from shipyard.core.ship_state import (
    DispatchedRun,
    ShipState,
    ShipStateStore,
    compute_policy_signature,
)
from shipyard.executor.dispatch import ExecutorDispatcher
from shipyard.governance.github import detect_repo_from_remote
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
from shipyard.preflight import (
    EXIT_BACKEND_UNREACHABLE,
    BackendUnreachableError,
    run_submission_preflight,
)
from shipyard.release_bot.setup import (
    ReleaseBotError,
    describe_state,
    detect_state,
    open_browser,
    plan_setup,
    render_pat_creation_url,
    set_secret,
    verify_token,
)
from shipyard.targets.warm_pool import (
    PoolEntry,
    WarmPool,
    compute_expires_at,
    default_pool_path,
    extract_warm_keepalive_seconds,
    is_backend_eligible,
    warm_host_key,
    warm_reuse_disabled_by_env,
)

if TYPE_CHECKING:
    from shipyard.ship.lane_policy import LanePolicy


class Context:
    """Shared CLI context."""

    def __init__(self, json_mode: bool = False) -> None:
        self.json_mode = json_mode
        self._config: Config | None = None
        self._queue: Queue | None = None
        self._evidence: EvidenceStore | None = None
        self._cloud_records: CloudRecordStore | None = None
        self._ship_state: ShipStateStore | None = None

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

    @property
    def ship_state(self) -> ShipStateStore:
        if self._ship_state is None:
            self._ship_state = ShipStateStore(self.config.state_dir / "ship")
        return self._ship_state


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
    help=(
        "Proceed even if a target's backend is unreachable. The lane "
        "is SKIPPED, NOT validated — a validation gap is recorded and "
        "printed loudly. Distinct from --skip-target, which is a "
        "deliberate skip not tied to backend health."
    ),
)
@click.option(
    "--skip-target",
    "skip_target",
    multiple=True,
    metavar="NAME",
    help=(
        "Deliberately skip a lane (not tied to backend reachability). "
        "Repeatable. Unlike --allow-unreachable-targets, this does not "
        "run a probe — it's the right flag when the user knows they "
        "don't want to validate that target at all."
    ),
)
@click.option(
    "--no-warm",
    is_flag=True,
    help=(
        "Force cold-start (skip warm-pool reuse even when the target "
        "opted in with warm_keepalive_seconds). Equivalent to setting "
        "SHIPYARD_NO_WARM_POOL=1 for this invocation."
    ),
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
    skip_target: tuple[str, ...],
    no_warm: bool,
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
            skip_targets=skip_target,
        )
    except BackendUnreachableError as exc:
        render_error(str(exc))
        sys.exit(EXIT_BACKEND_UNREACHABLE)
    except ValueError as exc:
        render_error(str(exc))
        sys.exit(1)

    # Honor --skip-target by dropping the skipped names from the job
    # target set. The preflight already recorded them in
    # `skipped_targets` for the output envelope.
    target_names = [n for n in target_names if n not in set(skip_target)]
    if not target_names:
        render_error("No targets remain after --skip-target filtering.")
        sys.exit(2)

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
        warm_disabled=no_warm,
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
    pending = queue.get_pending()
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
@click.option(
    "--release-chain",
    is_flag=True,
    help=(
        "Additionally dispatch auto-release.yml to verify the release-bot "
        "token actually works at actions/checkout. Catches PAT-scope and "
        "secret-drift failures before a real release attempt. Adds ~3-5 "
        "minutes to the run."
    ),
)
@click.option(
    "--runners",
    is_flag=True,
    help=(
        "Probe every configured SSH host and cloud target for "
        "reachability and the last heartbeat stamped on an in-flight "
        "or recently-finished ship. Surfaces dead/silent runners the "
        "chain would otherwise only notice at ship time."
    ),
)
@click.pass_obj
def doctor(ctx: Context, release_chain: bool, runners: bool) -> None:
    """Check environment, dependencies, and targets."""
    checks: dict[str, dict[str, Any]] = {}

    # Core tools
    core: dict[str, Any] = {}
    core["git"] = _check_command("git", "--version")
    core["ssh"] = _check_command("ssh", "-V")
    # PATH-shadow check: if a stale `shipyard` binary sits on a
    # lower-priority PATH entry, a reshuffle of PATH silently
    # downgrades the user to a pre-fix version. Surface this in
    # doctor so the user learns about it before it bites.
    core["shipyard-on-path"] = _check_shipyard_path_shadows()
    daemon_drift = _check_daemon_version_drift(ctx)
    if daemon_drift:
        core["daemon-version"] = daemon_drift
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

    # Release-bot token check — informational. The auto-release workflow
    # falls back to GITHUB_TOKEN when RELEASE_BOT_TOKEN isn't set, but
    # tags pushed by GITHUB_TOKEN don't trigger downstream workflows
    # (GitHub anti-infinite-loop safety), so the binary release pipeline
    # silently never fires. Surfacing this in doctor is the cheapest way
    # to keep new contributors out of that trap.
    release_token_section = _check_release_bot_token()
    if release_token_section:
        checks["Release pipeline"] = release_token_section

    # Tag drift — surfaces the #70 rollup gap. Always best-effort;
    # silently skipped outside a git repo or when no vN.N.N tag
    # exists yet.
    drift_section = _check_tag_drift()
    if drift_section:
        checks.setdefault("Release pipeline", {}).update(drift_section)

    if release_chain:
        chain_section = _check_release_chain()
        if chain_section:
            checks.setdefault("Release pipeline", {}).update(chain_section)

    if runners:
        runners_section = _check_runners(ctx)
        if runners_section:
            checks["Runners"] = runners_section

    # Ready = core tools healthy AND (no governance section declared
    # OR every governance check is ok). Informational entries (the
    # immutable-releases line) set `ok=True` specifically because
    # they can't lie — they report what the API exposes and nothing
    # more — so they're allowed to contribute to the ready rollup.
    # The release-token section is informational only — a missing
    # bot token doesn't break shipyard run/ship, it only breaks the
    # auto-release tag chain — so we DON'T include it in `ready`.
    ready = all(info.get("ok", False) for info in core.values()) and all(
        info.get("ok", False) for info in (governance_section or {}).values()
    )

    if ctx.json_mode:
        ctx.output("doctor", {"ready": ready, "checks": checks})
    else:
        render_doctor(checks, ready)


def _check_shipyard_path_shadows() -> dict[str, Any]:
    """Detect stale `shipyard` binaries shadowed on PATH.

    Pulp's agents discovered this concretely: a v0.11.0 binary under
    ``~/.local/bin/shipyard`` (from an old ``uv tool install shipyard``)
    was sitting second on PATH behind the pinned
    ``~/.pulp/bin/shipyard``. If PATH order had ever shifted, every
    ``shipyard ship`` would have silently run v0.11.0 with none of the
    recent fixes. This check enumerates every ``shipyard`` on PATH,
    asks each for its ``--version``, and flags the ones that aren't
    the one currently running.

    Returns a row suitable for the `Core` section of doctor:
    - ``ok=True`` when only one shipyard is on PATH, or every PATH
      entry resolves to the same real file (symlinks are fine).
    - ``ok=False`` when two or more distinct binaries are found,
      with `detail` naming every path + version + who wins.
    """
    import os as _os
    import shlex as _shlex
    import subprocess as _sp

    from shipyard import __version__ as _self_version

    paths = [p for p in _os.environ.get("PATH", "").split(_os.pathsep) if p]
    # Preserve first-seen order so we can show "winner: <first>" below.
    seen_bins: list[Path] = []
    seen_reals: set[Path] = set()
    for directory in paths:
        candidate = Path(directory) / "shipyard"
        if not candidate.is_file() or not _os.access(candidate, _os.X_OK):
            continue
        try:
            real = candidate.resolve()
        except OSError:
            real = candidate
        if real in seen_reals:
            continue
        seen_reals.add(real)
        seen_bins.append(candidate)

    if len(seen_bins) <= 1:
        return {
            "ok": True,
            "version": f"v{_self_version} (single binary on PATH)",
        }

    # Multiple distinct binaries — fetch each version. Keep the probe
    # fast so doctor stays snappy on repos with unrelated shipyard
    # checkouts in ~/Code.
    rows: list[str] = []
    for i, binary in enumerate(seen_bins):
        label = "WINS" if i == 0 else f"shadowed by {seen_bins[0]}"
        try:
            result = _sp.run(
                [str(binary), "--version"],
                capture_output=True, text=True, timeout=5,
            )
            version = (result.stdout or result.stderr).strip().split("\n")[0]
        except (_sp.SubprocessError, OSError) as exc:
            version = f"<unreadable: {exc}>"
        rows.append(f"    {binary} → {version}  [{label}]")

    detail = (
        "Multiple `shipyard` binaries on PATH — if PATH order ever "
        "shifts, a stale one could silently shadow the active one.\n"
        + "\n".join(rows)
        + "\n  Fix: remove the unused entries (e.g. "
        f"{_shlex.quote(str(seen_bins[-1]))}) or reorder PATH so the "
        f"intended binary stays first."
    )
    return {
        "ok": False,
        "version": f"v{_self_version} ({len(seen_bins)} binaries found)",
        "detail": detail,
    }


def _check_daemon_version_drift(ctx: Context) -> dict[str, Any] | None:
    """Flag when the running daemon is older than the CLI on PATH.

    The daemon is a long-lived process; a ``shipyard`` binary upgrade
    doesn't automatically restart it. Clients (GUI, ``shipyard wait``,
    anything that talks to the IPC socket) may then send newer message
    types the running daemon doesn't understand — and the old daemon
    silently drops them. That's exactly the trap that consumed most of
    2026-04-22 and motivated this check.

    Returns None when the daemon isn't running (no drift to report).
    Returns an ``ok: True`` row when versions match. Returns an
    ``ok: False`` row with a one-line recovery hint otherwise.
    """
    from shipyard import __version__ as _self_version
    from shipyard.daemon.controller import read_daemon_status

    try:
        status = read_daemon_status(ctx.config.state_dir)
    except Exception:  # noqa: BLE001 — doctor is best-effort
        return None
    if status is None:
        return None
    daemon_version = status.get("shipyard_version")
    if not isinstance(daemon_version, str) or not daemon_version:
        # Pre-v0.26.0 daemon doesn't advertise its version in status.
        # That IS the drift signal: if the daemon were current, it'd
        # have populated the field. Surface it the same way we'd
        # surface a known-older version.
        return {
            "ok": False,
            "version": f"daemon: <unknown>   cli: v{_self_version}",
            "detail": (
                "The running daemon predates v0.26.0 and doesn't report "
                "its own version. Run `shipyard daemon stop` to let the "
                "next GUI launch (or `shipyard daemon start`) spawn a "
                "fresh daemon from the current binary."
            ),
        }
    if daemon_version == _self_version:
        return {
            "ok": True,
            "version": f"daemon v{daemon_version} matches CLI",
        }
    return {
        "ok": False,
        "version": f"daemon: v{daemon_version}   cli: v{_self_version}",
        "detail": (
            "The running daemon is an older build than the CLI on "
            "PATH. New IPC message types (e.g. ship-state-list, added "
            "in v0.25.0) will be silently dropped. Run "
            "`shipyard daemon stop` to let the next GUI launch spawn "
            "a fresh daemon from the current binary."
        ),
    }


def _check_runners(ctx: Context) -> dict[str, Any] | None:
    """Probe configured SSH runners for reachability.

    Uses the same `run_probe` machinery as the ship preflight, so a
    doctor result that says "reachable" means a real `shipyard ship`
    will also succeed, and a "unreachable" here surfaces the same
    classified category agents see at ship time (auth / host_key /
    network / timeout / configuration). Fixed in #119 — the prior
    implementation ran a bare `true` on the remote which fails in
    cmd.exe on Windows SSH hosts even when the host is reachable.

    Returns None when the config has no SSH targets.
    """
    from shipyard.executor.ssh import run_probe

    ssh_targets = {
        name: t
        for name, t in ctx.config.targets.items()
        if t.get("backend") == "ssh"
    }
    if not ssh_targets:
        return None

    rows: dict[str, Any] = {}
    for name, target in ssh_targets.items():
        host = target.get("host")
        target_config = dict(target)
        target_config["name"] = name
        if not host:
            rows[name] = {
                "ok": False,
                "version": "misconfigured",
                "detail": "target has no `host` field",
            }
            continue

        diag = run_probe(target_config, remote_cmd=["echo", "ok"])
        if diag["reachable"]:
            rows[name] = {
                "ok": True,
                "version": f"reachable ({host})",
            }
        else:
            detail = f"ssh {host!r}: {diag.get('last_error') or 'unreachable'}"
            category = diag.get("category")
            if category:
                detail = f"{detail} [category={category}]"
            rows[name] = {
                "ok": False,
                "version": "unreachable",
                "detail": detail,
            }

    return rows


def _check_release_bot_token() -> dict[str, Any] | None:
    """Probe whether RELEASE_BOT_TOKEN is configured on the active repo.

    Returns None when the repo can't be detected or `gh` can't read the
    secret list (no auth, missing scope) — silent skip rather than
    blocking the rest of doctor. When the secret is present, returns ok=True.
    When it's missing, returns ok=False with the full setup pointer so the
    user has zero ambiguity about what to do.

    Uses `--paginate` so repos with many secrets (>30, the default page
    size) don't produce a false `missing` when RELEASE_BOT_TOKEN is on a
    later page. The setup recipe lives in `detail` — not `note` — because
    `render_doctor()` in output/human.py prints detail/version/error but
    silently drops `note`. Codex P2 on #39.
    """
    repo = detect_repo_from_remote()
    if repo is None:
        return None

    try:
        result = subprocess.run(
            ["gh", "api", f"repos/{repo.slug}/actions/secrets",
             "--paginate",
             "--jq", ".secrets[].name"],
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        return None

    if result.returncode != 0:
        # gh missing, not authed, or no actions:read scope. Keep silent;
        # `gh` health is already covered by the Cloud providers section.
        # A genuinely empty (zero-secret) repo still returns 0 with empty
        # stdout — that IS the bootstrap case we want to flag, not skip.
        return None

    secrets = {line.strip() for line in result.stdout.splitlines() if line.strip()}
    if "RELEASE_BOT_TOKEN" in secrets:
        return {
            "RELEASE_BOT_TOKEN": {
                "ok": True,
                "version": "configured",
                "detail": (
                    "auto-release.yml will use this for tag pushes; "
                    "downstream release.yml fires on its own."
                ),
            }
        }

    return {
        "RELEASE_BOT_TOKEN": {
            "ok": False,
            "version": "missing",
            "detail": (
                f"Auto-release will fall back to GITHUB_TOKEN; tag pushes won't "
                f"trigger release.yml. Fix: github.com → Settings → Developer "
                f"settings → Personal access tokens → Fine-grained tokens → "
                f"Generate. Repo access: only {repo.slug}. Permission: "
                f"Contents=Read and write. Then github.com/{repo.slug}/settings/"
                f"secrets/actions → New repository secret named RELEASE_BOT_TOKEN. "
                f"See RELEASING.md for the full walkthrough."
            ),
        }
    }


def _check_tag_drift(
    *, warn_threshold: int = 3
) -> dict[str, Any] | None:
    """Count CLI-surface commits on HEAD since the latest vN.N.N tag.

    Reports `no-drift` when count == 0, `tag-drift` (ok=False) when
    count >= warn_threshold. Between 1 and threshold-1 returns a
    quiet informational entry. Returns None if git calls fail (not
    a git repo, no tags, etc.) so doctor stays non-blocking.

    Closes the rollup-gap half of #70: even when auto_apply_patch
    is off (or the gate was bypassed for other reasons), maintainers
    see the drift explicitly rather than finding it via "why is
    production still on the old version?".
    """
    try:
        latest_tag = subprocess.run(
            ["git", "describe", "--tags", "--abbrev=0",
             "--match", "v[0-9]*"],
            capture_output=True, text=True, timeout=10,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        return None
    if latest_tag.returncode != 0 or not latest_tag.stdout.strip():
        return None
    tag = latest_tag.stdout.strip()
    try:
        count_out = subprocess.run(
            ["git", "rev-list", "--count",
             f"{tag}..HEAD", "--", "src/shipyard/"],
            capture_output=True, text=True, timeout=10,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        return None
    if count_out.returncode != 0:
        return None
    try:
        count = int(count_out.stdout.strip())
    except ValueError:
        return None

    if count == 0:
        return {
            "tag_drift": {
                "ok": True,
                "version": f"up-to-date ({tag})",
                "detail": "No CLI-surface commits since the latest tag.",
            }
        }
    if count < warn_threshold:
        return {
            "tag_drift": {
                "ok": True,
                "version": f"{count} commit(s) ahead of {tag}",
                "detail": (
                    "A release would pick up these changes. Under threshold "
                    f"({warn_threshold}), so advisory only."
                ),
            }
        }
    return {
        "tag_drift": {
            "ok": False,
            "version": f"{count} commits ahead of {tag}",
            "detail": (
                "User-visible changes have accumulated without a release. "
                "Either bump via `shipyard pr`, or enable `auto_apply_patch` "
                "in scripts/versioning.json for the CLI surface so future "
                "fix-only PRs auto-bump patch. See issue #70."
            ),
        }
    }


def _check_release_chain() -> dict[str, Any] | None:
    """Probe the release-bot token by dispatching auto-release.yml.

    Returns a doctor-shaped section keyed `release_chain`. The
    dispatched workflow itself exits cleanly when no version moved,
    so a conclusion of "success" here is proof that actions/checkout
    accepted *some* token — we also check that the PAT is actually
    present to avoid reporting "checkout-ok" when auto-release.yml
    silently fell back to GITHUB_TOKEN (#52 P2).
    """
    slug = _detect_repo_slug_or_empty()
    if not slug:
        return None
    # Establish whether RELEASE_BOT_TOKEN is present *before* dispatch.
    # The workflow's `${{ secrets.RELEASE_BOT_TOKEN || secrets.GITHUB_TOKEN }}`
    # fallback means a missing PAT still produces a "success" workflow
    # run — success alone isn't proof the bot token works.
    #
    # Three-state logic (#55 P2): _check_release_bot_token() returns
    # None when the secret listing is unreadable (auth/scope issues),
    # not when the secret is missing. Those two cases must be handled
    # distinctly — we can't cry "fallback-token" when we honestly
    # don't know what the repo's secret state is.
    token_section = _check_release_bot_token()
    if token_section is None:
        secret_state = "unknown"
    elif token_section.get("RELEASE_BOT_TOKEN", {}).get("ok") is True:
        secret_state = "present"
    else:
        secret_state = "missing"
    try:
        conclusion = verify_token(slug)
    except ReleaseBotError as exc:
        return {
            "release_chain": {
                "ok": False,
                "version": "dispatch-failed",
                "detail": exc.message + (f" {exc.detail}" if exc.detail else ""),
            }
        }
    if conclusion == "success":
        if secret_state == "present":
            return {
                "release_chain": {
                    "ok": True,
                    "version": "checkout-ok",
                    "detail": (
                        "auto-release.yml dispatched and completed; "
                        "actions/checkout accepted RELEASE_BOT_TOKEN."
                    ),
                }
            }
        if secret_state == "missing":
            return {
                "release_chain": {
                    "ok": False,
                    "version": "fallback-token",
                    "detail": (
                        "auto-release.yml succeeded but RELEASE_BOT_TOKEN is "
                        "missing — checkout used the GITHUB_TOKEN fallback. "
                        "Tag pushes from that token won't trigger "
                        "release.yml, so binary releases still won't ship. "
                        "Set the secret via `shipyard release-bot setup`."
                    ),
                }
            }
        # secret_state == "unknown": the workflow succeeded but we
        # couldn't probe the secret. Use a distinct `version` string
        # so the human doctor output (which prefers version to
        # detail) surfaces the uncertainty rather than reading as a
        # clean green check (#56 P2). ok=False because we can't
        # honestly rubber-stamp an unverified PAT.
        return {
            "release_chain": {
                "ok": False,
                "version": "checkout-ok-unverified",
                "detail": (
                    "auto-release.yml dispatched and completed, but we "
                    "could not probe whether RELEASE_BOT_TOKEN or the "
                    "GITHUB_TOKEN fallback was used — gh secret listing "
                    "unavailable in this environment. Re-run with "
                    "authenticated gh to get a definitive verdict."
                ),
            }
        }
    return {
        "release_chain": {
            "ok": False,
            "version": conclusion,
            "detail": (
                "auto-release.yml did not conclude success. Most likely: "
                "the stored token's PAT scope excludes this repo, or the "
                "stored value drifted. Run `shipyard release-bot status` "
                "for a non-destructive diagnosis; `shipyard release-bot "
                "setup --reconfigure` to re-paste."
            ),
        }
    }


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

    # In JSON mode, suppress human diagnostic output so the final
    # `ship` envelope is the only thing on stdout. Machine consumers
    # get valid JSON start-to-finish; human callers see the
    # progress messages as before.
    def _human(msg: str, style: str = "") -> None:
        if not ctx.json_mode:
            render_message(msg, style=style)

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
        _human(
            f"warning: --base {base} does not exist on origin and the "
            f"repo could not be detected from the git remote; skipping "
            f"auto-create-base.",
            style="bold yellow",
        )
        return

    gov = load_governance_config(ctx.config)
    rules = resolve_branch_rules(gov, base)
    _human(f"Creating base branch '{base}' from 'main' + applying governance rules…")
    result = create_branch_and_apply_rules(
        repo=repo, branch=base, base_branch="main", rules=rules,
    )
    if result.status == BranchCreateStatus.RULES_APPLIED or result.status == BranchCreateStatus.CREATED:
        _human(f"  ✓ {result.message}", style="green")
    elif result.status == BranchCreateStatus.ALREADY_EXISTS:
        _human(f"  ℹ {result.message}")
    else:
        _human(f"  ✗ {result.message}", style="bold red")
        _human(
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
@click.option(
    "--require-sha",
    default=None,
    metavar="SHA",
    help=(
        "Refuse to dispatch unless the remote ref currently resolves to "
        "this SHA. Guards against dispatching before a local force-push "
        "has propagated — the dispatched workflow runs against GitHub's "
        "view of the branch, not yours. Pass 'HEAD' to require the "
        "current local HEAD."
    ),
)
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
    require_sha: str | None,
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

    # We intentionally defer the --require-sha SHA comparison until
    # AFTER plan resolution so we can check the dispatch repository
    # — which may differ from the local origin (e.g., dispatching a
    # consumer-project workflow from a fork clone). Resolving the
    # plan here so the check uses plan.repository is #54 P1's fix.
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

    if require_sha is not None:
        expected = _resolve_expected_sha(require_sha)
        if not expected:
            render_error(
                f"Could not resolve --require-sha value '{require_sha}'. "
                "Pass an explicit 40-char SHA or 'HEAD'."
            )
            sys.exit(1)
        # Compare against the dispatch repository — NOT the local
        # origin (#54 P1). A consumer project running shipyard can
        # dispatch workflows to a different GitHub repo than the one
        # it's checked out from; validating against origin would be
        # checking unrelated history.
        dispatch_repo = plan.repository or _detect_repo_slug_or_empty()
        if not dispatch_repo:
            render_error(
                "--require-sha couldn't determine the dispatch repository."
            )
            sys.exit(1)
        remote_sha = _remote_ref_sha(dispatch_repo, plan.ref)
        if remote_sha is None:
            render_error(
                f"Could not read remote SHA for {dispatch_repo}@{plan.ref}."
            )
            sys.exit(1)
        if remote_sha != expected:
            render_error(
                f"Stale dispatch refused: expected {expected[:12]} but "
                f"{dispatch_repo}@{plan.ref} is at {remote_sha[:12]}. "
                "Push the expected commit, or re-run without --require-sha."
            )
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


@cloud.command("retarget")
@click.option("--pr", type=int, required=True, help="PR number")
@click.option(
    "--target",
    required=True,
    help=(
        "Target/job name to retarget. Matched against the job name "
        "in the workflow run (case-insensitive substring)."
    ),
)
@click.option(
    "--provider",
    required=True,
    help="New runner provider (e.g. namespace, github-hosted).",
)
@click.option(
    "--workflow",
    default=None,
    help="Workflow key (default: the same default `cloud run` uses).",
)
@click.option(
    "--dry-run/--apply",
    "dry_run",
    default=True,
    help=(
        "Dry-run by default. Shows the job that would be cancelled "
        "and the dispatch that would be issued. --apply executes."
    ),
)
@click.pass_obj
def cloud_retarget(
    ctx: Context,
    pr: int,
    target: str,
    provider: str,
    workflow: str | None,
    dry_run: bool,
) -> None:
    """Move one target on an in-flight PR to a new runner provider.

    Mid-batch provider switching: say you discover ten minutes into
    a ten-PR drain that Namespace macOS is much faster than
    GitHub-hosted. You want to flip the mac lane without tearing
    down every PR's Linux/Windows jobs that already passed.

    What this command does:
      1. Find the latest workflow run for this PR.
      2. Locate a job whose name matches --target (substring, case-
         insensitive). Cancel that specific job.
      3. Dispatch a new workflow_dispatch with `runner_provider=
         <provider>`, which starts a fresh run against the same ref.

    Known limitation: step 3 starts a NEW workflow run. If the
    target workflow doesn't support a per-target filter input
    (`only_target`, `targets`, …), the other targets will also
    re-run in that fresh workflow — but their PRIOR runs' pass/
    fail status is preserved on the PR's check rollup. Pulp-style
    workflows with a `resolve-provider` matrix step are the ideal
    case: each target is a separate job keyed on provider, so the
    new run can reuse cached artifacts and the old targets'
    statuses persist.

    Scope by workflow via --workflow. Dry-run by default.
    """

    workflows = discover_workflows()
    workflow_key = workflow or default_workflow_key(ctx.config, workflows)
    if not workflow_key:
        render_error("No workflows discovered")
        sys.exit(1)

    # Two-phase plan resolution (#67 P1):
    # Phase A — resolve a placeholder plan just to learn which repo
    #   to dispatch against. The ref is a best-effort hint from the
    #   local checkout; we do NOT trust it for the actual dispatch.
    # Phase B — after we fetch the PR's true headRefName from the
    #   dispatch repo, re-resolve the plan with that authoritative
    #   ref so `plan.ref` matches the branch whose jobs we
    #   cancelled. Without this, cross-repo/fork callers could
    #   cancel jobs on the right branch but dispatch against the
    #   wrong one (often the local branch or literal "HEAD").
    local_slug = _detect_repo_slug_or_empty()
    provisional_ref = _git_branch() or "HEAD"
    if local_slug:
        probe = _pr_fetch(local_slug, pr)
        if probe is not None:
            provisional_ref = probe.get("headRefName") or provisional_ref

    try:
        plan = resolve_cloud_dispatch_plan(
            config=ctx.config,
            workflows=workflows,
            workflow_key=workflow_key,
            ref=provisional_ref,
            provider_override=provider,
        )
    except ValueError as exc:
        render_error(f"Could not plan dispatch: {exc}")
        sys.exit(1)

    dispatch_repo = plan.repository or local_slug
    if not dispatch_repo:
        render_error(
            "Couldn't determine dispatch repository. Set cloud.repository "
            "in .shipyard/config.toml or run from within a git clone."
        )
        sys.exit(1)

    # Fetch the PR against the dispatch repo to resolve the
    # authoritative head ref — the branch whose jobs we'll cancel
    # and which the fresh dispatch must target.
    pr_state = _pr_fetch(dispatch_repo, pr)
    if pr_state is None:
        render_error(
            f"PR #{pr}: could not fetch state in {dispatch_repo} via gh."
        )
        sys.exit(1)
    head_ref = pr_state.get("headRefName")
    if not head_ref:
        render_error(f"PR #{pr}: no headRefName in gh response.")
        sys.exit(1)

    # Re-resolve the plan with the authoritative head_ref so
    # plan.ref lines up with the branch we're operating on.
    if head_ref != provisional_ref:
        try:
            plan = resolve_cloud_dispatch_plan(
                config=ctx.config,
                workflows=workflows,
                workflow_key=workflow_key,
                ref=head_ref,
                provider_override=provider,
            )
        except ValueError as exc:
            render_error(f"Could not re-plan with dispatch-repo ref: {exc}")
            sys.exit(1)

    # Find the latest run for this workflow on the PR's branch, in
    # the dispatch repo.
    run_info = _latest_workflow_run_for_branch(
        dispatch_repo, workflow_key_to_file(workflows, workflow_key),
        head_ref,
    )
    if run_info is None:
        render_error(
            f"No workflow runs found for {workflow_key} on "
            f"{dispatch_repo}@{head_ref}. Dispatch first, then retarget."
        )
        sys.exit(1)

    matching_jobs = _find_matching_jobs(
        dispatch_repo, int(run_info["databaseId"]), target
    )
    if not matching_jobs:
        render_error(
            f"No jobs matching '{target}' in run "
            f"{run_info['databaseId']}."
        )
        sys.exit(1)

    # Single JSON envelope per invocation (#66 P2): accumulate the
    # data and emit once at the end, either as `plan` (dry-run) or
    # `applied` (--apply). Previously this emitted `plan` *and*
    # `applied` back-to-back on --apply, producing two concatenated
    # JSON documents that json.loads() can't parse.
    payload: dict[str, Any] = {
        "pr": pr,
        "head_ref": head_ref,
        "repo": dispatch_repo,
        "workflow_key": workflow_key,
        "run_id": run_info["databaseId"],
        "matching_jobs": [
            {"id": j["databaseId"], "name": j["name"]}
            for j in matching_jobs
        ],
        "new_provider": provider,
        "dry_run": dry_run,
    }

    if not ctx.json_mode:
        render_message(
            f"Retarget plan for PR #{pr} ({dispatch_repo}):"
        )
        render_message(f"  workflow:    {workflow_key}")
        render_message(f"  ref:         {head_ref}")
        render_message(f"  prior run:   {run_info['databaseId']}")
        render_message(f"  target:      {target}")
        render_message(f"  new provider: {provider}")
        render_message(
            f"  matching jobs ({len(matching_jobs)}):"
        )
        for j in matching_jobs:
            render_message(f"    - {j['name']} (job id {j['databaseId']})")

    if dry_run:
        if ctx.json_mode:
            ctx.output("cloud.retarget", {"event": "plan", **payload})
        else:
            render_message(
                "\nDry-run. Re-run with --apply to cancel + redispatch.",
                style="dim",
            )
        return

    # Cancel matching jobs in the dispatch repo. gh supports
    # `gh api -X POST /repos/:owner/:repo/actions/jobs/:job_id/cancel`.
    cancelled: list[int] = []
    for j in matching_jobs:
        ok = _cancel_workflow_job(dispatch_repo, int(j["databaseId"]))
        if ok:
            cancelled.append(int(j["databaseId"]))
    if not cancelled:
        render_error(
            "Couldn't cancel the matching job(s). Your gh token may "
            "lack `actions:write` scope. Cancel manually in the UI, "
            "then re-run this with --apply to redispatch."
        )
        sys.exit(1)

    try:
        workflow_dispatch(
            repository=plan.repository,
            workflow_file=plan.workflow.file,
            ref=plan.ref,
            fields=plan.dispatch_fields,
        )
    except (subprocess.CalledProcessError, TimeoutError) as exc:
        render_error(f"workflow_dispatch failed: {exc}")
        sys.exit(1)

    # Mirror the retarget into ShipState so watch/auto-merge see the
    # new provider+run_id instead of the stale row (#110). Best-effort
    # — if the state file is absent (retarget can be invoked against a
    # PR that never went through shipyard ship), we skip the mirror
    # rather than fabricate a ShipState on the retarget path.
    new_run_id: str | None = None
    try:
        discovered = find_dispatched_run(
            repository=plan.repository,
            workflow_file=plan.workflow.file,
            ref=plan.ref,
        )
        new_run_id = str(discovered.get("databaseId") or "") or None
    except (subprocess.CalledProcessError, TimeoutError):
        new_run_id = None

    ship_state = ctx.ship_state.get(pr)
    if ship_state is not None:
        advisory_policy = _resolve_lane_policy_for_ship(ctx.config)
        now = datetime.now(timezone.utc)
        replacement = DispatchedRun(
            target=target,
            provider=plan.provider,
            run_id=new_run_id or f"pending-{target}",
            status="queued",
            started_at=now,
            updated_at=now,
            attempt=ship_state.attempt,
            required=advisory_policy.is_required(target),
        )
        # Drop prior rows for this target, append the new one. The
        # upsert helper keys on (target, run_id) so it would leave a
        # stale row behind when the run_id differs — retarget semantics
        # are "this lane has moved providers," which means the old row
        # is obsolete, not coexistent.
        ship_state.dispatched_runs = [
            r for r in ship_state.dispatched_runs if r.target != target
        ]
        ship_state.append_run(replacement)
        ctx.ship_state.save(ship_state)

    if ctx.json_mode:
        ctx.output(
            "cloud.retarget",
            {
                "event": "applied",
                **payload,
                "cancelled_job_ids": cancelled,
                "new_dispatch": plan.to_dict(),
                "new_run_id": new_run_id,
            },
        )
    else:
        render_message(
            f"✓ Cancelled {len(cancelled)} job(s); dispatched fresh "
            f"run with provider={provider}.",
            style="bold green",
        )


@cloud.command("add-lane")
@click.option("--pr", type=int, required=True, help="PR number")
@click.option(
    "--target",
    required=True,
    help=(
        "Target/lane name to add (e.g. 'windows'). Must not already be "
        "present in the PR's ShipState.dispatched_runs."
    ),
)
@click.option(
    "--provider",
    default=None,
    help=(
        "Runner provider for the new lane. Defaults to the target's "
        "configured provider (cloud.workflows.<key>.provider) or "
        "cloud.provider."
    ),
)
@click.option(
    "--workflow",
    default=None,
    help="Workflow key (default: the same default `cloud run` uses).",
)
@click.option(
    "--dry-run/--apply",
    "dry_run",
    default=True,
    help=(
        "Dry-run by default. Shows the workflow that would be "
        "dispatched. --apply executes."
    ),
)
@click.pass_obj
def cloud_add_lane(
    ctx: Context,
    pr: int,
    target: str,
    provider: str | None,
    workflow: str | None,
    dry_run: bool,
) -> None:
    """Add a new lane to an in-flight PR without touching other lanes.

    Sibling to `cloud retarget`. Retarget swaps *existing* lane to a
    new provider; add-lane appends a *new* lane entirely. Use case:
    started shipping with [macos, linux], then realized you wanted
    windows too. Without add-lane, the only option is cancel +
    re-dispatch the full matrix.

    What this does:
      1. Loads the PR's ShipState. Refuses if missing.
      2. Refuses if the ship is already past dispatch phase
         (evidence_snapshot shows terminal verdict — adding a lane
         after merge has been issued is nonsensical).
      3. No-ops with a clear message if the target is already in
         dispatched_runs (idempotent).
      4. Dispatches the single workflow for that target/provider.
      5. Appends a DispatchedRun to ShipState, saves atomically. The
         watch loop then sees a new in-flight run and joins it into
         the overall verdict.

    Dry-run by default. --apply executes.
    """

    # ── Load ShipState — no state means no in-flight ship ───────
    ship_state = ctx.ship_state.get(pr)
    if ship_state is None:
        render_error(
            f"No in-flight ship state for PR #{pr}. add-lane operates "
            "on a running ship — dispatch one with `shipyard ship` "
            "first."
        )
        sys.exit(1)

    # ── Refuse if the ship is already terminal ──────────────────
    # Adding a lane after merge has been issued is meaningless; the
    # PR is done. _ship_terminal_verdict returns True/False for
    # terminal, None for still-in-flight.
    if _ship_terminal_verdict(ship_state) is not None:
        render_error(
            f"PR #{pr}: ship is already past dispatch phase "
            f"(evidence={ship_state.evidence_snapshot}). "
            "Can't add a lane after merge has been issued."
        )
        sys.exit(1)

    # ── Idempotency: target already tracked ─────────────────────
    if ship_state.has_target(target):
        existing = ship_state.get_run(target)
        msg = (
            f"Target '{target}' is already tracked in PR #{pr}'s "
            f"dispatched_runs"
        )
        if existing is not None:
            msg += (
                f" (provider={existing.provider}, "
                f"run_id={existing.run_id}, status={existing.status})"
            )
        msg += ". No-op."
        noop_payload: dict[str, Any] = {
            "pr": pr,
            "target": target,
            "already_tracked": True,
            "existing_run": existing.to_dict() if existing else None,
            "dry_run": dry_run,
        }
        if ctx.json_mode:
            ctx.output(
                "cloud.add-lane", {"event": "noop", **noop_payload}
            )
        else:
            render_message(msg, style="dim")
        return

    # ── Resolve workflow + dispatch plan ────────────────────────
    workflows = discover_workflows()
    workflow_key = workflow or default_workflow_key(ctx.config, workflows)
    if not workflow_key:
        render_error("No workflows discovered")
        sys.exit(1)

    try:
        plan = resolve_cloud_dispatch_plan(
            config=ctx.config,
            workflows=workflows,
            workflow_key=workflow_key,
            ref=ship_state.branch,
            provider_override=provider,
        )
    except ValueError as exc:
        render_error(f"Could not plan dispatch: {exc}")
        sys.exit(1)

    dispatch_repo = plan.repository or ship_state.repo
    resolved_provider = plan.provider

    payload: dict[str, Any] = {
        "pr": pr,
        "branch": ship_state.branch,
        "repo": dispatch_repo,
        "workflow_key": workflow_key,
        "target": target,
        "provider": resolved_provider,
        "dispatch_fields": dict(plan.dispatch_fields),
        "dry_run": dry_run,
    }

    if not ctx.json_mode:
        render_message(f"Add-lane plan for PR #{pr} ({dispatch_repo}):")
        render_message(f"  workflow:  {workflow_key}")
        render_message(f"  ref:       {ship_state.branch}")
        render_message(f"  target:    {target}")
        render_message(f"  provider:  {resolved_provider}")
        if plan.dispatch_fields:
            render_message("  dispatch_fields:")
            for k, v in plan.dispatch_fields.items():
                render_message(f"    {k}={v}")

    if dry_run:
        if ctx.json_mode:
            ctx.output("cloud.add-lane", {"event": "plan", **payload})
        else:
            render_message(
                "\nDry-run. Re-run with --apply to dispatch.",
                style="dim",
            )
        return

    # ── Dispatch and record ─────────────────────────────────────
    try:
        workflow_dispatch(
            repository=plan.repository,
            workflow_file=plan.workflow.file,
            ref=plan.ref,
            fields=plan.dispatch_fields,
        )
    except (subprocess.CalledProcessError, TimeoutError) as exc:
        render_error(f"workflow_dispatch failed: {exc}")
        sys.exit(1)

    # Best-effort: look up the dispatched run ID so the DispatchedRun
    # entry carries it. If discovery times out, we still record the
    # lane — the watch loop can backfill the run_id later.
    run_id: str = ""
    run_url: str | None = None
    try:
        discovered = find_dispatched_run(
            repository=plan.repository,
            workflow_file=plan.workflow.file,
            ref=plan.ref,
        )
        run_id = str(discovered.get("databaseId") or "")
        run_url = discovered.get("url")
    except (subprocess.CalledProcessError, TimeoutError):
        run_id = ""

    now = datetime.now(timezone.utc)
    advisory_policy = _resolve_lane_policy_for_ship(ctx.config)
    dispatched = DispatchedRun(
        target=target,
        provider=resolved_provider,
        run_id=run_id or f"pending-{target}",
        status="queued",
        started_at=now,
        updated_at=now,
        attempt=ship_state.attempt,
        required=advisory_policy.is_required(target),
    )
    ship_state.append_run(dispatched)
    ctx.ship_state.save(ship_state)

    applied_payload = {
        **payload,
        "run_id": dispatched.run_id,
        "run_url": run_url,
    }
    if ctx.json_mode:
        ctx.output(
            "cloud.add-lane",
            {"event": "applied", **applied_payload},
        )
    else:
        render_message(
            f"✓ Dispatched {target} on {resolved_provider} "
            f"(run_id={dispatched.run_id}). Appended to PR #{pr} "
            "ship state.",
            style="bold green",
        )


def _pr_fetch(repo_slug: str, pr: int) -> dict[str, Any] | None:
    try:
        result = subprocess.run(
            ["gh", "pr", "view", str(pr), "--repo", repo_slug,
             "--json", "headRefName,number,state"],
            capture_output=True, text=True, timeout=15,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        return None
    if result.returncode != 0:
        return None
    import json as _json

    try:
        return _json.loads(result.stdout)
    except _json.JSONDecodeError:
        return None


def workflow_key_to_file(
    workflows: dict[str, Any], workflow_key: str
) -> str:
    wf = workflows.get(workflow_key)
    if wf is None:
        raise click.ClickException(
            f"Unknown workflow key: {workflow_key}"
        )
    return getattr(wf, "file", None) or f"{workflow_key}.yml"


def _latest_workflow_run_for_branch(
    repo_slug: str, workflow_file: str, branch: str
) -> dict[str, Any] | None:
    try:
        result = subprocess.run(
            ["gh", "run", "list", "--repo", repo_slug,
             "--workflow", workflow_file, "--branch", branch,
             "--limit", "1",
             "--json", "databaseId,status,conclusion,createdAt"],
            capture_output=True, text=True, timeout=15,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        return None
    if result.returncode != 0:
        return None
    import json as _json

    try:
        arr = _json.loads(result.stdout)
    except _json.JSONDecodeError:
        return None
    return arr[0] if arr else None


def _find_matching_jobs(
    repo_slug: str, run_id: int, target: str
) -> list[dict[str, Any]]:
    try:
        result = subprocess.run(
            ["gh", "run", "view", str(run_id), "--repo", repo_slug,
             "--json", "jobs"],
            capture_output=True, text=True, timeout=30,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        return []
    if result.returncode != 0:
        return []
    import json as _json

    try:
        data = _json.loads(result.stdout)
    except _json.JSONDecodeError:
        return []
    jobs = data.get("jobs", [])
    needle = target.lower()
    return [
        j for j in jobs
        if needle in (j.get("name") or "").lower()
        and j.get("status") in ("queued", "in_progress")
    ]


def _cancel_workflow_job(repo_slug: str, job_id: int) -> bool:
    """Cancel a single job. Returns True on success, False on any failure."""
    try:
        result = subprocess.run(
            ["gh", "api", "-X", "POST",
             f"repos/{repo_slug}/actions/jobs/{job_id}/cancel"],
            capture_output=True, text=True, timeout=15,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        return False
    return result.returncode == 0


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
    help=(
        "Proceed even if a target's backend is unreachable. The lane "
        "is SKIPPED, NOT validated — a validation gap is recorded and "
        "printed loudly. Distinct from --skip-target."
    ),
)
@click.option(
    "--skip-target",
    "skip_target",
    multiple=True,
    metavar="NAME",
    help=(
        "Deliberately skip a lane (not tied to backend reachability). "
        "Repeatable. Use this when you know you don't want to validate "
        "the target at all; use --allow-unreachable-targets only when "
        "the target SHOULD be validated but the backend is down."
    ),
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
@click.option(
    "--resume/--no-resume",
    "resume",
    default=None,
    help=(
        "Resume an in-flight ship from a saved state file. On by default "
        "when a state file exists for the current PR; --no-resume forces a "
        "fresh dispatch. Refuses to resume on SHA drift or merge-policy "
        "change since the saved state was written."
    ),
)
@click.option(
    "--no-warm",
    is_flag=True,
    help=(
        "Force cold-start (skip warm-pool reuse for every target on this "
        "ship). Equivalent to setting SHIPYARD_NO_WARM_POOL=1."
    ),
)
@click.pass_obj
def ship(
    ctx: Context,
    base: str,
    allow_root_mismatch: bool,
    allow_unreachable_targets: bool,
    skip_target: tuple[str, ...],
    auto_create_base: bool | None,
    resume: bool | None,
    no_warm: bool,
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
        from shipyard.ship.pr_text import compose_pr_body, compose_pr_title

        pr_title = compose_pr_title(branch)
        pr_body = compose_pr_body(config=ctx.config)
        pr_info = create_pr(branch, base, pr_title, pr_body)
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

    # ── Durable ship-state: detect or create ────────────────────
    # A state file lets a future session resume after this process
    # dies (laptop closed, OS restart, agent crash). Only one active
    # state file exists per PR; on policy/SHA drift we refuse to
    # resume silently and force the user to decide.
    repo_slug = _detect_repo_slug_or_empty()
    required_platforms = _required_platforms_for_config(config)
    policy_sig = compute_policy_signature(
        required_platforms=required_platforms,
        target_names=target_names,
        mode="FULL",
    )
    ship_state_store = ctx.ship_state
    existing_state = ship_state_store.get(pr_info.number)
    resume_effective = _resolve_resume_mode(resume, existing_state)
    # Track the bumped attempt across --no-resume so the fresh
    # ShipState below reflects the monotonic counter rather than
    # resetting to attempt=1 (#109). None means "no prior attempt".
    carried_attempt: int | None = None
    if existing_state is not None:
        if resume_effective is False:
            # Explicit --no-resume: archive the stale state and start fresh.
            # archive_and_replace returns a new ShipState with attempt+1;
            # we previously discarded it and fell through to a fresh
            # ShipState(...) with the default attempt=1 (#109).
            replacement = ship_state_store.archive_and_replace(existing_state)
            carried_attempt = replacement.attempt
            existing_state = None
        else:
            drift = _detect_ship_state_drift(
                existing_state, current_sha=sha, current_policy=policy_sig
            )
            if drift is not None:
                render_error(
                    f"Refusing to resume: {drift}. Re-run with "
                    f"--no-resume to archive the stale state and dispatch fresh."
                )
                sys.exit(1)
            if not ctx.json_mode:
                render_message(
                    f"Resuming ship for PR #{pr_info.number} "
                    f"(attempt {existing_state.attempt}).",
                    style="dim",
                )

    if existing_state is None:
        existing_state = ShipState(
            pr=pr_info.number,
            repo=repo_slug,
            branch=branch,
            base_branch=base,
            head_sha=sha,
            policy_signature=policy_sig,
            pr_url=_pr_url(repo_slug, pr_info.number),
            pr_title=getattr(pr_info, "title", "") or "",
            commit_subject=_git_commit_subject(sha),
            # Carry forward the bumped attempt on --no-resume so the
            # counter stays monotonic across force-restarts (#109).
            attempt=carried_attempt if carried_attempt is not None else 1,
        )
        ship_state_store.save(existing_state)
    else:
        # Refresh human-context fields on every invocation so
        # force-push / title edits are reflected without a new attempt.
        existing_state.pr_url = (
            existing_state.pr_url or _pr_url(repo_slug, pr_info.number)
        )
        existing_state.pr_title = (
            getattr(pr_info, "title", "") or existing_state.pr_title
        )
        existing_state.commit_subject = (
            _git_commit_subject(sha) or existing_state.commit_subject
        )
        existing_state.touch()
        ship_state_store.save(existing_state)

    dispatcher = _make_dispatcher(config)
    try:
        preflight = run_submission_preflight(
            config,
            target_names=target_names,
            dispatcher=dispatcher,
            allow_root_mismatch=allow_root_mismatch,
            allow_unreachable_targets=allow_unreachable_targets,
            skip_targets=skip_target,
        )
    except BackendUnreachableError as exc:
        render_error(str(exc))
        sys.exit(EXIT_BACKEND_UNREACHABLE)
    except ValueError as exc:
        render_error(str(exc))
        sys.exit(1)

    if not ctx.json_mode:
        for warning in preflight.warnings:
            render_message(f"warning: {warning}", style="bold yellow")

    target_names = [n for n in target_names if n not in set(skip_target)]
    if not target_names:
        render_error("No targets remain after --skip-target filtering.")
        sys.exit(2)

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
        ship_state=existing_state,
        warm_disabled=no_warm,
    )

    if job.passed:
        merged = merge_pr(pr_info.number)
        # On terminal outcome, archive the state file so future
        # `list-stale` does not flag it.
        ship_state_store.archive(pr_info.number)
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
@click.option(
    "--ship-state",
    is_flag=True,
    help=(
        "Also prune aged ship-state files. Archived entries older than "
        "30 days are deleted; active entries older than 14 days whose "
        "PR is closed/merged on GitHub are deleted."
    ),
)
@click.pass_obj
def cleanup(ctx: Context, dry_run: bool, apply: bool, ship_state: bool) -> None:
    """Clean up old logs, results, bundles, and (opt-in) ship state."""
    from shipyard.cleanup.retention import cleanup as do_cleanup

    state_dir = ctx.config.state_dir
    if apply:
        dry_run = False

    result = do_cleanup(state_dir, dry_run=dry_run)
    ship_state_report: dict[str, Any] | None = None

    if ship_state:
        store = ctx.ship_state
        closed_prs = _gather_closed_prs(store) if not dry_run else None
        if dry_run:
            # In dry-run, compute the would-delete set without touching disk
            # by constructing a parallel in-memory copy of the store logic.
            ship_state_report = _preview_ship_state_prune(store)
        else:
            report = store.prune(
                active_days=14, archive_days=30, closed_prs=closed_prs
            )
            ship_state_report = report.to_dict()

    if ctx.json_mode:
        payload = result.to_dict()
        if ship_state_report is not None:
            payload["ship_state"] = ship_state_report
        ctx.output("cleanup", payload)
    else:
        if not result.items and not ship_state_report:
            render_message("Nothing to clean up.", style="dim")
        else:
            for item in result.items:
                action = "would delete" if dry_run else "deleted"
                render_message(f"  {action}: {item.path} ({item.size_bytes} bytes)")
            if ship_state_report is not None:
                for pr in ship_state_report.get("deleted_active", []):
                    action = "would delete" if dry_run else "deleted"
                    render_message(f"  {action}: ship state for PR #{pr}")
                for name in ship_state_report.get("deleted_archived", []):
                    action = "would delete" if dry_run else "deleted"
                    render_message(f"  {action}: archived ship state {name}")
            if dry_run:
                render_message("\nRun with --apply to delete.", style="dim")


def _gather_closed_prs(store: ShipStateStore) -> set[int]:
    """Query `gh` for PR state of every active ship-state file.

    Used during cleanup to decide which state files are safe to prune.
    A PR whose state cannot be determined (gh missing, network error,
    auth failure) is treated as *not closed* — we prefer to keep state
    over delete it.
    """
    closed: set[int] = set()
    for state in store.list_active():
        closed_status = _pr_is_closed(state.pr)
        if closed_status:
            closed.add(state.pr)
    return closed


def _pr_is_merged(pr: int) -> bool:
    """Return True only if `gh pr view` confirms the PR is MERGED.

    Any other outcome (gh missing, auth failure, timeout, PR open,
    PR closed-but-not-merged) returns False. Used by auto-merge to
    preserve idempotent success semantics after a prior tick's
    merge archived the local state.
    """
    try:
        result = subprocess.run(
            ["gh", "pr", "view", str(pr), "--json", "state"],
            capture_output=True, text=True, timeout=15,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return False
    if result.returncode != 0:
        return False
    import json as _json

    try:
        data = _json.loads(result.stdout)
    except _json.JSONDecodeError:
        return False
    return data.get("state") == "MERGED"


def _pr_is_closed(pr: int) -> bool:
    """Return True only if we confirm the PR is merged or closed."""
    try:
        result = subprocess.run(
            ["gh", "pr", "view", str(pr), "--json", "state"],
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return False
    if result.returncode != 0:
        return False
    import json as _json

    try:
        data = _json.loads(result.stdout)
    except _json.JSONDecodeError:
        return False
    return data.get("state") in ("MERGED", "CLOSED")


def _preview_ship_state_prune(store: ShipStateStore) -> dict[str, Any]:
    """Dry-run preview of what `store.prune()` would remove."""
    now = datetime.now(timezone.utc)
    archive_cutoff = now - timedelta(days=30)
    would_archived: list[str] = []
    for archive_path in store.list_archived():
        mtime = datetime.fromtimestamp(archive_path.stat().st_mtime, tz=timezone.utc)
        if mtime <= archive_cutoff:
            would_archived.append(archive_path.name)
    # We don't hit `gh` in dry-run — just show what the age filter
    # would match if the PR is also closed. The caller is expected
    # to run --apply for accurate active-file pruning.
    return {
        "deleted_active": [],
        "deleted_archived": would_archived,
        "total": len(would_archived),
        "note": "Active-file pruning is only computed during --apply.",
    }


@main.group(name="release-bot")
def release_bot_group() -> None:
    """Guided RELEASE_BOT_TOKEN provisioning and diagnosis."""


@release_bot_group.command("status")
@click.option(
    "--siblings",
    "siblings",
    multiple=True,
    metavar="OWNER/REPO",
    help=(
        "Other repos to probe for RELEASE_BOT_TOKEN (multi). Surfaces "
        "whether a shared PAT is already in use so setup can recommend "
        "expanding it instead of cutting a new one."
    ),
)
@click.pass_obj
def release_bot_status(ctx: Context, siblings: tuple[str, ...]) -> None:
    """Report RELEASE_BOT_TOKEN presence, drift, and recent failures.

    Never prints the secret value. Safe to run in CI logs.
    """
    slug = _detect_repo_slug_or_empty()
    if not slug:
        render_error("Can't detect owner/repo from git remote.")
        sys.exit(1)
    state = detect_state(slug, known_repos_hint=list(siblings))
    if ctx.json_mode:
        ctx.output(
            "release-bot:status",
            {
                "repo": state.repo_slug,
                "secret_present": state.secret_present,
                "secret_updated_at": (
                    state.secret_updated_at.isoformat()
                    if state.secret_updated_at
                    else None
                ),
                "last_auto_release_conclusion": state.last_auto_release_conclusion,
                "last_auto_release_error_signature": state.last_auto_release_error_signature,
                "other_repos_with_secret": list(state.other_repos_with_secret),
            },
        )
        return
    for line in describe_state(state):
        render_message(line)
    if (
        state.last_auto_release_error_signature == "auth"
        and state.secret_present
    ):
        render_message("")
        render_message(
            "Diagnosis: the stored token is being rejected by actions/checkout.",
            style="bold yellow",
        )
        render_message(
            "Either the PAT doesn't list this repo under 'Selected "
            "repositories', or the stored secret value is stale (drifted "
            "from the PAT you later edited). Run `shipyard release-bot "
            "setup --reconfigure` to fix.",
        )


@release_bot_group.command("setup")
@click.option(
    "--shared-name",
    default=None,
    help=(
        "Use this PAT name instead of the per-project default. Pick one "
        "name for every Shipyard consumer repo to rotate a single token."
    ),
)
@click.option(
    "--paste",
    is_flag=True,
    help=(
        "Skip the wizard and just paste a token value you already have. "
        "Useful when you regenerated a PAT elsewhere and want to sync the "
        "secret on this repo."
    ),
)
@click.option(
    "--siblings",
    multiple=True,
    metavar="OWNER/REPO",
    help="Probe these repos for an existing RELEASE_BOT_TOKEN.",
)
@click.option(
    "--verify/--no-verify",
    default=True,
    help=(
        "Dispatch a workflow run after setting the secret to confirm "
        "actions/checkout accepts it. Defaults on."
    ),
)
@click.option(
    "--reconfigure",
    is_flag=True,
    help="Treat the secret as unset even if present (forces a re-paste).",
)
@click.pass_obj
def release_bot_setup(
    ctx: Context,
    shared_name: str | None,
    paste: bool,
    siblings: tuple[str, ...],
    verify: bool,
    reconfigure: bool,
) -> None:
    """Walk through RELEASE_BOT_TOKEN setup with a live verification step.

    Honest about what it can't do: Shipyard cannot create a
    fine-grained PAT for you — GitHub has no such API. The wizard
    opens the right URL with scope hints pre-filled, prompts for
    the generated token via stdin, stores it as a secret, and then
    dispatches a workflow run to prove actions/checkout accepts it.
    """
    slug = _detect_repo_slug_or_empty()
    if not slug:
        render_error("Can't detect owner/repo from git remote.")
        sys.exit(1)

    state = detect_state(slug, known_repos_hint=list(siblings))
    for line in describe_state(state):
        render_message(line)
    render_message("")

    if state.secret_present and not reconfigure and not paste:
        render_message(
            "RELEASE_BOT_TOKEN is already set. Pass --reconfigure to "
            "replace the stored value, or run `shipyard doctor "
            "--release-chain` to probe the current secret with a real "
            "actions/checkout step.",
            style="dim",
        )
        return

    plan = plan_setup(state, shared_name=shared_name)
    if not paste:
        owner, repo = slug.split("/", 1)
        pat_url = render_pat_creation_url(
            owner=owner, pat_name=plan.suggested_pat_name, repo=repo
        )
        render_message(f"Recommended PAT name: {plan.suggested_pat_name}")
        render_message(f"Rationale: {plan.reasoning}", style="dim")
        render_message("")
        render_message("Open this URL to create (or edit) the PAT:")
        render_message(f"  {pat_url}")
        render_message("")
        render_message("Required repository permissions:")
        render_message("  - Contents: Read and write")
        render_message("  - Metadata: Read-only (auto-added)")
        render_message(
            "  - Workflows: Read and write (only if the bot will commit "
            "changes under .github/workflows)"
        )
        render_message(
            "Include every Shipyard consumer repo under "
            "'Only select repositories' — fine-grained PATs are strict.",
            style="dim",
        )
        if open_browser(pat_url):
            render_message("(browser opened)", style="dim")

    render_message("")
    token = click.prompt(
        "Paste the token (input hidden)", hide_input=True, default="",
        show_default=False,
    ).strip()
    if not token:
        render_error("Empty token. Aborting.")
        sys.exit(1)

    try:
        set_secret(slug, token)
    except ReleaseBotError as exc:
        render_error(exc.message)
        if exc.detail:
            render_message(exc.detail, style="dim")
        sys.exit(1)
    render_message(
        f"✓ Stored RELEASE_BOT_TOKEN on {slug}.", style="bold green"
    )

    if not verify:
        return

    render_message("Dispatching auto-release.yml to verify checkout…")
    try:
        conclusion = verify_token(slug)
    except ReleaseBotError as exc:
        render_message(
            f"Verification dispatch failed: {exc.message}",
            style="bold yellow",
        )
        if exc.detail:
            render_message(exc.detail, style="dim")
        render_message(
            "The secret is stored. Verify manually with "
            "`shipyard release-bot status` after your next push to main."
        )
        return

    if conclusion == "success":
        render_message(
            "✓ actions/checkout accepted the token.",
            style="bold green",
        )
    else:
        render_message(
            f"Verification workflow concluded: {conclusion}.",
            style="bold yellow",
        )
        render_message(
            "Re-run `shipyard release-bot status` to inspect the "
            "failure signature — the common case is a PAT scope that "
            "excludes this repo.",
        )


@main.group(name="ship-state")
def ship_state_group() -> None:
    """Inspect and manage durable ship-state records."""


@ship_state_group.command("list")
@click.pass_obj
def ship_state_list(ctx: Context) -> None:
    """List active in-flight ship states (one per PR)."""
    states = ctx.ship_state.list_active()
    if ctx.json_mode:
        ctx.output(
            "ship-state:list",
            {"states": [s.to_dict() for s in states]},
        )
        return
    if not states:
        render_message("No active ship state.", style="dim")
        return
    now = datetime.now(timezone.utc)
    for s in states:
        age = now - s.updated_at
        age_s = f"{int(age.total_seconds() // 60)}m"
        title = s.pr_title or s.commit_subject or "(no title)"
        # Keep one line per PR — agents pipe this into a grep.
        render_message(
            f"PR #{s.pr}  sha={s.head_sha[:12]}  attempt={s.attempt}  "
            f"runs={len(s.dispatched_runs)}  age={age_s}  {title}"
        )
        if s.pr_url:
            render_message(f"    {s.pr_url}", style="dim")


@ship_state_group.command("show")
@click.argument("pr", type=int)
@click.pass_obj
def ship_state_show(ctx: Context, pr: int) -> None:
    """Print the full saved state for PR <pr>."""
    state = ctx.ship_state.get(pr)
    if state is None:
        render_error(f"No ship state for PR #{pr}")
        sys.exit(1)
    if ctx.json_mode:
        ctx.output("ship-state:show", state.to_dict())
        return
    render_message(f"PR #{state.pr}  attempt {state.attempt}")
    if state.pr_title:
        render_message(f"  title:          {state.pr_title}")
    if state.pr_url:
        render_message(f"  url:            {state.pr_url}")
    if state.commit_subject:
        render_message(f"  commit:         {state.commit_subject}")
    render_message(f"  repo:           {state.repo}")
    render_message(f"  branch:         {state.branch} -> {state.base_branch}")
    render_message(f"  head_sha:       {state.head_sha}")
    render_message(f"  policy:         {state.policy_signature}")
    render_message(f"  evidence:       {state.evidence_snapshot}")
    render_message(f"  dispatched:     {len(state.dispatched_runs)} run(s)")
    for run in state.dispatched_runs:
        render_message(
            f"    - {run.target} ({run.provider}) "
            f"run_id={run.run_id} status={run.status}"
        )
    render_message(f"  created_at:     {state.created_at.isoformat()}")
    render_message(f"  updated_at:     {state.updated_at.isoformat()}")


@main.command(name="auto-merge")
@click.argument("pr", type=int)
@click.option(
    "--merge-method",
    type=click.Choice(["merge", "squash", "rebase"]),
    default="squash",
    show_default=True,
    help="gh pr merge method.",
)
@click.option(
    "--delete-branch/--no-delete-branch",
    default=True,
    help="Delete the head branch on successful merge. Default on.",
)
@click.option(
    "--admin/--no-admin",
    default=False,
    help=(
        "Pass --admin to `gh pr merge` to bypass required-review "
        "protections. Off by default. Use only when the ship state "
        "already represents full merge evidence."
    ),
)
@click.pass_obj
def auto_merge(
    ctx: Context,
    pr: int,
    merge_method: str,
    delete_branch: bool,
    admin: bool,
) -> None:
    """One-shot: merge a PR if all ship-state targets are green.

    Unlike `shipyard ship`, this does NOT dispatch anything — it
    only *observes* the ship state and acts. Designed for cron /
    systemd timer / GitHub Actions schedule / anywhere you want an
    agent-agnostic merge daemon. Idempotent: running it repeatedly
    while the PR is still in flight is safe and cheap.

    Exit codes:
      0 — merged (or already merged — we treat that as success so
          a cron job doesn't alarm when it re-runs after a prior
          success)
      1 — ship state shows at least one target failed
      2 — no ship state for this PR (typo / never shipped)
      3 — ship is still in flight (retry later)

    Typical cron use:

        */10 * * * * cd /repo && shipyard auto-merge 224 || true

    Recommended pairing:
      • `shipyard ship --no-wait` (future) or `shipyard cloud run
        build <branch>` on the CI side to dispatch work.
      • This command on the merge side, unattended.

    Combined, these decouple dispatch from merge: one agent kicks
    the validation, a cron / systemd timer lands the PR when
    green. No need for an always-on conductor to follow every ship.
    """
    from shipyard.ship.pr import GhError, merge_pr

    state = ctx.ship_state.get(pr)
    if state is None:
        # No state file. Two very different cases: (a) typo / never
        # shipped; (b) we already merged this PR on a prior tick and
        # archived the state. For cron idempotency we must NOT treat
        # (b) as failure — probe GitHub for the PR's merged state
        # and return 0 if it's MERGED (#64 P2).
        if _pr_is_merged(pr):
            if ctx.json_mode:
                ctx.output(
                    "auto-merge",
                    {"event": "already-merged", "pr": pr},
                )
            else:
                render_message(
                    f"PR #{pr}: already merged — idempotent no-op.",
                    style="dim",
                )
            sys.exit(0)
        if ctx.json_mode:
            ctx.output(
                "auto-merge",
                {"event": "pr-not-found", "pr": pr},
            )
        else:
            render_message(
                f"PR #{pr}: no ship state found (typo / never shipped).",
                style="dim",
            )
        sys.exit(2)

    verdict = _ship_terminal_verdict(state)
    if verdict is None:
        # Still in flight. Surface the current picture and exit 3
        # so the cron caller knows to retry.
        if ctx.json_mode:
            ctx.output(
                "auto-merge",
                {
                    "event": "in-flight",
                    "pr": pr,
                    "evidence": dict(state.evidence_snapshot),
                },
            )
        else:
            render_message(
                f"PR #{pr}: ship still in flight — "
                f"evidence {dict(state.evidence_snapshot)}.",
                style="dim",
            )
        sys.exit(3)

    if verdict is False:
        # A target failed. Loud exit so the cron log grep catches it.
        # Exclude advisory lanes — if we're here and verdict is False,
        # an advisory lane's fail was tolerated by the verdict, so it
        # is not what the reviewer should look at.
        advisory_targets = {
            r.target for r in state.dispatched_runs if not r.required
        }
        failing = [
            t for t, v in state.evidence_snapshot.items()
            if v != "pass" and t not in advisory_targets
        ]
        if ctx.json_mode:
            ctx.output(
                "auto-merge",
                {
                    "event": "target-failed",
                    "pr": pr,
                    "failing_targets": failing,
                    "evidence": dict(state.evidence_snapshot),
                },
            )
        else:
            render_error(
                f"PR #{pr}: refusing to merge — targets failed: "
                f"{', '.join(failing)}"
            )
        sys.exit(1)

    # All green. Attempt the merge.
    #
    # merge_pr can raise GhError when the underlying `gh pr merge`
    # call fails (branch protection, auth, conflicts, transient
    # network). Cron consumers need a deterministic JSON event + a
    # non-zero exit — never a traceback (#64 P1). Also catch
    # TypeError for back-compat with older merge_pr signatures
    # shipped via pinned tool versions.
    merge_error: str | None = None
    merged = None
    try:
        merged = merge_pr(
            pr,
            method=merge_method,
            delete_branch=delete_branch,
            admin=admin,
        )
    except TypeError:
        try:
            merged = merge_pr(pr)
        except GhError as exc:
            merge_error = str(exc)
    except GhError as exc:
        merge_error = str(exc)

    if merge_error is not None:
        if ctx.json_mode:
            ctx.output(
                "auto-merge",
                {
                    "event": "merge-failed",
                    "pr": pr,
                    "error": merge_error,
                },
            )
        else:
            render_error(f"PR #{pr}: merge attempt failed — {merge_error}")
        sys.exit(1)

    if ctx.json_mode:
        ctx.output(
            "auto-merge",
            {
                "event": "merged" if merged else "merge-failed",
                "pr": pr,
            },
        )
    elif merged:
        render_message(f"PR #{pr}: merged.", style="bold green")
    else:
        render_error(f"PR #{pr}: merge attempt failed.")

    # Archive the state on success so re-runs exit cleanly (PR not
    # found) rather than re-merging.
    if merged:
        ctx.ship_state.archive(pr)
    sys.exit(0 if merged else 1)


@main.command()
@click.option(
    "--pr",
    type=int,
    default=None,
    help=(
        "PR number to watch. Defaults to the active ship for the "
        "current git branch, if one exists."
    ),
)
@click.option(
    "--follow/--no-follow",
    default=True,
    help=(
        "Keep polling until the ship reaches a terminal state. "
        "--no-follow renders one snapshot and exits."
    ),
)
@click.option(
    "--interval",
    type=float,
    default=5.0,
    show_default=True,
    help="Seconds between refreshes when --follow.",
)
@click.pass_obj
def watch(
    ctx: Context, pr: int | None, follow: bool, interval: float
) -> None:
    """Live view of an in-flight ship.

    Tails the per-PR ship state file plus the evidence store and
    renders a one-line-per-target summary of phase / status /
    heartbeat. Under --json, emits NDJSON events — one per state
    transition — suitable for piping into jq or an agent's stdin.

    Exit codes:
      0 — ship reached terminal success (all required targets pass)
      1 — ship reached terminal failure (at least one target failed)
      2 — no active ship to watch (no state for branch, or --pr
          pointed at a PR that has no state, or state was archived
          before we ever observed it)
      3 — --no-follow and the ship is still in flight (non-terminal
          snapshot; distinct from terminal success so scripts can
          distinguish "keep polling" from "done")
      130 — interrupted by SIGINT
    """
    target_pr = pr if pr is not None else _active_pr_for_current_branch(ctx)
    if target_pr is None:
        if ctx.json_mode:
            ctx.output(
                "watch",
                {
                    "event": "no-active-ship",
                    "message": "No active ship state for current branch.",
                },
            )
        else:
            render_message(
                "No active ship state for current branch. "
                "Pass --pr <n> to watch a specific PR.",
                style="dim",
            )
        sys.exit(2)

    last_signature: str | None = None
    observed_any_state = False  # distinguishes "never existed" from
    # "saw it then it went away" (#62 P1).
    import time as _time

    try:
        while True:
            state = ctx.ship_state.get(target_pr)
            if state is None:
                if not observed_any_state:
                    # We never saw a state for this PR — typo, wrong
                    # repo, or a PR that was never shipped. Exit 2
                    # rather than 0 so automation can't mistake
                    # "nothing found" for "done".
                    if ctx.json_mode:
                        ctx.output(
                            "watch",
                            {
                                "event": "pr-not-found",
                                "pr": target_pr,
                            },
                        )
                    else:
                        render_message(
                            f"PR #{target_pr}: no ship state found "
                            "(typo, wrong repo, or never shipped).",
                            style="dim",
                        )
                    sys.exit(2)
                # We did see state earlier; now it's archived
                # (merge / discard / prune). That's a clean exit.
                if ctx.json_mode:
                    ctx.output(
                        "watch",
                        {
                            "event": "state-archived",
                            "pr": target_pr,
                        },
                    )
                else:
                    render_message(
                        f"PR #{target_pr}: ship state archived "
                        "(merged, discarded, or pruned).",
                        style="dim",
                    )
                sys.exit(0)

            observed_any_state = True
            signature = _watch_event_signature(ctx, state)
            if signature != last_signature:
                _emit_watch_event(ctx, state)
                last_signature = signature

            terminal = _ship_terminal_verdict(state)
            if terminal is not None:
                sys.exit(0 if terminal else 1)

            if not follow:
                # Non-terminal snapshot under --no-follow. Exit 3 so
                # callers can distinguish "still in flight" from
                # "terminal success" (#62 P2).
                sys.exit(3)
            _time.sleep(max(1.0, interval))
    except KeyboardInterrupt:
        sys.exit(130)


def _active_pr_for_current_branch(ctx: Context) -> int | None:
    """Find the single in-flight ship state matching the current branch."""
    current = _git_branch()
    if not current:
        return None
    matches = [
        s for s in ctx.ship_state.list_active() if s.branch == current
    ]
    if not matches:
        return None
    # Prefer the most-recently-updated when more than one survives.
    return max(matches, key=lambda s: s.updated_at).pr


def _watch_signature(state: ShipState) -> str:
    """Stable hash-equivalent of the state fields `watch` renders.

    Two calls return the same string if there's nothing new to show.
    Deliberately excludes `updated_at` (which bumps on every save
    even when no target transitioned) but includes phase and the
    truncated heartbeat timestamp so a phase transition or a fresh
    heartbeat triggers a redraw.
    """
    parts = [
        f"pr={state.pr}",
        f"sha={state.head_sha}",
        f"attempt={state.attempt}",
        "evidence=" + ",".join(
            f"{k}:{v}" for k, v in sorted(state.evidence_snapshot.items())
        ),
        "runs=" + ",".join(
            f"{r.target}:{r.status}:{r.run_id}:{r.phase or ''}:"
            f"{r.last_heartbeat_at.isoformat() if r.last_heartbeat_at else ''}"
            for r in sorted(state.dispatched_runs, key=lambda r: r.target)
        ),
    ]
    return "|".join(parts)


# Heartbeat ages at or above this many seconds are considered "stale".
# Aligns with the 90s eviction threshold in FallbackChain (#84): 3
# missed 30-second heartbeat intervals. Override via WATCH_STALE_SECS.
_WATCH_STALE_THRESHOLD_SECS = 90.0


def _color_enabled(ctx: Context) -> bool:
    """Whether watch should emit ANSI styling.

    NO_COLOR=1 disables all color (XDG convention). JSON mode never
    colors output (NDJSON consumers expect plain ASCII).
    """
    import os

    if ctx.json_mode:
        return False
    return not os.environ.get("NO_COLOR")


def _style(text: str, style: str, *, enabled: bool) -> str:
    if not enabled or not style:
        return text
    return f"[{style}]{text}[/]"


def _run_symbol(status: str, liveness: str | None) -> tuple[str, str]:
    """Return (symbol, rich-style) for a dispatched run status."""
    s = status.lower()
    if s in {"pass", "success", "completed", "completed_success"}:
        return ("\u2713", "bold green")
    if s in {"fail", "failed", "failure", "completed_failure"}:
        return ("\u2717", "bold red")
    if s in {"cancelled", "canceled"}:
        return ("\u2717", "dim")
    if s in {"queued", "pending", "waiting"}:
        return ("\u00b7", "dim")
    style = "yellow"
    if liveness == "stuck":
        style = "bold magenta"
    return ("\u22ef", style)


def _evidence_symbol(status: str) -> tuple[str, str]:
    s = status.lower()
    if s == "pass":
        return ("\u2713", "bold green")
    if s == "fail":
        return ("\u2717", "bold red")
    return ("\u00b7", "dim")


def _heartbeat_age_label(
    run: DispatchedRun, now: datetime, stale_secs: float
) -> tuple[str, bool]:
    terminal = run.status.lower() in {
        "pass", "success", "completed", "completed_success",
        "fail", "failed", "failure", "completed_failure",
        "cancelled", "canceled",
    }
    if terminal:
        return ("-", False)
    hb = run.last_heartbeat_at
    if hb is None:
        return ("-", False)
    age = (now - hb).total_seconds()
    if age < 0:
        age = 0
    age_s = int(age)
    if age_s < 60:
        label = f"{age_s}s_ago"
    else:
        minutes, secs = divmod(age_s, 60)
        label = f"{minutes}m{secs:02d}s_ago"
    return (label, age >= stale_secs)


def _run_elapsed_secs(run: DispatchedRun, now: datetime) -> int:
    return max(0, int((now - run.started_at).total_seconds()))


def _progress_summary(state: ShipState) -> tuple[int, int, int]:
    total = len(state.dispatched_runs)
    complete = 0
    in_flight = 0
    terminal_statuses = {
        "pass", "success", "completed", "completed_success",
        "fail", "failed", "failure", "completed_failure",
        "cancelled", "canceled",
    }
    for run in state.dispatched_runs:
        if run.status.lower() in terminal_statuses:
            complete += 1
        else:
            in_flight += 1
    return (complete, in_flight, total)


def _watch_event_signature(ctx: Context, state: ShipState) -> str:
    """Signature that folds in reused_from annotations (see #88)."""
    base = _watch_signature(state)
    try:
        records = ctx.evidence.get_branch(state.branch)
    except Exception:  # noqa: BLE001
        records = {}
    reuse_parts = sorted(
        f"{t}:{rec.reused_from[:12]}"
        for t, rec in records.items()
        if rec.reused_from and rec.sha == state.head_sha
    )
    return base + "|reused=" + ",".join(reuse_parts)


def _emit_watch_event(ctx: Context, state: ShipState) -> None:
    now = datetime.now(timezone.utc)
    stale_secs = _watch_stale_threshold()

    # Cross-reference the evidence store so we can surface reused lanes (#88).
    reuse_map: dict[str, str] = {}
    try:
        branch_records = ctx.evidence.get_branch(state.branch)
    except Exception:  # noqa: BLE001 — evidence store is best-effort
        branch_records = {}
    for target, rec in branch_records.items():
        if rec.sha == state.head_sha and rec.reused_from:
            reuse_map[target] = rec.reused_from

    if ctx.json_mode:
        dispatched: list[dict[str, Any]] = []
        for run in state.dispatched_runs:
            entry = run.to_dict()
            entry["elapsed_seconds"] = _run_elapsed_secs(run, now)
            dispatched.append(entry)
        evidence_out: dict[str, dict[str, Any] | str] = {}
        for target, status in state.evidence_snapshot.items():
            if target in reuse_map and status == "pass":
                evidence_out[target] = {
                    "status": "reused",
                    "reused_from": reuse_map[target],
                }
            else:
                evidence_out[target] = status
        ctx.output(
            "watch",
            {
                "event": "update",
                "pr": state.pr,
                "head_sha": state.head_sha,
                "attempt": state.attempt,
                "evidence": evidence_out,
                "dispatched_runs": dispatched,
                "updated_at": state.updated_at.isoformat(),
            },
        )
        return

    color = _color_enabled(ctx)
    age_s = max(0, int((now - state.updated_at).total_seconds()))

    # Snapshot separator — local time isn't portable for agents
    # parsing scrollback; stick with UTC.
    stamp = now.strftime("%H:%M:%S UTC")
    render_message(_style(f"\u2500 {stamp} \u2500", "dim", enabled=color))

    render_message(
        f"PR #{state.pr}  sha={state.head_sha[:12]}  "
        f"attempt={state.attempt}  age={age_s}s"
    )
    advisory_targets = {
        r.target for r in state.dispatched_runs if not r.required
    }

    # Progress summary above the per-run block.
    complete, in_flight, total = _progress_summary(state)
    if total:
        render_message(
            f"  {complete}/{total} targets complete \u00b7 {in_flight} in flight"
        )

    # Evidence rendering: reuse-aware (#88) + advisory-aware (#87)
    # + color/symbols (#93). Advisory dims the whole line.
    for target, status in sorted(state.evidence_snapshot.items()):
        is_advisory = target in advisory_targets
        tag = " (advisory)" if is_advisory else ""
        if target in reuse_map and status == "pass":
            short = reuse_map[target][:7]
            sym, sym_style = _evidence_symbol("pass")
            line_style = "dim" if is_advisory else sym_style
            render_message(
                f"  {_style(sym, line_style, enabled=color)} evidence: "
                f"{target}=reused (from {short}){tag}"
            )
        else:
            sym, sym_style = _evidence_symbol(status)
            line_style = "dim" if is_advisory else sym_style
            render_message(
                f"  {_style(sym, line_style, enabled=color)} evidence: "
                f"{target}={status}{tag}"
            )

    for run in state.dispatched_runs:
        elapsed = _run_elapsed_secs(run, now)
        last_seen, is_stale = _heartbeat_age_label(run, now, stale_secs)
        sym, sym_style = _run_symbol(run.status, "stuck" if is_stale else None)
        phase = run.phase or "-"
        advisory_tag = " (advisory)" if not run.required else ""
        line_style = "dim" if not run.required else sym_style
        line = (
            f"  {_style(sym, line_style, enabled=color)} run: "
            f"{run.target} ({run.provider}) "
            f"id={run.run_id} status={run.status} "
            f"phase={phase} elapsed={elapsed}s "
            f"last_seen={last_seen}{advisory_tag}"
        )
        if is_stale:
            line += "  " + _style("stale", "bold magenta", enabled=color)
        render_message(line)


def _watch_stale_threshold() -> float:
    """Heartbeat-age cutoff for the `stale` marker.

    Default 90s (3 missed 30-second heartbeats) mirrors the eviction
    threshold used by FallbackChain in #84. Override via the
    ``SHIPYARD_WATCH_STALE_SECS`` env var.
    """
    import os

    raw = os.environ.get("SHIPYARD_WATCH_STALE_SECS")
    if not raw:
        return _WATCH_STALE_THRESHOLD_SECS
    try:
        value = float(raw)
    except ValueError:
        return _WATCH_STALE_THRESHOLD_SECS
    return value if value > 0 else _WATCH_STALE_THRESHOLD_SECS


def _ship_terminal_verdict(state: ShipState) -> bool | None:
    """Return True=pass, False=fail, None=still in flight.

    Terminal when every *required* dispatched lane has a terminal
    evidence value (pass/fail). Fix for #108: we now require
    coverage of every required ``DispatchedRun.target``, not just
    terminal values on whatever rows happen to be present. A ship
    that dispatched three lanes and only persisted evidence for one
    (all "pass") previously returned True; it now returns None
    until the remaining required lanes post evidence.

    Advisory lanes (``DispatchedRun.required is False``) are
    tolerant in both directions — their absence from the snapshot
    doesn't block the verdict, and a "fail" on an advisory lane is
    ignored when computing pass/fail. This matches the pre-fix
    advisory semantics.
    """
    if not state.evidence_snapshot:
        return None
    statuses = set(state.evidence_snapshot.values())
    if statuses - {"pass", "fail"}:
        return None
    advisory_targets = {
        r.target for r in state.dispatched_runs if not r.required
    }
    required_targets = {
        r.target for r in state.dispatched_runs if r.required
    }
    # Every required lane must have a terminal row. Without this
    # coverage check, a partial mirror of evidence + all-pass values
    # looked identical to a complete pass and triggered premature
    # merges (#108).
    if required_targets - set(state.evidence_snapshot):
        return None
    return all(
        v == "pass" or tgt in advisory_targets
        for tgt, v in state.evidence_snapshot.items()
    )


@ship_state_group.command("reconcile")
@click.argument("pr", type=int, required=False)
@click.option(
    "--all",
    "reconcile_all",
    is_flag=True,
    help="Reconcile every active ship-state (ignores the PR argument)",
)
@click.pass_obj
def ship_state_reconcile(
    ctx: Context, pr: int | None, reconcile_all: bool
) -> None:
    """Re-fetch CI state from GitHub and heal drifted ship-state.

    Webhook events update dispatched_runs as they arrive; if the daemon
    was down or not registered for the repo, those events are lost and
    the local ship-state file keeps stale statuses. This command pulls
    ``gh pr view <PR> --json statusCheckRollup`` and rewrites
    dispatched_runs to match what GitHub reports right now.
    """
    import subprocess as _sp

    from shipyard.ship.reconcile import reconcile_ship_state

    if reconcile_all and pr is not None:
        render_error("Pass either <pr> or --all, not both.")
        sys.exit(2)
    if not reconcile_all and pr is None:
        render_error("Usage: shipyard ship-state reconcile <pr> | --all")
        sys.exit(2)

    targets: list[ShipState] = []
    if reconcile_all:
        targets = ctx.ship_state.list_active()
    elif pr is not None:
        state = ctx.ship_state.get(pr)
        if state is not None:
            targets = [state]
    if not targets:
        render_message(
            f"No active ship state for PR #{pr}." if pr else "No active ship state.",
            style="dim",
        )
        return

    results: list[dict[str, Any]] = []
    for state in targets:
        try:
            raw = _sp.run(
                [
                    "gh", "pr", "view", str(state.pr),
                    "--repo", state.repo,
                    "--json", "statusCheckRollup",
                ],
                capture_output=True, text=True, check=True, timeout=20,
            ).stdout
        except (_sp.CalledProcessError, _sp.TimeoutExpired, FileNotFoundError) as exc:
            results.append({"pr": state.pr, "ok": False, "error": str(exc)})
            continue
        try:
            rollup = json.loads(raw).get("statusCheckRollup") or []
        except (ValueError, KeyError):
            results.append({"pr": state.pr, "ok": False, "error": "unparseable gh output"})
            continue
        new_state, changes = reconcile_ship_state(state, rollup)
        if changes:
            ctx.ship_state.save(new_state)
        results.append({
            "pr": state.pr, "ok": True, "changes": changes,
        })

    if ctx.json_mode:
        ctx.output("ship-state:reconcile", {"results": results})
        return

    for r in results:
        if not r["ok"]:
            render_error(f"PR #{r['pr']}: {r['error']}")
            continue
        changes = r.get("changes") or []
        if changes:
            render_message(
                f"PR #{r['pr']}: applied {len(changes)} change(s)", style="green",
            )
            for c in changes:
                render_message(f"  · {c}", style="dim")
        else:
            render_message(
                f"PR #{r['pr']}: already in sync with GitHub", style="dim",
            )


@ship_state_group.command("discard")
@click.argument("pr", type=int)
@click.pass_obj
def ship_state_discard(ctx: Context, pr: int) -> None:
    """Archive the active state for PR <pr> (does not delete; leaves a tombstone)."""
    state = ctx.ship_state.get(pr)
    if state is None:
        render_error(f"No ship state for PR #{pr}")
        sys.exit(1)
    archived = ctx.ship_state.archive(pr)
    if ctx.json_mode:
        ctx.output(
            "ship-state:discard",
            {"pr": pr, "archived_to": str(archived) if archived else None},
        )
    else:
        render_message(f"Archived ship state for PR #{pr}.")


# ---- Helpers ----


@main.command(name="pr")
@click.option(
    "--base",
    default="main",
    help="Base branch to ship into (default: main)",
)
@click.option(
    "--apply-bumps/--no-apply-bumps",
    default=True,
    help=(
        "Run scripts/version_bump_check.py --mode=apply to auto-rewrite "
        "version files when a surface moved. On by default (mirrors "
        "pulp's pulp pr). --no-apply-bumps switches to --mode=report so "
        "missing bumps hard-fail."
    ),
)
@click.option(
    "--allow-unreachable-targets",
    is_flag=True,
    help="Forwarded to `shipyard ship`. Lane is skipped, validation gap recorded.",
)
@click.option(
    "--skip-target",
    "skip_target",
    multiple=True,
    metavar="NAME",
    help="Forwarded to `shipyard ship`. Deliberate skip (not tied to reachability).",
)
@click.option(
    "--skip-bump",
    metavar="SURFACE",
    multiple=True,
    help=(
        "Shorthand: write a `Version-Bump: <surface>=skip reason=…` "
        "trailer onto the tip commit instead of remembering the exact "
        "format. Pair with --bump-reason. Repeatable for multiple "
        "surfaces. Amends the tip commit (not pushed yet)."
    ),
)
@click.option(
    "--bump-reason",
    default=None,
    help="Reason string used with --skip-bump. Required when --skip-bump is set.",
)
@click.option(
    "--skip-skill-update",
    metavar="SKILL",
    multiple=True,
    help=(
        "Shorthand: write a `Skill-Update: skip skill=<name> reason=…` "
        "trailer onto the tip commit. Pair with --skill-reason. "
        "Repeatable."
    ),
)
@click.option(
    "--skill-reason",
    default=None,
    help=(
        "Reason string used with --skip-skill-update. Required when "
        "--skip-skill-update is set."
    ),
)
@click.pass_context
def pr(
    ctx: click.Context,
    base: str,
    apply_bumps: bool,
    allow_unreachable_targets: bool,
    skip_target: tuple[str, ...],
    skip_bump: tuple[str, ...],
    bump_reason: str | None,
    skip_skill_update: tuple[str, ...],
    skill_reason: str | None,
) -> None:
    """One-shot push-a-PR: skill-sync + version-bump + ship.

    Mirrors pulp's `pulp pr` for parity with the ci skill's natural-
    language triggers ("push a PR", "ship this"). Internally:

        1. scripts/skill_sync_check.py --mode=report
        2. scripts/version_bump_check.py --mode=(apply|report)
        3. git commit of any bumps
        4. invokes `shipyard ship` for push + PR + validate + merge
    """
    import shutil

    # Trailer shortcuts: materialize Version-Bump / Skill-Update trailers
    # onto the tip commit *before* gate scripts run, since those scripts
    # read trailers off HEAD to decide whether a skip is authorized.
    if skip_bump and not bump_reason:
        render_error("--skip-bump requires --bump-reason \"...\".")
        sys.exit(2)
    if skip_skill_update and not skill_reason:
        render_error("--skip-skill-update requires --skill-reason \"...\".")
        sys.exit(2)
    trailers_to_add: list[str] = []
    for surface in skip_bump:
        trailers_to_add.append(
            f'Version-Bump: {surface}=skip reason="{bump_reason}"'
        )
    for skill in skip_skill_update:
        trailers_to_add.append(
            f'Skill-Update: skip skill={skill} reason="{skill_reason}"'
        )
    if trailers_to_add:
        try:
            added = _append_trailers_to_tip(trailers_to_add)
        except _TrailerAmendError as exc:
            render_error(str(exc))
            sys.exit(2)
        if added:
            for line in added:
                click.echo(f"▸ Added trailer: {line}")

    repo_root = Path(
        subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True
        ).strip()
    )

    from shipyard.gate_scripts import (
        SKILL_SYNC,
        VERSION_BUMP,
        VERSIONING_CONFIG,
        GateScriptNotFoundError,
    )
    from shipyard.gate_scripts import (
        resolve as resolve_gate_script,
    )

    try:
        ssc = resolve_gate_script(SKILL_SYNC, repo_root, config=ctx.obj.config)
        vbc = resolve_gate_script(VERSION_BUMP, repo_root, config=ctx.obj.config)
        cfg = resolve_gate_script(VERSIONING_CONFIG, repo_root, config=ctx.obj.config)
    except GateScriptNotFoundError as exc:
        render_error(str(exc))
        sys.exit(2)

    python = shutil.which("python3") or "python3"

    # Best-effort heads-up if the auto-release chain isn't wired up. We
    # don't block the PR — the gate scripts and the merge are unaffected
    # — but warning here is the cheapest way to surface the trap *before*
    # the user wonders why no GitHub Release appeared on merge. Run
    # `shipyard doctor` for the same check + the fix recipe.
    token_section = _check_release_bot_token()
    if token_section and not token_section.get("RELEASE_BOT_TOKEN", {}).get("ok", True):
        click.secho(
            "▸ Heads-up: RELEASE_BOT_TOKEN secret is missing on this repo.\n"
            "         Auto-release will tag but the binary release workflow won't fire.\n"
            "         See `shipyard doctor` for the one-time setup steps.",
            fg="yellow",
            err=True,
        )

    click.echo("▸ Skill-sync check")
    rc = subprocess.call(
        [python, str(ssc), "--base", f"origin/{base}", "--config", str(cfg), "--mode=report"]
    )
    if rc != 0:
        render_error(
            "skill-sync gate failed. Update the listed SKILL.md(s) or add a "
            "`Skill-Update: skip skill=<name> reason=\"...\"` trailer on the "
            "tip commit, then retry."
        )
        sys.exit(rc)

    click.echo("▸ Version-bump " + ("apply" if apply_bumps else "report"))
    mode = "apply" if apply_bumps else "report"

    # Ask version_bump_check itself which files it edited. Parsing its
    # "Edited files:" stdout is robust to files that were ALSO pre-staged
    # by the user — a simple pre/post index-set diff drops those. Users
    # with unrelated pre-staged work keep that work in the index; only the
    # bump-touched files go into the chore: commit, even if the user had
    # e.g. plugin.json staged for their own reason.
    vbc_run = subprocess.run(
        [python, str(vbc), "--base", f"origin/{base}", "--config", str(cfg), f"--mode={mode}"],
        capture_output=True,
        text=True,
    )
    # Stream the script's output so the user still sees it.
    if vbc_run.stdout:
        click.echo(vbc_run.stdout, nl=False)
    if vbc_run.stderr:
        click.echo(vbc_run.stderr, nl=False, err=True)
    if vbc_run.returncode != 0:
        render_error("version-bump gate failed; fix the bump and retry.")
        sys.exit(vbc_run.returncode)

    # Parse the "Edited files:" section. Lines after that header until a
    # blank line or the next `[` surface report begin with two spaces and
    # a path.
    bumped_files: list[str] = []
    in_edited_block = False
    for line in vbc_run.stdout.splitlines():
        if line.startswith("Edited files:"):
            in_edited_block = True
            continue
        if in_edited_block:
            if line.startswith("  ") and line.strip():
                bumped_files.append(line.strip())
            else:
                break

    if bumped_files:
        click.echo(f"▸ Committing version bump(s) — {len(bumped_files)} file(s)")
        # `--only -- <paths>` commits exactly those paths' current working-
        # tree state. If a file was pre-staged by the user with unrelated
        # changes, this captures the post-bump content (their changes +
        # the bump), which is the right semantics: the file's on-disk
        # state is now "user edit + bump", and committing it together
        # prevents shipping a split-brain bump.
        subprocess.check_call(
            [
                "git",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "chore: bump versions",
                "--only",
                "--",
                *bumped_files,
            ]
        )

    # Delegate to the existing ship command for push/PR/validate/merge.
    click.echo("▸ Handing off to `shipyard ship`")
    ctx.invoke(
        ship,
        base=base,
        allow_root_mismatch=False,
        allow_unreachable_targets=allow_unreachable_targets,
        skip_target=skip_target,
        auto_create_base=None,
    )


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
    """Merge target-specific and platform-specific overrides into base validation.

    Platform overrides are read from two locations for backwards
    compatibility:
      1. `base["overrides"][<platform_os>]` — nested inside the
         already-resolved mode subtable (e.g.
         `[validation.default.overrides.windows]`). This is how
         Pulp declares its Windows-specific build commands.
      2. `config.validation["overrides"][<platform_os>]` — at the
         top of the validation block. Older shape, still supported
         for projects that want a single override list for every
         mode.

    The nested form wins if both are declared, so per-mode
    overrides can replace top-level ones.
    """
    result = dict(base)

    target_config = config.targets.get(target_name, {})
    platform = target_config.get("platform", "")
    platform_os = platform.split("-")[0] if platform else ""

    # Top-level overrides (legacy shape)
    top_overrides = config.validation.get("overrides", {})
    if isinstance(top_overrides, dict) and platform_os in top_overrides:
        result.update(top_overrides[platform_os])

    # Mode-nested overrides (preferred shape — matches Pulp's config)
    nested_overrides = base.get("overrides", {})
    if isinstance(nested_overrides, dict) and platform_os in nested_overrides:
        result.update(nested_overrides[platform_os])

    # The `overrides` key has now been applied; strip it so it
    # doesn't leak into the downstream validation_config as if it
    # were a stage command.
    result.pop("overrides", None)

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
    ship_state: ShipState | None = None,
    warm_disabled: bool = False,
) -> Job:
    job = job.start()
    ctx.queue.update(job)

    validation_config = _resolve_validation(config, mode)
    had_failure = False
    warm_pool = WarmPool(default_pool_path(config.state_dir))
    # Global kill switch: env var disables everything for this process.
    warm_globally_off = warm_disabled or warm_reuse_disabled_by_env()
    warmed_bucket: set[str] = set()  # per-invocation warn-once bucket

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
        resolved_validation = _resolve_target_validation(
            config, name, validation_config
        )

        # Cross-PR evidence reuse (opt-in via the target's
        # ``reuse_if_paths_unchanged`` glob list). When the diff since
        # an ancestor SHA does not touch any path the target cares
        # about, we borrow that ancestor's PASS evidence instead of
        # dispatching. See ``shipyard.ship.reuse``.
        reuse_result = _maybe_reuse_evidence(
            ctx=ctx,
            job=job,
            target_name=name,
            target_config=target_config,
            validation_config=resolved_validation,
        )
        if reuse_result is not None:
            job = job.with_result(reuse_result)
            ctx.queue.update(job)
            if not ctx.json_mode:
                render_job(job)
            continue

        # Warm-pool reuse gate. When the target opts in via
        # ``warm_keepalive_seconds > 0`` and a live, same-SHA entry
        # exists for this ``(target, host)`` pair, re-enter the
        # previously warmed workdir and skip pre-stage (the bundle
        # delivery and the ``setup`` step). Validate is always re-run.
        # See ``src/shipyard/targets/warm_pool.py``.
        warm_hit, effective_resume = _apply_warm_reuse(
            pool=warm_pool,
            target_name=name,
            target_config=target_config,
            backend=backend_name,
            sha=job.sha,
            requested_resume_from=resume_from,
            globally_off=warm_globally_off,
            warn_bucket=warmed_bucket,
            json_mode=ctx.json_mode,
        )

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
            validation_config=resolved_validation,
            log_path=log_path,
            progress_callback=progress_callback,
            resume_from=effective_resume,
        )
        job = state["job"].with_result(result)
        ctx.queue.update(job)

        # Warm-pool bookkeeping. On PASS, refresh / insert the entry;
        # on any non-PASS outcome during a warm reuse, evict so a
        # potentially dirty workdir is not served twice.
        _update_warm_pool_after_run(
            pool=warm_pool,
            target_name=name,
            target_config=target_config,
            backend=backend_name,
            sha=job.sha,
            result=result,
            warm_was_applied=warm_hit,
            globally_off=warm_globally_off,
        )

        if not result.passed:
            had_failure = True

        if not ctx.json_mode:
            render_job(job)

    job = job.complete()
    ctx.queue.update(job)
    _record_evidence(ctx, job)
    if ship_state is not None:
        _update_ship_state_from_job(ctx, ship_state, job)
    return job


def _apply_warm_reuse(
    *,
    pool: WarmPool,
    target_name: str,
    target_config: dict[str, Any],
    backend: str,
    sha: str,
    requested_resume_from: str | None,
    globally_off: bool,
    warn_bucket: set[str],
    json_mode: bool,
) -> tuple[bool, str | None]:
    """Consult the warm pool and (if hit) mutate ``target_config`` in place.

    Returns ``(warm_hit, effective_resume_from)``.

    ``warm_hit`` is True when a live pool entry was consumed. The
    caller uses it to decide whether a subsequent failure should evict
    the entry.

    ``effective_resume_from`` is the resume stage to pass to the
    dispatcher. When reuse fires and the caller did not already ask
    for a more-aggressive resume point, we jump to ``configure`` — i.e.
    skip the ``setup`` stage (deps install) and re-run configure /
    build / test. If the caller explicitly asked for a later stage
    (e.g. ``--resume-from=test``), we honor that instead.
    """
    if globally_off:
        return False, requested_resume_from
    keepalive = extract_warm_keepalive_seconds(target_config)
    if keepalive <= 0:
        return False, requested_resume_from
    if not is_backend_eligible(backend, target_config):
        # The target opted in but the backend can't honor it
        # (workflow-dispatch runners are ephemeral). Warn once per
        # (target, invocation) so users can reconcile their config.
        if target_name not in warn_bucket:
            warn_bucket.add(target_name)
            if not json_mode:
                render_message(
                    f"warning: target '{target_name}' set "
                    f"warm_keepalive_seconds but backend {backend!r} is "
                    f"ephemeral — warm-pool reuse ignored",
                    style="dim yellow",
                )
        return False, requested_resume_from

    host = warm_host_key(target_config)
    entry = pool.get(target_name, host)
    if entry is None or entry.sha != sha:
        return False, requested_resume_from

    # Prefer the caller's request if it already skips further than
    # our default warm resume point — e.g. ``--resume-from=test``
    # should stay at ``test``.
    resume_order = ("setup", "configure", "build", "test")
    warm_default = "configure"  # skip pre-stage, re-run configure+build+test
    if requested_resume_from in resume_order:
        try:
            requested_idx = resume_order.index(requested_resume_from)
        except ValueError:
            requested_idx = -1
        default_idx = resume_order.index(warm_default)
        effective = requested_resume_from if requested_idx > default_idx else warm_default
    else:
        effective = warm_default

    # Re-enter the warmed workdir so the executor finds the repo
    # already populated. For SSH backends we overwrite ``repo_path``;
    # other backends treat ``_warm_workdir`` as a hint.
    target_config["repo_path"] = entry.workdir
    target_config["_warm_workdir"] = entry.workdir
    target_config["_warm_reuse"] = True
    return True, effective


def _update_warm_pool_after_run(
    *,
    pool: WarmPool,
    target_name: str,
    target_config: dict[str, Any],
    backend: str,
    sha: str,
    result: TargetResult,
    warm_was_applied: bool,
    globally_off: bool,
) -> None:
    """Record / evict warm-pool entries based on run outcome.

    - PASS on an eligible backend: upsert a fresh entry with a new
      expiry window.
    - Non-PASS following a warm reuse: evict the entry so the next
      ship on that ``(target, host)`` pair cold-starts from scratch.
    - PASS on an ineligible backend or when globally off: no-op.
    """
    host = warm_host_key(target_config)

    if result.passed:
        if globally_off:
            return
        keepalive = extract_warm_keepalive_seconds(target_config)
        if keepalive <= 0:
            return
        if not is_backend_eligible(backend, target_config):
            return
        workdir = (
            target_config.get("_warm_workdir")
            or target_config.get("repo_path")
            or "~/repo"
        )
        entry = PoolEntry(
            target=target_name,
            host=host,
            backend=backend,
            workdir=str(workdir),
            sha=sha,
            expires_at=compute_expires_at(keepalive),
            created_at=datetime.now(timezone.utc).timestamp(),
        )
        pool.upsert(entry)
        return

    if warm_was_applied:
        pool.evict(target_name, host)


def _maybe_reuse_evidence(
    *,
    ctx: Context,
    job: Job,
    target_name: str,
    target_config: dict[str, Any],
    validation_config: dict[str, Any],
) -> TargetResult | None:
    """Return a synthetic reuse :class:`TargetResult` or ``None``.

    ``None`` means "dispatch normally". When a result is returned,
    ``status`` is always PASS and ``reused_from`` carries the
    ancestor SHA the evidence was borrowed from. The caller should
    skip dispatch and let the standard evidence-record path write
    the synthetic PASS.
    """
    from shipyard.ship.reuse import evaluate_reuse

    decision = evaluate_reuse(
        target_name=target_name,
        target_config=target_config,
        validation_config=validation_config,
        head_sha=job.sha,
        evidence_store=ctx.evidence,
    )
    if not decision.reused:
        return None

    assert decision.ancestor_sha is not None  # narrows type for mypy
    now = datetime.now(timezone.utc)
    return TargetResult(
        target_name=target_name,
        platform=target_config.get("platform", "unknown"),
        status=TargetStatus.PASS,
        backend="reused",
        started_at=now,
        completed_at=now,
        duration_secs=0.0,
        reused_from=decision.ancestor_sha,
    )


def _record_evidence(ctx: Context, job: Job) -> None:
    from shipyard.core.evidence import EvidenceRecord
    from shipyard.ship.reuse import compute_validation_signature

    # Resolve the validation config once so the signature we write on
    # each record reflects the same config the dispatcher just ran.
    # A reuse decision on a future PR compares these fingerprints.
    validation_base = _resolve_validation(ctx.config, job.mode)

    for name, result in job.results.items():
        if not result.is_terminal:
            continue
        per_target_validation = _resolve_target_validation(
            ctx.config, name, validation_base
        )
        contract_digest, stages_signature = compute_validation_signature(
            per_target_validation
        )
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
            failure_class=result.failure_class,
            reused_from=result.reused_from,
            contract_digest=contract_digest,
            stages_signature=stages_signature,
        ))


def _update_ship_state_from_job(
    ctx: Context, ship_state: ShipState, job: Job
) -> None:
    """Mirror the job's per-target outcomes into the ship state.

    Writes both an evidence snapshot (target -> "pass"/"fail") and
    a DispatchedRun per terminal target so a future resume can see
    which run IDs it was tracking.

    Additionally propagates ``phase`` and ``last_heartbeat_at`` from
    the TargetResult onto the DispatchedRun so the watch renderer
    can show liveness for targets whose state comes from the local
    queue rather than a cloud polling loop.
    """
    now = datetime.now(timezone.utc)
    cloud_runs_by_platform = _cloud_runs_by_platform(ctx, job.sha)
    advisory_policy = _resolve_lane_policy_for_ship(ctx.config)
    for name, result in job.results.items():
        if not result.is_terminal:
            continue
        ship_state.update_evidence(
            name, "pass" if result.passed else "fail"
        )
        provider = result.provider or result.primary_backend or result.backend or "unknown"
        run_id = cloud_runs_by_platform.get(result.platform) or job.id
        ship_state.upsert_run(
            DispatchedRun(
                target=name,
                provider=provider,
                run_id=str(run_id),
                status="completed" if result.passed else "failed",
                started_at=result.started_at or now,
                updated_at=result.completed_at or now,
                attempt=ship_state.attempt,
                last_heartbeat_at=result.last_heartbeat_at,
                phase=result.phase,
                required=advisory_policy.is_required(name),
            )
        )
    ctx.ship_state.save(ship_state)


def _compose_pr_body(config: Config) -> str:
    """Legacy entry point preserved for any callers importing this
    name from cli.py. New code should import directly from
    ``shipyard.ship.pr_text``."""
    from shipyard.ship.pr_text import compose_pr_body

    return compose_pr_body(config=config)


def _resolve_lane_policy_for_ship(config: Config) -> LanePolicy:
    """Resolve the lane policy (advisory vs required) for the current PR.

    Reads the tip commit to pick up a ``Lane-Policy:`` trailer
    overlay. Falls back silently to config-only advisory if the git
    read fails — the tip commit isn't always reachable (worktree
    recreated, shallow clone) and we'd rather ship with the
    committed policy than abort.
    """
    from shipyard.ship.lane_policy import resolve_lane_policy

    return resolve_lane_policy(
        config,
        known_targets=list((config.targets or {}).keys()),
    )


def _cloud_runs_by_platform(ctx: Context, sha: str) -> dict[str, str]:
    """Best-effort map of platform -> cloud run_id for this SHA.

    Records whose ``requested_ref`` doesn't match ``sha`` are
    skipped. Prior to the #111 fix, the ``sha`` parameter was
    accepted but not read — the function returned the most-recent
    run_id per platform across every record in the store, so on a
    busy machine a DispatchedRun for an earlier ship could be
    attributed to a later ship's cloud run.

    Records whose ``requested_ref`` is blank or doesn't equal the
    requested SHA are ignored. Records with no ``platform`` /
    ``target`` hint in their dispatch_fields are also ignored —
    without a platform key we can't place them in the map.
    """
    try:
        records = ctx.cloud_records.list(limit=40)
    except Exception:
        return {}
    mapping: dict[str, str] = {}
    for record in records:
        if record.run_id is None:
            continue
        if record.requested_ref and record.requested_ref != sha:
            continue
        # A cloud record's dispatch_fields often carries the
        # platform / target hint. Best-effort; absence is fine.
        platform = record.dispatch_fields.get("platform") or record.dispatch_fields.get("target")
        if platform and platform not in mapping:
            mapping[platform] = str(record.run_id)
    return mapping


def _pr_url(repo_slug: str, pr_number: int) -> str:
    if not repo_slug or not pr_number:
        return ""
    return f"https://github.com/{repo_slug}/pull/{pr_number}"


def _git_commit_subject(sha: str) -> str:
    try:
        result = subprocess.run(
            ["git", "log", "-1", "--format=%s", sha],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return ""
    if result.returncode != 0:
        return ""
    return result.stdout.strip()


def _resolve_expected_sha(value: str) -> str | None:
    """Map --require-sha input to a full 40-char SHA.

    - "HEAD" → the current local HEAD
    - 40-char hex → returned as-is (no verification against the repo;
      the remote comparison is the check that matters)
    - anything else → None (caller errors out)
    """
    if value.upper() == "HEAD":
        try:
            result = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                capture_output=True,
                text=True,
                timeout=5,
            )
        except (FileNotFoundError, subprocess.TimeoutExpired):
            return None
        if result.returncode != 0:
            return None
        return result.stdout.strip() or None
    lowered = value.strip().lower()
    if len(lowered) == 40 and all(c in "0123456789abcdef" for c in lowered):
        return lowered
    return None


def _remote_ref_sha(repo_slug: str, ref: str) -> str | None:
    """Return the current commit SHA for `ref` on the remote, or None.

    Uses `gh api repos/:slug/commits/:ref --jq .sha`. Refuses to
    fall back to local data — the whole point of --require-sha is
    to compare against what GitHub will use when it dispatches.
    """
    try:
        result = subprocess.run(
            ["gh", "api", f"repos/{repo_slug}/commits/{ref}",
             "--jq", ".sha"],
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None
    if result.returncode != 0:
        return None
    sha = result.stdout.strip().lower()
    return sha if len(sha) == 40 else None


class _TrailerAmendError(Exception):
    """Raised when the tip commit can't be amended with a trailer.

    Message is already user-friendly — the CLI renders str(exc) directly.
    """


def _append_trailers_to_tip(trailers: list[str]) -> list[str]:
    """Amend HEAD to carry each trailer line (if not already present).

    Returns the subset of trailers that were actually added.

    Safety properties (#59 P1/P2):
    1. Refuses to run when the index has staged changes.
       `git commit --amend` without --only picks up whatever is
       staged, so letting this proceed with a dirty index would
       silently fold unrelated staged work into the amended tip
       commit. We fail fast and tell the user to unstage.
    2. For Version-Bump trailers we strip any existing trailer
       naming the same surface before appending the new one, so
       `--skip-bump sdk` replaces a prior `Version-Bump: sdk=patch`
       instead of racing it. version_bump_check's surface_trailer_
       override returns the first match — a stale one would win.
       Same logic for Skill-Update: replace per-skill rather than
       stack.
    """
    # Guard 1: refuse when the index is dirty.
    try:
        idx_check = subprocess.run(
            ["git", "diff", "--cached", "--quiet"],
            capture_output=True,
        )
    except (subprocess.SubprocessError, FileNotFoundError) as exc:
        raise _TrailerAmendError(
            "Couldn't probe git index — is this a git repo?"
        ) from exc
    if idx_check.returncode != 0:
        raise _TrailerAmendError(
            "Refusing to amend: staged changes would be folded into "
            "the tip commit. Commit, unstage (git reset), or stash "
            "them first, then re-run `shipyard pr` with the shortcut "
            "flags."
        )

    try:
        current = subprocess.check_output(
            ["git", "log", "-1", "--format=%B"], text=True,
            stderr=subprocess.PIPE,
        )
    except subprocess.CalledProcessError as exc:
        raise _TrailerAmendError(
            "Couldn't read tip commit message (is this a git repo "
            "with at least one commit?)."
        ) from exc

    new_msg = current
    added: list[str] = []
    for trailer in trailers:
        if trailer in new_msg:
            continue
        # Strip any stale trailer for the same (key, target) —
        # `Version-Bump: sdk=*` or `Skill-Update: skill=<name>`.
        new_msg = _strip_conflicting_trailer(new_msg, trailer)
        try:
            new_msg = subprocess.check_output(
                ["git", "interpret-trailers",
                 "--if-exists", "addIfDifferent",
                 "--trailer", trailer],
                input=new_msg, text=True, stderr=subprocess.PIPE,
            )
        except subprocess.CalledProcessError as exc:
            raise _TrailerAmendError(
                f"git interpret-trailers rejected trailer '{trailer}'. "
                "Check the trailer format."
            ) from exc
        added.append(trailer)

    if not added:
        return []

    # --allow-empty so an amend whose only change is the message
    # succeeds even when the prior commit's tree was already empty.
    # --only HEAD with no pathspec would require one; the index-
    # guard above ensures we're safe without it.
    try:
        subprocess.check_call(
            ["git", "commit", "--amend", "--allow-empty", "-m", new_msg],
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
    except subprocess.CalledProcessError as exc:
        raise _TrailerAmendError(
            "git commit --amend failed. Commit manually or re-run "
            "without the trailer shortcut flags."
        ) from exc
    return added


def _strip_conflicting_trailer(message: str, new_trailer: str) -> str:
    """Remove an existing trailer that conflicts with `new_trailer`.

    Trailers are key: value lines near the end of a commit message.
    `new_trailer` has the form `<Key>: <payload>` where payload
    starts with either `<surface>=...` (Version-Bump) or `skip
    skill=<name> ...` (Skill-Update). We identify the distinguishing
    sub-key (surface or skill name) from the new trailer and strip
    any existing line whose Key matches AND whose sub-key matches.

    Matching is on exact token boundaries (#60 P2): a substring
    match on `skill=ci` would also strip `skill=ci-tools`, so we
    tokenize each candidate line and compare the skill= / surface=
    value as a whole word.

    Unrelated trailers are left alone.
    """
    import re

    if ":" not in new_trailer:
        return message
    key, payload = new_trailer.split(":", 1)
    key = key.strip()
    payload = payload.strip()
    target: str | None = None
    if key == "Version-Bump" and "=" in payload:
        # `sdk=skip reason="..."` -> surface = "sdk"
        target = payload.split("=", 1)[0].strip()
    elif key == "Skill-Update" and "skill=" in payload:
        # `skip skill=ci reason="..."` -> skill name after "skill="
        after = payload.split("skill=", 1)[1]
        target = after.split(None, 1)[0].rstrip(",")
    else:
        return message
    if not target:
        return message

    # Match `target` as a whole token: `\b` alone is insufficient
    # because `-` is a word boundary for `\b`, so `skill=ci\b` would
    # match `skill=ci-tools`. We require the next character to be
    # neither a word char nor a hyphen (surface/skill names often
    # contain hyphens, e.g. `sdk-core`, `ci-tools`).
    escaped = re.escape(target)
    if key == "Version-Bump":
        conflict = re.compile(
            rf"^Version-Bump:\s*{escaped}(?![\w-])\s*="
        )
    else:  # Skill-Update
        conflict = re.compile(
            rf"^Skill-Update:.*\bskill={escaped}(?![\w-])"
        )

    kept_lines: list[str] = []
    for line in message.splitlines():
        if conflict.search(line):
            continue
        kept_lines.append(line)
    stripped = "\n".join(kept_lines)
    # Preserve trailing newline shape from the original message.
    if message.endswith("\n") and not stripped.endswith("\n"):
        stripped += "\n"
    return stripped


def _detect_repo_slug_or_empty() -> str:
    """Best-effort `owner/repo` slug from the git origin; empty string on miss."""
    try:
        ref = detect_repo_from_remote()
    except Exception:
        return ""
    if ref is None:
        return ""
    slug = getattr(ref, "slug", None)
    if slug:
        return slug
    return str(ref) if ref else ""


def _required_platforms_for_config(config: Config) -> list[str]:
    """Platform names required by the current merge policy.

    Falls back to the set of target platforms when the config does
    not declare an explicit merge policy.
    """
    merge_cfg = getattr(config, "merge", None) or {}
    required = merge_cfg.get("required_platforms") if isinstance(merge_cfg, dict) else None
    if required:
        return list(required)
    platforms: list[str] = []
    for target in config.targets.values():
        platform = target.get("platform")
        if platform and platform not in platforms:
            platforms.append(platform)
    return platforms


def _resolve_resume_mode(
    flag: bool | None, existing_state: ShipState | None
) -> bool | None:
    """Translate the CLI flag + state presence into an effective resume mode.

    Returns True for "resume", False for "force fresh", None when
    there is nothing to resume.
    """
    if existing_state is None:
        return None
    if flag is None:
        # Default: auto-resume when a state file exists.
        return True
    return flag


def _detect_ship_state_drift(
    state: ShipState, *, current_sha: str, current_policy: str
) -> str | None:
    """Return a human-readable reason why `state` must not be resumed, or None."""
    if state.is_sha_drift(current_sha):
        return (
            f"PR head SHA has moved since the saved state was written "
            f"(was {state.head_sha[:12]}, now {current_sha[:12]})"
        )
    if state.policy_signature and state.policy_signature != current_policy:
        return (
            "Merge policy (required platforms / targets / mode) has "
            "changed since the saved state was written"
        )
    return None


def _wait_for_cloud_completion(repository: str | None, run_id: str) -> dict[str, Any]:
    while True:
        view = run_view(repository=repository, run_id=run_id)
        if view.get("status") == "completed":
            return view
        render_message(f"waiting for cloud run {run_id}...", style="dim")
        import time

        time.sleep(5)


# ── targets commands ──────────────────────────────────────────────────


@main.group(invoke_without_command=True)
@click.pass_context
def targets(ctx: click.Context) -> None:
    """List, add, remove, and test validation targets.

    With no subcommand, lists all configured targets and their
    reachability — equivalent to `shipyard targets list`.
    """
    if ctx.invoked_subcommand is None:
        ctx.invoke(targets_list)


@targets.command("list")
@click.pass_obj
def targets_list(ctx: Context) -> None:
    """List configured targets with reachability status."""
    config = ctx.config
    if not config.targets:
        if ctx.json_mode:
            ctx.output("targets.list", {"targets": []})
        else:
            render_message("No targets configured. Run `shipyard init`.")
        return

    dispatcher = _make_dispatcher(config)
    rows: list[dict[str, Any]] = []
    for name, tconfig in config.targets.items():
        target_config = dict(tconfig)
        target_config["name"] = name
        reachable, selected_backend = _probe_target(target_config, dispatcher)
        rows.append({
            "name": name,
            "backend": dispatcher.backend_name(target_config),
            "platform": tconfig.get("platform", "unknown"),
            "reachable": reachable,
            "active_backend": selected_backend,
        })

    if ctx.json_mode:
        ctx.output("targets.list", {"targets": rows})
    else:
        console.print()
        console.print("[bold]Targets[/]")
        for row in rows:
            status = "[green]reachable[/]" if row["reachable"] else "[red]unreachable[/]"
            console.print(
                f"  {row['name']:<16} {row['backend']:<12} "
                f"{row['platform']:<16} {status}"
            )
        console.print()


@targets.command("test")
@click.argument("name")
@click.pass_obj
def targets_test(ctx: Context, name: str) -> None:
    """Probe a single target and report reachability."""
    config = ctx.config
    if name not in config.targets:
        render_error(f"Target '{name}' not configured")
        sys.exit(1)
    dispatcher = _make_dispatcher(config)
    target_config = dict(config.targets[name])
    target_config["name"] = name
    reachable, selected_backend = _probe_target(target_config, dispatcher)
    if ctx.json_mode:
        ctx.output("targets.test", {
            "name": name,
            "reachable": reachable,
            "active_backend": selected_backend,
        })
    else:
        if reachable:
            render_message(f"{name}: reachable via {selected_backend}", style="green")
        else:
            render_message(f"{name}: unreachable", style="red")
            sys.exit(1)


@targets.command("add")
@click.argument("name")
@click.option(
    "--backend",
    type=click.Choice(["local", "ssh", "ssh-windows", "cloud"]),
    required=True,
    help="Backend type for this target",
)
@click.option("--platform", help="Platform identifier (e.g. linux-x64)")
@click.option("--host", help="SSH host (alias or user@host) — required for ssh/ssh-windows")
@click.option("--repo-path", help="Remote repo path (ssh/ssh-windows)")
@click.pass_obj
def targets_add(
    ctx: Context,
    name: str,
    backend: str,
    platform: str | None,
    host: str | None,
    repo_path: str | None,
) -> None:
    """Add a new target to the project config.

    Writes a new ``[targets.<name>]`` section to
    ``.shipyard/config.toml``. For SSH backends, probes the host
    before writing so the user gets immediate feedback.
    """
    if ctx.config.project_dir is None:
        render_error("No .shipyard/config.toml found. Run `shipyard init` first.")
        sys.exit(1)
    if name in ctx.config.targets:
        render_error(f"Target '{name}' already exists. Remove it first or pick another name.")
        sys.exit(1)
    if backend in ("ssh", "ssh-windows") and not host:
        render_error(f"--host is required for backend={backend}")
        sys.exit(1)

    new_target: dict[str, Any] = {"backend": backend}
    if platform:
        new_target["platform"] = platform
    if host:
        new_target["host"] = host
    if repo_path:
        new_target["repo_path"] = repo_path

    # Probe before writing so the user knows whether the new target
    # is actually usable.
    if backend in ("ssh", "ssh-windows"):
        from shipyard.executor.ssh import SSHExecutor
        from shipyard.executor.ssh_windows import SSHWindowsExecutor
        executor = SSHExecutor() if backend == "ssh" else SSHWindowsExecutor()
        probe_target = {**new_target, "name": name}
        reachable = executor.probe(probe_target)
        if not reachable and not ctx.json_mode:
            render_message(
                f"warning: {host} is not reachable right now. Adding anyway.",
                style="bold yellow",
            )

    config_path = ctx.config.project_dir / "config.toml"
    _append_target_section(config_path, name, new_target)

    if ctx.json_mode:
        ctx.output("targets.add", {"name": name, "config": new_target})
    else:
        render_message(f"Added target '{name}' to {config_path}", style="green")


@targets.group("warm", invoke_without_command=True)
@click.pass_context
def targets_warm(ctx: click.Context) -> None:
    """Inspect and drain the warm-pool of reusable runners.

    With no subcommand, lists live entries — same as
    ``shipyard targets warm status``.
    """
    if ctx.invoked_subcommand is None:
        ctx.invoke(targets_warm_status)


@targets_warm.command("status")
@click.pass_obj
def targets_warm_status(ctx: Context) -> None:
    """Show live warm-pool entries (target, host, TTL remaining, SHA).

    Expired entries are pruned from the file as a side effect so
    repeated calls don't surface stale rows.
    """
    pool = WarmPool(default_pool_path(ctx.config.state_dir))
    pool.prune_expired()
    entries = pool.all_entries()

    rows: list[dict[str, Any]] = []
    for entry in entries:
        rows.append({
            "target": entry.target,
            "host": entry.host,
            "backend": entry.backend,
            "workdir": entry.workdir,
            "sha": entry.sha,
            "ttl_remaining_secs": round(entry.ttl_remaining_secs(), 1),
            "expires_at": datetime.fromtimestamp(
                entry.expires_at, tz=timezone.utc
            ).isoformat(),
            "created_at": datetime.fromtimestamp(
                entry.created_at, tz=timezone.utc
            ).isoformat(),
        })

    if ctx.json_mode:
        ctx.output("targets.warm.status", {"entries": rows})
        return

    if not rows:
        render_message("Warm pool is empty.", style="dim")
        return

    console.print()
    console.print("[bold]Warm pool[/]")
    for row in rows:
        ttl = row["ttl_remaining_secs"]
        console.print(
            f"  {row['target']:<16} {row['host']:<20} "
            f"sha={row['sha'][:8]} ttl={ttl:>6.0f}s "
            f"workdir={row['workdir']}"
        )
    console.print()


@targets_warm.command("drain")
@click.option(
    "--yes",
    is_flag=True,
    help="Skip the confirmation prompt.",
)
@click.pass_obj
def targets_warm_drain(ctx: Context, yes: bool) -> None:
    """Remove every warm-pool entry.

    Useful after a host reboot, a runner image change, or any other
    event that invalidates the workdirs the pool is tracking. Next
    ship on every target cold-starts.
    """
    pool = WarmPool(default_pool_path(ctx.config.state_dir))
    entries = pool.all_entries()
    if not entries:
        if ctx.json_mode:
            ctx.output("targets.warm.drain", {"drained": 0})
        else:
            render_message("Warm pool already empty.", style="dim")
        return

    if not yes and not ctx.json_mode and not click.confirm(
        f"Drain {len(entries)} warm-pool entr"
        f"{'y' if len(entries) == 1 else 'ies'}?",
        default=False,
    ):
        render_message("Aborted.", style="dim")
        return

    drained = pool.drain()
    if ctx.json_mode:
        ctx.output("targets.warm.drain", {"drained": drained})
    else:
        render_message(f"Drained {drained} warm-pool entries.", style="green")


@targets.command("remove")
@click.argument("name")
@click.pass_obj
def targets_remove(ctx: Context, name: str) -> None:
    """Remove a target from the project config."""
    if ctx.config.project_dir is None:
        render_error("No .shipyard/config.toml found.")
        sys.exit(1)
    if name not in ctx.config.targets:
        render_error(f"Target '{name}' not found")
        sys.exit(1)
    config_path = ctx.config.project_dir / "config.toml"
    _remove_target_section(config_path, name)
    if ctx.json_mode:
        ctx.output("targets.remove", {"name": name})
    else:
        render_message(f"Removed target '{name}' from {config_path}", style="green")


def _append_target_section(
    config_path: Path, name: str, target_config: dict[str, Any],
) -> None:
    """Append a new ``[targets.<name>]`` section to a TOML config file.

    Uses raw text append so existing comments and ordering are
    preserved. Each value is rendered with `tomli_w` to ensure
    correct escaping.
    """
    import tomli_w
    section = tomli_w.dumps({"targets": {name: target_config}})
    text = config_path.read_text()
    if not text.endswith("\n"):
        text += "\n"
    if not text.endswith("\n\n"):
        text += "\n"
    config_path.write_text(text + section)


def _remove_target_section(config_path: Path, name: str) -> None:
    """Remove the ``[targets.<name>]`` section from a TOML config file.

    Line-level edit so unrelated comments and ordering are preserved.
    """
    text = config_path.read_text()
    lines = text.splitlines(keepends=True)
    out: list[str] = []
    skipping = False
    section_marker = f"[targets.{name}]"
    for line in lines:
        stripped = line.strip()
        if stripped == section_marker:
            skipping = True
            continue
        if skipping and stripped.startswith("[") and stripped.endswith("]"):
            skipping = False
            out.append(line)
            continue
        if not skipping:
            out.append(line)
    config_path.write_text("".join(out))


# ── config commands ──────────────────────────────────────────────────


@main.group(invoke_without_command=True, name="config")
@click.pass_context
def config_cmd(ctx: click.Context) -> None:
    """Inspect and switch project profiles and configuration.

    With no subcommand, prints the effective merged config.
    """
    if ctx.invoked_subcommand is None:
        ctx.invoke(config_show)


@config_cmd.command("show")
@click.pass_obj
def config_show(ctx: Context) -> None:
    """Print the effective merged configuration as JSON."""
    import json as _json
    if ctx.json_mode:
        ctx.output("config.show", {"config": ctx.config.to_dict()})
    else:
        render_message(_json.dumps(ctx.config.to_dict(), indent=2))


@config_cmd.command("profiles")
@click.pass_obj
def config_profiles(ctx: Context) -> None:
    """List defined profiles and which one is active."""
    profiles = ctx.config.get("profiles", {}) or {}
    active = str(ctx.config.get("project.profile", "")) or None
    rows: list[dict[str, Any]] = []
    for name, body in profiles.items():
        rows.append({
            "name": name,
            "active": name == active,
            "targets": list((body or {}).get("targets", [])),
        })
    if ctx.json_mode:
        ctx.output("config.profiles", {"profiles": rows, "active": active})
    else:
        if not rows:
            render_message("No profiles defined. See docs/profiles.md.")
            return
        console.print()
        console.print("[bold]Profiles[/]")
        for row in rows:
            marker = "  [green]← active[/]" if row["active"] else ""
            console.print(
                f"  {row['name']:<10} {', '.join(row['targets'])}{marker}"
            )
        console.print()


@config_cmd.command("use")
@click.argument("profile_name")
@click.pass_obj
def config_use(ctx: Context, profile_name: str) -> None:
    """Switch the active profile.

    This is a project-config-only operation — it edits
    ``[project].profile`` in ``.shipyard/config.toml``. To switch the
    *governance* profile (and apply branch protection at the same
    time), use ``shipyard governance use``.
    """
    if ctx.config.project_dir is None:
        render_error("No .shipyard/config.toml found. Run `shipyard init` first.")
        sys.exit(1)
    profiles = ctx.config.get("profiles", {}) or {}
    if profile_name not in profiles:
        render_error(
            f"Profile '{profile_name}' is not defined. "
            f"Known profiles: {', '.join(profiles.keys()) or '(none)'}"
        )
        sys.exit(1)
    config_path = ctx.config.project_dir / "config.toml"
    _rewrite_profile_in_config(config_path, profile_name)
    if ctx.json_mode:
        ctx.output("config.use", {"profile": profile_name})
    else:
        render_message(
            f"Switched to profile '{profile_name}' in {config_path}",
            style="green",
        )


# ── quarantine (flaky targets) ──────────────────────────────────────


@main.group(name="quarantine", invoke_without_command=True)
@click.pass_context
def quarantine_group(ctx: click.Context) -> None:
    """Manage the flaky-target quarantine list.

    Targets listed in ``.shipyard/quarantine.toml`` whose failure class
    is TEST or UNKNOWN are treated as advisory during the merge
    decision — they don't block ``shipyard ship`` / ``auto-merge``.
    INFRA / TIMEOUT / CONTRACT failures are still authoritative.

    With no subcommand, lists the quarantined targets.
    """
    if ctx.invoked_subcommand is None:
        ctx.invoke(quarantine_list)


def _quarantine_path_or_exit(ctx: Context) -> Path:
    if ctx.config.project_dir is None:
        render_error("No .shipyard/ directory found. Run `shipyard init` first.")
        sys.exit(1)
    from shipyard.core.quarantine import QUARANTINE_FILENAME
    return ctx.config.project_dir / QUARANTINE_FILENAME


@quarantine_group.command("list")
@click.pass_obj
def quarantine_list(ctx: Context) -> None:
    """List quarantined targets."""
    from shipyard.core.quarantine import QuarantineList

    q = QuarantineList.load_from_project(ctx.config.project_dir)
    if ctx.json_mode:
        ctx.output("quarantine.list", q.to_dict())
        return
    if not q.entries:
        render_message(
            "No quarantined targets. "
            "(.shipyard/quarantine.toml is absent or empty.)",
            style="dim",
        )
        return
    console.print()
    console.print("[bold]Quarantined targets[/]")
    for entry in q.entries:
        reason = f" — {entry.reason}" if entry.reason else ""
        date_str = f" (added {entry.added_at})" if entry.added_at else ""
        console.print(f"  {entry.target}{reason}{date_str}")
    console.print()


@quarantine_group.command("add")
@click.argument("target")
@click.option("--reason", default="", help="Free-form note (recommended).")
@click.pass_obj
def quarantine_add(ctx: Context, target: str, reason: str) -> None:
    """Add a target to the quarantine list."""
    from shipyard.core.quarantine import QuarantineList

    path = _quarantine_path_or_exit(ctx)
    q = QuarantineList.load(path)
    q.path = path  # load() preserves path; defensive re-set
    added = q.add(target, reason=reason)
    if added:
        q.save()
    if ctx.json_mode:
        ctx.output(
            "quarantine.add",
            {"target": target, "added": added, "path": str(path)},
        )
    else:
        if added:
            render_message(
                f"Quarantined '{target}' — TEST/UNKNOWN failures on this "
                f"target will be advisory.",
                style="green",
            )
        else:
            render_message(
                f"'{target}' was already quarantined.", style="dim"
            )


@quarantine_group.command("remove")
@click.argument("target")
@click.pass_obj
def quarantine_remove(ctx: Context, target: str) -> None:
    """Remove a target from the quarantine list."""
    from shipyard.core.quarantine import QuarantineList

    path = _quarantine_path_or_exit(ctx)
    q = QuarantineList.load(path)
    q.path = path
    removed = q.remove(target)
    if removed:
        q.save()
    if ctx.json_mode:
        ctx.output(
            "quarantine.remove",
            {"target": target, "removed": removed, "path": str(path)},
        )
    else:
        if removed:
            render_message(
                f"Removed '{target}' from quarantine.", style="green"
            )
        else:
            render_message(
                f"'{target}' was not quarantined.", style="dim"
            )


# ── changelog / post-release docs sync ──────────────────────────────


@main.group(name="changelog")
def changelog_group() -> None:
    """Generate and check CHANGELOG.md from git tags.

    Opt-in via ``[release.changelog]`` in ``.shipyard/config.toml``. An
    absent section means ``shipyard changelog`` refuses to touch the file.
    """


def _load_changelog_cfg(ctx: Context) -> Any:
    """Shared loader — returns the ChangelogConfig or exits on missing/disabled."""
    from shipyard.changelog.generator import load_changelog_config

    cfg = load_changelog_config(ctx.config)
    if not cfg.enabled:
        if ctx.json_mode:
            ctx.output(
                "changelog:error",
                {
                    "error": "disabled",
                    "message": (
                        "No [release.changelog] section in .shipyard/config.toml, "
                        "or `enabled = false`. Run `shipyard changelog init` to "
                        "opt in."
                    ),
                },
            )
        else:
            render_error(
                "No [release.changelog] section enabled in .shipyard/config.toml."
            )
            render_message(
                "Run `shipyard changelog init` to opt in.",
                style="dim",
            )
        sys.exit(2)
    if not cfg.repo_url:
        if ctx.json_mode:
            ctx.output(
                "changelog:error",
                {
                    "error": "missing_repo_url",
                    "message": "release.changelog.repo_url is required",
                },
            )
        else:
            render_error(
                "release.changelog.repo_url is required — set it in "
                ".shipyard/config.toml."
            )
        sys.exit(2)
    return cfg


@changelog_group.command("regenerate")
@click.option("--check", is_flag=True, help="Exit 1 if the file is out of date.")
@click.option(
    "--release-notes",
    "release_notes_tag",
    metavar="TAG",
    default=None,
    help="Print release notes for TAG to stdout instead of writing the file.",
)
@click.option(
    "--stdout",
    is_flag=True,
    help="Write the rendered CHANGELOG to stdout instead of the configured path.",
)
@click.pass_obj
def changelog_regenerate(
    ctx: Context,
    check: bool,
    release_notes_tag: str | None,
    stdout: bool,
) -> None:
    """Rebuild CHANGELOG.md from the configured tag graph.

    With ``--check`` the file is compared to the rendered output; exit 1
    on drift so CI can catch stale docs. With ``--release-notes TAG`` the
    per-release markdown is printed to stdout for release-page bodies.
    """
    from shipyard.changelog.generator import (
        build_entries,
        changelog_path,
        render_changelog,
        render_release_notes,
    )

    cfg = _load_changelog_cfg(ctx)
    entries = build_entries(cfg)

    if release_notes_tag:
        by_tag = {e.tag: e for e in entries}
        target = by_tag.get(release_notes_tag)
        if not target:
            if ctx.json_mode:
                ctx.output(
                    "changelog:release-notes",
                    {"tag": release_notes_tag, "error": "not_found"},
                )
            else:
                render_error(
                    f"No tag {release_notes_tag!r} with user-visible merges."
                )
            sys.exit(2)
        idx = entries.index(target)
        prev = entries[idx + 1] if idx + 1 < len(entries) else None
        notes = render_release_notes(target, prev, cfg)
        sys.stdout.write(notes)
        return

    rendered = render_changelog(entries, cfg)
    path = changelog_path(cfg)

    if stdout:
        sys.stdout.write(rendered)
        return

    current = path.read_text() if path.exists() else ""
    drift = current != rendered

    if check:
        if drift:
            if ctx.json_mode:
                ctx.output(
                    "changelog:check",
                    {
                        "path": str(path),
                        "drift": True,
                        "versions": len(entries),
                    },
                )
            else:
                render_error(
                    f"{path} is out of date. "
                    f"Run `shipyard changelog regenerate` to regenerate."
                )
            sys.exit(1)
        if ctx.json_mode:
            ctx.output(
                "changelog:check",
                {"path": str(path), "drift": False, "versions": len(entries)},
            )
        else:
            render_message(f"{path} is in sync ({len(entries)} versions).")
        return

    path.write_text(rendered)
    if ctx.json_mode:
        ctx.output(
            "changelog:regenerate",
            {
                "path": str(path),
                "versions": len(entries),
                "drift_before": drift,
            },
        )
    else:
        render_message(f"Wrote {path} ({len(entries)} versions).")


@changelog_group.command("check")
@click.pass_context
def changelog_check(click_ctx: click.Context) -> None:
    """Alias for ``shipyard changelog regenerate --check``."""
    click_ctx.invoke(changelog_regenerate, check=True, release_notes_tag=None, stdout=False)


@changelog_group.command("init")
@click.option(
    "--product",
    default=None,
    help="Human-facing product name (defaults to project.name or directory name).",
)
@click.option(
    "--repo-url",
    default=None,
    help="GitHub repo URL. Auto-detected from `origin` if omitted.",
)
@click.option(
    "--force",
    is_flag=True,
    help="Overwrite an existing [release.changelog] section.",
)
@click.pass_obj
def changelog_init(
    ctx: Context,
    product: str | None,
    repo_url: str | None,
    force: bool,
) -> None:
    """Scaffold ``[release.changelog]`` + ``[release.post_tag_hook]`` stubs.

    Writes to ``.shipyard/config.toml`` (creating it if absent). If
    ``CHANGELOG.md`` already exists, prints a warning: shipyard will
    overwrite it on the next regen.
    """
    import shutil as _shutil

    cwd = Path.cwd()
    project_dir = cwd / ".shipyard"
    project_dir.mkdir(exist_ok=True)
    cfg_path = project_dir / "config.toml"

    resolved_repo = repo_url or _detect_repo_url_or_empty()
    resolved_product = product or ctx.config.get("project.name") or cwd.name

    section_marker = "[release.changelog]"
    existing_text = cfg_path.read_text() if cfg_path.exists() else ""
    if section_marker in existing_text and not force:
        if ctx.json_mode:
            ctx.output(
                "changelog:init",
                {
                    "path": str(cfg_path),
                    "status": "already_configured",
                },
            )
        else:
            render_message(
                f"[release.changelog] already present in {cfg_path}. "
                "Pass --force to overwrite."
            )
        return

    changelog_exists = (cwd / "CHANGELOG.md").exists()

    stub = (
        "\n"
        "[release.changelog]\n"
        "enabled    = true\n"
        f'path       = "CHANGELOG.md"\n'
        f'repo_url   = "{resolved_repo}"\n'
        f'tag_filter = "v*"\n'
        f'product    = "{resolved_product}"\n'
        "skip_commit_patterns = [\n"
        '  "^chore: bump",\n'
        '  "^chore\\\\(release\\\\):",\n'
        '  "^bump .*to v?\\\\d+\\\\.\\\\d+\\\\.\\\\d+$",\n'
        "]\n"
        "\n"
        "[release.post_tag_hook]\n"
        "enabled              = true\n"
        'command              = "shipyard changelog regenerate"\n'
        'watch                = ["CHANGELOG.md"]\n'
        "trailers = [\n"
        "  'Version-Bump: sdk=skip reason=\"docs-only automated regeneration\"',\n"
        "  'Skill-Update: skip skill=ci reason=\"no workflow shape change\"',\n"
        "  'Release: skip reason=\"bot commit; prevent recursive auto-release\"',\n"
        "]\n"
        'only_for_tag_pattern = "v*"\n'
        "max_push_attempts    = 5\n"
        "\n"
        "[release.post_tag_hook.bot_identity]\n"
        'name  = "shipyard-release-bot"\n'
        'email = "shipyard-release-bot@users.noreply.github.com"\n'
    )

    if force and section_marker in existing_text:
        # Strip any existing release.* sections and re-append cleanly.
        existing_text = _strip_release_sections(existing_text)

    new_text = (existing_text.rstrip() + "\n" if existing_text.strip() else "") + stub
    # Back up any existing CHANGELOG so the "shipyard owns this now" flip
    # never destroys hand-maintained content silently.
    backup_path: Path | None = None
    if changelog_exists:
        backup_path = cwd / "CHANGELOG.md.pre-shipyard.bak"
        if not backup_path.exists():
            _shutil.copy2(cwd / "CHANGELOG.md", backup_path)

    cfg_path.write_text(new_text)

    if ctx.json_mode:
        ctx.output(
            "changelog:init",
            {
                "path": str(cfg_path),
                "status": "written",
                "existing_changelog": changelog_exists,
                "changelog_backup": str(backup_path) if backup_path else None,
                "repo_url": resolved_repo,
                "product": resolved_product,
            },
        )
        return

    render_message(f"Wrote {cfg_path}", style="green")
    if changelog_exists:
        render_message("")
        render_message(
            "Heads up: CHANGELOG.md already exists. Shipyard will "
            "overwrite it on the next `shipyard changelog regenerate` run.",
            style="bold yellow",
        )
        if backup_path:
            render_message(
                f"Backed up existing file to {backup_path} so you can "
                "hand-merge anything that matters.",
                style="dim",
            )
    render_message("")
    render_message("Next steps:", style="bold")
    render_message("  1. shipyard changelog regenerate")
    render_message("  2. shipyard release-bot hook install")


def _strip_release_sections(toml_text: str) -> str:
    """Remove any ``[release.*]`` sections from a TOML document string."""
    lines = toml_text.splitlines(keepends=True)
    out: list[str] = []
    skipping = False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("[release.") or stripped == "[release]":
            skipping = True
            continue
        if skipping:
            if stripped.startswith("[") and stripped.endswith("]") and not stripped.startswith("[release"):
                skipping = False
                out.append(line)
            # else: keep skipping
            continue
        out.append(line)
    return "".join(out)


def _detect_repo_url_or_empty() -> str:
    """Read ``origin`` URL from git and return an ``https://github.com/...`` form."""
    try:
        url = subprocess.check_output(
            ["git", "config", "--get", "remote.origin.url"],
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
    except subprocess.CalledProcessError:
        return ""
    if url.startswith("git@github.com:"):
        slug = url.removeprefix("git@github.com:").removesuffix(".git")
        return f"https://github.com/{slug}"
    if url.startswith("https://github.com/"):
        return url.removesuffix(".git")
    return url


# ── release-bot hook subgroup ───────────────────────────────────────


@release_bot_group.group(name="hook")
def release_bot_hook_group() -> None:
    """Install and run the post-tag docs-sync workflow."""


@release_bot_hook_group.command("install")
@click.option(
    "--tag-pattern",
    default=None,
    help="Glob of tags that should trigger the workflow (default `v*`).",
)
@click.option(
    "--shipyard-version",
    default=None,
    help="Pinned shipyard version the workflow curls in. Defaults to the current CLI version.",
)
@click.pass_obj
def release_bot_hook_install(
    ctx: Context,
    tag_pattern: str | None,
    shipyard_version: str | None,
) -> None:
    """Drop ``.github/workflows/post-tag-sync.yml`` into the consumer repo.

    Shipyard owns this file end-to-end — re-running overwrites it in
    place. Uninstall is a plain ``rm`` (no subcommand yet; the file
    name is deliberate).
    """
    from shipyard.changelog.workflow import (
        DEFAULT_SHIPYARD_VERSION,
        WorkflowOptions,
        render_workflow,
    )

    opts = WorkflowOptions(
        tag_pattern=tag_pattern or "v*",
        shipyard_version=shipyard_version or __version__ or DEFAULT_SHIPYARD_VERSION,
    )
    body = render_workflow(opts)

    workflows_dir = Path.cwd() / ".github" / "workflows"
    workflows_dir.mkdir(parents=True, exist_ok=True)
    target = workflows_dir / "post-tag-sync.yml"
    existed = target.exists()
    target.write_text(body)

    if ctx.json_mode:
        ctx.output(
            "release-bot:hook:install",
            {
                "path": str(target),
                "overwrote": existed,
                "shipyard_version": opts.shipyard_version,
                "tag_pattern": opts.tag_pattern,
            },
        )
        return

    verb = "Overwrote" if existed else "Wrote"
    render_message(f"{verb} {target}", style="green")
    render_message(
        f"  - fires on tag push matching {opts.tag_pattern!r}",
        style="dim",
    )
    render_message(
        f"  - installs shipyard {opts.shipyard_version} before running the hook",
        style="dim",
    )


@release_bot_hook_group.command("run")
@click.option(
    "--tag",
    "tag",
    default=None,
    help="Tag to sync. If omitted, reads $GITHUB_REF (strips refs/tags/).",
)
@click.pass_obj
def release_bot_hook_run(ctx: Context, tag: str | None) -> None:
    """Execute the configured ``[release.post_tag_hook]`` for a tag."""
    import os as _os

    from shipyard.changelog.hook import load_hook_config, run_hook

    cfg = load_hook_config(ctx.config)
    if not cfg.enabled:
        if ctx.json_mode:
            ctx.output(
                "release-bot:hook:run",
                {
                    "tag": tag or "",
                    "ran_command": False,
                    "command_exit": 0,
                    "watched_diffed": [],
                    "committed": False,
                    "pushed": False,
                    "attempts": 0,
                    "skipped_reason": "hook disabled in config",
                    "error": None,
                },
            )
        else:
            render_message(
                "No [release.post_tag_hook] enabled. "
                "Run `shipyard changelog init` to opt in.",
                style="dim",
            )
        return

    resolved_tag = tag
    if not resolved_tag:
        ref = _os.environ.get("GITHUB_REF", "")
        if ref.startswith("refs/tags/"):
            resolved_tag = ref.removeprefix("refs/tags/")
    if not resolved_tag:
        render_error("--tag is required (or set GITHUB_REF=refs/tags/<tag>).")
        sys.exit(2)

    result = run_hook(cfg, resolved_tag)

    payload = {
        "tag": resolved_tag,
        "ran_command": result.ran_command,
        "command_exit": result.command_exit,
        "watched_diffed": result.watched_diffed,
        "committed": result.committed,
        "pushed": result.pushed,
        "attempts": result.attempts,
        "skipped_reason": result.skipped_reason,
        "error": result.error,
    }

    if ctx.json_mode:
        ctx.output("release-bot:hook:run", payload)
    else:
        if result.skipped_reason:
            render_message(f"skipped: {result.skipped_reason}", style="dim")
        elif result.error:
            render_error(f"hook failed: {result.error}")
        elif not result.committed:
            render_message(
                f"Watched files in sync for {resolved_tag}; nothing to commit.",
                style="dim",
            )
        elif result.pushed:
            render_message(
                f"Pushed docs sync commit for {resolved_tag} "
                f"({len(result.watched_diffed)} files, {result.attempts} attempt(s)).",
                style="green",
            )
        else:
            render_message(
                f"Committed docs sync for {resolved_tag} but did not push.",
                style="yellow",
            )

    # Exit code semantics: best-effort. Errors do NOT roll back the
    # tag, but we still want CI to light up red so humans notice.
    if result.error and not ctx.json_mode:
        sys.exit(1)


# --- shipyard daemon ------------------------------------------------
#
# Webhook receiver daemon — see Shipyard issue #125. Runs a local
# webhook server behind a Tailscale Funnel tunnel, registers GitHub
# webhooks on tracked repos, and fans out events to IPC subscribers
# (including the macOS menu-bar app and other shipyard CLI
# invocations).


@main.group()
def daemon() -> None:
    """Run the live-mode webhook receiver + IPC event broker."""


@daemon.command("start")
@click.option(
    "--repo",
    "repos",
    multiple=True,
    help="Repo(s) to register webhooks on (overrides ship-state detection)",
)
@click.option(
    "--detach/--no-detach",
    default=True,
    help="Fork into the background (default). Use --no-detach for dev/debug.",
)
@pass_context
def daemon_start(ctx: Context, repos: tuple[str, ...], detach: bool) -> None:
    """Start the webhook daemon."""
    import os as _os

    from shipyard.daemon.runner import (
        DaemonSpawnFailedError,
        run_blocking,
        spawn_detached,
    )

    repo_list = _resolve_repo_list(ctx, repos)
    state_dir = ctx.config.state_dir
    if detach:
        try:
            pid = spawn_detached(state_dir=state_dir, repos=repo_list)
        except DaemonSpawnFailedError as exc:
            # The child died before the verification window expired.
            # Don't lie about it ("daemon started") — surface the log
            # tail so users see the real error (PyInstaller bundling,
            # port in use, tunnel permission denied, etc.).
            if ctx.json_mode:
                ctx.output(
                    "daemon:start",
                    {"ok": False, "error": str(exc), "repos": repo_list},
                )
            else:
                render_message(str(exc), style="red")
            sys.exit(3)
        if ctx.json_mode:
            ctx.output("daemon:start", {"pid": pid, "repos": repo_list})
        else:
            render_message(
                f"daemon started (pid {pid}); registering {len(repo_list)} repo(s).",
                style="green",
            )
        return
    # Foreground mode — helpful for `shipyard daemon run`-equivalent
    # debugging. Blocks until SIGINT/SIGTERM.
    _os.environ.setdefault("SHIPYARD_DAEMON_FOREGROUND", "1")
    exit_code = run_blocking(state_dir=state_dir, repos=repo_list)
    if exit_code != 0:
        sys.exit(exit_code)


@daemon.command("run")
@click.option("--repo", "repos", multiple=True)
@pass_context
def daemon_run(ctx: Context, repos: tuple[str, ...]) -> None:
    """Run the daemon in the foreground (logs to stdout). Useful for
    systemd / launchd unit files and for debugging."""
    from shipyard.daemon.runner import run_blocking

    repo_list = _resolve_repo_list(ctx, repos)
    exit_code = run_blocking(state_dir=ctx.config.state_dir, repos=repo_list)
    if exit_code != 0:
        sys.exit(exit_code)


@daemon.command("stop")
@pass_context
def daemon_stop(ctx: Context) -> None:
    """Ask a running daemon to exit gracefully."""
    from shipyard.daemon.runner import stop_running

    state_dir = ctx.config.state_dir
    stopped = stop_running(state_dir)
    if ctx.json_mode:
        ctx.output("daemon:stop", {"stopped": stopped})
    else:
        if stopped:
            render_message("daemon stopped.", style="green")
        else:
            render_message("daemon wasn't running.", style="dim")


@daemon.command("status")
@pass_context
def daemon_status(ctx: Context) -> None:
    """Report whether the daemon is running + tunnel + subscribers."""
    from shipyard.daemon.controller import read_daemon_status

    status = read_daemon_status(ctx.config.state_dir)
    if status is None:
        if ctx.json_mode:
            ctx.output("daemon:status", {"running": False})
        else:
            render_message("daemon is not running.", style="dim")
        return
    if ctx.json_mode:
        ctx.output("daemon:status", {"running": True, **status})
    else:
        tunnel_raw = status.get("tunnel")
        tunnel: dict[str, Any] = tunnel_raw if isinstance(tunnel_raw, dict) else {}
        url = tunnel.get("url") or "—"
        backend = tunnel.get("backend") or "—"
        subs = status.get("subscribers", 0)
        repos_raw = status.get("registered_repos")
        repos_list: list[str] = [
            str(r) for r in (repos_raw if isinstance(repos_raw, list) else [])
        ]
        repos_text = ", ".join(repos_list) or "—"
        render_message(
            f"daemon running · tunnel={backend} · {url}\n"
            f"subscribers={subs} · repos={repos_text}",
            style="green",
        )


def _resolve_repo_list(ctx: Context, explicit: tuple[str, ...]) -> list[str]:
    """If the user passed `--repo` args, honor them. Otherwise derive
    the list from the ship-state store so the daemon registers hooks
    on every repo the user has shipped from this machine."""
    if explicit:
        return list(explicit)
    try:
        states = ctx.ship_state.list_active()
    except Exception:  # pragma: no cover — defensive against uninit state dir
        return []
    repos: list[str] = []
    seen: set[str] = set()
    for state in states:
        repo = getattr(state, "repo", None)
        if repo and repo not in seen:
            seen.add(repo)
            repos.append(repo)
    return repos


@main.group(name="wait", invoke_without_command=False)
def wait_cmd() -> None:
    """Wait for a GitHub condition to match, then exit.

    Three subcommands: ``release``, ``pr``, ``run``. Each opens a daemon
    subscription first (if one is running), takes one authoritative
    ``gh`` snapshot, and either exits 0 if the condition already
    matches or keeps consuming events (daemon) or polling (fallback)
    until it does — or until ``--timeout`` elapses.

    Exit codes:
      0  matched
      1  --timeout elapsed
      4  ``wait run --success`` hit a terminal-but-wrong conclusion
      5  invalid input (missing PR, bad tag, etc.)
      6  daemon unavailable + snapshot missed + --no-fallback
      7  unsupported scope (rulesets / merge-queue)
      130 SIGINT / SIGTERM
    """


_WAIT_EXIT_MATCH = 0
_WAIT_EXIT_TIMEOUT = 1
_WAIT_EXIT_RUN_TERMINAL_WRONG = 4
_WAIT_EXIT_INVALID = 5
_WAIT_EXIT_NO_FALLBACK = 6
_WAIT_EXIT_UNSUPPORTED = 7


def _render_wait_outcome(
    ctx: Context,
    *,
    command: str,
    condition: dict[str, Any],
    outcome: Any,
) -> None:
    """Common ``--json`` + human renderer for wait subcommands."""
    data = {
        "matched": outcome.matched,
        "condition": condition,
        "observed": outcome.observed,
        "transport": outcome.transport,
        "fallback_used": outcome.fallback_used,
        "events_received": outcome.events_received,
        "elapsed_seconds": round(outcome.elapsed_seconds, 3),
    }
    if ctx.json_mode:
        ctx.output(command, data)
        return
    if outcome.matched:
        render_message(
            f"matched after {data['elapsed_seconds']}s "
            f"(transport={outcome.transport}, events={outcome.events_received})",
            style="green",
        )
        return
    if outcome.timed_out:
        render_error(
            f"timeout after {data['elapsed_seconds']}s "
            f"(transport={outcome.transport})"
        )
        return
    if outcome.fallback_disabled_hit:
        render_error(
            "daemon unavailable and snapshot didn't match; --no-fallback set"
        )


@wait_cmd.command("release")
@click.argument("version")
@click.option("--timeout", "timeout_seconds", type=float, default=600.0,
              help="Give up after N seconds (default 600).")
@click.option("--poll-interval", "poll_interval", type=float, default=2.0,
              help="Polling cadence when daemon is unavailable (default 2s).")
@click.option("--no-fallback", is_flag=True,
              help="Fail with exit 6 rather than poll when the daemon isn't available.")
@pass_context
def wait_release(
    ctx: Context,
    version: str,
    timeout_seconds: float,
    poll_interval: float,
    no_fallback: bool,
) -> None:
    """Wait for a release tag to exist and all manifest assets to be
    uploaded. Matches when:

    \b
      * a release with tag == VERSION exists, draft=false, AND
      * every artifact in [release.artifacts] is state=uploaded
        (or — if no manifest — at least one asset is uploaded).
    """
    from shipyard.ship import wait as wait_mod
    from shipyard.ship import wait_transport

    repo_ref = detect_repo_from_remote()
    if repo_ref is None:
        render_error("couldn't resolve the current repo from the git remote.")
        sys.exit(_WAIT_EXIT_INVALID)
    repo = repo_ref.slug

    manifest_raw = ctx.config.get("release.artifacts", [])
    manifest: list[str] = []
    if isinstance(manifest_raw, list):
        for entry in manifest_raw:
            if isinstance(entry, str):
                manifest.append(entry)
            elif isinstance(entry, dict):
                name = entry.get("name")
                if isinstance(name, str):
                    manifest.append(name)

    def _evaluator(snapshot: dict[str, Any] | None) -> wait_mod.TruthResult:
        return wait_mod.evaluate_release(snapshot, manifest=manifest or None)

    def _fetch() -> dict[str, Any] | None:
        return wait_transport.fetch_release_snapshot(repo=repo, tag=version)

    try:
        outcome = wait_transport.wait_for_condition(
            evaluator=_evaluator,
            fetch_snapshot=_fetch,
            event_filter=wait_transport.release_event_filter(version, repo),
            timeout_seconds=timeout_seconds,
            poll_interval_seconds=poll_interval,
            no_fallback=no_fallback,
        )
    except wait_mod.InvalidInputError as exc:
        render_error(str(exc))
        sys.exit(_WAIT_EXIT_INVALID)
    except KeyboardInterrupt:
        sys.exit(130)

    condition: dict[str, Any] = {
        "type": "release",
        "repo": repo,
        "tag": version,
        "manifest": manifest,
    }
    _render_wait_outcome(ctx, command="wait:release", condition=condition, outcome=outcome)
    if outcome.matched:
        sys.exit(_WAIT_EXIT_MATCH)
    if outcome.fallback_disabled_hit:
        sys.exit(_WAIT_EXIT_NO_FALLBACK)
    sys.exit(_WAIT_EXIT_TIMEOUT)


@wait_cmd.command("pr")
@click.argument("pr_number", type=int)
@click.option("--state", "state", type=click.Choice(["green", "merged", "closed"]),
              required=True, help="What PR state to wait for.")
@click.option("--timeout", "timeout_seconds", type=float, default=1800.0,
              help="Give up after N seconds (default 1800).")
@click.option("--poll-interval", "poll_interval", type=float, default=30.0,
              help="Polling cadence when daemon is unavailable (default 30s).")
@click.option("--no-fallback", is_flag=True,
              help="Fail with exit 6 rather than poll when the daemon isn't available.")
@pass_context
def wait_pr(
    ctx: Context,
    pr_number: int,
    state: str,
    timeout_seconds: float,
    poll_interval: float,
    no_fallback: bool,
) -> None:
    """Wait for a PR to reach a specific state.

    \b
      --state green   all required checks on current HEAD pass
      --state merged  PR is merged
      --state closed  PR is closed (merged or not)
    """
    from shipyard.ship import wait as wait_mod
    from shipyard.ship import wait_transport

    repo_ref = detect_repo_from_remote()
    if repo_ref is None:
        render_error("couldn't resolve the current repo from the git remote.")
        sys.exit(_WAIT_EXIT_INVALID)
    repo = repo_ref.slug

    if state == "green":
        def _evaluator(snapshot: dict[str, Any] | None) -> wait_mod.TruthResult:
            return wait_mod.evaluate_pr_green(snapshot)
    else:
        target_state = state
        def _evaluator(snapshot: dict[str, Any] | None) -> wait_mod.TruthResult:
            return wait_mod.evaluate_pr_state(snapshot, target_state=target_state)

    def _fetch() -> dict[str, Any] | None:
        return wait_transport.fetch_pr_snapshot(repo=repo, pr_number=pr_number)

    try:
        outcome = wait_transport.wait_for_condition(
            evaluator=_evaluator,
            fetch_snapshot=_fetch,
            event_filter=wait_transport.pr_event_filter(pr_number, repo),
            timeout_seconds=timeout_seconds,
            poll_interval_seconds=poll_interval,
            no_fallback=no_fallback,
        )
    except wait_mod.InvalidInputError as exc:
        render_error(str(exc))
        sys.exit(_WAIT_EXIT_INVALID)
    except wait_mod.UnsupportedScopeError as exc:
        render_error(str(exc))
        sys.exit(_WAIT_EXIT_UNSUPPORTED)
    except KeyboardInterrupt:
        sys.exit(130)

    condition: dict[str, Any] = {
        "type": f"pr_{state}",
        "pr": pr_number,
        "repo": repo,
        "head_sha": outcome.observed.get("head_sha") if isinstance(outcome.observed, dict) else None,
    }
    _render_wait_outcome(ctx, command="wait:pr", condition=condition, outcome=outcome)
    if outcome.matched:
        sys.exit(_WAIT_EXIT_MATCH)
    if outcome.fallback_disabled_hit:
        sys.exit(_WAIT_EXIT_NO_FALLBACK)
    sys.exit(_WAIT_EXIT_TIMEOUT)


@wait_cmd.command("run")
@click.argument("run_id")
@click.option("--success", is_flag=True,
              help="Require conclusion=success; any other terminal state exits 4.")
@click.option("--timeout", "timeout_seconds", type=float, default=1800.0,
              help="Give up after N seconds (default 1800).")
@click.option("--poll-interval", "poll_interval", type=float, default=15.0,
              help="Polling cadence when daemon is unavailable (default 15s).")
@click.option("--no-fallback", is_flag=True,
              help="Fail with exit 6 rather than poll when the daemon isn't available.")
@pass_context
def wait_run(
    ctx: Context,
    run_id: str,
    success: bool,
    timeout_seconds: float,
    poll_interval: float,
    no_fallback: bool,
) -> None:
    """Wait for a GitHub Actions workflow run to reach a terminal status.

    With ``--success`` the run must end with conclusion=success. Any
    other terminal conclusion exits 4 immediately — no reason to keep
    waiting on a run that's already decided.
    """
    from shipyard.ship import wait as wait_mod
    from shipyard.ship import wait_transport

    repo_ref = detect_repo_from_remote()
    if repo_ref is None:
        render_error("couldn't resolve the current repo from the git remote.")
        sys.exit(_WAIT_EXIT_INVALID)
    repo = repo_ref.slug

    def _evaluator(snapshot: dict[str, Any] | None) -> wait_mod.TruthResult:
        # Let RunFailedFast propagate out of the transport — the
        # CLI-level handler maps it to exit code 4 with the observed
        # state in the JSON envelope.
        return wait_mod.evaluate_run(snapshot, require_success=success)

    def _fetch() -> dict[str, Any] | None:
        return wait_transport.fetch_run_snapshot(repo=repo, run_id=run_id)

    condition: dict[str, Any] = {
        "type": "run",
        "run_id": run_id,
        "repo": repo,
        "require_success": success,
    }
    try:
        outcome = wait_transport.wait_for_condition(
            evaluator=_evaluator,
            fetch_snapshot=_fetch,
            event_filter=wait_transport.run_event_filter(run_id, repo),
            timeout_seconds=timeout_seconds,
            poll_interval_seconds=poll_interval,
            no_fallback=no_fallback,
        )
    except wait_mod.InvalidInputError as exc:
        render_error(str(exc))
        sys.exit(_WAIT_EXIT_INVALID)
    except wait_mod.RunFailedFastError as exc:
        # Terminal-but-wrong: render the observed state and exit 4.
        from shipyard.ship.wait_transport import WaitOutcome

        synthetic = WaitOutcome(
            matched=False,
            observed=exc.observed,
            transport="polling",
            fallback_used=False,
            events_received=0,
            timed_out=False,
        )
        _render_wait_outcome(
            ctx, command="wait:run", condition=condition, outcome=synthetic,
        )
        sys.exit(_WAIT_EXIT_RUN_TERMINAL_WRONG)
    except KeyboardInterrupt:
        sys.exit(130)

    _render_wait_outcome(ctx, command="wait:run", condition=condition, outcome=outcome)
    if outcome.matched:
        sys.exit(_WAIT_EXIT_MATCH)
    if outcome.fallback_disabled_hit:
        sys.exit(_WAIT_EXIT_NO_FALLBACK)
    sys.exit(_WAIT_EXIT_TIMEOUT)


if __name__ == "__main__":
    main()
