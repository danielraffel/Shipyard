#!/usr/bin/env python3
"""Recover a release that stayed a draft because its release run failed.

`release.yml` keeps a release a draft until the macOS DMG is signed, attested
and published by the final job. If that run ends in failure (a runner that
cannot reach the Rust distribution servers, a Sigstore blip) nothing retries
it and the draft sits forever. This script runs from `release-heal.yml` on a
schedule and on every main push. For the release of the current Cargo version
it either does nothing (the release is published with every expected asset, even
when its run failed and the release was recovered by hand), re-runs the failed jobs of the release run (bounded by
MAX_ATTEMPTS), or exits non-zero with the exact reason. A run still in progress whose
current step has run past its stall threshold (see STALL_FLOOR_MINUTES) is
cancelled and re-run, within the same attempt cap. The attempt cap means an exhausted run fails every scheduled pass until a
newer release run exists; that is deliberate, a stuck draft stays visible.
It never edits the
release itself: publication stays inside the pipeline that attests the DMG.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

GRACE_MINUTES = 20
MAX_ATTEMPTS = 3
RELEASE_PAGE_SIZE = 100
# Drafts list ahead of published releases, so a backlog of abandoned drafts can
# push the current release past the first page; scan until the tag is found.
MAX_RELEASE_PAGES = 20
FAILED = {"failure", "timed_out", "startup_failure"}
# A run still in progress is cancelled and re-run when the step it is on has
# run longer than the largest of: STALL_FLOOR_MINUTES; STALL_MEDIAN_FACTOR times
# that step's median over the last STALL_HISTORY_RUNS green release runs; and
# the step's own timeout-minutes plus STALL_TIMEOUT_GRACE_MINUTES. The last term
# keeps heal from cutting short a step that bounds itself (a notarization retry
# can legitimately run 25 minutes), so heal only backs up a runner that failed
# to enforce a limit, or a step that has none. Step-level started_at is the
# progress signal: a run moving through its steps is never cancelled.
STALL_FLOOR_MINUTES = 20
STALL_MEDIAN_FACTOR = 3
STALL_TIMEOUT_GRACE_MINUTES = 5
STALL_HISTORY_RUNS = 10
CANCEL_SETTLE_SECS = 8 * 60
CANCEL_POLL_SECS = 20
RERUN_CONFIRM_SECS = 90
BLOCKED_LABEL = "release-blocked"
# Repository variable (passed as env) that lets heal cancel a stalled run.
CANCEL_STALLED_ENV = "RELEASE_HEAL_CANCEL_STALLED"


@dataclass(frozen=True)
class Decision:
    action: str  # "noop" | "rerun" | "cancel-rerun" | "resume-rerun" | "fail"
    reason: str
    run_id: int | None = None


def _parse(ts: str) -> datetime:
    return datetime.fromisoformat(ts.replace("Z", "+00:00"))


def decide(
    release: dict | None,
    run: dict | None,
    *,
    now: datetime,
    signing_enabled: bool,
    grace_minutes: int = GRACE_MINUTES,
    max_attempts: int = MAX_ATTEMPTS,
    failed_steps: str = "",
    expected_assets: tuple[str, ...] = (),
) -> Decision:
    """Pure decision over the release and the latest release run for its tag.

    A published release is healthy when it carries every expected asset,
    whatever its release run concluded: a release recovered by hand after its
    run exhausted its retries is done, not stuck.
    """
    if release is not None and not release.get("draft"):
        present = {a.get("name") for a in release.get("assets") or [] if isinstance(a, dict)}
        missing = [name for name in expected_assets if name not in present]
        if missing:
            return Decision(
                "fail",
                f"release is published but missing {len(missing)} expected asset(s): {', '.join(missing)}",
                run["id"] if run else None,
            )
        return Decision("noop", f"release is published with all {len(expected_assets)} expected assets"
                        if expected_assets else "release is already published")
    if not signing_enabled:
        return Decision("noop", "CI_MACOS_SIGNING_ENABLED is not 'true'; the draft is published by hand by design")
    if run is None:
        if release is None:
            return Decision("noop", "no release and no release run yet; auto-release tags first")
        return Decision("fail", "draft release exists but no release.yml run was found for its tag")
    if run.get("status") != "completed":
        return Decision("noop", f"release run {run['id']} is still {run.get('status')}")
    age = now - _parse(run["updated_at"])
    if age.total_seconds() < grace_minutes * 60:
        return Decision("noop", f"release run {run['id']} finished {int(age.total_seconds() // 60)} min ago; inside the grace window")
    conclusion = run.get("conclusion")
    if conclusion == "success":
        if release is None:
            return Decision("noop", "release run succeeded and no draft is visible")
        return Decision(
            "fail",
            f"release run {run['id']} succeeded but the release is still a draft; "
            "the publish step did not publish it (check the 'Publish the attested release' step)",
            run["id"],
        )
    if conclusion == "cancelled":
        # A maintainer may have cancelled on purpose, and a run superseded by
        # the release concurrency group is cancelled too; neither is resumed.
        return Decision(
            "fail",
            f"release run {run['id']} was cancelled; re-run it by hand or delete the draft",
            run["id"],
        )
    if conclusion in FAILED:
        attempt = int(run.get("run_attempt") or 1)
        if attempt >= max_attempts:
            return Decision(
                "fail",
                f"release run {run['id']} {conclusion} after {attempt} attempts; not retrying. "
                f"Failed: {failed_steps or 'see run logs'}",
                run["id"],
            )
        return Decision(
            "rerun",
            f"release run {run['id']} {conclusion} (attempt {attempt}/{max_attempts}): {failed_steps or 'see run logs'}",
            run["id"],
        )
    return Decision("noop", f"release run {run['id']} concluded {conclusion}")


@dataclass(frozen=True)
class Stall:
    job_id: int
    job_name: str
    step_name: str
    elapsed_secs: float
    threshold_secs: float


_ITEM_RE = re.compile(r"^(\s*)-\s+(.*)$")
_KEY_RE = re.compile(r"^(\s*)(name|timeout-minutes):\s*(.+?)\s*$")


def _scalar(value: str) -> str:
    value = re.sub(r"\s+#.*$", "", value).strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "'\"":
        value = value[1:-1]
    return value


def step_timeouts(workflow_text: str) -> dict[str, int]:
    """Step name -> timeout-minutes for every named step that sets one.

    A line scan, so heal needs no YAML library on the runner; a test checks it
    against PyYAML for release.yml. A name used by several jobs keeps its
    largest timeout, which only ever raises the stall threshold.
    """
    result: dict[str, int] = {}
    item: dict[str, str] | None = None
    item_indent = -1

    def close() -> None:
        if item and "name" in item and "timeout-minutes" in item:
            try:
                minutes = int(item["timeout-minutes"])
            except ValueError:
                return
            result[item["name"]] = max(minutes, result.get(item["name"], 0))

    for line in workflow_text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        indent = len(line) - len(line.lstrip())
        start = _ITEM_RE.match(line)
        if item is not None and indent <= item_indent:
            close()
            item = None
        if start and item is None:
            item = {}
            item_indent = len(start.group(1))
            first = _KEY_RE.match(" " * (item_indent + 2) + start.group(2))
            if first:
                item[first.group(2)] = _scalar(first.group(3))
            continue
        if start:
            continue  # a list nested inside the current item
        if item is not None and indent == item_indent + 2:
            key = _KEY_RE.match(line)
            if key:
                item[key.group(2)] = _scalar(key.group(3))
    close()
    return result


def _median(values: list[float]) -> float:
    ordered = sorted(values)
    mid = len(ordered) // 2
    return ordered[mid] if len(ordered) % 2 else (ordered[mid - 1] + ordered[mid]) / 2


def stall_threshold_secs(
    history_secs: list[float],
    step_timeout_minutes: int | None,
    *,
    floor_minutes: int = STALL_FLOOR_MINUTES,
    factor: int = STALL_MEDIAN_FACTOR,
    timeout_grace_minutes: int = STALL_TIMEOUT_GRACE_MINUTES,
) -> float:
    terms = [floor_minutes * 60.0]
    if history_secs:
        terms.append(factor * _median(history_secs))
    if step_timeout_minutes:
        terms.append((step_timeout_minutes + timeout_grace_minutes) * 60.0)
    return max(terms)


def find_stall(
    jobs: list[dict],
    *,
    now: datetime,
    history,
    timeouts: dict[str, int],
    floor_minutes: int = STALL_FLOOR_MINUTES,
) -> Stall | None:
    """The first in-progress step that has run past its stall threshold.

    ``history(job_name, step_name)`` returns that step's durations in recent
    green runs; it is called only for a step already past the floor. A job
    with no step reported in progress is skipped (fail closed): without a
    step start there is no evidence it stopped moving.
    """
    for job in jobs:
        if job.get("status") != "in_progress":
            continue
        current = [s for s in job.get("steps") or [] if s.get("status") == "in_progress" and s.get("started_at")]
        if len(current) != 1:
            continue
        step = current[0]
        elapsed = (now - _parse(step["started_at"])).total_seconds()
        stall = _over_threshold(job, step, elapsed, history, timeouts, floor_minutes)
        if stall:
            return stall
    return None


def _over_threshold(job, step, elapsed, history, timeouts, floor_minutes) -> Stall | None:
    """A Stall when ``step`` ran past its threshold; None otherwise.

    Fail closed: a step with neither a known timeout-minutes nor any green
    history (renamed since the tag, say) is never selected; the job timeout
    still bounds it.
    """
    if elapsed <= floor_minutes * 60:
        return None
    durations = history(job["name"], step["name"])
    timeout = timeouts.get(step["name"])
    if not durations and not timeout:
        return None
    threshold = stall_threshold_secs(durations, timeout, floor_minutes=floor_minutes)
    if elapsed > threshold:
        return Stall(int(job["id"]), job["name"], step["name"], elapsed, threshold)
    return None


def find_cancelled_stall(
    jobs: list[dict],
    *,
    history,
    timeouts: dict[str, int],
    floor_minutes: int = STALL_FLOOR_MINUTES,
) -> Stall | None:
    """In a cancelled run, a cancelled step that had run past its stall threshold.

    That is the trace of a heal cancel whose re-run never happened (the pass
    died, or the run settled after the pass gave up), so a later pass can
    finish it. A run cancelled while its steps were moving shows no such step.
    """
    for job in jobs:
        if job.get("conclusion") != "cancelled":
            continue
        for step in job.get("steps") or []:
            if step.get("conclusion") != "cancelled" or not step.get("started_at") or not step.get("completed_at"):
                continue
            elapsed = (_parse(step["completed_at"]) - _parse(step["started_at"])).total_seconds()
            stall = _over_threshold(job, step, elapsed, history, timeouts, floor_minutes)
            if stall:
                return stall
    return None


def decide_resume(run: dict, stall: Stall | None, *, max_attempts: int = MAX_ATTEMPTS) -> Decision | None:
    """Re-run decision for a run a heal pass cancelled as stalled but never re-ran."""
    if stall is None or run.get("status") != "completed" or run.get("conclusion") != "cancelled":
        return None
    attempt = int(run.get("run_attempt") or 1)
    where = (f"job {stall.job_name!r} step {stall.step_name!r} ran "
             f"{int(stall.elapsed_secs // 60)} min before it was cancelled "
             f"(threshold {int(stall.threshold_secs // 60)} min)")
    if attempt >= max_attempts:
        return Decision("fail", f"release run {run['id']} was cancelled as stalled after {attempt} attempts; "
                        f"not retrying: {where}", run["id"])
    return Decision("resume-rerun", f"release run {run['id']} was cancelled as stalled and not re-run "
                    f"(attempt {attempt}/{max_attempts}): {where}", run["id"])


def decide_stall(run: dict, stall: Stall | None, *, max_attempts: int = MAX_ATTEMPTS) -> Decision | None:
    """Cancel-and-re-run decision for an in-progress run, or None to leave it."""
    if stall is None or run.get("status") != "in_progress":
        return None
    attempt = int(run.get("run_attempt") or 1)
    where = (f"job {stall.job_name!r} step {stall.step_name!r} in progress "
             f"{int(stall.elapsed_secs // 60)} min (threshold {int(stall.threshold_secs // 60)} min)")
    if attempt >= max_attempts:
        return Decision(
            "fail",
            f"release run {run['id']} stalled after {attempt} attempts; not cancelling: {where}",
            run["id"],
        )
    return Decision(
        "cancel-rerun",
        f"release run {run['id']} stalled (attempt {attempt}/{max_attempts}): {where}",
        run["id"],
    )


def cargo_version(cargo_toml: Path) -> str:
    text = cargo_toml.read_text(encoding="utf-8")
    m = re.search(r'(?ms)^\[package\].*?^\s*version\s*=\s*"([^"]+)"', text)
    if not m:
        raise SystemExit("cannot read [package] version from Cargo.toml")
    return m.group(1)


def _gh(*args: str) -> str:
    out = subprocess.run(["gh", *args], check=True, capture_output=True, text=True)
    return out.stdout


ALREADY_RUNNING = re.compile(r"already (running|in progress|queued)|currently running|in progress", re.I)


def rerun_refusal_is_benign(stderr: str) -> bool:
    """True only for GitHub refusing because the run is already running or queued.

    `gh api` prints the message and `(HTTP <status>)` on stderr. Only that
    refusal means another heal pass won the race; any other status (401, 403
    scope or permission errors, 404, 5xx) means heal cannot recover anything.
    """
    m = re.search(r"\(HTTP (\d{3})\)", stderr)
    status = int(m.group(1)) if m else None
    if status not in (403, 409, 422):
        return False
    if status == 403 and not re.search(r"already running", stderr, re.I):
        return False
    return bool(ALREADY_RUNNING.search(stderr))


def find_release(repo: str, tag: str) -> dict | None:
    """The release for `tag`, draft or published, scanning every listing page.

    The tag-lookup endpoint cannot see drafts, so this walks the list instead.
    """
    for page in range(1, MAX_RELEASE_PAGES + 1):
        releases = json.loads(_gh(
            "api", f"repos/{repo}/releases?per_page={RELEASE_PAGE_SIZE}&page={page}"))
        for release in releases:
            if release.get("tag_name") == tag:
                return release
        if len(releases) < RELEASE_PAGE_SIZE:
            return None
    raise SystemExit(f"{tag}: release not found in the first "
                     f"{MAX_RELEASE_PAGES * RELEASE_PAGE_SIZE} releases; refusing to guess")


def expected_assets(artifact_prefix: str = "shipyard") -> tuple[str, ...]:
    """The asset set release.yml must attach before it publishes, per CI_PLATFORM_FOCUS."""
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from release_macos_local import expected_release_assets

    return expected_release_assets(artifact_prefix)


def _failed_steps(repo: str, run_id: int) -> str:
    jobs = json.loads(_gh("api", f"repos/{repo}/actions/runs/{run_id}/jobs?per_page=100"))
    parts = []
    for job in jobs.get("jobs", []):
        if job.get("conclusion") in FAILED | {"cancelled"}:
            steps = [s["name"] for s in job.get("steps", []) if s.get("conclusion") in FAILED]
            parts.append(f"{job['name']} -> {', '.join(steps) or job['conclusion']}")
    return "; ".join(parts)


def _green_step_history(repo: str):
    """``history(job_name, step_name)`` over the last green release runs, fetched once."""
    cache: list[list[dict]] = []

    def history(job_name: str, step_name: str) -> list[float]:
        if not cache:
            runs = json.loads(_gh(
                "api", f"repos/{repo}/actions/workflows/release.yml/runs"
                f"?status=success&per_page={STALL_HISTORY_RUNS}")).get("workflow_runs", [])
            cache.append([
                job
                for green in runs[:STALL_HISTORY_RUNS]
                for job in json.loads(_gh(
                    "api", f"repos/{repo}/actions/runs/{green['id']}/jobs?per_page=100")).get("jobs", [])
            ])
        durations = []
        for job in cache[0]:
            if job.get("name") != job_name:
                continue
            for step in job.get("steps") or []:
                if (step.get("name") == step_name and step.get("conclusion") == "success"
                        and step.get("started_at") and step.get("completed_at")):
                    durations.append((_parse(step["completed_at"]) - _parse(step["started_at"])).total_seconds())
        return durations

    return history


def _run(repo: str, run_id: int) -> dict:
    return json.loads(_gh("api", f"repos/{repo}/actions/runs/{run_id}"))


def _wait_for(predicate, *, timeout: float, poll: float, sleep=None, clock=None) -> bool:
    # Resolved at call time, so a patched time.sleep reaches every caller.
    sleep = sleep or time.sleep
    clock = clock or time.monotonic
    deadline = clock() + timeout
    while True:
        if predicate():
            return True
        if clock() >= deadline:
            return False
        sleep(poll)


def _comment_on_blocked_issue(repo: str, body: str) -> None:
    """Best effort: a heal action must not fail because the comment could not post."""
    try:
        issues = json.loads(_gh("api", f"repos/{repo}/issues?labels={BLOCKED_LABEL}&state=open&per_page=5"))
        issues = [i for i in issues if "pull_request" not in i]
        if not issues:
            return
        _gh("api", "-X", "POST", f"repos/{repo}/issues/{issues[0]['number']}/comments", "-f", f"body={body}")
        print(f"commented on {BLOCKED_LABEL} issue #{issues[0]['number']}")
    except (subprocess.CalledProcessError, json.JSONDecodeError, KeyError) as exc:
        print(f"::warning::could not comment on the {BLOCKED_LABEL} issue: {exc}")


def _try_run(repo: str, run_id: int) -> dict | None:
    """The run, or None when the read fails; a poll must not abort the pass."""
    try:
        return _run(repo, run_id)
    except (subprocess.CalledProcessError, json.JSONDecodeError) as exc:
        print(f"::warning::could not read run {run_id}: {exc}")
        return None


def rerun_and_confirm(repo: str, tag: str, run_id: int, attempt: int, stall: Stall, *, what: str,
                      sleep=None) -> int:
    """Re-run a cancelled stalled run and confirm from the API that a new attempt exists.

    Re-run failed jobs first; GitHub does not document whether that endpoint
    includes cancelled jobs, so if no new attempt appears, re-run the stalled
    job and its dependents directly. An increased run_attempt alone is the
    proof: a new attempt that already failed fast is still a restart, and
    re-running it again would burn a second attempt.
    """
    def restarted() -> bool:
        latest = _try_run(repo, run_id)
        return latest is not None and int(latest.get("run_attempt") or 1) > attempt

    for endpoint in (f"repos/{repo}/actions/runs/{run_id}/rerun-failed-jobs",
                     f"repos/{repo}/actions/jobs/{stall.job_id}/rerun"):
        try:
            _gh("api", "-X", "POST", endpoint)
        except subprocess.CalledProcessError as exc:
            print(f"re-run via {endpoint} refused: {(exc.stderr or '').strip()}", file=sys.stderr)
            if restarted():
                break
            continue
        if _wait_for(restarted, timeout=RERUN_CONFIRM_SECS, poll=10, sleep=sleep):
            break
    else:
        print(f"::error title=Release {tag} heal could not restart run {run_id}::"
              "the stalled run is cancelled but no re-run started; the next heal pass tries again")
        return 1
    message = (f"release-heal: {what} release run {run_id} ({tag}): job "
               f"`{stall.job_name}` step `{stall.step_name}` had run "
               f"{int(stall.elapsed_secs // 60)} min, over its {int(stall.threshold_secs // 60)} min "
               f"threshold. Re-ran it as attempt {attempt + 1}/{MAX_ATTEMPTS}.")
    print(message)
    _comment_on_blocked_issue(repo, message)
    return 0


def cancel_and_rerun(repo: str, tag: str, run: dict, stall: Stall, *, sleep=None) -> int:
    """Cancel a stalled run, wait for it to settle, then re-run it.

    A normal cancel, never force-cancel: force-cancel skips the job's
    ``if: always()`` keychain restore. If this pass dies or gives up after the
    cancel, the next pass finds the cancelled stalled step
    (find_cancelled_stall) and re-runs it, so no cancel strands the release.
    """
    run_id = run["id"]
    attempt = int(run.get("run_attempt") or 1)
    try:
        _gh("api", "-X", "POST", f"repos/{repo}/actions/runs/{run_id}/cancel")
        print(f"cancel requested for run {run_id}")
    except subprocess.CalledProcessError as exc:
        detail = (exc.stderr or "").strip()
        m = re.search(r"\(HTTP (\d{3})\)", detail)
        if not (m and m.group(1) == "409"):
            # Not "already stopping": nothing was cancelled, so do not wait for it.
            print(f"::error title=Release {tag} heal could not cancel stalled run {run_id}::"
                  f"{detail or 'no detail'}")
            return 1
        # A 409 when the run is already stopping; the settle wait decides.
        print(f"cancel request for run {run_id} refused as already stopping: {detail}", file=sys.stderr)

    def settled() -> bool:
        latest = _try_run(repo, run_id)
        return latest is not None and latest.get("status") == "completed"

    if not _wait_for(settled, timeout=CANCEL_SETTLE_SECS, poll=CANCEL_POLL_SECS, sleep=sleep):
        print(f"::error title=Release {tag} heal cancelled a stalled run that has not stopped::"
              f"run {run_id} is not completed {CANCEL_SETTLE_SECS // 60} min after the cancel; "
              "the next heal pass re-runs it once it stops")
        return 1
    latest = _try_run(repo, run_id) or {}
    if latest.get("conclusion") != "cancelled":
        print(f"run {run_id} finished {latest.get('conclusion')} before the cancel took effect; "
              "the next heal pass judges it")
        return 0
    return rerun_and_confirm(repo, tag, run_id, attempt, stall, what="cancelled stalled", sleep=sleep)


def workflow_timeouts_at(repo: str, ref: str | None) -> dict[str, int] | None:
    """Step timeouts from release.yml at the run's own commit, or None if unreadable.

    A tag runs its own commit's workflow, so main's copy can name steps or
    limits the run does not have. None is not "no timeouts": without the
    limits a self-bounded step (a 25-minute notarization attempt) would be
    judged on the 20-minute floor alone, so the caller skips stall judgement.
    """
    if not ref:
        print("::warning::release run has no head_sha; skipping stall judgement this pass")
        return None
    try:
        text = _gh("api", f"repos/{repo}/contents/.github/workflows/release.yml?ref={ref}",
                   "-H", "Accept: application/vnd.github.raw+json")
    except (subprocess.CalledProcessError, OSError) as exc:
        print(f"::warning::could not read release.yml at {ref}; skipping stall judgement this pass: "
              f"{getattr(exc, 'stderr', '') or exc}")
        return None
    return step_timeouts(text)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""))
    ap.add_argument("--cargo-toml", default="Cargo.toml")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()
    tag = f"v{cargo_version(Path(args.cargo_toml))}"
    repo = args.repo
    signing = os.environ.get("CI_MACOS_SIGNING_ENABLED", "") == "true"

    release = find_release(repo, tag)
    runs = json.loads(_gh(
        "api", f"repos/{repo}/actions/workflows/release.yml/runs?branch={tag}&event=push&per_page=5"
    )).get("workflow_runs", [])
    run = runs[0] if runs else None
    unpublished = release is None or release.get("draft")
    steps = (_failed_steps(repo, run["id"])
             if unpublished and run and run.get("conclusion") in FAILED else "")
    decision = decide(release, run, now=datetime.now(timezone.utc), signing_enabled=signing,
                      failed_steps=steps, expected_assets=expected_assets())
    stall = None
    cancel_enabled = os.environ.get(CANCEL_STALLED_ENV, "") == "true"
    in_flight = run is not None and run.get("status") == "in_progress" and decision.action == "noop"
    # Only a heal pass cancels as stalled, and only with the variable on.
    cancelled = (run is not None and cancel_enabled and run.get("status") == "completed"
                 and run.get("conclusion") == "cancelled")
    if signing and unpublished and (in_flight or cancelled):
        assert run is not None
        jobs = json.loads(_gh("api", f"repos/{repo}/actions/runs/{run['id']}/jobs?per_page=100")).get("jobs", [])
        timeouts = workflow_timeouts_at(repo, run.get("head_sha"))
        history = _green_step_history(repo)
        if timeouts is None:
            pass  # unreadable limits: judge nothing, the next pass retries
        elif in_flight:
            stall = find_stall(jobs, now=datetime.now(timezone.utc), history=history, timeouts=timeouts)
            decision = decide_stall(run, stall) or decision
        else:
            stall = find_cancelled_stall(jobs, history=history, timeouts=timeouts)
            decision = decide_resume(run, stall) or decision
    print(f"{tag}: {decision.action}: {decision.reason}")
    if decision.action == "resume-rerun" and not args.dry_run:
        assert stall is not None and run is not None
        return rerun_and_confirm(repo, tag, run["id"], int(run.get("run_attempt") or 1), stall,
                                 what="re-ran the cancelled stalled")
    if decision.action == "cancel-rerun" and not args.dry_run:
        assert stall is not None and run is not None
        if not cancel_enabled:
            # Default off until the cancel path has run against the real
            # repository (docs/autonomy-contract.md); the stall is still loud.
            print(f"::error title=Release {tag} run stalled::{decision.reason}. "
                  f"Not cancelling: repository variable {CANCEL_STALLED_ENV} is not 'true'")
            return 1
        return cancel_and_rerun(repo, tag, run, stall)
    if decision.action == "rerun" and not args.dry_run:
        try:
            _gh("api", "-X", "POST", f"repos/{repo}/actions/runs/{decision.run_id}/rerun-failed-jobs")
        except subprocess.CalledProcessError as exc:
            detail = (exc.stderr or "").strip()
            if rerun_refusal_is_benign(detail):
                print(f"rerun request was refused, the run is already running: {detail}", file=sys.stderr)
                return 0
            m = re.search(r"\(HTTP (\d{3})\)", detail)
            status = m.group(1) if m else "unknown"
            print(f"::error title=Release {tag} heal could not re-run run {decision.run_id}::HTTP {status}: {detail}")
            print(f"rerun request failed (HTTP {status}): {detail}", file=sys.stderr)
            return 1
        print(f"re-ran failed jobs of run {decision.run_id}")
    if decision.action == "fail":
        print(f"::error title=Release {tag} stuck as draft::{decision.reason}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
