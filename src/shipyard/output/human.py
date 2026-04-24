"""Rich terminal output for humans.

All user-facing output goes through this module. Business logic never
calls print() directly — it returns data, and this module renders it.

Rich itself pulls in ~60ms of imports on a cold start (rich.console
alone is ~48ms). Many `shipyard` invocations never render anything —
``shipyard --version``, ``--help``, JSON-mode commands, and the
daemon subprocess path — so we defer the ``rich`` imports until the
first actual render call. Ordinary Python module caching makes every
call after the first free. Perf/cold-start rationale: see #28.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from shipyard.core.job import Job, JobStatus

if TYPE_CHECKING:
    from rich.console import Console as _Console
    from rich.text import Text as _Text


class _LazyConsole:
    """Proxy that instantiates a real :class:`rich.console.Console` on
    first attribute access. ``console.print(...)`` anywhere in the
    codebase still works; the rich import just doesn't happen until
    the first call. Subsequent calls hit the cached instance.
    """

    _real: _Console | None = None

    def __getattr__(self, name: str) -> Any:
        cls = type(self)
        if cls._real is None:
            from rich.console import Console
            cls._real = Console()
        return getattr(cls._real, name)


console = _LazyConsole()

# ---- Status colors ----

_STATUS_STYLES: dict[str, str] = {
    "pass": "bold green",
    "fail": "bold red",
    "error": "bold red",
    "running": "bold yellow",
    "pending": "dim",
    "unreachable": "bold magenta",
    "cancelled": "dim",
}


def _style_status(status: str) -> _Text:
    from rich.text import Text
    style = _STATUS_STYLES.get(status, "")
    return Text(status, style=style)


# ---- Job rendering ----


def render_job(job: Job) -> None:
    """Render a job's current state to the terminal."""
    from rich.table import Table
    from rich.text import Text

    header = f"[bold]{job.id}[/] — {job.branch} @ {job.sha[:8]}"
    if job.mode.value == "smoke":
        header += " [dim](smoke)[/]"

    table = Table(show_header=True, header_style="bold", box=None, padding=(0, 2))
    table.add_column("Target", style="cyan")
    table.add_column("Status")
    table.add_column("Backend", style="dim")
    table.add_column("Duration", justify="right", style="dim")

    for name in job.target_names:
        result = job.results.get(name)
        if result:
            if result.reused_from:
                # Cross-PR reuse: show "reused" in the status column
                # so humans see the skipped lane and its provenance.
                status_text = Text("reused", style="green")
                backend = f"reused (from {result.reused_from[:7]})"
            else:
                status_text = _style_status(result.status.value)
                backend = result.backend
                if result.failover_reason:
                    backend = f"{result.backend} ({result.failover_reason})"
            duration = _format_duration(result.duration_secs) if result.duration_secs else "..."
        else:
            status_text = _style_status("pending")
            backend = ""
            duration = ""
        table.add_row(name, status_text, backend, duration)

    overall = ""
    if job.status == JobStatus.COMPLETED:
        overall = "[bold green]All green.[/]" if job.passed else "[bold red]Failed.[/]"
    elif job.status == JobStatus.RUNNING:
        overall = "[yellow]Running...[/]"
    elif job.status == JobStatus.CANCELLED:
        overall = "[dim]Cancelled.[/]"

    console.print()
    console.print(header)
    console.print(table)
    _render_target_errors(job)
    if overall:
        console.print(f"  {overall}")
    console.print()


def _render_target_errors(job: Job) -> None:
    """Emit one indented line per non-passing target with its
    ``error_message`` and log path.

    Without this, the summary table collapses a bundle-upload failure
    or remote-apply failure into a bare "error" cell and the user has
    to `cat` the log file by hand to know what happened. See #169.

    Backend error text is *uncontrolled* — it comes from raw exception
    strings (``str(exc)``, SSH stderr, remote cmd stderr) and often
    carries bracketed fragments like ``[Errno 2]`` or
    ``[WinError 32]``. Rich's markup parser would otherwise treat
    those as style tags and either render garbage or raise
    ``MarkupError`` mid-flush, turning a target failure into a CLI
    rendering crash. Pass every dynamic segment through
    ``rich.markup.escape``. Codex-review catch post-#170.
    """
    from rich.markup import escape

    for name in job.target_names:
        result = job.results.get(name)
        if result is None or result.passed:
            continue
        if result.reused_from:
            continue
        msg = result.error_message
        if not msg:
            continue
        # Trim to keep multi-paragraph tracebacks from ballooning the
        # terminal; the log file has the full text.
        first_line = msg.splitlines()[0].strip()
        if len(first_line) > 200:
            first_line = first_line[:200].rstrip() + "…"
        console.print(f"  [red]✗ {escape(name)}:[/] {escape(first_line)}")
        if result.log_path:
            console.print(f"    [dim]log: {escape(result.log_path)}[/]")


