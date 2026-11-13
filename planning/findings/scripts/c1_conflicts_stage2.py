#!/usr/bin/env python3
"""C1 stage 2: reconstruct each DIRTY episode with git merge-tree.
Reads DATA/episodes.json + DATA/pr_meta.json, writes DATA/results.json.
Read-only against PR branches: only local refs/c1/* are used.
Usage: python3 -I c1_conflicts_stage2.py DATA
"""
import json, subprocess, sys, os
DATA = sys.argv[1]
M = "/Volumes/Atelier/Code/agent-worktrees/c1-mirrors/"
SRC = {"Generous-Corp/pulp": "/Volumes/Atelier/Code/pulp", "Generous-Corp/vellum": M+"vellum.git",
       "danielraffel/Shipyard": "/Volumes/Atelier/Code/Shipyard", "danielraffel/tartci": "/Volumes/Atelier/Code/tartci",
       "danielraffel/fleet-handbook": "/Volumes/Atelier/Code/fleet-handbook",
       "danielraffel/spectr": M+"spectr.git", "danielraffel/fleet-ssh": M+"fleet-ssh.git",
       "danielraffel/cmux-capacity-watchdog": M+"cmux-capacity-watchdog.git"}
def git(d, *a):
    return subprocess.run(["git", "-C", d, *a], capture_output=True, text=True)
eps = json.load(open(f"{DATA}/episodes.json"))["episodes"]
meta = json.load(open(f"{DATA}/pr_meta.json"))
fetched = set()
out = []
for e in eps:
    repo, pr = e["repo"], e["pr"]; d = SRC[repo]; m = meta[f"{repo}#{pr}"]
    r = dict(e); r["pr_state"] = "merged" if m["merged"] else m["state"]; r["base"] = m["base"]
    r["labels"] = m["labels"]; r["merged_at"] = m["merged_at"]; r["closed_at"] = m["closed_at"]
    base = m["base"]; ref = ("main" if d.endswith(".git") else "origin/main") if base == "main" else f"refs/c1/base/{base}"
    if base != "main" and (repo, base) not in fetched:
        git(d, "fetch", "-q", "origin", f"+refs/heads/{base}:refs/c1/base/{base}"); fetched.add((repo, base))
    import datetime as dt
    def at(ts, off=0):
        return (dt.datetime.fromisoformat(ts) + dt.timedelta(minutes=off)).isoformat()
    def rl(refname, ts, fp=True):
        x = git(d, "rev-list", "-1", *(["--first-parent"] if fp else []), f"--before={ts}", refname).stdout.strip()
        return x or git(d, "rev-list", "-1", f"--before={ts}", refname).stdout.strip()
    pref = f"refs/c1/pull/{pr}"
    variants = [("first/first", e["first"], e["first"]), ("lastbase/firsthead", e["last"], e["first"]),
                ("last/last", e["last"], e["last"]), ("last+10m/last+10m", at(e["last"], 10), at(e["last"], 10))]
    r["status"] = "fail"; r["why"] = "no commits"
    head = bs = ""
    for name, tb, th in variants:
        head = rl(pref, th); bs = rl(ref, tb)
        if not head or not bs:
            r["why"] = "no head commit before time (history rewritten?)" if not head else "no base commit before time"
            continue
        mt = git(d, "merge-tree", "--write-tree", "--name-only", "--no-messages", bs, head)
        if mt.returncode == 1:
            lines = mt.stdout.split("\n")[1:]
            r["paths"] = sorted({x for x in lines if x.strip()}); r["status"] = "conflict"; r["variant"] = name
            r["head_at_T"], r["base_at_T"] = head, bs; r.pop("why", None); break
        elif mt.returncode == 0:
            r["status"] = "clean"; r["paths"] = []; r.pop("why", None); r["head_at_T"], r["base_at_T"] = head, bs
        else:
            r["why"] = mt.stderr.strip()[:200]
    if r["status"] == "fail": out.append(r); continue
    # how it ended: commits on PR after head_at_T
    if head:
        tip = git(d, "rev-parse", f"refs/c1/pull/{pr}").stdout.strip()
        r["tip"] = tip
        r["head_changed_after"] = tip != head
        r["commits_after"] = int(git(d, "rev-list", "--count", f"{head}..{tip}").stdout.strip() or 0)
        r["merge_commits_after"] = int(git(d, "rev-list", "--count", "--merges", f"{head}..{tip}").stdout.strip() or 0)
        # tip merges base now? (is origin/main-at-T reachable from tip)
        r["tip_contains_base_at_T"] = git(d, "merge-base", "--is-ancestor", bs, tip).returncode == 0
        r["files_changed_by_pr"] = len(git(d, "diff", "--name-only", f"{bs}...{head}").stdout.split())
    out.append(r)
json.dump(out, open(f"{DATA}/results.json", "w"), indent=1)
import collections
print(collections.Counter(x["status"] for x in out))
