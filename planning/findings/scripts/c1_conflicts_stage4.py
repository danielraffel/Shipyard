#!/usr/bin/env python3
"""C1 stage 4: classify conflicting paths/episodes and print summary tables (markdown).
Usage: python3 -I c1_conflicts_stage4.py DATA LOG
"""
import json, re, sys, collections, statistics, datetime as dt
DATA, LOG = sys.argv[1], sys.argv[2]
res = json.load(open(f"{DATA}/results.json")); hunks = json.load(open(f"{DATA}/hunks.json"))
ep = json.load(open(f"{DATA}/episodes.json"))

CT = re.compile(r"^\s*(SURFACE_INVENTORY_VERSION|MANIFEST_REVISION)\s*=\s*\d+\s*$")
FP = re.compile(r"""(header_fingerprint\s*=\s*['"]sha256:|['"]fingerprint['"]\s*:\s*['"]sha256:)""")
AMF = "tools/scripts/agent_capability_manifest.py"
GEN = {  # exact path or prefix -> (kind, generator)
 "docs/status/agent-capability-surface.json": ("generated", "python3 tools/scripts/agent_capability_manifest.py --write (named in tools/scripts/hotspot_edit_guard.json)"),
 "tools/agent-capabilities/contract-history.json": ("generated", "python3 tools/scripts/agent_capability_manifest.py --write (hotspot_edit_guard.json)"),
 "test/ctest_script_inputs.json": ("generated", "python3 tools/scripts/script_test_inputs.py --build-dir <configured build dir> --write (hotspot_edit_guard.json; needs a configured build)"),
 "docs/status/consumption-profiles.json": ("generated", "python3 tools/scripts/consumption_census.py --build-dir <dir> --write (hotspot_edit_guard.json, serialized hotspot; needs a build)"),
 "fleet/advertised-labels.json": ("generated", "tartci fleet-macos advertised-labels --publish (scripts/macos_fleet_lanes.py)"),
}
GENPFX = [
 ("docs/status/agent-capability-surface/", "agent_capability_manifest.py --write (fragment form)"),
 ("tools/agent-capabilities/fingerprints/", "agent_capability_manifest.py --accept-fingerprint (reviewed digest)"),
 ("test/ctest_script_inputs/", "script_test_inputs.py --write (fragment form)"),
]
DERIVED = {  # generated-looking, generator not located
 "scripts/INSTALLER_SHA256SUMS": "sha256 of scripts/install_core.py + scripts/install.sh; commit history says 'Regenerate INSTALLER_SHA256SUMS'; no script found",
 "provenance/pulp-observatory/reports/current.json": "'chore(provenance): coalesce Pulp observatory evidence' commits; generator not located",
 "native-ui/materialized/materialized-document.runtime.json": "materialized UI runtime document; generator not located",
}
def lines_all(hs, pred):
    return isinstance(hs, list) and hs and all(pred(l) for h in hs for side in h for l in side if l.strip())

def classify(repo, path, hs):
    """-> (class, note). class in: gen, counter, digest, derived, docs, config, source"""
    if path in GEN: return "gen", GEN[path][1]
    for p, g in GENPFX:
        if path.startswith(p): return "gen", g
    if path == AMF and lines_all(hs, lambda l: CT.match(l)):
        return "counter", "SURFACE_INVENTORY_VERSION / MANIFEST_REVISION literal; assigned at land by agent_capability_manifest.py --assign-pending (CI)"
    if re.search(r"agent_capability_(catalog_\w+|registry)\.py$", path) and lines_all(hs, lambda l: FP.search(l)):
        return "digest", "header digest literal; agent_capability_manifest.py --accept-fingerprint / --migrate-fingerprints (acceptance is a review step)"
    if path in DERIVED: return "derived", DERIVED[path]
    if path.endswith("CMakeLists.txt"): return "config", ""
    if path.endswith((".md", ".txt")) or "/docs/" in "/" + path: return "docs", ""
    if path.endswith((".yml", ".yaml", ".toml", ".cmake", "CMakeLists.txt", ".plist.template", ".gitignore", "CODEOWNERS")) or path.startswith(".github/") or path.endswith("SKILL.md") and False: return "config", ""
    if path.endswith(".json"): return "config", "hand-edited JSON"
    return "source", ""

