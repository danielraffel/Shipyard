#!/usr/bin/env python3
"""Refuse unaudited merge-queue removal through a ghapp wrapper."""

from __future__ import annotations

import os
import pathlib
import sys


REMOVAL_MUTATIONS = ("dequeuepullrequest", "disablepullrequestautomerge")


def query_value(value: str) -> str:
    query = value.removeprefix("query=")
    if query.startswith("@"):
        try:
            return pathlib.Path(query[1:]).read_text(encoding="utf-8")
        except OSError:
            return ""
    return query


def graphql_document(args: list[str]) -> str:
    for index, arg in enumerate(args):
        if arg in {"-f", "-F", "--field", "--raw-field"} and index + 1 < len(args):
            value = args[index + 1]
            if value.startswith("query="):
                return query_value(value)
        for prefix in ("query=", "-fquery=", "-Fquery=", "--field=query=", "--raw-field=query="):
            if arg.startswith(prefix):
                return query_value(arg.split("query=", 1)[1])
    for index, arg in enumerate(args):
        if arg == "--input" and index + 1 < len(args):
            path = args[index + 1]
            if path != "-":
                try:
                    return pathlib.Path(path).read_text(encoding="utf-8")
                except OSError:
                    return ""
    return ""


def is_queue_removal(args: list[str]) -> bool:
    if len(args) >= 2 and args[0] == "pr" and args[1] == "merge":
        return "--disable-auto" in args
    if len(args) >= 2 and args[0] == "api" and args[1] == "graphql":
        compact = "".join(graphql_document(args).split()).lower()
        return any(name in compact for name in REMOVAL_MUTATIONS)
    return False


def main(args: list[str]) -> int:
    if not is_queue_removal(args):
        return 0
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
