"""Signature validation parity with the Swift ``WebhookSignatureTests``."""

from __future__ import annotations

from base64 import b64decode

from shipyard.daemon import signature


def test_matches_github_reference_vector() -> None:
    # From https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries
    body = b"Hello, World!"
    secret = "It's a Secret to Everybody"
    expected = "757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
    assert signature.hmac_sha256_hex(body, secret) == expected


def test_accepts_sha256_prefixed_header() -> None:
    body = b"payload"
    mac = signature.hmac_sha256_hex(body, "shh")
    assert signature.is_valid(body, "shh", f"sha256={mac}")


def test_accepts_bare_hex_header() -> None:
    body = b"payload"
    mac = signature.hmac_sha256_hex(body, "shh")
    assert signature.is_valid(body, "shh", mac)


def test_rejects_body_tamper() -> None:
    body = b"payload"
    mac = signature.hmac_sha256_hex(body, "shh")
    assert not signature.is_valid(b"payloa!", "shh", f"sha256={mac}")


def test_rejects_secret_mismatch() -> None:
    body = b"payload"
    mac = signature.hmac_sha256_hex(body, "shh")
    assert not signature.is_valid(body, "wrong", f"sha256={mac}")


def test_rejects_missing_header() -> None:
    body = b"payload"
    assert not signature.is_valid(body, "shh", None)
    assert not signature.is_valid(body, "shh", "")


def test_generate_secret_is_distinct_and_base64() -> None:
    a = signature.generate_secret()
    b = signature.generate_secret()
    assert a != b
    # Should round-trip through base64.
    assert b64decode(a)
    assert b64decode(b)
