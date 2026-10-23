#!/usr/bin/env python3
"""Release-gate live smoke: run a built Shipyard binary's bulk GitHub reads
against real repositories before the release is published.

v0.302.0 to v0.307.0 shipped a `runner admission-clean` whose open-PR census
GitHub refused on every call (a nested connection exceeded the 500,000-node
limit). Unit tests against fakes could not see it; one read against a real
repository would have. This script is that read, and the release fails if it
does not succeed.

Every probe is read-only: `runner admission-clean` without `--apply` and
`runner steward` in its default dry-run. Each runs in `--mode isolated` under a
throwaway HOME so a production config, ledger or lock is never touched.

Pass criteria per repository:
- `runner admission-clean`: exit 0 with verdict `admit`, or exit 3 with
  verdict `defer` (both prove GitHub was observed). Anything else fails,
  including `error` / `observation_failed`.
- `runner steward` (dry-run): exit 0 and no per-repository `errors`.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

DEFAULT_REPOS = ("Generous-Corp/pulp",)
DEFAULT_LABELS = "self-hosted,macos,arm64"
PROBE_TIMEOUT_SECONDS = 300
PROBE_ATTEMPTS = 3
PROBE_RETRY_DELAYS_SECONDS = (1.0, 3.0)
# Defer reasons admission-clean returns before any GitHub read (a contended
# lock). They prove nothing about whether the binary can observe GitHub.
UNOBSERVED_DEFER_REASONS = frozenset({"observation_in_progress", "stewardship_in_progress"})


@dataclass
class ProbeResult:
    name: str
    repo: str
    ok: bool
    detail: str


def _last_json(text: str) -> dict | None:
    for line in reversed(text.strip().splitlines()):
        line = line.strip()
        if line.startswith("{"):
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict):
                return value
    try:
        value = json.loads(text)
    except json.JSONDecodeError:
        return None
    return value if isinstance(value, dict) else None


def judge_admission(repo: str, code: int, stdout: str, stderr: str) -> ProbeResult:
    document = _last_json(stdout) or {}
    verdict = document.get("verdict")
    reason = document.get("reason")
    named = str(document.get("repo") or "").lower() == repo.lower()
    ok = named and (
        (code, verdict) == (0, "admit")
        or ((code, verdict) == (3, "defer") and reason not in UNOBSERVED_DEFER_REASONS and reason)
    )
    ok = bool(ok)
    detail = (
        f"exit {code} verdict {verdict} reason {reason} repo {document.get('repo')}"
        + (f": {document.get('error')}" if document.get("error") else "")
        + ("" if document else f": {stderr.strip()[:600]}")
    )
    return ProbeResult("admission-clean", repo, ok, detail)


def judge_steward(repo: str, code: int, stdout: str, stderr: str) -> ProbeResult:
    try:
        document = json.loads(stdout)
    except json.JSONDecodeError:
        document = None
    errors: list[str] = []
    named = False
    if isinstance(document, dict):
        for entry in document.get("repos") or []:
            if not isinstance(entry, dict):
                continue
            named = named or str(entry.get("repo") or "").lower() == repo.lower()
            errors.extend(str(error) for error in entry.get("errors") or [])
    if isinstance(document, dict) and not named:
        errors.append(f"steward reported no entry for {repo}")
    ok = code == 0 and isinstance(document, dict) and not errors
    if ok:
        detail = "exit 0, no repository errors"
    elif document is None:
        detail = f"exit {code}, no JSON: {stderr.strip()[:600]}"
    else:
        detail = f"exit {code}: " + "; ".join(error[:600] for error in errors)
    return ProbeResult("steward (dry-run)", repo, ok, detail)


def _retryable_probe_failure(code: int, stdout: str, stderr: str) -> bool:
    """Recognize transport/transient observation failures, not bad binaries."""
    text = f"{stdout}\n{stderr}".lower()
    return any(
        marker in text
        for marker in (
            "http 429",
            "http 500",
            "http 502",
            "http 503",
            "http 504",
            "bad gateway",
            "unexpected end of json input",
            "connection reset",
            "connection refused",
            "timed out",
            "timeout",
        )
    )


def run_probes(
    binary: str,
    repos: list[str],
    base: str,
    labels: str,
    run=subprocess.run,
    sleep=time.sleep,
) -> list[ProbeResult]:
    results: list[ProbeResult] = []
    with tempfile.TemporaryDirectory(prefix="shipyard-release-smoke-") as home:
        env = dict(os.environ)
        # Keep the caller's GitHub CLI login reachable under the throwaway
        # HOME; GH_TOKEN (as CI sets it) wins over it either way.
        if "GH_CONFIG_DIR" not in env and "GH_TOKEN" not in env and "GITHUB_TOKEN" not in env:
            original = Path(env.get("HOME", str(Path.home()))) / ".config" / "gh"
            if original.is_dir():
                env["GH_CONFIG_DIR"] = str(original)
        env["HOME"] = home
        env.pop("XDG_CONFIG_HOME", None)
        env.pop("XDG_STATE_HOME", None)
        for repo in repos:
            probes = (
                (
                    [binary, "--mode", "isolated", "--json", "runner", "admission-clean",
                     "--repo", repo, "--base", base, "--labels", labels],
                    judge_admission,
                ),
                (
                    [binary, "--mode", "isolated", "--json", "runner", "steward",
                     "--repo", repo, "--base", base],
                    judge_steward,
                ),
            )
            for argv, judge in probes:
                assert "--apply" not in argv
                for attempt in range(PROBE_ATTEMPTS):
                    try:
                        completed = run(
                            argv,
                            env=env,
                            capture_output=True,
                            text=True,
                            timeout=PROBE_TIMEOUT_SECONDS,
                            check=False,
                        )
                    except subprocess.TimeoutExpired:
                        if attempt + 1 < PROBE_ATTEMPTS:
                            sleep(PROBE_RETRY_DELAYS_SECONDS[attempt])
                            continue
                        results.append(
                            ProbeResult(
                                argv[5],
                                repo,
                                False,
                                f"timed out after {PROBE_TIMEOUT_SECONDS}s ({PROBE_ATTEMPTS} attempts)",
                            )
                        )
                        break
                    result = judge(repo, completed.returncode, completed.stdout, completed.stderr)
                    if result.ok or not _retryable_probe_failure(
                        completed.returncode, completed.stdout, completed.stderr
                    ):
                        results.append(result)
                        break
                    if attempt + 1 < PROBE_ATTEMPTS:
                        sleep(PROBE_RETRY_DELAYS_SECONDS[attempt])
                    else:
                        results.append(
                            ProbeResult(
                                result.name,
                                result.repo,
                                result.ok,
                                f"{result.detail} (after {PROBE_ATTEMPTS} attempts)",
                            )
                        )
    return results


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--binary", required=True, help="Built shipyard binary to smoke")
    parser.add_argument(
        "--repo",
        action="append",
        dest="repos",
        help=f"Repository to read (repeatable; default {', '.join(DEFAULT_REPOS)})",
    )
    parser.add_argument("--base", default="main")
    parser.add_argument("--labels", default=DEFAULT_LABELS)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None, run=subprocess.run, sleep=time.sleep) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    binary = str(Path(args.binary).resolve())
    repos = args.repos or list(DEFAULT_REPOS)
    results = run_probes(binary, repos, args.base, args.labels, run=run, sleep=sleep)
    failed = [result for result in results if not result.ok]
    for result in results:
        mark = "ok  " if result.ok else "FAIL"
        print(f"[release-live-smoke] {mark} {result.name} {result.repo}: {result.detail}")
    if failed:
        print(
            f"[release-live-smoke] {len(failed)} of {len(results)} live probes failed; "
            "the release must not publish a binary that cannot observe GitHub.",
            file=sys.stderr,
        )
        return 1
    print(f"[release-live-smoke] all {len(results)} live probes passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
