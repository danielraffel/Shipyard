"""End-to-end metrics per merged PR, per repo, per UTC day (merge day).

Used by loss_report.py. Standard library only. Reads GitHub through `ghread api`
(Generous-Corp) or `ghapp api` (everything else), caches every answer worth
keeping on disk, and stops at a per-run read cap (`truncated`). Anything it
cannot read is "unknown" or null, never a zero.

Definitions (also in docs/rollups.md, "End-to-end metrics"):
  autonomous merge rate  PRs merged that day with no owner action from opening to merge,
                         over PRs where that is determinable; the rest count as unknown.
  handoff-to-merge time  median and p90 minutes from the first ready-for-review moment
                         to merged_at.
  compute per merged PR  Actions job minutes over runs on the PR's heads plus its
                         merge_group runs, split self-hosted and hosted.
  correctness            share of merged PRs with a revert or a red required check on the
                         base within 7 days; provisional while the window is open.
"""
from __future__ import annotations

import datetime as dt
import json
import math
import os
import re
import statistics
import subprocess

UTC = dt.timezone.utc
WINDOW = dt.timedelta(days=7)
# Timeline events that are an actor's own action on the PR (noise like
# `subscribed` or `cross-referenced` is not).
ACTION_EVENTS = {
    "labeled", "unlabeled", "commented", "reviewed", "review_dismissed", "review_requested",
    "review_request_removed", "closed", "reopened", "merged", "assigned", "unassigned",
    "ready_for_review", "converted_to_draft", "auto_merge_enabled", "auto_merge_disabled",
    "enqueued", "dequeued", "milestoned", "demilestoned", "renamed", "base_ref_changed",
}
AGENT_TRAILER_RE = re.compile(
    r"co-authored-by:[^\n]*(claude|codex|anthropic|openai|copilot)|generated with[^\n]*(claude|codex)"
    r"|^(version-bump|skill-update|release)\s*:", re.I | re.M)
BAD_CONCLUSIONS = {"failure", "timed_out"}
FACTS_VERSION = 2
ESCAPE_WINDOW = dt.timedelta(days=14)
# Labels that take a PR out of the eligible cohort, with the reason reported.
EXCLUDE_LABELS = {"automation": "automation", "shipyard:unmanaged": "unmanaged", "superseded": "superseded"}
INTEGRATION_BASES = ("develop/", "release/")
REPAIRED_RE = re.compile(r"^\s*repaired\s*:", re.I)
HOLD_RE = re.compile(r"^\s*[*_#> ]*HOLD\b")
ESCAPE_RE = r"(?i)\b(fix\w*|regress\w*|broke\w*|broken|caused|introduc\w*)\b[^\n]{0,60}?(?<![\w#])#%d(?!\d)"


class Truncated(Exception):
    """The per-run read cap was reached."""


class ReadError(Exception):
    pass


def iso(s):
    if not s:
        return None
    try:
        d = dt.datetime.fromisoformat(str(s).replace("Z", "+00:00"))
    except ValueError:
        return None
    return d if d.tzinfo else d.replace(tzinfo=UTC)


