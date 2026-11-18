"""End-to-end metrics per merged PR, per repo, per UTC day (merge day).

Used by loss_report.py. Standard library only. Reads GitHub through `ghread api`
(Generous-Corp) or `ghapp api` (everything else), caches every answer worth
keeping on disk, and stops at a per-run read cap (`truncated`). Anything it
cannot read is "unknown" or null, never a zero.

Definitions (also in docs/metrics.md):
  autonomous merge rate  PRs merged that day with no owner action from opening to merge,
                         over PRs where that is determinable; the rest count as unknown.
  handoff-to-merge time  median and p90 minutes from the first ready-for-review moment
                         to merged_at.
  compute per merged PR  Actions job minutes over runs on the PR's heads plus its
                         merge_group runs, split self-hosted and hosted.
  correctness            share of merged PRs with a revert or a red required check on the
                         base within 7 days; provisional while the window is open.
"""
from __future__ import annotations

import collections
import datetime as dt
import json
import math
import os
import re
import statistics
import subprocess
import sys
import base64
import importlib.util

UTC = dt.timezone.utc
WINDOW = dt.timedelta(days=7)
# Timeline events that are an actor's own action on the PR (noise like
# `subscribed` or `cross-referenced` is not).
ACTION_EVENTS = {
    "labeled", "unlabeled", "commented", "reviewed", "review_dismissed", "review_requested",
    "review_request_removed", "closed", "reopened", "merged", "assigned", "unassigned",
    "ready_for_review", "converted_to_draft", "auto_merge_enabled", "auto_merge_disabled",
    "enqueued", "dequeued", "milestoned", "demilestoned", "renamed", "base_ref_changed",
}
AGENT_TRAILER_RE = re.compile(
    r"co-authored-by:[^\n]*(claude|codex|anthropic|openai|copilot)|generated with[^\n]*(claude|codex)"
    r"|^(version-bump|skill-update|release)\s*:", re.I | re.M)
# Agent-authorship signals, strongest first. A PR carrying any of them is agent work even when its commits
# carry the owner's git identity, which is how agents commit on this fleet.
APP_OPENERS = {"shipyard-local[bot]"}
PROVENANCE_RE = re.compile(r"<!--\s*(shipyard-owner|whence)\b", re.I)
STAMP_RE = re.compile(r"<!--\s*session:\s*\S+", re.I)
GUARD_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "scripts", "ghapp_queue_arm_guard.py")
APP_LOGINS = {"shipyard-local[bot]", "shipyard-local"}
_GUARD = []
SIGNALS = ("app_opener", "provenance", "trailer", "session_stamp")
BAD_CONCLUSIONS = {"failure", "timed_out"}
FACTS_VERSION = 14  # 14: merge-queue ejection causes read from GraphQL; push_times are observed head updates; main-red facts
COMPUTE_VERSION = 7  # compute caches: entries written before this could be marked complete from a partial timeline or commit read or rerun history
CHECKS_VERSION = 3  # derived check caches: v2 entries (incomplete pagination read as clean, latest run by start) are read again
ESCAPE_WINDOW = dt.timedelta(days=14)
HORIZON = dt.timedelta(hours=48)
MIN_SAMPLE = 50
UNKNOWN_RANGE_SHARE = 0.20
SCORECARD_VERSION = "2026-11-17.3"  # .2: correctness reads the first-parent next push and the newest run by id; main-red episodes. .3: ejection causes from GraphQL
MERGE_MAIN_RE = re.compile(r"^Merge (remote-tracking )?branch '?(origin/)?(main|master)'?|^Merge pull request .* from .*/main", re.I)
# Positive controls: a real revert (#124 reverted #119), a real escaped defect (#126 names #116).
# Pulp: two escapes verified by hand from main-red traces. #12219 landed red on Workflow lint and
# #12375 fixed it; #12338 broke the WAM link and #12384 fixed it. Neither fix names the PR it fixes,
# so the text-based escape detector misses both, and the Pulp escaped metric is withheld until it can
# find them.
DEFAULT_CONTROLS = {  # "owner": [pr, ...] pins PRs with a hand-verified owner action
    "danielraffel/fleet-handbook": {"revert": [(119, 124)], "escape": [(116, 126)]},
    "Generous-Corp/pulp": {"escape": [(12219, 12375), (12338, 12384)]},
    # hand-verified main-red incident: #1459's merge 71c87fcd turned `Python helper tests [github-hosted]` red on main,
    # #1462's merge 22f19c07 turned it green, and #1459's own head f9bf6b3c was green there at merge (check runs read
    # 2026-11-17): other post-merge red, not a change failure. (escaped, fixer[, head red at merge])
    "danielraffel/Shipyard": {"incident": [(1459, 1462, False)]},
}
# Labels that take a PR out of the eligible cohort, with the reason reported.
# The gate-proof workflows whose runs per merged PR are reported by name, per repo; any other repo shows its busiest workflows.
PROOF_WORKFLOWS = {"Generous-Corp/vellum": ("Locked macOS GPU proof",)}
EXCLUDE_LABELS = {"automation": "automation", "shipyard:unmanaged": "unmanaged", "superseded": "superseded"}
INTEGRATION_BASES = ("develop/", "release/")
REPAIRED_RE = re.compile(r"^\s*repaired\s*:", re.I)
HOLD_RE = re.compile(r"^\s*[*_#> ]*(HOLD\b|(?i:verdict)\s*:\s*HOLD\b)", re.M)
ESCAPE_RE = r"(?i)\b(fix\w*|regress\w*|broke\w*|broken|caused|introduc\w*)\b[^\n]{0,60}?(?<![\w#])#%d(?!\d)"


DEFAULT_READ_CAP = 1500  # shared total per run, ~3 reads per PR
DEFAULT_REPOS = ("Generous-Corp/pulp", "Generous-Corp/vellum", "danielraffel/Shipyard", "danielraffel/tartci", "danielraffel/fleet-handbook")


class Truncated(Exception):
    """The per-run read cap was reached."""


class ReadError(Exception):
    pass


def iso(s):
    if not s:
        return None
    try:
        d = dt.datetime.fromisoformat(str(s).replace("Z", "+00:00"))
    except ValueError:
        return None
    return d if d.tzinfo else d.replace(tzinfo=UTC)


