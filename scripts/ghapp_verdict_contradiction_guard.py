#!/usr/bin/env python3
"""Refuse a verdict that contradicts a HOLD, or its own session's last verdict.

A HOLD makes its head un-approvable: the review gate (``queue-arm-guard``'s
``review_verdict``, which ``review_verdict_status.py`` calls) ignores every
APPROVE naming a head that a counting HOLD names, before or after the HOLD.
So an APPROVE posted on a held head changes nothing except how the pull
request looks: it reads approved, a reviewer's queue counts it as reviewed,
and nobody acts on the HOLD. This guard stops that APPROVE before it posts.

``ghapp`` runs it after the verdict stamper and before native ``gh``, on
every request. It recognises a verdict in the forms the stamper and the
reviewer-claim guard read: a body whose last non-blank line is
``verdict: APPROVE|HOLD head:<sha>`` or ``reviewed: <sha>`` (an APPROVE), in
``pr``/``issue`` ``comment``/``review``/``close`` options, ``api`` fields,
``--input`` (a file, or stdin when ``ghapp`` spooled it to a file), or a
GraphQL document; and a review event (``pr review --approve`` /
``--request-changes``, REST ``event=APPROVE|REQUEST_CHANGES``, GraphQL
``addPullRequestReview`` with ``event``), on ``commit_id``/``commitOID`` or
else the live head.

Rules, for the pull request's exact head the verdict names:

1. **A held head is not approvable.** An APPROVE is refused when a HOLD that
   the gate counts already names that head, from any signer. "Counts" is the
   gate's own ``counting_verdicts`` with the base's review policy, so this
   guard refuses exactly the APPROVEs the gate would ignore.
2. **One session does not contradict itself.** A HOLD is refused when the same
   session (same host, session id and agent id in its stamp) posted an APPROVE
   of that head through the fleet App in the last ``WINDOW_SECONDS``; an
   APPROVE is refused in the reverse case. A deliberate retraction of one's own
   APPROVE is allowed with ``SHIPYARD_VERDICT_RETRACT=1``: a HOLD is the
   conservative direction, so it has an override and an APPROVE has none.
3. A single request carrying both an APPROVE and a HOLD is refused.

Failure handling follows the direction of harm: when the pull request, its
verdicts or the base policy cannot be read, an APPROVE is refused (retry it)
and a HOLD posts with a warning. Requests that carry no verdict are never
read.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import pathlib
import re
import socket
import sys
from datetime import datetime, timezone
from types import ModuleType
from typing import Any, Iterable

WINDOW_SECONDS = 600
RETRACT_ENV = "SHIPYARD_VERDICT_RETRACT"
NOW_ENV = "SHIPYARD_VERDICT_GUARD_NOW"  # tests: an ISO-8601 UTC time
SESSION_ENVS = ("SHIPYARD_STAMP_SESSION_ID", "CLAUDE_CODE_SESSION_ID",
                "CODEX_COMPANION_SESSION_ID")
AGENT_ENV = "SHIPYARD_AGENT_ID"
SHA = re.compile(r"^[0-9a-fA-F]{40}$")
FIELD_FLAGS = {"-f", "-F", "--field", "--raw-field"}
REVIEW_EVENTS = {"APPROVE": "APPROVE", "REQUEST_CHANGES": "HOLD"}
PREFIX = "verdict-contradiction-guard"


class Unreadable(Exception):
    """A verdict-capable body source this guard cannot read."""


def _load(names: Iterable[str], module_name: str) -> ModuleType | None:
    here = pathlib.Path(__file__).resolve().parent
    for name in names:
        candidate = here / name
        if not candidate.is_file():
            continue
        loader = importlib.machinery.SourceFileLoader(module_name, str(candidate))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        if spec is None:
            continue
        module = importlib.util.module_from_spec(spec)
        try:
            loader.exec_module(module)
        except Exception:  # noqa: BLE001 - "unavailable" is handled by the caller
            continue
        return module
    return None


ARM = _load(("ghapp_queue_arm_guard.py", "queue-arm-guard"), "_contradiction_arm_guard")
CLAIM = _load(("ghapp_reviewer_claim_guard.py", "reviewer-claim-guard"),
              "_contradiction_claim_guard")


# ---------------------------------------------------------------- request text

_STDIN: list[str] = []


def _stdin_text() -> str:
    """stdin's text, left rewound for native ``gh``. ``ghapp`` spools a body
    read from stdin to a file and hands that file to gh as stdin, so it is
    seekable; a pipe is not, and its body is unreadable here."""
    if _STDIN:
        return _STDIN[0]
    try:
        start = os.lseek(0, 0, os.SEEK_CUR)
    except OSError as error:
        raise Unreadable("the body is read from a stdin that cannot be rewound") from error
    chunks = []
    while True:
        chunk = os.read(0, 65536)
        if not chunk:
            break
        chunks.append(chunk)
    os.lseek(0, start, os.SEEK_SET)
    _STDIN.append(b"".join(chunks).decode("utf-8", "surrogateescape"))
    return _STDIN[0]


def _read(path: str) -> str:
    if path == "-":
        return _stdin_text()
    try:
        return pathlib.Path(path).read_text(encoding="utf-8", errors="surrogateescape")
    except OSError as error:
        raise Unreadable(f"cannot read {path}: {error}") from error


def _fields(args: list[str]) -> dict[str, str]:
    """Every ``api`` field, a typed ``@file`` (``-F``/``--field``) read."""
    found: dict[str, str] = {}
    previous = ""
    for argument in args:
        flag, value = None, None
        if previous in FIELD_FLAGS:
            flag, value = previous, argument
        else:
            for prefix in ("--field=", "--raw-field="):
                if argument.startswith(prefix):
                    flag, value = prefix[:-1], argument[len(prefix):]
        previous = argument
        if value is None or "=" not in value:
            continue
        key, _, text = value.partition("=")
        if flag in ("-F", "--field") and text.startswith("@"):
            text = _read(text[1:])
        found[key] = text
    return found


def _inputs(args: list[str]) -> list[dict[str, Any]]:
    payloads: list[dict[str, Any]] = []
    for index, argument in enumerate(args):
        if argument == "--input" and index + 1 < len(args):
            path = args[index + 1]
        elif argument.startswith("--input="):
            path = argument[len("--input="):]
        else:
            continue
        try:
            value = json.loads(_read(path))
        except ValueError:
            continue
        if isinstance(value, dict):
            payloads.append(value)
    return payloads


def _option_texts(args: list[str], text_flags: set[str], file_flags: set[str]) -> list[str]:
    texts: list[str] = []
    for index, argument in enumerate(args):
        following = args[index + 1] if index + 1 < len(args) else None
        if argument in text_flags and following is not None:
            texts.append(following)
        elif argument in file_flags and following is not None:
            texts.append(_read(following))
        else:
            for flag in text_flags:
                if flag.startswith("--") and argument.startswith(flag + "="):
                    texts.append(argument[len(flag) + 1:])
            for flag in file_flags:
                if flag.startswith("--") and argument.startswith(flag + "="):
                    texts.append(_read(argument[len(flag) + 1:]))
    return texts


def _graphql_value(document: str, name: str, variables: dict[str, Any]) -> Any:
    match = re.search(r"\b" + name + r'\s*:\s*"((?:[^"\\]|\\.)*)"', document)
    if match:
        try:
            return json.loads(f'"{match.group(1)}"')
        except ValueError:
            return None
    match = re.search(r"\b" + name + r"\s*:\s*\$(\w+)", document)
    if match:
        return variables.get(match.group(1))
    match = re.search(r"\b" + name + r"\s*:\s*([A-Z_]+)\b", document)
    return match.group(1) if match else None


def verdicts_in(args: list[str]) -> list[tuple[str, str | None]]:
    """``(APPROVE|HOLD, head or None)`` for each verdict ``args`` would post;
    ``None`` is the pull request's live head."""
    found: list[tuple[str, str | None]] = []
    bodies: list[str] = []
    if len(args) >= 2 and args[0] in ("pr", "issue") and args[1] in ("comment", "review", "close"):
        if args[1] == "close":
            bodies += _option_texts(args[2:], {"--comment", "-c"}, set())
        else:
            bodies += _option_texts(args[2:], {"--body", "-b"}, {"--body-file", "-F"})
        if args[:2] == ["pr", "review"]:
            rest = args[2:]
            if any(arg in ("--approve", "-a") for arg in rest):
                found.append(("APPROVE", None))
            if any(arg in ("--request-changes", "-r") for arg in rest):
                found.append(("HOLD", None))
    elif args[:1] == ["api"]:
        fields = _fields(args)
        payloads = _inputs(args)
        variables: dict[str, Any] = {}
        documents: list[str] = []
        for payload in payloads:
            if isinstance(payload.get("body"), str):
                bodies.append(payload["body"])
            if isinstance(payload.get("variables"), dict):
                variables.update(payload["variables"])
            if isinstance(payload.get("query"), str):
                documents.append(payload["query"])
        variables.update({k: v for k, v in fields.items() if k != "query"})
        if "query" in fields:
            documents.append(fields["query"])
        if isinstance(fields.get("body"), str):
            bodies.append(fields["body"])
        commit = fields.get("commit_id") or next(
            (p.get("commit_id") for p in payloads if isinstance(p.get("commit_id"), str)), None)
        event = fields.get("event") or next(
            (p.get("event") for p in payloads if isinstance(p.get("event"), str)), None)
        if not documents and isinstance(event, str) and event.upper() in REVIEW_EVENTS:
            found.append((REVIEW_EVENTS[event.upper()],
                          commit.lower() if isinstance(commit, str) and SHA.match(commit) else None))
        for document in documents:
            body = _graphql_value(document, "body", variables)
            if isinstance(body, str):
                bodies.append(body)
            if "addpullrequestreview" in document.lower():
                event = _graphql_value(document, "event", variables)
                if isinstance(event, str) and event.upper() in REVIEW_EVENTS:
                    commit = _graphql_value(document, "commitOID", variables)
                    found.append((REVIEW_EVENTS[event.upper()],
                                  commit.lower() if isinstance(commit, str) and SHA.match(commit)
                                  else None))
    for body in bodies:
        marker = ARM.marker_verdict(body) if isinstance(body, str) else None
        if marker is not None:
            found.append((marker[0], marker[1].lower()))
    return found


