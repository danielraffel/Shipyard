#!/usr/bin/env python3
"""Before a HOLD verdict posts, take its exact head out of the landing path.

A HOLD posted on a head that is already armed or queued did not stop it: the
queue merged it anyway (six Generous-Corp/pulp pull requests on 2026-10-12/13
landed minutes after a `verdict: HOLD` naming their merge head). The
queue-arm guard judges only the arm, so the verdict itself has to act.

`scripts/ghapp` runs this hook before the native `gh` for every command. It
recognises a HOLD being posted on a pull request:

* `api repos/O/R/issues/N/comments` with the body in `--input FILE` (a JSON
  object with `body`) or `-f/-F body=...` (`body=@FILE` reads a file);
* `pr comment N` with `--body/-b TEXT` or `--body-file/-F FILE`;
* `api repos/O/R/pulls/N/reviews` with `--input FILE` whose `event` is
  `REQUEST_CHANGES` and whose `commit_id` names the head.

A comment is a HOLD when its last non-blank line is `verdict: HOLD
head:<40-hex>`. The hook then reads the pull request and, only when its
current head is exactly that commit, dequeues it (`dequeuePullRequest`) if it
is queued, else disables auto-merge (`disablePullRequestAutoMerge`) if it is
armed. Nothing else is touched: another pull request, another head, and a pull
request that is neither armed nor queued are left alone. Each action, and each
failure to act, is appended to `holds.jsonl` in the guards directory.

The mutation goes straight to the real `gh` with the App token `ghapp`
exported, not back through `ghapp`, so the queue-removal guard keeps refusing
agents' own dequeues and disarms while the verdict flow can still act.

The hook never blocks the HOLD from posting: it always exits 0, and a failure
to read or act is printed with what to do instead.

None of this applies unless the host turns it on: `SHIPYARD_HOLD_DISARMS=1`
(`0` forces it off), or a file named `hold-disarms` in the guards directory.
Off, it reads nothing.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import pathlib
import re
import sys
from datetime import datetime, timezone
from types import ModuleType
from typing import Any

HOST_ENABLE_ENV = "SHIPYARD_HOLD_DISARMS"
HOST_ENABLE_FILE = "hold-disarms"
HOLD_LINE = re.compile(r"^\s*verdict:\s*HOLD\s+head:\s*([0-9a-fA-F]{40})\s*$")
COMMENTS_PATH = re.compile(r"^repos/([^/]+/[^/]+)/issues/(\d+)/comments$")
REVIEWS_PATH = re.compile(r"^repos/([^/]+/[^/]+)/pulls/(\d+)/reviews$")
STATE_QUERY = (
    "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name)"
    "{pullRequest(number:$number){id headRefOid autoMergeRequest{enabledAt}"
    " mergeQueueEntry{id state}}}}"
)
DEQUEUE = "mutation($id:ID!){dequeuePullRequest(input:{id:$id}){mergeQueueEntry{id}}}"
DISARM = (
    "mutation($id:ID!){disablePullRequestAutoMerge(input:{pullRequestId:$id})"
    "{pullRequest{number}}}"
)


def _load_request_parser() -> ModuleType | None:
    here = pathlib.Path(__file__).resolve().parent
    for name in ("ghapp_queue_removal_guard.py", "queue-removal-guard"):
        candidate = here / name
        if not candidate.is_file():
            continue
        loader = importlib.machinery.SourceFileLoader("_ghapp_request_parser", str(candidate))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        if spec is None:
            continue
        module = importlib.util.module_from_spec(spec)
        try:
            loader.exec_module(module)
        except Exception:  # noqa: BLE001 - any failure means "parser unavailable"
            continue
        if all(hasattr(module, name) for name in ("api_target", "option_values", "run_real_gh",
                                                   "GuardError")):
            return module
    return None


PARSER = _load_request_parser()


def _guards_dir() -> pathlib.Path:
    return pathlib.Path(
        os.environ.get("SHIPYARD_GHAPP_GUARDS_DIR")
        or pathlib.Path.home() / ".config" / "shipyard" / "guards"
    )


def host_enabled() -> bool:
    switch = os.environ.get(HOST_ENABLE_ENV)
    if switch == "1":
        return True
    if switch == "0":
        return False
    return (_guards_dir() / HOST_ENABLE_FILE).is_file()


def hold_head(body: str) -> str | None:
    """The head a comment body holds, from its last non-blank line."""
    lines = [line for line in body.splitlines() if line.strip()]
    match = HOLD_LINE.match(lines[-1]) if lines else None
    return match.group(1).lower() if match else None


def _read(path: str) -> str:
    return pathlib.Path(path).read_text(encoding="utf-8")


def _field_body(args: list[str]) -> str | None:
    for value in PARSER.option_values(args, {"-f", "-F", "--field", "--raw-field"}):
        if value.startswith("body="):
            body = value.removeprefix("body=")
            return _read(body[1:]) if body.startswith("@") and body != "@-" else body
    return None


def _input_json(args: list[str]) -> dict[str, Any] | None:
    for path in PARSER.option_values(args, {"--input"}):
        if path == "-":
            return None
        value = json.loads(_read(path))
        return value if isinstance(value, dict) else None
    return None


def posted_hold(args: list[str]) -> tuple[str, int, str] | None:
    """`(repo, number, head)` when `args` posts a HOLD on a pull request."""
    if args[:2] == ["pr", "comment"]:
        positional = [arg for arg in args[2:3] if arg.isdigit()]
        repo = (PARSER.option_values(args, {"--repo", "-R"}) or [os.environ.get("GH_REPO", "")])[0]
        bodies = PARSER.option_values(args, {"--body", "-b"})
        files = PARSER.option_values(args, {"--body-file", "-F"})
        body = bodies[0] if bodies else (_read(files[0]) if files and files[0] != "-" else None)
        head = hold_head(body) if body is not None else None
        if positional and repo and head:
            return repo, int(positional[0]), head
        return None
    if args[:1] != ["api"]:
        return None
    path, _ = PARSER.api_target(args)
    if match := COMMENTS_PATH.match(path):
        body = _field_body(args)
        if body is None:
            data = _input_json(args)
            body = data.get("body") if data else None
        head = hold_head(body) if isinstance(body, str) else None
        return (match.group(1), int(match.group(2)), head) if head else None
    if match := REVIEWS_PATH.match(path):
        data = _input_json(args)
        commit = data.get("commit_id") if data else None
        if data and data.get("event") == "REQUEST_CHANGES" and isinstance(commit, str) \
                and re.fullmatch(r"[0-9a-fA-F]{40}", commit):
            return match.group(1), int(match.group(2)), commit.lower()
    return None


def _graphql(query: str, variables: dict[str, Any]) -> Any:
    arguments = ["api", "graphql", "-f", f"query={query}"]
    for key, value in variables.items():
        arguments += ["-F" if isinstance(value, int) else "-f", f"{key}={value}"]
    return PARSER.run_real_gh(arguments)


def act(repo: str, number: int, head: str) -> dict[str, Any]:
    """Dequeue or disarm `repo#number` when it is armed or queued at `head`."""
    owner, _, name = repo.partition("/")
    answer = _graphql(STATE_QUERY, {"owner": owner, "name": name, "number": number})
    pull = ((answer or {}).get("data") or {}).get("repository", {}).get("pullRequest")
    if not isinstance(pull, dict) or not isinstance(pull.get("id"), str):
        raise PARSER.GuardError(f"{repo}#{number} could not be read")
    live = str(pull.get("headRefOid") or "").lower()
    record: dict[str, Any] = {"repo": repo, "pr": number, "hold_head": head, "head": live}
    if live != head:
        return {**record, "action": "none", "why": "the HOLD names another head"}
    if pull.get("mergeQueueEntry"):
        _graphql(DEQUEUE, {"id": pull["id"]})
        return {**record, "action": "dequeued"}
    if pull.get("autoMergeRequest"):
        _graphql(DISARM, {"id": pull["id"]})
        return {**record, "action": "disarmed"}
    return {**record, "action": "none", "why": "neither armed nor queued"}


def audit(record: dict[str, Any]) -> None:
    line = {"at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"), **record}
    try:
        directory = _guards_dir()
        directory.mkdir(parents=True, exist_ok=True)
        with (directory / "holds.jsonl").open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(line, sort_keys=True) + "\n")
    except OSError:
        pass


def main(args: list[str]) -> int:
    if not host_enabled() or PARSER is None:
        return 0
    try:
        target = posted_hold(args)
    except (PARSER.GuardError, OSError, ValueError):
        return 0
    if target is None:
        return 0
    repo, number, head = target
    try:
        record = act(repo, number, head)
    except PARSER.GuardError as error:
        record = {"repo": repo, "pr": number, "hold_head": head, "action": "failed",
                  "why": str(error)}
        audit(record)
        print(
            f"verdict-hold-hook: could not take {repo}#{number} out of the landing path for this "
            f"HOLD ({error}). The HOLD still posts. Check it yourself: `ghapp pr view {number} "
            f"--repo {repo} --json autoMergeRequest,headRefOid` and, if it is armed or queued at "
            f"{head[:12]}, ask the steward to disarm or dequeue it.",
            file=sys.stderr,
        )
        return 0
    audit(record)
    if record["action"] in ("dequeued", "disarmed"):
        print(
            f"verdict-hold-hook: {record['action']} {repo}#{number} at {head[:12]} before posting "
            "this HOLD; it lands again only after a new verdict approves its head.",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
