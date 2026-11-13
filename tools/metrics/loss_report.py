#!/usr/bin/env python3
"""Daily loss report: what blocked pull requests cost, from the steward's own ticks.

Run as `python3 -I tools/metrics/loss_report.py ...`. Standard library only.
Read-only over the evidence; writes only the report files it is asked to.

Input: the per-tick JSON the steward tick keeps live for a few days and then
archives by UTC day (`archive/ticks-YYYYMMDD.tar.gz`). Every open PR has a row
in each tick: head, `blocked_on`, `since`, `next_action {actor, stop}`.

An EPISODE is one contiguous interval where one (repo, PR, head, blocked_on)
held; repeated tick observations collapse to one. Definitions live in
docs/rollups.md ("Daily loss report"). Coverage is `available`, `partial` or
`unavailable`; unavailable is never reported as zero (the value is null).

  loss_report.py --log-dir DIR [--since 7d] [--json-out F] [--md-out F]
                 [--github CACHE_DIR] [--now ISO]
  loss_report.py --daily --log-dir DIR     # once per UTC day, never fails
"""
from __future__ import annotations

import argparse
import collections
import datetime as dt
import glob
import json
import os
import re
import sys
import tarfile
import time

SCHEMA = "shipyard-loss-report/v1"
UTC = dt.timezone.utc
STAMP_RE = re.compile(r"-(\d{8}T\d{6}Z)\.json$")
INTERV_RE = re.compile(r"^\s*(repaired|dispute)\s*:", re.I | re.M)
INTENTIONAL = {"draft"}
SYSTEM_WAIT = {
    "queued", "admitted", "ready", "deferred_pass_budget", "revalidating",
    "transient_rerun", "carried_commits_over_cap", "carried_commits_unreadable",
    "capacity_queued", "required_pending",
}
OWNER_STOP = {"excluded_label", "direct_merge_refused", "owner_decision"}


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


def parse_duration(s):
    m = re.fullmatch(r"(\d+)([dhm])", s)
    if not m:
        raise ValueError(f"bad duration {s!r} (use 7d, 12h, 90m)")
    return dt.timedelta(**{{"d": "days", "h": "hours", "m": "minutes"}[m.group(2)]: int(m.group(1))})


def family(token):
    return token.split(":", 1)[0]


def classify(token, actor, stop):
    fam = family(token)
    if fam in INTENTIONAL:
        return "intentional"
    if stop or actor == "owner" or fam in OWNER_STOP:
        return "human_stop"
    if fam in SYSTEM_WAIT or actor in ("steward", "nobody", "ci"):
        return "normal_wait"
    return "actionable"


# ---------------------------------------------------------------- reading
def tick_sources(log_dir, start):
    """Yield (basename, stamp_time, loader) for live files and archive members in the window.

    A file present both live and in an archive (a tick mid-archive) is read once.
    """
    seen = set()
    for f in sorted(glob.glob(os.path.join(log_dir, "*.json"))):
        base = os.path.basename(f)
        m = STAMP_RE.search(base)
        if not m or base.startswith(("fleet-invariants-", "loss-report-")):
            continue
        t = parse_stamp(m.group(1))
        if t < start:
            continue
        seen.add(base)
        yield base, t, ("live", f)
    first_day = start.strftime("%Y%m%d")
    for a in sorted(glob.glob(os.path.join(log_dir, "archive", "ticks-*.tar.gz"))):
        day = re.search(r"ticks-(\d{8})\.tar\.gz$", a)
        if not day or day.group(1) < first_day:
            continue
        try:
            with tarfile.open(a, "r:gz") as tf:
                members = [m for m in tf.getmembers() if m.isfile()]
                for mem in members:
                    base = os.path.basename(mem.name)
                    m = STAMP_RE.search(base)
                    if not m or base in seen or base.startswith(("fleet-invariants-", "loss-report-")):
                        continue
                    t = parse_stamp(m.group(1))
                    if t < start:
                        continue
                    seen.add(base)
                    try:
                        data = tf.extractfile(mem).read()
                    except Exception:
                        data = None
                    yield base, t, ("archive", data)
        except Exception:
            yield os.path.basename(a), None, ("bad_archive", None)


