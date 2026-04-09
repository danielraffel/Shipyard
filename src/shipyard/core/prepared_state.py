"""Per-stage prepared-state tracking for warm validation re-runs.

When a validation runs successfully on a given (sha, target, mode),
the work that the stage produced — fetched dependencies, configured
build tree, compiled binaries — is still on disk. A subsequent
validation against the same (sha, target, mode) does not need to
redo that work. Pulp's local_ci.py implements this via the
PULP_VALIDATE_REUSE_PREPARED env var: when set, the validation
script checks for marker files and skips stages it knows already
passed.

Shipyard's prepared-state store tracks per-stage pass/fail in a
JSON file keyed by (sha, target, mode). Before running a stage on
a re-run, the executor consults the store and skips the stage if
it has previously passed for the exact same key. After each stage
runs, the executor records the result in the store.

This is intentionally separate from the merge-gating EvidenceStore
in core/evidence.py, which tracks the *overall* per-target outcome
for each branch (used by `shipyard ship`). The prepared-state store
tracks *per-stage* progress for the *exact* SHA, used to short-circuit
warm re-runs.

Storage layout:

    <state_dir>/prepared/<sha>/<target>--<mode>.json

    {
        "sha": "abc1234",
        "target": "ubuntu",
        "mode": "default",
        "stages": {
            "setup":     "pass",
            "configure": "pass",
            "build":     "pass",
            "test":      "fail"
        },
        "updated_at": "2026-04-09T17:00:00Z",
        "config_hash": "0123abcd"
    }

The `config_hash` is a digest of the stage command strings at the
time the file was written. If the strings change in the project
config (someone edited .shipyard/config.toml), the hash differs and
the prepared state is invalidated automatically — this prevents the
"I edited the build command but Shipyard skipped the build because
the old run had passed" failure mode.
"""

from __future__ import annotations

import contextlib
import hashlib
import json
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from collections.abc import Iterable
    from pathlib import Path


@dataclass(frozen=True)
class StageOutcome:
    """A single stage's pass/fail recorded in the prepared-state store."""

    stage: str
    status: str  # "pass" or "fail"

    @property
    def passed(self) -> bool:
        return self.status == "pass"