def fmt(t):
    return t.astimezone(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def gh_runner(repo):
    """The command that reads `repo`: the read-only admission reader for Generous-Corp, else the App."""
    tool = "ghread" if repo.startswith("Generous-Corp/") else "ghapp"

    def run(path):
        try:
            p = subprocess.run([tool, "api", path], capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.TimeoutExpired) as e:
            raise ReadError(f"{tool}: {type(e).__name__}") from e
        if p.returncode != 0:
            raise ReadError(f"{tool} exit {p.returncode}: {p.stderr.strip()[:120]}")
        try:
            return json.loads(p.stdout)
        except ValueError as e:
            raise ReadError(f"{tool}: not JSON") from e
    return run


class Reader:
    """GitHub GET with a hard cap on calls for one repo in one run, and an on-disk cache."""

    def __init__(self, repo, cache_dir, cap, fetch=None):
        self.repo, self.cap, self.calls = repo, cap, 0
        self.fetch = fetch or gh_runner(repo)
        self.root = os.path.join(cache_dir, repo.replace("/", "__"))
        self.truncated = False
        self.errors = 0

    def get(self, path):
        if self.calls >= self.cap:
            self.truncated = True
            raise Truncated()
        self.calls += 1
        try:
            return self.fetch(f"repos/{self.repo}/{path}")
        except ReadError:
            self.errors += 1
            raise

    def pages(self, path, key=None, max_pages=3):
        """All items of a paginated list (up to max_pages of 100). Returns (items, complete)."""
        out = []
        sep = "&" if "?" in path else "?"
        for n in range(1, max_pages + 1):
            d = self.get(f"{path}{sep}per_page=100&page={n}")
            items = d.get(key, []) if key and isinstance(d, dict) else d
            if not isinstance(items, list):
                raise ReadError("unexpected shape")
            out += items
            if len(items) < 100:
                return out, True
        return out, False

    def load(self, kind, name):
        try:
            with open(os.path.join(self.root, kind, f"{name}.json")) as fh:
                return json.load(fh)
        except (OSError, ValueError):
            return None

    def save(self, kind, name, data):
        d = os.path.join(self.root, kind)
        try:
            os.makedirs(d, exist_ok=True)
            tmp = os.path.join(d, f"{name}.json.tmp")
            with open(tmp, "w") as fh:
                json.dump(data, fh)
            os.replace(tmp, os.path.join(d, f"{name}.json"))
        except OSError:
            pass  # a cache that cannot be written only costs a re-read


# ----------------------------------------------------------- per-PR facts
def pr_facts(rd, pr, owner):
    """Timeline and commits of one merged PR, reduced to what the metrics need. Cached forever."""
    n = pr["number"]
    tl, tl_full = rd.pages(f"issues/{n}/timeline", max_pages=5)
    commits, c_full = rd.pages(f"pulls/{n}/commits", max_pages=3)
    created, merged = iso(pr["created_at"]), iso(pr["merged_at"])
    # first ready-for-review: a PR that was opened as a draft is ready at its first
    # ready_for_review event; otherwise (first lifecycle event is a conversion to draft, or
    # none) it was ready when opened.
    life = sorted((iso(e.get("created_at")), e["event"]) for e in tl
                  if e.get("event") in ("ready_for_review", "converted_to_draft") and e.get("created_at"))
    ready = created
    if life and life[0][1] == "ready_for_review":
        ready = life[0][0]
    definite, ambiguous = [], []
    repairs = holds = 0
    for e in tl:
        body = e.get("body") or ""
        if e.get("event") in ("commented", "reviewed"):
            repairs += bool(REPAIRED_RE.search(body))
            holds += bool(HOLD_RE.search(body))
        if e.get("event") not in ACTION_EVENTS:
            continue
        who = ((e.get("actor") or e.get("user") or {}).get("login") or "")
        if who.lower() == owner.lower():
            definite.append(e["event"])
    heads = {pr["head"]["sha"]}
    for e in tl:
        if e.get("event") == "head_ref_force_pushed":
            heads.update(x for x in (e.get("before_sha"), e.get("after_sha")) if x)
    for c in commits:
        heads.add(c["sha"])
        logins = {((c.get("author") or {}).get("login") or "").lower(), ((c.get("committer") or {}).get("login") or "").lower()}
        if owner.lower() in logins and not AGENT_TRAILER_RE.search((c.get("commit") or {}).get("message") or ""):
            ambiguous.append("commit")
    heads.discard(None)
    return {
        "number": n, "base": pr["base"]["ref"], "created_at": fmt(created), "merged_at": fmt(merged),
        "merge_commit_sha": pr.get("merge_commit_sha"), "first_ready": fmt(ready),
        "v": FACTS_VERSION, "owner_definite": sorted(set(definite)), "owner_action_count": len(definite),
        "repair_cycles": repairs, "holds": holds,
        "labels": sorted(l["name"] for l in pr.get("labels") or []), "owner_ambiguous": sorted(set(ambiguous)),
        "heads": sorted(heads), "timeline_complete": tl_full, "commits_complete": c_full,
    }


def eligible(f, managed_base):
    """(True, None) or (False, reason). A merged PR was non-draft at merge, so the draft test always passes.
    Out: a base that is not the managed base or an integration base, or a label from EXCLUDE_LABELS."""
    for lab in f.get("labels") or []:
        if lab in EXCLUDE_LABELS:
            return False, EXCLUDE_LABELS[lab]
    b = f["base"]
    if b != managed_base and not b.startswith(INTEGRATION_BASES):
        return False, "unmanaged_base"
    return True, None


def autonomy(f, commits=True):
    """owner_action, unknown or autonomous for one PR's facts.

    commits=True (strict): an owner-identity commit with no agent trailer is ambiguous, so unknown.
    commits=False (GitHub-side only): commits are never an owner action, since agents commit under the
    owner's git identity; only the owner account's actions on GitHub (review, label, UI merge, comment) count."""
    if f["owner_definite"]:
        return "owner_action"
    if (commits and f["owner_ambiguous"]) or not f["timeline_complete"] or not f["commits_complete"]:
        return "unknown"
    return "autonomous"


# --------------------------------------------------------------- compute
def job_minutes(rd, run):
    """(self_hosted_min, hosted_min, complete) for one run, read once ever when it has completed."""
    rid = run["id"]
    cached = rd.load("jobs", str(rid))
    if cached:
        return cached["self_hosted"], cached["hosted"], True
    jobs, full = rd.pages(f"actions/runs/{rid}/jobs?filter=all", key="jobs", max_pages=3)
    sh = ho = 0.0
    for j in jobs:
        a, b = iso(j.get("started_at")), iso(j.get("completed_at"))
        if not a or not b or j.get("conclusion") == "skipped":
            continue
        m = max(0.0, (b - a).total_seconds() / 60.0)
        if "self-hosted" in (j.get("labels") or []):
            sh += m
        else:
            ho += m
    done = run.get("status") == "completed" and full
    if done:
        rd.save("jobs", str(rid), {"self_hosted": round(sh, 2), "hosted": round(ho, 2)})
    return round(sh, 2), round(ho, 2), done


def runs_index(rd, lo, hi):
    """Runs created in [lo, hi]: (list, complete). One read per page, shared by every PR in the window."""
    runs, full = rd.pages(f"actions/runs?created={fmt(lo)}..{fmt(hi)}", key="workflow_runs", max_pages=10)
    return runs, full


def pr_compute(rd, f, runs, runs_complete):
    heads = set(f["heads"])
    mg = re.compile(rf"^gh-readonly-queue/{re.escape(f['base'])}/pr-{f['number']}-")
    mine = {}
    for r in runs:
        if r.get("head_sha") in heads or (r.get("event") == "merge_group" and mg.match(r.get("head_branch") or "")):
            mine[r["id"]] = r
    sh = ho = 0.0
    complete = runs_complete
    for r in mine.values():
        a, b, ok = job_minutes(rd, r)
        sh += a
        ho += b
        complete = complete and ok
    return {"self_hosted_min": round(sh, 1), "hosted_min": round(ho, 1), "runs": len(mine), "complete": complete}


# ----------------------------------------------------------- correctness
def strip_app(ctx):
    return re.sub(r"\s*\(app_id=\d+\)$", "", ctx)


def required_contexts(rd, base, tick_ctx):
    if tick_ctx:
        return {strip_app(c) for c in tick_ctx}
    try:
        rules = rd.get(f"rules/branches/{base}")
    except ReadError:
        return None
    out = set()
    for r in rules if isinstance(rules, list) else []:
        if r.get("type") == "required_status_checks":
            out |= {c["context"] for c in (r.get("parameters") or {}).get("required_status_checks", [])}
    return out or None


def commit_red(rd, sha, required):
    """(red_contexts, settled). Cached once every check run has completed."""
    cached = rd.load("checks", sha)
    if cached is not None:
        return set(cached["red"]) & required, True
    runs, _ = rd.pages(f"commits/{sha}/check-runs", key="check_runs", max_pages=2)
    latest = {}
    for c in runs:
        k = c["name"]
        if k not in latest or (c.get("started_at") or "") > (latest[k].get("started_at") or ""):
            latest[k] = c
    settled = bool(latest) and all(c.get("status") == "completed" for c in latest.values())
    red = {k for k, c in latest.items() if c.get("conclusion") in BAD_CONCLUSIONS}
    if settled:
        rd.save("checks", sha, {"red": sorted(red)})
    return red & required, settled


def revert_of(commits, f):
    """A commit in the 7 days after the merge whose message starts `Revert` and names the merge sha or PR."""
    merged = iso(f["merged_at"])
    sha = (f.get("merge_commit_sha") or "")
    pat = re.compile(rf"(?<![\w#]){re.escape('#' + str(f['number']))}(?!\d)")
    for c in commits:
        t = iso(((c.get("commit") or {}).get("committer") or {}).get("date"))
        msg = (c.get("commit") or {}).get("message") or ""
        if not t or not (merged <= t <= merged + WINDOW) or c["sha"] == sha:
            continue
        if not msg.lstrip().lower().startswith("revert"):
            continue
        if (sha and sha[:7] in msg) or pat.search(msg):
            return c["sha"]
    return None


def poison_hit(ticks, repo, merged, until):
    """True when a tick of `repo` reports a base_poison red-everywhere episode that began in [merged, until]."""
    for began in ticks.get(repo, []):
        if merged <= began <= until:
            return True
    return False


def escaped_by(later, f):
    """Best effort: the number of a later PR, merged within 14 days after this one, whose title or body
    says it fixes or follows from this PR (`fix ... #N`, `regression from #N`). None when unreadable."""
    rx = re.compile(ESCAPE_RE % f["number"])
    merged = iso(f["merged_at"])
    for p in later:
        m = iso(p.get("merged_at"))
        if p["number"] == f["number"] or not m or not (merged < m <= merged + ESCAPE_WINDOW):
            continue
        if rx.search(f"{p.get('title') or ''}\n{p.get('body') or ''}"):
            return p["number"]
    return None


def pr_correct(rd, f, base_commits, commits_ok, required, ticks, now, later=None, later_ok=False):
    merged = iso(f["merged_at"])
    provisional = now < merged + WINDOW
    out = {"provisional": provisional, "reverted": None, "base_red": None, "bad": None,
           "escape_provisional": now < merged + ESCAPE_WINDOW, "escaped": None, "escaped_by": None}
    if later_ok:
        out["escaped_by"] = escaped_by(later or [], f)
        out["escaped"] = out["escaped_by"] is not None
    if commits_ok:
        out["reverted"] = revert_of(base_commits, f) is not None
    # base red: the merge commit and the next push to the base
    nxt = None
    if commits_ok and f.get("merge_commit_sha"):
        chron = sorted(base_commits, key=lambda c: ((c.get("commit") or {}).get("committer") or {}).get("date") or "")
        shas = [c["sha"] for c in chron]
        if f["merge_commit_sha"] in shas and shas.index(f["merge_commit_sha"]) + 1 < len(shas):
            nxt = chron[shas.index(f["merge_commit_sha"]) + 1]
    until = merged + dt.timedelta(hours=2)
    if nxt:
        until = max(until, iso(nxt["commit"]["committer"]["date"]))
    if poison_hit(ticks, rd.repo, merged, until):
        out["base_red"] = True
        out["base_red_source"] = "ticks"
    elif required and f.get("merge_commit_sha"):
        red, settled = commit_red(rd, f["merge_commit_sha"], required)
        if nxt and not red:
            red2, settled2 = commit_red(rd, nxt["sha"], required)
            red, settled = red | red2, settled and settled2
        if red:
            out["base_red"], out["base_red_source"] = True, "check_runs"
        elif settled:
            out["base_red"], out["base_red_source"] = False, "check_runs"
    if out["reverted"] or out["base_red"]:
        out["bad"] = True
    elif out["reverted"] is False and out["base_red"] is False:
        out["bad"] = False
    return out


# -------------------------------------------------------------- aggregate
def pctile(xs, p):
    xs = sorted(xs)
    return None if not xs else round(xs[max(0, math.ceil(p * len(xs)) - 1)], 1)


def wilson(k, n, z=1.96):
    """95% Wilson score interval for k of n, in percent, or None when n is 0."""
    if not n:
        return None
    p = k / n
    d = 1 + z * z / n
    c = (p + z * z / (2 * n)) / d
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return [round(100 * max(0.0, c - h), 1), round(100 * min(1.0, c + h), 1)]


def pct_of(a, b):
    return None if not b else round(100.0 * a / b, 1)


def summarize(rows):
    """The four metrics over a list of per-PR rows."""
    n = len(rows)
    auto = [r for r in rows if r["autonomy"] == "autonomous"]
    owner = [r for r in rows if r["autonomy"] == "owner_action"]
    unk = [r for r in rows if r["autonomy"] == "unknown"]
    g_auto = sum(1 for r in rows if r["autonomy_github_only"] == "autonomous")
    g_owner = sum(1 for r in rows if r["autonomy_github_only"] == "owner_action")
    hand = [r["handoff_min"] for r in rows if r["handoff_min"] is not None]
    comp = [r for r in rows if r["compute"] and r["compute"]["complete"]]
    good = [r for r in rows if r["correct"]["bad"] is False]
    bad = [r for r in rows if r["correct"]["bad"]]
    return {
        "merged": n,
        "autonomous": {"autonomous": len(auto), "owner_action": len(owner), "unknown": len(unk),
                       "rate_pct": None if not (auto or owner) else round(100.0 * len(auto) / (len(auto) + len(owner)), 1),
                       "unknown_share_pct": pct_of(len(unk), n),
                       "rate_ci95": wilson(len(auto), len(auto) + len(owner)), "n_determinable": len(auto) + len(owner),
                       "github_only": {"autonomous": g_auto, "owner_action": g_owner, "unknown": n - g_auto - g_owner,
                                       "rate_pct": None if not (g_auto or g_owner) else round(100.0 * g_auto / (g_auto + g_owner), 1),
                                       "rate_ci95": wilson(g_auto, g_auto + g_owner)}},
        "interventions": {
            "owner_actions_total": sum(r["owner_actions"] for r in rows),
            "repair_cycles_total": sum(r["repair_cycles"] for r in rows),
            "holds_total": sum(r["holds"] for r in rows),
            "prs_with_owner_action": sum(1 for r in rows if r["owner_actions"]),
            "prs_with_repair_cycle": sum(1 for r in rows if r["repair_cycles"]),
            "owner_actions_per_pr": None if not n else round(sum(r["owner_actions"] for r in rows) / n, 2),
            "repair_cycles_per_pr": None if not n else round(sum(r["repair_cycles"] for r in rows) / n, 2),
        },
        "handoff_min": {"n": len(hand), "median": None if not hand else round(statistics.median(hand), 1), "p90": pctile(hand, 0.9)},
        "compute_per_pr": {
            "prs_read": len(comp), "prs_unread_or_partial": n - len(comp),
            "self_hosted_min_total": round(sum(r["compute"]["self_hosted_min"] for r in comp), 1),
            "hosted_min_total": round(sum(r["compute"]["hosted_min"] for r in comp), 1),
            "self_hosted_min_median": None if not comp else round(statistics.median(r["compute"]["self_hosted_min"] for r in comp), 1),
            "hosted_min_median": None if not comp else round(statistics.median(r["compute"]["hosted_min"] for r in comp), 1),
        },
        "correctness": {
            "bad": len(bad), "good": len(good), "unknown": n - len(bad) - len(good),
            "reverted": sum(1 for r in rows if r["correct"]["reverted"]),
            "base_red": sum(1 for r in rows if r["correct"]["base_red"]),
            "provisional_prs": sum(1 for r in rows if r["correct"]["provisional"]),
            "bad_rate_pct": None if not (bad or good) else round(100.0 * len(bad) / (len(bad) + len(good)), 1),
            "bad_rate_ci95": wilson(len(bad), len(bad) + len(good)),
            "escaped": sum(1 for r in rows if r["correct"].get("escaped")),
            "escaped_known": sum(1 for r in rows if r["correct"].get("escaped") is not None),
            "escaped_rate_pct": pct_of(sum(1 for r in rows if r["correct"].get("escaped")), sum(1 for r in rows if r["correct"].get("escaped") is not None)),
            "escaped_rate_ci95": wilson(sum(1 for r in rows if r["correct"].get("escaped")), sum(1 for r in rows if r["correct"].get("escaped") is not None)),
            "escape_provisional_prs": sum(1 for r in rows if r["correct"].get("escape_provisional")),
            "provisional": any(r["correct"]["provisional"] for r in rows),
        },
    }


def merged_prs(rd, lo, hi):
    """Merged PRs with merged_at in [lo, hi], newest first, from the closed list. Returns (prs, complete)."""
    out, n = [], 0
    while n < 5:
        n += 1
        items = rd.get(f"pulls?state=closed&sort=updated&direction=desc&per_page=100&page={n}")
        if not isinstance(items, list):
            raise ReadError("unexpected shape")
        for p in items:
            m = iso(p.get("merged_at"))
            if m and lo <= m <= hi:
                out.append(p)
        if len(items) < 100 or (items and (iso(items[-1].get("updated_at")) or hi) < lo):
            return out, True
    return out, False


def collect_repo(repo, cache_dir, cap, owner, lo, hi, now, ticks, tick_ctx, fetch=None, managed_base="main"):
    rd = Reader(repo, cache_dir, cap, fetch)
    res = {"repo": repo, "rows": [], "unread": 0, "listing_complete": False, "reads": 0, "truncated": False, "errors": 0}
    try:
        prs, res["listing_complete"] = merged_prs(rd, lo, hi)
    except (Truncated, ReadError):
        prs = []
    rows, pending = [], []
    for pr in prs:
        f = rd.load("facts", str(pr["number"]))
        if f is not None and f.get("v") != FACTS_VERSION:
            f = None  # cached by an older reduction: read again once
        if f is None:
            try:
                f = pr_facts(rd, pr, owner)
                rd.save("facts", str(pr["number"]), f)
            except (Truncated, ReadError):
                res["unread"] += 1
                continue
        rows.append({"f": f, "pr": pr})
    # compute: cached per PR, else read from one shared run listing
    need = [r for r in rows if rd.load("compute", str(r["f"]["number"])) is None]
    runs, runs_ok = [], False
    if need:
        try:
            runs, runs_ok = runs_index(rd, min(iso(r["f"]["created_at"]) for r in need), max(iso(r["f"]["merged_at"]) for r in need) + dt.timedelta(hours=1))
        except (Truncated, ReadError):
            runs, runs_ok = [], False
    base_commits, commits_ok = [], False
    if rows:
        try:
            base = rows[0]["f"]["base"]
            base_commits, commits_ok = rd.pages(
                f"commits?sha={base}&since={fmt(min(iso(r['f']['merged_at']) for r in rows))}&until={fmt(max(iso(r['f']['merged_at']) for r in rows) + WINDOW + dt.timedelta(hours=2))}",
                max_pages=5)
        except (Truncated, ReadError):
            pass
    req = None
    if rows:
        try:
            req = required_contexts(rd, rows[0]["f"]["base"], tick_ctx.get(repo))
        except (Truncated, ReadError):
            pass
    out = []
    for r in rows:
        f = r["f"]
        comp = rd.load("compute", str(f["number"]))
        if comp is None and runs_ok:
            try:
                comp = pr_compute(rd, f, runs, runs_ok)
                if comp["complete"]:
                    rd.save("compute", str(f["number"]), comp)
            except (Truncated, ReadError):
                comp = None
        corr = rd.load("correct", str(f["number"]))
        if corr is None:
            try:
                corr = pr_correct(rd, f, base_commits, commits_ok, req, ticks, now, prs, res["listing_complete"])
                if now >= iso(f["merged_at"]) + ESCAPE_WINDOW and corr["bad"] is not None and corr["escaped"] is not None:
                    rd.save("correct", str(f["number"]), corr)
            except (Truncated, ReadError):
                corr = {"provisional": True, "reverted": None, "base_red": None, "bad": None, "escaped": None, "escaped_by": None}
        if corr.get("escaped") is None and res["listing_complete"]:  # independent of the base reads
            corr["escaped_by"] = escaped_by(prs, f)
            corr["escaped"] = corr["escaped_by"] is not None
        corr["provisional"] = now < iso(f["merged_at"]) + WINDOW  # age is relative to this run
        corr["escape_provisional"] = now < iso(f["merged_at"]) + ESCAPE_WINDOW
        ok, why = eligible(f, managed_base)
        out.append({
            "pr": f["number"], "day": f["first_ready"][:10], "merge_day": f["merged_at"][:10],
            "eligible": ok, "excluded_because": why,
            "owner_actions": f.get("owner_action_count", len(f["owner_definite"])),
            "repair_cycles": f.get("repair_cycles", 0), "holds": f.get("holds", 0), "autonomy": autonomy(f), "autonomy_github_only": autonomy(f, False),
            "handoff_min": round((iso(f["merged_at"]) - iso(f["first_ready"])).total_seconds() / 60.0, 1),
            "compute": comp, "correct": corr,
        })
    res.update(rows=out, reads=rd.calls, truncated=rd.truncated, errors=rd.errors)
    return res


def build(repos, cache_dir, cap, owner, start, now, ticks, tick_ctx, baseline=None, fetch=None, bases=None):
    """The `end_to_end` report section. `ticks`: repo -> [datetimes a base_poison red-everywhere episode began]."""
    lo = start
    if baseline:
        lo = min(lo, baseline[0])
    per_cap = max(1, cap // max(1, len(repos)))
    out = {"owner": owner, "read_cap_per_repo": per_cap, "repos": {}, "truncated": False,
           "coverage": {"status": "available", "reason": ""}, "baseline": None}
    notes = []
    for repo in repos:
        r = collect_repo(repo, cache_dir, per_cap, owner, lo, now, now, ticks, tick_ctx, fetch, (bases or {}).get(repo, "main"))
        days = {}
        excluded = {}
        for row in r["rows"]:
            if not row["eligible"]:
                excluded[row["excluded_because"]] = excluded.get(row["excluded_because"], 0) + 1
                continue
            days.setdefault(row["day"], []).append(row)
        out["repos"][repo] = {
            "per_day": {d: summarize(v) for d, v in sorted(days.items())}, "prs": r["rows"], "excluded": excluded,
            "unread_prs": r["unread"], "listing_complete": r["listing_complete"],
            "reads": r["reads"], "truncated": r["truncated"], "read_errors": r["errors"],
        }
        out["truncated"] = out["truncated"] or r["truncated"]
        if r["truncated"] or r["unread"] or not r["listing_complete"] or r["errors"]:
            notes.append(f"{repo}: truncated={r['truncated']} unread={r['unread']} listing_complete={r['listing_complete']} read_errors={r['errors']}")
    if notes:
        out["coverage"] = {"status": "partial", "reason": "; ".join(notes)}
    ds = start.strftime("%Y-%m-%d")
    cohort = lambda v, a, b: summarize([p for p in v["prs"] if p["eligible"] and a <= p["day"] <= b])  # noqa: E731
    out["window"] = {"from": ds, "to": now.strftime("%Y-%m-%d"),
                     "per_repo": {repo: cohort(v, ds, "9999") for repo, v in out["repos"].items()}}
    if baseline:
        b0, b1 = (x.strftime("%Y-%m-%d") for x in baseline)
        out["baseline"] = {"from": b0, "to": b1, "per_repo": {repo: cohort(v, b0, b1) for repo, v in out["repos"].items()}}
    return out


def ci(x):
    return "n/a" if not x else f"{x[0]}-{x[1]}"


def markdown(e2e):
    L = ["", "## End-to-end per eligible PR (cohort = UTC day the PR became ready)", "",
         "Eligible: a merged PR on the managed base or an integration base, without the `automation`, "
         "`shipyard:unmanaged` or `superseded` label (docs/metrics.md). Rates carry a 95% Wilson interval and n; "
         "no per-change attribution is made.",
         f"Autonomous merge rate: eligible PRs with no owner action (review, label, comment, merge or close by {e2e['owner']}'s own account; "
         "a commit under that identity with no agent trailer makes the PR unknown). `gh-only` ignores commits. "
         "Handoff-to-merge: first ready moment to merge, median and p90 minutes. Compute: job minutes on the PR's heads and merge-group runs, "
         "self-hosted/hosted. Correctness: revert or red required check on the base within 7 days (provisional while open); "
         "escaped defect: a later PR within 14 days whose title or body names this one as the cause (best effort).", "",
         f"Coverage: {e2e['coverage']['status']} {e2e['coverage']['reason']}" + (" (TRUNCATED at the read cap)" if e2e["truncated"] else ""), ""]
    head = ("| {0} | n | autonomous a/o/unknown | rate (CI95) | unknown share | gh-only rate (CI95) | handoff median/p90 min | "
            "compute sh/hosted median min (PRs read) | owner actions/PR | repair cycles/PR | correctness bad/good/unknown | bad rate (CI95) | escaped rate (CI95) |")
    sep = "|" + "---|" * 13

    def row(label, s):
        a, h, c, k, iv = s["autonomous"], s["handoff_min"], s["compute_per_pr"], s["correctness"], s["interventions"]
        g = a["github_only"]
        prov = " provisional" if k["provisional"] else ""
        esc = f"{k['escaped_rate_pct']}% ({ci(k['escaped_rate_ci95'])}), n={k['escaped_known']}" + (" provisional" if k["escape_provisional_prs"] else "")
        return (f"| {label} | {s['merged']} | {a['autonomous']}/{a['owner_action']}/{a['unknown']} | {a['rate_pct']}% ({ci(a['rate_ci95'])}), n={a['n_determinable']} | "
                f"{a['unknown_share_pct']}% | {g['rate_pct']}% ({ci(g['rate_ci95'])}) | {h['median']}/{h['p90']} (n={h['n']}) | "
                f"{c['self_hosted_min_median']}/{c['hosted_min_median']} ({c['prs_read']}) | {iv['owner_actions_per_pr']} | {iv['repair_cycles_per_pr']} | "
                f"{k['bad']}/{k['good']}/{k['unknown']} | {k['bad_rate_pct']}% ({ci(k['bad_rate_ci95'])}){prov} | {esc} |")
    for repo, v in sorted(e2e["repos"].items()):
        L += [f"### {repo}", ""]
        if v.get("excluded"):
            L += ["Excluded from the cohort: " + ", ".join(f"{k} {n}" for k, n in sorted(v["excluded"].items())), ""]
        L += [head.format("ready day"), sep] + [row(d, s) for d, s in v["per_day"].items()] + [""]
    w, b = e2e.get("window"), e2e.get("baseline")
    for title, blk in (("Window", w), ("Baseline", b)):
        if blk:
            L += [f"## {title} {blk['from']} to {blk['to']} (pooled cohort)", "", head.format("repo"), sep]
            L += [row(repo, s) for repo, s in sorted(blk["per_repo"].items())] + [""]
    return "\n".join(L) + "\n"
