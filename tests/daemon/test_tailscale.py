"""Tailscale status decoder parity with ``TailscaleProbeTests``."""

from __future__ import annotations

import json

from shipyard.daemon import tailscale


def test_decode_ready_happy_path() -> None:
    raw = json.dumps(
        {
            "BackendState": "Running",
            "Self": {
                "DNSName": "spacely.corvus-rufinus.ts.net.",
                "CapMap": {
                    "https://tailscale.com/cap/funnel": ["ports:443"],
                },
            },
        }
    ).encode()
    status = tailscale.decode(raw, binary_path="/usr/local/bin/tailscale")
    assert status.is_ready
    assert status.funnel_url == "https://spacely.corvus-rufinus.ts.net"


def test_decode_backend_not_running_is_not_ready() -> None:
    raw = json.dumps(
        {
            "BackendState": "NeedsLogin",
            "Self": {
                "DNSName": "foo.ts.net",
                "CapMap": {"https://tailscale.com/cap/funnel": []},
            },
        }
    ).encode()
    status = tailscale.decode(raw, binary_path="/x")
    assert not status.is_ready
    assert status.funnel_url is None


def test_decode_missing_funnel_cap_is_not_ready() -> None:
    raw = json.dumps(
        {
            "BackendState": "Running",
            "Self": {
                "DNSName": "foo.ts.net",
                "CapMap": {"https://tailscale.com/cap/other": []},
            },
        }
    ).encode()
    assert not tailscale.decode(raw, binary_path="/x").is_ready


def test_decode_bare_funnel_key_accepted() -> None:
    raw = json.dumps(
        {
            "BackendState": "Running",
            "Self": {
                "DNSName": "foo.ts.net",
                "CapMap": {"funnel": []},
            },
        }
    ).encode()
    assert tailscale.decode(raw, binary_path="/x").is_ready


def test_decode_trailing_dot_stripped_from_dns_name() -> None:
    raw = json.dumps(
        {
            "BackendState": "Running",
            "Self": {
                "DNSName": "foo.ts.net.",
                "CapMap": {"https://tailscale.com/cap/funnel": []},
            },
        }
    ).encode()
    status = tailscale.decode(raw, binary_path="/x")
    assert status.funnel_url == "https://foo.ts.net"


def test_decode_malformed_json_returns_not_ready() -> None:
    status = tailscale.decode(b"not json", binary_path="/x")
    assert not status.is_ready
    assert status.funnel_url is None


def test_decode_nil_binary_path_is_not_ready() -> None:
    raw = json.dumps(
        {
            "BackendState": "Running",
            "Self": {
                "DNSName": "foo.ts.net",
                "CapMap": {"https://tailscale.com/cap/funnel": []},
            },
        }
    ).encode()
    assert not tailscale.decode(raw, binary_path=None).is_ready
