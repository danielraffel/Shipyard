"""Log, result, and bundle retention cleanup.

Scans the state directory for orphaned logs, stale results, and
leftover git bundles that are no longer referenced by any job in
the queue.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class CleanupItem:
    """An item identified for cleanup."""

    path: str
    kind: str  # "log", "result", "bundle", "evidence"
    size_bytes: int
    reason: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "path": self.path,
            "kind": self.kind,
            "size_bytes": self.size_bytes,
            "reason": self.reason,
        }


@dataclass(frozen=True)
class CleanupResult:
    """Summary of a cleanup run."""

    items: list[CleanupItem]
    total_bytes: int
    dry_run: bool

    def to_dict(self) -> dict[str, Any]:
        return {
            "items": [i.to_dict() for i in self.items],
            "total_bytes": self.total_bytes,
            "dry_run": self.dry_run,
            "count": len(self.items),
        }


def cleanup(state_dir: Path, dry_run: bool = True) -> CleanupResult:
    """Scan for orphaned logs, results, and bundles.

    Compares log directories against active job IDs in the queue.
    Items not referenced by any job are candidates for deletion.

    Args:
        state_dir: The shipyard state directory (contains queue/, logs/, evidence/, bundles/).
        dry_run: If True (default), only report what would be deleted.
                 If False, actually delete the files.

    Returns:
        CleanupResult with the list of items found/deleted and total size.
    """
    items: list[CleanupItem] = []

    # Load active job IDs from queue
    active_ids = _load_active_job_ids(state_dir / "queue")

    # Scan log directories
    logs_dir = state_dir / "logs"
    if logs_dir.exists():
        for job_dir in sorted(logs_dir.iterdir()):
            if job_dir.is_dir() and job_dir.name not in active_ids:
                size = _dir_size(job_dir)
                items.append(CleanupItem(
                    path=str(job_dir),
                    kind="log",
                    size_bytes=size,
                    reason=f"Job {job_dir.name} not in queue",
                ))
                if not dry_run:
                    _rmtree(job_dir)

    # Scan bundle directory
    bundles_dir = state_dir / "bundles"
    if bundles_dir.exists():
        for bundle_file in sorted(bundles_dir.iterdir()):
            if bundle_file.is_file() and bundle_file.suffix == ".bundle":
                # Bundles older than any active job are orphaned
                size = bundle_file.stat().st_size
                items.append(CleanupItem(
                    path=str(bundle_file),
                    kind="bundle",
                    size_bytes=size,
                    reason="Orphaned git bundle",
                ))
                if not dry_run:
                    bundle_file.unlink()

    # Scan stale evidence files for branches that no longer exist
    evidence_dir = state_dir / "evidence"
    if evidence_dir.exists():
        for evidence_file in sorted(evidence_dir.iterdir()):
            if evidence_file.is_file() and evidence_file.suffix == ".json":
                # Check if evidence file is empty or contains only failed records
                try:
                    data = json.loads(evidence_file.read_text())
                    if not data:
                        size = evidence_file.stat().st_size
                        items.append(CleanupItem(
                            path=str(evidence_file),
                            kind="evidence",
                            size_bytes=size,
                            reason="Empty evidence file",
                        ))
                        if not dry_run:
                            evidence_file.unlink()
                except (json.JSONDecodeError, OSError):
                    size = evidence_file.stat().st_size if evidence_file.exists() else 0
                    items.append(CleanupItem(
                        path=str(evidence_file),
                        kind="evidence",
                        size_bytes=size,
                        reason="Corrupt evidence file",
                    ))
                    if not dry_run and evidence_file.exists():
                        evidence_file.unlink()

    total_bytes = sum(i.size_bytes for i in items)

    return CleanupResult(
        items=items,
        total_bytes=total_bytes,
        dry_run=dry_run,
    )


def _load_active_job_ids(queue_dir: Path) -> set[str]:
    """Load job IDs from the queue file."""
    queue_file = queue_dir / "queue.json"
    if not queue_file.exists():
        return set()

    try:
        data = json.loads(queue_file.read_text())
        return {job["id"] for job in data.get("jobs", [])}
    except (json.JSONDecodeError, KeyError):
        return set()


def _dir_size(path: Path) -> int:
    """Calculate total size of a directory tree."""
    total = 0
    try:
        for f in path.rglob("*"):
            if f.is_file():
                total += f.stat().st_size
    except OSError:
        pass
    return total


def _rmtree(path: Path) -> None:
    """Remove a directory tree."""
    import shutil
    shutil.rmtree(path, ignore_errors=True)
