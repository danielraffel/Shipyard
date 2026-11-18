#!/usr/bin/env python3
"""Release watchdog: keep a release-chain failure visible in a tracking issue.

Port of Pulp's release-watchdog triad (Pulp docs/guides/release-watchdog.md);
see docs/release-watchdog.md. ``workflow-lint.yml`` is layer 1. This script is
layers 2 and 3:

``watchdog`` (``auto-release-watchdog.yml``) runs when ``version-at-land.yml``
or ``auto-release.yml`` completes. A failed run opens or updates that
workflow's tracker with the run, the error lines from its log and the fix for
errors it recognises; the next successful run closes it.

``chain`` (``release-cadence-check.yml``, every 30 minutes and whenever a
``release.yml`` or ``release-heal.yml`` run completes) follows the current
``Cargo.toml`` version on main to an installable release. Within
``--bound-minutes`` (90) of the version landing on main: the ``v*`` tag exists,
the tag's ``release.yml`` run succeeded (its publish step installs the tag with
install.sh on the fleet Mac and checks ``--version``), the release is public
with every expected asset, and ``releases/latest`` is that tag. Separately, a
``release/version-at-land`` pull request may not stay open past
``--bump-stale-minutes`` (60). It keeps one ``release-blocked`` issue whose
body carries a ``release-blocked-key`` marker; ``tools/metrics/release_notify.py``
on m3 messages the fleet's lead sessions when that key changes.

``cadence`` (``release-cadence-check.yml``) runs every 30 minutes and looks
only at outcomes, so it fires whatever broke. Two invariants:

- a ``feat:``/``fix:`` pull request merged to main more than ``--grace-hours``
  ago is in a ``v*`` tag, or a ``release/version-at-land`` pull request is open;
- a commit on main that moved the ``Cargo.toml`` package version is in a
  ``v*`` tag within ``--bump-grace-minutes``.

Decisions are pure functions; I/O is the thin layer at the bottom.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timedelta, timezone

DEFAULT_REPO = "danielraffel/Shipyard"
BUMP_BRANCH = "release/version-at-land"
# GitHub refuses a label without a colour, so each carries one.
LABEL_SPECS = {
    "ci": ("0E8A16", "Continuous integration and release automation"),
    "blocks-release": ("B60205", "Blocks the next release until resolved"),
    "release-stall": ("D93F0B", "Opened and closed by the release watchdog"),
    # tools/metrics/release_notify.py messages the lead sessions about every
    # open issue with this label, so every tracker carries it.
    "release-blocked": ("B60205", "A release is not reaching users; lead sessions are told"),
}
LABELS = tuple(LABEL_SPECS)
TRACKER_LABEL = "release-stall"
CADENCE_TITLE = "Release cadence: merged changes but no release"
CHAIN_TITLE = "Release blocked: the current version is not released and installable"
CHAIN_BOUND_MINUTES = 90.0
BUMP_STALE_MINUTES = 60.0
KEY_MARKER = "release-blocked-key"
# Which tracker a watched workflow owns, keyed by its `name:`.
WATCHED = {
    "Assign versions at land": "version-at-land",
    "Auto-Release on CLI Version Bump": "auto-release",
}
FAILED = {"failure", "timed_out", "startup_failure"}
RELEASE_TITLE = re.compile(r"^(feat|fix)(\([^)]*\))?!?:", re.I)
# Anchored so the echoed step script and its env listing never match.
EXCERPT = re.compile(
    r"^(version-at-land:|gh stderr:|gh response:|Release App token (cannot|is required|was rejected)|"
    r"subprocess\.CalledProcessError|remote: error|remote: - |error:|fatal:|##\[error\])")
TIMESTAMP = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z ?")


def watchdog_title(workflow: str) -> str:
    return f"{WATCHED.get(workflow, workflow)} workflow failed: RELEASES BLOCKED"


# ----------------------------------------------------------------- decisions

def classify(conclusion: str, jobs: int) -> str:
    """Pulp's run classes: workflow_file_error, job_failure, success, none."""
    if conclusion in FAILED and jobs == 0:
        return "workflow_file_error"
    if conclusion in FAILED:
        return "job_failure"
    if conclusion == "success":
        return "success"
    return "none"  # cancelled or skipped says nothing about the chain


