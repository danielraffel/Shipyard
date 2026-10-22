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
unstamped; the stamper also stops itself after 5 seconds. ``ghapp`` then
refuses the comment only if its body carries stamp text, which nothing has
vouched for.
Handled: ``pr comment``/``issue comment`` with ``--body``/``-b``, or with
``--body-file``/``-F <file>``, which becomes a ``--body`` (no file is written);
``api`` on an issue-comment endpoint with ``-f``/``-F``/``--field``/
``--raw-field body=...`` (a typed ``body=@<file>`` is read), or
``--input <file>`` holding a ``{"body": ...}`` object, which becomes a
``--raw-field body=...``. ``ghapp`` spools a body read from stdin to a private
file first, so stdin takes the file path. Every stamp already in any comment
body, verdict or not, is removed, repeatedly until none is left, and only a
verdict gets this session's; what counts as a verdict is queue-arm-guard's
own ``marker_verdict``.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import pathlib
import re
import signal
import socket
import sys
from typing import Any, Callable, Optional, Tuple

# A body that is not valid UTF-8 is read and written back byte for byte, so
# one stray byte cannot crash the stamper and post a foreign stamp unstripped
# (Python reads argv the same way).
BODY_ERRORS = "surrogateescape"
SENTINEL = "SHIPYARD-STAMP-V1"
END = "SHIPYARD-STAMP-END"
DEADLINE_SECONDS = 5
SESSION_ENVS = ("CLAUDE_CODE_SESSION_ID", "CODEX_COMPANION_SESSION_ID")
AGENT_ENV = "SHIPYARD_AGENT_ID"
# Same pattern as queue-arm-guard's _MARKER and src/head_approval.rs marker_verdict.
# Used only when queue-arm-guard's own marker_verdict cannot be loaded.
_MARKER = re.compile(
    r"^[ \t]*(?:reviewed:[ \t]*([0-9a-f]{40})"
    r"|verdict:[ \t]*(APPROVE|HOLD)[ \t]+head:[ \t]*([0-9a-f]{40}))[ \t]*$"
)
# Any stamp already in a body, whole line or inline. The stamper replaces it:
# the arm guard checks the stamp's host against the `session:` line, so a
# stamp written by hand (or copied with the rest of a verdict) must not stand.
_STAMPED = re.compile(r"<!--\s*session:.*?-->", re.DOTALL)
_COMMENT_ENDPOINT = re.compile(r"^/?repos/[^/]+/[^/]+/issues/(?:[0-9]+/comments|comments/[0-9]+)$")


def stamp() -> str:
    session = next((os.environ[name].strip() for name in SESSION_ENVS
                    if os.environ.get(name, "").strip()), "unattributed")
    agent = os.environ.get(AGENT_ENV, "").strip()
    tail = f" agent: {agent}" if agent and not any(c.isspace() for c in agent) else ""
    return f"<!-- session: {socket.gethostname().split('.')[0]}/{session}{tail} -->"


def _local_marker_verdict(body: str) -> Optional[Tuple[str, str]]:
    """queue-arm-guard's ``marker_verdict``, for a host where it cannot be
    loaded: the last non-blank line by ``str.splitlines``."""
    lines = [line for line in body.splitlines() if line.strip()]
    match = _MARKER.match(lines[-1]) if lines else None
    if match is None:
        return None
    return ("APPROVE", match.group(1)) if match.group(1) else (match.group(2), match.group(3))


_GUARD_MARKER: Optional[Callable[[str], Any]] = None


def marker_verdict(body: str) -> Optional[Tuple[str, str]]:
    """The verdict the arm guard reads in ``body``. The stamper decides what
    is a verdict with the guard's own parser, loaded from the sibling
    queue-arm-guard, so a body the guard counts is never one the stamper
    passes over (a `\\r\\n` ending once was)."""
    global _GUARD_MARKER
    if _GUARD_MARKER is None:
        _GUARD_MARKER = _load_guard_marker() or _local_marker_verdict
    return _GUARD_MARKER(body)


def _load_guard_marker() -> Optional[Callable[[str], Any]]:
    here = pathlib.Path(__file__).resolve().parent
    for name in ("ghapp_queue_arm_guard.py", "queue-arm-guard"):
        candidate = here / name
        if not candidate.is_file():
            continue
        loader = importlib.machinery.SourceFileLoader("_ghapp_arm_guard", str(candidate))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        if spec is None:
            continue
        module = importlib.util.module_from_spec(spec)
        try:
            loader.exec_module(module)
        except Exception:  # noqa: BLE001 - fall back to the local copy
            continue
        if callable(getattr(module, "marker_verdict", None)):
            return module.marker_verdict
    return None


