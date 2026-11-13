#!/usr/bin/env python3
"""Drain and fault watch: catch stalled drains and runner faults without a human reading logs.

Run as `python3 -I tools/metrics/drain_watch.py ...`. Standard library only.
Reads local files only (steward tick JSON, pass-runner claim and attempt files,
fleet-pass logs). The only network use is `ghapp api` to file, update or close
one issue per finding, and only on the second consecutive run in which the
finding persists. See docs/drain-watch.md.

Kinds:
  drain_stall    actionable PRs wait (for 2 h or more) while none of the repository's PRs
                 merged in the last 2 h
  stale_hold     a held PR row with the same head for 45 min or more
  stuck_claim    a repair claim 30 min or more old whose PR head has not moved
  pass_fault     new log lines matching a fault signature, per signature and lane

  drain_watch.py --tick           # what the steward tick runs: always exits 0
  drain_watch.py --dry-run        # print what would be announced and filed, change nothing
"""
from __future__ import annotations

import argparse
import datetime as dt
import glob
import json
import os
import re
import subprocess
import sys
import time

UTC = dt.timezone.utc
STALL_WINDOW = dt.timedelta(hours=2)
STALE_HOLD = dt.timedelta(minutes=45)
STUCK_CLAIM = dt.timedelta(minutes=30)
CLEAR_RUNS = 3
ISSUE_UPDATE_EVERY = 3600
LABEL = "drain-watch"
ISSUE_REPO = "danielraffel/Shipyard"
SIGNATURES = [
    "PermissionError", "Operation not permitted", "command not found", "getcwd",
    "Unable to read current working directory", "sr status timed out", "Traceback",
]
MERGE_FROM = {"queued", "admitted", "ready"}
# Only the runner's own logs (<lane>.log, <lane>.launchd.log). The *.last-output.txt files
# are agent transcripts that quote these strings in prose and code, so they are not scanned.
LOG_GLOBS = ("*.log",)
LINE_TS = re.compile(r"^\s*(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?)")
CUTOFF_SLACK = dt.timedelta(minutes=15)
FIRST_RUN_LOOKBACK = dt.timedelta(hours=2)
MAX_READ = 8 * 1024 * 1024


STAMP_RE = re.compile(r"-(\d{8}T\d{6}Z)\.json$")
# Same classification as tools/metrics/loss_report.py (a test keeps them equal
# once both are on main). Copied so this file stands alone.
INTENTIONAL = {"draft"}
SYSTEM_WAIT = {
    "queued", "admitted", "ready", "deferred_pass_budget", "revalidating",
    "transient_rerun", "carried_commits_over_cap", "carried_commits_unreadable",
    "capacity_queued", "required_pending",
}
OWNER_STOP = {"excluded_label", "direct_merge_refused", "owner_decision"}


def classify(token, actor, stop):
    fam = token.split(":", 1)[0]
    if fam in INTENTIONAL:
        return "intentional"
    if stop or actor == "owner" or fam in OWNER_STOP:
        return "human_stop"
    if fam in SYSTEM_WAIT or actor in ("steward", "nobody", "ci", "reviewer", "operator"):
        return "normal_wait"
    return "actionable"


def parse_stamp(s):
    return dt.datetime.strptime(s, "%Y%m%dT%H%M%SZ").replace(tzinfo=UTC)


def parse_iso(s):
    if not s:
        return None
    try:
        d = dt.datetime.fromisoformat(str(s).replace("Z", "+00:00"))
    except ValueError:
        return None
    return d if d.tzinfo else d.replace(tzinfo=UTC)


def home(*p):
    return os.path.join(os.path.expanduser("~"), *p)


# ------------------------------------------------------------------ ticks
def recent_ticks(log_dir, now, span):
    """{repo: [(t, rows|None)]} from live tick files newer than now-span."""
    start = now - span
    per_repo = {}
    try:
        names = os.listdir(log_dir)
    except OSError:
        return per_repo
    for name in sorted(names):
        m = STAMP_RE.search(name)
        if not m or name.startswith(("fleet-invariants-", "loss-report-")):
            continue
        t = parse_stamp(m.group(1))
        if t < start or t > now:
            continue
        try:
            with open(os.path.join(log_dir, name), "rb") as fh:
                d = json.loads(fh.read())
            repos = d["repos"]
        except Exception:
            continue
        for r in repos:
            if "prs" not in r or r.get("errors"):
                per_repo.setdefault(r["repo"], []).append((t, None))
                continue
            rows = {}
            for p in r["prs"]:
                na = p.get("next_action") or {}
                rows[p["number"]] = {"head": p.get("head_sha"), "token": p.get("blocked_on") or "unknown",
                                     "actor": na.get("actor"), "stop": bool(na.get("stop")),
                                     "since": parse_iso(p.get("since")), "deferred": read_deferred(p)}
            per_repo.setdefault(r["repo"], []).append((t, rows))
    for v in per_repo.values():
        v.sort(key=lambda x: x[0])
    return per_repo


