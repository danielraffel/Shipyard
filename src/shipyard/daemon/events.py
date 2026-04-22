"""Typed, normalized view of GitHub webhook deliveries.

Only the fields consumers actually need are decoded — enough to drive
the UI's ship/run/PR state changes. Everything else is dropped.

Mirrors ``WebhookEvent.swift`` + ``WebhookEventDecoder.swift`` in the
macOS GUI. Kept as plain dataclasses rather than Pydantic so the
daemon has no extra dependencies.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True)
class WorkflowRunPayload:
    action: str
    run_id: int
    repo: str
    head_branch: str
    head_sha: str
    status: str
    conclusion: str | None
    workflow_name: str
    html_url: str | None


@dataclass(frozen=True)
class WorkflowJobPayload:
    action: str
    run_id: int
    job_id: int
    repo: str
    name: str
    status: str
    conclusion: str | None
    runner_name: str | None
    labels: list[str]


@dataclass(frozen=True)
class PullRequestPayload:
    action: str
    number: int
    repo: str
    state: str
    merged: bool
    merged_at: str | None
    closed_at: str | None


@dataclass(frozen=True)
class CheckRunPayload:
    action: str
    repo: str
    name: str
    status: str
    conclusion: str | None
    head_sha: str
    pull_request_numbers: list[int]


@dataclass(frozen=True)
class CheckSuitePayload:
    action: str
    repo: str
    status: str
    conclusion: str | None
    head_sha: str
    pull_request_numbers: list[int]


@dataclass(frozen=True)
class ReleaseAssetInfo:
    name: str
    state: str
    size: int


@dataclass(frozen=True)
class ReleasePayload:
    action: str
    repo: str
    tag_name: str
    draft: bool
    prerelease: bool
    assets: list[ReleaseAssetInfo]


@dataclass(frozen=True)
class WebhookEvent:
    """Tagged union of decoded events. Exactly one of the ``*_payload``
    fields is populated; ``kind`` tells you which."""

    kind: str
    unhandled_type: str | None = None
    workflow_run: WorkflowRunPayload | None = None
    workflow_job: WorkflowJobPayload | None = None
    pull_request: PullRequestPayload | None = None
    check_run: CheckRunPayload | None = None
    check_suite: CheckSuitePayload | None = None
    release: ReleasePayload | None = None

    def to_wire(self) -> dict[str, Any]:
        """Shape sent over the IPC socket as NDJSON."""
        if self.kind == "workflow_run" and self.workflow_run:
            return {"kind": "workflow_run", "payload": self.workflow_run.__dict__}
        if self.kind == "workflow_job" and self.workflow_job:
            return {"kind": "workflow_job", "payload": self.workflow_job.__dict__}
        if self.kind == "pull_request" and self.pull_request:
            return {"kind": "pull_request", "payload": self.pull_request.__dict__}
        if self.kind == "check_run" and self.check_run:
            return {"kind": "check_run", "payload": self.check_run.__dict__}
        if self.kind == "check_suite" and self.check_suite:
            return {"kind": "check_suite", "payload": self.check_suite.__dict__}
        if self.kind == "release" and self.release:
            payload = {
                **self.release.__dict__,
                "assets": [a.__dict__ for a in self.release.assets],
            }
            return {"kind": "release", "payload": payload}
        return {"kind": "unhandled", "type": self.unhandled_type}


def decode(event_header: str | None, body: bytes) -> WebhookEvent | None:
    """Decode a raw webhook delivery. Returns ``None`` when the header
    is missing or the body isn't decodable JSON. Unknown event types
    come back as ``WebhookEvent(kind="unhandled", unhandled_type=...)``
    so callers can still record the liveness signal."""
    if not event_header:
        return None
    try:
        obj = json.loads(body)
    except (json.JSONDecodeError, UnicodeDecodeError):
        return None
    if not isinstance(obj, dict):
        return None
    if event_header == "workflow_run":
        return _decode_workflow_run(obj)
    if event_header == "workflow_job":
        return _decode_workflow_job(obj)
    if event_header == "pull_request":
        return _decode_pull_request(obj)
    if event_header == "check_run":
        return _decode_check_run(obj)
    if event_header == "check_suite":
        return _decode_check_suite(obj)
    if event_header == "release":
        return _decode_release(obj)
    return WebhookEvent(kind="unhandled", unhandled_type=event_header)


def _decode_workflow_run(obj: dict[str, Any]) -> WebhookEvent | None:
    run = obj.get("workflow_run")
    repo = (obj.get("repository") or {}).get("full_name")
    if not isinstance(run, dict) or not repo:
        return None
    run_id = _as_int(run.get("id"))
    if run_id is None:
        return None
    return WebhookEvent(
        kind="workflow_run",
        workflow_run=WorkflowRunPayload(
            action=str(obj.get("action", "")),
            run_id=run_id,
            repo=repo,
            head_branch=str(run.get("head_branch", "")),
            head_sha=str(run.get("head_sha", "")),
            status=str(run.get("status", "")),
            conclusion=run.get("conclusion"),
            workflow_name=str(run.get("name", "")),
            html_url=run.get("html_url"),
        ),
    )


def _decode_workflow_job(obj: dict[str, Any]) -> WebhookEvent | None:
    job = obj.get("workflow_job")
    repo = (obj.get("repository") or {}).get("full_name")
    if not isinstance(job, dict) or not repo:
        return None
    job_id = _as_int(job.get("id"))
    run_id = _as_int(job.get("run_id"))
    if job_id is None or run_id is None:
        return None
    labels_raw = job.get("labels") or []
    labels = [str(x) for x in labels_raw] if isinstance(labels_raw, list) else []
    return WebhookEvent(
        kind="workflow_job",
        workflow_job=WorkflowJobPayload(
            action=str(obj.get("action", "")),
            run_id=run_id,
            job_id=job_id,
            repo=repo,
            name=str(job.get("name", "")),
            status=str(job.get("status", "")),
            conclusion=job.get("conclusion"),
            runner_name=job.get("runner_name"),
            labels=labels,
        ),
    )


def _decode_pull_request(obj: dict[str, Any]) -> WebhookEvent | None:
    pr = obj.get("pull_request")
    repo = (obj.get("repository") or {}).get("full_name")
    if not isinstance(pr, dict) or not repo:
        return None
    number = _as_int(pr.get("number"))
    if number is None:
        return None
    return WebhookEvent(
        kind="pull_request",
        pull_request=PullRequestPayload(
            action=str(obj.get("action", "")),
            number=number,
            repo=repo,
            state=str(pr.get("state", "")),
            merged=bool(pr.get("merged", False)),
            merged_at=pr.get("merged_at"),
            closed_at=pr.get("closed_at"),
        ),
    )


def _decode_check_run(obj: dict[str, Any]) -> WebhookEvent | None:
    check = obj.get("check_run")
    repo = (obj.get("repository") or {}).get("full_name")
    if not isinstance(check, dict) or not repo:
        return None
    pr_numbers: list[int] = []
    for pr in check.get("pull_requests") or []:
        if isinstance(pr, dict):
            num = _as_int(pr.get("number"))
            if num is not None:
                pr_numbers.append(num)
    return WebhookEvent(
        kind="check_run",
        check_run=CheckRunPayload(
            action=str(obj.get("action", "")),
            repo=repo,
            name=str(check.get("name", "")),
            status=str(check.get("status", "")),
            conclusion=check.get("conclusion"),
            head_sha=str(check.get("head_sha", "")),
            pull_request_numbers=pr_numbers,
        ),
    )


def _decode_check_suite(obj: dict[str, Any]) -> WebhookEvent | None:
    suite = obj.get("check_suite")
    repo = (obj.get("repository") or {}).get("full_name")
    if not isinstance(suite, dict) or not repo:
        return None
    pr_numbers: list[int] = []
    for pr in suite.get("pull_requests") or []:
        if isinstance(pr, dict):
            num = _as_int(pr.get("number"))
            if num is not None:
                pr_numbers.append(num)
    return WebhookEvent(
        kind="check_suite",
        check_suite=CheckSuitePayload(
            action=str(obj.get("action", "")),
            repo=repo,
            status=str(suite.get("status", "")),
            conclusion=suite.get("conclusion"),
            head_sha=str(suite.get("head_sha", "")),
            pull_request_numbers=pr_numbers,
        ),
    )


def _decode_release(obj: dict[str, Any]) -> WebhookEvent | None:
    release = obj.get("release")
    repo = (obj.get("repository") or {}).get("full_name")
    if not isinstance(release, dict) or not repo:
        return None
    assets_raw = release.get("assets") or []
    assets: list[ReleaseAssetInfo] = []
    if isinstance(assets_raw, list):
        for asset in assets_raw:
            if not isinstance(asset, dict):
                continue
            size = _as_int(asset.get("size")) or 0
            assets.append(
                ReleaseAssetInfo(
                    name=str(asset.get("name", "")),
                    state=str(asset.get("state", "")),
                    size=size,
                )
            )
    return WebhookEvent(
        kind="release",
        release=ReleasePayload(
            action=str(obj.get("action", "")),
            repo=repo,
            tag_name=str(release.get("tag_name", "")),
            draft=bool(release.get("draft", False)),
            prerelease=bool(release.get("prerelease", False)),
            assets=assets,
        ),
    )


def _as_int(value: Any) -> int | None:
    if isinstance(value, bool):
        # bool is a subclass of int in Python — explicit reject.
        return None
    if isinstance(value, int):
        return value
    return None
