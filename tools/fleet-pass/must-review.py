#!/usr/bin/env python3
"""must-review.py [--repair | --adversary] [--missed FILE]: the PRs a scheduled pass must work on.

Review mode (default): open, non-draft PRs targeting main with no hold label,
every required check (other than review-verdict) green at the head, and no
verdict at that head. A PR whose author disputed the previous HOLD (a comment
starting "dispute:" after it) is marked DISPUTED so the reviewer re-judges the
disputed point instead of repeating it.

Repair mode (--repair): the rescue sweep. Open, non-draft, unlabelled PRs that
nothing has pushed to for STALL_MINUTES while they sit in a state only a push
can change: a HOLD at the head, a required check failed at the head, a merge
conflict, or a stacked base whose PR has already merged. A PR labelled
`stacked:<n>` (opened with `shipyard pr --stacked-on <n>`) is listed as soon as
PR n merges, without the stall wait, and never while n is open; if n closed
unmerged it is listed as STACK-PARENT-CLOSED for a person, never retargeted. A
label naming a PR whose head is not the stack's base is STACK-LABEL-MISMATCH, and
a parent that cannot be read is STACK-PARENT-UNREADABLE: neither is retargeted.
Fresh repair claims are honoured.

Adversary mode (--adversary): the second, adversarial review that Vellum's
required `release-path-review` status needs. Open, non-draft, unlabelled Vellum
PRs targeting main that change a release-path file (one main's
.github/CODEOWNERS names, or anything under .github/workflows/ or
.github/actions/), with every required check other than the two review statuses
green, exactly the gate's kind of counted APPROVE at the head from one signer,
no HOLD at the head, and no verdict at the head from the adversarial signer.

Output is one line per PR, "<repo>#<n> <head> [NOTE]", previously missed
entries (from FILE) first, then oldest first. Exits 2 when any repo could not
be read, so the runner never mistakes a failed read for "nothing to do".
"""
import datetime, json, os, re, subprocess, sys

REPOS = ["Generous-Corp/pulp", "Generous-Corp/vellum", "danielraffel/Shipyard", "danielraffel/tartci"]
HOLD_LABELS = {"owner-decision", "superseded"}
# Statuses that wait on a review cannot gate the review itself.
SELF = {"review-verdict", "post-review-verdict", "release-path-review"}
ADVERSARY_REPOS = ["Generous-Corp/vellum"]
ADVERSARY_SIGNER = "infra-review2"
RELEASE_PATH_PREFIXES = (".github/workflows/", ".github/actions/")
STALL_MINUTES = 45
VERDICT = re.compile(r"^verdict: (APPROVE|HOLD) head:([0-9a-f]{40})\s*$", re.M)
STACKED_LABEL = re.compile(r"^stacked:([1-9][0-9]*)$")
CLAIM = re.compile(r"repair-claim: \S+ until (\d{4}-\d\d-\d\dT\d\d:\d\d(?::\d\d)?Z)")


def api(path):
    out = subprocess.run(["ghapp", "api", "--paginate", "--slurp", path], capture_output=True, text=True)
    if out.returncode != 0:
        raise RuntimeError(f"ghapp api {path}: {out.stderr.strip()[:200]}")
    pages = json.loads(out.stdout or "[]")
    if pages and isinstance(pages[0], list):
        return [x for p in pages for x in p]
    # A single object can still paginate (a commit's `files`); the first page
    # carries every top-level field, so return it rather than the page list.
    if pages and isinstance(pages[0], dict) and "check_runs" not in pages[0] and "statuses" not in pages[0]:
        return pages[0]
    return pages[0] if len(pages) == 1 else pages


def ts(s):
    return datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))


def required(repo):
    ctx = set()
    try:
        for r in api(f"repos/{repo}/rules/branches/main"):
            if r.get("type") == "required_status_checks":
                ctx |= {c["context"] for c in r["parameters"]["required_status_checks"]}
    except RuntimeError:
        pass
    try:
        ctx |= {c["context"] for c in api(f"repos/{repo}/branches/main/protection/required_status_checks").get("checks", [])}
    except RuntimeError:
        pass
    return ctx - SELF


