#!/usr/bin/env python3
"""Reconstruct blocked episodes from the steward's per-tick JSON and price them.

Read-only. Standard library only. Two subcommands:

  fetch    read GitHub (REST, bulk list endpoints) into a disk cache, with a
           hard call budget. Uses `ghread api` for Generous-Corp/* and
           `ghapp api` for danielraffel/*. Never writes to GitHub.
  analyze  build episodes from tick files and join the cache, state files and
           the metrics store. Prints markdown tables and writes JSON.

An EPISODE is one contiguous interval in which one (repo, PR, head, blocked_on
token) held. Repeated tick observations collapse to one episode. A gap in
observations breaks an episode only when the repo's tick ran and the PR was
absent from it; a missing tick file is "no information" and does not break it.

Every cost component carries a coverage tag: available, partial, unavailable.
Unavailable is never reported as zero. Elapsed time is context, never cost.

Usage:
  loss_baseline.py fetch   --ticks DIR --cache DIR [--budget 400] [--sample-heads 120]
  loss_baseline.py analyze --ticks DIR --cache DIR [--state DIR] [--metrics-db F]
                           [--summary-log F] [--json-out F]
"""
from __future__ import annotations

import argparse
import collections
import datetime as dt
import glob
import hashlib
import json
import os
import re
import sqlite3
import statistics
import subprocess
import sys

TS_RE = re.compile(r"-(\d{8}T\d{6}Z)\.json$")
UTC = dt.timezone.utc


def parse_stamp(s: str) -> dt.datetime:
    return dt.datetime.strptime(s, "%Y%m%dT%H%M%SZ").replace(tzinfo=UTC)


def parse_iso(s: str | None) -> dt.datetime | None:
    if not s:
        return None
    s = s.replace("Z", "+00:00")
    try:
        d = dt.datetime.fromisoformat(s)
    except ValueError:
        return None
    return d if d.tzinfo else d.replace(tzinfo=UTC)


def family(token: str) -> str:
    return token.split(":", 1)[0]


# --------------------------------------------------------------------------
# Token classification (documented mapping; used when next_action is the only
# evidence). `next_action.actor` is persisted per row and is used first.
# --------------------------------------------------------------------------
INTENTIONAL = {"draft"}
SYSTEM_WAIT = {
    "queued", "admitted", "ready", "deferred_pass_budget", "revalidating",
    "transient_rerun", "carried_commits_over_cap", "carried_commits_unreadable",
}
OWNER_STOP = {"excluded_label", "direct_merge_refused", "owner_decision"}


def classify(token: str, actor: str | None, stop: bool | None) -> str:
    """normal_wait | intentional | actionable | human_stop"""
    fam = family(token)
    if fam in INTENTIONAL:
        return "intentional"
    if stop or actor == "owner" or fam in OWNER_STOP:
        return "human_stop"
    if fam in SYSTEM_WAIT or actor in ("steward", "nobody", "ci"):
        return "normal_wait"
    # author / reviewer / repair / operator named as the one who must act
    return "actionable"


# --------------------------------------------------------------------------
# Episodes
# --------------------------------------------------------------------------
class Obs:
    __slots__ = ("t", "head", "token", "actor", "stop", "since", "detail", "pr", "extra")

    def __init__(self, t, head, token, actor, stop, since, detail, pr, extra):
        self.t, self.head, self.token, self.actor = t, head, token, actor
        self.stop, self.since, self.detail, self.pr, self.extra = stop, since, detail, pr, extra


def load_ticks(ticks_dir: str):
    """Return {repo: [(t, {pr: Obs})]} sorted by time, plus load stats."""
    per_repo: dict[str, list] = collections.defaultdict(list)
    stats = {"files": 0, "unreadable": 0, "no_pr_list": 0}
    for f in sorted(glob.glob(os.path.join(ticks_dir, "*.json"))):
        m = TS_RE.search(f)
        if not m:
            continue
        stats["files"] += 1
        try:
            d = json.load(open(f))
        except Exception:
            stats["unreadable"] += 1
            continue
        t = parse_stamp(m.group(1))
        for r in d.get("repos", []):
            if "prs" not in r or r.get("errors"):
                stats["no_pr_list"] += 1
                continue
            rows = {}
            for p in r["prs"]:
                na = p.get("next_action") or {}
                rows[p["number"]] = Obs(
                    t, p.get("head_sha"), p.get("blocked_on") or "unknown",
                    na.get("actor"), na.get("stop"), parse_iso(p.get("since")),
                    p.get("blocked_detail"), p["number"],
                    {"held_by": p.get("held_by"), "superseded": p.get("superseded"),
                     "since_source": p.get("since_source")},
                )
            per_repo[r["repo"]].append((t, rows))
    for v in per_repo.values():
        v.sort(key=lambda x: x[0])
    return per_repo, stats


