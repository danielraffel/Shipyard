"""Persistence for cloud workflow dispatch records."""

from __future__ import annotations

import json
import uuid
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from pathlib import Path


@dataclass(frozen=True)
class CloudRunRecord:
    """Persistent record for a cloud-dispatched workflow run."""

    dispatch_id: str
    workflow_key: str
    workflow_file: str
    workflow_name: str
    repository: str | None
    requested_ref: str
    provider: str
    dispatch_fields: dict[str, str]
    status: str
    conclusion: str | None = None
    run_id: str | None = None
    url: str | None = None
    dispatched_at: datetime | None = None
    started_at: datetime | None = None
    completed_at: datetime | None = None
    updated_at: datetime | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "dispatch_id": self.dispatch_id,
            "workflow_key": self.workflow_key,
            "workflow_file": self.workflow_file,
            "workflow_name": self.workflow_name,
            "repository": self.repository,
            "requested_ref": self.requested_ref,
            "provider": self.provider,
            "dispatch_fields": dict(self.dispatch_fields),
            "status": self.status,
            "conclusion": self.conclusion,
            "run_id": self.run_id,
            "url": self.url,
            "dispatched_at": _iso(self.dispatched_at),
            "started_at": _iso(self.started_at),
            "completed_at": _iso(self.completed_at),
            "updated_at": _iso(self.updated_at),
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> CloudRunRecord:
        return cls(
            dispatch_id=data["dispatch_id"],
            workflow_key=data["workflow_key"],
            workflow_file=data["workflow_file"],
            workflow_name=data["workflow_name"],
            repository=data.get("repository"),
            requested_ref=data["requested_ref"],
            provider=data["provider"],
            dispatch_fields=dict(data.get("dispatch_fields", {})),
            status=data["status"],
            conclusion=data.get("conclusion"),
            run_id=data.get("run_id"),
            url=data.get("url"),
            dispatched_at=_from_iso(data.get("dispatched_at")),
            started_at=_from_iso(data.get("started_at")),
            completed_at=_from_iso(data.get("completed_at")),
            updated_at=_from_iso(data.get("updated_at")),
        )


class CloudRecordStore:
    """Store cloud run records in the Shipyard state dir."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self.path.mkdir(parents=True, exist_ok=True)

    def new_dispatch_id(self) -> str:
        now = datetime.now(timezone.utc)
        return f"cloud-{now.strftime('%Y%m%d')}-{uuid.uuid4().hex[:8]}"

    def save(self, record: CloudRunRecord) -> Path:
        target = self.path / f"{record.dispatch_id}.json"
        target.write_text(json.dumps(record.to_dict(), indent=2) + "\n")
        return target

    def get(self, dispatch_id: str) -> CloudRunRecord | None:
        path = self.path / f"{dispatch_id}.json"
        if not path.exists():
            return None
        return CloudRunRecord.from_dict(json.loads(path.read_text()))

    def list(self, limit: int = 20) -> list[CloudRunRecord]:
        records: list[CloudRunRecord] = []
        for path in sorted(self.path.glob("*.json"), reverse=True):
            records.append(CloudRunRecord.from_dict(json.loads(path.read_text())))
        minimum = datetime.min.replace(tzinfo=timezone.utc)
        records.sort(
            key=lambda record: record.updated_at or record.dispatched_at or minimum,
            reverse=True,
        )
        return records[:limit]


def _iso(value: datetime | None) -> str | None:
    return value.isoformat() if value else None


def _from_iso(value: str | None) -> datetime | None:
    if not value:
        return None
    return datetime.fromisoformat(value)
