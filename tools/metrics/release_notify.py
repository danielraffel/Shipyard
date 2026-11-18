#!/usr/bin/env python3
"""Release notify: tell the lead sessions when a release is blocked.

Run as `python3 -I tools/metrics/release_notify.py --tick`; the steward tick on
m3 starts it detached every tick and it acts at most every `--interval`
seconds. Standard library only. See docs/release-watchdog.md.

The release watchdog (`scripts/release_watchdog.py`, run by GitHub Actions)
keeps every release-chain problem in an open issue labelled `release-blocked`.
An issue alone wakes nobody, so this reads the open ones with one `ghapp api`
call and sends a cmux agent message to every target in the targets file:

- when an issue opens, or its `release-blocked-key` marker (the version and
  the failing steps) changes;
- once more when an issue it reported is no longer open (fixed or superseded).

A delivery that fails is retried on the next pass. After three consecutive
failures to one target it comments on the issue instead, logs one line, and
stops; if even the comment cannot be posted it stops after 24 hours. The tick
never waits on this and it exits 0.

Targets file (`--targets`, else ~/.config/shipyard/release-notify-targets,
else release-notify-targets.txt next to this script), one per line:

    <host> <cmux target> [<cmux socket>]

`<host>` is `local` or an ssh host; `<cmux target>` is a stable id (a
surface or agent UUID), never a `surface:N` ref, which changes after a reboot. Values may only use [A-Za-z0-9._:/@+-]
and may not start with "-", so nothing reaches a remote shell unquoted or is
read as an option.
"""
from __future__ import annotations

import argparse
import datetime as dt
import fcntl
import json
import os
import re
import subprocess
import sys
import time

REPO = "danielraffel/Shipyard"
LABEL = "release-blocked"
SENDER = "shipyard-release-watch"
# No leading "-": a field must never be read as an ssh or cmux option.
SAFE = re.compile(r"^[A-Za-z0-9._:/@+][A-Za-z0-9._:/@+-]*$")
KEY = re.compile(r"<!--\s*release-blocked-key:\s*(.*?)\s*-->")
HERE = os.path.dirname(os.path.abspath(__file__))
FALLBACK_AFTER = 3  # consecutive failed sends before the issue comment
GIVE_UP_AFTER = 24 * 3600  # stop retrying a message nobody can receive


def home(*p: str) -> str:
    return os.path.join(os.path.expanduser("~"), *p)


# ----------------------------------------------------------------- decisions

def parse_targets(text: str) -> tuple[list[dict], list[str]]:
    """Targets and the lines refused (unsafe or malformed)."""
    targets, refused = [], []
    for raw in text.splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        parts = line.split()
        if len(parts) not in (2, 3) or not all(SAFE.match(p) for p in parts):
            refused.append(line)
            continue
        targets.append({"host": parts[0], "target": parts[1],
                        "socket": parts[2] if len(parts) == 3 else ""})
    return targets, refused


def issue_key(issue: dict) -> str:
    m = KEY.search(issue.get("body") or "")
    return f"#{issue['number']}:{m.group(1) if m else 'open'}"


def summary_lines(body: str, limit: int = 6) -> list[str]:
    """The failing-step bullets of a tracker body, else its first prose lines."""
    lines = [ln.strip() for ln in (body or "").splitlines()]
    steps = [ln for ln in lines if ln.startswith("- **")]
    if steps:
        return steps[:limit]
    return [ln for ln in lines if ln and not ln.startswith(("<!--", "_"))][:limit]


def plan(issues: list[dict], state: dict, targets: list[dict]) -> list[dict]:
    """Messages owed: one per (event, target) not yet delivered."""
    out = []
    reported = state.get("reported", {})
    open_numbers = {str(i["number"]) for i in issues}
    for issue in issues:
        key = issue_key(issue)
        text = "\n".join([
            f"Release blocked: {issue.get('title', '')}",
            issue.get("html_url", ""),
            *summary_lines(issue.get("body", "")),
            "Next: read the issue, fix the named step, re-run the failed job. "
            "It closes itself on the next green pass.",
        ])
        out += [{"key": key, "number": str(issue["number"]), "text": text, "to": t}
                for t in targets]
    for number, last in reported.items():
        if number in open_numbers:
            continue
        key = f"#{number}:resolved"
        text = (f"Release issue closed (fixed or superseded): {last.get('title', '')} "
                f"(#{number}). The watchdog judges the next version on its own.")
        out += [{"key": key, "number": number, "text": text, "to": t, "resolved": True}
                for t in targets]
    delivered = state.get("delivered", {})
    return [m for m in out if dest(m["to"]) not in delivered.get(m["key"], [])]