def build_episodes(per_repo):
    episodes = []
    window = {}
    for repo, ticks in per_repo.items():
        window[repo] = (ticks[0][0], ticks[-1][0])
        open_ep: dict[int, dict] = {}
        for idx, (t, rows) in enumerate(ticks):
            # close episodes whose PR vanished from this tick
            for pr in list(open_ep):
                if pr not in rows:
                    ep = open_ep.pop(pr)
                    ep["end"] = t
                    ep["end_kind"] = "disappeared"
                    episodes.append(ep)
            for pr, o in rows.items():
                ep = open_ep.get(pr)
                if ep and ep["head"] == o.head and ep["token"] == o.token:
                    ep["last_obs"] = t
                    ep["n_obs"] += 1
                    continue
                if ep:  # token or head changed
                    ep["end"] = t
                    ep["end_kind"] = "new_head" if ep["head"] != o.head else "same_head"
                    ep["next_token"] = o.token
                    ep["next_actor"] = o.actor
                    ep["next_head"] = o.head
                    episodes.append(ep)
                open_ep[pr] = {
                    "repo": repo, "pr": pr, "head": o.head, "token": o.token,
                    "family": family(o.token), "actor": o.actor, "stop": o.stop,
                    "since": o.since, "first_obs": t, "last_obs": t, "n_obs": 1,
                    "detail": o.detail, "held_by": o.extra.get("held_by"),
                    "superseded": bool(o.extra.get("superseded")),
                    "klass": classify(o.token, o.actor, o.stop),
                    "end": None, "end_kind": None,
                }
        for pr, ep in open_ep.items():
            ep["end"] = None
            ep["end_kind"] = "open_at_end"
            episodes.append(ep)
    return episodes, window


def elapsed_minutes(ep, window_start):
    start = ep["since"] or ep["first_obs"]
    left_censored = ep["first_obs"] <= window_start + dt.timedelta(minutes=15)
    end = ep["end"] or ep["last_obs"]
    return max(0.0, (end - start).total_seconds() / 60.0), left_censored


# --------------------------------------------------------------------------
# GitHub cache
# --------------------------------------------------------------------------
def tool_for(repo: str) -> list[str]:
    return ["ghread", "api"] if repo.startswith("Generous-Corp/") else ["ghapp", "api"]


class Gh:
    def __init__(self, cache_dir, budget, fetch):
        self.dir = os.path.join(cache_dir, "gh")
        os.makedirs(self.dir, exist_ok=True)
        self.budget, self.fetch, self.calls_file = budget, fetch, os.path.join(self.dir, "_calls.json")
        try:
            self.calls = json.load(open(self.calls_file))
        except Exception:
            self.calls = []

    def _path(self, repo, endpoint):
        h = hashlib.sha1(f"{repo}|{endpoint}".encode()).hexdigest()[:16]
        return os.path.join(self.dir, f"{repo.replace('/', '_')}__{h}.json")

    def get(self, repo, endpoint, allow_fetch=True):
        p = self._path(repo, endpoint)
        if os.path.exists(p):
            try:
                return json.load(open(p))
            except Exception:
                pass
        if not (self.fetch and allow_fetch):
            return None
        if len(self.calls) >= self.budget:
            return "BUDGET"
        out = subprocess.run(tool_for(repo) + [endpoint], capture_output=True, text=True, timeout=120)
        self.calls.append({"repo": repo, "endpoint": endpoint, "rc": out.returncode,
                           "at": dt.datetime.now(UTC).isoformat(timespec="seconds")})
        json.dump(self.calls, open(self.calls_file, "w"))
        if out.returncode != 0:
            return None
        try:
            data = json.loads(out.stdout)
        except Exception:
            return None
        json.dump(data, open(p, "w"))
        return data

    def paged(self, repo, base, stop, max_pages=30):
        """Fetch pages until `stop(page_items)`, a short page, or max_pages. Returns (items, status)."""
        items, page = [], 1
        while True:
            if page > max_pages:
                return items, "page_cap"
            data = self.get(repo, f"{base}&per_page=100&page={page}")
            if data == "BUDGET" or data is None:
                return items, ("budget" if data == "BUDGET" else "error" if self.fetch else "uncached")
            items.extend(data)
            if len(data) < 100 or stop(data):
                return items, "ok"
            page += 1