READ_ERROR_RE = re.compile(r"HTTP 50\d|timed out|unreadable|cannot read|read failed")


def read_deferred(p):
    """A PR the pass could not read this tick (a transient 5xx or timeout): it was
    neither judged nor refused, so it is unobserved, never an event. Mirrors the
    `read_deferred=` split in shipyard-steward-tick.sh."""
    err = p.get("error")
    if err is None:
        return False
    kind = p.get("error_kind")
    return kind == "read" or (kind is None and bool(READ_ERROR_RE.search(str(err))))


def latest_valid(ticks):
    for t, rows in reversed(ticks):
        if rows is not None:
            return t, rows
    return None, None


def check_drain_stall(per_repo, now):
    out = []
    for repo, ticks in per_repo.items():
        valid = [(t, r) for t, r in ticks if r is not None]
        if not valid:
            continue
        t_last, rows = valid[-1]
        if now - t_last > dt.timedelta(minutes=30):
            continue  # no fresh observation: not a verdict
        if t_last - valid[0][0] < STALL_WINDOW - dt.timedelta(minutes=15):
            continue  # not enough history to say nothing merged in 2 h
        waiting = [pr for pr, o in rows.items()
                   if not o["deferred"] and classify(o["token"], o["actor"], o["stop"]) == "actionable"
                   and (o["since"] or t_last) <= t_last - STALL_WINDOW]
        if not waiting:
            continue
        merged = 0
        for (t0, r0), (t1, r1) in zip(valid, valid[1:]):
            for pr, o in r0.items():
                if pr not in r1 and o["token"].split(":", 1)[0] in MERGE_FROM and t1 >= now - STALL_WINDOW:
                    merged += 1
        if merged == 0:
            out.append(dict(kind="drain_stall", repo=repo, lane="-", key=f"drain_stall:{repo}",
                            detail=f"{len(waiting)} actionable PR(s) waiting 2h+, none merged in 2h, e.g. "
                                   + ",".join(f"#{n}" for n in sorted(waiting)[:5]),
                            evidence=[f"{repo}#{n} {rows[n]['token']} actor={rows[n]['actor']}" for n in sorted(waiting)[:5]]))
    return out


def check_stale_hold(per_repo, now):
    out = []
    for repo, ticks in per_repo.items():
        t_last, rows = latest_valid(ticks)
        if rows is None or now - t_last > dt.timedelta(minutes=30):
            continue
        old = [pr for pr, o in rows.items()
               if not o["deferred"] and (o["token"] == "held" or o["token"].startswith("held:")) and o["since"]
               and t_last - o["since"] >= STALE_HOLD]
        if old:
            out.append(dict(kind="stale_hold", repo=repo, lane="-", key=f"stale_hold:{repo}",
                            detail=f"{len(old)} held PR(s) 45m+ with no new head: " + ",".join(f"#{n}" for n in sorted(old)[:5]),
                            evidence=[f"{repo}#{n} {rows[n]['token']} since {rows[n]['since'].isoformat()}" for n in sorted(old)[:5]]))
    return out


# ----------------------------------------------------------------- claims
def latest_attempts(runner_dir):
    """{(repo, pr): head} from repair-attempts.log lines `epoch owner/repo#pr head tier`."""
    res = {}
    try:
        with open(os.path.join(runner_dir, "repair-attempts.log")) as fh:
            for line in fh:
                p = line.split()
                if len(p) >= 3 and "#" in p[1]:
                    repo, _, pr = p[1].rpartition("#")
                    if pr.isdigit():
                        res[(repo, int(pr))] = p[2]
    except OSError:
        pass
    return res