rows = []
for x in res:
    if x["status"] != "conflict": continue
    key = f"{x['repo']}#{x['pr']}@{x['first']}"
    cl = {p: classify(x["repo"], p, hunks[key][p]) for p in x["paths"]}
    kinds = {c for c, _ in cl.values()}
    strict = {"gen", "counter"}; lenient = strict | {"digest", "derived"}
    if kinds <= strict: cat = "mechanical"
    elif kinds <= lenient: cat = "mechanical-lenient"
    elif kinds & lenient: cat = "mixed"
    else: cat = "hand"
    x["cat"] = cat; x["classes"] = {p: c for p, (c, _) in cl.items()}; rows.append(x)

def pct(a, b): return f"{100*a/b:.0f}%" if b else "-"
repos = sorted({r["repo"] for r in rows})
print("## Episode categories\n")
print("| repo | conflict episodes | mechanical (strict) | +digest/derived (lenient) | mixed | hand-written | distinct PRs |")
print("|---|---|---|---|---|---|---|")
tot = collections.Counter(); 
for repo in repos + ["ALL"]:
    rs = [r for r in rows if repo == "ALL" or r["repo"] == repo]
    c = collections.Counter(r["cat"] for r in rs); n = len(rs)
    print(f"| {repo} | {n} | {c['mechanical']} ({pct(c['mechanical'],n)}) | {c['mechanical-lenient']} ({pct(c['mechanical-lenient'],n)}) | {c['mixed']} ({pct(c['mixed'],n)}) | {c['hand']} ({pct(c['hand'],n)}) | {len({r['pr'] for r in rs})} |")
print("\n## Top conflicting paths per repo (episodes containing the path, share of repo conflict episodes)\n")
for repo in repos:
    rs = [r for r in rows if r["repo"] == repo]; n = len(rs)
    c = collections.Counter(p for r in rs for p in r["paths"])
    print(f"\n### {repo} ({n} episodes)\n\n| path | episodes | share | class |\n|---|---|---|---|")
    for p, k in c.most_common(8):
        cls = collections.Counter(r["classes"][p] for r in rs if p in r["paths"]).most_common(1)[0][0]
        print(f"| `{p}` | {k} | {pct(k,n)} | {cls} |")
print("\n## Path class totals (path-occurrences across all conflict episodes)\n")
pc = collections.Counter(c for r in rows for c in r["classes"].values()); t = sum(pc.values())
for k, v in pc.most_common(): print(f"- {k}: {v} ({pct(v,t)})")
print("\n## Mechanical paths with named generator\n")
gens = collections.Counter(); 
for r in rows:
    for p, c in r["classes"].items():
        if c in ("gen", "counter", "digest", "derived"):
            gens[(p if not p.startswith(("docs/status/agent-capability-surface/","tools/agent-capabilities/fingerprints/","test/ctest_script_inputs/")) else p.split("/")[0]+"/.../"+p.split("/")[-1][:0]+"(fragments)", c)] += 1
for (p, c), k in gens.most_common(): print(f"- {p} [{c}] {k}")

# endings
print("\n## How episodes ended\n")
def state(r):
    if r["pr_state"] == "merged": return "merged"
    if r["pr_state"] == "closed": return "closed-unmerged" + ("(superseded label)" if "superseded" in r["labels"] else "")
    return "open"
allr = [r for r in res if r["status"] == "conflict"]
sc = collections.Counter(state(r) for r in allr)
for k, v in sc.items(): print(f"- {k}: {v} episodes ({pct(v,len(allr))})")
prs = {(r["repo"], r["pr"]): state(r) for r in res}
pc2 = collections.Counter(prs.values()); print("- per distinct PR (all 114 PRs incl. unreconstructed):", dict(pc2))
mg = [r for r in allr if r["pr_state"] == "merged"]
def med(a): return statistics.median(a) if a else None
print("- merged episodes: median commits on PR after the episode head:", med([r["commits_after"] for r in mg]), "; median merge commits:", med([r["merge_commits_after"] for r in mg]),
      "; share with >=1 merge commit after:", pct(sum(r["merge_commits_after"] > 0 for r in mg), len(mg)))
