#!/usr/bin/env python3
"""Input events for `shipyard pr-watch rules --input-events`, from sources GitHub does not hold.

Three readers, each printing one JSON line per condition (`kind`, `key`,
`at`, `summary`, and a `fix` object), and nothing when there is nothing to
act on. The watcher emits each `key` once, so a reader may print a standing
condition on every pass.

``cmux``
    Reads the last lines of every cmux workspace (``cmux read-screen``,
    read-only; it never types). It reports a Codex session that exited
    (``codex_session_dead``), one idle on an error (``codex_session_errored``),
    an unsent draft in an idle session (``codex_draft_unsent``), and a draft
    waiting for "tab to queue" while the session works
    (``codex_draft_queued``).

``hosts``
    Runs ``tartci pool status --json`` on each gate host over ssh
    (``BatchMode``, read-only). It reports a host out of the pool
    (``host_out_of_pool``), each problem tartci itself names
    (``host_problem``), and an unreachable host (``host_unreachable``).

``gate-serving``
    Reads the gate workflow's jobs from GitHub. It reports a self-hosted gate
    host that started no gate job for ``--absent-minutes`` while gate jobs
    waited more than ``--wait-minutes`` (``host_not_serving``). It also
    replays a past window (``--from``/``--to``/``--tick``), which is how the
    host rules are scored offline.

Every event carries ``fix``. Its fields are ``what`` (the exact subject),
``evidence`` (where to look), ``cause`` (cause class), ``command`` (the next
command), ``safe_to_auto_apply`` (bool) and ``verify`` (how to confirm it
worked). An event never says only that something is wrong.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timedelta, timezone
from typing import Any, Callable

CMUX = os.environ.get("CMUX_BIN", "/Applications/cmux.app/Contents/Resources/bin/cmux")


def gh_binary(environ: "os._Environ[str] | dict[str, str]") -> str:
    """The GitHub reader: the App-authenticated `ghapp`, never a `gh` on PATH.

    Reads go out under the fleet App's identity and budget, through the
    installed wrapper's grammar; `WATCH_INPUTS_GH` names another pinned binary.
    """
    return environ.get("WATCH_INPUTS_GH") or os.path.join(
        environ.get("HOME") or os.path.expanduser("~"), ".local", "bin", "ghapp")


GH = gh_binary(os.environ)
SCREEN_LINES = 20
PLACEHOLDER = "Ask Codex to do anything"
# A turn in progress, including Codex retrying a provider error by itself.
WORKING = re.compile(r"\bWorking \(|esc to interrupt")
CODEX_PROMPT = re.compile(r"^› ?(.*)$")
SHELL_PROMPT = re.compile(r"^\S+@\S+ .*[%$#]\s*$")
RESUME_FAILED = re.compile(r"^Error: Failed to resume session from (\S+?): (.*)$")
SESSION_ID = re.compile(r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})")
# Signatures of a Codex turn that stopped on an error rather than finishing.
ERROR_SIGNATURES = (
    "503", "unexpected status", "stream disconnected", "exceeded retry limit",
    "Service Unavailable", "stream error",
)
TAB_TO_QUEUE = "tab to queue"
# Text only a Codex session prints; a `› ` line alone is not enough, since
# other tools and shells print it too.
CODEX_MARKERS = ("Ask Codex", "Worked for ", "GPT-", "codex")
GATE_JOB = "macos"

Runner = Callable[[list[str]], tuple[int, str]]


def now() -> datetime:
    return datetime.now(timezone.utc).replace(microsecond=0)


def stamp(at: datetime) -> str:
    return at.strftime("%Y-%m-%dT%H:%M:%SZ")


def digest(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:12]


def run(argv: list[str], timeout: int = 30) -> tuple[int, str]:
    try:
        completed = subprocess.run(argv, capture_output=True, text=True, timeout=timeout,
                                   stdin=subprocess.DEVNULL, check=False)
    except (OSError, subprocess.TimeoutExpired) as error:
        return 127, str(error)
    return completed.returncode, completed.stdout if completed.returncode == 0 else completed.stderr


def event(kind: str, key: str, at: datetime, summary: str, fix: dict[str, Any],
          **extra: Any) -> dict[str, Any]:
    missing = {"what", "evidence", "cause", "command", "safe_to_auto_apply", "verify"} - set(fix)
    if missing:
        raise ValueError(f"{kind}: fix lacks {sorted(missing)}")
    return {"kind": kind, "key": key, "at": stamp(at), "summary": summary, "fix": fix, **extra}


# ---------------------------------------------------------------------------
# cmux
# ---------------------------------------------------------------------------


def classify_screen(workspace: str, title: str, screen: str, at: datetime) -> list[dict[str, Any]]:
    """Events for one workspace's last lines."""
    lines = [line.rstrip() for line in screen.splitlines()]
    text = "\n".join(lines)
    where = f"cmux workspace {workspace} ({title})"
    read = f"cmux read-screen --workspace {workspace} --lines {SCREEN_LINES}"
    codex = title.lower().startswith("codex") or any(
        marker.lower() in text.lower() for marker in CODEX_MARKERS)
    prompts = [i for i, line in enumerate(lines) if CODEX_PROMPT.match(line)] if codex else []
    last = next((line for line in reversed(lines) if line.strip()), "")
    if not prompts or SHELL_PROMPT.match(last.strip()):
        for line in reversed(lines):
            match = RESUME_FAILED.match(line.strip())
            if match:
                session = SESSION_ID.search(match.group(1))
                session_id = session.group(1) if session else "unknown"
                reason = match.group(2)
                return [event(
                    "codex_session_dead", f"codex_dead:{workspace}:{session_id}", at,
                    f"{where}: Codex session {session_id} did not resume: {reason[:200]}",
                    {
                        "what": f"Codex session {session_id} in {workspace}",
                        "evidence": read,
                        "cause": "resume_failed",
                        "command": (f"fix the cause the error names ({reason[:120]}), then in "
                                    f"that workspace run: cmux restore codex {session_id}"),
                        "safe_to_auto_apply": False,
                        "verify": f"{read} shows the Codex prompt (`› `) and no `Error:` line",
                    },
                    workspace=workspace, session=session_id,
                )]
        if prompts:
            # A Codex prompt above a shell prompt: the session exited.
            residue = CODEX_PROMPT.match(lines[prompts[-1]]).group(1).strip()
            return [event(
                "codex_session_dead", f"codex_exited:{workspace}:{digest(residue)}", at,
                f"{where}: the Codex session exited to the shell (last line on its prompt: "
                f"{residue[:120]})",
                {
                    "what": f"the Codex session in {workspace}",
                    "evidence": read,
                    "cause": "session_exited",
                    "command": (f"find the session id (`cmux sessions list --json` or the "
                                f"workspace's resume hint) and run: cmux restore codex <id>"),
                    "safe_to_auto_apply": False,
                    "verify": f"{read} shows the Codex prompt (`› `) as the last prompt",
                },
                workspace=workspace,
            )]
        return []
    working = bool(WORKING.search(text))
    prompt_index = prompts[-1]
    draft = CODEX_PROMPT.match(lines[prompt_index]).group(1).strip()
    continuation = []
    for line in lines[prompt_index + 1:]:
        if not line.startswith("  ") or not line.strip() or "·" in line:
            break
        continuation.append(line.strip())
    draft = " ".join([draft, *continuation]).strip()
    has_draft = bool(draft) and draft != PLACEHOLDER
    above = "\n".join(lines[:prompt_index])
    events: list[dict[str, Any]] = []
    send = f"cmux send --workspace {workspace}"
    key_cmd = f"cmux send-key --workspace {workspace}"
    if has_draft and working and TAB_TO_QUEUE in text:
        events.append(event(
            "codex_draft_queued", f"codex_draft_queued:{workspace}:{digest(draft)}", at,
            f"{where}: a draft waits for Tab to queue while the session works: {draft[:120]}",
            {
                "what": f"draft in {workspace}", "evidence": read, "cause": "draft_not_queued",
                "command": f"{key_cmd} tab",
                # Typing into an agent session needs an applier with that
                # authority; until one exists, a steward applies it.
                "safe_to_auto_apply": False,
                "verify": f"{read} no longer shows the draft on the `› ` line",
            },
            workspace=workspace,
        ))
    elif has_draft and not working:
        events.append(event(
            "codex_draft_unsent", f"codex_draft_unsent:{workspace}:{digest(draft)}", at,
            f"{where}: an unsent draft sits in an idle session: {draft[:120]}",
            {
                "what": f"draft in {workspace}", "evidence": read, "cause": "draft_not_sent",
                "command": f"{key_cmd} enter",
                # Someone may still be typing it: send only if a second read a
                # minute later shows the same draft.
                "safe_to_auto_apply": False,
                "verify": f"{read} shows `Working (` or the draft as a sent message",
            },
            workspace=workspace,
        ))
    elif not working and not has_draft:
        tail = above.splitlines()[-12:]
        hit = next((sig for sig in ERROR_SIGNATURES
                    if any(sig.lower() in line.lower() for line in tail)), None)
        if hit is not None:
            line = next(line for line in reversed(tail) if hit.lower() in line.lower()).strip()
            events.append(event(
                "codex_session_errored", f"codex_errored:{workspace}:{digest(line)}", at,
                f"{where}: the session stopped on an error and is idle: {line[:160]}",
                {
                    "what": f"Codex session in {workspace}", "evidence": read,
                    "cause": "provider_error",
                    "command": f"{send} continue && {key_cmd} enter",
                    # Typing into an agent session needs an applier with that
                    # authority; until one exists, a steward applies it.
                    "safe_to_auto_apply": False,
                    "verify": f"{read} shows `Working (` within a minute",
                },
                workspace=workspace,
            ))
    return events


