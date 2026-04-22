"""Decoder coverage for the wait-primitive event kinds.

Exercises `check_run`, `check_suite`, and `release` — the three kinds
added in #149 so `shipyard wait pr` / `shipyard wait release` can wake
on the right events.
"""

from __future__ import annotations

import json

from shipyard.daemon import events


def test_decode_check_run_completed() -> None:
    body = json.dumps(
        {
            "action": "completed",
            "check_run": {
                "name": "Build / linux",
                "status": "completed",
                "conclusion": "success",
                "head_sha": "deadbeef",
                "pull_requests": [{"number": 151}, {"number": 152}],
            },
            "repository": {"full_name": "org/repo"},
        }
    ).encode()
    got = events.decode("check_run", body)
    assert got is not None
    assert got.kind == "check_run"
    assert got.check_run is not None
    assert got.check_run.name == "Build / linux"
    assert got.check_run.status == "completed"
    assert got.check_run.conclusion == "success"
    assert got.check_run.head_sha == "deadbeef"
    assert got.check_run.pull_request_numbers == [151, 152]


def test_decode_check_suite_in_progress() -> None:
    body = json.dumps(
        {
            "action": "in_progress",
            "check_suite": {
                "status": "in_progress",
                "conclusion": None,
                "head_sha": "a1b2c3",
                "pull_requests": [{"number": 42}],
            },
            "repository": {"full_name": "org/repo"},
        }
    ).encode()
    got = events.decode("check_suite", body)
    assert got is not None
    assert got.check_suite is not None
    assert got.check_suite.status == "in_progress"
    assert got.check_suite.conclusion is None
    assert got.check_suite.pull_request_numbers == [42]


def test_decode_release_published_with_assets() -> None:
    body = json.dumps(
        {
            "action": "published",
            "release": {
                "tag_name": "v0.23.0",
                "draft": False,
                "prerelease": False,
                "assets": [
                    {"name": "shipyard-linux", "state": "uploaded", "size": 1024},
                    {"name": "shipyard-darwin", "state": "starter", "size": 0},
                ],
            },
            "repository": {"full_name": "org/repo"},
        }
    ).encode()
    got = events.decode("release", body)
    assert got is not None
    assert got.release is not None
    assert got.release.tag_name == "v0.23.0"
    assert got.release.draft is False
    assert len(got.release.assets) == 2
    assert got.release.assets[0].name == "shipyard-linux"
    assert got.release.assets[0].state == "uploaded"
    assert got.release.assets[0].size == 1024


def test_to_wire_shapes_for_new_kinds() -> None:
    for header, body in [
        (
            "check_run",
            {
                "action": "completed",
                "check_run": {
                    "name": "x",
                    "status": "completed",
                    "conclusion": "success",
                    "head_sha": "abc",
                    "pull_requests": [],
                },
                "repository": {"full_name": "o/r"},
            },
        ),
        (
            "check_suite",
            {
                "action": "completed",
                "check_suite": {
                    "status": "completed",
                    "conclusion": "success",
                    "head_sha": "abc",
                    "pull_requests": [],
                },
                "repository": {"full_name": "o/r"},
            },
        ),
        (
            "release",
            {
                "action": "published",
                "release": {
                    "tag_name": "v1",
                    "draft": False,
                    "prerelease": False,
                    "assets": [],
                },
                "repository": {"full_name": "o/r"},
            },
        ),
    ]:
        evt = events.decode(header, json.dumps(body).encode())
        assert evt is not None, header
        wire = evt.to_wire()
        assert wire["kind"] == header
        assert "payload" in wire


def test_decode_check_run_drops_non_int_pr_numbers() -> None:
    body = json.dumps(
        {
            "action": "created",
            "check_run": {
                "name": "x",
                "status": "queued",
                "conclusion": None,
                "head_sha": "abc",
                "pull_requests": [
                    {"number": True},  # bool: rejected
                    {"number": 1},
                    {},
                    "garbage",
                ],
            },
            "repository": {"full_name": "o/r"},
        }
    ).encode()
    got = events.decode("check_run", body)
    assert got is not None
    assert got.check_run is not None
    assert got.check_run.pull_request_numbers == [1]


def test_registrar_subscribes_release_event() -> None:
    from shipyard.daemon.registrar import SUBSCRIBED_EVENTS

    assert "release" in SUBSCRIBED_EVENTS
