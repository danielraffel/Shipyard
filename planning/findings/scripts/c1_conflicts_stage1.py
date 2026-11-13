#!/usr/bin/env python3
"""C1 stage 1: build DIRTY episodes from the steward summary log and cache one
REST read per PR (state, base, head sha, merged/closed times, labels).
Usage: python3 -I c1_conflicts_stage1.py LOG OUTDIR
"""
import json, re, subprocess, sys, os, datetime as dt, collections

LOG, OUT = sys.argv[1], sys.argv[2]
os.makedirs(OUT, exist_ok=True)
GAP = 30 * 60
def ts(s): return dt.datetime.strptime(s, "%Y%m%dT%H%M%SZ").replace(tzinfo=dt.timezone.utc)

seen = collections.defaultdict(list)
ticks_per_repo = collections.Counter()
span = [None, None]
for l in open(LOG):
    p = l.split()
    if len(p) < 2 or "/" not in p[1]: continue
    t = ts(p[0]); ticks_per_repo[p[1]] += 1
    span[0] = span[0] or t; span[1] = t
    for m in re.finditer(r"#(\d+):merge_state:DIRTY:(\d+)m", l):
        seen[(p[1], int(m.group(1)))].append(t)

episodes = []
for (repo, pr), tl in sorted(seen.items()):
    tl.sort(); start = tl[0]; prev = tl[0]; n = 1
    for t in tl[1:] + [None]:
        if t is None or (t - prev).total_seconds() > GAP:
            episodes.append(dict(repo=repo, pr=pr, first=start.isoformat(), last=prev.isoformat(), sightings=n))
            if t: start = t; n = 0
        n += 1 if t else 0
        if t: prev = t
meta = {}
cache = os.path.join(OUT, "pr_meta.json")
if os.path.exists(cache): meta = json.load(open(cache))
calls = 0
for (repo, pr) in sorted(seen):
    k = f"{repo}#{pr}"
    if k in meta: continue
    tool = "ghread" if repo.startswith("Generous-Corp/") else "ghapp"
    exe = os.path.expanduser(f"~/.local/bin/{tool}")
    jq = '{state,merged_at,closed_at,created_at,base:.base.ref,head:.head.sha,labels:(.labels|map(.name)),title,merged}'
    r = subprocess.run([exe, "api", f"repos/{repo}/pulls/{pr}", "--jq", jq], capture_output=True, text=True)
    calls += 1
    if r.returncode: meta[k] = {"error": r.stderr.strip()[:200]}
    else: meta[k] = json.loads(r.stdout)
json.dump(meta, open(cache, "w"), indent=1)
json.dump(dict(episodes=episodes, ticks=ticks_per_repo, span=[x.isoformat() for x in span]), open(os.path.join(OUT, "episodes.json"), "w"), indent=1)
print("episodes", len(episodes), "prs", len(seen), "api_calls", calls, "errors", sum('error' in v for v in meta.values()))
