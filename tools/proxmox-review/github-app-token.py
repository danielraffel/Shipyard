#!/usr/bin/env python3
"""Mint a short-lived GitHub App installation token without dependencies."""

from __future__ import annotations

import argparse
import base64
import json
from pathlib import Path
import subprocess
import time
import urllib.error
import urllib.request


class TokenError(RuntimeError):
    pass


def b64(value: bytes) -> str:
    return base64.urlsafe_b64encode(value).rstrip(b"=").decode()


def request(method: str, url: str, bearer: str, data: bytes | None = None) -> dict[str, object]:
    value = urllib.request.Request(url, method=method, data=data)
    value.add_header("Authorization", f"Bearer {bearer}")
    value.add_header("Accept", "application/vnd.github+json")
    value.add_header("X-GitHub-Api-Version", "2022-11-28")
    try:
        with urllib.request.urlopen(value, timeout=30) as response:
            decoded = json.load(response)
    except (urllib.error.URLError, json.JSONDecodeError) as error:
        raise TokenError("GitHub App token request failed") from error
    if not isinstance(decoded, dict):
        raise TokenError("GitHub App returned an unexpected response")
    return decoded


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--app-id", required=True)
    parser.add_argument("--private-key", type=Path, required=True)
    parser.add_argument("--repo", required=True)
    args = parser.parse_args()
    if args.private_key.is_symlink() or not args.private_key.is_file():
        raise TokenError("GitHub App private key is unavailable")
    stat = args.private_key.stat()
    if stat.st_mode & 0o077:
        raise TokenError("GitHub App private key must have mode 0600")
    now = int(time.time())
    header = b64(json.dumps({"alg": "RS256", "typ": "JWT"}, separators=(",", ":")).encode())
    payload = b64(json.dumps({"iat": now - 60, "exp": now + 540, "iss": args.app_id}, separators=(",", ":")).encode())
    signing_input = f"{header}.{payload}".encode()
    signed = subprocess.run(
        ["/usr/bin/openssl", "dgst", "-sha256", "-sign", str(args.private_key)],
        input=signing_input, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        timeout=30, check=False,
    )
    if signed.returncode != 0:
        raise TokenError("GitHub App JWT signing failed")
    jwt = f"{signing_input.decode()}.{b64(signed.stdout)}"
    installation = request("GET", f"https://api.github.com/repos/{args.repo}/installation", jwt)
    installation_id = installation.get("id")
    if not isinstance(installation_id, int):
        raise TokenError("GitHub App installation lookup failed")
    token = request(
        "POST", f"https://api.github.com/app/installations/{installation_id}/access_tokens",
        jwt, b"{}",
    )
    if not isinstance(token.get("token"), str) or not isinstance(token.get("expires_at"), str):
        raise TokenError("GitHub App token response was incomplete")
    print(json.dumps({"token": token["token"], "expires_at": token["expires_at"]}, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except TokenError as error:
        print(f"github-app-token: {error}", file=__import__("sys").stderr)
        raise SystemExit(1)