def dest(t: dict) -> str:
    return f"{t['host']}/{t['target']}/{t['socket']}"


def command(t: dict, cmux: str = "cmux") -> list[str]:
    cmd = [cmux]
    if t["socket"]:
        cmd += ["--socket", t["socket"]]
    cmd += ["agent", "message", t["target"], "--from", SENDER, "--", "-"]
    if t["host"] == "local":
        return cmd
    return ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "--", t["host"], *cmd]


def record(state: dict, msg: dict, issues: list[dict]) -> None:
    state.setdefault("delivered", {}).setdefault(msg["key"], []).append(dest(msg["to"]))
    reported = state.setdefault("reported", {})
    if msg.get("resolved"):
        return
    title = next((i.get("title", "") for i in issues if str(i["number"]) == msg["number"]), "")
    reported[msg["number"]] = {"title": title}


def prune(state: dict, issues: list[dict], targets: list[dict]) -> None:
    """Forget a closed issue once every target has its resolved message."""
    open_numbers = {str(i["number"]) for i in issues}
    want = {dest(t) for t in targets}
    for number in list(state.get("reported", {})):
        key = f"#{number}:resolved"
        if number not in open_numbers and want <= set(state.get("delivered", {}).get(key, [])):
            del state["reported"][number]
            for k in [k for k in state.get("delivered", {}) if k.startswith(f"#{number}:")]:
                del state["delivered"][k]


# ----------------------------------------------------------------------- I/O

def load_state(path: str) -> dict:
    try:
        with open(path) as fh:
            return json.load(fh)
    except (OSError, ValueError):
        return {}


def save_state(path: str, state: dict) -> None:
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    tmp = f"{path}.tmp.{os.getpid()}"
    with open(tmp, "w") as fh:
        json.dump(state, fh, indent=1, sort_keys=True)
    os.replace(tmp, path)


def read_targets(path: str | None) -> tuple[list[dict], list[str], str]:
    for candidate in (path, home(".config", "shipyard", "release-notify-targets"),
                      os.path.join(HERE, "release-notify-targets.txt")):
        if candidate and os.path.isfile(candidate):
            with open(candidate) as fh:
                targets, refused = parse_targets(fh.read())
            return targets, refused, candidate
    return [], [], ""


def open_issues(ghapp: str, repo: str) -> list[dict]:
    out = subprocess.run([ghapp, "api", f"repos/{repo}/issues?labels={LABEL}&state=open&per_page=50"],
                         capture_output=True, text=True, timeout=60)
    if out.returncode != 0:
        raise RuntimeError(f"ghapp exit {out.returncode}: {out.stderr.strip()[:160]}")
    return [i for i in json.loads(out.stdout) if "pull_request" not in i]


def send(msg: dict, cmux: str) -> tuple[bool, str]:
    try:
        out = subprocess.run(command(msg["to"], cmux), input=msg["text"], capture_output=True,
                             text=True, timeout=30)
    except (OSError, subprocess.TimeoutExpired) as exc:
        return False, str(exc)[:160]
    return out.returncode == 0, (out.stderr or out.stdout).strip()[:160]


def comment(ghapp: str, repo: str, number: str, body: str) -> tuple[bool, str]:
    try:
        out = subprocess.run([ghapp, "api", "-X", "POST",
                              f"repos/{repo}/issues/{number}/comments", "-f", f"body={body}"],
                             capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.TimeoutExpired) as exc:
        return False, str(exc)[:160]
    return out.returncode == 0, (out.stderr or out.stdout).strip()[:160]