def known_fix(text: str, repo: str = DEFAULT_REPO) -> str:
    """The exact fix for a release-chain error, from its log lines."""
    lowered = text.lower()
    if ("resource not accessible by integration" in lowered
            or "resource not accessible by personal access token" in lowered):
        return ("The release App token lacks a permission. The shipyard-local App "
                f"installation must grant {repo} Contents, Workflows, Pull requests: Read "
                "and write and Commit statuses: Read and write, and the workflow's token "
                "step must request them; then "
                f"`gh workflow run version-at-land.yml --repo {repo}`.")
    if "release app credentials are required" in lowered:
        return ("The release App private key is not set: "
                f"`gh secret set SHIPYARD_APP_PRIVATE_KEY --repo {repo} < <key.pem>`.")
    if ("bad credentials" in lowered or "http 401" in lowered or "could not read username" in lowered
            or "release app token was rejected" in lowered):
        return ("The release App token was rejected or not authorised for this repository. "
                "Check that SHIPYARD_APP_PRIVATE_KEY holds a current shipyard-local App "
                f"private key and that the App is installed on {repo}.")
    if "gh013" in lowered:
        return ("A repository rule refused a push. version-at-land pushes only "
                f"{BUMP_BRANCH}; read which ref the log names.")
    return "No known fix for this error; read the run log."


def log_excerpt(log: str, limit: int = 20) -> list[str]:
    """The lines of a job log that say what went wrong."""
    lines: list[str] = []
    for raw in log.splitlines():
        if "\x1b[36;1m" in raw:  # GitHub echoes the step's script in this colour
            continue
        line = TIMESTAMP.sub("", raw.split("\t")[-1].lstrip("\ufeff")).rstrip()
        if line and EXCERPT.match(line) and line not in lines:
            lines.append(line[:600])
    return lines[-limit:]


def watchdog_body(workflow: str, kind: str, run_url: str, sha: str, jobs: int,
                  excerpt: list[str], now: datetime) -> str:
    stamp = now.strftime("%Y-%m-%d %H:%M UTC")
    out = [f"_Auto-generated by `.github/workflows/auto-release-watchdog.yml` on {stamp}._", ""]
    if kind == "workflow_file_error":
        out += [f"**{workflow} was rejected at the workflow-file level**: GitHub dispatched no "
                "jobs, so there are no logs. Run `actionlint` on the workflow file and fix "
                "what it names.", ""]
        fix = "Fix the workflow file; `actionlint` names the error."
    else:
        out += [f"**The {workflow} run failed.**", "", "```",
                *(excerpt or ["(no error lines found in the job log)"]), "```", ""]
        fix = known_fix("\n".join(excerpt))
    out += [f"**Fix:** {fix}", "", f"**Run:** {run_url}", f"**Tip SHA:** `{sha}`",
            f"**Jobs dispatched:** {jobs}", "",
            f"_Releases are BLOCKED until {workflow} returns to green._",
            "_This tracker closes automatically on the next successful run._"]
    return "\n".join(out)


def untagged_releases(merged: list[dict], untagged: set[str], bump_pr_open: bool,
                      now: datetime, grace_hours: float) -> list[dict]:
    """feat/fix pull requests merged past the grace window whose merge is in no tag."""
    if bump_pr_open:
        return []  # the bump is waiting on review or checks, not stalled
    cutoff = now - timedelta(hours=grace_hours)
    return sorted((pr for pr in merged
                   if RELEASE_TITLE.match(pr.get("title") or "")
                   and pr.get("merge_commit_sha") in untagged
                   and _parse(pr["merged_at"]) <= cutoff),
                  key=lambda pr: pr["merged_at"])


def untagged_bumps(bumps: list[dict], now: datetime, grace_minutes: float) -> list[dict]:
    """Version moves on main past the grace window that their tag does not contain.

    Each bump is ``{sha, version, landed_at, tagged}``; ``tagged`` is whether
    any ``v*`` tag contains the commit.
    """
    cutoff = now - timedelta(minutes=grace_minutes)
    return [b for b in bumps if not b["tagged"] and _parse(b["landed_at"]) <= cutoff]


