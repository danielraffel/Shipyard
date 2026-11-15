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
    for e in tl:
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
        "owner_definite": sorted(set(definite)), "owner_ambiguous": sorted(set(ambiguous)),
        "heads": sorted(heads), "timeline_complete": tl_full, "commits_complete": c_full,
    }


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


def pr_correct(rd, f, base_commits, commits_ok, required, ticks, now):
    merged = iso(f["merged_at"])
    provisional = now < merged + WINDOW
    out = {"provisional": provisional, "reverted": None, "base_red": None, "bad": None}
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
                       "github_only": {"autonomous": g_auto, "owner_action": g_owner, "unknown": n - g_auto - g_owner,
                                       "rate_pct": None if not (g_auto or g_owner) else round(100.0 * g_auto / (g_auto + g_owner), 1)}},
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


def collect_repo(repo, cache_dir, cap, owner, lo, hi, now, ticks, tick_ctx, fetch=None):
    rd = Reader(repo, cache_dir, cap, fetch)
    res = {"repo": repo, "rows": [], "unread": 0, "listing_complete": False, "reads": 0, "truncated": False, "errors": 0}
    try:
        prs, res["listing_complete"] = merged_prs(rd, lo, hi)
    except (Truncated, ReadError):
        prs = []
    rows, pending = [], []
    for pr in prs:
        f = rd.load("facts", str(pr["number"]))
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
                corr = pr_correct(rd, f, base_commits, commits_ok, req, ticks, now)
                if not corr["provisional"] and corr["bad"] is not None:
                    rd.save("correct", str(f["number"]), corr)
            except (Truncated, ReadError):
                corr = {"provisional": now < iso(f["merged_at"]) + WINDOW, "reverted": None, "base_red": None, "bad": None}
        corr["provisional"] = now < iso(f["merged_at"]) + WINDOW  # age is relative to this run
        out.append({
            "pr": f["number"], "day": f["merged_at"][:10], "autonomy": autonomy(f), "autonomy_github_only": autonomy(f, False),
            "handoff_min": round((iso(f["merged_at"]) - iso(f["first_ready"])).total_seconds() / 60.0, 1),
            "compute": comp, "correct": corr,
        })
    res.update(rows=out, reads=rd.calls, truncated=rd.truncated, errors=rd.errors)
    return res


def build(repos, cache_dir, cap, owner, start, now, ticks, tick_ctx, baseline=None, fetch=None):
    """The `end_to_end` report section. `ticks`: repo -> [datetimes a base_poison red-everywhere episode began]."""
    lo = start
    if baseline:
        lo = min(lo, baseline[0])
    per_cap = max(1, cap // max(1, len(repos)))
    out = {"owner": owner, "read_cap_per_repo": per_cap, "repos": {}, "truncated": False,
           "coverage": {"status": "available", "reason": ""}, "baseline": None}
    notes = []
    for repo in repos:
        r = collect_repo(repo, cache_dir, per_cap, owner, lo, now, now, ticks, tick_ctx, fetch)
        days = {}
        for row in r["rows"]:
            days.setdefault(row["day"], []).append(row)
        out["repos"][repo] = {
            "per_day": {d: summarize(v) for d, v in sorted(days.items())}, "prs": r["rows"],
            "unread_prs": r["unread"], "listing_complete": r["listing_complete"],
            "reads": r["reads"], "truncated": r["truncated"], "read_errors": r["errors"],
        }
        out["truncated"] = out["truncated"] or r["truncated"]
        if r["truncated"] or r["unread"] or not r["listing_complete"] or r["errors"]:
            notes.append(f"{repo}: truncated={r['truncated']} unread={r['unread']} listing_complete={r['listing_complete']} read_errors={r['errors']}")
    if notes:
        out["coverage"] = {"status": "partial", "reason": "; ".join(notes)}
    if baseline:
        b0, b1 = baseline
        out["baseline"] = {"from": b0.strftime("%Y-%m-%d"), "to": b1.strftime("%Y-%m-%d"), "per_repo": {
            repo: summarize([p for p in v["prs"] if b0.strftime("%Y-%m-%d") <= p["day"] <= b1.strftime("%Y-%m-%d")])
            for repo, v in out["repos"].items()}}
    return out


def markdown(e2e):
    L = ["", "## End-to-end per merged PR (by merge day, UTC)", "",
         "Autonomous merge rate: merged PRs with no owner action (a commit, review, label, comment, merge or close by "
         f"{e2e['owner']}'s own account) from opening to merge, over PRs where that is determinable; the rest are unknown (a commit under the owner's identity with no agent trailer). "
         "The `gh-only` figure ignores commits and counts only GitHub-side owner actions. "
         "Handoff-to-merge: first ready-for-review moment to merged_at, median and p90 minutes. "
         "Compute: Actions job minutes on the PR's heads plus its merge-group runs, self-hosted and hosted. "
         "Correctness: share of merged PRs with a revert or a red required check on the base within 7 days; "
         "provisional while the 7 days are open.", "",
         f"Coverage: {e2e['coverage']['status']} {e2e['coverage']['reason']}" + (" (TRUNCATED at the read cap)" if e2e["truncated"] else ""), ""]
    head = "| day | merged | autonomous (a/o/unknown, rate) | handoff median/p90 | compute sh/hosted median min (PRs read) | correctness (bad/good/unknown, rate) |"
    sep = "|---|---|---|---|---|---|"

    def row(label, s):
        a, h, c, k = s["autonomous"], s["handoff_min"], s["compute_per_pr"], s["correctness"]
        prov = " provisional" if k["provisional"] else ""
        return (f"| {label} | {s['merged']} | {a['autonomous']}/{a['owner_action']}/{a['unknown']}, {a['rate_pct']}% | "
                f"{h['median']}/{h['p90']} | {c['self_hosted_min_median']}/{c['hosted_min_median']} ({c['prs_read']}) | "
                f"{k['bad']}/{k['good']}/{k['unknown']}, {k['bad_rate_pct']}%{prov} |")
    for repo, v in sorted(e2e["repos"].items()):
        L += [f"### {repo}", "", head, sep] + [row(d, s) for d, s in v["per_day"].items()] + [""]
    b = e2e.get("baseline")
    if b:
        L += [f"## Baseline {b['from']} to {b['to']} (pooled)", "", head.replace("| day", "| repo"), sep]
        L += [row(repo, s) for repo, s in sorted(b["per_repo"].items())] + [""]
    return "\n".join(L) + "\n"