def load_ticks(log_dir, start, end):
    per_repo = collections.defaultdict(list)
    st = {"live_files": 0, "archive_files": 0, "unreadable": 0, "bad_archives": 0, "no_pr_list": 0,
          "ticks_per_day": collections.Counter()}
    for base, t, (kind, payload) in tick_sources(log_dir, start):
        if kind == "bad_archive":
            st["bad_archives"] += 1
            continue
        if t > end:
            continue
        try:
            if kind == "live":
                with open(payload, "rb") as fh:
                    d = json.loads(fh.read())
            else:
                d = json.loads(payload)
            repos = d["repos"]
        except Exception:
            st["unreadable"] += 1
            continue
        st["live_files" if kind == "live" else "archive_files"] += 1
        st["ticks_per_day"][t.strftime("%Y-%m-%d")] += 1
        for r in repos:
            if "prs" not in r or r.get("errors"):
                st["no_pr_list"] += 1
                per_repo[r["repo"]].append((t, None))  # invalid tick: a gap, never a clear
                continue
            rows = {}
            for p in r["prs"]:
                na = p.get("next_action") or {}
                rows[p["number"]] = {
                    "head": p.get("head_sha"), "token": p.get("blocked_on") or "unknown",
                    "actor": na.get("actor"), "stop": bool(na.get("stop")),
                    "since": parse_iso(p.get("since")),
                    "sup_detected": bool(p.get("superseded")),
                    "red_everywhere": _red_everywhere(p),
                }
            per_repo[r["repo"]].append((t, rows))
    for v in per_repo.values():
        v.sort(key=lambda x: x[0])
    return per_repo, st


def _red_everywhere(p):
    """True only when the row names red_everywhere as a token or detail, or its
    decision carries a true `red_everywhere` boolean. A decision of
    {"red_everywhere": false} is a negative, never a hit."""
    if p.get("blocked_on") == "red_everywhere":
        return True
    if "red_everywhere" in str(p.get("blocked_detail") or ""):
        return True
    d = p.get("decision")
    if isinstance(d, dict):
        return d.get("red_everywhere") is True
    return isinstance(d, str) and "red_everywhere" in d


# --------------------------------------------------------------- episodes
def build_episodes(per_repo, gap):
    eps, gaps = [], 0
    for repo, ticks in per_repo.items():
        open_ep, prev_t = {}, None
        for t, rows in ticks:
            if rows is None:
                continue  # unobserved interval: open episodes stay open
            if prev_t is not None and t - prev_t > gap:
                gaps += 1
                for ep in open_ep.values():
                    ep["end"], ep["end_kind"] = prev_t, "gap"
                    eps.append(ep)
                open_ep = {}
            prev_t = t
            for pr in list(open_ep):
                if pr not in rows:
                    ep = open_ep.pop(pr)
                    ep["end"], ep["end_kind"] = t, "left_list"
                    eps.append(ep)
            for pr, o in rows.items():
                ep = open_ep.get(pr)
                if ep and ep["head"] == o["head"] and ep["token"] == o["token"]:
                    ep["last_obs"] = t
                    ep["n_obs"] += 1
                    # Every observation counts: flags are OR-ed, the earliest `since` wins, and
                    # an episode that was actionable on any observation stays actionable.
                    ep["sup_detected"] = ep["sup_detected"] or o["sup_detected"]
                    ep["red_everywhere"] = ep["red_everywhere"] or o["red_everywhere"]
                    if o["since"] and (ep["since"] is None or o["since"] < ep["since"]):
                        ep["since"] = o["since"]
                    if classify(o["token"], o["actor"], o["stop"]) == "actionable":
                        ep["klass"] = "actionable"
                    continue
                if ep:
                    ep["end"] = t
                    ep["end_kind"] = "new_head" if ep["head"] != o["head"] else "same_head"
                    ep["next_token"] = o["token"]
                    ep["next_actor"] = o["actor"]
                    eps.append(ep)
                open_ep[pr] = {
                    "repo": repo, "pr": pr, "head": o["head"], "token": o["token"],
                    "family": family(o["token"]), "actor": o["actor"], "since": o["since"],
                    "first_obs": t, "last_obs": t, "n_obs": 1, "end": None, "end_kind": None,
                    "klass": classify(o["token"], o["actor"], o["stop"]),
                    "sup_detected": o["sup_detected"], "red_everywhere": o["red_everywhere"],
                    "comments": 0,
                }
        for ep in open_ep.values():
            ep["end_kind"] = "open_at_end"
            eps.append(ep)
    return eps, gaps


