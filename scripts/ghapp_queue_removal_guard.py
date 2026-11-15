#!/usr/bin/env python3
"""Refuse unaudited merge-queue removal through a ghapp wrapper."""

from __future__ import annotations

import json
import os
import pathlib
import posixpath
import re
import socket
import subprocess
import sys
from datetime import datetime, timezone
from typing import Any
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
            try:
                port = parts.port
            except ValueError as error:
                raise GuardError("cannot inspect absolute API endpoint") from error
            if (
                parts.scheme.lower() != "https"
                or parts.hostname is None
                or parts.hostname.lower() != "api.github.com"
                or port not in (None, 443)
                or parts.username is not None
                or parts.password is not None
            ):
                raise GuardError("cannot inspect absolute API endpoint")
        if re.search(r"%(?![0-9A-Fa-f]{2})", parts.path):
            raise GuardError("cannot inspect malformed encoded API endpoint")
        path = posixpath.normpath(unquote(parts.path)).lstrip("/")
        if "{" in path or "}" in path:
            owner, repo = current_repo_identity()
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


def current_repo_identity() -> tuple[str, str]:
    repo_parts = os.environ.get("GH_REPO", "").split("/")
    if len(repo_parts) >= 2 and repo_parts[-2] and repo_parts[-1]:
        return repo_parts[-2], repo_parts[-1]
    real_gh = os.environ.get("GHAPP_REAL_GH", "/opt/homebrew/bin/gh")
    try:
        output = subprocess.run(
            [real_gh, "repo", "view", "--json", "nameWithOwner"],
            check=True,
            capture_output=True,
            text=True,
            timeout=30,
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        raise GuardError(f"cannot resolve current repository identity: {error}") from error
    try:
        repo = json.loads(output).get("nameWithOwner")
    except (json.JSONDecodeError, AttributeError) as error:
        raise GuardError("cannot resolve current repository identity") from error
    parts = repo.split("/") if isinstance(repo, str) else []
    if len(parts) != 2 or not parts[0] or not parts[1]:
        raise GuardError("cannot resolve current repository identity")
    return parts[0], parts[1]


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


# ---------------------------------------------------------------------------
# Who is asking: the internal marker is Shipyard's, and only for the one call
# Shipyard built it for.
# ---------------------------------------------------------------------------

# Shipyard puts a fresh random nonce in the environment of exactly the `gh`
# (or guard) child it builds for one of its own queue mutations, and writes a
# record for it: a 0600 file, in a 0700 directory it owns, named by the
# SHA-256 of the nonce and the purpose it is for. A guard honours a marker only
# when it can consume that record: it opens the file without following links,
# checks it is a regular file owned by this user, readable by no one else,
# younger than NONCE_MAX_AGE_SECONDS and naming the purpose, then unlinks it.
# Unlinking is the claim, so each record is honoured once. A marker without a
# nonce, with a wrong or reused one, or with a record that fails any check is
# ignored and the request is judged in full. This ties the marker to a call
# Shipyard made rather than to whoever exported it; it is not a boundary
# against a process of the same user that forges Shipyard's own state.
NONCE_ENV = "SHIPYARD_QUEUE_MUTATION_NONCE"
NONCE_DIR_ENV = "SHIPYARD_QUEUE_MUTATION_NONCE_DIR"
NONCE_MAX_AGE_SECONDS = 600
# The purposes Shipyard issues records for; one record serves one guard once.
NONCE_INTERNAL_REMOVAL = "internal-removal"
NONCE_INTERNAL_ARM = "internal-arm"
NONCE_REVIEW_ONLY = "review-only"


def nonce_record_name(nonce: str, purpose: str) -> str:
    """The record's file name: what Shipyard writes and the guard looks for."""
    import hashlib

    return hashlib.sha256(f"{nonce}:{purpose}".encode("utf-8")).hexdigest()


def consume_nonce(purpose: str) -> bool:
    """Whether this process carries Shipyard's one-shot nonce for ``purpose``,
    consuming its record. Never raises."""
    import stat
    import time

    nonce = os.environ.get(NONCE_ENV, "")
    directory = os.environ.get(NONCE_DIR_ENV, "")
    if not re.fullmatch(r"[0-9a-f]{64}", nonce) or not directory:
        return False
    try:
        uid = os.getuid()
        info = os.lstat(directory)
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid != uid
                or info.st_mode & 0o077):
            return False
        path = os.path.join(directory, nonce_record_name(nonce, purpose))
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        try:
            record = os.fstat(descriptor)
            if (not stat.S_ISREG(record.st_mode) or record.st_uid != uid
                    or record.st_mode & 0o077
                    or time.time() - record.st_mtime > NONCE_MAX_AGE_SECONDS):
                return False
            content = os.read(descriptor, 256).decode("utf-8", "replace").strip()
        finally:
            os.close(descriptor)
        if content != purpose:
            return False
        os.unlink(path)
    except (OSError, AttributeError, ValueError):
        return False
    return True