def rest_check_states(repo, sha):
    """REST fallback for a head the GraphQL snapshot does not hold."""
    runs = api(f"repos/{repo}/commits/{sha}/check-runs?per_page=100")
    runs = runs.get("check_runs", []) if isinstance(runs, dict) else [x for p in runs for x in p.get("check_runs", [])]
    st = api(f"repos/{repo}/commits/{sha}/status")
    return fold_states(runs, st.get("statuses", []))


def fold_states(runs, statuses):
    state = {}
    # Newest run per name wins, ordered by id: timestamps can be skewed (DST hour).
    for r in sorted(runs, key=lambda r: r["id"]):
        state[r["name"]] = r.get("conclusion") if r.get("status") == "completed" else "pending"
    # A commit status overrides a check run of the same name; the newest status wins.
    for s in sorted(statuses, key=lambda s: (s.get("id") or 0, s.get("created_at") or "")):
        state[s["context"]] = s["state"]
    return state


def check_states(repo, sha):
    for pr in _PRS.get(repo, ()):
        if pr["head"]["sha"] == sha:
            return fold_states(pr["_runs"], pr["_statuses"])
    return rest_check_states(repo, sha)


def comments(repo, n):
    for pr in _PRS.get(repo, ()):
        if pr["number"] == n:
            return pr["_comments"]
    return api(f"repos/{repo}/issues/{n}/comments?per_page=100")


# One paginated GraphQL query per repo replaces the per-PR commit, check-run, status,
# comment and detail REST reads. The result is shaped like the REST objects the rules
# below were written against.
_PRS = {}
PR_QUERY = """
query($owner:String!,$name:String!,$after:String){
  repository(owner:$owner,name:$name){
    pullRequests(states:OPEN,first:30,after:$after,orderBy:{field:CREATED_AT,direction:DESC}){
      pageInfo{hasNextPage endCursor}
      nodes{
        id number isDraft baseRefName headRefName headRefOid createdAt mergeStateStatus
        labels(first:50){nodes{name}}
        comments(last:100){totalCount nodes{body createdAt author{login __typename}}}
        commits(last:1){nodes{commit{committedDate statusCheckRollup{contexts(first:100){
          pageInfo{hasNextPage endCursor}
          nodes{__typename ... on CheckRun{name status conclusion databaseId}
                ... on StatusContext{context state createdAt}}}}}}}
      }
    }
  }
}"""
CONTEXTS_QUERY = """
query($id:ID!,$after:String){
  node(id:$id){... on PullRequest{commits(last:1){nodes{commit{statusCheckRollup{contexts(first:100,after:$after){
    pageInfo{hasNextPage endCursor}
    nodes{__typename ... on CheckRun{name status conclusion databaseId}
          ... on StatusContext{context state createdAt}}}}}}}}}
}"""


def graphql(repo, query, **variables):
    # ghapp mints its token per repository and cannot infer it from a GraphQL body or
    # variables, so the repository is bound through GH_REPO.
    cmd = ["ghapp", "api", "graphql", "-f", f"query={query}"]
    for k, v in variables.items():
        if v is not None:
            cmd += ["-f", f"{k}={v}"]
    out = subprocess.run(cmd, capture_output=True, text=True, env={**os.environ, "GH_REPO": repo})
    if out.returncode != 0:
        raise RuntimeError(f"ghapp api graphql: {out.stderr.strip()[:200] or out.stdout.strip()[:200]}")
    doc = json.loads(out.stdout or "{}")
    if doc.get("errors") or not doc.get("data"):
        raise RuntimeError(f"ghapp api graphql: {json.dumps(doc.get('errors'))[:200]}")
    return doc["data"]