def run_fetch(a):
    per_repo, _ = load_ticks(a.ticks)
    episodes, window = build_episodes(per_repo)
    gh = Gh(a.cache, a.budget, True)
    win_start = min(w[0] for w in window.values())
    horizon = a.horizon
    log = []
    # 1. closed PR lists (final state, merged_at, labels) back to the horizon
    for repo in sorted(per_repo):
        items, st = gh.paged(
            repo, f"repos/{repo}/pulls?state=closed&sort=updated&direction=desc",
            lambda page: (parse_iso(page[-1]["updated_at"]) or win_start) < horizon,
        )
        log.append((repo, "closed_pr_list", len(items), st))
    # 2. issue comments in the tick window (repaired:/dispute:/verdicts)
    since = win_start.strftime("%Y-%m-%dT%H:%M:%SZ")
    for repo in sorted(per_repo):
        if a.skip_comments_for and repo in a.skip_comments_for:
            continue
        items, st = gh.paged(repo, f"repos/{repo}/issues/comments?since={since}", lambda page: False)
        log.append((repo, "comments", len(items), st))
    # 3. PRs that vanished from ticks but are not in any closed list
    closed = load_closed(gh, per_repo, allow_fetch=False)
    missing = sorted({(e["repo"], e["pr"]) for e in episodes
                      if e["end_kind"] == "disappeared" and (e["repo"], e["pr"]) not in closed})
    got = 0
    for repo, pr in missing:
        d = gh.get(repo, f"repos/{repo}/pulls/{pr}")
        if d == "BUDGET":
            break
        got += 1
    log.append(("*", "single_pr_lookups", got, f"of {len(missing)} missing"))
    # 4. per-head runs for a stratified sample of actionable heads
    heads = sample_heads(episodes, a.sample_heads)
    n = 0
    for repo, head in heads:
        d = gh.get(repo, f"repos/{repo}/actions/runs?head_sha={head}&per_page=100")
        if d == "BUDGET":
            break
        n += 1
    log.append(("*", "head_runs_sample", n, f"of {len(heads)} sampled"))
    for row in log:
        print("fetch:", *row)
    print("api calls used:", len(gh.calls), "of budget", a.budget)


def sample_heads(episodes, n):
    """Deterministic stratified sample: round-robin over (repo, family) of actionable episodes."""
    buckets = collections.defaultdict(list)
    seen = set()
    for e in sorted(episodes, key=lambda e: (e["repo"], e["pr"], e["first_obs"])):
        if e["klass"] != "actionable" or not e["head"]:
            continue
        k = (e["repo"], e["head"])
        if k in seen:
            continue
        seen.add(k)
        buckets[(e["repo"], e["family"])].append(k)
    out, i = [], 0
    keys = sorted(buckets)
    while len(out) < n and any(buckets.values()):
        for k in keys:
            if buckets[k] and len(out) < n:
                out.append(buckets[k].pop(0))
        i += 1
    return out


def load_closed(gh, per_repo, allow_fetch=False):
    """{(repo, pr): {state, merged, closed_at, merged_at, labels}} from cached lists + singles."""
    res = {}
    for repo in per_repo:
        page = 1
        while True:
            d = gh.get(repo, f"repos/{repo}/pulls?state=closed&sort=updated&direction=desc&per_page=100&page={page}", allow_fetch=False)
            if not isinstance(d, list):
                break
            for p in d:
                res[(repo, p["number"])] = {
                    "state": "merged" if p.get("merged_at") else "closed_unmerged",
                    "closed_at": parse_iso(p.get("closed_at")), "merged_at": parse_iso(p.get("merged_at")),
                    "labels": [l["name"] for l in p.get("labels", [])],
                }
            page += 1
            if len(d) < 100:
                break
    # singles
    for f in glob.glob(os.path.join(gh.dir, "*.json")):
        if f.endswith("_calls.json"):
            continue
    for repo in per_repo:
        pass
    return res


def load_singles(gh, keys):
    res = {}
    for repo, pr in keys:
        d = gh.get(repo, f"repos/{repo}/pulls/{pr}", allow_fetch=False)
        if isinstance(d, dict) and "number" in d:
            res[(repo, pr)] = {
                "state": "merged" if d.get("merged_at") else ("open" if d.get("state") == "open" else "closed_unmerged"),
                "closed_at": parse_iso(d.get("closed_at")), "merged_at": parse_iso(d.get("merged_at")),
                "labels": [l["name"] for l in d.get("labels", [])],
            }
    return res


def load_comments(gh, repos):
    """{repo: [comment]} and per-repo coverage."""
    out, cov = {}, {}
    sample_since = None
    for repo in repos:
        items, page, ok = [], 1, False
        while True:
            # the `since` value is part of the cached endpoint; find any cached variant
            d = None
            for f in glob.glob(os.path.join(gh.dir, f"{repo.replace('/', '_')}__*.json")):
                try:
                    x = json.load(open(f))
                except Exception:
                    continue
                if isinstance(x, list) and x and isinstance(x[0], dict) and "issue_url" in x[0]:
                    d = d or []
                    d.extend(x)
            items = d or []
            break
        out[repo] = items
        cov[repo] = "available" if items else "unavailable"
    return out, cov


VERDICT_RE = re.compile(r"verdict:\s*(APPROVE|HOLD)\s+head:\s*([0-9a-f]{7,40})", re.I)
INTERV_RE = re.compile(r"^\s*(repaired|dispute)\s*:", re.I | re.M)
NODEFECT_RE = re.compile(
    r"(could not|cannot|can't|unable to|failed to)\s+(read|access|open|review|load).{0,60}(workspace|diff|checkout|repo|file)|"
    r"review workspace|no workspace|UNVERIFIED", re.I)


def pr_of(c):
    m = re.search(r"/issues/(\d+)$", c.get("issue_url", ""))
    return int(m.group(1)) if m else None


# --------------------------------------------------------------------------
# Analysis
# --------------------------------------------------------------------------
def pct(n, d):
    return "n/a" if d == 0 else f"{100.0 * n / d:.0f}%"