def elapsed_min(e):
    start = e["since"] or e["first_obs"]
    return max(0.0, ((e["end"] or e["last_obs"]) - start).total_seconds() / 60.0)


# ----------------------------------------------------------- github cache
def load_github_cache(path):
    """Read cached REST answers written by the baseline fetcher. Never calls GitHub."""
    closed, comments = {}, {}
    files = glob.glob(os.path.join(path, "**", "*.json"), recursive=True)
    for f in files:
        try:
            with open(f) as fh:
                x = json.load(fh)
        except Exception:
            continue
        items = x if isinstance(x, list) else [x] if isinstance(x, dict) else []
        for it in items:
            if not isinstance(it, dict):
                continue
            # an issue comment: not a pull-request or issue object, which also carry issue_url
            if ("issue_url" in it and "id" in it and "body" in it
                    and not any(k in it for k in ("number", "base", "head", "merged_at"))):
                m = re.search(r"/repos/([^/]+/[^/]+)/issues/(\d+)$", it["issue_url"])
                if m:
                    comments[it["id"]] = (m.group(1), int(m.group(2)), parse_iso(it.get("created_at")), it.get("body") or "")
            elif "merged_at" in it and "number" in it and isinstance(it.get("base"), dict):
                full = (it["base"].get("repo") or {}).get("full_name")
                if full and it.get("state") == "closed":
                    closed[(full, it["number"])] = "merged" if it.get("merged_at") else "closed_unmerged"
    return closed, list(comments.values())


# ------------------------------------------------------------------ report
def pct(n, d):
    return None if not d else round(100.0 * n / d, 1)


def cov(status, reason):
    return {"status": status, "reason": reason}


