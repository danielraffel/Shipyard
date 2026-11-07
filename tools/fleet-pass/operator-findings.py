#!/usr/bin/env python3
"""operator-findings.py: what is stuck in the drain right now, for the scheduled operator pass.

Reads live GitHub state and the latest steward tick JSON per repo. Prints one line per
stuck PR with the steward's own blocked_on reason, or nothing when the drain is healthy.
Exits 2 when a repo could not be read, so the runner never mistakes a failed read for
"nothing stuck".
"""
import datetime, glob, json, os, sys
import importlib.util

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("mr", os.path.join(HERE, "must-review.py"))
mr = importlib.util.module_from_spec(spec); spec.loader.exec_module(mr)

APPROVED_IDLE_MIN = 20   # approved + green but not queued
NO_VERDICT_MIN = 45      # green with no verdict (reviewer missed it)
HELD_IDLE_MIN = 60       # HOLD / required failure / conflict with no push (rescue missed it)


def steward_rows(repo):
    files = sorted(glob.glob(os.path.expanduser(
        f"~/Library/Logs/shipyard-steward-tick/{repo.replace('/', '_')}-*.json")))
    for f in reversed(files):
        if os.path.getsize(f):
            try:
                d = json.load(open(f))
                return {p["number"]: p for p in d["repos"][0].get("prs", [])}, d["repos"][0]
            except (ValueError, KeyError, IndexError):
                continue
    return {}, {}


def main():
    now = datetime.datetime.now(datetime.timezone.utc)
    failed = False
    for repo in mr.REPOS:
        try:
            req = mr.required(repo)
            rows, rep = steward_rows(repo)
            queued = set()
            owner, name = repo.split("/")
            qd = mr.api(f"repos/{repo}/pulls?state=open&per_page=100")
            for pr in qd:
                n = pr["number"]
                labels = {l["name"] for l in pr["labels"]}
                if pr["draft"] or labels & mr.HOLD_LABELS or any(l.startswith("PARKED/") for l in labels):
                    continue
                sha = pr["head"]["sha"]
                pushed = mr.ts(mr.api(f"repos/{repo}/commits/{sha}")["commit"]["committer"]["date"])
                idle = (now - pushed).total_seconds() / 60
                row = rows.get(n, {})
                why = row.get("blocked_on") or "-"
                detail = (row.get("blocked_detail") or row.get("error") or "")[:160]
                if why in ("queued", "admitted"):
                    continue
                st = mr.check_states(repo, sha)
                green = all(st.get(c) in ("success", "neutral", "skipped") for c in req)
                v, _ = mr.verdict_at(mr.comments(repo, n), sha)
                base = pr["base"]["ref"]
                if base != "main":
                    if idle > HELD_IDLE_MIN and why.startswith(("carried", "steward_error")):
                        print(f"{repo}#{n} {sha[:10]} STACKED-BLOCKING idle={int(idle)}m steward={why} {detail}")
                    continue
                if v == "APPROVE" and green and idle > APPROVED_IDLE_MIN:
                    print(f"{repo}#{n} {sha[:10]} APPROVED-NOT-QUEUED idle={int(idle)}m steward={why} {detail}")
                elif green and not v and idle > NO_VERDICT_MIN:
                    print(f"{repo}#{n} {sha[:10]} GREEN-NO-VERDICT idle={int(idle)}m steward={why}")
                elif (v == "HOLD" or not green) and idle > HELD_IDLE_MIN:
                    print(f"{repo}#{n} {sha[:10]} STALLED idle={int(idle)}m verdict={v or 'none'} steward={why} {detail}")
            if rep.get("errors"):
                print(f"{repo} STEWARD-ERRORS {json.dumps(rep.get('errors'))[:200]}")
        except RuntimeError as e:
            print(f"# {repo}: query failed: {e}", file=sys.stderr)
            failed = True
    sys.exit(2 if failed else 0)


if __name__ == "__main__":
    main()
