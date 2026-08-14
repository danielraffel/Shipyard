#!/usr/bin/env python3
"""Refuse unaudited merge-queue removal through a ghapp wrapper."""

from __future__ import annotations

import json
import os
import pathlib
import posixpath
import re
import subprocess
import sys
from urllib.parse import parse_qs, unquote, urlsplit


REMOVAL_MUTATIONS = ("dequeuepullrequest", "disablepullrequestautomerge")


class GuardError(RuntimeError):
    """The wrapper cannot prove a raw API request is harmless."""


def option_values(args: list[str], names: set[str]) -> list[str]:
    values: list[str] = []
    for index, arg in enumerate(args):
        for name in names:
            if arg == name and index + 1 < len(args):
                values.append(args[index + 1])
                break
            if name.startswith("--") and arg.startswith(f"{name}="):
                values.append(arg.removeprefix(f"{name}="))
                break
            if len(name) == 2 and arg.startswith(name) and len(arg) > 2:
                values.append(arg[len(name) :].removeprefix("="))
                break
    return values


def query_value(value: str) -> str:
    query = value.removeprefix("query=")
    if query == "@-":
        raise GuardError("cannot inspect a GraphQL body read from stdin")
    if query.startswith("@"):
        try:
            return pathlib.Path(query[1:]).read_text(encoding="utf-8")
        except OSError as error:
            raise GuardError(f"cannot inspect GraphQL query file: {error}") from error
    return query


def graphql_document(args: list[str]) -> str:
    documents = [
        query_value(value)
        for value in option_values(args, {"-f", "-F", "--field", "--raw-field"})
        if value.startswith("query=")
    ]
    inputs = option_values(args, {"--input"})
    for path in inputs:
        if path == "-":
            raise GuardError("cannot inspect a GraphQL body read from stdin")
        try:
            body = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
        except OSError as error:
            raise GuardError(f"cannot inspect GraphQL input file: {error}") from error
        except json.JSONDecodeError as error:
            raise GuardError(f"cannot inspect malformed GraphQL input: {error}") from error
        if not isinstance(body, dict) or not isinstance(body.get("query"), str):
            raise GuardError("GraphQL input must be a JSON object with a string query")
        documents.append(body["query"])
    return "\n".join(documents)


def api_target(args: list[str]) -> tuple[str, dict[str, list[str]]]:
    value_options = {
        "--cache", "-F", "--field", "-H", "--header", "--hostname", "--input",
        "-q", "--jq", "-X", "--method", "-p", "--preview", "-f", "--raw-field",
        "-t", "--template",
    }
    skip_next = False
    for arg in args[1:]:
        if skip_next:
            skip_next = False
            continue
        if arg in value_options:
            skip_next = True
            continue
        if any(
            (name.startswith("--") and arg.startswith(f"{name}="))
            or (len(name) == 2 and arg.startswith(name) and len(arg) > 2)
            for name in value_options
        ):
            continue
        if arg.startswith("-"):
            continue
        parts = urlsplit(arg)
        if parts.scheme or parts.netloc:
            raise GuardError("cannot inspect absolute API endpoint")
        if re.search(r"%(?![0-9A-Fa-f]{2})", parts.path):
            raise GuardError("cannot inspect malformed encoded API endpoint")
        path = posixpath.normpath(unquote(parts.path)).lstrip("/")
        if "{" in path or "}" in path:
            repo_parts = os.environ.get("GH_REPO", "").split("/")
            owner, repo = (repo_parts[-2], repo_parts[-1]) if len(repo_parts) >= 2 else ("", "")
            branch = ""
            if "{branch}" in path:
                try:
                    branch = subprocess.run(
                        ["git", "branch", "--show-current"],
                        check=True,
                        capture_output=True,
                        text=True,
                    ).stdout.strip()
                except (OSError, subprocess.CalledProcessError) as error:
                    raise GuardError(f"cannot resolve API endpoint branch placeholder: {error}") from error
            values = {"{owner}": owner, "{repo}": repo, "{branch}": branch}
            for placeholder, value in values.items():
                if placeholder in path:
                    if not value:
                        raise GuardError(f"cannot resolve API endpoint placeholder {placeholder}")
                    path = path.replace(placeholder, value)
            if "{" in path or "}" in path:
                raise GuardError("cannot resolve unknown API endpoint placeholder")
        return path, parse_qs(parts.query, keep_blank_values=True)
    return "", {}


def is_queue_removal(args: list[str]) -> bool:
    if len(args) >= 2 and args[0] == "pr" and args[1] == "merge":
        return "--disable-auto" in args
    if args and args[0] == "api":
        endpoint, query = api_target(args)
        if endpoint != "graphql":
            return False
        document = "\n".join([graphql_document(args), *query.get("query", [])])
        compact = "".join(document.split()).lower()
        contains_removal = any(name in compact for name in REMOVAL_MUTATIONS)
        return contains_removal
    return False


def main(args: list[str]) -> int:
    try:
        if not is_queue_removal(args):
            return 0
    except GuardError as error:
        if os.environ.get("SHIPYARD_INTERNAL_QUEUE_MUTATION") == "1":
            return 0
        if os.environ.get("GHAPP_ALLOW_QUEUE_REMOVAL") == "1":
            print(
                "queue-removal-guard: WARNING: allowing ambiguous API request via explicit override",
                file=sys.stderr,
            )
            return 0
        print(f"queue-removal-guard: refusing ambiguous API request: {error}", file=sys.stderr)
        return 1
    if os.environ.get("SHIPYARD_INTERNAL_QUEUE_MUTATION") == "1":
        return 0
    if os.environ.get("GHAPP_ALLOW_QUEUE_REMOVAL") == "1":
        print(
            "queue-removal-guard: WARNING: explicit override permits an unaudited "
            "merge-queue removal",
            file=sys.stderr,
        )
        return 0
    print(
        "queue-removal-guard: refusing unaudited merge-queue removal. "
        "Use Shipyard's exact-head queue path, or set "
        "GHAPP_ALLOW_QUEUE_REMOVAL=1 for an explicit authority action.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
