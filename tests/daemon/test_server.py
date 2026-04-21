"""End-to-end webhook-server test parity with ``WebhookServerTests``."""

from __future__ import annotations

import json
import urllib.error
import urllib.request

import pytest

from shipyard.daemon import signature
from shipyard.daemon.server import HandlerResult, WebhookServer


def test_accepts_signed_post_on_webhook_path() -> None:
    secret = "unit-test-secret"
    body = json.dumps({"zen": "Non-blocking is better than blocking."}).encode()
    mac = signature.hmac_sha256_hex(body, secret)
    seen: dict[str, object] = {}

    def handler(headers: dict[str, str], payload: bytes) -> HandlerResult:
        seen["headers"] = headers
        seen["body"] = payload
        if not signature.is_valid(payload, secret, headers.get("x-hub-signature-256")):
            return HandlerResult.unauthorized()
        return HandlerResult.ok()

    server = WebhookServer(handler)
    port = server.start()
    try:
        req = urllib.request.Request(
            f"http://127.0.0.1:{port}/webhook",
            data=body,
            method="POST",
            headers={
                "X-Hub-Signature-256": f"sha256={mac}",
                "X-GitHub-Event": "ping",
                "Content-Type": "application/json",
            },
        )
        with urllib.request.urlopen(req, timeout=5) as resp:  # noqa: S310
            assert resp.status == 200
    finally:
        server.stop()

    assert seen["body"] == body
    assert isinstance(seen["headers"], dict)
    assert seen["headers"]["x-github-event"] == "ping"


def test_accepts_post_on_root_path_for_legacy_hook_compat() -> None:
    server = WebhookServer(lambda _h, _b: HandlerResult.ok())
    port = server.start()
    try:
        req = urllib.request.Request(
            f"http://127.0.0.1:{port}/",
            data=b"{}",
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=5) as resp:  # noqa: S310
            assert resp.status == 200
    finally:
        server.stop()


def test_rejects_get() -> None:
    server = WebhookServer(lambda _h, _b: HandlerResult.ok())
    port = server.start()
    try:
        with pytest.raises(urllib.error.HTTPError) as excinfo:
            urllib.request.urlopen(  # noqa: S310
                f"http://127.0.0.1:{port}/webhook", timeout=5
            )
        assert excinfo.value.code == 405
    finally:
        server.stop()


def test_returns_404_for_unknown_path() -> None:
    server = WebhookServer(lambda _h, _b: HandlerResult.ok())
    port = server.start()
    try:
        req = urllib.request.Request(
            f"http://127.0.0.1:{port}/nope",
            data=b"",
            method="POST",
        )
        with pytest.raises(urllib.error.HTTPError) as excinfo:
            urllib.request.urlopen(req, timeout=5)  # noqa: S310
        assert excinfo.value.code == 404
    finally:
        server.stop()


def test_rejects_bogus_signature() -> None:
    secret = "shh"

    def handler(headers: dict[str, str], payload: bytes) -> HandlerResult:
        if not signature.is_valid(payload, secret, headers.get("x-hub-signature-256")):
            return HandlerResult.unauthorized()
        return HandlerResult.ok()

    server = WebhookServer(handler)
    port = server.start()
    try:
        req = urllib.request.Request(
            f"http://127.0.0.1:{port}/webhook",
            data=b"payload",
            method="POST",
            headers={"X-Hub-Signature-256": "sha256=deadbeef"},
        )
        with pytest.raises(urllib.error.HTTPError) as excinfo:
            urllib.request.urlopen(req, timeout=5)  # noqa: S310
        assert excinfo.value.code == 401
    finally:
        server.stop()
