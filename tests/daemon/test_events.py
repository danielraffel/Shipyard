"""Webhook event decoder parity with ``WebhookEventDecoderTests``."""

from __future__ import annotations

import json

from shipyard.daemon import events


def test_decode_workflow_run_completed() -> None:
    body = json.dumps(
        {
            "action": "completed",
            "workflow_run": {
                "id": 42,
                "head_branch": "feature/x",
                "head_sha": "abc",
                "status": "completed",
                "conclusion": "success",
                "name": "CI",
                "html_url": "https://github.com/org/repo/actions/runs/42",
            },
            "repository": {"full_name": "org/repo"},
        }
    ).encode()
    got = events.decode("workflow_run", body)
    assert got is not None
    assert got.kind == "workflow_run"
    assert got.workflow_run is not None
    assert got.workflow_run.action == "completed"
    assert got.workflow_run.run_id == 42
    assert got.workflow_run.repo == "org/repo"
    assert got.workflow_run.head_branch == "feature/x"
    assert got.workflow_run.conclusion == "success"


def test_decode_workflow_job_in_progress() -> None:
    body = json.dumps(
        {
            "action": "in_progress",
            "workflow_job": {
                "id": 99,
                "run_id": 42,
                "name": "macOS (arm64)",
                "status": "in_progress",
                "conclusion": None,
                "runner_name": "macOS-arm64-1",
                "labels": ["self-hosted", "macOS"],
            },
            "repository": {"full_name": "org/repo"},
        }
    ).encode()
    got = events.decode("workflow_job", body)
    assert got is not None
    assert got.workflow_job is not None
    assert got.workflow_job.job_id == 99
    assert got.workflow_job.run_id == 42
    assert got.workflow_job.name == "macOS (arm64)"
    assert got.workflow_job.status == "in_progress"
    assert got.workflow_job.conclusion is None
    assert got.workflow_job.labels == ["self-hosted", "macOS"]


def test_decode_pull_request_merged() -> None:
    body = json.dumps(
        {
            "action": "closed",
            "number": 581,
            "pull_request": {
                "number": 581,
                "state": "closed",
                "merged": True,
                "merged_at": "2026-04-20T12:00:00Z",
                "closed_at": "2026-04-20T12:00:00Z",
            },
            "repository": {"full_name": "org/repo"},
        }
    ).encode()
    got = events.decode("pull_request", body)
    assert got is not None
    assert got.pull_request is not None
    assert got.pull_request.number == 581
    assert got.pull_request.state == "closed"
    assert got.pull_request.merged is True


def test_decode_unknown_event_type_is_unhandled() -> None:
    got = events.decode("star", b"{}")
    assert got is not None
    assert got.kind == "unhandled"
    assert got.unhandled_type == "star"


def test_decode_missing_event_header_returns_none() -> None:
    assert events.decode(None, b"{}") is None
    assert events.decode("", b"{}") is None


def test_decode_malformed_body_returns_none() -> None:
    assert events.decode("workflow_run", b"{oops") is None


def test_decode_workflow_run_missing_repository_returns_none() -> None:
    body = json.dumps(
        {
            "action": "completed",
            "workflow_run": {
                "id": 1,
                "head_branch": "x",
                "head_sha": "y",
                "status": "completed",
            },
        }
    ).encode()
    assert events.decode("workflow_run", body) is None


def test_to_wire_shape_for_workflow_run() -> None:
    body = json.dumps(
        {
            "action": "completed",
            "workflow_run": {
                "id": 1,
                "head_branch": "x",
                "head_sha": "y",
                "status": "completed",
                "name": "CI",
            },
            "repository": {"full_name": "o/r"},
        }
    ).encode()
    evt = events.decode("workflow_run", body)
    assert evt is not None
    wire = evt.to_wire()
    assert wire["kind"] == "workflow_run"
    assert wire["payload"]["run_id"] == 1
    assert wire["payload"]["repo"] == "o/r"