def read_cmux(runner: Runner, at: datetime) -> list[dict[str, Any]]:
    code, listing = runner([CMUX, "workspace", "list"])
    if code != 0:
        raise RuntimeError(f"cmux workspace list failed: {listing.strip()[:200]}")
    events: list[dict[str, Any]] = []
    for line in listing.splitlines():
        match = re.search(r"(workspace:\d+)\s+(.*?)\s*(\[selected\])?$", line)
        if not match:
            continue
        workspace, title = match.group(1), match.group(2).strip()
        code, screen = runner([CMUX, "read-screen", "--workspace", workspace,
                               "--lines", str(SCREEN_LINES)])
        if code == 0:
            events.extend(classify_screen(workspace, title, screen, at))
    return events


# ---------------------------------------------------------------------------
# tartci hosts
# ---------------------------------------------------------------------------


NUMBER = re.compile(r"\d+(?:\.\d+)?")


def problem_key(problem: str) -> str:
    """A problem's identity: its text with the numbers left out, so a stale
    tool's "24.8 h behind" that counts up every pass stays one condition, live
    and in a replayed pool-log alike (tartci's pool-log dedupes the same way)."""
    return digest(NUMBER.sub("#", problem))


def classify_pool(alias: str, status: dict[str, Any], at: datetime) -> list[dict[str, Any]]:
    host = status.get("host") or alias
    inspect = ("~/.local/bin/tartci pool status" if alias == "local"
               else f"ssh {alias} '~/.local/bin/tartci pool status'")
    events: list[dict[str, Any]] = []
    state = status.get("state")
    if state != "on" or status.get("participating") is False:
        events.append(event(
            "host_out_of_pool", f"host_out_of_pool:{alias}:{state}", at,
            f"{alias} ({host}) is out of the pool: state {state}, "
            f"participating {status.get('participating')}",
            {
                "what": f"gate host {alias}", "evidence": inspect, "cause": f"pool_{state}",
                "command": f"{inspect}  # read why it is off before turning it on",
                "safe_to_auto_apply": False,
                "verify": f"{inspect} reports state on and participating true",
            },
            host=alias,
        ))
    fleet = status.get("fleet") or {}
    for section_name, section in (fleet.get("config") or {}).items():
        problem = section.get("problem") if isinstance(section, dict) else None
        if not problem:
            continue
        first = str(problem).splitlines()[0]
        events.append(event(
            "host_problem", f"host_problem:{alias}:{section_name}:{problem_key(str(problem))}", at,
            f"{alias} ({host}) {section_name}: {first[:200]}",
            {
                "what": f"{section_name} on {alias}", "evidence": inspect,
                "cause": section_name,
                "command": f"{inspect}  # the {section_name} section names the receipt and next step",
                "safe_to_auto_apply": False,
                "verify": f"{inspect} --json shows fleet.config.{section_name}.problem null",
            },
            host=alias,
        ))
    return events