# ---------------------------------------------------------------- identity

def own_identity() -> tuple[str, str, str | None] | None:
    """``(host, session, agent)`` exactly as the stamper writes this process's
    stamp; ``None`` when the session is unattributed."""
    session = next((os.environ[name].strip() for name in SESSION_ENVS
                    if os.environ.get(name, "").strip()), "")
    if not session:
        return None
    agent = os.environ.get(AGENT_ENV, "").strip()
    agent = agent if agent and not any(c.isspace() for c in agent) else None
    return socket.gethostname().split(".")[0].lower(), session, agent


def stamp_identity(body: Any) -> tuple[str, str, str | None] | None:
    if not isinstance(body, str):
        return None
    found = ARM._SESSION_STAMP.findall(body)
    if not found:
        return None
    host, session, agent = found[-1]
    return host.lower(), session, agent or None


def _now() -> datetime:
    configured = os.environ.get(NOW_ENV, "").strip()
    if configured:
        return datetime.fromisoformat(configured.replace("Z", "+00:00"))
    return datetime.now(timezone.utc)


def _at(record: dict[str, Any], key: str) -> datetime | None:
    value = record.get(key)
    if not isinstance(value, str):
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def _is_app(record: dict[str, Any]) -> bool:
    user = record.get("user") or {}
    return (isinstance(user.get("login"), str) and user.get("type") == "Bot"
            and ARM._normalized_login(user["login"]) == ARM.FLEET_AUTHOR)


