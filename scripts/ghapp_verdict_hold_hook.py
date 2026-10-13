#!/usr/bin/env python3
"""Before a HOLD verdict posts, take its exact head out of the landing path.

A HOLD posted on a head that is already armed or queued did not stop it: the
queue merged it anyway (six Generous-Corp/pulp pull requests on 2026-10-12/13
landed minutes after a `verdict: HOLD` naming their merge head). The
queue-arm guard judges only the arm, so the verdict itself has to act.

`scripts/ghapp` runs this hook before the native `gh` for every command. It
recognises a HOLD being posted on a pull request, in any of these forms:

* `pr comment <selector>` with `--body/-b` or `--body-file/-F`, the selector
  anywhere among the arguments: a number, `#N`, a pull request URL, or none
  (the current branch's pull request);
* `pr review <selector> --request-changes/-r` (a HOLD on the current head
  unless its body names one), or any `pr review` whose body is a HOLD;
* `api repos/O/R/issues/N/comments` (POST) with the body in `--input FILE`
  (a JSON object with `body`) or `-f/-F body=...` (`body=@FILE` reads a file);
* `api repos/O/R/issues/comments/ID` (PATCH, editing a comment into a HOLD):
  the pull request comes from that comment's `issue_url`;
* `api repos/O/R/pulls/N/reviews` with `event: REQUEST_CHANGES` (on its
  `commit_id`, else the current head), or a body that is a HOLD;
* `api graphql` with `addComment` (a HOLD body on a pull request subject) or
  `addPullRequestReview` (`REQUEST_CHANGES`, or a HOLD body), the subject and
  body read from the document or its variables.

A body is a HOLD when its last non-blank line is `verdict: HOLD head:<40-hex>`.
The hook reads the pull request and, only when its current head is exactly
that commit, dequeues it (`dequeuePullRequest`) if it is queued, else
disables auto-merge (`disablePullRequestAutoMerge`) if it is armed. Nothing
else is touched: another pull request, another head, and a pull request that
is neither armed nor queued are left alone. Each action, and each failure to
act, is appended to `holds.jsonl` in the guards directory.

When it cannot act on a recognised HOLD (the read or the mutation failed), it
posts a follow-up comment on the pull request saying so ("this HOLD could not
dequeue/disarm head <sha>: <error>; it may still land"), so the record on
GitHub never shows an ineffective HOLD as an effective one. When it cannot
read the body at all (`--input -`, `body=@-`, `--body-file -`) of a comment
or review being posted, it prints that a HOLD in it would not be acted on.

The mutation goes straight to the real `gh` with the App token `ghapp`
exported, not back through `ghapp`. Posting a HOLD naming a pull request's
head is therefore the one sanctioned route around `queue-removal-guard`,
which keeps refusing agents' own dequeues and disarms; the public HOLD is its
trace.

The hook never blocks the HOLD from posting: it always exits 0.

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
from typing import Any, Optional

HOST_ENABLE_ENV = "SHIPYARD_HOLD_DISARMS"
HOST_ENABLE_FILE = "hold-disarms"
HOLD_LINE = re.compile(r"^\s*verdict:\s*HOLD\s+head:\s*([0-9a-fA-F]{40})\s*$")
SHA = re.compile(r"[0-9a-fA-F]{40}")
COMMENTS_PATH = re.compile(r"^repos/([^/]+/[^/]+)/issues/(\d+)/comments$")
COMMENT_PATH = re.compile(r"^repos/([^/]+/[^/]+)/issues/comments/(\d+)$")
REVIEWS_PATH = re.compile(r"^repos/([^/]+/[^/]+)/pulls/(\d+)/reviews$")
PULL_URL = re.compile(r"^https?://github\.com/([^/]+/[^/]+)/pull/(\d+)(?:[/?#].*)?$")
ISSUE_URL = re.compile(r"/repos/([^/]+/[^/]+)/issues/(\d+)$")
STATE_QUERY = (
    "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name)"
    "{pullRequest(number:$number){id headRefOid autoMergeRequest{enabledAt}"
    " mergeQueueEntry{id state}}}}"
)
NODE_QUERY = (
    "query($id:ID!){node(id:$id){... on PullRequest{number repository{nameWithOwner}}}}"
)
DEQUEUE = "mutation($id:ID!){dequeuePullRequest(input:{id:$id}){mergeQueueEntry{id}}}"
DISARM = (
    "mutation($id:ID!){disablePullRequestAutoMerge(input:{pullRequestId:$id})"
    "{pullRequest{number}}}"
)
# `pr comment` / `pr review` options that take a value.
PR_VALUE_OPTIONS = {"--body", "-b", "--body-file", "-F", "--repo", "-R"}
CURRENT = "current"  # A HOLD on whatever head the pull request has now.


class Unreadable(Exception):
    """A comment or review is being posted whose body cannot be read."""


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
                                                   "graphql_document", "GuardError")):
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


def hold_head(body: Optional[str]) -> str | None:
    """The head a body holds, from its last non-blank line."""
    if not isinstance(body, str):
        return None
    lines = [line for line in body.splitlines() if line.strip()]
    match = HOLD_LINE.match(lines[-1]) if lines else None
    return match.group(1).lower() if match else None


def _read(path: str) -> str:
    if path == "-":
        raise Unreadable("the body is read from stdin")
    return pathlib.Path(path).read_text(encoding="utf-8")


def _field(args: list[str], name: str) -> str | None:
    for value in PARSER.option_values(args, {"-f", "-F", "--field", "--raw-field"}):
        if value.startswith(f"{name}="):
            text = value[len(name) + 1:]
            return _read(text[1:]) if text.startswith("@") else text
    return None


def _input_json(args: list[str]) -> dict[str, Any] | None:
    for path in PARSER.option_values(args, {"--input"}):
        value = json.loads(_read(path))
        return value if isinstance(value, dict) else None
    return None


def _pr_selector(args: list[str]) -> tuple[str | None, int | None]:
    """`(repo, number)` from a `pr comment|review` selector; Nones when absent."""
    rest = args[2:]
    index = 0
    while index < len(rest):
        arg = rest[index]
        if arg in PR_VALUE_OPTIONS:
            index += 2
            continue
        index += 1
        if arg.startswith("-"):
            continue
        if match := PULL_URL.match(arg):
            return match.group(1), int(match.group(2))
        digits = arg.removeprefix("#")
        if digits.isdigit():
            return None, int(digits)
    return None, None


def _current_pr(repo: str | None) -> tuple[str, int]:
    """The current branch's pull request, as `gh pr view` resolves it."""
    arguments = ["pr", "view", "--json", "number,url"]
    if repo:
        arguments += ["--repo", repo]
    answer = PARSER.run_real_gh(arguments)
    match = PULL_URL.match(str((answer or {}).get("url") or ""))
    if not match:
        raise PARSER.GuardError("the current branch has no pull request")
    return match.group(1), int(match.group(2))


