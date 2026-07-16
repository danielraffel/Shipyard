#!/usr/bin/env python3
"""Poll exact GitHub review triggers without exposing an inbound webhook."""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time

MODULE_PATH = Path(__file__).with_name("review-control.py")
SPEC = importlib.util.spec_from_file_location("shipyard_review_control", MODULE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("cannot load review controller")
CONTROL = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CONTROL)

COMMAND = "/shipyard review"
REPO_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*\Z")
SHA_RE = re.compile(r"[0-9a-f]{40}\Z")
MAX_GITHUB_RESPONSE = 8 * 1024 * 1024
MAX_PUBLISH_BODY = 4096
RUNNING_STALE_SECONDS = 3 * 60 * 60


def load_policy(path: Path) -> dict[str, object]:
    policy = json.loads(path.read_text(encoding="utf-8"))
    expected = {
        "enabled", "publish_results", "ghapp", "controller_config", "state_db",
        "results_dir", "authorized_users", "repositories",
    }
    if not isinstance(policy, dict) or set(policy) != expected:
        raise CONTROL.Blocked("comment policy has missing or unexpected fields")
    if policy["enabled"] is not True:
        raise CONTROL.Blocked("GitHub comment polling is disabled")
    CONTROL.require_root_protected_file(path)
    if not isinstance(policy["publish_results"], bool):
        raise CONTROL.Blocked("publish_results must be a boolean")
    authorized_users = policy["authorized_users"]
    if not isinstance(authorized_users, dict) or not authorized_users:
        raise CONTROL.Blocked("authorized user allowlist is empty")
    if not all(
        isinstance(login, str) and login and type(user_id) is int and user_id > 0
        for login, user_id in authorized_users.items()
    ):
        raise CONTROL.Blocked("authorized users must map login to numeric GitHub user id")
    repositories = policy["repositories"]
    if not isinstance(repositories, dict) or not repositories:
        raise CONTROL.Blocked("repository allowlist is empty")
    for repo, recipe in repositories.items():
        if not isinstance(repo, str) or not REPO_RE.fullmatch(repo):
            raise CONTROL.Blocked(f"invalid repository allowlist entry: {repo!r}")
        recipe_path = Path(str(recipe))
        if not recipe_path.is_absolute() or not recipe_path.is_file():
            raise CONTROL.Blocked(f"protected recipe does not exist: {recipe_path}")
        CONTROL.require_root_protected_file(recipe_path)
    controller_config = Path(str(policy["controller_config"]))
    CONTROL.require_root_protected_file(controller_config)
    ghapp = Path(str(policy["ghapp"]))
    CONTROL.require_root_protected_file(ghapp)
    return policy


def exact_command(body: object) -> bool:
    return isinstance(body, str) and body == COMMAND


def gh_json(ghapp: Path, endpoint: str) -> object:
    if not ghapp.is_absolute() or not ghapp.is_file():
        raise CONTROL.Blocked("configured ghapp executable is unavailable")
    completed = subprocess.run(
        [str(ghapp), "api", "--method", "GET", endpoint],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        timeout=60, check=False,
    )
    if len(completed.stdout) > MAX_GITHUB_RESPONSE or len(completed.stderr) > 64 * 1024:
        raise CONTROL.Blocked("GitHub response exceeded limit")
    if completed.returncode != 0:
        raise CONTROL.Blocked("GitHub App request failed: " + completed.stderr.decode(errors="replace")[-1000:])
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise CONTROL.Blocked("GitHub App returned invalid JSON") from error


def gh_post_json(ghapp: Path, endpoint: str, value: dict[str, object]) -> object:
    """Write JSON through stdin so user-controlled text never appears in argv."""
    encoded = json.dumps(value, separators=(",", ":")).encode()
    if len(encoded) > MAX_PUBLISH_BODY:
        raise CONTROL.Blocked("GitHub publication exceeded limit")
    completed = subprocess.run(
        [str(ghapp), "api", "--method", "POST", "--input", "-", endpoint],
        input=encoded, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        timeout=60, check=False,
    )
    if len(completed.stdout) > MAX_GITHUB_RESPONSE or len(completed.stderr) > 64 * 1024:
        raise CONTROL.Blocked("GitHub publication response exceeded limit")
    if completed.returncode != 0:
        raise CONTROL.Blocked("GitHub result publication failed: " + completed.stderr.decode(errors="replace")[-1000:])
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise CONTROL.Blocked("GitHub publication returned invalid JSON") from error