# ---------------------------------------------------------------------------
# What is being removed, and is its queue entry healthy?
# ---------------------------------------------------------------------------

HEALTHY_REFUSAL = (
    "A queued PR does not need a rebase; the queue merges it on top of current main."
)
REORDER_REFUSAL = (
    "Dequeuing other PRs to reorder the queue discards their merge-group runs; "
    "enqueue the PR that must land first with the queue's jump option instead. "
    "A PR that must land alone gets a solo reservation (`shipyard merge-queue solo`), "
    "under which the `solo` reason may remove the entries ahead of it."
)
PR_MERGE_VALUE_OPTIONS = {
    "-A", "--author-email", "-b", "--body", "-F", "--body-file", "--match-head-commit",
    "-R", "--repo", "-t", "--subject",
}
REASON_ENV = "GHAPP_QUEUE_REMOVAL_REASON"
NOTE_ENV = "GHAPP_QUEUE_REMOVAL_NOTE"
FIX_PR_ENV = "GHAPP_QUEUE_REMOVAL_FIX_PR"
LOG_ENV = "GHAPP_QUEUE_REMOVAL_LOG"
# Reasons the override accepts. Anything else, including no reason, refuses.
ALLOWED_REASONS = ("defect-fix", "reorder-main-red-fix", "solo")
REFUSED_REASONS = {"rebase": HEALTHY_REFUSAL, "reorder": REORDER_REFUSAL}

_TARGET_FIELDS = "number headRefOid isInMergeQueue mergeQueueEntry{state position}"
ENTRY_BY_NUMBER_QUERY = (
    "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name)"
    "{nameWithOwner pullRequest(number:$number){" + _TARGET_FIELDS + "}}}"
)
ENTRY_BY_ID_QUERY = (
    "query($id:ID!){node(id:$id){... on PullRequest{"
    + _TARGET_FIELDS
    + " repository{nameWithOwner}}}}"
)