def split_contexts(nodes):
    runs, statuses = [], []
    for c in nodes:
        if c.get("__typename") == "CheckRun":
            runs.append({"id": c["databaseId"], "name": c["name"],
                         "status": (c.get("status") or "").lower(),
                         "conclusion": (c.get("conclusion") or "").lower() or None})
        elif c.get("__typename") == "StatusContext":
            state = c["state"].lower()
            statuses.append({"context": c["context"], "created_at": c.get("createdAt") or "",
                             "state": "pending" if state == "expected" else state})
    return runs, statuses


def rest_comment(c):
    a = c.get("author") or {}
    login = a.get("login")
    bot = a.get("__typename") == "Bot"
    return {"body": c.get("body") or "", "created_at": c["createdAt"],
            "user": {"login": login + "[bot]" if bot and login else login, "type": "Bot" if bot else "User"}}


def fetch_prs(repo):
    """All open PRs of repo as REST-shaped dicts, newest first, fetched once per process."""
    if repo in _PRS:
        return _PRS[repo]
    owner, name = repo.split("/")
    out, after = [], None
    while True:
        page = graphql(repo, PR_QUERY, owner=owner, name=name, after=after)["repository"]["pullRequests"]
        for n in page["nodes"]:
            commit = ((n.get("commits") or {}).get("nodes") or [{}])[0].get("commit") or {}
            ctx = ((commit.get("statusCheckRollup") or {}).get("contexts")) or {}
            runs, statuses = split_contexts(ctx.get("nodes") or [])
            more = (ctx.get("pageInfo") or {})
            while more.get("hasNextPage"):
                nxt = graphql(repo, CONTEXTS_QUERY, id=n["id"], after=more["endCursor"])["node"]
                ctx = nxt["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]
                r2, s2 = split_contexts(ctx["nodes"])
                runs += r2
                statuses += s2
                more = ctx["pageInfo"]
            cs = [rest_comment(c) for c in n["comments"]["nodes"]]
            if n["comments"]["totalCount"] > len(cs):
                # More than the newest 100 comments: read them all, as the REST path did.
                cs = [{"body": c.get("body") or "", "created_at": c["created_at"], "user": c.get("user")}
                      for c in api(f"repos/{repo}/issues/{n['number']}/comments?per_page=100")]
            out.append({
                "number": n["number"], "draft": n["isDraft"], "state": "open",
                "labels": [{"name": l["name"]} for l in n["labels"]["nodes"]],
                "head": {"sha": n["headRefOid"], "ref": n["headRefName"]},
                "base": {"ref": n["baseRefName"]}, "created_at": n["createdAt"],
                "mergeable_state": (n.get("mergeStateStatus") or "").lower(),
                "_pushed": commit.get("committedDate"),
                "_runs": runs, "_statuses": statuses, "_comments": cs})
        if not page["pageInfo"]["hasNextPage"]:
            break
        after = page["pageInfo"]["endCursor"]
    _PRS[repo] = out
    return out


SIGNERS_FILE = __import__("os").path.expanduser(__import__("os").environ.get(
    "FLEET_PASS_REVIEWER_SIGNERS", "~/.config/shipyard/guards/reviewer-signers.json"))
SIGNERS = set(json.load(open(SIGNERS_FILE))["default"])
SIGNER = re.compile(r"^reviewer: (\S+)\s*$", re.M)
STAMP = re.compile(r"<!-- session: (\S+)")


def counted(body):
    """A verdict counts only from an allowlisted signer with an attributed session stamp,
    mirroring the review-verdict gate, so a stray or self-authored verdict line never
    removes a PR from the review queue."""
    signer = SIGNER.findall(body)
    stamp = STAMP.findall(body)
    if not (bool(signer) and signer[-1] in SIGNERS and bool(stamp) and not stamp[-1].endswith("/unattributed")):
        return False
    # Mirror the bases' [auto_merge] reviewer_stamp_sessions: an APPROVE counts only from a
    # scheduled reviewer run; a HOLD from any attributed reviewer session still counts.
    session = stamp[-1].split("/", 1)[-1]
    if "verdict: APPROVE" in body and not session.startswith("fleet-pass-reviewer-"):
        return False
    return True


def verdict_at(cs, sha):
    """(verdict, created_at) of the last counted verdict naming this exact head, or (None, None)."""
    last = (None, None)
    for c in cs:
        if not counted(c.get("body") or ""):
            continue
        for v, h in VERDICT.findall(c.get("body") or ""):
            if h == sha:
                last = (v, c["created_at"])
    return last


def last_hold(cs):
    last = None
    for c in cs:
        if any(v == "HOLD" for v, _ in VERDICT.findall(c.get("body") or "")):
            last = c["created_at"]
    return last


def disputed(cs):
    hold = last_hold(cs)
    return bool(hold) and any((c.get("body") or "").lstrip().lower().startswith("dispute:") and c["created_at"] > hold for c in cs)


def claimed(cs, now):
    for c in cs:
        for until in CLAIM.findall(c.get("body") or ""):
            if ts(until) > now:
                return True
    return False


def overrides():
    """Operator-routed re-reviews: "<repo>#<n> <head>" lines whose existing verdicts came
    from the PR's own author lane. Listed regardless of labels or prior verdicts, until a
    verdict newer than the override file exists."""
    import os
    p = os.path.join(os.path.dirname(os.path.abspath(__file__)), "review-override.txt")
    try:
        mtime = datetime.datetime.fromtimestamp(os.path.getmtime(p), datetime.timezone.utc)
        return {l.split()[0]: (l.split()[1], mtime) for l in open(p) if l.strip() and not l.startswith("#")}
    except FileNotFoundError:
        return {}


def review_candidates(repo, req, now):
    out = []
    for key, (sha, since) in overrides().items():
        if not key.startswith(repo + "#"):
            continue
        n = int(key.split("#")[1])
        pr = next((p for p in fetch_prs(repo) if p["number"] == n), None)
        if pr is None or pr["head"]["sha"] != sha:
            continue  # not open any more, or the head moved
        v, at = verdict_at(comments(repo, n), sha)
        if v and ts(at) > since:
            continue
        out.append((key, sha, pr["created_at"], "AUTHOR-SELF-APPROVED"))
    seen = {o[0] for o in out}
    for pr in fetch_prs(repo):
        if pr["base"]["ref"] != "main":
            continue
        labels = {l["name"] for l in pr["labels"]}
        if pr["draft"] or labels & HOLD_LABELS or any(l.startswith("PARKED/") for l in labels):
            continue
        sha = pr["head"]["sha"]
        st = check_states(repo, sha)
        if not all(st.get(c) in ("success", "neutral", "skipped") for c in req):
            continue
        if f"{repo}#{pr['number']}" in seen:
            continue
        cs = comments(repo, pr["number"])
        if verdict_at(cs, sha)[0]:
            continue
        out.append((f"{repo}#{pr['number']}", sha, pr["created_at"], "DISPUTED" if disputed(cs) else ""))
    return out


def raw(repo, path, ref="main"):
    out = subprocess.run(["ghapp", "api", "-H", "Accept: application/vnd.github.raw",
                          f"repos/{repo}/contents/{path}?ref={ref}"], capture_output=True, text=True)
    if out.returncode != 0:
        raise RuntimeError(f"ghapp api contents/{path}: {out.stderr.strip()[:200]}")
    return out.stdout


def release_path_policy(repo):
    """(protected files, signers, session globs) exactly as main's copy of the
    release-path-review gate reads them."""
    import tomllib
    protected = set()
    for line in raw(repo, ".github/CODEOWNERS").splitlines():
        fields = line.split("#", 1)[0].split()
        if fields:
            protected.add(fields[0].lstrip("/"))
    section = tomllib.loads(raw(repo, ".shipyard/config.toml")).get("auto_merge") or {}
    signers = {str(x).lower() for x in section.get("reviewer_signers") or []}
    globs = [str(x).lower() for x in section.get("reviewer_stamp_sessions") or []]
    if not protected or not signers or not globs:
        raise RuntimeError(f"{repo}: main's release-path policy is unreadable")
    return protected, signers, globs


GATE_STAMP = re.compile(r"<!-- session: ([!-.0-~]+)/([!-~]+?)(?: agent: ([!-~]+?))? -->")


def gate_approval(c, sha, signers, globs):
    """The signer of a comment the release-path-review gate counts as an APPROVE of sha, or None."""
    import fnmatch
    body = c.get("body") or ""
    user = c.get("user") or {}
    if user.get("login") != "shipyard-local[bot]" or user.get("type") != "Bot":
        return None
    lines = [l for l in body.splitlines() if l.strip()]
    if not lines or not re.fullmatch(rf"[ \t]*(?:verdict:[ \t]*APPROVE[ \t]+head:[ \t]*{sha}|reviewed:[ \t]*{sha})[ \t]*", lines[-1]):
        return None
    stamps = GATE_STAMP.findall(body)
    if len(stamps) != 1 or re.search(r"<!--\s*session\s*:", GATE_STAMP.sub("", body), re.I):
        return None
    session = stamps[0][1].lower()
    if session == "unattributed" or not any(fnmatch.fnmatchcase(session, g) for g in globs):
        return None
    named = {n.lower() for n in SIGNER.findall(body)}
    return named.pop() if len(named) == 1 and named <= signers else None


def adversary_candidates(repo, req, now):
    if repo not in ADVERSARY_REPOS:
        return []
    protected, signers, globs = release_path_policy(repo)
    out = []
    for pr in fetch_prs(repo):
        if pr["base"]["ref"] != "main":
            continue
        labels = {l["name"] for l in pr["labels"]}
        if pr["draft"] or labels & HOLD_LABELS or any(l.startswith("PARKED/") for l in labels):
            continue
        sha = pr["head"]["sha"]
        files = api(f"repos/{repo}/pulls/{pr['number']}/files?per_page=100")
        names = {f.get(k) for f in files for k in ("filename", "previous_filename") if f.get(k)}
        if not any(n in protected or n.startswith(RELEASE_PATH_PREFIXES) for n in names):
            continue
        st = check_states(repo, sha)
        if not all(st.get(c) in ("success", "neutral", "skipped") for c in req):
            continue
        cs = comments(repo, pr["number"])
        at_head = [v for c in cs for v, h in VERDICT.findall(c.get("body") or "") if h == sha]
        if "HOLD" in at_head:
            continue
        approvers = {s for s in (gate_approval(c, sha, signers, globs) for c in cs) if s}
        if len(approvers) != 1 or ADVERSARY_SIGNER in approvers:
            continue
        if any(ADVERSARY_SIGNER in SIGNER.findall(c.get("body") or "") and VERDICT.search(c.get("body") or "")
               and any(h == sha for _, h in VERDICT.findall(c.get("body") or "")) for c in cs):
            continue
        out.append((f"{repo}#{pr['number']}", sha, pr["created_at"], f"RELEASE-PATH approved-by:{approvers.pop()}"))
    return out


def stacked_parent(pr):
    """The parent PR number a `stacked:<n>` label names, or None."""
    for label in pr["labels"]:
        m = STACKED_LABEL.match(label["name"])
        if m:
            return int(m.group(1))
    return None


def stacked_state(repo, pr, open_heads_by_number):
    """(state, parent) for a labelled stacked PR still off main. state is "open",
    "merged" or "closed" for a parent whose head branch is this PR's base;
    "mismatch" when the label names a PR whose head is not this PR's base (the
    label is unverified: anyone with triage can add it); "unreadable" when the
    parent cannot be read (a bogus number that 404s). (None, parent) when unlabelled
    or already on main, where the label no longer means anything. Reads the parent
    only once it has left the open list, so a pass costs one REST read per stack
    whose parent closed. Never raises: one bad label must not abort the listing."""
    parent = stacked_parent(pr)
    base = pr["base"]["ref"]
    if parent is None or base == "main":
        return None, parent
    if parent in open_heads_by_number:
        return ("open" if open_heads_by_number[parent] == base else "mismatch"), parent
    try:
        p = api(f"repos/{repo}/pulls/{parent}")
    except RuntimeError:
        return "unreadable", parent
    if not isinstance(p, dict) or ((p.get("head") or {}).get("ref")) != base:
        return ("unreadable" if not isinstance(p, dict) else "mismatch"), parent
    return ("merged" if p.get("merged_at") else "closed"), parent


STACK_NOTES = {"merged": "STACK-BASE-MERGED", "closed": "STACK-PARENT-CLOSED",
               "mismatch": "STACK-LABEL-MISMATCH", "unreadable": "STACK-PARENT-UNREADABLE"}


def repair_candidates(repo, req, now):
    out = []
    prs = fetch_prs(repo)
    open_heads = {p["head"]["ref"] for p in prs}
    open_numbers = {p["number"]: p["head"]["ref"] for p in prs}
    for pr in prs:
        labels = {l["name"] for l in pr["labels"]}
        if pr["draft"] or labels & HOLD_LABELS or any(l.startswith("PARKED/") for l in labels):
            continue
        sha = pr["head"]["sha"]
        pushed = ts(pr["_pushed"])
        stack, parent = stacked_state(repo, pr, open_numbers)
        if stack == "open":
            continue  # invisible until its parent merges
        if stack in STACK_NOTES:
            # Immediate: the parent's merge is the event, not a stall.
            if not claimed(comments(repo, pr["number"]), now):
                out.append((f"{repo}#{pr['number']}", sha, pushed.isoformat(),
                            f"{STACK_NOTES[stack]} parent:#{parent}"))
            continue
        if (now - pushed).total_seconds() < STALL_MINUTES * 60:
            continue
        cs = comments(repo, pr["number"])
        if claimed(cs, now):
            continue
        reason = ""
        base = pr["base"]["ref"]
        if base != "main" and base not in open_heads:
            reason = "STACK-BASE-MERGED"  # its base PR is gone: retarget to main, merge main
        elif base == "main":
            v, _ = verdict_at(cs, sha)
            if v == "HOLD":
                reason = "HOLD"
            else:
                st = check_states(repo, sha)
                failed = [c for c in req if st.get(c) in ("failure", "timed_out", "cancelled", "action_required")]
                if failed:
                    reason = "FAILED:" + ",".join(sorted(failed))
                else:
                    if pr["mergeable_state"] == "dirty":
                        reason = "CONFLICT"
        if reason:
            out.append((f"{repo}#{pr['number']}", sha, pushed.isoformat(), reason))
    return out


def main():
    args = sys.argv[1:]
    mode_repair = "--repair" in args
    mode_adversary = "--adversary" in args
    missed = []
    if "--missed" in args:
        try:
            missed = [l.split()[0] for l in open(args[args.index("--missed") + 1]) if l.strip()]
        except (FileNotFoundError, IndexError):
            pass
    now = datetime.datetime.now(datetime.timezone.utc)
    found, failed = [], False
    for repo in REPOS:
        try:
            req = required(repo)
            mode = repair_candidates if mode_repair else adversary_candidates if mode_adversary else review_candidates
            found += mode(repo, req, now)
        except RuntimeError as e:
            print(f"# {repo}: query failed: {e}", file=sys.stderr)
            failed = True
    found.sort(key=lambda f: (f[0] not in missed, f[2]))
    for key, sha, _, note in found:
        print(f"{key} {sha} {note}".rstrip())
    sys.exit(2 if failed else 0)


if __name__ == "__main__":
    main()