def _pr_command(args: list[str]) -> dict[str, Any] | None:
    review = args[1] == "review"
    files = PARSER.option_values(args, {"--body-file", "-F"})
    bodies = PARSER.option_values(args, {"--body", "-b"})
    if bodies:
        body: str | None = bodies[0]
    elif files:
        body = _read(files[0])
    else:
        body = None
    head = hold_head(body)
    if head is None and review and any(arg in ("--request-changes", "-r") for arg in args[2:]):
        head = CURRENT
    if head is None:
        return None
    repo, number = _pr_selector(args)
    repo = repo or (PARSER.option_values(args, {"--repo", "-R"}) or [os.environ.get("GH_REPO") or None])[0]
    if number is None:
        repo, number = _current_pr(repo)
    if not repo:
        raise PARSER.GuardError(f"cannot tell which repository #{number} is in")
    return {"repo": repo, "pr": number, "head": head}


def _graphql_hold(args: list[str]) -> dict[str, Any] | None:
    document = PARSER.graphql_document(args)
    adding_comment = "addComment" in document
    adding_review = "addPullRequestReview" in document
    if not (adding_comment or adding_review):
        return None
    variables: dict[str, Any] = {}
    data = _input_json(args)
    if data and isinstance(data.get("variables"), dict):
        variables.update(data["variables"])
    for value in PARSER.option_values(args, {"-f", "-F", "--field", "--raw-field"}):
        key, _, text = value.partition("=")
        if key != "query":
            variables[key] = _read(text[1:]) if text.startswith("@") else text

    def inline(name: str) -> str | None:
        match = re.search(r"\b" + name + r'\s*:\s*"((?:[^"\\]|\\.)*)"', document)
        if match:
            return json.loads(f'"{match.group(1)}"')
        match = re.search(rf"\b{name}\s*:\s*\$(\w+)", document)
        return variables.get(match.group(1)) if match else None

    body = inline("body")
    head = hold_head(body)
    requests_changes = bool(re.search(r"\bevent\s*:\s*REQUEST_CHANGES\b", document)) \
        or variables.get("event") == "REQUEST_CHANGES"
    if head is None and adding_review and requests_changes:
        commit = inline("commitOID")
        head = commit.lower() if isinstance(commit, str) and SHA.fullmatch(commit) else CURRENT
    if head is None:
        return None
    subject = inline("subjectId") if adding_comment else inline("pullRequestId")
    if not isinstance(subject, str) or not subject:
        raise PARSER.GuardError("the mutation names no subject")
    answer = PARSER.run_real_gh(["api", "graphql", "-f", f"query={NODE_QUERY}", "-f",
                                 f"id={subject}"])
    node = ((answer or {}).get("data") or {}).get("node") or {}
    if "number" not in node:
        return None  # An issue, not a pull request.
    return {"repo": node["repository"]["nameWithOwner"], "pr": int(node["number"]), "head": head}