def result_comment(comment_id: int, request: dict[str, object], result: dict[str, object]) -> str:
    status = result.get("status")
    controller = result.get("controller")
    commands = result.get("commands")
    if status not in {"pass", "fail"} or not isinstance(controller, dict) or not isinstance(commands, list):
        raise CONTROL.Blocked("result is not publishable")
    if controller.get("teardown") != "confirmed":
        raise CONTROL.Blocked("result cannot be published before teardown is confirmed")
    head = str(request["head_sha"])
    marker = f"<!-- shipyard-review:{comment_id}:{head} -->"
    outcome = "passed" if status == "pass" else "failed"
    return f"Shipyard review {outcome} for `{head[:12]}` ({len(commands)} steps).\n\n{marker}"


def publish_result(
    ghapp: Path, repo: str, issue_number: int, comment_id: int,
    request: dict[str, object], result: dict[str, object],
) -> None:
    body = result_comment(comment_id, request, result)
    marker = body.rsplit("\n", 1)[-1]
    existing = gh_json(ghapp, f"repos/{repo}/issues/{issue_number}/comments?per_page=100")
    if not isinstance(existing, list):
        raise CONTROL.Blocked("GitHub issue comments response is not an array")
    if any(isinstance(item, dict) and marker in str(item.get("body", "")) for item in existing):
        return
    created = gh_post_json(ghapp, f"repos/{repo}/issues/{issue_number}/comments", {"body": body})
    if not isinstance(created, dict) or not isinstance(created.get("id"), int):
        raise CONTROL.Blocked("GitHub publication response lacked a comment id")


def gh_archive(ghapp: Path, repo: str, head_sha: str, destination: Path) -> None:
    if not ghapp.is_absolute() or not ghapp.is_file():
        raise CONTROL.Blocked("configured ghapp executable is unavailable")
    if not REPO_RE.fullmatch(repo) or not SHA_RE.fullmatch(head_sha):
        raise CONTROL.Blocked("GitHub source archive provenance is invalid")
    endpoint = f"repos/{repo}/tarball/{head_sha}"
    with destination.open("wb") as output:
        completed = subprocess.run(
            [str(ghapp), "api", "--method", "GET", "-H", "Accept: application/vnd.github+json", endpoint],
            stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.PIPE,
            timeout=180, check=False,
        )
    if completed.returncode != 0:
        destination.unlink(missing_ok=True)
        raise CONTROL.Blocked("GitHub source download failed: " + completed.stderr.decode(errors="replace")[-1000:])
    if destination.stat().st_size > CONTROL.MAX_SOURCE_BYTES:
        destination.unlink(missing_ok=True)
        raise CONTROL.Blocked("GitHub source archive exceeded limit")


def initialize_db(path: Path) -> sqlite3.Connection:
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(path.parent, 0o700)
    connection = sqlite3.connect(path)
    connection.execute("PRAGMA journal_mode=WAL")
    connection.execute(
        "CREATE TABLE IF NOT EXISTS comments ("
        "repo TEXT NOT NULL, comment_id INTEGER NOT NULL, status TEXT NOT NULL, "
        "updated_at INTEGER NOT NULL, detail TEXT NOT NULL, PRIMARY KEY (repo, comment_id))"
    )
    connection.commit()
    os.chmod(path, 0o600)
    return connection


def comment_record(connection: sqlite3.Connection, repo: str, comment_id: int) -> tuple[str, int] | None:
    row = connection.execute(
        "SELECT status, updated_at FROM comments WHERE repo = ? AND comment_id = ?", (repo, comment_id)
    ).fetchone()
    return (str(row[0]), int(row[1])) if row else None


def record(connection: sqlite3.Connection, repo: str, comment_id: int, status: str, detail: str) -> None:
    connection.execute(
        "INSERT INTO comments(repo, comment_id, status, updated_at, detail) VALUES(?,?,?,?,?) "
        "ON CONFLICT(repo, comment_id) DO UPDATE SET status=excluded.status, "
        "updated_at=excluded.updated_at, detail=excluded.detail",
        (repo, comment_id, status, int(time.time()), detail[:4096]),
    )
    connection.commit()


def validate_pr(repo: str, issue_number: int, value: object) -> dict[str, object]:
    if not isinstance(value, dict) or value.get("state") != "open" or value.get("number") != issue_number:
        raise CONTROL.Blocked("trigger does not refer to an open pull request")
    head = value.get("head")
    base = value.get("base")
    if not isinstance(head, dict) or not isinstance(base, dict):
        raise CONTROL.Blocked("pull request provenance is incomplete")
    head_sha = head.get("sha")
    base_sha = base.get("sha")
    base_repo = base.get("repo")
    if not SHA_RE.fullmatch(str(head_sha)) or not SHA_RE.fullmatch(str(base_sha)):
        raise CONTROL.Blocked("pull request SHA is invalid")
    if not isinstance(base_repo, dict) or base_repo.get("full_name") != repo:
        raise CONTROL.Blocked("pull request base repository does not match policy")
    return {"repo": repo, "pr": issue_number, "head_sha": head_sha, "base_sha": base_sha}


