#!/usr/bin/env python3
"""Stamp a verdict comment with the session that posts it.

``ghapp`` runs this before native ``gh``. When the request posts or edits a
pull-request comment whose last non-blank line is a verdict marker
(``verdict: APPROVE|HOLD head:<sha>`` or ``reviewed: <sha>``), the body gets
one more line just above that marker::

    <!-- session: <host>/<session>[ agent: <id>] -->

``<session>`` is ``CLAUDE_CODE_SESSION_ID``, else ``CODEX_COMPANION_SESSION_ID``,
else ``unattributed``; ``agent`` is ``SHIPYARD_AGENT_ID`` when set. The marker
stays the last line, so every verdict reader is unchanged. ``queue-arm-guard``
refuses an arm whose agent id equals the deciding approval's, and only warns
when just the session matches (every agent of one team shares it).

Output: ``SHIPYARD-STAMP-V1``, the (possibly rewritten) argv, then
``SHIPYARD-STAMP-END``, every item NUL-terminated. ``ghapp`` uses the
rewritten argv only when both sentinels are present and reads for at most 5
seconds, so a failure, a hang or a truncated answer posts the comment
unstamped, never blocks it; the stamper also stops itself after 5 seconds.
Handled: ``pr comment``/``issue comment`` with ``--body``/``-b``; ``api`` on an
issue-comment endpoint with ``-f``/``-F``/``--field``/``--raw-field body=...``,
or ``--input <file>`` holding exactly ``{"body": ...}``, which becomes a
``--raw-field body=...`` (no file is written). Anything else (``--body-file``,
stdin, other fields) passes through unstamped.
"""

from __future__ import annotations

import json
import os
import re
import signal
import socket
import sys

SENTINEL = "SHIPYARD-STAMP-V1"
END = "SHIPYARD-STAMP-END"
DEADLINE_SECONDS = 5
SESSION_ENVS = ("CLAUDE_CODE_SESSION_ID", "CODEX_COMPANION_SESSION_ID")
AGENT_ENV = "SHIPYARD_AGENT_ID"
# Same pattern as queue-arm-guard's _MARKER and src/head_approval.rs marker_verdict.
_MARKER = re.compile(
    r"^[ \t]*(?:reviewed:[ \t]*([0-9a-f]{40})"
    r"|verdict:[ \t]*(APPROVE|HOLD)[ \t]+head:[ \t]*([0-9a-f]{40}))[ \t]*$"
)
_STAMPED = re.compile(r"<!--\s*session:")
_COMMENT_ENDPOINT = re.compile(r"^/?repos/[^/]+/[^/]+/issues/(?:[0-9]+/comments|comments/[0-9]+)$")


def stamp() -> str:
    session = next((os.environ[name].strip() for name in SESSION_ENVS
                    if os.environ.get(name, "").strip()), "unattributed")
    agent = os.environ.get(AGENT_ENV, "").strip()
    tail = f" agent: {agent}" if agent and not any(c.isspace() for c in agent) else ""
    return f"<!-- session: {socket.gethostname().split('.')[0]}/{session}{tail} -->"


def stamped(body: str) -> str | None:
    """``body`` with the stamp above its verdict line, or ``None`` when it is
    not a verdict or already carries a stamp."""
    if _STAMPED.search(body):
        return None
    lines = body.split("\n")
    index = max((i for i, line in enumerate(lines) if line.strip()), default=None)
    if index is None or not _MARKER.match(lines[index]):
        return None
    return "\n".join([*lines[:index], stamp(), *lines[index:]])


def _rewrite_field(args: list[str]) -> bool:
    for i, arg in enumerate(args[:-1]):
        if arg in ("-f", "-F", "--field", "--raw-field") and args[i + 1].startswith("body="):
            new = stamped(args[i + 1][len("body="):])
            if new is not None:
                args[i + 1] = "body=" + new
                return True
    for i, arg in enumerate(args):
        for flag in ("--field=", "--raw-field="):
            if arg.startswith(flag + "body="):
                new = stamped(arg[len(flag + "body="):])
                if new is not None:
                    args[i] = flag + "body=" + new
                    return True
    return False


def _rewrite_input(args: list[str]) -> bool:
    for i, arg in enumerate(args[:-1]):
        if arg != "--input" or args[i + 1] == "-":
            continue
        with open(args[i + 1], encoding="utf-8") as handle:
            payload = json.load(handle)
        if (not isinstance(payload, dict) or set(payload) != {"body"}
                or not isinstance(payload["body"], str)):
            return False
        new = stamped(payload["body"])
        if new is None:
            return False
        # The same request as a field: nothing to write, so nothing to clean up.
        args[i:i + 2] = ["--raw-field", "body=" + new]
        return True
    return False


def rewrite(args: list[str]) -> list[str]:
    """``args`` with a verdict comment's body stamped; unchanged otherwise."""
    out = list(args)
    if len(out) >= 2 and out[0] in ("pr", "issue") and out[1] == "comment":
        for i, arg in enumerate(out[:-1]):
            if arg in ("--body", "-b"):
                new = stamped(out[i + 1])
                if new is not None:
                    out[i + 1] = new
                return out
        for i, arg in enumerate(out):
            if arg.startswith("--body="):
                new = stamped(arg[len("--body="):])
                if new is not None:
                    out[i] = "--body=" + new
                return out
        return out
    if out and out[0] == "api":
        if not any(_COMMENT_ENDPOINT.match(arg) for arg in out[1:] if not arg.startswith("-")):
            return out
        if not _rewrite_field(out):
            _rewrite_input(out)
    return out


def _give_up(_signum: int, _frame: object) -> None:
    os._exit(0)


def main(args: list[str]) -> int:
    if hasattr(signal, "SIGALRM"):
        signal.signal(signal.SIGALRM, _give_up)
        signal.alarm(DEADLINE_SECONDS)
    try:
        result = rewrite(args)
    except Exception:  # noqa: BLE001 - never block a comment over a stamp
        return 0
    sys.stdout.buffer.write(
        b"".join(item.encode() + b"\0" for item in [SENTINEL, *result, END]))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
