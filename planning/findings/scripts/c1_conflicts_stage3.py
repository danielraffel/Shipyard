#!/usr/bin/env python3
"""C1 stage 3: extract conflict hunks (both sides) for every conflicting path.
Writes DATA/hunks.json: {"repo#pr@first": {path: [[ours_lines, theirs_lines], ...]}}.
Usage: python3 -I c1_conflicts_stage3.py DATA
"""
import json, subprocess, sys, re
DATA = sys.argv[1]
sys.path.insert(0, __file__.rsplit("/", 1)[0])
SRC = {}
M = "/Volumes/Atelier/Code/agent-worktrees/c1-mirrors/"
SRC = {"Generous-Corp/pulp": "/Volumes/Atelier/Code/pulp", "Generous-Corp/vellum": M+"vellum.git",
       "danielraffel/Shipyard": "/Volumes/Atelier/Code/Shipyard", "danielraffel/tartci": "/Volumes/Atelier/Code/tartci",
       "danielraffel/fleet-handbook": "/Volumes/Atelier/Code/fleet-handbook",
       "danielraffel/spectr": M+"spectr.git", "danielraffel/fleet-ssh": M+"fleet-ssh.git",
       "danielraffel/cmux-capacity-watchdog": M+"cmux-capacity-watchdog.git"}
res = json.load(open(f"{DATA}/results.json"))
out = {}
for x in res:
    if x["status"] != "conflict": continue
    d = SRC[x["repo"]]
    o = subprocess.run(["git", "-C", d, "merge-tree", "--write-tree", "--no-messages", x["base_at_T"], x["head_at_T"]],
                       capture_output=True, text=True).stdout.split("\n")[0].strip()
    hs = {}
    for p in x["paths"]:
        b = subprocess.run(["git", "-C", d, "show", f"{o}:{p}"], capture_output=True)
        if b.returncode: hs[p] = "unreadable"; continue
        t = b.stdout.decode("utf-8", "replace")
        hunks = []
        for m in re.finditer(r"<<<<<<< [^\n]*\n(.*?)=======\n(.*?)>>>>>>> [^\n]*\n", t, re.S):
            hunks.append([m.group(1).split("\n")[:-1][:40], m.group(2).split("\n")[:-1][:40]])
        hs[p] = hunks if hunks else "no-text-markers"
    out[f"{x['repo']}#{x['pr']}@{x['first']}"] = hs
json.dump(out, open(f"{DATA}/hunks.json", "w"))
print(len(out))