def build_report(log_dir, since, now, github=None, gap_minutes=120):
    start = now - since
    per_repo, st = load_ticks(log_dir, start, now)
    eps, gaps = build_episodes(per_repo, dt.timedelta(minutes=gap_minutes))
    closed, comments = load_github_cache(github) if github else ({}, [])
    # PRs listed in a repo's last tick are open; GitHub cache decides the rest
    last_open = {r: set(([x for x in t if x[1] is not None] or [(None, {})])[-1][1]) for r, t in per_repo.items()}
    for e in eps:
        if e["pr"] in last_open.get(e["repo"], ()):
            e["outcome"] = "open"
        else:
            e["outcome"] = closed.get((e["repo"], e["pr"])) if github else None
        if e["outcome"] == "closed_unmerged":
            e["klass"] = "closed_unmerged"
    by_pr = collections.defaultdict(list)
    for e in eps:
        by_pr[(e["repo"], e["pr"])].append(e)
    interv_total = 0
    for repo, pr, t, body in comments:
        if not INTERV_RE.search(body) or t is None:
            continue
        interv_total += 1
        cand = [e for e in by_pr.get((repo, pr), [])
                if (e["since"] or e["first_obs"]) <= t <= (e["end"] or e["last_obs"]) + dt.timedelta(minutes=10)]
        if cand:
            cand.sort(key=lambda e: (e["klass"] != "actionable", e["first_obs"]))
            cand[0]["comments"] += 1

    n_ticks_days = st["ticks_per_day"]
    days, d = [], start.date()
    while d <= now.date():
        days.append(d.isoformat())
        d += dt.timedelta(days=1)

    def day_of(e):
        return e["first_obs"].strftime("%Y-%m-%d")

    def series(pred):
        out = {}
        for day in days:
            out[day] = None if n_ticks_days.get(day, 0) == 0 else 0
        for e in eps:
            if pred(e) and out.get(day_of(e)) is not None:
                out[day_of(e)] += 1
        return out

    def checks(e):
        return e["token"].split(":", 1)[1].split(",") if e["family"] == "required_failed" and ":" in e["token"] else []

    rv = lambda e: "review-verdict" in checks(e) and e["klass"] == "actionable"
    s = {
        "review_verdict_episodes": series(rv),
        "review_verdict_same_head_no_push": series(lambda e: rv(e) and e["end_kind"] == "same_head"),
        "required_failed_episodes": series(lambda e: e["family"] == "required_failed" and e["klass"] == "actionable"),
        "required_failed_fix_pushes": series(lambda e: e["family"] == "required_failed" and e["klass"] == "actionable" and e["end_kind"] == "new_head"),
        "capacity_queued_episodes": series(lambda e: e["family"] == "capacity_queued"),
        "superseded_label_episodes": series(lambda e: e["token"] == "excluded_label:superseded"),
        "superseded_detected_episodes": series(lambda e: e["sup_detected"] and e["token"] != "excluded_label:superseded"),
    }
    red_seen = any(e["red_everywhere"] for e in eps)
    s["red_everywhere_episodes"] = series(lambda e: e["red_everywhere"]) if red_seen else None

    kc = collections.Counter(e["klass"] for e in eps)
    act = [e for e in eps if e["klass"] == "actionable"]
    resolved = [e for e in act if e["end_kind"] in ("same_head", "new_head", "left_list")]
    upper = [e for e in resolved if e["end_kind"] == "same_head" and e["comments"] == 0]
    strict = [e for e in upper if classify(e.get("next_token", ""), e.get("next_actor"), False) in ("normal_wait", "intentional")]
    prs = {(e["repo"], e["pr"]) for e in eps if e["klass"] != "closed_unmerged"}
    owner_prs = {(e["repo"], e["pr"]) for e in eps if e["klass"] == "human_stop"}

    rows = collections.defaultdict(lambda: collections.Counter())
    for e in eps:
        key = e["token"] if e["token"].startswith("merge_state") else e["family"]
        c = rows[(e["repo"], key)]
        c["episodes"] += 1
        c[e["klass"]] += 1
        if e["klass"] == "actionable":
            c["fix_pushes"] += e["end_kind"] == "new_head"
            c["same_head_clear"] += e["end_kind"] == "same_head"
            c["elapsed_min_context"] += round(elapsed_min(e))
    table = [{"repo": r, "token": t, **dict(c)} for (r, t), c in sorted(rows.items(), key=lambda kv: (-kv[1]["actionable"], kv[0]))]

    ticks_total = st["live_files"] + st["archive_files"]
    observed_days = sum(1 for dd in days if n_ticks_days.get(dd, 0))
    window_cov = cov("available" if observed_days == len(days) and not gaps and not st["bad_archives"] and not st["no_pr_list"] and not st["unreadable"] else
                     "partial" if ticks_total else "unavailable",
                     f"{observed_days} of {len(days)} UTC days have tick files; {gaps} gap(s) over {gap_minutes} min; "
                     f"{st['unreadable']} unreadable, {st['bad_archives']} unreadable archive(s), "
                     f"{st['no_pr_list']} repo tick(s) with errors or no PR list treated as gaps")
    gh_cov = (cov("partial", f"cached GitHub reads: {len(closed)} closed PRs, {len(comments)} comments; the cache may not span the window")
              if github else cov("unavailable", "no --github cache: closed-unmerged and repaired/dispute comments are not read"))
    gh_null = lambda v: v if github else None

    report = {
        "schema": SCHEMA,
        "generated_at": now.isoformat(timespec="seconds"),
        "window": {"since": start.isoformat(timespec="seconds"), "until": now.isoformat(timespec="seconds"), "days": days},
        "sources": {"live_files": st["live_files"], "archive_files": st["archive_files"], "unreadable": st["unreadable"],
                    "bad_archives": st["bad_archives"], "repo_ticks_without_pr_list": st["no_pr_list"],
                    "ticks_per_day": dict(sorted(n_ticks_days.items())), "gaps": gaps},
        "coverage": {"window": window_cov, "github": gh_cov,
                     "agent_work": cov("partial", "pushes after a block come from head changes; comments share one bot identity, so a push or comment cannot be attributed to a role"),
                     "ci_compute": cov("unavailable", "wasted and rework minutes per episode are not in tick JSON"),
                     "owner_stops_answered": cov("unavailable", "only stops raised are recorded")},
        "totals": {"rows_observed": sum(e["n_obs"] for e in eps), "episodes": len(eps), "prs": len(prs),
                   "by_class": dict(kc),
                   "closed_unmerged_episodes": gh_null(kc.get("closed_unmerged", 0))},
        "agent_free_resolution": {
            "denominator_resolved_actionable": len(resolved),
            "still_open_or_gap": len(act) - len(resolved),
            "same_head_no_comment": len(upper), "rate_upper_pct": pct(len(upper), len(resolved)),
            "same_head_into_normal_wait": len(strict), "rate_strict_pct": pct(len(strict), len(resolved)),
            "cleared_by_push": sum(1 for e in resolved if e["end_kind"] == "new_head"),
            "left_list": sum(1 for e in resolved if e["end_kind"] == "left_list"),
            "repaired_dispute_comments_in_cache": gh_null(interv_total),
            "coverage": cov("partial", "upper bound: a same-head clear can still involve an unrecorded rerun or label; "
                                       + ("comments checked from cache" if github else "comments not checked")),
        },
        "owner_stops": {"prs_with_owner_stop": len(owner_prs), "prs": len(prs),
                        "per_100_prs": pct(len(owner_prs), len(prs)) if prs else None,
                        "coverage": cov("partial", "stops raised, not answered")},
        "series_per_day": {k: v for k, v in s.items()},
        "series_coverage": {"red_everywhere_episodes": cov("available" if red_seen else "unavailable",
                                                          "token seen" if red_seen else "never seen in the window: not emitted by this binary, or none occurred; cannot tell"),
                            "default": cov("available", "null on a UTC day with no tick files; a day is the UTC day an episode was first seen")},
        "by_repo_token": table,
    }
    return report