def cadence_body(stuck: list[dict], bumps: list[dict], now: datetime, repo: str) -> str:
    stamp = now.strftime("%Y-%m-%d %H:%M UTC")
    out = [f"_Auto-generated by `.github/workflows/release-cadence-check.yml` on {stamp}._", ""]
    if stuck:
        out += ["**Invariant violated:** these `feat:`/`fix:` pull requests merged to `main` "
                "but are in no release tag, and no `release/version-at-land` pull request "
                "is open to release them.", ""]
        out += [f"- #{pr['number']} merged {pr['merged_at']}: {pr['title']}" for pr in stuck[:20]]
        out += ["", "**Likely cause:** `version-at-land.yml` is failing (see its "
                "`version-at-land workflow failed` tracker) or its bump pull request was "
                "closed. **Fix:** read the latest *Assign versions at land* run, fix what it "
                f"names, then `gh workflow run version-at-land.yml --repo {repo}`.", ""]
    if bumps:
        out += ["**Invariant violated:** `main` moved the `Cargo.toml` version but no "
                "`v*` tag contains that commit.", ""]
        out += [f"- `v{b['version']}`: commit `{b['sha'][:12]}` landed {b['landed_at']}"
                for b in bumps]
        out += ["", "**Likely cause:** `auto-release.yml` failed (see its tracker). **Fix:** "
                "read the latest *Auto-Release on CLI Version Bump* run; re-run it once the "
                "cause is fixed.", ""]
    out.append("_This tracker closes automatically on the next sweep that finds every "
               "change released._")
    return "\n".join(out)


def chain_decide(*, version: str, landed_at: str | None, now: datetime, tag_exists: bool,
                 run: dict | None, failed_jobs: str, release: dict | None,
                 latest_tag: str | None, expected_assets: tuple[str, ...],
                 bump_pr: dict | None, bound_minutes: float = CHAIN_BOUND_MINUTES,
                 bump_stale_minutes: float = BUMP_STALE_MINUTES) -> dict:
    """Judge the current version's path from main to an installable release.

    Returns ``{"state": "ok"|"pending"|"failing", "failures": [(step, detail)],
    "note": str}``. ``landed_at`` is when the version reached main (None: long
    ago). ``run`` is the latest ``release.yml`` push run for the tag; its
    publish step already ran install.sh on the fleet Mac and compared
    ``--version`` with the tag, so its success covers the install. ``release``
    is the public release (None when missing or still a draft, which the
    default token cannot see). ``bump_pr`` is the open bump pull request, with
    ``number``, ``created_at`` and ``verdict`` (the review-verdict check).
    """
    tag = f"v{version}"
    failures: list[tuple[str, str]] = []
    if bump_pr:
        age = (now - _parse(bump_pr["created_at"])).total_seconds() / 60
        if age > bump_stale_minutes:
            failures.append(("bump_pr_stale",
                             f"bump pull request #{bump_pr['number']} has been open "
                             f"{age:.0f} min (limit {bump_stale_minutes:.0f}); "
                             f"review-verdict at its head: {bump_pr.get('verdict') or 'absent'}"))
    waited = (now - _parse(landed_at)).total_seconds() / 60 if landed_at else None
    if waited is not None and waited < bound_minutes:
        note = f"{tag} landed {waited:.0f} min ago; judged after {bound_minutes:.0f} min"
        return {"state": "failing" if failures else "pending", "failures": failures,
                "note": note}
    since = f"{waited:.0f} min after it landed" if waited is not None else "long after it landed"
    if not tag_exists:
        failures.append(("tag_missing", f"tag {tag} does not exist {since}"))
    elif run is None:
        failures.append(("release_run_missing", f"no release.yml push run for {tag} {since}"))
    elif run.get("status") != "completed":
        failures.append(("release_run_incomplete",
                         f"release.yml run {run.get('html_url', run.get('id'))} is still "
                         f"{run.get('status')} {since}"))
    elif run.get("conclusion") != "success":
        failures.append(("release_run_failed",
                         f"release.yml run {run.get('html_url', run.get('id'))} concluded "
                         f"{run.get('conclusion')}; failed: {failed_jobs or 'see the run'}"))
    if tag_exists:
        if release is None or release.get("draft"):
            failures.append(("release_unpublished", f"release {tag} is missing or still a draft"))
        else:
            have = {a.get("name") for a in release.get("assets", [])}
            missing = [name for name in expected_assets if name not in have]
            if missing:
                failures.append(("release_assets_missing",
                                 f"release {tag} lacks {', '.join(missing)}"))
            elif latest_tag != tag:
                failures.append(("latest_mismatch",
                                 f"releases/latest is {latest_tag or 'unreadable'}, not {tag}, "
                                 "so install.sh without SHIPYARD_VERSION installs another tag"))
    return {"state": "failing" if failures else "ok", "failures": failures,
            "note": f"{tag} judged {since}"}


