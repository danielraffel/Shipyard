#!/usr/bin/env python3
"""Recover a release that stayed a draft because its release run failed.

`release.yml` keeps a release a draft until the macOS DMG is signed, attested
and published by the final job. If that run ends in failure (a runner that
cannot reach the Rust distribution servers, a Sigstore blip) nothing retries
it and the draft sits forever. This script runs from `release-heal.yml` on a
schedule and on every main push. For the release of the current Cargo version
it either does nothing (the release is published with every expected asset, even
when its run failed and the release was recovered by hand), re-runs the failed jobs of the release run (bounded by
MAX_ATTEMPTS), or exits non-zero with the exact reason. The attempt cap means an exhausted run fails every scheduled pass until a
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


@dataclass(frozen=True)
class Decision:
    action: str  # "noop" | "rerun" | "fail"
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
    print(f"{tag}: {decision.action}: {decision.reason}")
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