def _record_verdict(record: dict[str, Any], kind: str) -> tuple[str, str] | None:
    """The verdict one comment (``kind`` comment) or review states, by marker
    for a comment, by state then marker for a review."""
    if kind == "review":
        state = str(record.get("state") or "").upper()
        mapped = {"APPROVED": "APPROVE", "CHANGES_REQUESTED": "HOLD"}.get(state)
        if mapped and isinstance(record.get("commit_id"), str):
            return mapped, record["commit_id"].lower()
    body = record.get("body")
    marker = ARM.marker_verdict(body) if isinstance(body, str) else None
    return (marker[0], marker[1].lower()) if marker else None


# ---------------------------------------------------------------- the rules

def held_by(head: str, author: str | None, comments: list[dict[str, Any]],
            reviews: list[dict[str, Any]], policy: dict[str, Any]) -> dict[str, str] | None:
    """The first HOLD the review gate counts on ``head``, with its signer and
    URL; the gate's own ``counting_verdicts``, one record at a time."""
    candidates = [("comment", record, [record], []) for record in comments]
    candidates += [("review", record, [], [record]) for record in reviews]
    for kind, record, as_comment, as_review in candidates:
        for _at_time, verdict, named, what in ARM.counting_verdicts(
                author, as_comment, as_review, policy):
            if verdict == "HOLD" and named == head:
                signer = what.removeprefix("held by ").split(" at ", 1)[0]
                return {"signer": signer, "url": str(record.get("html_url") or what),
                        "at": str(record.get("submitted_at") or record.get("created_at") or "")}
    return None