def strip_stamps(body: str) -> str:
    """``body`` with every stamp removed, repeated until none is left: one
    pass can join the pieces around a removed stamp into a new one."""
    while True:
        lines = [line for line in body.split("\n")
                 if not (_STAMPED.search(line) and not _STAMPED.sub("", line).strip())]
        stripped = _STAMPED.sub("", "\n".join(lines))
        if stripped == body:
            return body
        body = stripped


def stamped(body: str) -> str | None:
    """``body`` without any stamp it carried and, when it is a verdict, with
    this session's stamp above its verdict line; ``None`` when there is
    nothing to change (not a verdict, no stamp)."""
    clean = strip_stamps(body)
    verdict = marker_verdict(clean)
    if verdict is None:
        return clean if clean != body else None
    lines = clean.splitlines()
    index = max(i for i, line in enumerate(lines) if line.strip())
    out = "\n".join([*lines[:index], stamp(), *lines[index:]])
    if clean[-1:] in ("\n", "\r"):
        out += "\n"
    return out


def _field_body(flag: str, value: str) -> Optional[str]:
    """The comment body a ``body=`` field sends: a typed field (``-F``,
    ``--field``) reads ``@<file>`` from that file."""
    value = value[len("body="):]
    if flag in ("-F", "--field") and value.startswith("@") and value != "@-":
        with open(value[1:], encoding="utf-8", errors=BODY_ERRORS) as handle:
            return handle.read()
    return value


def _rewrite_field(args: list[str]) -> bool:
    for i, arg in enumerate(args[:-1]):
        if arg in ("-f", "-F", "--field", "--raw-field") and args[i + 1].startswith("body="):
            body = _field_body(arg, args[i + 1])
            new = stamped(body) if body is not None else None
            if new is not None:
                args[i:i + 2] = ["--raw-field", "body=" + new]
                return True
    for i, arg in enumerate(args):
        for flag in ("--field=", "--raw-field="):
            if arg.startswith(flag + "body="):
                body = _field_body(flag[:-1], arg[len(flag):])
                new = stamped(body) if body is not None else None
                if new is not None:
                    args[i] = "--raw-field=body=" + new
                    return True
    return False


def _rewrite_input(args: list[str]) -> bool:
    for i, arg in enumerate(args):
        if arg == "--input" and i + 1 < len(args):
            path, span = args[i + 1], 2
        elif arg.startswith("--input="):
            path, span = arg[len("--input="):], 1
        else:
            continue
        if path == "-":
            return False
        with open(path, encoding="utf-8", errors=BODY_ERRORS) as handle:
            payload = json.load(handle)
        if not isinstance(payload, dict) or not isinstance(payload.get("body"), str):
            return False
        new = stamped(payload["body"])
        if new is None:
            return False
        # The same request as a field: nothing to write, so nothing to clean
        # up. An issue comment takes only `body`, so other keys are dropped.
        args[i:i + span] = ["--raw-field", "body=" + new]
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
        for i, arg in enumerate(out):
            if arg in ("--body-file", "-F") and i + 1 < len(out):
                path, span = out[i + 1], 2
            elif arg.startswith("--body-file="):
                path, span = arg[len("--body-file="):], 1
            else:
                continue
            if path == "-":
                return out
            with open(path, encoding="utf-8", errors=BODY_ERRORS) as handle:
                new = stamped(handle.read())
            if new is not None:
                out[i:i + span] = ["--body", new]
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
    # The deadline covers this call only. It is cancelled, and the previous
    # handler put back, however the call ends: an alarm left pending would
    # end whatever process imported this module, with status 0, seconds later.
    alarm = hasattr(signal, "SIGALRM")
    previous = signal.signal(signal.SIGALRM, _give_up) if alarm else None
    if alarm:
        signal.alarm(DEADLINE_SECONDS)
    try:
        try:
            result = rewrite(args)
        except Exception:  # noqa: BLE001 - never block a comment over a stamp
            return 0
        sys.stdout.buffer.write(
            b"".join(item.encode("utf-8", BODY_ERRORS) + b"\0"
                     for item in [SENTINEL, *result, END]))
        return 0
    finally:
        if alarm:
            signal.alarm(0)
            signal.signal(signal.SIGALRM, previous)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