@dataclass
class PreparedStateRecord:
    """Per-stage outcome cache for one (sha, target, mode) tuple.

    A record can be partially populated — Shipyard records each
    stage as it completes, so a run that fails midway through writes
    {setup: pass, configure: pass, build: fail} and stops. The next
    re-run sees that {setup, configure} already passed and only runs
    the {build, test} stages.
    """

    sha: str
    target: str
    mode: str
    stages: dict[str, str] = field(default_factory=dict)
    updated_at: datetime = field(default_factory=lambda: datetime.now(timezone.utc))
    config_hash: str = ""

    def is_passed(self, stage: str) -> bool:
        return self.stages.get(stage) == "pass"

    def mark(self, stage: str, status: str) -> None:
        self.stages[stage] = status
        self.updated_at = datetime.now(timezone.utc)

    def to_dict(self) -> dict[str, Any]:
        return {
            "sha": self.sha,
            "target": self.target,
            "mode": self.mode,
            "stages": dict(self.stages),
            "updated_at": self.updated_at.isoformat(),
            "config_hash": self.config_hash,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> PreparedStateRecord:
        return cls(
            sha=d["sha"],
            target=d["target"],
            mode=d["mode"],
            stages=dict(d.get("stages", {})),
            updated_at=datetime.fromisoformat(d["updated_at"]),
            config_hash=d.get("config_hash", ""),
        )


@dataclass
class PreparedStateStore:
    """Persists per-stage outcomes for the prepared-state cache.

    The store is opt-in: an executor only consults it when the project
    config sets `[validation.prepared_state] enabled = true`. The
    executor itself decides which stages to skip; this store only
    persists and retrieves records.
    """

    path: Path

    def __post_init__(self) -> None:
        self.path.mkdir(parents=True, exist_ok=True)

    def get(self, sha: str, target: str, mode: str) -> PreparedStateRecord | None:
        record_path = self._record_path(sha, target, mode)
        if not record_path.exists():
            return None
        try:
            data = json.loads(record_path.read_text())
            return PreparedStateRecord.from_dict(data)
        except (json.JSONDecodeError, KeyError, ValueError):
            # Corrupted record — treat as missing so the next run
            # writes a fresh one.
            return None

    def save(self, record: PreparedStateRecord) -> None:
        record_path = self._record_path(record.sha, record.target, record.mode)
        record_path.parent.mkdir(parents=True, exist_ok=True)
        record_path.write_text(json.dumps(record.to_dict(), indent=2) + "\n")

    def delete(self, sha: str, target: str, mode: str) -> None:
        record_path = self._record_path(sha, target, mode)
        if record_path.exists():
            record_path.unlink()

    def delete_sha(self, sha: str) -> int:
        """Remove every record for a SHA. Returns the number of files deleted."""
        sha_dir = self.path / _sanitize(sha)
        if not sha_dir.exists():
            return 0
        count = 0
        for f in sha_dir.glob("*.json"):
            f.unlink()
            count += 1
        with contextlib.suppress(OSError):
            sha_dir.rmdir()
        return count

    def cleanup_other_shas(self, keep_sha: str) -> int:
        """Remove every record except those for `keep_sha`. Returns count.

        Used during housekeeping to prevent the prepared-state cache
        from growing without bound — only the most-recent SHA is
        worth keeping for warm reruns.
        """
        if not self.path.exists():
            return 0
        keep_dir = _sanitize(keep_sha)
        count = 0
        for sha_dir in self.path.iterdir():
            if not sha_dir.is_dir() or sha_dir.name == keep_dir:
                continue
            for f in sha_dir.glob("*.json"):
                f.unlink()
                count += 1
            with contextlib.suppress(OSError):
                sha_dir.rmdir()
        return count

    def _record_path(self, sha: str, target: str, mode: str) -> Path:
        return self.path / _sanitize(sha) / f"{_sanitize(target)}--{_sanitize(mode)}.json"


def hash_stage_commands(stage_commands: Iterable[tuple[str, str]]) -> str:
    """Compute a stable digest of the stage name + command pairs.

    Two records with the same SHA but different stage commands have
    different config hashes, so editing a build command in
    `.shipyard/config.toml` automatically invalidates the prepared
    state for the affected (sha, target, mode).
    """
    h = hashlib.sha256()
    for stage_name, command in sorted(stage_commands):
        h.update(stage_name.encode("utf-8"))
        h.update(b"\x00")
        h.update(command.encode("utf-8"))
        h.update(b"\x00")
    return h.hexdigest()[:16]


def filter_stages_by_prepared_state(
    stages: list[tuple[str, str]],
    record: PreparedStateRecord | None,
    *,
    current_config_hash: str,
) -> tuple[list[tuple[str, str]], list[str]]:
    """Filter the requested stage list against a prepared-state record.

    Returns (stages_to_run, stages_skipped).

    A stage is skipped only when:
    - The record exists
    - The config_hash matches (same stage commands)
    - The stage is marked "pass" in the record

    The first stage in the list with no recorded "pass" causes every
    subsequent stage to also run, regardless of cached state — this
    matches Pulp's local_ci.py behavior, where a re-run from a failed
    stage forces every subsequent stage to re-run too (because the
    failed stage may have left intermediate artifacts in a broken
    state).
    """
    if record is None or record.config_hash != current_config_hash:
        return list(stages), []

    skipped: list[str] = []
    to_run: list[tuple[str, str]] = []
    skipping_phase = True

    for stage_name, command in stages:
        if skipping_phase and record.is_passed(stage_name):
            skipped.append(stage_name)
            continue
        skipping_phase = False
        to_run.append((stage_name, command))

    return to_run, skipped


def _sanitize(value: str) -> str:
    """Make an arbitrary identifier safe for use as a filename component."""
    return (
        value.replace("/", "--")
        .replace("\\", "--")
        .replace(":", "_")
        .replace(" ", "_")
    )
