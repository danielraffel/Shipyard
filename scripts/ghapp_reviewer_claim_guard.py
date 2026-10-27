#!/usr/bin/env python3
"""Require a live durable reviewer claim before a verdict is posted.

The guard is intentionally inert unless the protected base config enables both
``reviewer_claim_receipt`` and ``reviewer_claim_app``. When enabled, it binds
the verdict marker to the exact PR head and asks the release-matched Shipyard
binary to validate the local claim immediately before native ``gh`` writes the
comment or review. Expired, revoked, replaced, legacy, and malformed state all
fail closed.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
from typing import Any

VERDICT = re.compile(
    r"(?:verdict:\s*(?:APPROVE|HOLD)\s+head:\s*([0-9a-fA-F]{40})|"
    r"reviewed:\s*([0-9a-fA-F]{40}))",
    re.IGNORECASE,
)
PR_ENDPOINT = re.compile(
    r"^(?:https?://[^/]+/(?:api/v3/)?)?repos/([^/]+)/([^/]+)/issues/"
    r"([1-9][0-9]*)/comments(?:/([1-9][0-9]*))?/?$"
)
PR_URL = re.compile(r"/(?:pull|issues)/([1-9][0-9]*)(?:/|$)")
PULL_REVIEW_ENDPOINT = re.compile(r"^repos/([^/]+)/([^/]+)/pulls/([1-9][0-9]*)/reviews(?:/.*)?/?$")
COMMENT_ENDPOINT = re.compile(r"^repos/([^/]+)/([^/]+)/issues/comments/([1-9][0-9]*)/?$")


def _input_paths(args: list[str]) -> list[str]:
    paths: list[str] = []
    for index, argument in enumerate(args):
        if argument == "--input" and index + 1 < len(args):
            paths.append(args[index + 1])
        elif argument.startswith("--input="):
            paths.append(argument.split("=", 1)[1])
    return paths


def _input_payloads(args: list[str]) -> list[dict[str, Any]]:
    payloads: list[dict[str, Any]] = []
    for index, argument in enumerate(args):
        if argument == "--input" and index + 1 < len(args):
            path = args[index + 1]
        elif argument.startswith("--input="):
            path = argument.split("=", 1)[1]
        else:
            continue
        if path != "-":
            try:
                value = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
            except (OSError, ValueError):
                continue
            if isinstance(value, dict):
                payloads.append(value)
    return payloads


def _opaque_body_source(args: list[str]) -> bool:
    """Whether a verdict-capable request carries text this guard cannot read."""
    for path in _input_paths(args):
        if path == "-" or not pathlib.Path(path).is_file():
            return True
    previous = ""
    for argument in args:
        value = None
        if previous in {"-F", "--field"} and argument.startswith("body="):
            value = argument[len("body="):]
        elif argument.startswith("--field=body="):
            value = argument.split("=", 2)[2]
        if value is not None and value.startswith("@"):
            if value == "@-" or not pathlib.Path(value[1:]).is_file():
                return True
        if previous in {"-F", "--field"} and argument.startswith("query="):
            value = argument[len("query="):]
            if value.startswith("@") and (value == "@-" or not pathlib.Path(value[1:]).is_file()):
                return True
        elif argument.startswith("--field=query="):
            value = argument.split("=", 2)[2]
            if value.startswith("@") and (value == "@-" or not pathlib.Path(value[1:]).is_file()):
                return True
        previous = argument
    return False


def _potential_verdict_target(args: list[str]) -> bool:
    if len(args) >= 2 and args[0] in {"pr", "issue"} and args[1] in {"comment", "review"}:
        return True
    if args and args[0] == "api":
        if any(argument == "graphql" for argument in args[1:]):
            return True
        return any(
            PR_ENDPOINT.match(argument.split("?", 1)[0].strip("/"))
            or PULL_REVIEW_ENDPOINT.match(argument.split("?", 1)[0].strip("/"))
            or COMMENT_ENDPOINT.match(argument.split("?", 1)[0].strip("/"))
            for argument in args[1:]
        )
    return False


def _guard_module() -> Any:
    # Source checkouts keep the Python source name; `shipyard guards install`
    # publishes the managed executable as `queue-arm-guard`.  The reviewer
    # guard must use the same policy parser in either layout.
    here = pathlib.Path(__file__).resolve().parent
    candidates = (here / "ghapp_queue_arm_guard.py", here / "queue-arm-guard")
    for path in candidates:
        if not path.is_file():
            continue
        loader = importlib.machinery.SourceFileLoader("shipyard_queue_arm_guard", str(path))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        if spec is None:
            continue
        module = importlib.util.module_from_spec(spec)
        try:
            loader.exec_module(module)
        except Exception:  # noqa: BLE001 - fail closed below
            continue
        return module
    raise RuntimeError(f"cannot load managed queue-arm-guard from {here}")


def _body_values(args: list[str]) -> list[str]:
    values: list[str] = []
    previous = ""
    for index, argument in enumerate(args):
        if previous in {"--body", "-b", "--comment", "-c"}:
            values.append(argument)
        elif (previous == "--body-file"
              or (previous == "-F" and args[:2] in (["pr", "comment"], ["pr", "review"],
                                                     ["issue", "comment"]))) \
                and argument != "-":
            try:
                values.append(pathlib.Path(argument).read_text(encoding="utf-8", errors="surrogateescape"))
            except OSError:
                pass
        elif argument.startswith(("--body=", "--comment=")):
            values.append(argument.split("=", 1)[1])
        elif argument.startswith(("--body-file=", "--comment-file=")):
            try:
                values.append(pathlib.Path(argument.split("=", 1)[1]).read_text(
                    encoding="utf-8", errors="surrogateescape"))
            except OSError:
                pass
        elif previous in {"-f", "-F", "--field", "--raw-field"} and argument.startswith("body="):
            value = argument[len("body="):]
            if previous in {"-F", "--field"} and value.startswith("@") and value != "@-":
                try:
                    value = pathlib.Path(value[1:]).read_text(
                        encoding="utf-8", errors="surrogateescape")
                except OSError:
                    value = ""
            values.append(value)
        elif argument.startswith("--field=body="):
            value = argument.split("=", 2)[2]
            if argument.startswith("--field=body=@") and value != "@-":
                try:
                    value = pathlib.Path(value[1:]).read_text(
                        encoding="utf-8", errors="surrogateescape")
                except OSError:
                    value = ""
            values.append(value)
        previous = argument
    for value in _input_payloads(args):
        if isinstance(value.get("body"), str):
            values.append(value["body"])
        variables = value.get("variables")
        if isinstance(variables, dict) and isinstance(variables.get("body"), str):
            values.append(variables["body"])
        query = value.get("query")
        if isinstance(query, str):
            match = re.search(r"\bbody\s*:\s*\"((?:\\.|[^\"])*)\"", query)
            if match:
                try:
                    values.append(json.loads(f'"{match.group(1)}"'))
                except ValueError:
                    pass
    previous = ""
    for argument in args:
        query = None
        if previous in {"-f", "-F", "--field", "--raw-field"} and argument.startswith("query="):
            query = argument[len("query="):]
        elif argument.startswith("--field=query="):
            query = argument.split("=", 2)[2]
        if query is not None and query.startswith("@") and query != "@-":
            try:
                query = pathlib.Path(query[1:]).read_text(encoding="utf-8", errors="surrogateescape")
            except OSError:
                query = ""
        if query:
            match = re.search(r"\bbody\s*:\s*\"((?:\\.|[^\"])*)\"", query)
            if match:
                try:
                    values.append(json.loads(f'"{match.group(1)}"'))
                except ValueError:
                    pass
        previous = argument
    return values


def _target(args: list[str], repo: str | None) -> tuple[int | None, str | None]:
    if len(args) >= 2 and args[0] in {"pr", "issue"} and args[1] in {"comment", "review", "close"}:
        for argument in args[2:]:
            if argument.isdigit() and int(argument) > 0:
                return int(argument), repo
            match = PR_URL.search(argument)
            if match:
                return int(match.group(1)), repo
        if args[1] == "review":
            # `gh pr review` with no positional selector resolves the current
            # branch. Ask native gh for that same pull request instead of
            # allowing a bodyless review to bypass the claim gate.
            gh = os.environ.get("GHAPP_REAL_GH") or shutil.which("gh")
            if gh:
                result = subprocess.run(
                    [gh, "pr", "view", "--repo", repo, "--json", "number"],
                    check=False, capture_output=True, text=True, timeout=30,
                    env=os.environ.copy(),
                )
                if result.returncode == 0:
                    try:
                        number = json.loads(result.stdout).get("number")
                    except ValueError:
                        number = None
                    if isinstance(number, int) and number > 0:
                        return number, repo
    for argument in args:
        endpoint = argument.split("?", 1)[0].strip("/")
        match = PR_ENDPOINT.match(endpoint) or PULL_REVIEW_ENDPOINT.match(endpoint)
        if match:
            return int(match.group(3)), f"{match.group(1)}/{match.group(2)}"
        comment = COMMENT_ENDPOINT.match(endpoint)
        if comment:
            endpoint_repo = f"{comment.group(1)}/{comment.group(2)}"
            gh = os.environ.get("GHAPP_REAL_GH") or shutil.which("gh")
            if gh:
                result = subprocess.run(
                    [gh, "api", f"repos/{endpoint_repo}/issues/comments/{comment.group(3)}", "--jq", ".issue_url"],
                    check=False, capture_output=True, text=True, timeout=30,
                    env=os.environ.copy(),
                )
                if result.returncode == 0:
                    issue = PR_URL.search(result.stdout.strip())
                    if issue:
                        return int(issue.group(1)), endpoint_repo
        for pattern in (r"(?:pullRequest|issue)\s*\(\s*number\s*:\s*([1-9][0-9]*)",):
            query = re.search(pattern, argument, re.IGNORECASE)
            if query:
                return int(query.group(1)), repo
    graphql = _graphql_target(args, repo)
    if graphql is not None:
        return graphql, repo
    return None, None


def _graphql_target(args: list[str], repo: str | None) -> int | None:
    """Resolve a GraphQL review mutation's PullRequest ID to its number."""
    query_values: list[str] = []
    ids: list[str] = []
    previous = ""
    for index, argument in enumerate(args):
        if previous in {"-f", "-F", "--field", "--raw-field"}:
            if argument.startswith("query="):
                query = argument[len("query="):]
                if previous in {"-F", "--field"} and query.startswith("@") and query != "@-":
                    try:
                        query = pathlib.Path(query[1:]).read_text(
                            encoding="utf-8", errors="surrogateescape")
                    except OSError:
                        query = ""
                if query:
                    query_values.append(query)
            elif argument.startswith(("subjectId=", "pullRequestId=")):
                ids.append(argument.split("=", 1)[1])
        elif argument.startswith(("--field=query=", "--raw-field=query=")):
            query = argument.split("=", 2)[2]
            if argument.startswith("--field=") and query.startswith("@") and query != "@-":
                try:
                    query = pathlib.Path(query[1:]).read_text(
                        encoding="utf-8", errors="surrogateescape")
                except OSError:
                    query = ""
            if query:
                query_values.append(query)
        elif argument.startswith(("--field=subjectId=", "--raw-field=subjectId=",
                                  "--field=pullRequestId=", "--raw-field=pullRequestId=")):
            ids.append(argument.split("=", 2)[2])
        previous = argument
    for value in _input_payloads(args):
        if isinstance(value.get("query"), str):
            query_values.append(value["query"])
        variables = value.get("variables")
        if isinstance(variables, dict):
            for key in ("subjectId", "pullRequestId"):
                if isinstance(variables.get(key), str):
                    ids.append(variables[key])
    for query in query_values:
        direct = re.search(r"(?:pullRequest|issue)\s*\(\s*number\s*:\s*([1-9][0-9]*)", query,
                           re.IGNORECASE)
        if direct:
            return int(direct.group(1))
        for key in ("subjectId", "pullRequestId"):
            inline = re.search(rf"{key}\s*:\s*\"?([A-Za-z0-9_=-]+)", query)
            if inline and inline.group(1) not in {"$subjectId", "$pullRequestId"}:
                ids.append(inline.group(1).strip('"'))
            variable = re.search(rf"{key}\s*:\s*\$([A-Za-z0-9_]+)", query)
            if variable:
                for value in _input_payloads(args):
                    variables = value.get("variables")
                    if isinstance(variables, dict) and isinstance(variables.get(variable.group(1)), str):
                        ids.append(variables[variable.group(1)])
    if not ids or not repo:
        return None
    gh = os.environ.get("GHAPP_REAL_GH") or shutil.which("gh")
    if not gh:
        return None
    query = "query($id:ID!){node(id:$id){... on PullRequest{number repository{nameWithOwner}}}}"
    result = subprocess.run(
        [gh, "api", "graphql", "-f", f"query={query}", "-f", f"id={ids[-1]}"],
        check=False, capture_output=True, text=True, timeout=30, env=os.environ.copy(),
    )
    if result.returncode != 0:
        return None
    try:
        value = json.loads(result.stdout)
        node = ((value.get("data") or {}).get("node") or {})
    except ValueError:
        return None
    if node.get("repository", {}).get("nameWithOwner", "").lower() != repo.lower():
        return None
    number = node.get("number")
    return number if isinstance(number, int) and number > 0 else None