def check_stuck_claims(per_repo, runner_dir, now):
    out = []
    names = {r.replace("/", "_"): r for r in per_repo}
    attempts = latest_attempts(runner_dir)
    for d in sorted(glob.glob(os.path.join(runner_dir, "repair-claims", "*"))):
        base = os.path.basename(d)
        stem, _, pr = base.rpartition("_")
        repo = names.get(stem)
        pidf = os.path.join(d, "pid")
        if not (repo and pr.isdigit() and os.path.exists(pidf)):
            continue
        t_last, rows = latest_valid(per_repo[repo])
        if rows is None or int(pr) not in rows:
            continue
        claimed = dt.datetime.fromtimestamp(os.path.getmtime(pidf), UTC)
        age = now - claimed
        if age < STUCK_CLAIM:
            continue
        claim_head = attempts.get((repo, int(pr)))
        if claim_head and rows[int(pr)]["head"] == claim_head:
            out.append(dict(kind="stuck_claim", repo=repo, lane="repair", key=f"stuck_claim:{repo}#{pr}",
                            detail=f"#{pr} claimed {int(age.total_seconds() // 60)}m ago, head {claim_head[:8]} unchanged",
                            evidence=[f"{base}/pid mtime {claimed.isoformat()}; head at claim {claim_head}; head now {rows[int(pr)]['head']}"]))
    return out


# ------------------------------------------------------------ pass faults
def scan_logs(fleet_dir, state, from_start, now=None):
    """Count new matching lines since the stored offsets. Mutates state['offsets'].

    A line with a leading ISO timestamp is attributed to that time; a line without
    one to the preceding timestamped line (carried across runs per file). A line
    attributed to a time before the cutoff (last run minus 15 min, or 2 h before
    now on a first run) is old content, not a new fault, and is not counted. A line
    with no timestamp anywhere before it cannot be proven old and is counted.
    """
    now = now or dt.datetime.now(UTC)
    last_run = parse_iso(state.get("last_run"))
    if last_run:
        cutoff = last_run - CUTOFF_SLACK
    elif from_start:
        cutoff = None  # --from-start with no prior run: count everything, however old
    else:
        cutoff = now - FIRST_RUN_LOOKBACK
    state["last_run"] = now.isoformat(timespec="seconds")
    offs = state.setdefault("offsets", {})
    found = {}
    seeded = bool(offs) or from_start
    files = []
    for g in LOG_GLOBS:
        files += glob.glob(os.path.join(fleet_dir, g))
    for f in sorted(set(files)):
        try:
            st = os.stat(f)
        except OSError:
            continue
        rec = offs.get(f)
        if rec is None and not seeded:
            # first ever run: do not count history, but remember its last timestamp so
            # an untimestamped continuation line later is attributed to it
            ts = None
            try:
                with open(f, "rb") as fh:
                    fh.seek(max(0, st.st_size - 65536))
                    for line in fh.read().decode("utf-8", "replace").splitlines():
                        m = LINE_TS.match(line)
                        if m:
                            ts = parse_iso(m.group(1)) or ts
            except OSError:
                pass
            offs[f] = {"inode": st.st_ino, "offset": st.st_size, "ts": ts.isoformat() if ts else None}
            continue
        start = 0
        line_ts = None
        if rec and rec["inode"] == st.st_ino and rec["offset"] <= st.st_size:
            start = rec["offset"]
            line_ts = parse_iso(rec.get("ts"))
        end = st.st_size
        if end - start > MAX_READ:
            start = end - MAX_READ
        try:
            with open(f, "rb") as fh:
                fh.seek(start)
                data = fh.read(end - start)
        except OSError:
            continue
        cut = data.rfind(b"\n")
        if cut < 0:
            consumed = 0
            text = b""
        else:
            consumed = cut + 1
            text = data[:consumed]
        lane = os.path.basename(f).split(".")[0]
        for line in text.decode("utf-8", "replace").splitlines():
            m = LINE_TS.match(line)
            if m:
                line_ts = parse_iso(m.group(1)) or line_ts
            if cutoff is not None and line_ts is not None and line_ts < cutoff:
                continue
            for sig in SIGNATURES:
                if sig in line:
                    e = found.setdefault((lane, sig), {"count": 0, "evidence": []})
                    e["count"] += 1
                    if len(e["evidence"]) < 3:
                        e["evidence"].append(f"{os.path.basename(f)}: {line.strip()[:200]}")
        offs[f] = {"inode": st.st_ino, "offset": start + consumed,
                   "ts": line_ts.isoformat() if line_ts else None}
    out = []
    for (lane, sig), e in sorted(found.items()):
        out.append(dict(kind="pass_fault", repo="-", lane=lane, key=f"pass_fault:{lane}:{sig}",
                        detail=f"{sig} x{e['count']}", evidence=e["evidence"], occurrences=e["count"]))
    return out


