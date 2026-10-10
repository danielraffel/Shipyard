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
# Who is asking: the internal marker is Shipyard's, and only Shipyard's.
# ---------------------------------------------------------------------------

# Processes that sit between Shipyard and this guard without deciding anything:
# the ghapp wrapper's shell, a `gh` shim that execs it, `env`, `timeout`.
# Python is not among them: no Shipyard path reaches ghapp through a Python
# helper, and a Python-driven agent is a deciding process like any other.
WRAPPER_PROCESSES = {"bash", "sh", "zsh", "dash", "env", "timeout", "gh", "ghapp"}


def _process_name_and_parent(pid: int) -> tuple[str, int] | None:
    try:
        output = subprocess.run(
            ["/bin/ps", "-o", "ppid=", "-o", "comm=", "-p", str(pid)],
            check=True,
            capture_output=True,
            text=True,
            timeout=10,
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
        return None
    parent, _, command = output.partition(" ")
    command = command.strip()
    try:
        return command, int(parent.strip())
    except ValueError:
        return None


def executable_path(pid: int) -> str | None:
    """The real executable of ``pid``, from the kernel rather than its name."""
    if sys.platform.startswith("linux"):
        try:
            return os.readlink(f"/proc/{pid}/exe")
        except OSError:
            return None
    if sys.platform == "darwin":
        try:
            import ctypes

            libproc = ctypes.CDLL("/usr/lib/libproc.dylib")
            buffer = ctypes.create_string_buffer(4096)
            length = libproc.proc_pidpath(ctypes.c_int(pid), buffer, ctypes.c_uint32(4096))
        except (OSError, AttributeError):
            return None
        return buffer.value.decode("utf-8", "replace") if length > 0 else None
    return None


def process_ancestry(limit: int = 8) -> list[tuple[str, str | None]]:
    """``(basename, executable path)`` from this guard's parent upward, nearest first."""
    entries: list[tuple[str, str | None]] = []
    pid = os.getppid()
    while pid > 1 and len(entries) < limit:
        found = _process_name_and_parent(pid)
        if found is None:
            break
        command, parent = found
        entries.append((posixpath.basename(command).lstrip("-"), executable_path(pid)))
        pid = parent
    return entries


def install_roots() -> list[pathlib.Path]:
    """Where an installed Shipyard binary lives: one auth generation per release."""
    return [pathlib.Path.home() / ".local" / "share" / "shipyard" / "auth-generations"]


def is_installed_shipyard(path: str | None, roots: list[pathlib.Path]) -> bool:
    """Whether ``path`` resolves to a ``shipyard`` binary under an install root."""
    if not path:
        return False
    real = pathlib.Path(os.path.realpath(path))
    if real.name != "shipyard":
        return False
    for root in roots:
        resolved = pathlib.Path(os.path.realpath(root))
        if resolved == real.parent or resolved in real.parents:
            return True
    return False


def shipyard_parent(
    ancestry: list[tuple[str, str | None]] | None = None,
    roots: list[pathlib.Path] | None = None,
) -> bool:
    """True when the nearest deciding ancestor is the installed Shipyard binary.

    Shells and the ghapp wrapper are skipped; the first other process decides.
    An agent that exports the marker from its own shell has that agent
    (codex, node, claude, python) as the nearest deciding ancestor, even when
    the agent itself was launched by a Shipyard process. The deciding process
    must also run an executable that resolves under a Shipyard install root,
    read from the kernel, so a binary merely named ``shipyard`` does not pass.
    """
    roots = install_roots() if roots is None else roots
    for name, path in process_ancestry() if ancestry is None else ancestry:
        if name in WRAPPER_PROCESSES:
            continue
        return name == "shipyard" and is_installed_shipyard(path, roots)
    return False


# ---------------------------------------------------------------------------
# What is being removed, and is its queue entry healthy?
# ---------------------------------------------------------------------------

HEALTHY_REFUSAL = (
    "A queued PR does not need a rebase; the queue merges it on top of current main."
)
REORDER_REFUSAL = (
    "Dequeuing other PRs to reorder the queue discards their merge-group runs; "
    "enqueue the PR that must land first with the queue's jump option instead."
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
ALLOWED_REASONS = ("defect-fix", "reorder-main-red-fix")
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
    remove only the fix PR itself, so it can jump the queue.
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
                "reason": reason,
                "note": note,
                "fix_pr": os.environ.get(FIX_PR_ENV) or None,
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
        if marker and shipyard_parent():
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
        if shipyard_parent():
            return 0
        print(
            "queue-removal-guard: ignoring SHIPYARD_INTERNAL_QUEUE_MUTATION=1: the "
            "calling process is not the Shipyard binary, so it cannot claim "
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