def own_contradiction(kind: str, head: str, comments: list[dict[str, Any]],
                      reviews: list[dict[str, Any]], now: datetime) -> dict[str, str] | None:
    """This session's opposite verdict on ``head`` within the window."""
    me = own_identity()
    if me is None:
        return None
    opposite = "HOLD" if kind == "APPROVE" else "APPROVE"
    records = [("comment", r, "created_at") for r in comments]
    records += [("review", r, "submitted_at") for r in reviews]
    for record_kind, record, time_key in records:
        if not _is_app(record):
            continue
        verdict = _record_verdict(record, record_kind)
        if verdict != (opposite, head):
            continue
        posted = _at(record, time_key)
        if posted is None or not 0 <= (now - posted).total_seconds() <= WINDOW_SECONDS:
            continue
        if stamp_identity(record.get("body")) != me:
            continue
        return {"verdict": opposite, "url": str(record.get("html_url") or ""),
                "age": f"{int((now - posted).total_seconds())}s"}
    return None


def _refuse(message: str) -> int:
    print(f"{PREFIX}: refused: {message}", file=sys.stderr)
    return 1


def judge(args: list[str]) -> int:
    if ARM is None or not hasattr(ARM, "counting_verdicts"):
        # Cannot even tell what a verdict is: refuse only what names one.
        if any("verdict:" in a and "APPROVE" in a for a in args) or "--approve" in args:
            return _refuse("queue-arm-guard is not installed next to this guard, so an "
                           "APPROVE cannot be checked against HOLDs; run "
                           "`shipyard guards install`")
        return 0
    try:
        verdicts = verdicts_in(args)
    except Unreadable as error:
        print(f"{PREFIX}: warning: cannot read this request's body ({error}); if it is a "
              "verdict it was not checked against HOLDs. Post verdicts with --body-file FILE or "
              "-f body=...", file=sys.stderr)
        return 0
    if not verdicts:
        return 0
    kinds = {kind for kind, _ in verdicts}
    if kinds == {"APPROVE", "HOLD"}:
        return _refuse("this one request posts both an APPROVE and a HOLD; post one verdict")
    kind = kinds.pop()
    approving = kind == "APPROVE"
    heads = {head for _, head in verdicts if head is not None}
    if len(heads) > 1:
        return _refuse("this request's verdicts name more than one head; post one verdict")

    def unreadable(error: object) -> int:
        if approving:
            return _refuse(f"cannot check whether this head is held ({error}); an APPROVE is "
                           "not posted unchecked. Retry it.")
        print(f"{PREFIX}: warning: could not check this HOLD against earlier verdicts "
              f"({error}); it posts anyway.", file=sys.stderr)
        return 0

    repo = os.environ.get("GH_REPO", "").strip() or None
    try:
        number, endpoint_repo = CLAIM._target(args, repo) if CLAIM else (None, None)
    except Exception as error:  # noqa: BLE001 - see unreadable()
        return unreadable(f"cannot tell which pull request: {error}")
    repo = repo or endpoint_repo
    if number is None or not repo:
        if approving:
            return unreadable("cannot tell which pull request this verdict is on")
        return 0
    try:
        pull = ARM.run_real_gh(["api", f"repos/{repo}/pulls/{number}"])
        if not isinstance(pull, dict):
            return 0  # An issue, not a pull request: nothing to judge.
        live = str((pull.get("head") or {}).get("sha") or "").lower()
        base = (pull.get("base") or {}).get("ref")
        author = (pull.get("user") or {}).get("login")
        head = heads.pop() if heads else live
        if not SHA.match(head):
            return unreadable("the pull request has no exact head")
        comments = ARM._rest_pages(f"repos/{repo}/issues/{number}/comments?per_page=100")
        reviews = ARM._rest_pages(f"repos/{repo}/pulls/{number}/reviews?per_page=100")
        if approving:
            if not isinstance(base, str) or not base:
                return unreadable("the pull request carries no base branch")
            policy = ARM.review_policy(ARM.base_config_text(repo, base), repo, True)
            hold = held_by(head, author if isinstance(author, str) else None,
                           comments, reviews, policy)
            if hold is not None:
                return _refuse(
                    f"head {head} is held by {hold['signer']} at {hold['url']}; a HOLD makes a "
                    "head un-approvable. If the HOLD is wrong, the author must push a new head "
                    f"(merge the base) and you review that; do not approve this head. "
                    f"(https://github.com/{repo}/pull/{number})")
        contradiction = own_contradiction(kind, head, comments, reviews, _now())
    except Exception as error:  # noqa: BLE001 - see unreadable()
        return unreadable(error)
    if contradiction is not None:
        if approving:
            return _refuse(
                f"this session posted a HOLD of head {head} {contradiction['age']} ago at "
                f"{contradiction['url']}; a reviewer does not contradict itself, and a HOLD "
                "makes a head un-approvable. Only a new head can be approved.")
        if os.environ.get(RETRACT_ENV) != "1":
            return _refuse(
                f"this session posted an APPROVE of head {head} {contradiction['age']} ago at "
                f"{contradiction['url']}; a reviewer does not post contradictory verdicts in one "
                "pass. Decide which is right. To retract that APPROVE deliberately (the HOLD "
                f"makes the head un-approvable), re-run with {RETRACT_ENV}=1.")
        print(f"{PREFIX}: note: retracting this session's APPROVE of head {head} "
              f"({contradiction['url']}) with a HOLD ({RETRACT_ENV}=1).", file=sys.stderr)
    return 0


def main(args: list[str]) -> int:
    try:
        return judge(args)
    except Exception as error:  # noqa: BLE001 - an APPROVE fails closed
        approving = any("APPROVE" in a for a in args) or "--approve" in args or "-a" in args
        if approving:
            return _refuse(f"internal error ({error}); an APPROVE is not posted unchecked")
        print(f"{PREFIX}: warning: internal error ({error}); not checked", file=sys.stderr)
        return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