def on_failure(state: dict, msg: dict, issues: list[dict], detail: str, now: float,
               stamp: str, a: argparse.Namespace) -> list[str]:
    """Count consecutive failures for this message and target. After
    FALLBACK_AFTER, say it on the issue instead (one comment) and stop; if even
    that fails for GIVE_UP_AFTER seconds, stop anyway. One log line each."""
    fails = state.setdefault("failures", {})
    k = f"{msg['key']}@{dest(msg['to'])}"
    f = fails.setdefault(k, {"count": 0, "first": now})
    f["count"] += 1
    if f["count"] < FALLBACK_AFTER:
        return []
    who = dest(msg["to"])
    ok, why = comment(a.ghapp, a.repo, msg["number"],
                      f"release-notify could not reach `{who}` over cmux after {f['count']} "
                      f"tries ({detail}). The message it carried:\n\n{msg['text']}\n\n"
                      "Fix the target in tools/metrics/release-notify-targets.txt or the "
                      "host's ~/.config/shipyard/release-notify-targets.")
    if ok:
        record(state, msg, issues)
        del fails[k]
        return [f"{stamp} release-notify ERROR {who} unreachable after {FALLBACK_AFTER} tries "
                f"({detail}); commented on #{msg['number']} instead"]
    if now - f["first"] >= GIVE_UP_AFTER:
        record(state, msg, issues)
        del fails[k]
        return [f"{stamp} release-notify ERROR gave up on {msg['key']} to {who} after 24 h: "
                f"send {detail}; comment {why}"]
    return []


def run(a: argparse.Namespace, now: float | None = None) -> list[str]:
    """One pass. Returns the summary lines it wrote (or would write)."""
    now = time.time() if now is None else now
    stamp = dt.datetime.fromtimestamp(now, dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    state = load_state(a.state)
    if not a.force and now - state.get("last_run", 0) < a.interval:
        return []
    state["last_run"] = now
    lines = []
    targets, refused, source = read_targets(a.targets)
    for line in refused:
        lines.append(f"{stamp} release-notify ERROR refused target line: {line[:80]}")
    try:
        issues = open_issues(a.ghapp, a.repo)
    except (RuntimeError, OSError, ValueError, subprocess.TimeoutExpired) as exc:
        lines.append(f"{stamp} release-notify ERROR read: {str(exc)[:160]}")
        issues = None
    if issues is not None:
        if not targets and issues:
            lines.append(f"{stamp} release-notify ERROR no targets file; "
                         f"{len(issues)} release-blocked issue(s) unannounced")
        owed = plan(issues, state, targets)
        sent = failed = 0
        for msg in owed:
            if a.dry_run:
                lines.append(f"{stamp} release-notify would send {msg['key']} to {dest(msg['to'])}")
                continue
            ok, detail = send(msg, a.cmux)
            if ok:
                record(state, msg, issues)
                state.get("failures", {}).pop(f"{msg['key']}@{dest(msg['to'])}", None)
                sent += 1
            else:
                failed += 1
                lines += on_failure(state, msg, issues, detail, now, stamp, a)
        if not a.dry_run:
            prune(state, issues, targets)
            # A count belongs to a message still owed; a superseded key or a
            # closed issue must not carry old failures into a later message.
            owed_now = {f"{m['key']}@{dest(m['to'])}" for m in plan(issues, state, targets)}
            state["failures"] = {k: v for k, v in state.get("failures", {}).items()
                                 if k in owed_now}
        if sent or failed:
            lines.append(f"{stamp} release-notify sent={sent} failed={failed} "
                         f"open={len(issues)} targets={source or 'none'}")
    if not a.dry_run:
        save_state(a.state, state)
        if lines:
            with open(a.summary, "a") as fh:
                fh.write("\n".join(lines) + "\n")
    return lines


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tick", action="store_true", help="steward-tick mode: always exit 0")
    ap.add_argument("--log-dir", default=home("Library", "Logs", "shipyard-steward-tick"))
    ap.add_argument("--state")
    ap.add_argument("--summary")
    ap.add_argument("--targets")
    ap.add_argument("--repo", default=REPO)
    ap.add_argument("--interval", type=float, default=600)
    ap.add_argument("--ghapp", default=os.environ.get("RELEASE_NOTIFY_GHAPP", "ghapp"))
    ap.add_argument("--cmux", default=os.environ.get("RELEASE_NOTIFY_CMUX", "cmux"))
    ap.add_argument("--force", action="store_true", help="ignore --interval")
    ap.add_argument("--dry-run", action="store_true", help="print what would be sent, change nothing")
    a = ap.parse_args(argv)
    a.state = a.state or os.path.join(a.log_dir, "release-notify-state.json")
    a.summary = a.summary or os.path.join(a.log_dir, "summary.log")
    lock_path = os.path.join(a.log_dir, "release-notify.lock")
    try:
        os.makedirs(a.log_dir, exist_ok=True)
        with open(lock_path, "w") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except OSError:
                return 0  # another pass is running
            for line in run(a):
                print(line)
    except Exception as exc:  # the tick must never fail on this
        if not a.tick:
            raise
        print(f"release-notify ERROR {exc!r}"[:200], file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