def markdown(r):
    t, a, o = r["totals"], r["agent_free_resolution"], r["owner_stops"]
    out = [f"# Loss report {r['window']['since'][:10]} to {r['window']['until'][:10]}", "",
           f"Episodes {t['episodes']} over {t['prs']} PRs ({t['rows_observed']} tick rows). Classes: {t['by_class']}.",
           f"Window coverage: {r['coverage']['window']['status']} ({r['coverage']['window']['reason']}).", "",
           f"Agent-free resolution: upper {a['rate_upper_pct']}% , strict {a['rate_strict_pct']}% of {a['denominator_resolved_actionable']} resolved actionable episodes ({a['coverage']['status']}: {a['coverage']['reason']}).",
           f"Owner stops per 100 PRs: {o['per_100_prs']} ({o['prs_with_owner_stop']} of {o['prs']}; raised, not answered).", "",
           "## Per day", "", "| day | " + " | ".join(r["series_per_day"]) + " |", "|---|" + "---|" * len(r["series_per_day"])]
    for day in r["window"]["days"]:
        cells = []
        for k, v in r["series_per_day"].items():
            cells.append("unavail" if v is None or v.get(day) is None else str(v[day]))
        out.append(f"| {day} | " + " | ".join(cells) + " |")
    out += ["", "## Top actionable by repo and token", "", "| repo | token | episodes | actionable | normal wait | closed unmerged | fix pushes |", "|---|---|---|---|---|---|---|"]
    for row in r["by_repo_token"][:15]:
        cu = row.get("closed_unmerged", 0) if r["coverage"]["github"]["status"] != "unavailable" else "unavail"
        out.append(f"| {row['repo']} | {row['token']} | {row['episodes']} | {row.get('actionable', 0)} | {row.get('normal_wait', 0)} | {cu} | {row.get('fix_pushes', 0)} |")
    out += ["", "Unavailable: CI compute per episode, owner stops answered, dispute outcomes. Elapsed time is context only."]
    return "\n".join(out) + "\n"