def table(headers, rows):
    out = ["| " + " | ".join(headers) + " |", "|" + "|".join(["---"] * len(headers)) + "|"]
    for r in rows:
        out.append("| " + " | ".join(str(x) for x in r) + " |")
    return "\n".join(out)


def run_analyze(a):
    per_repo, lstats = load_ticks(a.ticks)
    episodes, window = build_episodes(per_repo)
    gh = Gh(a.cache, 0, False)
    win_start = min(w[0] for w in window.values())
    win_end = max(w[1] for w in window.values())
    closed = load_closed(gh, per_repo)
    # PR final outcome (only for PRs that left the tick lists, or any closed in cache)
    singles = load_singles(gh, {(e["repo"], e["pr"]) for e in episodes if e["end_kind"] == "disappeared"})
    outcome = {}
    for k in {(e["repo"], e["pr"]) for e in episodes}:
        o = closed.get(k) or singles.get(k)
        outcome[k] = o["state"] if o else None
    last_pr_state = {}
    for repo, ticks in per_repo.items():
        last_pr_state[repo] = set(ticks[-1][1])
    for e in episodes:
        k = (e["repo"], e["pr"])
        oc = outcome[k]
        if oc is None:
            oc = "open" if e["pr"] in last_pr_state[e["repo"]] else "unresolved"
        e["pr_outcome"] = oc
        # a PR reopened after close would appear open in ticks; ticks win
        if e["pr"] in last_pr_state[e["repo"]]:
            e["pr_outcome"] = "open"
    # comments
    comments, ccov = load_comments(gh, per_repo)
    by_pr_comments = collections.defaultdict(list)
    for repo, cs in comments.items():
        for c in cs:
            n = pr_of(c)
            if n is not None:
                by_pr_comments[(repo, n)].append(c)
    # downstream: children carrying a parent's unapproved commits
    children = collections.defaultdict(set)
    for e in episodes:
        if e["family"] == "carries_unapproved_commits":
            m = re.search(r"#(\d+)", e["token"])
            if m:
                children[(e["repo"], int(m.group(1)))].add(e["pr"])
    # metrics.db local-gate runs per head (pulp only; local validation, not CI)
    local_runs = {}
    if a.metrics_db and os.path.exists(a.metrics_db):
        db = sqlite3.connect(f"file:{a.metrics_db}?mode=ro", uri=True)
        for sha, n, fails, ms in db.execute(
            "select r.sha, count(distinct r.id), sum(r.status='failure'), "
            "(select sum(j.total_ms) from jobs j where j.run_id in (select id from runs where sha=r.sha)) "
            "from runs r where r.sha is not null group by r.sha"
        ):
            local_runs[sha] = (n, fails or 0, ms or 0)
    # per-head GitHub runs (sample)
    head_runs = {}
    for repo in per_repo:
        for f in glob.glob(os.path.join(gh.dir, f"{repo.replace('/', '_')}__*.json")):
            try:
                x = json.load(open(f))
            except Exception:
                continue
            if isinstance(x, dict) and "workflow_runs" in x and x["workflow_runs"]:
                head = x["workflow_runs"][0]["head_sha"]
                head_runs[(repo, head)] = x["workflow_runs"]
    sampled_heads = set(head_runs)

    # assign each comment to at most ONE episode of its PR (no double counting)
    eps_by_pr = collections.defaultdict(list)
    for e in episodes:
        e["_c_interv"] = 0
        e["_c_owner"] = 0
        e["_c_all"] = 0
        eps_by_pr[(e["repo"], e["pr"])].append(e)
    unassigned = collections.Counter()
    for k, cs in by_pr_comments.items():
        for c in cs:
            t = parse_iso(c["created_at"])
            cand = [e for e in eps_by_pr.get(k, []) if (e["since"] or e["first_obs"]) <= t <= (e["end"] or e["last_obs"]) + dt.timedelta(minutes=10)]
            kind = "interv" if INTERV_RE.search(c.get("body") or "") else ("owner" if (c.get("user") or {}).get("login") == "danielraffel" else None)
            if not cand:
                if kind:
                    unassigned[kind] += 1
                continue
            cand.sort(key=lambda e: (e["klass"] != "actionable", e["first_obs"]))
            e = cand[0]
            e["_c_all"] += 1
            if kind == "interv":
                e["_c_interv"] += 1
            elif kind == "owner":
                e["_c_owner"] += 1

    # per-episode metrics ---------------------------------------------------
    claimed_heads = set()
    for e in sorted(episodes, key=lambda x: x["first_obs"]):
        mins, lc = elapsed_minutes(e, window[e["repo"]][0])
        e["elapsed_min"], e["left_censored"] = mins, lc
        k = (e["repo"], e["pr"])
        lo = e["since"] or e["first_obs"]
        hi = e["end"] or e["last_obs"]
        e["repaired_or_dispute"] = e["_c_interv"]
        e["owner_acct_comments"] = e["_c_owner"]
        e["comments_in_window"] = e["_c_all"]
        e["comment_cov"] = ccov.get(e["repo"], "unavailable")
        e["downstream"] = len(children.get(k, ())) if e["klass"] == "actionable" else 0
        # same-head clear => nobody pushed
        e["cleared_no_push"] = e["end_kind"] == "same_head"
        # runs on this head (sample)
        runs = head_runs.get((e["repo"], e["head"]))
        if (e["repo"], e["head"]) in claimed_heads:
            runs = None  # a head's run cost is attributed to its first episode only
        elif runs is not None and e["klass"] == "actionable":
            claimed_heads.add((e["repo"], e["head"]))
        else:
            runs = None
        e["runs_sampled"] = runs is not None
        if runs is not None:
            wasted, reruns = 0.0, 0
            for r in runs:
                s, u = parse_iso(r.get("run_started_at")), parse_iso(r.get("updated_at"))
                if r.get("conclusion") in ("failure", "cancelled", "timed_out") and s and u:
                    wasted += (u - s).total_seconds() / 60.0
                reruns += max(0, (r.get("run_attempt") or 1) - 1)
            e["wasted_run_min"], e["rerun_attempts"] = wasted, reruns
        lr = local_runs.get(e["head"])
        e["local_gate_runs"] = lr[0] if lr else None

    # clearing label
    def cleared_by(e):
        ek = e["end_kind"]
        if ek == "open_at_end":
            return "still_open"
        if ek == "new_head":
            return "new_head(push)"
        if ek == "same_head":
            nk = classify(e.get("next_token", ""), e.get("next_actor"), None)
            return "same_head->" + ("normal_wait" if nk in ("normal_wait", "intentional") else "other_block" if nk == "actionable" else nk)
        # disappeared
        return {"merged": "merged", "closed_unmerged": "closed_unmerged"}.get(e["pr_outcome"], "gone_unresolved")

    for e in episodes:
        e["cleared_by"] = cleared_by(e)

    # classes for reporting: abandoned PRs get their own class
    def rep_class(e):
        if e["pr_outcome"] == "closed_unmerged":
            return "abandoned_pr"
        return e["klass"]

    for e in episodes:
        e["rep_class"] = rep_class(e)

    # ---- output ----------------------------------------------------------
    md = []
    md.append(f"Window: {win_start:%Y-%m-%dT%H:%MZ} to {win_end:%Y-%m-%dT%H:%MZ}; tick files {lstats['files']}, "
              f"unreadable {lstats['unreadable']}, repo-ticks without a PR list {lstats['no_pr_list']}.")
    md.append(f"Episodes: {len(episodes)}; PRs: {len({(e['repo'], e['pr']) for e in episodes})}; raw PR-rows observed: "
              f"{sum(e['n_obs'] for e in episodes)} (dedup ratio {sum(e['n_obs'] for e in episodes)/max(1,len(episodes)):.1f}x).\n")

    kc = collections.Counter(e["rep_class"] for e in episodes)
    md.append("### Episodes by class\n")
    md.append(table(["class", "episodes", "share"], [(k, v, pct(v, len(episodes))) for k, v in kc.most_common()]))

    def agg_rows(group_key):
        g = collections.defaultdict(list)
        for e in episodes:
            if e["rep_class"] in ("normal_wait", "intentional"):
                continue
            g[group_key(e)].append(e)
        rows = []
        for k, es in g.items():
            act = [e for e in es if e["rep_class"] == "actionable"]
            hum = [e for e in es if e["rep_class"] == "human_stop"]
            aba = [e for e in es if e["rep_class"] == "abandoned_pr"]
            cb = collections.Counter(e["cleared_by"] for e in act)
            pushes = sum(1 for e in act if e["end_kind"] == "new_head")
            rd = sum(e["repaired_or_dispute"] for e in act)
            ccov = collections.Counter(e["comment_cov"] for e in act)
            hs = [e for e in act if e["runs_sampled"]]
            wasted = sum(e["wasted_run_min"] for e in hs)
            rr = sum(e["rerun_attempts"] for e in hs)
            down = sum(e["downstream"] for e in act)
            el = sum(e["elapsed_min"] for e in act) / 60.0
            rows.append((k, len(es), len(act), len(hum), len(aba),
                         "; ".join(f"{a_}={b}" for a_, b in cb.most_common()),
                         pushes, rd, f"{rr}" if hs else "unavail", f"{wasted:.0f}" if hs else "unavail",
                         f"{len(hs)}/{len(act)}", down, f"{el:.1f}"))
        rows.sort(key=lambda r: (-r[2], -r[1]))
        return rows

    H = ["key", "episodes", "actionable", "human_stop", "abandoned_pr", "cleared_by (actionable)",
         "pushes_after_block", "repaired/dispute cmts", "reruns (sampled)", "wasted run-min (sampled)",
         "heads w/ run data", "downstream PRs held", "elapsed h (context)"]
    md.append("\n### By blocked_on family (all repos)\n")
    md.append(table(H, agg_rows(lambda e: e["family"])))
    md.append("\n### By repo and family\n")
    md.append(table(H, agg_rows(lambda e: f"{e['repo'].split('/')[1]} / {e['family']}")))
    md.append("\n### By repo\n")
    md.append(table(H, agg_rows(lambda e: e["repo"])))

    # agent-free resolution -----------------------------------------------
    act = [e for e in episodes if e["rep_class"] == "actionable"]
    resolved = [e for e in act if e["end_kind"] != "open_at_end"]
    af = [e for e in resolved if e["end_kind"] == "same_head" and e["repaired_or_dispute"] == 0]
    af_unknown = [e for e in resolved if e["comment_cov"] == "unavailable"]
    md.append("\n### Agent-free resolution (actionable episodes)\n")
    md.append(f"- actionable episodes: {len(act)}; resolved in window: {len(resolved)}; still open at end: {len(act)-len(resolved)}")
    md.append(f"- cleared on the same head with no `repaired:`/`dispute:` comment: {len(af)} ({pct(len(af), len(resolved))} of resolved)")
    md.append(f"- cleared by new head (a push after the block): {sum(1 for e in resolved if e['end_kind']=='new_head')}")
    md.append(f"- cleared by PR leaving the tick list: {sum(1 for e in resolved if e['end_kind']=='disappeared')} "
              f"(merged {sum(1 for e in resolved if e['cleared_by']=='merged')}, closed unmerged {sum(1 for e in resolved if e['cleared_by']=='closed_unmerged')}, "
              f"unresolved {sum(1 for e in resolved if e['cleared_by']=='gone_unresolved')})")
    md.append(f"- unknown share (no comment coverage for the repo): {pct(len(af_unknown), len(resolved))}")


    # ranking: elapsed vs work ----------------------------------------------
    md.append("\n### Ranking of actionable, non-abandoned episodes: elapsed time versus work\n")
    g = collections.defaultdict(lambda: {"ep": 0, "el": 0.0, "push": 0, "cmt": 0, "wm": 0.0, "wh": 0, "down": 0})
    for e in episodes:
        if e["rep_class"] != "actionable":
            continue
        x = g[e["family"]]
        x["ep"] += 1
        x["el"] += e["elapsed_min"] / 60.0
        x["push"] += e["end_kind"] == "new_head"
        x["cmt"] += e["repaired_or_dispute"]
        x["down"] += e["downstream"]
        if e["runs_sampled"]:
            x["wm"] += e["wasted_run_min"]
            x["wh"] += 1
    def rk(key):
        order = sorted(g, key=lambda f: -g[f][key])
        return {f: i + 1 for i, f in enumerate(order)}
    r_el, r_work = rk("el"), None
    for f, x in g.items():
        x["work"] = x["push"] + x["cmt"]
    r_work = rk("work")
    r_ep = rk("ep")
    md.append(table(["family", "episodes", "rank by episodes", "elapsed h", "rank by elapsed", "fix pushes + repaired/dispute cmts", "rank by agent work", "wasted run-min per sampled head", "downstream PRs"],
                    [(f, x["ep"], r_ep[f], f"{x['el']:.0f}", r_el[f], x["work"], r_work[f],
                      (f"{x['wm']/x['wh']:.0f} (n={x['wh']})" if x["wh"] else "unavail"), x["down"])
                     for f, x in sorted(g.items(), key=lambda kv: r_work[kv[0]])]))
    # DIRTY elapsed by repo (tick window)
    byr = collections.defaultdict(lambda: [0, 0.0])
    for e in episodes:
        if e["token"].startswith("merge_state:DIRTY") and e["rep_class"] == "actionable":
            byr[e["repo"]][0] += 1
            byr[e["repo"]][1] += e["elapsed_min"] / 60.0
    md.append("\nDIRTY (actionable, non-abandoned) by repo, tick window:\n")
    md.append(table(["repo", "episodes", "elapsed h"], [(k, v[0], f"{v[1]:.0f}") for k, v in sorted(byr.items(), key=lambda kv: -kv[1][1])]))

    # human stops -----------------------------------------------------------
    hum = [e for e in episodes if e["rep_class"] == "human_stop"]
    md.append("\n### Human-stop episodes (next_action.stop = true)\n")
    hg = collections.defaultdict(lambda: [0, set(), 0.0])
    for e in hum:
        g = hg[(e["repo"], e["family"])]
        g[0] += 1
        g[1].add(e["pr"])
        g[2] += e["elapsed_min"] / 60.0
    md.append(table(["repo / family", "episodes", "PRs", "elapsed h (context)"],
                    [(f"{k[0]} / {k[1]}", v[0], len(v[1]), f"{v[2]:.0f}") for k, v in sorted(hg.items(), key=lambda x: -x[1][0])]))
    prs_total = len({(e["repo"], e["pr"]) for e in episodes if e["pr_outcome"] != "closed_unmerged"})
    owner_prs = len({(e["repo"], e["pr"]) for e in hum if e["pr_outcome"] != "closed_unmerged"})
    md.append(f"\nOwner stops raised per 100 PRs (non-abandoned): {100.0*owner_prs/max(1,prs_total):.1f} "
              f"({owner_prs} of {prs_total}). This counts stops raised, not stops answered; answered is unavailable.")

    # DIRTY claim -----------------------------------------------------------
    md.append("\n### merge_state:DIRTY in the tick window\n")
    d = [e for e in episodes if e["family"] == "merge_state" and e["token"].startswith("merge_state:DIRTY")]
    dprs = {(e["repo"], e["pr"]) for e in d}
    oc = collections.Counter(outcome_for(e, outcome, last_pr_state) for e in {(x["repo"], x["pr"]): x for x in d}.values())
    md.append(f"- DIRTY episodes {len(d)} on {len(dprs)} PRs; PR outcomes: {dict(oc)}")
    ch = collections.Counter(e["cleared_by"] for e in d)
    md.append(f"- how DIRTY episodes cleared: {dict(ch)}")
    dd = [e for e in d if e["end_kind"] == "new_head"]
    md.append(f"- cleared by a push (a real repair): {len(dd)}; cleared with no push {sum(1 for e in d if e['end_kind']=='same_head')}")

    # false holds -----------------------------------------------------------
    md.append("\n### Candidate false holds (HOLD with no defect)\n")
    fh = []
    for (repo, pr), cs in by_pr_comments.items():
        holds = []
        for c in cs:
            m = VERDICT_RE.search(c.get("body") or "")
            if m:
                holds.append((parse_iso(c["created_at"]), m.group(1).upper(), m.group(2), c.get("body") or ""))
        holds.sort(key=lambda x: x[0])
        for i, (t, v, h, body) in enumerate(holds):
            if v != "HOLD":
                continue
            later_ok = any(v2 == "APPROVE" and h2[:7] == h[:7] for (t2, v2, h2, _) in holds[i + 1:])
            nodefect = bool(NODEFECT_RE.search(body))
            if later_ok or nodefect:
                fh.append((repo, pr, h[:7], "approved same head later" if later_ok else "", "reviewer could not read" if nodefect else ""))
    total_holds = sum(1 for cs in by_pr_comments.values() for c in cs if (m := VERDICT_RE.search(c.get('body') or '')) and m.group(1).upper() == "HOLD")
    md.append(f"- HOLD verdict comments seen: {total_holds} (comment coverage: {dict(ccov)})")
    md.append(f"- candidate false holds: {len(fh)} (same-head APPROVE after HOLD, or comment text says the reviewer could not read the workspace)")
    md.append(table(["repo", "PR", "head", "signal 1", "signal 2"], fh[:40]))

    # interventions per 100 PRs ---------------------------------------------
    ints = sum(e["repaired_or_dispute"] for e in episodes)
    allint = sum(1 for cs in by_pr_comments.values() for c in cs if INTERV_RE.search(c.get("body") or ""))
    md.append(f"\n- distinct `repaired:`/`dispute:` comments in cache: {allint} on {len({k for k, cs in by_pr_comments.items() if any(INTERV_RE.search(c.get('body') or '') for c in cs)})} PRs; "
              f"assigned to an episode: {ints}; outside any episode: {dict(unassigned)}")
    own = sum(e["owner_acct_comments"] for e in episodes)
    md.append(f"- comments by the owner login inside episodes: {own} (human and agents using the personal token are not separable: upper bound)")
    hp = {(repo, pr, m.group(2)[:7]) for (repo, pr), cs in by_pr_comments.items() for c in cs if (m := VERDICT_RE.search(c.get('body') or '')) and m.group(1).upper() == 'HOLD'}
    md.append(f"- distinct (PR, head) pairs with a HOLD: {len(hp)}; candidate false holds above are {len(fh)} of them")
    md.append(f"\n- `repaired:`/`dispute:` comments inside actionable episodes: {ints} over {prs_total} PRs "
              f"= {100.0*ints/max(1,prs_total):.1f} per 100 PRs (coverage per repo: {dict(ccov)})")

    out = {"window": [win_start.isoformat(), win_end.isoformat()],
           "episodes": [{k: (v.isoformat() if isinstance(v, dt.datetime) else v) for k, v in e.items()} for e in episodes],
           "api_calls": len(gh.calls)}
    if a.json_out:
        json.dump(out, open(a.json_out, "w"), indent=1, default=str)
    print("\n".join(md))
    sys.stdout.flush()