def chain_key(version: str, failures: list[tuple[str, str]]) -> str:
    """What the notifier compares: a new version or a new failing step is news."""
    return f"v{version}:" + ",".join(step for step, _ in failures)


CHAIN_FIX = {
    "bump_pr_stale": "Read the bump pull request's checks and verdict; re-run a transient red "
                     "check. The steward merges it once review-verdict is green.",
    "tag_missing": "Read the latest *Auto-Release on CLI Version Bump* run (its tracker names "
                   "the error) and re-run it once fixed.",
    "release_run_missing": "The tag push started no release.yml run: "
                           "`gh workflow run release.yml --repo {repo} --ref {tag}`.",
    "release_run_incomplete": "A release job is queued or hung, usually on the self-hosted "
                              "macOS runner (`local-mac`, Shipyard-studio-*); check that runner.",
    "release_run_failed": "release-heal.yml re-runs failed jobs up to 3 times while the release "
                          "is a draft. Once that is spent, or if the release is already public, "
                          "fix the failing step and `gh run rerun <id> --failed --repo {repo}`.",
    "release_unpublished": "The release stays a draft until sign-and-upload-macos publishes it "
                           "after the install.sh check; read that job's log.",
    "release_assets_missing": "Re-run the job that uploads the missing asset.",
    "latest_mismatch": "Make {tag} the latest release (not a prerelease): "
                       "`gh release edit {tag} --latest --repo {repo}`.",
}


def chain_body(version: str, verdict: dict, evidence: list[str], now: datetime,
               repo: str) -> str:
    tag = f"v{version}"
    stamp = now.strftime("%Y-%m-%d %H:%M UTC")
    out = [f"<!-- {KEY_MARKER}: {chain_key(version, verdict['failures'])} -->",
           f"_Auto-generated by `release_watchdog.py chain` "
           f"(`.github/workflows/release-cadence-check.yml`) on {stamp}._", "",
           f"**{tag} is not released and installable.** ({verdict['note']})", ""]
    for step, detail in verdict["failures"]:
        out += [f"- **{step}**: {detail}",
                f"  - Fix: {CHAIN_FIX[step].format(repo=repo, tag=tag)}"]
    out += ["", "**Evidence:**", *[f"- {line}" for line in evidence], "",
            "_The lead sessions are messaged when the failing step changes. This issue "
            "closes on the next pass that finds the release public, complete and latest, "
            "with no stale bump pull request._"]
    return "\n".join(out)


def issue_action(open_issue: bool, failing: bool) -> str:
    """Pulp's tracker lifecycle: create, update, close or nothing."""
    if failing:
        return "update" if open_issue else "create"
    return "close" if open_issue else "none"


def _parse(ts: str) -> datetime:
    return datetime.fromisoformat(ts.replace("Z", "+00:00"))


# ----------------------------------------------------------------------- I/O

def run_gh(*args: str) -> subprocess.CompletedProcess[str]:
    """Every gh call goes through here (tests replace it)."""
    return subprocess.run([os.environ.get("RELEASE_WATCHDOG_GH", "gh"), *args],
                          capture_output=True, text=True)


def gh(*args: str, check: bool = True) -> str:
    result = run_gh(*args)
    if check and result.returncode != 0:
        raise SystemExit(f"gh {' '.join(args[:4])} failed (exit {result.returncode}): "
                         f"{result.stderr.strip()} {result.stdout.strip()}")
    return result.stdout


def git(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["git", *args], capture_output=True, text=True)