def read_hosts(runner: Runner, aliases: list[str], at: datetime) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    for alias in aliases:
        argv = (["sh", "-c", "~/.local/bin/tartci pool status --json"] if alias == "local" else
                ["ssh", "-n", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", alias,
                 "~/.local/bin/tartci pool status --json"])
        code, output = runner(argv)
        status: Any = None
        if code == 0:
            try:
                status = json.loads(output)
            except json.JSONDecodeError:
                status = None
        if not isinstance(status, dict):
            hour = at.strftime("%Y-%m-%dT%H")
            events.append(event(
                "host_unreachable", f"host_unreachable:{alias}:{hour}", at,
                f"{alias}: tartci pool status could not be read ({output.strip()[:160]})",
                {
                    "what": f"gate host {alias}", "evidence": " ".join(argv),
                    "cause": "unreachable" if code == 255 else "unreadable",
                    "command": f"ssh {alias} true  # then check the host is up and on the LAN",
                    "safe_to_auto_apply": False,
                    "verify": f"{' '.join(argv)} prints JSON",
                },
                host=alias,
            ))
            continue
        events.extend(classify_pool(alias, status, at))
    return events


def read_pool_log(paths: list[str]) -> list[dict[str, Any]]:
    """The records of a tartci pool-log (`events.jsonl` and its rotated
    `events.jsonl.1`), oldest first, each with its time parsed as `_at`."""
    records: dict[tuple[str, str], dict[str, Any]] = {}
    for path in paths:
        with open(path, encoding="utf-8") as handle:
            for number, line in enumerate(handle, 1):
                if not line.strip():
                    continue
                try:
                    record = json.loads(line)
                    at = parse_time(record.get("at"))
                except (json.JSONDecodeError, ValueError, AttributeError) as error:
                    raise RuntimeError(f"{path}:{number}: not a pool-log record ({error})") from None
                if at is None:
                    raise RuntimeError(f"{path}:{number}: a pool-log record needs `at`")
                records[(record["at"], json.dumps(record, sort_keys=True))] = {**record, "_at": at}
    return sorted(records.values(), key=lambda record: record["_at"])