def _live_pull(repo: str, pr: int) -> tuple[str, str]:
    gh = os.environ.get("GHAPP_REAL_GH") or shutil.which("gh")
    if not gh:
        raise RuntimeError("native gh is unavailable")
    completed = subprocess.run(
        [gh, "api", f"repos/{repo}/pulls/{pr}"],
        check=False, capture_output=True, text=True, timeout=30,
        env=os.environ.copy(),
    )
    if completed.returncode != 0:
        raise RuntimeError(completed.stderr.strip()[-300:] or "cannot read pull request")
    value = json.loads(completed.stdout)
    head = ((value.get("head") or {}).get("sha") if isinstance(value, dict) else None)
    base = ((value.get("base") or {}).get("ref") if isinstance(value, dict) else None)
    if not isinstance(head, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", head):
        raise RuntimeError("pull request has no exact head")
    if not isinstance(base, str) or not base:
        raise RuntimeError("pull request has no base branch")
    return head.lower(), base


def main(argv: list[str]) -> int:
    bodies = _body_values(argv)
    verdicts = [match.group(1) or match.group(2) for body in bodies
                for match in VERDICT.finditer(body)]
    bodyless_review = (
        len(argv) >= 2 and argv[0] == "pr" and argv[1] == "review"
        and any(argument in {"--approve", "--request-changes"} for argument in argv[2:])
    )
    input_payloads = _input_payloads(argv)
    rest_review = any(re.search(r"(?:^|=)(?:APPROVE|REQUEST_CHANGES)$", argument, re.IGNORECASE)
                      for argument in argv) or any(
                          payload.get("event") in {"APPROVE", "REQUEST_CHANGES"}
                          for payload in input_payloads
                      )
    graphql_review = any(
        "addpullrequestreview" in argument.lower()
        and re.search(r"\bevent\s*:\s*(?:APPROVE|REQUEST_CHANGES)", argument, re.IGNORECASE)
        for argument in argv
    ) or any(re.search(r"(?:^|=)event=(?:APPROVE|REQUEST_CHANGES)$", argument, re.IGNORECASE)
             for argument in argv) or any(
                 isinstance(payload.get("query"), str)
                 and "addpullrequestreview" in payload["query"].lower()
                 and isinstance(payload.get("variables"), dict)
                 and payload["variables"].get("event") in {"APPROVE", "REQUEST_CHANGES"}
                 for payload in input_payloads
             )
    opaque_body = _opaque_body_source(argv) and _potential_verdict_target(argv)
    if not verdicts and not bodyless_review and not rest_review and not graphql_review and not opaque_body:
        return 0
    if verdicts and len(set(head.lower() for head in verdicts)) != 1:
        print("reviewer-claim-guard: refused: verdict bodies name multiple heads", file=sys.stderr)
        return 1
    bound_repo = os.environ.get("GH_REPO", "").strip()
    if bound_repo and not re.fullmatch(r"[^/\s]+/[^/\s]+", bound_repo):
        print("reviewer-claim-guard: refused: exact repository binding is malformed", file=sys.stderr)
        return 1
    pr, endpoint_repo = _target(argv, bound_repo or None)
    if pr is None:
        print("reviewer-claim-guard: refused: verdict target is not a pull request", file=sys.stderr)
        return 1
    if bound_repo and endpoint_repo and bound_repo.lower() != endpoint_repo.lower():
        print("reviewer-claim-guard: refused: endpoint repository disagrees with GH_REPO", file=sys.stderr)
        return 1
    repo = bound_repo or endpoint_repo
    if not repo:
        print("reviewer-claim-guard: refused: exact repository binding is missing", file=sys.stderr)
        return 1
    try:
        live_head, base = _live_pull(repo, pr)
        guard = _guard_module()
        policy = guard.review_policy(guard.base_config_text(repo, base), repo, True)
        if policy.get("claim_receipt") is None:
            return 0
        marker_head = verdicts[0].lower() if verdicts else live_head
        if marker_head != live_head:
            raise RuntimeError(f"verdict names {marker_head}, live head is {live_head}")
        reviewer = os.environ.get("SHIPYARD_AGENT_ID", "").strip()
        if not reviewer or any(char.isspace() for char in reviewer):
            raise RuntimeError("SHIPYARD_AGENT_ID is required when reviewer claims are enabled")
        shipyard = os.environ.get("SHIPYARD_GHAPP_SHIPYARD_BINARY", "").strip()
        if not shipyard or not os.path.isfile(shipyard) or not os.access(shipyard, os.X_OK):
            raise RuntimeError("release-matched Shipyard binary is unavailable")
        command = [shipyard, "--json", "reviewer-claim", "validate", "--repo", repo,
                   "--pr", str(pr), "--head", live_head, "--reviewer", reviewer]
        result = subprocess.run(command, check=False, capture_output=True, text=True,
                                timeout=30, env=os.environ.copy())
        if result.returncode != 0:
            detail = (result.stderr.strip() or result.stdout.strip())[-400:]
            raise RuntimeError(detail or "reviewer claim is not current")
    except Exception as error:  # noqa: BLE001 - a verdict must fail closed
        print(f"reviewer-claim-guard: refused: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