def hrs(a, b): return (dt.datetime.fromisoformat(a.replace("Z", "+00:00")) - dt.datetime.fromisoformat(b)).total_seconds() / 3600
print("- merged episodes: median hours from first DIRTY sighting to merge:", round(med([hrs(r["merged_at"], r["first"]) for r in mg]), 1),
      "; p90:", round(sorted([hrs(r["merged_at"], r["first"]) for r in mg])[int(.9*len(mg))], 1))
print("- merged episodes: median observed DIRTY duration (last-first sighting, lower bound), minutes:", round(med([hrs(r["last"].replace("+00:00","Z"), r["first"])*60 for r in mg]), 0))
print("- episodes whose PR head never changed after the episode head:", sum(not r["head_changed_after"] for r in allr))
print("- PR recurrence: PRs with >1 conflict episode:", sum(1 for k, v in collections.Counter((r["repo"], r["pr"]) for r in allr).items() if v > 1), "of", len({(r['repo'], r['pr']) for r in allr}))
for cat in ["mechanical", "mechanical-lenient", "mixed", "hand"]:
    s = [r for r in rows if r["cat"] == cat]
    if s: print(f"- {cat}: end state {dict(collections.Counter(state(r) for r in s))}; median PR files changed {med([r['files_changed_by_pr'] for r in s])}; median conflicting paths {med([len(r['paths']) for r in s])}")

# abandoned
ab = [r for r in allr if r["pr_state"] == "closed"]
print("\n## Abandoned\n\n- conflict episodes on PRs later closed unmerged:", len(ab), pct(len(ab), len(allr)))
print("- of those, mechanical/lenient/mixed/hand:", dict(collections.Counter(r["cat"] for r in ab)))
# coverage
sat = tot_d = 0; ticks = 0; dirty_ticks = 0
for l in open(LOG):
    p = l.split()
    if len(p) < 2 or "/" not in p[1]: continue
    ticks += 1
    m = re.search(r"oldest=(\S+)", l)
    if m:
        items = m.group(1).split(",")
        n = sum(1 for i in items if "merge_state:DIRTY" in i)
        if n: dirty_ticks += 1
        if len(items) >= 3: sat += 1
print("\n## Coverage\n")
print(f"- log span {ep['span'][0][:10]}..{ep['span'][1][:10]}; tick lines {ticks}; ticks listing 3 PRs (list saturated, others hidden): {sat} ({pct(sat,ticks)}); ticks with a DIRTY PR listed: {dirty_ticks}")
print("- status counts:", dict(collections.Counter(r["status"] for r in res)), "of", len(res), "episodes; reproduced-on-first-variant:", sum(r.get("variant") == "first/first" for r in res))
print("- unreconstructed:", [(r["repo"].split("/")[1], r["pr"], r.get("why", "clean at reconstruction")) for r in res if r["status"] != "conflict"][:50])
# post-fragmentation analysis for pulp
pl = [r for r in rows if r["repo"].endswith("/pulp")]
cut1 = "2026-11-08T20:00:00+00:00"; cut2 = "2026-11-09T15:36:00+00:00"
print("\n## Pulp hot files vs the fragment migrations\n")
for name, pred, cut in [("ctest_script_inputs.json", lambda r: "test/ctest_script_inputs.json" in r["paths"], cut1),
                        ("agent-capability-surface.json / contract-history.json", lambda r: any(p in r["paths"] for p in ("docs/status/agent-capability-surface.json","tools/agent-capabilities/contract-history.json")), cut2)]:
    a = [r for r in pl if pred(r)]
    print(f"- {name}: {len(a)} episodes total; before migration cutoff {sum(r['first']<cut for r in a)}; after {sum(r['first']>=cut for r in a)}")
hot = lambda r: any(c in ("gen","counter","digest") for c in r["classes"].values())
print("- pulp episodes touching any generated/counter/digest path:", sum(hot(r) for r in pl), "of", len(pl), pct(sum(hot(r) for r in pl), len(pl)))
print("- pulp episodes by day:", sorted(collections.Counter(r["first"][:10] for r in pl).items()))
print("- pulp episodes with any generated/counter/digest path, by day:", sorted(collections.Counter(r["first"][:10] for r in pl if hot(r)).items()))
json.dump(rows, open(f"{DATA}/classified.json", "w"))