def find_tracker(repo: str, title: str) -> dict | None:
    """The open tracker with this exact title (never a duplicate).

    Found by label first; an unlabelled tracker (opened while its labels could
    not be created) is found by title among the open issues.
    """
    for query in (f"labels={TRACKER_LABEL}&state=open&per_page=100", "state=open&per_page=100"):
        issues = json.loads(gh("api", f"repos/{repo}/issues?{query}"))
        match = next((i for i in issues
                      if "pull_request" not in i and i.get("title") == title), None)
        if match:
            return match
    return None


def ensure_labels(repo: str) -> list[str]:
    """Create the tracker labels if missing; return the ones that exist.

    A 422 ``already_exists`` is success. Any other refusal is reported and that
    label is left off the issue rather than failing the issue.
    """
    usable = []
    for name, (color, description) in LABEL_SPECS.items():
        result = run_gh("api", "-X", "POST", f"repos/{repo}/labels", "-f", f"name={name}",
                        "-f", f"color={color}", "-f", f"description={description}")
        if result.returncode == 0 or "already_exists" in result.stdout + result.stderr:
            usable.append(name)
        else:
            print(f"release-watchdog: could not create label {name!r} "
                  f"(exit {result.returncode}): {result.stderr.strip()} {result.stdout.strip()}",
                  file=sys.stderr)
    return usable


def create_tracker(repo: str, title: str, body: str) -> list[str]:
    """Open the tracker; return the labels it carries.

    Labels are best-effort: if GitHub refuses the issue with them, it is
    opened without labels and the log says so.
    """
    labels = ensure_labels(repo)
    base = ["api", "-X", "POST", f"repos/{repo}/issues", "-f", f"title={title}",
            "-f", f"body={body}"]
    if labels:
        labelled = base + [arg for label in labels for arg in ("-f", f"labels[]={label}")]
        result = run_gh(*labelled)
        if result.returncode == 0:
            return labels
        print(f"release-watchdog: opening the issue with labels {labels} failed "
              f"(exit {result.returncode}): {result.stderr.strip()} {result.stdout.strip()}; "
              "opening it without labels", file=sys.stderr)
    gh(*base)
    if not labels:
        print("release-watchdog: opened the issue without labels", file=sys.stderr)
    return []


def apply_tracker(repo: str, title: str, failing: bool, body: str, close_note: str) -> str:
    issue = find_tracker(repo, title)
    action = issue_action(issue is not None, failing)
    if action == "create":
        create_tracker(repo, title, body)
    elif action == "update":
        gh("api", "-X", "PATCH", f"repos/{repo}/issues/{issue['number']}", "-f", f"body={body}")
    elif action == "close":
        gh("api", "-X", "POST", f"repos/{repo}/issues/{issue['number']}/comments",
           "-f", f"body={close_note}")
        gh("api", "-X", "PATCH", f"repos/{repo}/issues/{issue['number']}",
           "-f", "state=closed", "-f", "state_reason=completed")
    return action


def cargo_version_at(sha: str) -> str | None:
    text = git("show", f"{sha}:Cargo.toml").stdout
    m = re.search(r'(?ms)^\[package\].*?^\s*version\s*=\s*"([^"]+)"', text)
    return m.group(1) if m else None


def collect_bumps(since: datetime) -> list[dict]:
    log = git("log", "origin/main", "--first-parent", f"--since={since.isoformat()}",
              "--format=%H %cI", "--", "Cargo.toml").stdout.split("\n")
    bumps = []
    for line in filter(None, log):
        sha, landed = line.split(" ", 1)
        version = cargo_version_at(sha)
        if not version or version == cargo_version_at(f"{sha}^1"):
            continue
        # A bump superseded before it was tagged ships in the next tag, so any
        # v* tag containing the commit satisfies it.
        tagged = bool(git("tag", "--contains", sha, "--list", "v*").stdout.strip())
        bumps.append({"sha": sha, "version": version, "landed_at": landed, "tagged": tagged})
    return bumps