def replay_pool_log(alias: str, records: list[dict[str, Any]],
                    gap: timedelta = timedelta(hours=2)) -> tuple[list[dict[str, Any]], int]:
    """Host events from a pool-log, each stamped when its condition began.

    Each record is classified as the live `hosts` reader classifies a status
    (an `error` record as an unreadable status). An event is emitted when its
    condition first holds and again only after it cleared, so a condition the
    log re-states every heartbeat fires once, as the watcher would. Returns the
    events and how many gaps longer than `gap` the log has (the agent was down
    then, so nothing is known about that time).
    """
    events: list[dict[str, Any]] = []
    holding: set[str] = set()
    gaps = 0
    previous: datetime | None = None
    for record in records:
        at = record["_at"]
        if previous is not None and at - previous > gap:
            gaps += 1
        previous = at
        if "error" in record:
            now = [event(
                "host_unreachable", f"host_unreachable:{alias}:pool-log", at,
                f"{alias}: tartci pool status could not be read ({str(record['error'])[:160]})",
                {
                    "what": f"gate host {alias}", "evidence": f"{alias} pool-log record at {record['at']}",
                    "cause": "unreadable",
                    "command": f"ssh {alias} '~/.local/bin/tartci pool status'  # read the error",
                    "safe_to_auto_apply": False,
                    "verify": f"ssh {alias} '~/.local/bin/tartci pool status --json' prints JSON",
                },
                host=alias,
            )]
        else:
            status = {
                "host": record.get("host"),
                "state": record.get("state"),
                "participating": record.get("participating"),
                "fleet": {"config": {section: {"problem": text} for section, text
                                     in (record.get("problems") or {}).items()}},
            }
            now = classify_pool(alias, status, at)
        keys = {item["key"] for item in now}
        events.extend(item for item in now if item["key"] not in holding)
        holding = keys
    return events, gaps