def posted_hold(args: list[str]) -> dict[str, Any] | None:
    """`{repo, pr, head}` when `args` posts a HOLD on a pull request.

    Raises Unreadable when it posts a comment or review whose body cannot be
    read, and the parser's GuardError when the pull request cannot be told.
    """
    if args[:2] in (["pr", "comment"], ["pr", "review"]):
        return _pr_command(args)
    if args[:1] != ["api"]:
        return None
    path, _ = PARSER.api_target(args)
    if path == "graphql":
        return _graphql_hold(args)
    for pattern in (COMMENTS_PATH, COMMENT_PATH, REVIEWS_PATH):
        match = pattern.match(path)
        if not match:
            continue
        body = _field(args, "body")
        data = _input_json(args) if body is None else None
        if body is None and data:
            body = data.get("body")
        head = hold_head(body)
        if pattern is REVIEWS_PATH and head is None:
            event = (data or {}).get("event") or _field(args, "event")
            if event == "REQUEST_CHANGES":
                commit = (data or {}).get("commit_id") or _field(args, "commit_id")
                head = commit.lower() if isinstance(commit, str) and SHA.fullmatch(commit) else CURRENT
        if head is None:
            return None
        repo, number = match.group(1), int(match.group(2))
        if pattern is COMMENT_PATH:
            comment = PARSER.run_real_gh(["api", f"repos/{repo}/issues/comments/{number}"])
            issue = ISSUE_URL.search(str((comment or {}).get("issue_url") or ""))
            if not issue:
                raise PARSER.GuardError(f"comment {number} names no issue")
            repo, number = issue.group(1), int(issue.group(2))
        return {"repo": repo, "pr": number, "head": head}
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
    head = live if head == CURRENT else head
    record: dict[str, Any] = {"repo": repo, "pr": number, "hold_head": head, "head": live}
    if live != head:
        return {**record, "action": "none", "why": "the HOLD names another head"}
    if pull.get("mergeQueueEntry"):
        try:
            _graphql(DEQUEUE, {"id": pull["id"]})
        except PARSER.GuardError as error:
            raise PARSER.GuardError(f"dequeue failed: {error}") from error
        return {**record, "action": "dequeued"}
    if pull.get("autoMergeRequest"):
        try:
            _graphql(DISARM, {"id": pull["id"]})
        except PARSER.GuardError as error:
            raise PARSER.GuardError(f"disarm failed: {error}") from error
        return {**record, "action": "disarmed"}
    return {**record, "action": "none", "why": "neither armed nor queued"}


def announce_failure(repo: str, number: int, head: str, error: str) -> bool:
    """Say on the pull request that this HOLD did not take effect."""
    shown = "its current head" if head == CURRENT else f"head {head}"
    body = (
        f"**verdict-hold-hook:** this HOLD could not dequeue/disarm {shown}: {error}; "
        "it may still land. Check `autoMergeRequest`, `mergeQueueEntry` and `headRefOid`, "
        "and dequeue or disarm it by hand (a steward), or re-post the HOLD."
    )
    try:
        PARSER.run_real_gh(["api", "-X", "POST", f"repos/{repo}/issues/{number}/comments",
                            "-f", f"body={body}"])
    except PARSER.GuardError:
        return False
    return True


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
    except Unreadable as error:
        print(f"verdict-hold-hook: cannot read this comment or review ({error}); if it is a "
              "`verdict: HOLD`, it was NOT acted on and its head may still land. Post it with "
              "`--body-file FILE` or `--input FILE` instead.", file=sys.stderr)
        return 0
    except (PARSER.GuardError, OSError, ValueError, KeyError, TypeError) as error:
        print(f"verdict-hold-hook: cannot tell which pull request this HOLD is on ({error}); it "
              "was NOT acted on and its head may still land.", file=sys.stderr)
        return 0
    if target is None:
        return 0
    repo, number, head = target["repo"], target["pr"], target["head"]
    try:
        record = act(repo, number, head)
    except PARSER.GuardError as error:
        announced = announce_failure(repo, number, head, str(error))
        record = {"repo": repo, "pr": number, "hold_head": head, "action": "failed",
                  "why": str(error), "announced": announced}
        audit(record)
        print(
            f"verdict-hold-hook: could not take {repo}#{number} out of the landing path for this "
            f"HOLD ({error}). The HOLD still posts"
            + ("; a comment on the pull request says it may still land." if announced else
               ", and saying so on the pull request failed too.")
            + f" Check: `ghapp pr view {number} --repo {repo} --json "
            "autoMergeRequest,headRefOid` and ask the steward to disarm or dequeue it.",
            file=sys.stderr,
        )
        return 0
    audit(record)
    if record["action"] in ("dequeued", "disarmed"):
        print(
            f"verdict-hold-hook: {record['action']} {repo}#{number} at {record['head'][:12]} "
            "before posting this HOLD; it lands again only after a new verdict approves its head.",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
