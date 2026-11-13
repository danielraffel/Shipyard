#!/usr/bin/env python3
"""C1 stage 5: estimate the repair step of each conflict episode.
Walk the PR's first-parent commits after head_at_T (oldest first); the first one that
merges cleanly with the base as of its own commit time is the 'clearing commit'.
Records steps (first-parent commits to reach it), whether it is a merge commit, and the delay.
Read-only. Usage: python3 -I c1_conflicts_stage5.py DATA
"""
import json, subprocess, sys, datetime as dt
DATA = sys.argv[1]
M = "/Volumes/Atelier/Code/agent-worktrees/c1-mirrors/"
SRC = {"Generous-Corp/pulp": "/Volumes/Atelier/Code/pulp", "Generous-Corp/vellum": M+"vellum.git",
       "danielraffel/Shipyard": "/Volumes/Atelier/Code/Shipyard", "danielraffel/tartci": "/Volumes/Atelier/Code/tartci",
       "danielraffel/fleet-handbook": "/Volumes/Atelier/Code/fleet-handbook",
       "danielraffel/spectr": M+"spectr.git", "danielraffel/fleet-ssh": M+"fleet-ssh.git",
       "danielraffel/cmux-capacity-watchdog": M+"cmux-capacity-watchdog.git"}
def git(d, *a): return subprocess.run(["git", "-C", d, *a], capture_output=True, text=True)
rows = json.load(open(f"{DATA}/classified.json"))
CAP = 80
for r in rows:
    d = SRC[r["repo"]]; pr = r["pr"]
    ref = ("main" if d.endswith(".git") else "origin/main") if r["base"] == "main" else f"refs/c1/base/{r['base']}"
    out = git(d, "rev-list", "--first-parent", "--reverse", "--format=%H %cI %P", f"{r['head_at_T']}..refs/c1/pull/{pr}").stdout.split("\n")
    cs = [l.split() for l in out if l and not l.startswith("commit ")]
    r["fix"] = None
    for i, c in enumerate(cs[:CAP], 1):
        sha, when, parents = c[0], c[1], c[2:]
        b = git(d, "rev-list", "-1", "--first-parent", f"--before={when}", ref).stdout.strip()
        if not b: continue
        mt = git(d, "merge-tree", "--write-tree", "--name-only", "--no-messages", b, sha)
        if mt.returncode == 0:
            delay = (dt.datetime.fromisoformat(when) - dt.datetime.fromisoformat(r["first"])).total_seconds() / 60
            r["fix"] = dict(steps=i, is_merge=len(parents) > 1, delay_min=round(delay), total_after=len(cs)); break
json.dump(rows, open(f"{DATA}/classified.json", "w"))
print(sum(1 for r in rows if r["fix"]), "of", len(rows), "cleared within cap")
