#!/usr/bin/env python3
"""Refuse arming or enqueuing a pull request whose live queue state makes it harmful.

Intercepted (through the App-authenticated ``ghapp`` wrapper):

* ``pr merge ... --auto``;
* plain ``pr merge <pr>`` when the PR's base branch has a merge queue (there
  a plain merge enqueues);
* ``api graphql`` documents containing ``enablePullRequestAutoMerge`` or
  ``enqueuePullRequest``, whether passed as ``-f/-F query=``, ``query=@file``
  or ``--input file``. A GraphQL body read from stdin cannot be inspected and
  is refused as ambiguous.

Each intercepted PR's live state is read with the same GraphQL shape as
``shipyard landing --pr`` and classified by the Python twin of
``src/pr_queue_state.rs``. Both implementations assert against the shared
real-response corpus in ``tests/fixtures/github/``.

Refused: ``queued``, ``armed_not_queued``, ``ejected`` with no new head since
the last removal, ``merged``, ``closed`` and anything unreadable. Allowed:
``never_armed`` and ``ejected`` after a new head.

Shipyard's own exact-head, audited enqueue path carries an internal marker
and bypasses the guard; an operator override exists and always prints a
WARNING. Both are documented for humans in docs/ghapp-guards.md and are kept
out of refusal text.

Actor identity is deliberately not consulted: every queue mutation is
attributed to the same App actor whether Shipyard or an agent issued it.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import pathlib
import re
import subprocess
import sys
from types import ModuleType
from typing import Any


ARM_MUTATIONS = ("enablepullrequestautomerge", "enqueuepullrequest")
RETRY_HAZARD_REASONS = ("failed_checks", "merge_conflict")
SAME_HEAD_REQUEUE_ALLOWED_REASONS = ("invalid_merge_commit",)
OPERATOR_NOTE = (
    "An explicit authority override exists for operators; see docs/ghapp-guards.md."
)
REST_AUTO_MERGE_NOTE = (
    "REST pulls/<n>.auto_merge is null for every queued PR (GitHub consumes "
    "auto-merge on enqueue); never read it as 'unarmed'."
)

_TIMELINE = (
    "timelineItems(last:100,itemTypes:[PULL_REQUEST_COMMIT,HEAD_REF_FORCE_PUSHED_EVENT,"
    "ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT,AUTO_MERGE_ENABLED_EVENT,"
    "AUTO_MERGE_DISABLED_EVENT,MERGED_EVENT]){pageInfo{hasPreviousPage} nodes{__typename "
    "... on PullRequestCommit{commit{oid}} ... on HeadRefForcePushedEvent{createdAt} "
    "... on AddedToMergeQueueEvent{createdAt} "
    "... on RemovedFromMergeQueueEvent{createdAt reason} "
    "... on AutoMergeEnabledEvent{createdAt} ... on AutoMergeDisabledEvent{createdAt} "
    "... on MergedEvent{createdAt}}}"
)
_PR_FIELDS = (
    "number state headRefOid baseRefName isInMergeQueue mergeQueueEntry{state position} "
    "autoMergeRequest{enabledAt} repository{nameWithOwner} " + _TIMELINE
)
PR_BY_NUMBER_QUERY = (
    "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name)"
    "{pullRequest(number:$number){" + _PR_FIELDS + "}}}"
)
PR_BY_ID_QUERY = "query($id:ID!){node(id:$id){... on PullRequest{" + _PR_FIELDS + "}}}"
BASE_QUEUE_QUERY = (
    "query($owner:String!,$name:String!,$base:String!){repository(owner:$owner,name:$name)"
    "{mergeQueue(branch:$base){id}}}"
)


class GuardError(RuntimeError):
    """The guard cannot prove the request is safe."""


# ---------------------------------------------------------------------------
# Request parsing: reuse the removal guard's helpers rather than a second copy.
# ---------------------------------------------------------------------------


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
        if all(
            hasattr(module, attribute)
            for attribute in ("api_target", "graphql_document", "option_values", "GuardError")
        ):
            return module
    return None


PARSER = _load_request_parser()


# ---------------------------------------------------------------------------
# Classifier: the Python twin of src/pr_queue_state.rs.
# ---------------------------------------------------------------------------


def _pull_request(response: Any) -> dict[str, Any] | None:
    if not isinstance(response, dict):
        return None
    data = response.get("data")
    if isinstance(data, dict):
        repository = data.get("repository")
        if isinstance(repository, dict) and isinstance(repository.get("pullRequest"), dict):
            return repository["pullRequest"]
        if isinstance(data.get("node"), dict):
            return data["node"]
    if "state" in response:
        return response
    return None


def classify_pr_queue_state(response: Any) -> dict[str, Any]:
    """Classify a GraphQL pull-request response. Unreadable input is ``unknown``."""
    errors = response.get("errors") if isinstance(response, dict) else None
    if isinstance(errors, list) and errors:
        messages = "; ".join(
            str(error.get("message")) for error in errors if isinstance(error, dict)
        )
        return {"class": "unknown", "detail": f"GraphQL errors: {messages}"}
    pr = _pull_request(response)
    if pr is None:
        return {"class": "unknown", "detail": "response carries no pull request object"}
    state = pr.get("state")
    if not isinstance(state, str):
        return {"class": "unknown", "detail": "state is missing"}
    in_queue = pr.get("isInMergeQueue")
    if not isinstance(in_queue, bool):
        return {"class": "unknown", "detail": "isInMergeQueue is missing"}
    timeline = pr.get("timelineItems")
    nodes = timeline.get("nodes") if isinstance(timeline, dict) else None
    if not isinstance(nodes, list):
        return {"class": "unknown", "detail": "timelineItems.nodes is missing"}
    page_info = timeline.get("pageInfo") if isinstance(timeline, dict) else None
    has_previous = page_info.get("hasPreviousPage") if isinstance(page_info, dict) else None
    timeline_complete = (not has_previous) if isinstance(has_previous, bool) else None

    requeues = 0
    hazard_pending = False
    last_removal: tuple[int, str, str | None] | None = None
    last_new_head: int | None = None
    for index, node in enumerate(nodes):
        typename = node.get("__typename", "") if isinstance(node, dict) else ""
        if typename in ("PullRequestCommit", "HeadRefForcePushedEvent"):
            hazard_pending = False
            last_new_head = index
        elif typename == "RemovedFromMergeQueueEvent":
            reason = node.get("reason") if isinstance(node.get("reason"), str) else "UNKNOWN"
            hazard_pending = reason.lower() in RETRY_HAZARD_REASONS
            at = node.get("createdAt") if isinstance(node.get("createdAt"), str) else None
            last_removal = (index, reason, at)
        elif typename == "AddedToMergeQueueEvent":
            if hazard_pending:
                requeues += 1
            hazard_pending = False

    last_ejection = None
    if last_removal is not None and last_removal[1].lower() != "merged":
        index, reason, at = last_removal
        last_ejection = {
            "reason": reason,
            "at": at,
            "new_head_since": last_new_head is not None and last_new_head > index,
        }

    entry = pr.get("mergeQueueEntry") if isinstance(pr.get("mergeQueueEntry"), dict) else None
    auto_merge = (
        pr.get("autoMergeRequest") if isinstance(pr.get("autoMergeRequest"), dict) else None
    )
    result: dict[str, Any] = {
        "pr": pr.get("number"),
        "requeues_without_new_head": requeues,
        "last_ejection": last_ejection,
        "timeline_complete": timeline_complete,
    }
    upper = state.upper()
    if upper == "MERGED":
        result["class"] = "merged"
    elif upper == "CLOSED":
        result["class"] = "closed"
    elif upper != "OPEN":
        result.update({"class": "unknown", "detail": f"unrecognized pull request state {state}"})
    elif in_queue:
        result.update(
            {
                "class": "queued",
                "entry_state": entry.get("state") if entry else None,
                "position": entry.get("position") if entry else None,
            }
        )
    elif auto_merge is not None:
        result.update({"class": "armed_not_queued", "enabled_at": auto_merge.get("enabledAt")})
    elif last_ejection is not None:
        result.update(
            {
                "class": "ejected",
                "reason": last_ejection["reason"],
                "at": last_ejection["at"],
                "new_head_since_removal": last_ejection["new_head_since"],
            }
        )
    elif timeline_complete is False and last_removal is None and last_new_head is None:
        # A truncated window with no removal and no new head could be hiding an
        # older same-head ejection; that is not "never armed".
        result.update(
            {
                "class": "unknown",
                "detail": "timelineItems window is truncated and contains no queue removal and "
                "no new head, so an older ejection of this head cannot be ruled out",
            }
        )
    else:
        result["class"] = "never_armed"
    return result


def decide(classification: dict[str, Any]) -> tuple[bool, str]:
    """Return ``(allowed, message)`` for one classified pull request.

    Refusal text names the correct path only. Override mechanisms are
    documented for operators in docs/ghapp-guards.md and deliberately not
    spelled out here, where an agent would read them as the next step.
    """
    number = classification.get("pr", "?")
    label = f"PR #{number}"
    land = f"`shipyard ship --pr {number}`"
    klass = classification.get("class")
    reason = str(classification.get("reason") or "unknown")
    at = classification.get("at") or "an unknown time"
    if klass == "never_armed":
        return True, f"{label} is not armed and not queued"
    if klass == "ejected" and classification.get("new_head_since_removal"):
        return True, f"{label} was removed ({reason}) and has a new head since"
    if klass == "ejected" and reason.lower() in SAME_HEAD_REQUEUE_ALLOWED_REASONS:
        return True, f"{label} was removed ({reason}); GitHub-side, nothing against this head"
    if klass == "queued":
        position = classification.get("position")
        where = f" at position {position}" if position is not None else ""
        return False, (
            f"{label} is already in the merge queue{where}; nothing to do. "
            "REST auto_merge=null is expected: " + REST_AUTO_MERGE_NOTE
        )
    if klass == "armed_not_queued":
        since = classification.get("enabled_at") or "an unknown time"
        return False, (
            f"{label} is already armed since {since}; the queue will pick it up. Nothing to do."
        )
    if klass == "ejected" and reason.lower() in RETRY_HAZARD_REASONS:
        return False, (
            f"{label} was ejected for {reason} at {at}; re-enqueuing the same head under "
            f"ALLGREEN fails its batch-mates. Push a fix first, then {land}."
        )
    if klass == "ejected":
        return False, (
            f"{label} was removed from the queue ({reason}) at {at} and the head has not "
            f"changed; confirm with whoever dequeued it before re-enqueuing with {land}."
        )
    if klass in ("merged", "closed"):
        return False, f"{label} is {klass}; there is nothing to arm"
    return False, (
        f"{label} merge-queue state could not be determined "
        f"({classification.get('detail', 'unknown')}); refusing to arm blind"
    )


# ---------------------------------------------------------------------------
# Live reads through the real gh binary and the App token ghapp exported.
# ---------------------------------------------------------------------------


def run_real_gh(arguments: list[str]) -> Any:
    real_gh = os.environ.get("GHAPP_REAL_GH", "/opt/homebrew/bin/gh")
    try:
        completed = subprocess.run(
            [real_gh, *arguments],
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
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


def graphql(query: str, variables: dict[str, str]) -> Any:
    arguments = ["api", "graphql", "-f", f"query={query}"]
    for key, value in variables.items():
        arguments += ["-F" if key == "number" else "-f", f"{key}={value}"]
    return run_real_gh(arguments)


def split_repo(repo: str) -> tuple[str, str]:
    parts = repo.strip().split("/")
    if len(parts) >= 2 and parts[-2] and parts[-1]:
        return parts[-2], parts[-1]
    raise GuardError(f"cannot resolve repository `{repo}`")


def base_has_merge_queue(owner: str, name: str, base: str) -> bool:
    response = graphql(BASE_QUEUE_QUERY, {"owner": owner, "name": name, "base": base})
    try:
        repository = response["data"]["repository"]
    except (KeyError, TypeError) as error:
        raise GuardError("cannot read whether the base branch has a merge queue") from error
    if response.get("errors") or not isinstance(repository, dict) or "mergeQueue" not in repository:
        raise GuardError("cannot read whether the base branch has a merge queue")
    return repository["mergeQueue"] is not None


# ---------------------------------------------------------------------------
# Request detection.
# ---------------------------------------------------------------------------

PR_MERGE_VALUE_OPTIONS = {
    "-A", "--author-email", "-b", "--body", "-F", "--body-file", "--match-head-commit",
    "-R", "--repo", "-t", "--subject",
}


def _require_parser() -> ModuleType:
    if PARSER is None:
        raise GuardError("request parser (queue-removal-guard) is unavailable")
    return PARSER


def pr_merge_target(args: list[str]) -> tuple[str | None, str | None]:
    """Return ``(selector, repo)`` for ``pr merge`` arguments."""
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
    repos = _require_parser().option_values(args[2:], {"-R", "--repo"})
    return selector, (repos[-1] if repos else None)


def resolve_merge_pr(args: list[str]) -> tuple[str, str, int]:
    selector, repo = pr_merge_target(args)
    if selector:
        match = re.search(r"github\.com/([^/]+)/([^/]+)/pull/([1-9][0-9]*)", selector)
        if match:
            return match.group(1), match.group(2), int(match.group(3))
    if repo is None:
        repo = os.environ.get("GH_REPO", "")
    if not repo:
        owner, name = _require_parser().current_repo_identity()
        repo = f"{owner}/{name}"
    owner, name = split_repo(repo)
    if selector and re.fullmatch(r"#?[1-9][0-9]*", selector):
        return owner, name, int(selector.lstrip("#"))
    view = ["pr", "view", "--repo", f"{owner}/{name}", "--json", "number"]
    if selector:
        view.insert(2, selector)
    value = run_real_gh(view)
    number = value.get("number") if isinstance(value, dict) else None
    if not isinstance(number, int):
        raise GuardError(f"cannot resolve the pull request for `{selector or 'current branch'}`")
    return owner, name, number


def graphql_variables(args: list[str]) -> dict[str, Any]:
    parser = _require_parser()
    variables: dict[str, Any] = {}
    for value in parser.option_values(args, {"-f", "-F", "--field", "--raw-field"}):
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
    for path in parser.option_values(args, {"--input"}):
        if path == "-":
            raise GuardError("cannot inspect a GraphQL body read from stdin")
        try:
            body = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise GuardError(f"cannot inspect GraphQL input: {error}") from error
        if isinstance(body, dict) and isinstance(body.get("variables"), dict):
            variables.update(body["variables"])
    return variables


def arm_pull_request_ids(document: str, variables: dict[str, Any]) -> list[str]:
    """Every pull-request node id an arm mutation in ``document`` targets."""
    ids: list[str] = []
    mutation = re.compile(r"(enablePullRequestAutoMerge|enqueuePullRequest)\s*\(", re.IGNORECASE)
    for match in mutation.finditer(document):
        tail = document[match.end():]
        target = re.search(r"pullRequestId\s*:\s*(\$[A-Za-z_][A-Za-z0-9_]*|\"([^\"]+)\")", tail)
        if target is None:
            raise GuardError("cannot find the pullRequestId an arm mutation targets")
        if target.group(2) is not None:
            ids.append(target.group(2))
            continue
        name = target.group(1)[1:]
        value = variables.get(name)
        if not isinstance(value, str) or not value:
            raise GuardError(f"cannot resolve arm mutation variable ${name}")
        ids.append(value)
    if not ids:
        raise GuardError("arm mutation found but no pullRequestId could be read")
    return ids


def arm_request(args: list[str]) -> list[dict[str, Any]] | None:
    """``None`` when ``args`` cannot arm or enqueue; else the classified targets."""
    if "--help" in args or "-h" in args:
        return None
    if len(args) >= 2 and args[0] == "pr" and args[1] == "merge":
        if "--disable-auto" in args:
            return None
        auto = "--auto" in args
        if not auto and "--admin" in args:
            # A direct administrator merge bypasses the queue; it enqueues nothing.
            return None
        owner, name, number = resolve_merge_pr(args)
        response = graphql(PR_BY_NUMBER_QUERY, {"owner": owner, "name": name, "number": str(number)})
        if not auto:
            base = _base_ref(response)
            if base is None:
                raise GuardError("cannot read the pull request's base branch")
            if not base_has_merge_queue(owner, name, base):
                return None
        return [classify_pr_queue_state(response)]
    if args and args[0] == "api":
        parser = _require_parser()
        try:
            endpoint, query = parser.api_target(args)
            document = "\n".join([parser.graphql_document(args), *query.get("query", [])])
        except parser.GuardError as error:
            if any("graphql" in arg.lower() for arg in args):
                raise GuardError(str(error)) from error
            return None
        if endpoint != "graphql":
            return None
        compact = "".join(document.split()).lower()
        if not any(name in compact for name in ARM_MUTATIONS):
            return None
        ids = arm_pull_request_ids(document, graphql_variables(args))
        return [classify_pr_queue_state(graphql(PR_BY_ID_QUERY, {"id": node})) for node in ids]
    return None


def _base_ref(response: Any) -> str | None:
    pr = _pull_request(response)
    base = pr.get("baseRefName") if pr else None
    return base if isinstance(base, str) and base else None


def _override(message: str) -> int:
    print(
        "queue-arm-guard: WARNING: GHAPP_ALLOW_QUEUE_REARM=1 overrides a refusal: " + message,
        file=sys.stderr,
    )
    return 0


def main(args: list[str]) -> int:
    if os.environ.get("SHIPYARD_INTERNAL_QUEUE_MUTATION") == "1":
        return 0
    override = os.environ.get("GHAPP_ALLOW_QUEUE_REARM") == "1"
    try:
        targets = arm_request(args)
    except GuardError as error:
        message = f"refusing ambiguous queue-arm request: {error}"
        if override:
            return _override(message)
        print(
            f"queue-arm-guard: {message}. Inspect the PR with `shipyard landing --pr <n>` and "
            f"land it with `shipyard ship --pr <n>`. {OPERATOR_NOTE}",
            file=sys.stderr,
        )
        return 1
    if targets is None:
        return 0
    refusals = [message for allowed, message in map(decide, targets) if not allowed]
    if not refusals:
        return 0
    message = " | ".join(refusals)
    if override:
        return _override(message)
    print(
        f"queue-arm-guard: refusing: {message} Inspect it with `shipyard landing --pr <n>`. "
        f"{OPERATOR_NOTE}",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