def process_comment(
    policy: dict[str, object], connection: sqlite3.Connection, repo: str, comment: object,
) -> None:
    if not isinstance(comment, dict) or not isinstance(comment.get("id"), int):
        return
    comment_id = comment["id"]
    prior = comment_record(connection, repo, comment_id)
    if prior:
        status, updated_at = prior
        if status in {"ignored", "completed", "blocked"}:
            return
        if status == "running":
            if int(time.time()) - updated_at <= RUNNING_STALE_SECONDS:
                return
            record(
                connection, repo, comment_id, "blocked",
                "stale interrupted attempt; reconcile fixed resources and submit a fresh trigger",
            )
            return
        raise CONTROL.Blocked(f"comment state is invalid: {status}")
    user = comment.get("user")
    login = user.get("login") if isinstance(user, dict) else None
    user_id = user.get("id") if isinstance(user, dict) else None
    if (
        not exact_command(comment.get("body"))
        or not isinstance(login, str)
        or type(user_id) is not int
        or login not in policy["authorized_users"]
        or policy["authorized_users"].get(login) != user_id
    ):
        record(connection, repo, comment_id, "ignored", "not an exact authorized trigger")
        return
    issue_url = str(comment.get("issue_url", ""))
    prefix = f"https://api.github.com/repos/{repo}/issues/"
    if not issue_url.startswith(prefix) or not issue_url[len(prefix):].isdigit():
        record(connection, repo, comment_id, "blocked", "issue URL did not match repository")
        return
    issue_number = int(issue_url[len(prefix):])
    ghapp = Path(str(policy["ghapp"]))
    try:
        pr_value = gh_json(ghapp, f"repos/{repo}/pulls/{issue_number}")
        request = validate_pr(repo, issue_number, pr_value)
        record(connection, repo, comment_id, "running", "admitted to disposable VM")
        controller_config = CONTROL.load_json(Path(str(policy["controller_config"])))
        if not isinstance(controller_config, dict):
            raise CONTROL.Blocked("controller config is invalid")
        lifecycle = CONTROL.ReviewLifecycle(controller_config)
        with tempfile.TemporaryDirectory(prefix="shipyard-comment-review-") as temp_name:
            temp = Path(temp_name)
            source = temp / "source.tar.gz"
            iso = temp / lifecycle.iso_name
            gh_archive(ghapp, repo, str(request["head_sha"]), source)
            manifest = CONTROL.build_iso(source, Path(str(policy["repositories"][repo])), request, iso)
            result = lifecycle.run(iso, manifest)
        result["controller"]["teardown"] = "confirmed"
        results = Path(str(policy["results_dir"]))
        results.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(results, 0o700)
        result_path = results / f"{repo.replace('/', '--')}-{issue_number}-{comment_id}.json"
        result_path.write_text(json.dumps(result, sort_keys=True) + "\n", encoding="utf-8")
        os.chmod(result_path, 0o600)
        if policy["publish_results"]:
            publish_result(ghapp, repo, issue_number, comment_id, request, result)
        record(connection, repo, comment_id, "completed", str(result.get("status", "unknown")))
    except Exception as error:
        record(connection, repo, comment_id, "blocked", str(error))
        raise


def poll_once(policy: dict[str, object]) -> None:
    connection = initialize_db(Path(str(policy["state_db"])))
    try:
        for repo in policy["repositories"]:
            comments = gh_json(Path(str(policy["ghapp"])), f"repos/{repo}/issues/comments?sort=created&direction=desc&per_page=100")
            if not isinstance(comments, list):
                raise CONTROL.Blocked("GitHub comments response is not an array")
            for comment in reversed(comments):
                process_comment(policy, connection, repo, comment)
    finally:
        connection.close()


def poll_once_with_teardown(policy: dict[str, object]) -> None:
    previous_handler = signal.signal(signal.SIGTERM, CONTROL.interrupt_for_teardown)
    try:
        poll_once(policy)
    finally:
        signal.signal(signal.SIGTERM, previous_handler)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--policy", type=Path, required=True)
    parser.add_argument("--once", action="store_true")
    args = parser.parse_args()
    policy = load_policy(args.policy)
    if not args.once:
        raise CONTROL.Blocked("only one-shot polling is supported; systemd owns repetition")
    poll_once_with_teardown(policy)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (CONTROL.Blocked, CONTROL.ControllerInterrupted) as error:
        print(json.dumps({"status": "blocked", "reason": str(error)}), file=sys.stderr)
        raise SystemExit(3)