def run_watchdog(args: argparse.Namespace, now: datetime) -> int:
    jobs = json.loads(gh("api", f"repos/{args.repo}/actions/runs/{args.run_id}/jobs"))["jobs"]
    kind = classify(args.conclusion, len(jobs))
    print(f"{args.workflow} run {args.run_id} conclusion={args.conclusion} "
          f"jobs={len(jobs)} -> {kind}")
    if kind == "none":
        return 0
    excerpt: list[str] = []
    for job in jobs:
        if job.get("conclusion") in FAILED:
            excerpt += log_excerpt(gh("api", f"repos/{args.repo}/actions/jobs/{job['id']}/logs",
                                      check=False))
    body = watchdog_body(args.workflow, kind, args.run_url, args.sha, len(jobs), excerpt, now)
    if args.dry_run:
        print(body)
        return 0
    action = apply_tracker(args.repo, watchdog_title(args.workflow), kind != "success", body,
                           f"{args.workflow} returned to success on {args.run_url}. Closing.")
    print(f"tracker: {action}")
    return 0


def run_cadence(args: argparse.Namespace, now: datetime) -> int:
    since = now - timedelta(days=args.lookback_days)
    pulls = json.loads(gh("api", f"repos/{args.repo}/pulls?state=closed&base=main"
                                 "&sort=updated&direction=desc&per_page=100"))
    merged = [p for p in pulls if p.get("merged_at") and _parse(p["merged_at"]) >= since]
    untagged = set()
    for pr in merged:
        sha = pr.get("merge_commit_sha") or ""
        if (sha and git("merge-base", "--is-ancestor", sha, "origin/main").returncode == 0
                and not git("tag", "--contains", sha, "--list", "v*").stdout.strip()):
            untagged.add(sha)
    owner = args.repo.split("/", 1)[0]
    bump_prs = json.loads(gh("api", f"repos/{args.repo}/pulls?state=open&base=main"
                                    f"&head={owner}:{BUMP_BRANCH}"))
    stuck = untagged_releases(merged, untagged, bool(bump_prs), now, args.grace_hours)
    bumps = untagged_bumps(collect_bumps(since), now, args.bump_grace_minutes)
    print(json.dumps({"untagged_merges": [p["number"] for p in stuck],
                      "untagged_bumps": [b["version"] for b in bumps]}))
    body = cadence_body(stuck, bumps, now, args.repo)
    if args.dry_run:
        print(body)
        return 0
    action = apply_tracker(args.repo, CADENCE_TITLE, bool(stuck or bumps), body,
                           f"Sweep on {now:%Y-%m-%d %H:%M} UTC found every change released. "
                           "Closing.")
    print(f"tracker: {action}")
    return 0


def latest_bump_landing(version: str, lookback_days: float, now: datetime) -> str | None:
    """When ``version`` reached main, or None when that was before the lookback."""
    for bump in reversed(collect_bumps(now - timedelta(days=lookback_days))):
        if bump["version"] == version:
            return bump["landed_at"]
    return None


def verdict_at(repo: str, sha: str) -> str:
    runs = json.loads(gh("api", f"repos/{repo}/commits/{sha}/check-runs?check_name=review-verdict",
                         check=False) or "{}").get("check_runs", [])
    if not runs:
        return "absent"
    run = runs[0]
    return run.get("conclusion") or run.get("status") or "unknown"


def optional_json(*args: str) -> dict | None:
    """A GET that may 404 (a draft or missing release reads as None)."""
    result = run_gh(*args)
    if result.returncode != 0:
        if "404" in result.stderr + result.stdout:
            return None
        raise SystemExit(f"gh {' '.join(args[:4])} failed (exit {result.returncode}): "
                         f"{result.stderr.strip()} {result.stdout.strip()}")
    return json.loads(result.stdout)