# ---------------------------------------------------------------------------
# Gate serving, from GitHub jobs
# ---------------------------------------------------------------------------


def parse_time(text: Any) -> datetime | None:
    if not isinstance(text, str) or not text:
        return None
    return datetime.fromisoformat(text.replace("Z", "+00:00")).astimezone(timezone.utc)


def host_of(runner_name: str, hosts: list[str]) -> str | None:
    """The host id a self-hosted gate runner belongs to: its `<host>-` prefix.

    The dash ends the id, so `m5studio-...` never matches the host `m5`.
    """
    for host in hosts:
        if runner_name.startswith(f"{host}-"):
            return host
    return None


def serving_events(jobs: list[dict[str, Any]], hosts: list[str], at: datetime,
                   wait: timedelta, absent: timedelta, repo: str) -> list[dict[str, Any]]:
    """Hosts absent while gate jobs waited, as of `at`."""
    waiting = []
    for job in jobs:
        created, started = parse_time(job.get("created_at")), parse_time(job.get("started_at"))
        completed = parse_time(job.get("completed_at"))
        if created is None or created > at or job.get("conclusion") == "skipped":
            continue
        # A job cancelled before any runner took it ends without waiting longer.
        ended = completed is not None and completed <= at
        if (started is None or started > at) and not ended and at - created >= wait:
            waiting.append(job)
    if not waiting:
        return []
    oldest = min(parse_time(job["created_at"]) for job in waiting)
    events = []
    for host in hosts:
        starts = [parse_time(job.get("started_at")) for job in jobs
                  if host_of(job.get("runner_name") or "", hosts) == host]
        starts = [start for start in starts if start is not None and start <= at]
        last = max(starts) if starts else None
        if last is not None and at - last < absent:
            continue
        since = stamp(last) if last else "the window start"
        evidence = (f"ghapp api 'repos/{repo}/actions/runs?status=queued' and the runner list "
                    f"for {host}")
        events.append(event(
            "host_not_serving", f"host_not_serving:{host}:{since}", at,
            f"gate host {host} has started no {GATE_JOB} job since {since} while "
            f"{len(waiting)} gate job(s) waited (oldest queued {stamp(oldest)})",
            {
                "what": f"gate host {host}", "evidence": evidence, "cause": "host_not_serving",
                "command": f"ssh <{host} alias> '~/.local/bin/tartci pool status'",
                "safe_to_auto_apply": False,
                "verify": f"a {GATE_JOB} job starts on a {host}-* runner",
            },
            host=host, waiting=len(waiting),
        ))
    return events


