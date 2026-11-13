#!/usr/bin/env python3
"""C1 stage 6: summary of the repair estimate and per-PR numbers (markdown lines).
Usage: python3 -I c1_conflicts_stage6.py DATA"""
import json, sys, collections, statistics
rows = json.load(open(sys.argv[1] + "/classified.json"))
med = lambda a: statistics.median(a) if a else None
def q(a, f): a = sorted(a); return a[min(len(a)-1, int(f*len(a)))] if a else None
print("| category | episodes | cleared (est.) | median first-parent commits to clear | share cleared by a merge commit | median minutes to clearing commit | p90 min |")
print("|---|---|---|---|---|---|---|")
for cat in ["mechanical", "mechanical-lenient", "mixed", "hand", "ALL"]:
    s = [r for r in rows if cat == "ALL" or r["cat"] == cat]; f = [r["fix"] for r in s if r["fix"]]
    if not s: continue
    print(f"| {cat} | {len(s)} | {len(f)} | {med([x['steps'] for x in f])} | {sum(x['is_merge'] for x in f)}/{len(f)} | {med([x['delay_min'] for x in f])} | {q([x['delay_min'] for x in f],.9)} |")
print()
perpr = collections.defaultdict(list)
for r in rows: perpr[(r["repo"], r["pr"])].append(r)
st = collections.Counter()
for k, v in perpr.items(): st[v[0]["pr_state"]] += 1
print("per conflicting PR:", dict(st), "of", len(perpr))
by = collections.defaultdict(collections.Counter)
for k, v in perpr.items(): by[k[0]][v[0]["pr_state"]] += 1
for k, v in by.items(): print(k, dict(v))
# a PR is 'mechanical-only' if every one of its episodes is mechanical or lenient
mo = [k for k, v in perpr.items() if all(r["cat"] in ("mechanical", "mechanical-lenient") for r in v)]
print("PRs whose every conflict episode was mechanical (strict+lenient):", len(mo), "of", len(perpr))
ms = [k for k, v in perpr.items() if all(r["cat"] == "mechanical" for r in v)]
print("PRs whose every conflict episode was strictly mechanical:", len(ms))
# how many episodes remain if gen/counter/digest/derived paths never appear in PR diffs
rem = [r for r in rows if any(c in ("source", "docs", "config") for c in r["classes"].values())]
print("episodes that still conflict with hand-written paths if all generated/counter/digest/derived paths left PR diffs:", len(rem), "of", len(rows))
tot = sum(len(r["paths"]) for r in rows); h = sum(1 for r in rows for c in r["classes"].values() if c in ("source", "docs", "config"))
print("path-occurrences: total", tot, "hand-written", h)
# stacked/superseded
print("labels superseded:", sum("superseded" in r["labels"] for r in rows))
# per-repo hand-written classes
for repo in sorted({r["repo"] for r in rows}):
    c = collections.Counter(cl for r in rows if r["repo"] == repo for cl in r["classes"].values())
    print(repo, dict(c))
