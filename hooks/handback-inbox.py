#!/usr/bin/env python3
"""Show this session the PR hand-back notes Shipyard queued for it.

`shipyard pr-watch` (hand-back tier 1) appends one JSON line per flagged pull
request to ``~/.local/state/shipyard/inbox/<session-id>.jsonl`` on the host
where the owning agent session runs. This hook runs at SessionStart and
UserPromptSubmit (Claude Code and Codex use the same hook contract): it
claims the file, prints a short bounded summary as agent context, and moves
the entries to ``<session-id>.shown.jsonl`` so they are shown once.

Each entry moved to the shown file is stamped `shown_at`; Shipyard reads that
file back as the wake's acknowledgement (`wake.seen`). PostToolUse runs reach
this script only through `handback-inbox-poll.sh`, which costs one file test
per tool call and starts nothing when the inbox is empty.

It is a silent no-op when there is no session id, no inbox, or an empty one,
and it never fails the agent's turn: any error exits 0 with no output.
Nothing here writes to GitHub or to the session's input.
"""

from __future__ import annotations

import json
import os
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

MAX_ENTRIES = 5
MAX_LINE = 300
MAX_TOTAL = 2000
SHOWN_ID_WINDOW = 500
SAFE_SESSION = re.compile(r"^[A-Za-z0-9_:][A-Za-z0-9._:-]{0,127}$")


def inbox_dir() -> Path:
    override = os.environ.get("SHIPYARD_INBOX_DIR")
    if override:
        return Path(override)
    return Path.home() / ".local" / "state" / "shipyard" / "inbox"


def clip(text: str, limit: int) -> str:
    text = " ".join(str(text).split())
    return text if len(text) <= limit else text[: limit - 1] + "…"


def shown_ids(path: Path) -> set[str]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()[-SHOWN_ID_WINDOW:]
    except OSError:
        return set()
    ids = set()
    for line in lines:
        try:
            ids.add(json.loads(line)["id"])
        except (ValueError, KeyError, TypeError):
            continue
    return ids


def claim(directory: Path, session: str) -> list[str]:
    """Atomically take the inbox file; a concurrent writer starts a new one."""
    inbox = directory / f"{session}.jsonl"
    try:
        if inbox.stat().st_size == 0:
            return []
    except OSError:
        return []
    claimed = directory / f"{session}.jsonl.claim-{os.getpid()}"
    try:
        os.rename(inbox, claimed)
    except OSError:
        return []
    try:
        lines = claimed.read_text(encoding="utf-8").splitlines()
    except OSError:
        return []
    shown = directory / f"{session}.shown.jsonl"
    # `shown_at` is the acknowledgement Shipyard reads back: the entry reached
    # this session inside an agent turn.
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    with shown.open("a", encoding="utf-8") as handle:
        for line in lines:
            if not line.strip():
                continue
            try:
                entry = json.loads(line)
            except ValueError:
                entry = None
            if isinstance(entry, dict):
                entry.setdefault("shown_at", stamp)
                handle.write(json.dumps(entry) + "\n")
            else:
                handle.write(line.rstrip("\n") + "\n")
    claimed.unlink(missing_ok=True)
    return lines


def age(stamp: object, now: datetime) -> str:
    try:
        then = datetime.strptime(str(stamp), "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
    except ValueError:
        return ""
    minutes = max(0, int((now - then).total_seconds() // 60))
    if minutes < 60:
        return f"{minutes}m ago"
    if minutes < 48 * 60:
        return f"{minutes // 60}h ago"
    return f"{minutes // (24 * 60)}d ago"


def where(entry: dict, now: datetime) -> str:
    head = str(entry.get("head_sha") or "")[:7]
    when = age(entry.get("delivered_at"), now)
    text = ", ".join(part for part in (f"on head {head}" if head else "", when) if part)
    return f" {text}" if text else ""


def render(entries: list[dict], now: datetime | None = None) -> str:
    now = now or datetime.now(timezone.utc)
    prs = sorted({entry.get("pr") for entry in entries if entry.get("pr") is not None})
    head = (
        f"Shipyard PR watch: {len(prs)} pull request(s) you opened need attention "
        "(read-only notes from CI; nothing was changed for you):"
    )
    out = [head]
    for entry in entries[:MAX_ENTRIES]:
        # The link and verdict survive; only the evidence is shortened.
        prefix = clip(
            f"- #{entry.get('pr')} {entry.get('url', '')} "
            f"{entry.get('verdict', '')} ({entry.get('kind', '')}){where(entry, now)}: ",
            MAX_LINE * 2 // 3,
        )
        out.append(prefix + " " + clip(entry.get("evidence", ""), MAX_LINE - len(prefix) - 2))
    if len(entries) > MAX_ENTRIES:
        out.append(f"- …and {len(entries) - MAX_ENTRIES} more; see each PR's pr-watch comment.")
    out.append(
        "Each note is about the head it names: if you pushed after that head, "
        "it is stale, so check the PR's current state first. Look at the failing "
        "check before pushing again; if it is not yours (pre-existing or a "
        "neighbour), say so on the PR."
    )
    text = "\n".join(out)
    return text if len(text) <= MAX_TOTAL else text[: MAX_TOTAL - 1] + "…"


def main() -> int:
    try:
        payload = json.loads(sys.stdin.read() or "{}")
    except ValueError:
        return 0
    if not isinstance(payload, dict):
        return 0
    session = str(payload.get("session_id") or "")
    if not SAFE_SESSION.match(session):
        return 0
    directory = inbox_dir()
    already = shown_ids(directory / f"{session}.shown.jsonl")
    lines = claim(directory, session)
    parsed = []
    retracted = set()
    seen = set()
    for line in lines:
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        if not isinstance(entry, dict):
            continue
        if "retract" in entry:
            retracted.add(entry["retract"])
            continue
        parsed.append(entry)
    entries = []
    for entry in parsed:
        key = entry.get("id")
        if key in retracted or key in already or key in seen:
            continue
        seen.add(key)
        entries.append(entry)
    if not entries:
        return 0
    event = str(payload.get("hook_event_name") or "UserPromptSubmit")
    print(
        json.dumps(
            {
                "hookSpecificOutput": {
                    "hookEventName": event,
                    "additionalContext": render(entries),
                }
            }
        )
    )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:  # noqa: BLE001 - a notice must never break the turn
        sys.exit(0)