def summary_line(r, stamp):
    a, o, s = r["agent_free_resolution"], r["owner_stops"], r["series_per_day"]
    rvs = sum(v for v in s["review_verdict_same_head_no_push"].values() if v)
    return (f"{stamp} loss-report window={len(r['window']['days'])}d episodes={r['totals']['episodes']} "
            f"actionable={r['totals']['by_class'].get('actionable', 0)} agent_free_upper={a['rate_upper_pct']}% "
            f"agent_free_strict={a['rate_strict_pct']}% denom={a['denominator_resolved_actionable']} "
            f"owner_stops_per_100={o['per_100_prs']} rv_same_head_no_push={rvs} "
            f"coverage={r['coverage']['window']['status']}")


# ------------------------------------------------------------------- daily
def daily(log_dir, now=None, since=dt.timedelta(days=7), github=None):
    """Once per UTC day. Never raises: any failure is one `loss-report ERROR` line."""
    now = now or dt.datetime.now(UTC)
    stamp = now.strftime("%Y%m%dT%H%M%SZ")
    day = now.strftime("%Y%m%d")
    summary = os.path.join(log_dir, "summary.log")

    def say(line):
        try:
            with open(summary, "a") as fh:
                fh.write(line + "\n")
        except Exception:
            pass

    lock = os.path.join(log_dir, ".loss-report.lock")
    got = False
    try:
        marker = os.path.join(log_dir, ".loss-report-last-day")
        try:
            with open(marker) as fh:
                if fh.read().strip() == day:
                    return 0
        except OSError:
            pass
        try:
            os.mkdir(lock)
        except FileExistsError:
            try:
                if time.time() - os.path.getmtime(lock) > 3600:
                    os.rmdir(lock)
                    os.mkdir(lock)
                else:
                    return 0
            except OSError:
                return 0
        got = True
        # Marked at start: a report that fails is logged once and retried tomorrow, not every tick.
        with open(marker, "w") as fh:
            fh.write(day + "\n")
        rep = build_report(log_dir, since, now, github)
        jp, mp = (os.path.join(log_dir, f"loss-report-{day}.{x}") for x in ("json", "md"))
        for path, text in ((jp, json.dumps(rep, indent=1)), (mp, markdown(rep))):
            tmp = path + ".tmp"
            with open(tmp, "w") as fh:
                fh.write(text)
            os.replace(tmp, path)
        say(summary_line(rep, stamp))
    except BaseException as exc:  # a report must never fail the tick
        say(f"{stamp} loss-report ERROR {type(exc).__name__}: {str(exc)[:120]}")
    finally:
        if got:
            try:
                os.rmdir(lock)
            except OSError:
                pass
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--log-dir", default=os.path.expanduser("~/Library/Logs/shipyard-steward-tick"))
    ap.add_argument("--since", default="7d")
    ap.add_argument("--now", help="ISO time, for tests and replays")
    ap.add_argument("--github", help="directory of cached GitHub REST answers (never fetches)")
    ap.add_argument("--json-out")
    ap.add_argument("--md-out")
    ap.add_argument("--daily", action="store_true", help="once per UTC day; writes loss-report-YYYYMMDD.{json,md} and a summary.log line; always exits 0")
    a = ap.parse_args(argv)
    now = parse_iso(a.now) if a.now else dt.datetime.now(UTC)
    if a.daily:
        try:
            since = parse_duration(a.since)
        except ValueError:
            since = dt.timedelta(days=7)
        return daily(a.log_dir, now, since, a.github)
    rep = build_report(a.log_dir, parse_duration(a.since), now, a.github)
    if a.json_out:
        with open(a.json_out, "w") as fh:
            json.dump(rep, fh, indent=1)
    if a.md_out:
        with open(a.md_out, "w") as fh:
            fh.write(markdown(rep))
    if not (a.json_out or a.md_out):
        sys.stdout.write(markdown(rep))
    return 0


if __name__ == "__main__":
    sys.exit(main())