def run_real_gh(arguments: list[str]) -> Any:
    real_gh = os.environ.get("GHAPP_REAL_GH", "/opt/homebrew/bin/gh")
    try:
        completed = subprocess.run(
            [real_gh, *arguments], check=False, capture_output=True, text=True, timeout=30
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise GuardError(f"cannot read live PR state: {error}") from error
    if completed.returncode != 0:
        detail = completed.stderr.strip().splitlines()
        raise GuardError(
            "cannot read live PR state: " + (detail[-1] if detail else f"exit {completed.returncode}")
        )
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise GuardError(f"live PR state was not JSON: {error}") from error


def _graphql(query: str, variables: dict[str, str]) -> Any:
    arguments = ["api", "graphql", "-f", f"query={query}"]
    for key, value in variables.items():
        arguments += ["-F" if key == "number" else "-f", f"{key}={value}"]
    return run_real_gh(arguments)


def _split_repo(repo: str) -> tuple[str, str]:
    parts = repo.strip().split("/")
    if len(parts) >= 2 and parts[-2] and parts[-1]:
        return parts[-2], parts[-1]
    raise GuardError(f"cannot resolve repository `{repo}`")


def _merge_target(args: list[str]) -> dict[str, str]:
    selector = None
    skip = False
    for arg in args[2:]:
        if skip:
            skip = False
            continue
        if arg in PR_MERGE_VALUE_OPTIONS:
            skip = True
            continue
        if arg.startswith("-"):
            continue
        if selector is None:
            selector = arg
    if selector:
        match = re.search(r"github\.com/([^/]+)/([^/]+)/pull/([1-9][0-9]*)", selector)
        if match:
            return {"owner": match.group(1), "name": match.group(2), "number": match.group(3)}
    repos = option_values(args[2:], {"-R", "--repo"})
    repo = repos[-1] if repos else os.environ.get("GH_REPO", "")
    owner, name = _split_repo(repo) if repo else current_repo_identity()
    if selector and re.fullmatch(r"#?[1-9][0-9]*", selector):
        return {"owner": owner, "name": name, "number": selector.lstrip("#")}
    view = ["pr", "view", "--repo", f"{owner}/{name}", "--json", "number"]
    if selector:
        view.insert(2, selector)
    value = run_real_gh(view)
    number = value.get("number") if isinstance(value, dict) else None
    if not isinstance(number, int):
        raise GuardError(f"cannot resolve the pull request for `{selector or 'current branch'}`")
    return {"owner": owner, "name": name, "number": str(number)}


def _graphql_variables(args: list[str]) -> dict[str, Any]:
    variables: dict[str, Any] = {}
    for value in option_values(args, {"-f", "-F", "--field", "--raw-field"}):
        key, separator, raw = value.partition("=")
        if not separator or key == "query":
            continue
        if raw == "@-":
            raise GuardError("cannot inspect a GraphQL variable read from stdin")
        if raw.startswith("@"):
            try:
                raw = pathlib.Path(raw[1:]).read_text(encoding="utf-8").strip()
            except OSError as error:
                raise GuardError(f"cannot inspect GraphQL variable file: {error}") from error
        variables[key] = raw
    for path in option_values(args, {"--input"}):
        try:
            body = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise GuardError(f"cannot inspect GraphQL input: {error}") from error
        if isinstance(body, dict) and isinstance(body.get("variables"), dict):
            variables.update(body["variables"])
    return variables


def _removal_ids(document: str, variables: dict[str, Any]) -> list[str]:
    ids: list[str] = []
    mutation = re.compile(r"(dequeuePullRequest|disablePullRequestAutoMerge)\s*\(", re.IGNORECASE)
    for match in mutation.finditer(document):
        tail = document[match.end():]
        literal = re.match(
            r"\s*input\s*:\s*\{[^}]*?\b(?:pullRequestId|id)\s*:\s*"
            r"(\$[A-Za-z_][A-Za-z0-9_]*|\"([^\"]+)\")",
            tail,
        )
        if literal is not None:
            if literal.group(2) is not None:
                ids.append(literal.group(2))
                continue
            value = variables.get(literal.group(1)[1:])
        else:
            whole = re.match(r"\s*input\s*:\s*\$([A-Za-z_][A-Za-z0-9_]*)", tail)
            if whole is None:
                raise GuardError("cannot find the pull request a removal mutation targets")
            value = variables.get(whole.group(1))
            if isinstance(value, str):
                try:
                    value = json.loads(value)
                except json.JSONDecodeError:
                    value = None
            if isinstance(value, dict):
                value = value.get("pullRequestId") or value.get("id")
        if not isinstance(value, str) or not value:
            raise GuardError("cannot resolve the pull request a removal mutation targets")
        ids.append(value)
    if not ids:
        raise GuardError("removal mutation found but no pull request id could be read")
    return ids


def removal_targets(args: list[str]) -> list[dict[str, str]]:
    """Every pull request a removal request in ``args`` targets."""
    if len(args) >= 2 and args[0] == "pr" and args[1] == "merge":
        return [_merge_target(args)]
    endpoint, query = api_target(args)
    document = "\n".join([graphql_document(args), *query.get("query", [])])
    return [{"id": node} for node in _removal_ids(document, _graphql_variables(args))]


def _pull_request(response: Any) -> dict[str, Any] | None:
    if not isinstance(response, dict) or response.get("errors"):
        return None
    data = response.get("data")
    if not isinstance(data, dict):
        return None
    if isinstance(data.get("node"), dict):
        return data["node"]
    repository = data.get("repository")
    if isinstance(repository, dict) and isinstance(repository.get("pullRequest"), dict):
        return repository["pullRequest"]
    return None


def read_target(target: dict[str, str]) -> dict[str, Any]:
    """The live PR a removal targets: number, head, repository, queue entry."""
    if "id" in target:
        response = _graphql(ENTRY_BY_ID_QUERY, {"id": target["id"]})
    else:
        response = _graphql(ENTRY_BY_NUMBER_QUERY, target)
    pr = _pull_request(response)
    if pr is None or not isinstance(pr.get("number"), int):
        raise GuardError("the pull request a removal targets could not be read")
    repository = pr.get("repository") if isinstance(pr.get("repository"), dict) else None
    repo = (repository or {}).get("nameWithOwner")
    if not repo and "owner" in target:
        repo = f"{target['owner']}/{target['name']}"
    entry = pr.get("mergeQueueEntry") if isinstance(pr.get("mergeQueueEntry"), dict) else None
    return {
        "repo": repo,
        "pr": pr["number"],
        "head": pr.get("headRefOid"),
        "queue_state": entry.get("state") if entry else None,
    }


# ---------------------------------------------------------------------------
# Solo reservations: `shipyard merge-queue solo` (src/merge_queue_solo.rs)
# reserves a repository's queue for one exact-head-approved PR. While it is
# live, a `solo` removal may take out entries AHEAD of that PR in the same
# queue, and nothing else. The reservation file is read with the same checks
# Shipyard writes it under (0600, owned by this user, never through a link);
# a file that fails any check, has expired, or names a PR that is not queued
# at its reserved head grants nothing.
# ---------------------------------------------------------------------------

SOLO_DIR_ENV = "SHIPYARD_SOLO_RESERVATION_DIR"
SOLO_SCHEMA = 1
SOLO_KIND = "shipyard.merge-queue-solo"
# src/paths.rs: the state root per platform; src/merge_queue_solo.rs SOLO_DIR.
SOLO_STATE_STEM = "shipyard"
SOLO_SUBDIR = ("merge_queue", "solo")

SOLO_ENTRY_QUERY = (
    "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name)"
    "{nameWithOwner pullRequest(number:$number){number state headRefOid baseRefName "
    "isInMergeQueue mergeQueueEntry{state position}}}}"
)
SOLO_ENTRY_BY_ID_QUERY = (
    "query($id:ID!){node(id:$id){... on PullRequest{number state headRefOid baseRefName "
    "isInMergeQueue mergeQueueEntry{state position} repository{nameWithOwner}}}}"
)


def solo_reservation_dir() -> pathlib.Path:
    configured = os.environ.get(SOLO_DIR_ENV)
    if configured:
        return pathlib.Path(configured)
    home = pathlib.Path.home()
    if sys.platform == "darwin":
        root = home / "Library" / "Application Support" / SOLO_STATE_STEM
    else:
        root = home / ".local" / "state" / SOLO_STATE_STEM
    return root.joinpath(*SOLO_SUBDIR)


def _solo_key(repo: str) -> str:
    parts = repo.strip().split("/")
    allowed = re.compile(r"[A-Za-z0-9._-]+")
    if (len(parts) != 2 or not all(allowed.fullmatch(part) for part in parts)
            or any(part in (".", "..") for part in parts)):
        raise GuardError(f"`{repo}` cannot name a solo reservation")
    return f"{parts[0].lower()}__{parts[1].lower()}"


def _parse_time(value: Any) -> datetime:
    if not isinstance(value, str):
        raise GuardError("the solo reservation has no valid time")
    # Rust writes up to nanoseconds; Python reads up to microseconds.
    text = re.sub(r"(\.\d{6})\d+", r"\1", value.strip()).replace("Z", "+00:00")
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError as error:
        raise GuardError(f"the solo reservation has no valid time: {error}") from error
    if parsed.tzinfo is None:
        raise GuardError("the solo reservation's time has no zone")
    return parsed


def read_solo_reservation(repo: str, now: datetime) -> dict[str, Any]:
    """The live reservation for ``repo``; raises GuardError for anything else."""
    import stat

    path = solo_reservation_dir() / f"{_solo_key(repo)}.json"
    try:
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    except FileNotFoundError as error:
        raise GuardError(f"{repo} has no solo reservation") from error
    except OSError as error:
        raise GuardError(f"the solo reservation for {repo} is unreadable: {error}") from error
    try:
        info = os.fstat(descriptor)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_mode & 0o077):
            raise GuardError(f"the solo reservation for {repo} is not a private regular file")
        with os.fdopen(descriptor, "r", encoding="utf-8") as handle:
            descriptor = -1
            record = json.load(handle)
    except (OSError, ValueError) as error:
        raise GuardError(f"the solo reservation for {repo} is unreadable: {error}") from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    if not isinstance(record, dict) or record.get("schema") != SOLO_SCHEMA \
            or record.get("kind") != SOLO_KIND:
        raise GuardError(f"the solo reservation for {repo} is not a reservation record")
    if str(record.get("repo", "")).lower() != repo.lower():
        raise GuardError(f"the solo reservation filed for {repo} names {record.get('repo')}")
    pr, head, base = record.get("pr"), record.get("head"), record.get("base")
    if (not isinstance(pr, int) or isinstance(pr, bool) or pr <= 0
            or not isinstance(head, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", head)
            or not isinstance(base, str) or not base):
        raise GuardError(f"the solo reservation for {repo} does not name a pull request head")
    if now >= _parse_time(record.get("expires_at")):
        raise GuardError(f"the solo reservation for {repo}#{pr} has expired")
    return record


def _solo_entry(response: Any) -> dict[str, Any]:
    pr = _pull_request(response)
    if pr is None or not isinstance(pr.get("number"), int):
        raise GuardError("a pull request in the solo removal could not be read")
    repository = pr.get("repository") if isinstance(pr.get("repository"), dict) else None
    repo = (repository or {}).get("nameWithOwner")
    if not repo and isinstance(response, dict):
        repo = ((response.get("data") or {}).get("repository") or {}).get("nameWithOwner")
    entry = pr.get("mergeQueueEntry") if isinstance(pr.get("mergeQueueEntry"), dict) else None
    position = entry.get("position") if entry else None
    return {
        "repo": repo,
        "pr": pr["number"],
        "head": pr.get("headRefOid"),
        "base": pr.get("baseRefName"),
        "state": pr.get("state"),
        "position": position if isinstance(position, int) and not isinstance(position, bool) else None,
    }


def judge_solo(args: list[str], now: datetime | None = None) -> tuple[bool, str, dict[str, Any]]:
    """Whether a `solo` removal takes out only entries ahead of a reserved PR."""
    now = now or datetime.now(timezone.utc)
    entries = []
    for target in removal_targets(args):
        if "id" in target:
            response = _graphql(SOLO_ENTRY_BY_ID_QUERY, {"id": target["id"]})
        else:
            response = _graphql(SOLO_ENTRY_QUERY, target)
        entries.append(_solo_entry(response))
    repos = {str(entry["repo"] or "").lower() for entry in entries}
    if len(repos) != 1 or "" in repos:
        return False, "a solo removal must target entries of one repository's queue", {}
    repo = next(entry["repo"] for entry in entries)
    reservation = read_solo_reservation(repo, now)
    owner, name = _split_repo(repo)
    reserved = _solo_entry(_graphql(
        SOLO_ENTRY_QUERY, {"owner": owner, "name": name, "number": str(reservation["pr"])}))
    if (reserved["state"] != "OPEN" or reserved["position"] is None
            or str(reserved["head"]).lower() != reservation["head"].lower()
            or reserved["base"] != reservation["base"]):
        return False, (
            f"{repo}#{reservation['pr']} is not queued at its reserved head "
            f"{reservation['head'][:12]} on {reservation['base']}, so no entry is ahead of it"
        ), {}
    for entry in entries:
        if entry["pr"] == reservation["pr"]:
            return False, f"a solo removal never removes the reserved {repo}#{entry['pr']}", {}
        if entry["position"] is None or entry["base"] != reservation["base"]:
            return False, f"{repo}#{entry['pr']} is not in the reserved {reservation['base']} queue", {}
        if entry["position"] >= reserved["position"]:
            return False, (
                f"{repo}#{entry['pr']} is at position {entry['position']}, behind the reserved "
                f"#{reservation['pr']} at {reserved['position']}; a solo removal takes out only "
                "entries ahead of it. " + REORDER_REFUSAL
            ), {}
    return True, f"ahead of the reserved {repo}#{reservation['pr']}", {
        "reserved_pr": reservation["pr"],
        "reserved_head": reservation["head"],
        "reserved_position": reserved["position"],
        "reservation_reason": reservation.get("reason"),
        "expires_at": reservation.get("expires_at"),
        "positions": {str(entry["pr"]): entry["position"] for entry in entries},
    }


def removal_log_path() -> pathlib.Path:
    configured = os.environ.get(LOG_ENV)
    if configured:
        return pathlib.Path(configured)
    state = os.environ.get("XDG_STATE_HOME") or str(pathlib.Path.home() / ".local" / "state")
    return pathlib.Path(state) / "shipyard" / "queue-removals.jsonl"


def append_removal_log(record: dict[str, Any]) -> pathlib.Path:
    path = removal_log_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as log:
        log.write(json.dumps(record, sort_keys=True) + "\n")
        log.flush()
        os.fsync(log.fileno())
    return path


def judge_override(args: list[str]) -> tuple[bool, str]:
    """``(allowed, message)`` for a removal made under the operator override.

    The override must state why. A rebase or a reorder is refused whatever the
    queue looks like; a defect fix is allowed and logged; a main-red fix may
    remove only the fix PR itself, so it can jump the queue; a solo removal
    may remove only entries ahead of the PR a live solo reservation names
    (``judge_solo``).
    """
    reason = os.environ.get(REASON_ENV, "").strip().lower()
    note = os.environ.get(NOTE_ENV, "").strip()
    if reason in REFUSED_REASONS:
        return False, f"the stated reason is `{reason}`. {REFUSED_REASONS[reason]}"
    if reason not in ALLOWED_REASONS:
        stated = f"`{reason}` is not a reason class" if reason else "no reason was stated"
        return False, (
            f"{stated}; the override needs {REASON_ENV} set to one of "
            + ", ".join(sorted([*ALLOWED_REASONS, *REFUSED_REASONS]))
            + f" and {NOTE_ENV} saying what is wrong"
        )
    if not note:
        return False, f"a `{reason}` removal needs {NOTE_ENV} saying what is wrong"
    solo: dict[str, Any] = {}
    if reason == "solo":
        allowed, detail, solo = judge_solo(args)
        if not allowed:
            return False, detail
    targets = [read_target(target) for target in removal_targets(args)]
    if reason == "reorder-main-red-fix":
        fix_pr = os.environ.get(FIX_PR_ENV, "").strip().lstrip("#")
        others = [str(target["pr"]) for target in targets if str(target["pr"]) != fix_pr]
        if not fix_pr or others:
            return False, (
                "a reorder-main-red-fix removal may remove only the PR that fixes main "
                f"({FIX_PR_ENV} names it), never another PR in the queue"
                + (f"; this request targets #{', #'.join(others)}" if others else "")
                + ". "
                + REORDER_REFUSAL
            )
    try:
        path = append_removal_log(
            {
                "at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
                # Who removed it, for `shipyard who`; a missing session never refuses.
                "host": socket.gethostname().split(".")[0],
                "session": next((os.environ[name].strip() for name in
                                 ("CLAUDE_CODE_SESSION_ID", "CODEX_COMPANION_SESSION_ID")
                                 if os.environ.get(name, "").strip()), "unattributed"),
                **({"agent": os.environ["SHIPYARD_AGENT_ID"].strip()}
                   if os.environ.get("SHIPYARD_AGENT_ID", "").strip() else {}),
                "reason": reason,
                "note": note,
                "fix_pr": os.environ.get(FIX_PR_ENV) or None,
                **({"solo": solo} if solo else {}),
                "targets": targets,
                "argv": args,
            }
        )
    except OSError as error:
        return False, f"cannot record the removal, so it is not allowed: {error}"
    named = ", ".join(f"{target['repo']}#{target['pr']}" for target in targets)
    return True, f"`{reason}` removal of {named} recorded in {path}: {note}"


def main(args: list[str]) -> int:
    marker = os.environ.get("SHIPYARD_INTERNAL_QUEUE_MUTATION") == "1"
    override = os.environ.get("GHAPP_ALLOW_QUEUE_REMOVAL") == "1"
    try:
        if not is_queue_removal(args):
            return 0
    except GuardError as error:
        if marker and consume_nonce(NONCE_INTERNAL_REMOVAL):
            return 0
        if override:
            print(
                "queue-removal-guard: refusing an ambiguous request despite "
                f"GHAPP_ALLOW_QUEUE_REMOVAL=1: {error}. The override is honoured only "
                "when the guard can read the PR it targets; pass the query "
                "inline or as a file.",
                file=sys.stderr,
            )
            return 1
        print(f"queue-removal-guard: refusing ambiguous API request: {error}", file=sys.stderr)
        return 1
    if marker:
        if consume_nonce(NONCE_INTERNAL_REMOVAL):
            return 0
        print(
            "queue-removal-guard: ignoring SHIPYARD_INTERNAL_QUEUE_MUTATION=1: it carries "
            "no valid one-shot nonce from a call Shipyard made, so it cannot claim "
            "Shipyard's authority.",
            file=sys.stderr,
        )
    if override:
        try:
            allowed, message = judge_override(args)
        except GuardError as error:
            allowed, message = False, f"cannot read the PR it targets: {error}"
        if allowed:
            print(f"queue-removal-guard: WARNING: override permits a {message}", file=sys.stderr)
            return 0
        print(
            f"queue-removal-guard: refusing despite GHAPP_ALLOW_QUEUE_REMOVAL=1: {message}",
            file=sys.stderr,
        )
        return 1
    print(
        "queue-removal-guard: refusing unaudited merge-queue removal. "
        + HEALTHY_REFUSAL
        + " Use Shipyard's exact-head queue path; operators see docs/ghapp-guards.md.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
