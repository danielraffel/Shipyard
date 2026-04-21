"""GitHub webhook HMAC-SHA256 signature validation.

GitHub signs every webhook body with the secret configured on the
hook and sends the signature in the ``X-Hub-Signature-256`` header as
``sha256=<hex>``. The server MUST reject any request whose recomputed
signature doesn't match — the URL alone isn't secret enough to
authenticate deliveries.

Mirrors ``WebhookSignature.swift`` in the macOS GUI (CryptoKit there,
``hmac`` here).
"""

from __future__ import annotations

import hashlib
import hmac
import secrets
from base64 import b64encode


def hmac_sha256_hex(body: bytes, secret: str) -> str:
    """Compute the hex-encoded HMAC-SHA256 for ``body`` + ``secret``."""
    return hmac.new(secret.encode("utf-8"), body, hashlib.sha256).hexdigest()


def is_valid(body: bytes, secret: str, header: str | None) -> bool:
    """Constant-time verify a GitHub ``sha256=…`` header.

    Accepts either ``sha256=<hex>`` (GitHub's format) or bare hex, to
    stay defensive when callers have already stripped the prefix.
    """
    if not header:
        return False
    provided = header[len("sha256=") :] if header.startswith("sha256=") else header
    expected = hmac_sha256_hex(body, secret)
    return hmac.compare_digest(expected, provided)


def generate_secret() -> str:
    """Fresh 32-byte secret, base64-encoded — stored in Keychain (or a
    600-perm file on Linux) and handed to GitHub when registering the
    hook."""
    return b64encode(secrets.token_bytes(32)).decode("ascii")
