"""Flaky-target quarantine list.

When a target is listed in ``.shipyard/quarantine.toml`` and its failure
class is TEST or UNKNOWN, the ship layer treats the failure as
*advisory* — it records the failure but does not block the merge. This
gives maintainers a decoupled way to say "we know this target is flaky,
don't block the world on it" without having to delete the target or
edit workflow code.

Quarantine is deliberately narrow:

- It only suppresses TEST / UNKNOWN failures. INFRA / TIMEOUT /
  CONTRACT are still authoritative because they indicate real
  fixable problems (unreachable host, wall-clock cap, bypassed
  contract) that quarantine shouldn't paper over.
- It's an opt-in file. The absence of a quarantine file is a no-op —
  the ship layer behaves exactly as before.
- Entries carry a free-form ``reason`` so code review sees why a target
  was quarantined.

File format (``.shipyard/quarantine.toml``)::

    [[quarantine]]
    target = "windows-arm64"
    reason = "flaky Windows runner during Apr 2026 outage"
    added_at = "2026-04-18"

Add / remove / list is exposed via ``shipyard quarantine {add,remove,list}``.
"""

from __future__ import annotations

import sys
from dataclasses import dataclass, field
from datetime import date
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from pathlib import Path

if sys.version_info >= (3, 11):
    import tomllib
else:  # pragma: no cover
    import tomli as tomllib

import tomli_w

QUARANTINE_FILENAME = "quarantine.toml"


@dataclass(frozen=True)
class QuarantineEntry:
    """One quarantined target and why."""

    target: str
    reason: str = ""
    added_at: str = ""

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"target": self.target}
        if self.reason:
            d["reason"] = self.reason
        if self.added_at:
            d["added_at"] = self.added_at
        return d


@dataclass
class QuarantineList:
    """In-memory view of ``.shipyard/quarantine.toml``."""

    entries: list[QuarantineEntry] = field(default_factory=list)
    path: Path | None = None

    @classmethod
    def load(cls, path: Path | None) -> QuarantineList:
        """Load from disk. Missing file → empty list (not an error)."""
        if path is None or not path.exists():
            return cls(entries=[], path=path)
        data = tomllib.loads(path.read_text())
        raw = data.get("quarantine", [])
        entries = [
            QuarantineEntry(
                target=str(item.get("target", "")).strip(),
                reason=str(item.get("reason", "")).strip(),
                added_at=str(item.get("added_at", "")).strip(),
            )
            for item in raw
            if isinstance(item, dict) and item.get("target")
        ]
        return cls(entries=entries, path=path)

    @classmethod
    def load_from_project(cls, project_dir: Path | None) -> QuarantineList:
        """Locate the file under ``<project_dir>/quarantine.toml``."""
        if project_dir is None:
            return cls(entries=[], path=None)
        return cls.load(project_dir / QUARANTINE_FILENAME)

    def is_quarantined(self, target: str) -> bool:
        """True iff ``target`` appears in the list."""
        return any(e.target == target for e in self.entries)

    def get(self, target: str) -> QuarantineEntry | None:
        for e in self.entries:
            if e.target == target:
                return e
        return None

    def add(self, target: str, reason: str = "") -> bool:
        """Append an entry. Returns False if ``target`` was already present."""
        if self.is_quarantined(target):
            return False
        self.entries.append(
            QuarantineEntry(
                target=target,
                reason=reason,
                added_at=date.today().isoformat(),
            )
        )
        return True

    def remove(self, target: str) -> bool:
        """Remove by target name. Returns False if not present."""
        before = len(self.entries)
        self.entries = [e for e in self.entries if e.target != target]
        return len(self.entries) < before

    def save(self) -> None:
        """Write back to ``self.path``. Requires ``path`` to be set."""
        if self.path is None:
            raise ValueError("QuarantineList.save() requires self.path")
        self.path.parent.mkdir(parents=True, exist_ok=True)
        payload: dict[str, Any] = {
            "quarantine": [e.to_dict() for e in self.entries]
        }
        self.path.write_bytes(tomli_w.dumps(payload).encode("utf-8"))

    def to_dict(self) -> dict[str, Any]:
        return {"entries": [e.to_dict() for e in self.entries]}


# Failure classes that quarantine can suppress. INFRA/TIMEOUT/CONTRACT
# are authoritative regardless of quarantine status.
_SUPPRESSIBLE_CLASSES = frozenset({"TEST", "UNKNOWN"})


def is_advisory_failure(
    quarantine: QuarantineList,
    target_name: str,
    failure_class: str | None,
) -> bool:
    """Decide whether a failure should be treated as advisory.

    Returns True iff the target is quarantined AND the failure class
    is one that quarantine is allowed to suppress (TEST / UNKNOWN).
    Callers (ship/merge) should skip blocking the merge on advisory
    failures while still recording them in evidence.

    A missing ``failure_class`` (None) is conservatively treated as
    non-advisory — we'd rather block than silently merge an
    unclassified failure.
    """
    if not failure_class:
        return False
    if not quarantine.is_quarantined(target_name):
        return False
    return failure_class in _SUPPRESSIBLE_CLASSES