# ------------------------------------------------------------------ issues
class Issues:
    def __init__(self, ghapp, repo=ISSUE_REPO):
        self.ghapp, self.repo = ghapp, repo

    def api(self, *args):
        r = subprocess.run([self.ghapp, "api", *args], capture_output=True, text=True, timeout=60)
        if r.returncode != 0:
            raise RuntimeError((r.stderr or r.stdout)[:200])
        return json.loads(r.stdout) if r.stdout.strip() else {}

    @staticmethod
    def marker(key):
        return f"<!-- drain-watch:{key} -->"

    def find(self, key):
        items = self.api(f"repos/{self.repo}/issues?labels={LABEL}&state=all&per_page=100")
        for it in items if isinstance(items, list) else []:
            if self.marker(key) in (it.get("body") or ""):
                return it
        return None

    def body(self, f, st):
        ev = "\n".join(f"- {e}" for e in f["evidence"])
        return (f"{self.marker(f['key'])}\n\nDrain watch finding `{f['key']}`.\n\n"
                f"- kind: {f['kind']}\n- repo: {f['repo']}\n- lane: {f['lane']}\n- detail: {f['detail']}\n"
                f"- first seen: {st['first_seen']}\n- runs seen: {st['runs']}\n- occurrences: {st.get('occurrences', 0)}\n\n"
                f"Evidence:\n{ev}\n\nThis issue is opened and closed by `tools/metrics/drain_watch.py`; "
                "it closes itself after the finding has been absent for 3 runs.\n")

    def upsert(self, f, st):
        n = st.get("issue")
        if not n:
            it = self.find(f["key"])
            if it:
                n = it["number"]
                if it.get("state") == "closed":
                    self.api(f"repos/{self.repo}/issues/{n}", "-X", "PATCH", "-f", "state=open")
        if not n:
            r = self.api(f"repos/{self.repo}/issues", "-f", f"title=drain-watch: {f['key']}",
                         "-f", f"body={self.body(f, st)}", "-f", f"labels[]={LABEL}")
            n = r["number"]
        else:
            self.api(f"repos/{self.repo}/issues/{n}", "-X", "PATCH", "-f", f"body={self.body(f, st)}")
        return n

    def close(self, n, key):
        self.api(f"repos/{self.repo}/issues/{n}/comments", "-f", "body=Cleared: the finding was absent for 3 consecutive runs.")
        self.api(f"repos/{self.repo}/issues/{n}", "-X", "PATCH", "-f", "state=closed")


# --------------------------------------------------------------------- run
def load_state(path):
    try:
        with open(path) as fh:
            s = json.load(fh)
        return s if isinstance(s, dict) else {}
    except Exception:
        return {}


def save_state(path, s):
    tmp = path + ".tmp"
    with open(tmp, "w") as fh:
        json.dump(s, fh, indent=1)
    os.replace(tmp, path)


def collect(a, now, state):
    per_repo = recent_ticks(a.log_dir, now, STALL_WINDOW + dt.timedelta(minutes=30))
    f = []
    f += check_drain_stall(per_repo, now)
    f += check_stale_hold(per_repo, now)
    f += check_stuck_claims(per_repo, a.runner_dir, now)
    f += scan_logs(a.fleet_dir, state, a.from_start, now)
    return f