# --------------------------------------------------------------------------
# summary.log (long horizon, biased: only the 3 oldest blocked PRs per repo per tick)
# --------------------------------------------------------------------------
SUM_RE = re.compile(r"^(\d{8}T\d{6}Z) (\S+) .*?oldest=(.*)$")
ITEM_RE = re.compile(r"#(\d+):(.+):(-?\d+)m$")


def parse_summary(path):
    """{(repo, pr, token): [first_t, last_t, max_age_min, n]} and tick count."""
    res = {}
    n = 0
    for line in open(path):
        m = SUM_RE.match(line.strip())
        if not m:
            continue
        n += 1
        t = parse_stamp(m.group(1))
        for item in re.split(r",(?=#\d+:)", m.group(3)):
            im = ITEM_RE.match(item)
            if not im:
                continue
            k = (m.group(2), int(im.group(1)), im.group(2))
            age = max(0, int(im.group(3)))
            r = res.setdefault(k, [t, t, age, 0])
            r[1] = t
            r[2] = max(r[2], age)
            r[3] += 1
    return res, n


def run_summary(a):
    rows, nlines = parse_summary(a.summary_log)
    gh = Gh(a.cache, a.budget, a.fetch)
    repos = sorted({k[0] for k in rows})
    fam_prs = collections.defaultdict(lambda: [set(), 0.0])
    for (repo, pr, tok), (t0, t1, age, n) in rows.items():
        f = family(tok) if not tok.startswith("merge_state") else tok
        g = fam_prs[f]
        g[0].add((repo, pr))
        g[1] += age / 60.0
    print(f"summary.log tick rows parsed: {nlines}; distinct (repo, PR, token): {len(rows)}\n")
    print("Reproduction of the plan's elapsed-time table (sum of longest wait per PR and token):\n")
    out = []
    for f, (prs, hrs) in sorted(fam_prs.items(), key=lambda x: -x[1][1])[:14]:
        ages = [r[2] for k, r in rows.items() if ((family(k[2]) if not k[2].startswith("merge_state") else k[2]) == f)]
        out.append((f, len(prs), f"{statistics.median(ages):.0f}", sum(1 for x in ages if x > 240), f"{hrs:.0f}"))
    print(table(["token", "PRs", "median longest wait (min)", "over 4h", "hours"], out))
    # DIRTY outcomes
    dirty = sorted({(k[0], k[1]) for k in rows if k[2] == "merge_state:DIRTY"})
    closed = load_closed(gh, {r: None for r in repos})
    if a.fetch:
        for repo, pr in dirty:
            if (repo, pr) not in closed:
                if gh.get(repo, f"repos/{repo}/pulls/{pr}") == "BUDGET":
                    break
    singles = load_singles(gh, dirty)
    oc = {}
    for k in dirty:
        o = closed.get(k) or singles.get(k)
        oc[k] = o
    cnt = collections.Counter((o["state"] if o else "unresolved") for o in oc.values())
    print(f"\nDIRTY PRs: {len(dirty)}; final state: {dict(cnt)}")
    # elapsed by outcome
    byo = collections.defaultdict(lambda: [0, 0.0, 0])
    for (repo, pr, tok), (t0, t1, age, n) in rows.items():
        if tok != "merge_state:DIRTY":
            continue
        o = oc.get((repo, pr))
        key = o["state"] if o else "unresolved"
        if o and o["state"] == "closed_unmerged":
            labs = ",".join(sorted(o["labels"]))
            key = "closed_unmerged" + (" (labelled superseded)" if "superseded" in labs else "")
        byo[key][0] += 1
        byo[key][1] += age / 60.0
        byo[key][2] += age > 240
    print()
    print(table(["final state of PR", "DIRTY PRs", "hours (longest wait)", "over 4h"],
                [(k, v[0], f"{v[1]:.0f}", v[2]) for k, v in sorted(byo.items(), key=lambda x: -x[1][1])]))
    tot = sum(v[1] for v in byo.values())
    print(f"\ntotal DIRTY hours {tot:.0f}; on PRs that were closed unmerged: "
          f"{sum(v[1] for k, v in byo.items() if k.startswith('closed_unmerged')):.0f}")
    # time dirty-to-merge, for merged PRs, vs the DIRTY age
    ex = []
    for (repo, pr, tok), (t0, t1, age, n) in rows.items():
        o = oc.get((repo, pr))
        if tok == "merge_state:DIRTY" and o and o["state"] == "merged":
            ex.append((age / 60.0, repo.split("/")[1], pr))
    ex.sort(reverse=True)
    print("\nMerged PRs with the longest DIRTY waits (hours, repo, PR):", [(round(h), r, p) for h, r, p in ex[:8]])
    json.dump({f"{k[0]}#{k[1]}:{k[2]}": [r[0].isoformat(), r[1].isoformat(), r[2], r[3]] for k, r in rows.items()},
              open(os.path.join(a.cache, "summary_rows.json"), "w"))


def outcome_for(e, outcome, last_pr_state):
    k = (e["repo"], e["pr"])
    if e["pr"] in last_pr_state[e["repo"]]:
        return "open"
    return outcome.get(k) or "unresolved"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("fetch", "analyze", "summary"):
        p = sub.add_parser(name)
        p.add_argument("--ticks", required=(name != "summary"))
        p.add_argument("--cache", required=True)
        p.add_argument("--state")
        p.add_argument("--metrics-db")
        p.add_argument("--summary-log")
        p.add_argument("--json-out")
        p.add_argument("--budget", type=int, default=400)
        p.add_argument("--sample-heads", type=int, default=120)
        p.add_argument("--horizon", type=lambda s: parse_iso(s), default=parse_iso("2026-10-30T00:00:00Z"))
        p.add_argument("--skip-comments-for", nargs="*")
        p.add_argument("--fetch", action="store_true")
    a = ap.parse_args()
    if a.cmd == "fetch":
        run_fetch(a)
    elif a.cmd == "summary":
        run_summary(a)
    else:
        run_analyze(a)


if __name__ == "__main__":
    main()
