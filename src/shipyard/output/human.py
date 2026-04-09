"""Rich terminal output for humans.

All user-facing output goes through this module. Business logic never
calls print() directly — it returns data, and this module renders it.
"""

from __future__ import annotations

from typing import Any

from rich.console import Console
from rich.table import Table
from rich.text import Text

from shipyard.core.job import Job, JobStatus

console = Console()

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


def _style_status(status: str) -> Text:
    style = _STATUS_STYLES.get(status, "")
    return Text(status, style=style)


# ---- Job rendering ----


def render_job(job: Job) -> None:
    """Render a job's current state to the terminal."""
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
    if overall:
        console.print(f"  {overall}")
    console.print()


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
            icon = "[green]\u2713[/]" if ok else "[red]\u2717[/]"
            detail = info.get("version", info.get("error", ""))
            extra = ""
            if info.get("user"):
                extra = f" (as {info['user']})"
            elif info.get("workspace"):
                extra = f" ({info['workspace']})"
            elif info.get("latency_ms"):
                extra = f" ({info['latency_ms']}ms)"
            console.print(f"    {icon} {name} {detail}{extra}")
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