def render_status(
    active: Job | None,
    pending_count: int,
    recent: list[Job],
    targets_info: dict[str, dict[str, Any]],
) -> None:
    """Render the full status dashboard."""
    console.print()
    console.print("[bold]Shipyard[/]")
    console.print()

    # Queue section
    console.print("  [bold]Queue:[/]")
    if active:
        console.print(f"    active:  {active.id} ({active.branch} @ {active.sha[:8]})")
    else:
        console.print("    active:  [dim]none[/]")
    console.print(f"    pending: {pending_count}")
    console.print(f"    recent:  {len(recent)} completed")

    # Active run detail
    if active:
        console.print()
        console.print("  [bold]Active run:[/]")
        for name in active.target_names:
            result = active.results.get(name)
            if result:
                status = _style_status(result.status.value)
                extra = f"  ({result.backend}"
                if result.phase:
                    extra += f", phase={result.phase}"
                if result.liveness:
                    extra += f", liveness={result.liveness}"
                if result.quiet_for_secs is not None:
                    extra += f", idle={int(result.quiet_for_secs)}s"
                if result.duration_secs:
                    extra += f", {_format_duration(result.duration_secs)}"
                extra += ")"
            else:
                status = _style_status("pending")
                extra = ""
            console.print(f"    {name:12s} ", end="")
            console.print(status, end="")
            console.print(f" [dim]{extra}[/]")

    # Targets section
    if targets_info:
        console.print()
        console.print("  [bold]Targets:[/]")
        for name, info in targets_info.items():
            reachable = info.get("reachable", False)
            backend = info.get("backend", "?")
            if reachable:
                latency = info.get("latency_ms")
                lat_str = f"  {latency}ms" if latency else ""
                console.print(f"    {name:12s} {backend:12s} [green]reachable[/]{lat_str}")
            else:
                fallback = info.get("fallback", "")
                fb_str = f" [dim]→ fallback: {fallback}[/]" if fallback else ""
                console.print(f"    {name:12s} {backend:12s} [red]unreachable[/]{fb_str}")

    console.print()


def render_evidence(records: dict[str, dict[str, Any]]) -> None:
    """Render evidence for a branch."""
    from rich.table import Table
    from rich.text import Text

    table = Table(show_header=True, header_style="bold", box=None, padding=(0, 2))
    table.add_column("Platform", style="cyan")
    table.add_column("Status")
    table.add_column("SHA", style="dim")
    table.add_column("When", style="dim")
    table.add_column("Backend", style="dim")

    for platform, info in records.items():
        if info:
            status = _style_status(info["status"])
            table.add_row(
                platform,
                status,
                info.get("sha", "?")[:8],
                info.get("completed_at", "?"),
                info.get("backend", "?"),
            )
        else:
            table.add_row(platform, Text("—", style="dim"), "", "", "")

    console.print()
    console.print(table)
    console.print()


def render_doctor(checks: dict[str, Any], ready: bool) -> None:
    """Render doctor check results."""
    console.print()
    console.print("[bold]shipyard doctor[/]")
    console.print()

    for category, items in checks.items():
        console.print(f"  [bold]{category}:[/]")
        for name, info in items.items():
            ok = info.get("ok", False)
            icon = "[green]✓[/]" if ok else "[red]✗[/]"
            summary = info.get("version", info.get("error", info.get("detail", "")))
            extra = ""
            if info.get("user"):
                extra = f" (as {info['user']})"
            elif info.get("workspace"):
                extra = f" ({info['workspace']})"
            elif info.get("latency_ms"):
                extra = f" ({info['latency_ms']}ms)"
            console.print(f"    {icon} {name} {summary}{extra}")
            # Codex P2 on #214: pre-fix, `detail` was only rendered
            # when `version` was absent — which it never is for
            # failure rows that want to show actionable recovery info
            # (e.g. the rich-bundle reinstall hint on corrupt
            # PyInstaller installs). Show `detail` on every failing
            # row as indented follow-up lines so the user sees the
            # fix command without needing --json mode.
            failure_detail = info.get("detail") if not ok else None
            if failure_detail and failure_detail != summary:
                for line in failure_detail.splitlines():
                    if line.strip():
                        console.print(f"        [dim]{line.rstrip()}[/]")
        console.print()

    if ready:
        console.print("  [bold green]Overall: ready[/]")
    else:
        console.print("  [bold yellow]Overall: not ready (see above)[/]")
    console.print()


def render_message(msg: str, style: str = "") -> None:
    """Print a simple message."""
    if style:
        console.print(f"[{style}]{msg}[/]")
    else:
        console.print(msg)


def render_error(msg: str) -> None:
    """Print an error message to stderr."""
    console.print(f"[bold red]error:[/] {msg}", highlight=False)


# ---- Helpers ----


def _format_duration(secs: float | None) -> str:
    if secs is None:
        return ""
    if secs < 60:
        return f"{secs:.0f}s"
    minutes = int(secs // 60)
    remaining = int(secs % 60)
    return f"{minutes}m{remaining:02d}s"