def fmt(t):
    return t.astimezone(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def gh_runner(repo):
    """The command that reads `repo`: the read-only admission reader for Generous-Corp, else the App."""
    tool = "ghread" if repo.startswith("Generous-Corp/") else "ghapp"

    def run(path):
        argv, env = [tool, "api", path], None
        if path.startswith("graphql:"):  # ghread has no GraphQL route: the App reads it, with the repo named for provenance
            argv, env = ["ghapp", "api", "graphql", "-f", "query=" + path[len("graphql:"):]], dict(os.environ, GH_REPO=repo)
        try:
            p = subprocess.run(argv, capture_output=True, text=True, timeout=30, env=env)
        except (OSError, subprocess.TimeoutExpired) as e:
            raise ReadError(f"{tool}: {type(e).__name__}") from e
        if p.returncode != 0:
            raise ReadError(f"{tool} exit {p.returncode}: {p.stderr.strip()[:120]}")
        try:
            return json.loads(p.stdout)
        except ValueError as e:
            raise ReadError(f"{tool}: not JSON") from e
    return run


class Reader:
    """GitHub GET with a hard cap on calls for one repo in one run, and an on-disk cache."""

    def __init__(self, repo, cache_dir, cap, fetch=None):
        self.repo, self.cap, self.calls = repo, cap, 0
        self.fetch = fetch or gh_runner(repo)
        self.root = os.path.join(cache_dir, repo.replace("/", "__"))
        self.truncated = False
        self.errors = 0
        self.policies = {}

    def get(self, path):
        if self.calls >= self.cap:
            self.truncated = True
            raise Truncated()
        self.calls += 1
        try:
            return self.fetch(f"repos/{self.repo}/{path}")
        except ReadError:
            self.errors += 1
            raise

    def gql(self, query):
        """One GraphQL read, counted against the same cap as a REST read."""
        if self.calls >= self.cap:
            self.truncated = True
            raise Truncated()
        self.calls += 1
        try:
            d = self.fetch("graphql:" + query)
        except ReadError:
            self.errors += 1
            raise
        if not isinstance(d, dict) or d.get("errors") and not d.get("data"):
            self.errors += 1
            raise ReadError("graphql error")
        return d

    def pages(self, path, key=None, max_pages=3):
        """All items of a paginated list (up to max_pages of 100). Returns (items, complete)."""
        out = []
        sep = "&" if "?" in path else "?"
        for n in range(1, max_pages + 1):
            d = self.get(f"{path}{sep}per_page=100&page={n}")
            items = d.get(key, []) if key and isinstance(d, dict) else d
            if not isinstance(items, list):
                raise ReadError("unexpected shape")
            out += items
            if len(items) < 100:
                return out, True
        return out, False

    def load(self, kind, name):
        try:
            with open(os.path.join(self.root, kind, f"{name}.json")) as fh:
                return json.load(fh)
        except (OSError, ValueError):
            return None

    def save(self, kind, name, data):
        d = os.path.join(self.root, kind)
        try:
            os.makedirs(d, exist_ok=True)
            tmp = os.path.join(d, f"{name}.json.tmp")
            with open(tmp, "w") as fh:
                json.dump(data, fh)
            os.replace(tmp, os.path.join(d, f"{name}.json"))
        except OSError:
            pass  # a cache that cannot be written only costs a re-read


# ----------------------------------------------------------- per-PR facts
def commit_date(c):
    return iso(((c.get("commit") or {}).get("committer") or {}).get("date"))


def guard():
    """The queue-arm guard module, loaded by path: its verdict and approval rules are the ones that count
    approvals everywhere else, so the scorecard reuses them instead of copying them."""
    if not _GUARD:
        spec = importlib.util.spec_from_file_location("ghapp_queue_arm_guard", GUARD_PATH)
        mod = importlib.util.module_from_spec(spec)
        sys.modules.setdefault("ghapp_queue_arm_guard", mod)
        spec.loader.exec_module(mod)
        _GUARD.append(mod)
    return _GUARD[0]


def trust_policy(rd, base):
    """The base's review policy, or None when it cannot be read or does not configure both `reviewer_signers`
    and `reviewer_stamp_sessions`. None means no approval is trusted (fail closed). One read per repo and base."""
    cached = rd.policies.get(base, 0)
    if cached != 0:
        return cached
    pol = None
    try:
        before = rd.errors
        try:
            d = rd.get(f"contents/.shipyard/config.toml?ref={base}")
        finally:
            rd.errors = before  # a repo with no config is an answer, not a read error
        text = base64.b64decode(d.get("content") or "").decode("utf-8")
        g = guard()
        saved = os.environ.get(g.SIGNERS_FROM_BASE_ENV)
        os.environ[g.SIGNERS_FROM_BASE_ENV] = "1"  # the repository-side rule: no host file is read
        try:
            p = g.review_policy(text, rd.repo, False)
        finally:
            if saved is None:
                os.environ.pop(g.SIGNERS_FROM_BASE_ENV, None)
            else:
                os.environ[g.SIGNERS_FROM_BASE_ENV] = saved
        if p.get("signers") and p.get("stamp_sessions"):
            pol = p
    except Truncated:
        raise
    except Exception:  # unreadable or unparsable: no approval is trusted
        pol = None
    rd.policies[base] = pol
    return pol


MERGE_REMOVAL_WINDOW = dt.timedelta(minutes=2)


def ejection(e, merged):
    """Whether a timeline event is a removal from the merge queue that is an ejection. The REST event names no reason, and GitHub
    records a removal when a PR merges, so a removal within two minutes before the merge is the merge, not an ejection (the
    rule the m3 fleet-metrics parser uses); one that says `merged` is the merge too, and a removal after the merge cannot be an
    ejection of the merged PR."""
    if e.get("event") not in ("removed_from_merge_queue", "dequeued") or (e.get("reason") or "unknown") in ("merged", "already_merged"):
        return False
    t, m = iso(e.get("created_at")), iso(merged)
    return not (t and m and m - t <= MERGE_REMOVAL_WINDOW)


def steward_seq(pr, tl):
    """The events that bear on how a PR reached its merge, in the timeline's own order and up to the merge, with no
    dates involved: ["push", sha] (a commit or force-push that moved the head), ["enq", head-then] (an enqueue by the fleet
    App, with the head as the timeline had it at that point, or None), ["remove"] (a queue removal other than the merge),
    ["hold"] and ["approve", body] (an App verdict, as a comment or a review that was not dismissed, for the merged head;
    `approve` keeps its body so it is judged against the base's policy when classified, never when cached)."""
    g = guard()
    head = pr["head"]["sha"]
    cut = next((i for i, e in enumerate(tl) if e.get("event") == "merged"), len(tl))
    tl = tl[:cut]
    dismissed = {(e.get("dismissed_review") or {}).get("review_id") for e in tl if e.get("event") == "review_dismissed"}
    dismissed.discard(None)
    seq, cur = [], None
    for e in tl:
        ev = e.get("event")
        login = ((e.get("actor") or e.get("user") or e.get("author") or {}).get("login") or "").lower()
        if ev in ("committed", "head_ref_force_pushed"):
            cur = e.get("sha") if ev == "committed" else e.get("after_sha")
            seq.append(["push", cur])
        elif ejection(e, pr.get("merged_at")):
            seq.append(["remove"])
        elif ev in ("added_to_merge_queue", "enqueued") and login in APP_LOGINS:
            seq.append(["enq", cur])
        elif ev in ("commented", "reviewed") and login in APP_LOGINS and not (ev == "reviewed" and e.get("id") in dismissed):
            body = e.get("body") or ""
            v = g.marker_verdict(body)
            if v and v[1] == head and v[0] in ("HOLD", "APPROVE"):
                seq.append(["hold"] if v[0] == "HOLD" else ["approve", body])
    return seq


def steward_evidence(rd, f):
    """True, False or None (cannot be bound to the merged SHA). True only when the last thing before the merge was trusted
    steward evidence: an App enqueue made while the timeline's head was the merged SHA, with nothing after it that moves the
    head, removes it from the queue or holds it; or an APPROVE the guard's own rules count (App, configured signer,
    configured reviewer session stamp, read from the base's config) for the merged SHA, with no App HOLD at that SHA at all.
    None: the last event is an enqueue the timeline cannot tie to the merged SHA (no commit event names it), so the merge is
    neither the steward's nor a human's. Evidence followed by a push, a removal or a HOLD is False."""
    g = guard()
    seq = f.get("steward_seq") or []
    state, held, pol = None, any(e[0] == "hold" for e in seq), None
    for e in seq:
        kind = e[0]
        if kind == "enq":
            state = "enq" if e[1] and e[1] == f["head_sha"] else "unbound"
        elif kind == "approve":
            if pol is None:
                pol = trust_policy(rd, f["base"]) or False
            saved = os.environ.get(g.SIGNERS_FROM_BASE_ENV)
            os.environ[g.SIGNERS_FROM_BASE_ENV] = "1"
            try:
                ok = bool(pol) and g.app_approval_refusal(e[1], pol) is None
            finally:
                if saved is None:
                    os.environ.pop(g.SIGNERS_FROM_BASE_ENV, None)
                else:
                    os.environ[g.SIGNERS_FROM_BASE_ENV] = saved
            if ok:
                state = "approve"
        else:  # push, remove, hold
            state = None
    if state == "unbound":
        return None
    return state == "enq" or (state == "approve" and not held)


def reads_complete(f):
    """Whether the timeline and the commit list were read in full: only then are they a settled fact about the PR."""
    return bool(f.get("timeline_complete") and f.get("commits_complete"))


def finalize(rd, f, pr):
    """Facts that depend on inputs that change independently of the cached reduction, recomputed on every run:
    the opener and body (from the PR list), the stamp (body, commits and timeline kept apart), and the steward
    evidence (judged against the base's current policy)."""
    body = pr.get("body") or ""
    f["opener"] = (pr.get("user") or {}).get("login")
    f["provenance"] = bool(PROVENANCE_RE.search(body))
    f["stamp"] = bool(STAMP_RE.search(body)) or bool(f.get("stamp_commit")) or bool(f.get("agent_events"))
    owner_merge = bool(f.get("merged_at")) and any(e[0] == "merged" for e in f["owner_events"])
    verdict = False
    if owner_merge and reads_complete(f):  # a hidden later HOLD, removal or push could cancel the evidence
        try:
            verdict = steward_evidence(rd, f)
        except Truncated:  # the policy could not be read within the cap: no approval is trusted this run
            verdict = False
    f["steward_path"] = verdict is True
    # an owner-account merge the data cannot place as the steward's or a human's: unknown, credited to neither
    f["merge_unverified"] = owner_merge and (not reads_complete(f) or verdict is None)
    return f


def ejection_reasons(rd, n):
    """The reason of each removal from the merge queue of PR `n`, as {created_at: reason}, lowercased (ci_failure,
    merge_conflict, branch_protections, manual, ...). The REST timeline does not carry it; GraphQL does."""
    owner, name = rd.repo.split("/")
    d = rd.gql('query { repository(owner: "%s", name: "%s") { pullRequest(number: %d) { timelineItems(first: 100, '
               'itemTypes: [REMOVED_FROM_MERGE_QUEUE_EVENT]) { nodes { ... on RemovedFromMergeQueueEvent { createdAt reason } } } } } }' % (owner, name, n))
    try:
        nodes = d["data"]["repository"]["pullRequest"]["timelineItems"]["nodes"]
    except (KeyError, TypeError):
        raise ReadError("graphql shape")
    return {x["createdAt"]: (x.get("reason") or "unknown").lower() for x in nodes if x}  # no reason: still unknown, read again


def pr_facts(rd, pr, owner):
    """Timeline and commits of one PR (merged or not), reduced to what the metrics need."""
    n = pr["number"]
    tl, tl_full = rd.pages(f"issues/{n}/timeline", max_pages=5)
    commits, c_full = rd.pages(f"pulls/{n}/commits", max_pages=3)
    created, merged = iso(pr["created_at"]), iso(pr.get("merged_at"))
    # first ready-for-review: opened as a draft means ready at the first ready_for_review event;
    # first lifecycle event a conversion to draft, or none on a non-draft PR, means ready when opened.
    life = sorted((iso(e.get("created_at")), e["event"]) for e in tl
                  if e.get("event") in ("ready_for_review", "converted_to_draft") and e.get("created_at"))
    if life and life[0][1] == "ready_for_review":
        ready = life[0][0]
    elif life:
        ready = created
    else:
        ready = None if pr.get("draft") else created
    owner_events, agent_events, repairs, holds, ejections, enqueues = [], [], [], [], [], 0
    enqueue_times, removal_ts = [], []
    observed_push_times = []
    stamp_commit = False
    for e in tl:
        ev, ts = e.get("event"), e.get("created_at") or e.get("submitted_at")
        body = e.get("body") or ""
        if ev in ("committed", "head_ref_force_pushed") and ts and ready:
            observed = iso(ts)
            if observed and observed > ready:
                observed_push_times.append(fmt(observed))
        if ev in ("commented", "reviewed") and ts:
            if REPAIRED_RE.search(body):
                repairs.append(ts)
            if HOLD_RE.search(body):
                holds.append(ts)
        if ev in ("added_to_merge_queue", "enqueued"):
            enqueues += 1
            if ts:
                enqueue_times.append(ts)
        if ejection(e, pr.get("merged_at")):
            ejections.append(e.get("reason") or "unknown")
            removal_ts.append(ts)
        if ev not in ACTION_EVENTS:
            continue
        who = ((e.get("actor") or e.get("user") or {}).get("login") or "")
        if who.lower() == owner.lower() and ts:
            if STAMP_RE.search(body) or PROVENANCE_RE.search(body):
                # an agent speaking under the owner's account: not an owner action, and a signal of its own
                agent_events.append([ev, ts])
            else:
                owner_events.append([ev, ts])
    causes_known, pairs = True, list(zip(removal_ts, ejections))
    if any(r == "unknown" for r in ejections):
        # the REST timeline names no cause; ask GraphQL once for this PR, and keep "unknown" (and read again next run) if it fails
        try:
            by_ts = ejection_reasons(rd, n)
            pairs = [(t, by_ts.get(t, r) if r == "unknown" else r) for t, r in pairs]
            pairs = [(t, r) for t, r in pairs if r not in ("merged", "already_merged")]  # GraphQL can name a merge REST could not
            causes_known = all(r != "unknown" for _, r in pairs)
        except ReadError:
            causes_known = False
    ejections = [r for _, r in pairs]
    dequeue_times = [t for t, _ in pairs if t]
    heads = {pr["head"]["sha"]}
    for e in tl:
        if e.get("event") == "head_ref_force_pushed":
            heads.update(x for x in (e.get("before_sha"), e.get("after_sha")) if x)
    ambiguous, pushes, trailer = [], {}, False
    for c in commits:
        heads.add(c["sha"])
        d = commit_date(c)
        msg = (c.get("commit") or {}).get("message") or ""
        if AGENT_TRAILER_RE.search(msg):
            trailer = True
        if STAMP_RE.search(msg):
            stamp_commit = True
        logins = {((c.get("author") or {}).get("login") or "").lower(), ((c.get("committer") or {}).get("login") or "").lower()}
        if owner.lower() in logins and not AGENT_TRAILER_RE.search(msg):
            ambiguous.append(c["sha"])  # by identity, never by date
        if ready and d and d > ready:  # a push after the first ready moment, one per distinct commit minute
            pushes[fmt(d)[:16]] = (MERGE_MAIN_RE.search(msg.splitlines()[0] if msg else "") is not None, d)
    split = {"repair": 0, "author": 0, "merge_main": 0}
    pivot = min([iso(x) for x in holds + repairs], default=None)
    for is_merge, d in pushes.values():
        split["merge_main" if is_merge else "repair" if pivot and d > pivot else "author"] += 1
    hs = sorted(iso(x) for x in holds)
    rep = sorted(iso(x) for x in repairs)
    pushd = sorted(d for _, d in pushes.values())
    re_holds = sum(1 for i in range(1, len(hs)) if any(hs[i - 1] < x < hs[i] for x in rep + pushd))
    size = None
    if holds:  # the size bucket is only needed for re-HOLD rates, so only PRs with a HOLD cost a detail read
        d = rd.get(f"pulls/{n}")
        size = (d.get("additions") or 0) + (d.get("deletions") or 0)
    heads.discard(None)
    return {
        "v": FACTS_VERSION, "number": n, "base": pr["base"]["ref"], "created_at": fmt(created),
        "merged_at": fmt(merged) if merged else None, "state": pr.get("state"),
        "merge_commit_sha": pr.get("merge_commit_sha"), "first_ready": fmt(ready) if ready else None,
        "owner_events": owner_events, "agent_events": agent_events, "steward_seq": steward_seq(pr, tl), "head_sha": pr["head"]["sha"],
        "opener": (pr.get("user") or {}).get("login"), "provenance": bool(PROVENANCE_RE.search(pr.get("body") or "")),
        "agent_trailer": trailer, "stamp_commit": stamp_commit, "ambiguous_commits": sorted(set(ambiguous)),
        "repair_cycles": len(repairs), "holds": len(holds), "re_holds": re_holds, "size": size,
        "pushes": split, "enqueues": enqueues, "enqueue_times": enqueue_times, "dequeue_times": dequeue_times,
        "ejection_causes_known": causes_known, "hold_times": sorted(holds), "push_times": sorted(set(observed_push_times)), "ejections": ejections,
        "labels": sorted(l["name"] for l in pr.get("labels") or []),
        "heads": sorted(heads), "timeline_complete": tl_full, "commits_complete": c_full,
    }


def eligible(f, managed_base):
    """(True, None) or (False, reason). Out: never ready (still a draft), a base that is not the managed
    base or an integration base, or a label from EXCLUDE_LABELS."""
    if not f.get("first_ready"):
        return False, "never_ready"
    for lab in f.get("labels") or []:
        if lab in EXCLUDE_LABELS:
            return False, EXCLUDE_LABELS[lab]
    b = f["base"]
    if b != managed_base and not b.startswith(INTEGRATION_BASES):
        return False, "unmanaged_base"
    return True, None


def attribution(f):
    """The agent-authorship signals a PR carries, strongest first: opened by the fleet App, `shipyard pr`
    provenance in the body, an agent Co-Authored-By (or Version-Bump style) trailer, an agent session stamp.
    `primary` is the first that applies; every one that applies is listed so the attribution can be audited."""
    have = {"app_opener": (f.get("opener") or "").lower() in APP_OPENERS, "provenance": bool(f.get("provenance")),
            "trailer": bool(f.get("agent_trailer")), "session_stamp": bool(f.get("stamp"))}
    sig = [k for k in SIGNALS if have[k]]
    return {"signals": sig, "primary": sig[0] if sig else None}


def effective_owner_events(f, attributed=True):
    """The owner events that are a human's. Without attribution every owner-account event counts.
    With it, a merge by the owner account is a steward merge only with trusted evidence (see
    trusted_steward_evidence): in user-owned repos the steward merges under the owner account, so the
    account alone cannot tell it from a human, and no PR-level signal stands in for the evidence.
    Returns (events, steward_merges_dropped)."""
    ev = f["owner_events"]
    if not attributed:  # the earlier rule: every owner-account event, stamped ones included
        return sorted(ev + list(f.get("agent_events") or []), key=lambda e: iso(e[1])), 0
    a = attribution(f)
    steward = bool(f.get("steward_path"))
    keep = [e for e in ev if not (e[0] == "merged" and steward)]
    return keep, len(ev) - len(keep)


def north_star(f, commits=True, attributed=True):
    """autonomous, owner_action, unknown or not_merged for one ready PR, over the 48h horizon.

    autonomous: merged within 48h of ready with no human owner event between ready and merge.
    owner_action: a human owner-account timeline event in that interval (see effective_owner_events).
    unknown: merged in time with no such event, but an owner-identity commit with no agent trailer and
        no agent signal on the PR at all (commits=True), or a timeline or commit list not read in full.
    not_merged: not merged within 48h and no owner event.
    commits=False ignores commits: only GitHub-side owner actions count.
    attributed=False is the earlier rule (every owner-account event is an owner action, owner-identity
    commits make a PR unknown), kept so a change can be shown against it."""
    r = iso(f["first_ready"])
    h = r + HORIZON
    m = iso(f.get("merged_at"))
    end = min(m, h) if m else h
    events, _ = effective_owner_events(f, attributed)
    inside = [e for e in events if r <= iso(e[1]) <= end]
    if inside:
        if attributed and f.get("merge_unverified") and all(e[0] == "merged" for e in inside):
            # an owner-account merge that could have been the steward's: with a partial read, or an enqueue the
            # timeline cannot tie to the merged SHA, there is no telling, so it is unknown rather than a claimed human action
            return "unknown"
        return "owner_action"
    if not m or m > h:
        return "not_merged"
    ambiguous = bool(f["ambiguous_commits"])  # every commit of a merged PR precedes its merge: no date decides this
    if attributed and attribution(f)["primary"]:
        ambiguous = False
    if (commits and ambiguous) or not (f["timeline_complete"] and f["commits_complete"]):
        return "unknown"
    return "autonomous"


def row_attribution(f):
    a = attribution(f)
    _, stew = effective_owner_events(f)
    return {"signals": a["signals"], "primary": a["primary"], "steward_merge_recognised": bool(stew),
            "agent_owner_events": len(f.get("agent_events") or []),
            "owner_identity_commits": bool(f["ambiguous_commits"]), "merge_unverified": bool(f.get("merge_unverified")),
            "legacy_north": north_star(f, attributed=False)}


# --------------------------------------------------------------- compute
def job_minutes(rd, run):
    """(self_hosted_min, hosted_min, macos_min, runner_wait_min, complete) for one run, read once ever when it has completed.
    Every non-skipped job must have created, started and completed timestamps; malformed timing leaves the run incomplete
    and is not cached. macos_min is the part of the minutes on jobs whose labels name macOS, whoever hosts them.
    runner_wait_min is the time its jobs sat queued for a runner (`started_at` minus `created_at`)."""
    rid = run["id"]
    attempt = int(run.get("run_attempt") or 1)
    cached = rd.load("jobs", str(rid))
    # an entry is for one attempt of one run: a rerun keeps the run id, so a different attempt is read again
    if (cached and "macos" in cached and "wait" in cached and cached.get("attempt") == attempt
            and cached.get("timing_complete") is True):
        return cached["self_hosted"], cached["hosted"], cached["macos"], cached["wait"], True
    jobs, full = rd.pages(f"actions/runs/{rid}/jobs?filter=all", key="jobs", max_pages=3)
    sh = ho = mac = wait = 0.0
    timing_complete = True
    for j in jobs:
        a, b = iso(j.get("started_at")), iso(j.get("completed_at"))
        if j.get("conclusion") == "skipped":
            continue
        q = iso(j.get("created_at"))
        if not a or not b or not q:
            timing_complete = False
            continue
        wait += max(0.0, (a - q).total_seconds() / 60.0)
        m = max(0.0, (b - a).total_seconds() / 60.0)
        if "self-hosted" in (j.get("labels") or []):
            sh += m
        else:
            ho += m
        if any("macos" in str(l).lower() for l in j.get("labels") or []):
            mac += m
    done = run.get("status") == "completed" and full and timing_complete
    if not done:
        return None, None, None, None, False
    rd.save("jobs", str(rid), {"self_hosted": round(sh, 2), "hosted": round(ho, 2), "macos": round(mac, 2), "wait": round(wait, 2), "attempt": attempt,
                                "timing_complete": True})
    return round(sh, 2), round(ho, 2), round(mac, 2), round(wait, 2), True


RUNS_SLICE_LIMIT = 900  # the Actions API serves at most 1000 runs for one `created` range: a busier range is split in halves


def runs_index(rd, lo, hi):
    """Runs created in [lo, hi]: (list, complete). A range with more than RUNS_SLICE_LIMIT runs is split in
    halves until each part fits, so a busy repo is read in full instead of cut at 1000. One page read per 100 runs,
    shared by every PR in the window. A part that cannot be split further (one second) and still overflows is incomplete."""
    runs, complete = {}, [True]

    def part(a, b):
        d = rd.get(f"actions/runs?created={fmt(a)}..{fmt(b)}&per_page=100&page=1")
        items = d.get("workflow_runs", []) if isinstance(d, dict) else None
        if not isinstance(items, list):
            raise ReadError("unexpected shape")
        total = d.get("total_count", len(items))
        if total > RUNS_SLICE_LIMIT and (b - a).total_seconds() > 1:
            mid = a + (b - a) / 2
            mid = mid.replace(microsecond=0)
            part(a, mid)
            part(mid + dt.timedelta(seconds=1), b)
            return
        pages = [items]
        for n in range(2, min(10, math.ceil(total / 100)) + 1):
            more = rd.get(f"actions/runs?created={fmt(a)}..{fmt(b)}&per_page=100&page={n}").get("workflow_runs", [])
            pages.append(more)
        for page in pages:
            for r in page:
                runs[r["id"]] = r
        if total > RUNS_SLICE_LIMIT:
            complete[0] = False  # one second holds more runs than the API serves

    part(lo.replace(microsecond=0), hi.replace(microsecond=0))
    return list(runs.values()), complete[0]


def push_repeats_of_group_shas(runs):
    """(push runs, push runs that repeat a merge_group run of the same workflow on the same sha).
    The queue fast-forwards the base to the group sha, so such a push re-runs what the group already proved."""
    group = {(r.get("name"), r.get("head_sha")) for r in runs if r.get("event") == "merge_group"}
    push = [r for r in runs if r.get("event") == "push"]
    return len(push), sum(1 for r in push if (r.get("name"), r.get("head_sha")) in group)


CAUSES = ("hold", "red", "queue", "runner", "dirty")


def _wait_until_next(starts, nexts, merged):
    """Minutes from each start to the next `nexts` event after it, or to the merge when none follows."""
    nexts = sorted(iso(x) for x in nexts if iso(x))
    total = 0.0
    for st in sorted(iso(x) for x in starts if iso(x)):
        if st > merged:
            continue
        end = next((n for n in nexts if n > st), None)
        total += ((min(end, merged) if end else merged) - st).total_seconds() / 60.0
    return round(total, 1)


def outcomes_known(comp):
    """True when every run's outcome and the head list were read, whether or not the job timings were. Entries cached
    before `outcomes_complete` existed were only saved complete, so they fall back to `complete`."""
    return bool(comp.get("outcomes_complete", comp.get("complete")))


def path_of(f, comp):
    """clean, slow or None (unknown) for a merged PR, and why. Slow: any HOLD, red run on a head, ejection from the
    queue, or DIRTY. A red run is only known from a read of the run list and heads in full, so a PR with no other cause
    and an unread compute is unknown, not clean."""
    why = []
    if f["holds"]:
        why.append("hold")
    if comp and outcomes_known(comp) and comp.get("red_events"):  # a red run is an outcome: job timing, a jobs read or another run's rerun history cannot hide it
        why.append("red")
    if f["ejections"]:
        why.append("ejection")
    if f.get("dirty"):
        why.append("dirty")
    if why:
        return "slow", why
    return ("clean", []) if comp and comp.get("complete") else (None, [])


def cause_minutes(f, comp):
    """Minutes each cause held a merged PR up, or None where it is not measured. The causes overlap (a HOLD and a red run
    can wait at once), so they are not additive. hold: from each HOLD to the next push; red: from a failed head run to the
    next push; queue: from each enqueue to its removal or the merge; runner: queued job time; dirty: not measured."""
    m = iso(f.get("merged_at"))
    if not m or not reads_complete(f):  # a partial timeline or commit list omits holds, pushes and queue events: no number, not an undercount
        return None
    done = comp and comp.get("complete")
    outcomes = comp and outcomes_known(comp)
    push_times = f.get("push_times") or []
    hold_times = f.get("hold_times") or []
    red_events = comp.get("red_events") or [] if comp else []
    hold = None if hold_times and not push_times else _wait_until_next(hold_times, push_times, m)
    red = None if red_events and not push_times else _wait_until_next(red_events, push_times, m)
    return {"hold": hold,
            "red": None if comp and comp.get("red_partial") else red if outcomes else None,  # None: an earlier failed attempt is not in the list
            "queue": _wait_until_next(f.get("enqueue_times") or [], f.get("dequeue_times") or [], m),
            "runner": comp.get("runner_wait_min") if done else None, "dirty": None}  # None when the jobs were not read


def enqueue_to_merge(f):
    """Minutes from the last merge-queue enqueue before the merge to `merged_at`, or None (no enqueue, not merged)."""
    m = iso(f.get("merged_at"))
    ts = [iso(x) for x in f.get("enqueue_times") or []]
    ts = [x for x in ts if x and m and x <= m]
    return round((m - max(ts)).total_seconds() / 60.0, 1) if ts and m else None


def run_fingerprint(r):
    return [int(r.get("run_attempt") or 1), r.get("updated_at")]


def load_compute(rd, number, f=None, runs_by_id=None):
    """A cached compute reduction, or None when absent or written before COMPUTE_VERSION (recomputed). With `f`, an
    entry is never complete while that PR's own timeline or commit read is incomplete (the heads came from them). With
    `runs_by_id` (the current run list) every run the entry was built from must still have the attempt and update time
    it had: a rerun keeps its run id and can turn a cached success into a failure. Without the run list a cached entry
    cannot be validated (the run list was not read, for example under the read cap): it is used as it stands, since the
    alternative is to lose a settled result.  A run that appeared since the entry was written is a change too."""
    if not READ_JOB_MINUTES:  # a cached entry carries job minutes this run is meant to leave empty
        return None
    c = rd.load("compute", str(number))
    if c is None or c.get("v") != COMPUTE_VERSION or not c.get("complete"):
        return None
    if f is not None and not reads_complete(f):
        return None
    if runs_by_id is not None and f is not None:
        stored = c.get("run_fp") or {}
        if {str(i) for i in pr_runs(f, list(runs_by_id.values()))} != set(stored):
            return None  # a run appeared (or left the list) since the entry was written
        for rid, fp in stored.items():
            if run_fingerprint(runs_by_id[int(rid)]) != fp:
                return None
    return c


READ_JOB_MINUTES = True  # False skips the per-run jobs reads (the bulk of the reads): runs, failed head runs and paths still work


def pr_runs(f, runs):
    """The runs that belong to a merged PR: those on one of its heads, its merge-group runs, and pushes to the base on a
    merge-group sha (the queue fast-forwards the base to the group)."""
    heads = set(f["heads"])
    mg = re.compile(rf"^gh-readonly-queue/{re.escape(f['base'])}/pr-{f['number']}-")
    mine, group_shas = {}, set()
    for r in runs:
        if r.get("head_sha") in heads or (r.get("event") == "merge_group" and mg.match(r.get("head_branch") or "")):
            mine[r["id"]] = r
            if r.get("event") == "merge_group":
                group_shas.add(r.get("head_sha"))
    for r in runs:
        if r["id"] not in mine and r.get("event") == "push" and r.get("head_sha") in group_shas:
            mine[r["id"]] = r
    return mine


def pr_compute(rd, f, runs, runs_complete):
    heads = set(f["heads"])
    mine = pr_runs(f, runs)
    # a repeat is a run of a workflow on a sha that workflow already ran on (reruns, a push on a merge-group sha)
    seen, repeats = {}, 0
    for r in sorted(mine.values(), key=lambda x: (x.get("event") != "merge_group", x["id"])):
        k = (r.get("name"), r.get("head_sha"))
        repeats += k in seen
        seen[k] = True
    sh = ho = 0.0
    complete = runs_complete and reads_complete(f)  # the heads come from the timeline and the commits
    outcomes_complete = complete  # as `complete`, but job timing and job reads do not count: run outcomes are in the run list
    mac = wait = 0.0
    timing_complete = True
    by_wf, red_events, red_partial = {}, [], False
    for r in mine.values():
        # skipping the jobs reads must not hide an unfinished run: its outcome is still unknown, so the PR is not clean
        try:
            a, b, m, w, ok = job_minutes(rd, r) if READ_JOB_MINUTES else (0.0, 0.0, 0.0, 0.0, r.get("status") == "completed")
        except ReadError:  # a failed jobs read withholds this run's minutes; its outcome is in the run list
            a = b = m = w = 0.0
            ok = False
        if ok:
            sh += a
            ho += b
            mac += m
            wait += w
        else:
            timing_complete = False
        # The Actions run list exposes only the latest conclusion for a rerun. A successful latest attempt
        # does not establish that an earlier attempt was also successful, so leave the path unknown unless
        # the latest attempt itself is red (which is independently sufficient to classify the PR as slow).
        try:
            attempt = int(r.get("run_attempt") or 1)
        except (TypeError, ValueError):
            attempt = 1
        attempts_known = attempt <= 1 or r.get("conclusion") in BAD_CONCLUSIONS
        complete = complete and ok and attempts_known
        outcomes_complete = outcomes_complete and r.get("status") == "completed"  # a rerun's history only blocks a clean verdict (`complete`)
        if (r.get("head_sha") in heads and r.get("event") != "merge_group"
                and r.get("conclusion") in BAD_CONCLUSIONS and r.get("updated_at")):
            red_events.append(r["updated_at"])  # any failed non-merge-group run on one of the PR's heads
            red_partial = red_partial or attempt > 1  # the list shows only its latest attempt: earlier failed attempts are not in red_events
        level = by_wf.setdefault(r.get("name") or "?", {"pull_request": 0, "merge_group": 0, "push": 0, "other": 0})
        ev = r.get("event")
        level[ev if ev in ("merge_group", "push") else "pull_request" if ev in ("pull_request", "pull_request_target") else "other"] += 1
    j = READ_JOB_MINUTES  # job-derived numbers are None, never a measured zero, when jobs were not read or timing was incomplete
    measured = j and timing_complete
    return {"self_hosted_min": round(sh, 1) if measured else None, "hosted_min": round(ho, 1) if measured else None, "macos_min": round(mac, 1) if measured else None,
            "runner_wait_min": round(wait, 1) if measured else None, "jobs_read": j, "red_events": sorted(red_events), "by_workflow": by_wf,
            "runs": len(mine), "repeat_runs": repeats, "red_partial": red_partial, "run_fp": {str(i): run_fingerprint(r) for i, r in mine.items()},
            "group_runs": sum(1 for r in mine.values() if r.get("event") == "merge_group"), "complete": complete,
            "outcomes_complete": outcomes_complete}


# ----------------------------------------------------------- correctness
def strip_app(ctx):
    return re.sub(r"\s*\(app_id=\d+\)$", "", ctx)


def required_contexts(rd, base, tick_ctx):
    if tick_ctx:
        return {strip_app(c) for c in tick_ctx}
    try:
        rules = rd.get(f"rules/branches/{base}")
    except ReadError:
        return None
    out = set()
    for r in rules if isinstance(rules, list) else []:
        if r.get("type") == "required_status_checks":
            out |= {c["context"] for c in (r.get("parameters") or {}).get("required_status_checks", [])}
    return out or None


def run_order(c):
    """Newest run of a context: the highest id (a queued rerun has no start time yet but a newer id), then the start."""
    return (c.get("id") or 0, c.get("started_at") or "")


def commit_red(rd, sha, required):
    """(red_contexts, settled). Settled and cached only when every page was read and every check run completed."""
    cached = rd.load("checks", sha)
    if cached is not None and cached.get("v") == CHECKS_VERSION:
        return set(cached["red"]) & required, True
    runs, complete = rd.pages(f"commits/{sha}/check-runs", key="check_runs", max_pages=2)
    latest = {}
    for c in runs:
        k = c["name"]
        if k not in latest or run_order(c) > run_order(latest[k]):
            latest[k] = c
    settled = complete and bool(latest) and all(c.get("status") == "completed" for c in latest.values())
    red = {k for k, c in latest.items() if c.get("conclusion") in BAD_CONCLUSIONS}
    if settled:
        rd.save("checks", sha, {"v": CHECKS_VERSION, "red": sorted(red)})
    return red & required, settled


CACHE_AFTER = dt.timedelta(days=1)  # a push's state is cached once it is a day old: a rerun or late workflow comes sooner
STUCK_AFTER = dt.timedelta(hours=3)  # a run still queued or running this long after its push is stuck, not pending


def commit_state(rd, sha, age=None):
    """The check state of one commit: {"red": contexts failing, "green": contexts succeeding, "settled": every page read and
    every run finished}. A context's latest run (by start) decides it. A run that has not finished `STUCK_AFTER` after the
    push (`age`) and `STUCK_AFTER` after its own start is stuck (GitHub keeps phantom queued runs for days) and says
    nothing: it is neither red nor green and does not hold the commit open; a rerun queued minutes ago on an old push is
    pending. Cached only when every run finished and the push is a day old: a stuck run can still finish, and a rerun or
    a late workflow can still appear.""" 
    cached = rd.load("cstate", sha)
    if cached is not None and cached.get("v") == CHECKS_VERSION:
        return cached
    runs, complete = rd.pages(f"commits/{sha}/check-runs", key="check_runs", max_pages=5)
    latest = {}
    for c in runs:
        k = c["name"]
        if k not in latest or run_order(c) > run_order(latest[k]):
            latest[k] = c
    now = dt.datetime.now(UTC)

    def done_or_stuck(c):
        if c.get("status") == "completed":
            return True
        if age is None or age < STUCK_AFTER:
            return False
        return bool(c.get("started_at")) and now - iso(c["started_at"]) >= STUCK_AFTER  # no start time yet: pending

    st = {"v": CHECKS_VERSION, "settled": bool(complete and latest and all(done_or_stuck(c) for c in latest.values())),
          "red": sorted(k for k, c in latest.items() if c.get("status") == "completed" and c.get("conclusion") in BAD_CONCLUSIONS),
          "green": sorted(k for k, c in latest.items() if c.get("status") == "completed" and c.get("conclusion") == "success")}
    if st["settled"] and all(c.get("status") == "completed" for c in latest.values()) and age is not None and age >= CACHE_AFTER:
        rd.save("cstate", sha, st)
    return st


# `since` filters the base listing by commit date, and a merge queue pushes merge commits written while the group ran
# (up to its check timeout, 6 h on Vellum), so the listing starts a day early: that also gives the first PR in the
# window a settled push before it to compare against.
BASE_LOOKBACK = dt.timedelta(days=1)
# The listing of a merge-commit base also returns every merged branch's own commits: Pulp's main lists about 2,900
# commits over 8 days (153 of Shipyard's 651 are base pushes). A listing cut short reads every PR as unknown.
BASE_LISTING_PAGES = 40
NOISY_RED_STARTS = 0.1  # a context that turns red on this share of its pushes or more is a gate that flaps, not a check a merge breaks
NOISY_MIN_PUSHES = 20  # ... judged only over at least this many pushes, so a short window cannot call a real break noise


def untracked_contexts(chron, states):
    """Contexts the detector does not follow: `one_off` (seen on a single push that is not the newest settled one, a
    per-version or per-event name that can never turn green again; a context new on the newest push is kept until a
    later push shows whether it recurs) and `noisy` (turns red, from green or absent, on NOISY_RED_STARTS or more of the pushes it
    appears on: review-verdict and the reviewer judge are PR gates that flap on a merge-queue base). It counts turns,
    not red pushes, so one sustained break that stays red for many pushes is still followed."""
    seen, starts, was_red, last = collections.Counter(), collections.Counter(), set(), set()
    for sha, _ in chron:
        st = states.get(sha)
        if st and st.get("settled"):
            last = set(st["red"]) | set(st["green"])
            for c in last:
                seen[c] += 1
            red = set(st["red"])
            for c in red - was_red:
                starts[c] += 1
            was_red = red
    return {"one_off": sorted(c for c, n in seen.items() if n <= 1 and c not in last),
            "noisy": sorted(c for c, n in seen.items() if n >= NOISY_MIN_PUSHES and starts[c] / n >= NOISY_RED_STARTS)}


def main_red_episodes(chron, states, pr_by_sha):
    """Main-red episodes from the base's pushes, oldest first. `chron`: [(sha, ts)]; `states`: sha -> commit_state (or None
    when unread); `pr_by_sha`: merge commit sha -> PR number. An episode starts at the first commit where a context is
    red that was not red before, is attributed to the PR whose merge commit that is (None for a direct push or when the
    first usable commit is already red: it began before the window), and ends at the commit where its last context
    turns green; that commit's PR is the fixer. A commit with unsettled checks says nothing and is skipped, and the
    next settled commit after it has no baseline: an episode that starts there has no PR (`after_gap`), since the
    skipped push may have turned the context red. Returns (episodes, skipped)."""
    open_ctx, episodes, skipped, first, prev_settled = {}, [], 0, True, False
    untracked = untracked_contexts(chron, states)
    skip = set(untracked["one_off"]) | set(untracked["noisy"])
    for sha, ts in chron:
        st = states.get(sha)
        if not st or not st.get("settled"):
            skipped += 1
            prev_settled = False
            # the base's state across an unread push is unknown: an open episode ends there, with no fixer and no
            # duration, and a red seen after it is a new episode with no PR
            for ep in {id(e): e for e in open_ctx.values()}.values():
                ep["open"], ep["end_unknown"] = set(), True
            open_ctx = {}
            continue
        red, green = set(st["red"]) - skip, set(st["green"]) - skip
        new = sorted(c for c in red if c not in open_ctx)
        if new:
            ep = {"start_sha": sha, "start": ts, "pr": pr_by_sha.get(sha) if prev_settled else None, "began_before_window": first,
                  "after_gap": not first and not prev_settled, "contexts": new, "open": set(new), "end": None, "fixer_pr": None, "minutes_to_green": None}
            episodes.append(ep)
            for c in new:
                open_ctx[c] = ep
        for c in sorted(set(open_ctx) & green):
            ep = open_ctx.pop(c)
            ep["open"].discard(c)
            if not ep["open"]:
                ep["end"], ep["fixer_pr"], ep["fixer_sha"] = ts, pr_by_sha.get(sha), sha
                ep["minutes_to_green"] = round((iso(ts) - iso(ep["start"])).total_seconds() / 60.0, 1)
        first, prev_settled = False, True
    for ep in episodes:
        ep["open"] = sorted(ep["open"])
    return episodes, skipped


def head_states_at(rd, sha, at):
    """Each context's verdict on a PR head when it merged: the newest run started by `at` decides it, "red" or "green" if
    it had finished by then, "pending" if not. A rerun after the merge does not rewrite history. None when the runs could
    not be read in full."""
    key = f"{sha}@{at}"
    cached = rd.load("headstate", key)
    if cached is not None and cached.get("v") == CHECKS_VERSION:
        return cached["states"]
    # filter=all: the default (latest) drops a run superseded by a rerun after the merge
    runs, complete = rd.pages(f"commits/{sha}/check-runs?filter=all", key="check_runs", max_pages=5)
    if not complete:
        return None
    latest, unstarted = {}, set()
    for c in runs:
        # a run with no start time yet may have been queued before the merge or after it: its context is unknown
        if not c.get("started_at") and c.get("status") != "completed":
            unstarted.add(c["name"])
        elif c.get("started_at") and c["started_at"] <= at and (c["name"] not in latest or run_order(c) > run_order(latest[c["name"]])):
            latest[c["name"]] = c
    states = {}
    for k, c in latest.items():
        done_at = c.get("completed_at") or c.get("started_at")  # the API always sets completed_at on a completed run
        if c.get("status") != "completed" or (done_at or "") > at:
            states[k] = "pending"
        else:
            states[k] = "red" if c.get("conclusion") in BAD_CONCLUSIONS else "green" if c.get("conclusion") == "success" else "pending"
    for k in unstarted:
        states[k] = "pending"
    if not unstarted:  # an unstarted run gets its start time later, which can settle the question
        rd.save("headstate", key, {"v": CHECKS_VERSION, "states": states})
    return states


def head_red_same_context(states, contexts):
    """True when the head was red in any of the episode's contexts at merge, False only when it was green in all of
    them, None when any is pending, absent or unread."""
    if states is None:
        return None
    vals = [states.get(c) for c in contexts]
    if "red" in vals:
        return True
    return False if all(v == "green" for v in vals) else None


def attributable_pushes(chron, states):
    """The pushes whose effect on the base is known: settled, with a settled push right before them. A PR whose merge
    commit is not one of these (absent from the listing, unsettled, first in the window, or after a skipped push) has
    an unknown main-red attribution, never a known zero."""
    out, prev = set(), False
    for sha, _ in chron:
        st = states.get(sha)
        ok = bool(st and st.get("settled"))
        if ok and prev:
            out.add(sha)
        prev = ok
    return out


def commit_date_str(c):
    return ((c.get("commit") or {}).get("committer") or {}).get("date") or ""


def base_order(commits, anchors=None):
    """The base's pushes, oldest first. A merge queue fast-forwards the base, so each commit keeps its own (older) date
    and dates mis-order pushes: follow first-parent links when every commit carries them, else fall back to commit date
    (test fixtures and API answers without parents). Listing a base that takes merge commits also returns the commits
    of every merged branch (a PR's own commits and its merges of main), which never were base pushes and carry the PR's
    own check runs: only a first-parent chain is the base. With `anchors=None` the listing reaches the base's tip, so
    its one tip is followed (more than one tip falls back to commit date). A listing cut off by `until` can hold a
    feature commit whose merge fell outside it, so its tip is not trusted: `anchors` (merged PRs' merge commits, known
    base pushes, oldest first) give the chain, walked back from the newest one listed."""
    by = {c["sha"]: c for c in commits}
    if commits and all(c.get("parents") is not None for c in commits):
        if anchors is None:
            referenced = {p["sha"] for c in commits for p in c["parents"]}
            tips = [c["sha"] for c in commits if c["sha"] not in referenced]
            start = tips[0] if len(tips) == 1 else None
        else:
            start = next((a for a in reversed(list(anchors)) if a in by), None)
        if start is not None:
            order = [start]
            while by[order[-1]]["parents"] and by[order[-1]]["parents"][0]["sha"] in by:
                order.append(by[order[-1]]["parents"][0]["sha"])
            return [by[x] for x in reversed(order)]
    return sorted(commits, key=commit_date_str)


def detect_main_red(rd, base_commits, merged_list, now=None, listing_cut=False):
    """Read every base push's checks (the cap raises Truncated, an unreadable page ReadError), find the episodes and,
    for each one a PR started, whether that PR's final head was red in the same context when it merged."""
    by_sha = {p["merge_commit_sha"]: p for p in merged_list if p.get("merge_commit_sha")}
    # a PR's push time is its merged_at; a commit's own date is when it was written, not when it reached the base
    chron, floor = [], ""
    anchors = [p["merge_commit_sha"] for p in sorted(by_sha.values(), key=lambda p: p["merged_at"])] if listing_cut else None
    for c in base_order(base_commits, anchors):
        t = by_sha[c["sha"]]["merged_at"] if c["sha"] in by_sha else commit_date_str(c)
        floor = max(floor, t)  # a push cannot precede its parent's: a commit written earlier than the one before it still arrived after it
        chron.append((c["sha"], floor))
    now = now or dt.datetime.now(UTC)
    states = {sha: commit_state(rd, sha, now - iso(ts)) for sha, ts in chron}
    episodes, skipped = main_red_episodes(chron, states, {k: p["number"] for k, p in by_sha.items()})
    for ep in episodes:
        p = by_sha.get(ep["start_sha"])
        ep["head_red_same_context"] = None
        if p and ep["pr"]:
            ep["head_red_same_context"] = head_red_same_context(head_states_at(rd, p["head"]["sha"], p["merged_at"]), ep["contexts"])
    return episodes, skipped, untracked_contexts(chron, states), attributable_pushes(chron, states)


def revert_of(commits, f):
    """A commit in the 7 days after the merge whose message starts `Revert` and names the merge sha or PR."""
    merged = iso(f["merged_at"])
    sha = (f.get("merge_commit_sha") or "")
    pat = re.compile(rf"(?<![\w#]){re.escape('#' + str(f['number']))}(?!\d)")
    for c in commits:
        t = iso(((c.get("commit") or {}).get("committer") or {}).get("date"))
        msg = (c.get("commit") or {}).get("message") or ""
        if not t or not (merged <= t <= merged + WINDOW) or c["sha"] == sha:
            continue
        if not msg.lstrip().lower().startswith("revert"):
            continue
        if (sha and sha[:7] in msg) or pat.search(msg):
            return c["sha"]
    return None


def poison_hit(ticks, repo, merged, until):
    """True when a tick of `repo` reports a base_poison red-everywhere episode that began in [merged, until]."""
    for began in ticks.get(repo, []):
        if merged <= began <= until:
            return True
    return False


def escaped_by(later, f):
    """Best effort: the number of a later PR, merged within 14 days after this one, whose title or body
    says it fixes or follows from this PR (`fix ... #N`, `regression from #N`). None when unreadable."""
    rx = re.compile(ESCAPE_RE % f["number"])
    merged = iso(f["merged_at"])
    for p in later:
        m = iso(p.get("merged_at"))
        if p["number"] == f["number"] or not m or not (merged < m <= merged + ESCAPE_WINDOW):
            continue
        if rx.search(f"{p.get('title') or ''}\n{p.get('body') or ''}"):
            return p["number"]
    return None


def pr_correct(rd, f, base_commits, commits_ok, required, ticks, now, later=None, later_ok=False, anchors=None):
    merged = iso(f["merged_at"])
    provisional = now < merged + WINDOW
    out = {"provisional": provisional, "reverted": None, "base_red": None, "bad": None,
           "escape_provisional": now < merged + ESCAPE_WINDOW, "escaped": None, "escaped_by": None}
    if later_ok:
        out["escaped_by"] = escaped_by(later or [], f)
        out["escaped"] = out["escaped_by"] is not None
    if commits_ok:
        out["reverted"] = revert_of(base_commits, f) is not None
    # base red: the merge commit and the next push to the base
    nxt = None
    if commits_ok and f.get("merge_commit_sha"):
        chron = base_order(base_commits, anchors)
        shas = [c["sha"] for c in chron]
        if f["merge_commit_sha"] in shas and shas.index(f["merge_commit_sha"]) + 1 < len(shas):
            nxt = chron[shas.index(f["merge_commit_sha"]) + 1]
    until = merged + dt.timedelta(hours=2)
    if nxt:
        until = max(until, iso(nxt["commit"]["committer"]["date"]))
    if poison_hit(ticks, rd.repo, merged, until):
        out["base_red"] = True
        out["base_red_source"] = "ticks"
    elif required and f.get("merge_commit_sha"):
        red, settled = commit_red(rd, f["merge_commit_sha"], required)
        if nxt and not red:
            red2, settled2 = commit_red(rd, nxt["sha"], required)
            red, settled = red | red2, settled and settled2
        if red:
            out["base_red"], out["base_red_source"] = True, "check_runs"
        elif settled:
            out["base_red"], out["base_red_source"] = False, "check_runs"
    if out["reverted"] or out["base_red"]:
        out["bad"] = True
    elif out["reverted"] is False and out["base_red"] is False:
        out["bad"] = False
    return out


# -------------------------------------------------------------- aggregate
def wilson(k, n, z=1.96):
    """95% Wilson score interval for k of n, in percent, or None when n is 0."""
    if not n:
        return None
    p = k / n
    d = 1 + z * z / n
    c = (p + z * z / (2 * n)) / d
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return [round(100 * max(0.0, c - h), 1), round(100 * min(1.0, c + h), 1)]


def pct_of(a, b):
    return None if not b else round(100.0 * a / b, 1)


def pctile(xs, p):
    xs = sorted(xs)
    return None if not xs else round(xs[max(0, math.ceil(p * len(xs)) - 1)], 1)


def med(xs):
    return None if not xs else round(statistics.median(xs), 1)


def bucket(size):
    return "unknown" if size is None else "<200" if size < 200 else "200-800" if size <= 800 else ">800"


def change_failure_summary(merged):
    """Merged PRs whose merge started a main-red episode: counts, the raw minutes to green (no median: n is small and the
    distribution is long), the unresolved ones, and the PRs whose base checks were not read in full (`unknown`)."""
    known = [r for r in merged if r.get("main_red") is not None]
    eps = [e for r in known for e in r["main_red"]]
    cf = [e for e in eps if e["head_red_same_context"]]  # merged red on its own final head, and main went red after: a change failure
    return {"merged": len(merged), "unknown": len(merged) - len(known),
            "prs_that_started_red": sum(1 for r in known if r["main_red"]), "episodes": len(eps),
            "change_failures": len(cf), "other_post_merge_red": sum(1 for e in eps if e["head_red_same_context"] is False),
            "minutes_to_green": sorted(e["minutes_to_green"] for e in cf if e["minutes_to_green"] is not None),
            "unresolved": sum(1 for e in cf if e["minutes_to_green"] is None and not e.get("end_unknown")),
            "end_unknown": sum(1 for e in eps if e.get("end_unknown")),
            "head_red_same_context": len(cf), "head_red_unknown": sum(1 for e in eps if e["head_red_same_context"] is None)}


def path_summary(merged):
    """Clean and slow paths over merged PRs: counts, lead-time medians and p90, the slow share, and the minutes per merged
    PR each cause held the slow ones up. `unknown` PRs (no slow cause seen, compute not read in full) are in neither path."""
    clean = [r for r in merged if r.get("path") == "clean"]
    slow = [r for r in merged if r.get("path") == "slow"]
    lead = lambda rs: [r["handoff_min"] for r in rs if r["handoff_min"] is not None]
    causes = {}
    for c in CAUSES:
        vals = []
        measured = True
        for r in slow:
            cm = r.get("cause_min")
            if not cm or cm.get(c) is None:
                measured = False
                break
            vals.append(cm[c])
        # An unknown slow PR must not be treated as a zero-minute contribution. Withhold the
        # aggregate until every slow PR has a measured value for this cause.
        causes[c] = round(sum(vals) / len(merged), 2) if measured and vals and merged else None
    known = {c: v for c, v in causes.items() if v is not None}
    return {
        "merged": len(merged), "unknown": len(merged) - len(clean) - len(slow),
        "clean": {"n": len(clean), "median_min": med(lead(clean)), "p90_min": pctile(lead(clean), 0.9)},
        "slow": {"n": len(slow), "median_min": med(lead(slow)), "p90_min": pctile(lead(slow), 0.9)},
        "slow_share_pct": pct_of(len(slow), len(slow) + len(clean)),
        "slow_reasons": dict(collections.Counter(w for r in slow for w in r["path_why"]).most_common()),
        "cause_min_per_merge": causes, "top_cause": max(known, key=known.get) if known else None,
        "note": "causes overlap and are not additive; dirty is not measured",
    }


def workflow_means(comp):
    """Per merged PR with a complete compute read: mean runs of each workflow at each level (the PR's own heads,
    its merge group, pushes to the base). `prs` is the denominator."""
    names = sorted({n for r in comp for n in r["compute"].get("by_workflow", {})})
    out = {}
    for n in names:
        out[n] = {lv: round(sum(r["compute"]["by_workflow"].get(n, {}).get(lv, 0) for r in comp) / len(comp), 2)
                  for lv in ("pull_request", "merge_group", "push")}
        out[n]["prs"] = len(comp)
    return out


def attribution_summary(rows):
    """What each agent signal contributed, so the attribution can be audited. `by_primary` counts each PR once, under
    its strongest signal; `with_signal` counts every PR a signal applies to. `ambiguous_cleared` counts PRs
    with an owner-identity commit that the earlier rule called unknown and a signal now explains.
    `legacy` is the earlier rule over the same PRs."""
    at = [r["attribution"] for r in rows if r.get("attribution")]
    n = len(rows)
    legacy = collections.Counter(a["legacy_north"] for a in at)
    prim = collections.Counter(a["primary"] or "none" for a in at)
    cleared = collections.Counter(a["primary"] for a, r in ((r["attribution"], r) for r in rows if r.get("attribution"))
                                  if a["primary"] and a["legacy_north"] == "unknown" and r["north"] != "unknown")
    return {
        "by_primary": {k: prim.get(k, 0) for k in SIGNALS + ("none",)},
        "with_signal": {k: sum(1 for a in at if k in a["signals"]) for k in SIGNALS},
        "ambiguous_cleared": {k: cleared.get(k, 0) for k in SIGNALS},
        "steward_merges_recognised": sum(1 for a in at if a["steward_merge_recognised"]),
        "owner_merges_unverified": sum(1 for a in at if a.get("merge_unverified")),
        "agent_stamped_owner_events": sum(a["agent_owner_events"] for a in at),
        "legacy": {"autonomous": legacy["autonomous"], "owner_action": legacy["owner_action"], "unknown": legacy["unknown"],
                   "not_merged_in_48h": legacy["not_merged"], "rate_pct": pct_of(legacy["autonomous"], n),
                   "unknown_share_pct": pct_of(legacy["unknown"], n)},
    }


def summarize(rows):
    """The scorecard over a list of eligible ready PRs (one row each)."""
    n = len(rows)
    ns = collections.Counter(r["north"] for r in rows)
    gh = collections.Counter(r["north_gh"] for r in rows)
    auto, unk = ns["autonomous"], ns["unknown"]
    merged = [r for r in rows if r["merged"]]
    hand = [r["handoff_min"] for r in merged]
    comp = [r for r in merged if r["compute"] and r["compute"]["complete"]]
    jobs_comp = [r for r in comp if r["compute"].get("jobs_read", True)]  # minutes come only from PRs whose jobs were read
    held = [r for r in rows if r["holds"]]
    rehold = [r for r in held if r["re_holds"]]
    by_size = {}
    for r in held:
        b = by_size.setdefault(bucket(r["size"]), {"prs_with_hold": 0, "prs_with_re_hold": 0})
        b["prs_with_hold"] += 1
        b["prs_with_re_hold"] += bool(r["re_holds"])
    push = {k: sum(r["pushes"].get(k, 0) for r in rows) for k in ("repair", "author", "merge_main")}
    q2m = [r["enqueue_to_merge_min"] for r in merged if r.get("enqueue_to_merge_min") is not None]
    ej = collections.Counter(c for r in rows for c in r["ejections"])
    good = [r for r in merged if r["correct"] and r["correct"]["bad"] is False]
    bad = [r for r in merged if r["correct"] and r["correct"]["bad"]]
    esc_known = [r for r in merged if r["correct"] and r["correct"].get("escaped") is not None]
    esc = [r for r in esc_known if r["correct"]["escaped"]]
    attr = attribution_summary(rows)
    return {
        "ready_prs": n, "merged": len(merged),
        "paths": path_summary(merged),
        "change_failure": change_failure_summary(merged),
        "attribution": attr,
        "north_star": {
            "autonomous": auto, "owner_action": ns["owner_action"], "unknown": unk, "not_merged_in_48h": ns["not_merged"],
            "rate_pct": pct_of(auto, n), "rate_ci95": wilson(auto, n),
            "rate_range_pct": [pct_of(auto, n), pct_of(auto + unk, n)],
            "unknown_share_pct": pct_of(unk, n), "unknown_over_20pct": bool(n) and unk / n > UNKNOWN_RANGE_SHARE,
            "insufficient_sample": n < MIN_SAMPLE,
            "github_only": {"autonomous": gh["autonomous"], "owner_action": gh["owner_action"], "unknown": gh["unknown"],
                            "rate_pct": pct_of(gh["autonomous"], n), "rate_ci95": wilson(gh["autonomous"], n)},
        },
        "rounds": {
            "holds_per_ready_pr": None if not n else round(sum(r["holds"] for r in rows) / n, 2),
            "prs_with_hold": len(held), "prs_with_re_hold": len(rehold),
            "re_hold_rate_pct": pct_of(len(rehold), len(held)), "re_hold_rate_ci95": wilson(len(rehold), len(held)),
            "by_size": dict(sorted(by_size.items())),
        },
        "pushes_per_ready_pr": {k: None if not n else round(v / n, 2) for k, v in push.items()},
        "pushes_total": push,
        "merge_queue": {
            "attempts_per_ready_pr": None if not n else round(sum(r["enqueues"] for r in rows) / n, 2),
            "ejections_by_cause": dict(ej.most_common()),
            "ejections_per_merge": None if not merged else round(sum(ej.values()) / len(merged), 3),
            "top_ejection_cause": next((c for c, _ in ej.most_common() if c != "unknown"), None),
            "ejections_cause_unknown": ej.get("unknown", 0),
            "enqueue_to_merge_min": {"n": len(q2m), "median": med(q2m), "p90": pctile(q2m, 0.9)},
        },
        "dirty_per_100_prs": None if not n else round(100.0 * sum(1 for r in rows if r["dirty"]) / n, 1),
        "interventions": {
            "owner_actions_per_pr": None if not n else round(sum(r["owner_actions"] for r in rows) / n, 2),
            "repair_cycles_per_pr": None if not n else round(sum(r["repair_cycles"] for r in rows) / n, 2),
            "prs_with_owner_action": sum(1 for r in rows if r["owner_actions"]),
            "prs_with_repair_cycle": sum(1 for r in rows if r["repair_cycles"]),
        },
        "handoff_min": {"n": len(hand), "median": med(hand), "p90": pctile(hand, 0.9),
                        "note": "load-dependent context, not the verdict"},
        "compute_per_merged_pr": {
            "prs_read": len(comp), "prs_unread_or_partial": len(merged) - len(comp),
            "runs_median": med([r["compute"]["runs"] for r in comp]),
            "runs_mean": None if not comp else round(sum(r["compute"]["runs"] for r in comp) / len(comp), 2),
            "repeat_runs_total": sum(r["compute"]["repeat_runs"] for r in comp),
            "runs_total": sum(r["compute"]["runs"] for r in comp),
            "context_self_hosted_min_median": med([r["compute"]["self_hosted_min"] for r in jobs_comp]),
            "context_hosted_min_median": med([r["compute"]["hosted_min"] for r in jobs_comp]),
            "macos_min_mean": None if not jobs_comp else round(sum(r["compute"]["macos_min"] for r in jobs_comp) / len(jobs_comp), 1),
            "macos_min_median": med([r["compute"]["macos_min"] for r in jobs_comp]),
            "by_workflow": workflow_means(comp),
        },
        "correctness": {
            "bad": len(bad), "good": len(good), "unknown": len(merged) - len(bad) - len(good),
            "reverted": sum(1 for r in merged if r["correct"] and r["correct"]["reverted"]),
            "base_red": sum(1 for r in merged if r["correct"] and r["correct"]["base_red"]),
            "bad_rate_pct": pct_of(len(bad), len(bad) + len(good)), "bad_rate_ci95": wilson(len(bad), len(bad) + len(good)),
            "provisional_prs": sum(1 for r in merged if r["correct"] and r["correct"]["provisional"]),
            "escaped": len(esc), "escaped_known": len(esc_known),
            "escaped_rate_pct": pct_of(len(esc), len(esc_known)), "escaped_rate_ci95": wilson(len(esc), len(esc_known)),
            "escape_provisional_prs": sum(1 for r in merged if r["correct"] and r["correct"].get("escape_provisional")),
            "escape_floor": "titles and bodies of later PRs naming the PR only",
        },
    }


def all_prs(rd, lo, hi):
    """PRs created in [lo, hi], newest first. Returns (prs, complete)."""
    out, n = [], 0
    while n < 8:
        n += 1
        items = rd.get(f"pulls?state=all&sort=created&direction=desc&per_page=100&page={n}")
        if not isinstance(items, list):
            raise ReadError("unexpected shape")
        for p in items:
            c = iso(p.get("created_at"))
            if c and lo <= c <= hi:
                out.append(p)
        if len(items) < 100 or (items and (iso(items[-1].get("created_at")) or hi) < lo):
            return out, True
    return out, False


def collect_repo(repo, cache_dir, cap, owner, lo, hi, now, ticks, tick_ctx, fetch=None, managed_base="main", dirty=(), merged_between=None):
    rd = Reader(repo, cache_dir, cap, fetch)
    res = {"repo": repo, "rows": [], "unread": 0, "total_prs": 0, "listing_complete": False, "reads": 0, "truncated": False, "errors": 0}
    try:
        prs, res["listing_complete"] = all_prs(rd, lo, hi)
    except (Truncated, ReadError):
        prs = []
    merged_list = [p for p in prs if p.get("merged_at")]
    if merged_between:  # a measurement window: only the PRs that merged in it are read; the rest created since `lo` are skipped
        a, b = merged_between
        prs = [p for p in prs if p.get("merged_at") and a <= iso(p["merged_at"]) <= b]
    # Read the oldest cohort day first, whole days at a time: when the read cap runs out, the unread PRs are the newest
    # ones, the cache keeps what was read, and the next run resumes where this one stopped.
    prs = sorted(prs, key=lambda p: (iso(p["created_at"]), p["number"]))
    res["total_prs"] = len(prs)
    facts = []
    for pr in prs:
        f = rd.load("facts", str(pr["number"]))
        if f is not None and (f.get("v") != FACTS_VERSION or not reads_complete(f)):
            f = None  # an older reduction, or one cached from a partial read (an entry saved before partial reads were refused): read again
        if f is None:
            try:
                f = pr_facts(rd, pr, owner)
                if f["state"] == "closed" and reads_complete(f) and f.get("ejection_causes_known", True):  # a closed PR does not change; an open one, or a partial read, is read again next run
                    rd.save("facts", str(pr["number"]), f)
            except (Truncated, ReadError):
                res["unread"] += 1
                continue
        finalize(rd, f, pr)
        ok, why = eligible(f, managed_base)
        facts.append((f, ok, why))
    mrows = [f for f, ok, _ in facts if ok and f["merged_at"]]
    runs, runs_ok = [], False
    if mrows:  # always read: a cached compute entry is validated against the current runs (a rerun changes a run in place)
        try:
            runs, runs_ok = runs_index(rd, min(iso(f["created_at"]) for f in mrows), max(iso(f["merged_at"]) for f in mrows) + dt.timedelta(hours=1))
        except (Truncated, ReadError):
            pass
    runs_by_id = {r["id"]: r for r in runs} if runs_ok else None
    base_commits, commits_ok, req, listing_cut, base_anchors = [], False, None, False, None
    if mrows:
        try:
            until = max(iso(f['merged_at']) for f in mrows) + WINDOW + dt.timedelta(hours=2)
            listing_cut = until < (now or dt.datetime.now(UTC))  # the listing stops short of the base's tip
            if listing_cut:
                base_anchors = [p["merge_commit_sha"] for p in sorted(merged_list, key=lambda p: p["merged_at"]) if p.get("merge_commit_sha")]
            base_commits, commits_ok = rd.pages(
                f"commits?sha={mrows[0]['base']}&since={fmt(min(iso(f['merged_at']) for f in mrows) - BASE_LOOKBACK)}"
                f"&until={fmt(until)}", max_pages=BASE_LISTING_PAGES)
        except (Truncated, ReadError):
            pass
        try:
            req = required_contexts(rd, mrows[0]["base"], tick_ctx.get(repo))
        except (Truncated, ReadError):
            pass
    episodes, ep_ok, ep_skipped, untracked, attributable = [], False, 0, {"one_off": [], "noisy": []}, set()
    if mrows and commits_ok:
        try:
            episodes, ep_skipped, untracked, attributable = detect_main_red(rd, base_commits, merged_list, now, listing_cut)
            ep_ok = True
        except (Truncated, ReadError):
            pass
    res["main_red"] = {"episodes": episodes, "complete": ep_ok, "skipped_unsettled_commits": ep_skipped, "untracked_contexts": untracked}
    out = []
    for f, ok, why in facts:
        merged = f["merged_at"]
        row = {"pr": f["number"], "eligible": ok, "excluded_because": why, "merged": bool(merged),
               "ready_at": f["first_ready"], "merged_at": merged}
        if f["first_ready"]:
            row["day"] = f["first_ready"][:10]
        row.update({
            "north": north_star(f) if f["first_ready"] else None,
            "attribution": row_attribution(f) if f["first_ready"] else None, "north_gh": north_star(f, False) if f["first_ready"] else None,
            "handoff_min": round((iso(merged) - iso(f["first_ready"])).total_seconds() / 60.0, 1) if merged and f["first_ready"] else None,
            "holds": f["holds"], "re_holds": f["re_holds"], "size": f["size"], "pushes": f["pushes"],
            "enqueues": f["enqueues"], "ejections": f["ejections"], "dirty": f["number"] in dirty,
            "enqueue_to_merge_min": enqueue_to_merge(f),
            "owner_actions": sum(1 for ev, ts in effective_owner_events(f)[0]
                                 if not (ev == "merged" and f.get("merge_unverified")) and f["first_ready"] and iso(f["first_ready"]) <= iso(ts) <= iso(f["first_ready"]) + HORIZON),
            "repair_cycles": f["repair_cycles"], "compute": None, "correct": None, "path": None, "path_why": [], "cause_min": None, "main_red": None,
        })
        if ok and merged:
            comp = load_compute(rd, f["number"], f, runs_by_id)
            if comp is None and runs_ok:
                try:
                    comp = pr_compute(rd, f, runs, runs_ok)
                    if comp["complete"] and comp["jobs_read"]:  # minutes not read: do not cache, a full read recomputes
                        rd.save("compute", str(f["number"]), dict(comp, v=COMPUTE_VERSION))
                except (Truncated, ReadError):
                    comp = None
            row["compute"] = comp
            row["path"], row["path_why"] = path_of(dict(f, dirty=row["dirty"]), comp)
            if runs_by_id is None and row["path"] == "clean":
                row["path"] = None  # a cached success that could not be checked against the current runs (a rerun may have failed): unknown, not clean
            row["cause_min"] = cause_minutes(f, comp)
            # the episodes this PR's merge started (empty list: none), or None when the base's checks were not read in full
            row["main_red"] = [{k: ep.get(k) for k in ("contexts", "minutes_to_green", "fixer_pr", "head_red_same_context", "end_unknown")}
                               for ep in episodes if ep["pr"] == f["number"]] if ep_ok and f.get("merge_commit_sha") in attributable else None
            corr = rd.load("correct", str(f["number"]))
            if corr is not None and corr.get("v") != CHECKS_VERSION:
                corr = None
            if corr is None:
                try:
                    corr = pr_correct(rd, f, base_commits, commits_ok, req, ticks, now, merged_list, res["listing_complete"], base_anchors)
                    if now >= iso(merged) + ESCAPE_WINDOW and corr["bad"] is not None and corr["escaped"] is not None:
                        rd.save("correct", str(f["number"]), dict(corr, v=CHECKS_VERSION))
                except (Truncated, ReadError):
                    corr = {"provisional": True, "reverted": None, "base_red": None, "bad": None, "escaped": None, "escaped_by": None}
            if corr.get("escaped") is None and res["listing_complete"]:  # independent of the base reads
                corr["escaped_by"] = escaped_by(merged_list, f)
                corr["escaped"] = corr["escaped_by"] is not None
            corr["provisional"] = now < iso(merged) + WINDOW
            corr["escape_provisional"] = now < iso(merged) + ESCAPE_WINDOW
            row["correct"] = corr
        out.append(row)
    res.update(rows=out, reads=rd.calls, truncated=rd.truncated, errors=rd.errors)
    return res


def day_closed(day, now):
    """A cohort day is reported once its 48h horizon has closed: the end of the day plus 48 hours."""
    d = dt.datetime.strptime(day, "%Y-%m-%d").replace(tzinfo=UTC)
    return now >= d + dt.timedelta(days=1) + HORIZON


def check_controls(repo, rows, controls, refused, main_red=None):
    """Positive controls: a known revert, a known escaped defect, and owner-touched PRs must be found.
    A control whose PR is not among the report's rows is `not_in_window` (unchecked, not passed)."""
    by = {r["pr"]: r for r in rows}
    c = (controls or {}).get(repo, {})
    out = {"revert": {"expected": 0, "found": 0, "not_in_window": 0}, "escape": {"expected": 0, "found": 0, "not_in_window": 0},
           "owner_touched": {"in_report": 0, "non_autonomous": 0, "not_in_window": 0}}
    for reverted, _ in c.get("revert", []):
        r = by.get(reverted)
        if not r or not r["correct"] or r["correct"]["reverted"] is None:
            out["revert"]["not_in_window"] += 1
            continue
        out["revert"]["expected"] += 1
        out["revert"]["found"] += bool(r["correct"]["reverted"])
    for escaped, naming in c.get("escape", []):
        r = by.get(escaped)
        if not r or not r["correct"] or r["correct"].get("escaped") is None:
            out["escape"]["not_in_window"] += 1
            continue
        out["escape"]["expected"] += 1
        out["escape"]["found"] += r["correct"].get("escaped_by") == naming
    # Known incidents: a PR whose merge turned a main context red and the PR that turned it green (the change-failure detector).
    out["incident"] = {"expected": 0, "found": 0, "not_in_window": 0}
    eps = (main_red or {}).get("episodes", [])
    for pin in c.get("incident", []):
        escaped, fixer = pin[0], pin[1]
        head = pin[2] if len(pin) > 2 else None
        if escaped not in by or not (main_red or {}).get("complete"):
            out["incident"]["not_in_window"] += 1
            continue
        out["incident"]["expected"] += 1
        out["incident"]["found"] += any(e["pr"] == escaped and e["fixer_pr"] == fixer and (head is None or e.get("head_red_same_context") is head)
                                        for e in eps)
    # `refused` PRs were stopped by direct_merge_refused (a ruleset config gap), which says nothing about
    # whether the owner touched them, so they are only counted for context and never fail the control.
    for n in sorted(refused or ()):
        r = by.get(n)
        if not r or not r["north"]:
            out["owner_touched"]["not_in_window"] += 1
            continue
        out["owner_touched"]["in_report"] += 1
        out["owner_touched"]["non_autonomous"] += r["north"] == "owner_action"
    # Pinned owner-touched PRs (verified by hand to carry a genuine owner action) must read owner_action.
    out["owner_pinned"] = {"expected": 0, "found": 0, "not_in_window": 0}
    for n in c.get("owner", []):
        r = by.get(n)
        if not r or not r["north"]:
            out["owner_pinned"]["not_in_window"] += 1
            continue
        out["owner_pinned"]["expected"] += 1
        out["owner_pinned"]["found"] += r["north"] == "owner_action"
    o = out
    failed, disabled = [], []
    if o["revert"]["found"] < o["revert"]["expected"]:
        failed.append("revert control not found")
        disabled.append("correctness")
    if o["escape"]["found"] < o["escape"]["expected"]:
        failed.append("escaped-defect control not found")
        disabled.append("escaped")
    if o["owner_pinned"]["found"] < o["owner_pinned"]["expected"]:
        failed.append("owner-touched control read as autonomous")
        disabled.append("north_star")
    if o["incident"]["found"] < o["incident"]["expected"]:
        failed.append("known main-red incident not found")
        disabled.append("change_failure")
    checked = o["revert"]["expected"] + o["escape"]["expected"] + o["owner_pinned"]["expected"] + o["incident"]["expected"]
    out["status"] = "failed" if failed else "ok" if checked else "unchecked"
    out["failed"] = failed
    out["disabled"] = disabled  # parallel to `failed`; only these metrics are withheld; the rest of the report stands
    return out


def build(repos, cache_dir, cap, owner, start, now, ticks, tick_ctx, baseline=None, fetch=None, bases=None,
          dirty=None, controls=None, refused=None, control=None, merged_between=None):
    """The `end_to_end` report section. `ticks`: repo -> [datetimes a base_poison red-everywhere episode began].
    `control`: None runs the synthetic control (tools/metrics/synthetic_control.py) on every call; False skips it
    (the control itself runs with False); a callable returns a control result. A failed control withholds every
    outcome number: the report then carries only the failure and its deltas."""
    lo = start
    if baseline:
        lo = min(lo, baseline[0])
    # `cap` is a shared total per run. Each repo gets an equal share of what is left, so reads a small
    # repo does not use (a warm cache) flow to the repos read after it.
    per_cap = max(1, cap // max(1, len(repos)))
    remaining = cap
    controls = DEFAULT_CONTROLS if controls is None else controls
    out = {"scorecard_version": SCORECARD_VERSION, "owner": owner, "read_cap_per_repo": per_cap, "read_cap_total": cap, "repos": {}, "truncated": False,
           "reads": 0, "coverage": {"status": "available", "reason": ""}, "baseline": None, "window": None, "controls_failed": False}
    notes = []
    for i, repo in enumerate(repos):
        repo_cap = remaining // (len(repos) - i)  # may be 0: the shared total is never exceeded
        r = collect_repo(repo, cache_dir, repo_cap, owner, lo, now, now, ticks, tick_ctx, fetch,
                         (bases or {}).get(repo, "main"), (dirty or {}).get(repo, ()), merged_between)
        excluded = collections.Counter(x["excluded_because"] for x in r["rows"] if not x["eligible"])
        elig = [x for x in r["rows"] if x["eligible"]]
        days = {}
        for x in elig:
            days.setdefault(x["day"], []).append(x)
        ctl = check_controls(repo, r["rows"], controls, (refused or {}).get(repo), r["main_red"])
        out["repos"][repo] = {
            "per_day": {d: {"closed": day_closed(d, now), **summarize(v)} for d, v in sorted(days.items())},
            "prs": r["rows"], "excluded": dict(excluded), "controls": ctl,
            "unread_prs": r["unread"], "listing_complete": r["listing_complete"], "total_prs": r["total_prs"], "main_red": r["main_red"],
            "reads": r["reads"], "truncated": r["truncated"], "read_errors": r["errors"],
        }
        if r["unread"] or not r["listing_complete"]:
            # read everything or withhold: a number over some of a repo's PRs says nothing about the repo
            v = out["repos"][repo]
            v["withheld"] = {"unread_prs": r["unread"], "total_prs": r["total_prs"], "listing_complete": r["listing_complete"]}
            v["per_day"], v["prs"] = {}, []
        out["truncated"] = out["truncated"] or r["truncated"]
        out["reads"] += r["reads"]
        remaining = max(0, remaining - r["reads"])
        out["controls_failed"] = out["controls_failed"] or ctl["status"] == "failed"
        if r["truncated"] or r["unread"] or not r["listing_complete"] or r["errors"]:
            notes.append(f"{repo}: truncated={r['truncated']} unread={r['unread']} listing_complete={r['listing_complete']} read_errors={r['errors']}")
    if notes:
        out["coverage"] = {"status": "partial", "reason": "; ".join(notes)}
    ds = start.strftime("%Y-%m-%d")

    def pooled(v, a, b):
        rows = [p for p in v["prs"] if p["eligible"] and a <= p["day"] <= b and day_closed(p["day"], now)]
        s = summarize(rows)
        open_days = sorted({p["day"] for p in v["prs"] if p["eligible"] and a <= p["day"] <= b and not day_closed(p["day"], now)})
        s["open_days_not_reported"] = open_days
        return s
    live = {repo: v for repo, v in out["repos"].items() if not v.get("withheld")}
    out["window"] = {"from": ds, "to": now.strftime("%Y-%m-%d"), "per_repo": {repo: pooled(v, ds, "9999") for repo, v in live.items()}}
    if baseline:
        b0, b1 = (x.strftime("%Y-%m-%d") for x in baseline)
        per = {repo: pooled(v, b0, b1) for repo, v in live.items()}
        floor = {}
        for repo, v in live.items():
            rates = {d: s["north_star"]["rate_pct"] for d, s in v["per_day"].items() if b0 <= d <= b1 and s["closed"] and s["ready_prs"]}
            vals = [x for x in rates.values() if x is not None]
            floor[repo] = {"north_star_rate_pct_by_day": rates, "spread_pp": None if len(vals) < 2 else round(max(vals) - min(vals), 1)}
        out["baseline"] = {"from": b0, "to": b1, "per_repo": per, "noise_floor": floor}
    for repo, s in out["window"]["per_repo"].items():  # window vs baseline: overlapping intervals mean no difference is claimed
        bs = (out["baseline"] or {}).get("per_repo", {}).get(repo)
        a, b = s["north_star"]["rate_ci95"], (bs or {}).get("north_star", {}).get("rate_ci95")
        s["north_star"]["overlaps_baseline"] = None if not (a and b) else not (a[0] > b[1] or b[0] > a[1])
    out["synthetic_control"] = run_synthetic_control(control)
    if out["synthetic_control"] and out["synthetic_control"]["status"] == "pass":
        for v in out["repos"].values():  # no live PR may pin a control for this repo; the injected case covers every repo
            if v["controls"]["status"] == "unchecked":
                v["controls"]["status"] = "synthetic"
    if out["synthetic_control"] and out["synthetic_control"]["status"] != "pass":
        out["withheld"] = "the synthetic control failed, so no outcome number is reported"
        for v in out["repos"].values():
            v["per_day"], v["prs"] = {}, []
        out["window"]["per_repo"], out["baseline"] = {}, None
    return out


def run_synthetic_control(control):
    """The synthetic control's result for this report, or None when it is switched off."""
    if control is False:
        return None
    if callable(control):
        return control()
    here = os.path.dirname(os.path.abspath(__file__))
    if here not in sys.path:
        sys.path.insert(0, here)
    import synthetic_control
    return synthetic_control.run(sys.modules[__name__])


def ci(x):
    return "n/a" if not x else f"{x[0]}-{x[1]}"


def repo_partial(v):
    """A repo is partial when its read cap ran out, a read failed, PRs were left unread or its listing was cut short."""
    return bool(v["truncated"] or v.get("read_errors") or v["unread_prs"] or not v["listing_complete"])


def sample_label(s, truncated):
    ns = s["north_star"]
    why = []
    if ns["insufficient_sample"]:
        why.append(f"n<{MIN_SAMPLE}")
    if ns.get("overlaps_baseline"):
        why.append("interval overlaps baseline")
    if truncated:
        why.append("partial: not fully read")
    return "insufficient sample (" + ", ".join(why) + ")" if why else "ok"


def markdown(e2e):
    L = ["", f"## Scorecard {e2e['scorecard_version']}: end-to-end per eligible PR (cohort = UTC day the PR became ready)", "",
         "Baseline cohort: PRs ready 2026-11-12 to 2026-11-14. A day closes 48h after it ends, so the first comparable later cohort day closes 48h after that day.", "",
         f"Reads {e2e['reads']} (cap {e2e.get('read_cap_total', e2e['read_cap_per_repo'])} per run, shared: each repo gets an equal share of what is left), truncated={str(e2e['truncated']).lower()}"
         + (" : a repo with any unread PR is withheld entirely; fully read repos are reported normally" if e2e["truncated"] else "") + f". Coverage: {e2e['coverage']['status']} {e2e['coverage']['reason']}", "",
         f"North Star: eligible PRs that became ready in the window and merged within 48h of ready with no owner action ({e2e['owner']}'s own account: "
         "review, label, comment, UI merge or close), over ALL eligible PRs ready in the window. A cohort day is reported only after its 48h horizon closed. "
         "Agents commit under the owner's git identity, so a PR is agent work when it carries any signal: opened by the fleet App, `shipyard pr` provenance in the body, "
         "an agent Co-Authored-By trailer, an agent session stamp. Only a PR with an owner-identity commit and no signal is unknown; above 20% unknown the rate is a range. "
         "An owner-account event is an owner action only when it is a human's: one carrying a session stamp is an agent's, and a merge by the owner account is a "
         "steward merge only with trusted evidence bound to the merged SHA and read in timeline order, never by date: an App enqueue made while the timeline's head was the merged SHA, "
         "or an exact-head APPROVE counted by the guard's rules (the App, a configured `reviewer_signers` signer, a configured `reviewer_stamp_sessions` stamp), with no later push, queue removal or HOLD; "
         "a HOLD, a verdict path or `shipyard pr` provenance alone does not qualify. A merge the timeline cannot tie to the merged SHA, or read from an incomplete timeline or commit list, is unknown: "
         "credited to neither side, counted as an unverified owner merge and left out of the owner-action counts. `gh-only` ignores commits. "
         "Rates carry a 95% Wilson interval; below 50 PRs, or when the interval overlaps the baseline's, the sample is insufficient and no difference is claimed. "
         "Handoff-to-merge is load-dependent context, not the verdict. Verdict proxies: review rounds (HOLDs) and re-HOLD rate per PR, agent pushes by kind "
         "(repair after a HOLD or `repaired:`, author before, merge-main), merge-queue attempts and ejections by cause, DIRTY per 100 PRs, gate runs per merged PR "
         "(repeats counted; minutes are context). Correctness: revert or red required check on the base within 7 days; escaped defect (best effort, titles and bodies naming the PR only) within 14.", ""]
    sc = e2e.get("synthetic_control")
    if sc:
        import synthetic_control
        L += [f"**{synthetic_control.line(sc)}**", ""]
        if sc["status"] != "pass":
            return "\n".join(L + ["**Outcome numbers are withheld: the instrument did not reproduce its own injected case. Nothing below this line is reported.**", ""]) + "\n"
    if e2e["controls_failed"]:
        L += ["**CONTROL FAILED for some metrics (those cells read `withheld`; everything else is reported).** A positive control read zero, so that instrument is broken:", ""]
        for repo, v in sorted(e2e["repos"].items()):
            for f, m in zip(v["controls"]["failed"], v["controls"]["disabled"]):
                L.append(f"- {repo}: {f} (withheld: {m})")
        L.append("")
    head = ("| {0} | ready PRs | autonomous a/o/unknown/not merged | rate (CI95) | range if unknown>20% | unknown | gh-only rate | sample | "
            "holds/PR | re-HOLD rate (CI95) | pushes/PR repair/author/merge-main | queue attempts/PR | ejections | DIRTY/100 | owner actions/PR | repair cycles/PR | "
            "runs per merged PR median/mean (repeats) | handoff median/p90 min (context) | correctness bad/good/unk | bad rate (CI95) | escaped (CI95) | context min sh/hosted |")
    sep = "|" + "---|" * 22

    def row(label, s, ctl="", trunc=False):
        a, r, p, c, k, h = s["north_star"], s["rounds"], s["pushes_per_ready_pr"], s["compute_per_merged_pr"], s["correctness"], s["handoff_min"]
        off = set((ctl or {}).get("disabled", ()))
        W = "withheld"
        rng = f"{a['rate_range_pct'][0]}-{a['rate_range_pct'][1]}%" if a["unknown_over_20pct"] else "-"
        ej = ", ".join(f"{k2} {n}" for k2, n in s["merge_queue"]["ejections_by_cause"].items()) or "0"
        zero_bad = f" (control: reverts {ctl['revert']['found']}/{ctl['revert']['expected']})" if ctl and ctl["revert"]["expected"] and k["reverted"] == 0 and k["base_red"] == 0 else ""
        zero_esc = f" (control: escapes {ctl['escape']['found']}/{ctl['escape']['expected']})" if ctl and ctl["escape"]["expected"] and k["escaped"] == 0 else ""
        ns_off = "north_star" in off
        return (f"| {label} | {s['ready_prs']} | {W if ns_off else str(a['autonomous']) + '/' + str(a['owner_action']) + '/' + str(a['unknown']) + '/' + str(a['not_merged_in_48h'])} | "
                f"{W if ns_off else str(a['rate_pct']) + '% (' + ci(a['rate_ci95']) + ')'} | {W if ns_off else rng} | {W if ns_off else str(a['unknown_share_pct']) + '%'} | "
                f"{W if ns_off else str(a['github_only']['rate_pct']) + '% (' + ci(a['github_only']['rate_ci95']) + ')'} | "
                f"{sample_label(s, trunc)} | {r['holds_per_ready_pr']} | {r['re_hold_rate_pct']}% ({ci(r['re_hold_rate_ci95'])}), n={r['prs_with_hold']} | "
                f"{p['repair']}/{p['author']}/{p['merge_main']} | {s['merge_queue']['attempts_per_ready_pr']} | {ej} | {s['dirty_per_100_prs']} | "
                f"{s['interventions']['owner_actions_per_pr']} | {s['interventions']['repair_cycles_per_pr']} | {c['runs_median']}/{c['runs_mean']} ({c['repeat_runs_total']}/{c['runs_total']}) | "
                f"{h['median']}/{h['p90']} (n={h['n']}) | {W if 'correctness' in off else str(k['bad']) + '/' + str(k['good']) + '/' + str(k['unknown'])} | "
                f"{W if 'correctness' in off else str(k['bad_rate_pct']) + '% (' + ci(k['bad_rate_ci95']) + ')' + zero_bad} | "
                f"{W if 'escaped' in off else str(k['escaped']) + '/' + str(k['escaped_known']) + ' (' + ci(k['escaped_rate_ci95']) + ')' + zero_esc} | {c['context_self_hosted_min_median']}/{c['context_hosted_min_median']} |")

    def ctl_str(v):
        c = v["controls"]
        return (f"reverts {c['revert']['found']}/{c['revert']['expected']}, escapes {c['escape']['found']}/{c['escape']['expected']}, owner-touched (context) {c['owner_touched']['non_autonomous']}/{c['owner_touched']['in_report']}, "
               f"owner-pinned {c['owner_pinned']['found']}/{c['owner_pinned']['expected']} ({c['status']})")
    for repo, v in sorted(e2e["repos"].items()):
        L += [f"### {repo}", ""]
        if v.get("withheld"):
            w = v["withheld"]
            L += [f"**WITHHELD: {w['unread_prs']} of {w['total_prs']} PRs unread, listing complete={str(w['listing_complete']).lower()}.** "
                  "No row is printed for a repo until every PR in its window is read. PRs are read oldest cohort day first and cached, so the next run resumes where this one stopped.", "",
                  f"Controls ({v['controls']['status']}): " + ctl_str(v), ""]
            continue
        if repo_partial(v):
            L += [f"**TRUNCATED: {v['unread_prs']} PRs unread** ({v['reads']} reads, read cap hit={str(v['truncated']).lower()}, "
                  f"listing complete={str(v['listing_complete']).lower()}). Cached PRs are not re-read, so the next run resumes.", ""]
        if v.get("excluded"):
            L += ["Excluded from the cohort: " + ", ".join(f"{k} {n}" for k, n in sorted(v["excluded"].items())), ""]
        L += [f"Controls ({v['controls']['status']}): " + ctl_str(v) + f"; not in window: {sum(x['not_in_window'] for x in v['controls'].values() if isinstance(x, dict))}", ""]
        L += [head.format("ready day"), sep]
        for d, s in v["per_day"].items():
            L.append(row(d, s, v["controls"], repo_partial(v)) if s["closed"] else f"| {d} (48h horizon open: not reported) |" + " |" * 21)
        L.append("")
    for title, blk in (("Window", e2e.get("window")), ("Baseline", e2e.get("baseline"))):
        if blk:
            L += [f"## {title} {blk['from']} to {blk['to']} (pooled, closed days only)", "", head.format("repo"), sep]
            L += [row(repo, s, e2e["repos"][repo]["controls"], repo_partial(e2e["repos"][repo])) for repo, s in sorted(blk["per_repo"].items())] + [""]
    if e2e.get("window"):
        L += ["### Attribution by signal (window; each PR counted once under its strongest signal)", "",
              "| repo | PRs | app opener | provenance | trailer | session stamp | no signal | unknown cleared by signals | steward merges | unverified owner merges | agent-stamped owner events | legacy rate / unknown |",
              "|---|---|---|---|---|---|---|---|---|---|---|---|"]
        for repo, s_ in sorted(e2e["window"]["per_repo"].items()):
            at = s_["attribution"]
            p, lg = at["by_primary"], at["legacy"]
            L.append(f"| {repo} | {s_['ready_prs']} | {p['app_opener']} | {p['provenance']} | {p['trailer']} | {p['session_stamp']} | {p['none']} | "
                     f"{sum(at['ambiguous_cleared'].values())} | {at['steward_merges_recognised']} | {at['owner_merges_unverified']} | {at['agent_stamped_owner_events']} | {lg['rate_pct']}% / {lg['unknown_share_pct']}% |")
        L.append("")
    if e2e.get("window"):
        L += ["### Proofs, macOS minutes and queue wait per merged PR (window)", "",
              "Runs per merged PR with a complete compute read, by level: `PR` is the PR's own heads, `group` its merge group (batches share one run), `push` pushes to the base. "
              "macOS minutes are job minutes on jobs labelled macOS. Enqueue to merge is the last enqueue before the merge.", "",
              "| repo | merged PRs read | workflow | runs/PR PR / group / push | macOS min mean / median | enqueue to merge median / p90 min (n) |", "|---|---|---|---|---|---|"]
        for repo, s_ in sorted(e2e["window"]["per_repo"].items()):
            c, q = s_["compute_per_merged_pr"], s_["merge_queue"]["enqueue_to_merge_min"]
            wf = c.get("by_workflow", {})
            names = [n for n in PROOF_WORKFLOWS.get(repo, ()) if n in wf] or sorted(wf, key=lambda n: -wf[n]["pull_request"])[:3]
            tail = f"{c['macos_min_mean']} / {c['macos_min_median']} | {q['median']} / {q['p90']} ({q['n']})"
            for i, n in enumerate(names or [None]):
                lv = wf.get(n, {})
                L.append(f"| {repo if i == 0 else ''} | {c['prs_read'] if i == 0 else ''} | {n or '-'} | "
                         f"{lv.get('pull_request', '-')} / {lv.get('merge_group', '-')} / {lv.get('push', '-')} | {tail if i == 0 else ''} |")
        L.append("")
    if e2e.get("window"):
        L += ["### Clean and slow paths (window, merged PRs)", "",
              "A merged PR is slow when it had any HOLD, a failed run on a head, an ejection from the queue or a DIRTY tick; clean when none, with compute read in full; "
              "otherwise unknown (a failed run cannot be ruled out). Lead time is first ready to merge. Cause minutes are per merged PR over the slow PRs, overlap, "
              "and are not additive; dirty is not measured.", "",
              "| repo | merged | slow share | clean median (n) | slow median / p90 (n) | top cause | min per merge: hold / red / queue / runner | unknown path |", "|---|---|---|---|---|---|---|---|"]
        for repo, s_ in sorted(e2e["window"]["per_repo"].items()):
            p = s_["paths"]
            cm = p["cause_min_per_merge"]
            L.append(f"| {repo} | {p['merged']} | {p['slow_share_pct']}% | {p['clean']['median_min']} ({p['clean']['n']}) | {p['slow']['median_min']} / {p['slow']['p90_min']} ({p['slow']['n']}) | "
                     f"{p['top_cause']} | {cm['hold']} / {cm['red']} / {cm['queue']} / {cm['runner']} | {p['unknown']} |")
        L.append("")
    if e2e.get("window"):
        L += ["### Change failures: merges that went to main red (window)", "",
              "A change failure is a merged PR whose own final head was red in a context and whose merge then turned that same context red on the base, until a later push turned it green "
              "(the push's PR is the fixer). Counts and raw minutes to green are printed, with no rate and no median. `Other post-merge red` are episodes the PR's merge started "
              "whose head was green in that context: base red that a merge may not have caused (a flake, an interaction, a post-merge job); they are listed apart and not counted as change failures. "
              "Recall is known incidents found over known incidents expected. Pushes are ordered by parents, pushes with unsettled checks are skipped, and contexts that flap or appear once are not tracked.", "",
              "| repo | merged | change failures | minutes to green (each) | unresolved | other post-merge red | head unknown | recall k/n | unknown attribution |", "|---|---|---|---|---|---|---|---|---|"]
        for repo, s_ in sorted(e2e["window"]["per_repo"].items()):
            cf, ctl = s_["change_failure"], e2e["repos"][repo]["controls"]
            inc = ctl.get("incident", {"found": 0, "expected": 0})
            off = "change_failure" in ctl["disabled"]
            W = "withheld"
            L.append(f"| {repo} | {cf['merged']} | {W if off else cf['change_failures']} | {W if off else ', '.join(str(x) for x in cf['minutes_to_green']) or '-'} | "
                     f"{W if off else cf['unresolved']} | {W if off else cf['other_post_merge_red']} | {W if off else cf['head_red_unknown']} | {inc['found']}/{inc['expected']} | {cf['unknown']} |")
        for repo, v in sorted(e2e["repos"].items()):
            u = (v.get("main_red") or {}).get("untracked_contexts")
            if u and (u["noisy"] or u["one_off"]):
                L.append(f"- {repo}: not tracked: noisy gates {', '.join(u['noisy']) or 'none'}; {len(u['one_off'])} one-off contexts")
        L.append("")
    if e2e.get("window"):
        L += ["### Merge-queue ejections (window, ready PRs)", "",
              "A removal from the queue other than the merge itself. The cause is GraphQL's `RemovedFromMergeQueueEvent.reason`, lowercased (the REST timeline names none); "
              "a PR whose cause could not be read stays `unknown` and is read again next run.", "",
              "| repo | ready PRs | ejections | per merged PR | top cause | by cause | cause unknown |", "|---|---|---|---|---|---|---|"]
        for repo, s_ in sorted(e2e["window"]["per_repo"].items()):
            q = s_["merge_queue"]
            by = ", ".join(f"{k2} {n}" for k2, n in q["ejections_by_cause"].items()) or "0"
            L.append(f"| {repo} | {s_['ready_prs']} | {sum(q['ejections_by_cause'].values())} | {q['ejections_per_merge']} | {q['top_ejection_cause']} | {by} | {q['ejections_cause_unknown']} |")
        L.append("")
    nf = (e2e.get("baseline") or {}).get("noise_floor")
    if nf:
        L += ["### Baseline noise floor (day against day, North Star rate)", ""]
        for repo, v in sorted(nf.items()):
            L.append(f"- {repo}: " + ", ".join(f"{d} {r}%" for d, r in v["north_star_rate_pct_by_day"].items()) + f"; spread {v['spread_pp']} points. A later change smaller than the spread is no change.")
        L.append("")
    return "\n".join(L) + "\n"