def run(a, now=None, issues=None):
    """One watch run. Returns the lines it announced or would announce."""
    now = now or dt.datetime.now(UTC)
    stamp = now.strftime("%Y%m%dT%H%M%SZ")
    state = load_state(a.state)
    findings_state = state.setdefault("findings", {})
    findings = collect(a, now, state)
    seen = {f["key"] for f in findings}
    lines, actions = [], []
    issues = issues or Issues(a.ghapp)
    for f in findings:
        st = findings_state.get(f["key"])
        new = st is None
        if new:
            st = findings_state[f["key"]] = {"first_seen": now.isoformat(timespec="seconds"), "runs": 0,
                                             "absent": 0, "occurrences": 0, "issue": None, "issue_updated": 0}
        st["runs"] += 1
        st["absent"] = 0
        st["occurrences"] += f.get("occurrences", 1)
        if new:
            lines.append(f"{stamp} drain-watch kind={f['kind']} repo={f['repo']} lane={f['lane']} "
                         f"detail={f['detail'].replace(' ', '_')} first_seen={st['first_seen']} count={st['runs']}")
        st["evidence"] = (st.get("evidence", []) + [e for e in f["evidence"] if e not in st.get("evidence", [])])[:6]
    # File or refresh issues. A finding whose issue call failed stays `pending` in the
    # state with its evidence and is retried on later runs even if its log lines are
    # gone (the log offset has already moved past them).
    current = {f["key"]: f for f in findings}
    for key, st in findings_state.items():
        if st["runs"] < 2:
            continue
        pending = st.get("pending")
        f = current.get(key)
        if f is None and pending is None:
            continue
        f = dict(f or pending, evidence=st.get("evidence") or (pending or f)["evidence"])
        due = pending is not None or not st["issue"] or time.time() - st.get("issue_updated", 0) >= ISSUE_UPDATE_EVERY
        if not due:
            continue
        if a.dry_run:
            actions.append(f"would file/update issue for {key}")
            continue
        try:
            st["issue"] = issues.upsert(f, st)
            st["issue_updated"] = time.time()
            st.pop("pending", None)
        except Exception as exc:
            st["pending"] = {k: f[k] for k in ("kind", "repo", "lane", "key", "detail", "evidence")}
            lines.append(f"{stamp} drain-watch ERROR issue {key}: {str(exc)[:100]}")
    for key in list(findings_state):
        if key in seen:
            continue
        st = findings_state[key]
        st["absent"] = st.get("absent", 0) + 1
        if st.get("pending") and not st.get("issue"):
            continue  # no issue could be filed yet: keep it until one is
        if st["absent"] >= CLEAR_RUNS:
            if st.get("issue"):
                if a.dry_run:
                    actions.append(f"would close issue #{st['issue']} for {key}")
                else:
                    try:
                        issues.close(st["issue"], key)
                    except Exception as exc:
                        lines.append(f"{stamp} drain-watch ERROR close {key}: {str(exc)[:100]}")
                        continue
            del findings_state[key]
    if not a.dry_run:
        os.makedirs(os.path.dirname(a.state) or ".", exist_ok=True)
        save_state(a.state, state)
        if lines:
            with open(a.summary, "a") as fh:
                fh.write("\n".join(lines) + "\n")
    return lines, [x for x in actions if x], findings


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--log-dir", default=home("Library", "Logs", "shipyard-steward-tick"))
    ap.add_argument("--fleet-dir", default=home("Library", "Logs", "fleet-pass"))
    ap.add_argument("--runner-dir", default=home(".local", "share", "shipyard-steward", "pass-runner"))
    ap.add_argument("--state")
    ap.add_argument("--summary")
    ap.add_argument("--ghapp", default=os.environ.get("DRAIN_WATCH_GHAPP", "ghapp"))
    ap.add_argument("--now")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--from-start", action="store_true", help="count log lines from the start on the first run instead of seeding at the end")
    ap.add_argument("--tick", action="store_true", help="always exit 0; take a lock; log failures as one line")
    a = ap.parse_args(argv)
    a.state = a.state or os.path.join(a.log_dir, "drain-watch-state.json")
    a.summary = a.summary or os.path.join(a.log_dir, "summary.log")
    now = parse_iso(a.now) if a.now else None
    if not a.tick:
        lines, actions, findings = run(a, now)
        for line in lines + actions:
            print(line)
        if a.dry_run:
            print(f"dry-run: {len(findings)} finding(s) this run")
        return 0
    lock = os.path.join(a.log_dir, ".drain-watch.lock")
    got = False
    try:
        try:
            os.mkdir(lock)
        except FileExistsError:
            if time.time() - os.path.getmtime(lock) > 600:
                os.rmdir(lock)
                os.mkdir(lock)
            else:
                return 0
        got = True
        run(a, now)
    except BaseException as exc:
        try:
            with open(a.summary, "a") as fh:
                fh.write(f"{dt.datetime.now(UTC):%Y%m%dT%H%M%SZ} drain-watch ERROR {type(exc).__name__}: {str(exc)[:120]}\n")
        except Exception:
            pass
    finally:
        if got:
            try:
                os.rmdir(lock)
            except OSError:
                pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