def run_chain(args: argparse.Namespace, now: datetime) -> int:
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    from release_draft_heal import expected_assets  # noqa: PLC0415 (script sibling)

    version = cargo_version_at(args.ref)
    if not version:
        raise SystemExit(f"cannot read the Cargo.toml version at {args.ref}")
    tag = f"v{version}"
    landed = latest_bump_landing(version, args.lookback_days, now)
    tag_exists = git("rev-parse", "-q", "--verify", f"refs/tags/{tag}").returncode == 0
    runs = json.loads(gh("api", f"repos/{args.repo}/actions/workflows/release.yml/runs"
                                f"?branch={tag}&event=push&per_page=1")).get("workflow_runs", [])
    run = runs[0] if runs else None
    failed_jobs = ""
    if run and run.get("status") == "completed" and run.get("conclusion") != "success":
        jobs = json.loads(gh("api", f"repos/{args.repo}/actions/runs/{run['id']}/jobs"
                                    "?per_page=100")).get("jobs", [])
        failed_jobs = "; ".join(
            f"{j['name']} -> " + (", ".join(s["name"] for s in j.get("steps", [])
                                            if s.get("conclusion") in FAILED) or j["conclusion"])
            for j in jobs if j.get("conclusion") in FAILED | {"cancelled"})
    release = optional_json("api", f"repos/{args.repo}/releases/tags/{tag}")
    latest = (optional_json("api", f"repos/{args.repo}/releases/latest") or {}).get("tag_name")
    owner = args.repo.split("/", 1)[0]
    bump_prs = json.loads(gh("api", f"repos/{args.repo}/pulls?state=open&base=main"
                                    f"&head={owner}:{BUMP_BRANCH}"))
    bump_pr = None
    if bump_prs:
        pr = bump_prs[0]
        bump_pr = {"number": pr["number"], "created_at": pr["created_at"],
                   "verdict": verdict_at(args.repo, pr["head"]["sha"])}
    verdict = chain_decide(version=version, landed_at=landed, now=now, tag_exists=tag_exists,
                           run=run, failed_jobs=failed_jobs, release=release, latest_tag=latest,
                           expected_assets=expected_assets(), bump_pr=bump_pr,
                           bound_minutes=args.bound_minutes,
                           bump_stale_minutes=args.bump_stale_minutes)
    evidence = [f"version on main: {tag}, landed {landed or 'before the lookback window'}",
                f"tag {tag}: {'present' if tag_exists else 'absent'}",
                f"release.yml run: {run['html_url'] + ' ' + str(run.get('status')) + '/' + str(run.get('conclusion')) if run else 'none'}",
                f"release: {release['html_url'] + (' draft' if release.get('draft') else ' public') if release else 'not public'}",
                f"releases/latest: {latest or 'unreadable'}",
                f"bump pull request: {'#' + str(bump_pr['number']) + ' opened ' + bump_pr['created_at'] + ', review-verdict ' + bump_pr['verdict'] if bump_pr else 'none open'}"]
    print(json.dumps({"state": verdict["state"], "version": tag,
                      "failures": verdict["failures"], "note": verdict["note"]}))
    if verdict["state"] == "pending":
        print("chain: pending, tracker untouched")
        return 0
    body = chain_body(version, verdict, evidence, now, args.repo)
    if args.dry_run:
        print(body)
        return 0
    action = apply_tracker(args.repo, CHAIN_TITLE, verdict["state"] == "failing", body,
                           f"Pass on {now:%Y-%m-%d %H:%M} UTC found {tag} public, complete and "
                           "latest, with no stale bump pull request. Closing.")
    print(f"tracker: {action}")
    return 0


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", DEFAULT_REPO))
    ap.add_argument("--dry-run", action="store_true", help="print, touch no issue")
    sub = ap.add_subparsers(dest="check", required=True)
    w = sub.add_parser("watchdog")
    w.add_argument("--workflow", required=True, help="the watched workflow's name")
    w.add_argument("--run-id", required=True)
    w.add_argument("--run-url", required=True)
    w.add_argument("--sha", default="")
    w.add_argument("--conclusion", required=True)
    c = sub.add_parser("cadence")
    c.add_argument("--grace-hours", type=float, default=2.0)
    c.add_argument("--bump-grace-minutes", type=float, default=30.0)
    c.add_argument("--lookback-days", type=float, default=7.0)
    ch = sub.add_parser("chain")
    ch.add_argument("--bound-minutes", type=float, default=CHAIN_BOUND_MINUTES)
    ch.add_argument("--bump-stale-minutes", type=float, default=BUMP_STALE_MINUTES)
    ch.add_argument("--lookback-days", type=float, default=14.0)
    ch.add_argument("--ref", default="origin/main",
                    help="judge the version at this ref (a known-good tag is a control)")
    args = ap.parse_args(argv)
    now = datetime.now(timezone.utc)
    runner = {"watchdog": run_watchdog, "cadence": run_cadence, "chain": run_chain}[args.check]
    return runner(args, now)


if __name__ == "__main__":
    sys.exit(main())