def read_gate_jobs(runner: Runner, repo: str, workflow: str, start: datetime,
                   end: datetime, gh: str = GH) -> list[dict[str, Any]]:
    """Every `macos` job of the gate workflow's runs created in `start..end`, read with `gh`."""
    jobs: list[dict[str, Any]] = []
    day = start
    while day < end:
        until = min(day + timedelta(days=1), end)
        code, out = runner([gh, "api", "--paginate", "--slurp", "-X", "GET",
                            f"repos/{repo}/actions/workflows/{workflow}/runs",
                            "-f", f"created={stamp(day)}..{stamp(until)}", "-f", "per_page=100"])
        if code != 0:
            raise RuntimeError(f"run listing failed: {out.strip()[:200]}")
        runs = [run for page in json.loads(out) for run in page.get("workflow_runs", [])]
        for item in runs:
            code, out = runner([gh, "api", f"repos/{repo}/actions/runs/{item['id']}/jobs?per_page=100",
                                "--jq", f'[.jobs[] | select(.name == "{GATE_JOB}") | '
                                '{created_at, started_at, completed_at, conclusion, runner_name}]'])
            if code != 0:
                raise RuntimeError(f"jobs of run {item['id']} unreadable: {out.strip()[:200]}")
            jobs.extend(json.loads(out))
        day = until
    return jobs


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def emit(events: list[dict[str, Any]]) -> None:
    for item in events:
        sys.stdout.write(json.dumps(item, sort_keys=True) + "\n")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="reader", required=True)
    sub.add_parser("cmux", help="Codex tab states from cmux (read-only)")
    hosts = sub.add_parser("hosts", help="tartci pool status from each gate host (read-only)")
    hosts.add_argument("--host", action="append",
                       help="ssh alias, or `local` for this host; repeatable")
    hosts.add_argument("--replay-log", action="append", metavar="ALIAS=PATH",
                       help="replay a tartci pool-log (events.jsonl, and events.jsonl.1 by "
                            "repeating the alias) instead of reading hosts live; repeatable")
    serving = sub.add_parser("gate-serving", help="gate hosts absent while gate jobs waited")
    serving.add_argument("--repo", default="Generous-Corp/pulp")
    serving.add_argument("--workflow", default="build.yml")
    serving.add_argument("--host", action="append", required=True,
                         help="host id, the runner-name prefix (studio, m5studio, m1, m5)")
    serving.add_argument("--wait-minutes", type=int, default=10)
    serving.add_argument("--absent-minutes", type=int, default=45)
    serving.add_argument("--from", dest="start", help="replay window start (RFC 3339)")
    serving.add_argument("--to", dest="end", help="replay window end (RFC 3339)")
    serving.add_argument("--tick", type=int, default=5, help="replay step in minutes")
    serving.add_argument("--jobs", help="read jobs from this JSON file instead of GitHub")
    serving.add_argument("--save-jobs", help="also write the jobs read from GitHub to this file")
    args = parser.parse_args(argv)
    at = now()
    try:
        if args.reader == "cmux":
            emit(read_cmux(run, at))
        elif args.reader == "hosts" and args.replay_log:
            if args.host:
                parser.error("--host reads live; --replay-log replays; give one")
            logs: dict[str, list[str]] = {}
            for item in args.replay_log:
                alias, sep, path = item.partition("=")
                if not sep or not alias or not path:
                    parser.error(f"--replay-log wants ALIAS=PATH, got {item!r}")
                logs.setdefault(alias, []).append(path)
            out = []
            for alias, paths in sorted(logs.items()):
                replayed, gaps = replay_pool_log(alias, read_pool_log(paths))
                out.extend(replayed)
                sys.stderr.write(f"watch_inputs: {alias}: {len(replayed)} event(s) from "
                                 f"{len(paths)} log file(s); {gaps} gap(s) over 2 h, when the "
                                 "pool-log agent was down\n")
            emit(sorted(out, key=lambda item: item["at"]))
        elif args.reader == "hosts":
            if not args.host:
                parser.error("hosts needs --host (live) or --replay-log")
            emit(read_hosts(run, args.host, at))
        else:
            wait = timedelta(minutes=args.wait_minutes)
            absent = timedelta(minutes=args.absent_minutes)
            end = parse_time(args.end) or at
            start = parse_time(args.start) or end
            if args.jobs:
                with open(args.jobs, encoding="utf-8") as handle:
                    jobs = json.load(handle)
            else:
                jobs = read_gate_jobs(run, args.repo, args.workflow,
                                      start - timedelta(hours=12), end)
                if args.save_jobs:
                    with open(args.save_jobs, "w", encoding="utf-8") as handle:
                        json.dump(jobs, handle)
            seen: set[str] = set()
            tick = start
            out: list[dict[str, Any]] = []
            while tick <= end:
                for item in serving_events(jobs, args.host, tick, wait, absent, args.repo):
                    if item["key"] not in seen:
                        seen.add(item["key"])
                        out.append(item)
                tick += timedelta(minutes=args.tick)
            emit(out)
    except RuntimeError as error:
        sys.stderr.write(f"watch-inputs {args.reader}: {error}\n")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
