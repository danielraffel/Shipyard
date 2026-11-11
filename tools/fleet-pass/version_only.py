#!/usr/bin/env python3
"""Version-bump-only diffs: a deterministic class and a consistency check.

Ruled 2026-11-11 (Fable): a diff that only moves version strings can be judged
by a script, in four steps, and this module is the first two.

1. ``classify``: a path-and-hunk allowlist. Every changed file must be one of
   ``ALLOWED`` and every changed line must match that file's version-line
   grammar (``LINE_RULES``). One extra file or one extra line routes the PR to
   the full reviewer. A path the base lists in ``[auto_merge]
   carry_excluded_paths`` is never in the class. Renames, mode changes,
   binary files and deletions are never in the class.
2. ``check``: the consistency rule. The surfaces and version files come from
   the base's ``versioning.json`` (``tools/scripts/`` or ``scripts/``, the
   configuration ``version_bump_check.py`` reads), never from the head. For each surface
   every version file must agree at the head, and a surface that moved must be
   strictly ahead of the base. A release heading added to a surface's
   changelog must name a version that has been cut: newer than every release
   the base's changelog lists and no newer than the version the base carried
   (a bump moves the version files on to the next version). A link reference
   must name a heading the diff adds. Anything unreadable is a HOLD.

Until it has run in shadow beside the LLM reviewer on ten version-only pull
requests and agreed on all ten, this module decides nothing: ``shadow``
appends its verdict to a log and ``shadow-report`` compares that log with the
counted verdicts. Its eventual verdict is an exact-head APPROVE posted by the
scheduled reviewer session with the text ``diff-class: version-only, check
<sha>``; that wiring is not here.

    version_only.py classify --git-dir D --base B --head H [--excluded GLOB ...]
    version_only.py check    --git-dir D --base B --head H
    version_only.py shadow   --git-dir D --log FILE --repo R < must-review-lines
    version_only.py shadow-report --log FILE --verdicts FILE

``--base`` is the main commit the head is compared with (the merge base is
taken from it). Output is one JSON object per call.
"""
from __future__ import annotations

import argparse
import fnmatch
import json
import os
import re
import subprocess
import sys
import time
from typing import Iterable

SEMVER = r"\d+\.\d+\.\d+"
ALLOWED = ("CMakeLists.txt", "CHANGELOG.md", ".claude-plugin/plugin.json",
           ".claude-plugin/marketplace.json", "tools/shipyard.toml")
LINE_RULES = {
    "CMakeLists.txt": [re.compile(rf"^[ \t]*VERSION {SEMVER}[ \t]*$")],
    ".claude-plugin/plugin.json": [re.compile(rf'^[ \t]*"version": "{SEMVER}",?[ \t]*$')],
    ".claude-plugin/marketplace.json": [re.compile(rf'^[ \t]*"version": "{SEMVER}",?[ \t]*$')],
    "tools/shipyard.toml": [re.compile(rf'^version = "v{SEMVER}"[ \t]*$')],
}
# CHANGELOG.md may only gain ONE release block, inserted directly above the base's newest
# block, plus ONE link reference inserted directly above the base's newest link reference.
# The block is: anchor, heading, blank, one or more bullets, blank. A bullet is a pull
# request title (no links, markup or URLs of its own) ending in that repository's own pull
# request link.
ANCHOR = re.compile(r'^<a id="v(\d+)"></a>$')
BULLET_TEXT = re.compile(r"^[^\[\]<>`]{1,200}$")
HEADING = re.compile(rf"^## \[({SEMVER})\] - ")
LINKREF = re.compile(rf"^\[({SEMVER})\]: https://github\.com/([\w.-]+/[\w.-]+)/releases/tag/v({SEMVER})$")
BULLET = re.compile(r"^- (.+) \(\[#(\d+)\]\(https://github\.com/([\w.-]+/[\w.-]+)/pull/(\d+)\)\)$")


def changelog_insert(fork_text: str, head_text: str, repo: str | None) -> tuple:
    """(version, None) when head = fork + one release block at the top + its link ref,
    else (None, why). ``repo`` pins the pull request and tag links (None: the repository
    the base's own newest link reference names)."""
    f, h = fork_text.split("\n"), head_text.split("\n")
    i = next((k for k, l in enumerate(f) if ANCHOR.match(l) or HEADING.match(l)), None)
    j = next((k for k, l in enumerate(f) if LINKREF.match(l)), None)
    if i is None or j is None or j < i:
        return None, "the base changelog has no release block and link list to anchor on"
    if repo is None:
        repo = LINKREF.match(f[j]).group(2)
    n_block = len(h) - len(f) - 1
    if n_block < 5:
        return None, "the changelog does not gain exactly one release block and one link"
    block = h[i:i + n_block]
    rest = h[:i] + h[i + n_block:]
    ref = rest[j]
    if rest[:j] + rest[j + 1:] != f:
        return None, "the changelog changes something besides one new block and one link"
    a, head, blank = ANCHOR.match(block[0]), HEADING.match(block[1]), block[2]
    if not (a and head and blank == "" and block[-1] == ""):
        return None, "the new block is not anchor, heading, blank, bullets, blank"
    ver = head.group(1)
    if a.group(1) != ver.replace(".", ""):
        return None, f"anchor v{a.group(1)} does not name {ver}"
    bullets = block[3:-1]
    if not bullets:
        return None, "the new block lists no pull requests"
    for line in bullets:
        m = BULLET.match(line)
        if not (m and BULLET_TEXT.match(m.group(1)) and "http" not in m.group(1).lower()
                and m.group(2) == m.group(4) and m.group(3) == repo):
            return None, f"bullet is not a plain title with this repository's pull link: {line[:80]!r}"
    r = LINKREF.match(ref)
    if not (r and r.group(1) == ver == r.group(3) and r.group(2) == repo):
        return None, f"the new link reference does not name {ver} in {repo}"
    return ver, None
# Where version_bump_check.py reads its surfaces: Pulp, then Shipyard's layout.
CONFIGS = ("tools/scripts/versioning.json", "scripts/versioning.json")
CMAKE_PROJECT_VERSION = re.compile(r"project\s*\(\s*\w+[^)]*?\bVERSION\s+(" + SEMVER + r")", re.S)


def _run(argv: list) -> subprocess.CompletedProcess:
    # Bytes, decoded without newline translation, so a CR in a file stays visible to the rules.
    env = dict(os.environ, GIT_TERMINAL_PROMPT="0",
               GIT_SSH_COMMAND="ssh -o BatchMode=yes -o ConnectTimeout=20")
    p = subprocess.run(argv, capture_output=True, env=env)
    p.stdout = p.stdout.decode("utf-8", "surrogateescape")
    p.stderr = p.stderr.decode("utf-8", "replace")
    return p


def git(git_dir: str, *args: str) -> str:
    out = _run(["git", "--git-dir", git_dir, *args])
    if out.returncode:
        raise RuntimeError(f"git {' '.join(args[:3])}: {out.stderr.strip()[:200]}")
    return out.stdout


def show(git_dir: str, rev: str, path: str) -> str | None:
    out = _run(["git", "--git-dir", git_dir, "show", f"{rev}:{path}"])
    return out.stdout if out.returncode == 0 else None


def _glob(path: str, pat: str) -> bool:
    if pat.endswith("/**"):
        return path == pat[:-3] or path.startswith(pat[:-2])
    return fnmatch.fnmatchcase(path, pat) or ("**/" in pat and fnmatch.fnmatchcase(path, pat.replace("**/", "")))


# --------------------------------------------------------------------- classify
def base_carry_excluded(git_dir: str, base: str) -> list[str]:
    """``[auto_merge] carry_excluded_paths`` (+ ``_extra``) from the BASE's config."""
    text = show(git_dir, base, ".shipyard/config.toml")
    if text is None:
        return []
    try:
        import tomllib
        am = tomllib.loads(text).get("auto_merge") or {}
    except Exception:  # noqa: BLE001 - no tomllib (Python < 3.11) or unreadable policy
        return ["**"]  # nothing is in the class
    return [str(g) for g in (am.get("carry_excluded_paths") or []) + (am.get("carry_excluded_paths_extra") or [])]


def classify(git_dir: str, base: str, head: str, excluded: Iterable[str] | None = None,
             repo: str | None = None) -> dict:
    if excluded is None:
        excluded = base_carry_excluded(git_dir, base)
    fork = git(git_dir, "merge-base", base, head).strip()
    raw = git(git_dir, "diff", "--no-color", "--no-renames", "--raw", "-z", fork, head)
    fields = raw.split("\0")
    files = []
    i = 0
    while i < len(fields) - 1:
        meta, path = fields[i], fields[i + 1]
        i += 2
        if not meta.startswith(":"):
            break
        old_mode, new_mode, _, _, status = meta[1:].split()
        files.append((path, old_mode, new_mode, status))
    if not files:
        return {"class": "other", "reason": "empty diff"}
    for path, old_mode, new_mode, status in files:
        if path not in ALLOWED:
            return {"class": "other", "reason": f"{path} is not a version file"}
        if any(g == "**" or _glob(path, g) for g in excluded):
            return {"class": "other", "reason": f"{path} is carry-excluded"}
        if status != "M" or old_mode != new_mode:
            return {"class": "other", "reason": f"{path}: status {status}, mode {old_mode}->{new_mode}"}
    diff = git(git_dir, "diff", "--no-color", "--no-renames", "--no-ext-diff", "-U0", fork, head)
    current = None
    for line in diff.split("\n"):
        if line.startswith("diff --git "):
            current = None
            continue
        if line.startswith("+++ "):
            current = line[6:] if line.startswith("+++ b/") else None
            continue
        if line.startswith(("--- ", "@@", "index ")):
            continue
        if line.startswith("Binary files"):
            return {"class": "other", "reason": "binary change"}
        if not line or line[0] not in "+-":
            continue
        if current is None:
            return {"class": "other", "reason": "unparseable diff"}
        sign, text = line[0], line[1:]
        if current == "CHANGELOG.md":
            continue  # judged as a whole below
        if not any(r.match(text) for r in LINE_RULES[current]):
            return {"class": "other", "reason": f"{current} line is not a version line: {text[:80]!r}"}
    if any(p == "CHANGELOG.md" for p, *_ in files):
        _, why = changelog_insert(show(git_dir, fork, "CHANGELOG.md") or "",
                                  show(git_dir, head, "CHANGELOG.md") or "", repo)
        if why:
            return {"class": "other", "reason": f"CHANGELOG.md: {why}"}
    return {"class": "version-only", "files": sorted(p for p, *_ in files)}


# ------------------------------------------------------------------------ check
def _key(v: str) -> tuple:
    return tuple(int(x) for x in v.split("."))


def _fmt(k) -> str:
    return ".".join(map(str, k)) if k else "-"


def read_version(text: str | None, vf: dict) -> str | None:
    if text is None:
        return None
    kind = vf.get("kind")
    if kind == "cmake_project_version":
        m = CMAKE_PROJECT_VERSION.search(text)
        return m.group(1) if m else None
    if kind in ("json_field", "json_path"):
        try:
            node = json.loads(text)
        except json.JSONDecodeError:
            return None
        for part in str(vf.get("field", "")).split("."):
            if isinstance(node, list) and part.isdigit() and int(part) < len(node):
                node = node[int(part)]
            elif isinstance(node, dict) and part in node:
                node = node[part]
            else:
                return None
        return node if isinstance(node, str) and re.fullmatch(SEMVER, node) else None
    if kind == "regex" and vf.get("pattern"):
        try:
            m = re.search(vf["pattern"], text)
        except re.error:
            return None
        v = m.group(1) if m and m.groups() else None
        return v if v and re.fullmatch(SEMVER, v) else None
    return None


def _next(old: str, new: str) -> bool:
    """``new`` is the next patch, minor or major release after ``old``."""
    o, n = _key(old), _key(new)
    return n in ((o[0], o[1], o[2] + 1), (o[0], o[1] + 1, 0), (o[0] + 1, 0, 0))


def check(git_dir: str, base: str, head: str, repo: str | None = None) -> dict:
    fork = git(git_dir, "merge-base", base, head).strip()
    cfg_text = next((t for t in (show(git_dir, base, c) for c in CONFIGS) if t is not None), None)
    if cfg_text is None:
        return {"check": "HOLD", "reasons": ["the base has no versioning.json"]}
    try:
        surfaces = json.loads(cfg_text)["surfaces"]
    except (ValueError, KeyError, TypeError):
        return {"check": "HOLD", "reasons": ["the base's versioning.json is unreadable"]}
    changed = {p for p in git(git_dir, "diff", "--no-renames", "--name-only", "-z", fork, head).split("\0") if p}
    reasons, moved, fork_versions = [], {}, {}
    owned: set = set()
    changelogs: dict = {}
    for name, s in surfaces.items():
        vfs = s.get("version_files") or []
        owned |= {vf["path"] for vf in vfs}
        if s.get("changelog"):
            owned.add(s["changelog"])
            changelogs.setdefault(s["changelog"], set()).add(name)
        if not vfs:
            continue
        at_head = [read_version(show(git_dir, head, vf["path"]), vf) for vf in vfs]
        at_fork = [read_version(show(git_dir, fork, vf["path"]), vf) for vf in vfs]
        touched = any(vf["path"] in changed for vf in vfs)
        if None in at_head or len(set(at_head)) != 1:
            if touched or None not in at_head:
                reasons.append(f"{name}: version files unreadable or disagreeing at the head: {at_head}")
            continue
        if None in at_fork or len(set(at_fork)) != 1:
            if touched:
                reasons.append(f"{name}: version files unreadable or disagreeing at the base: {at_fork}")
            continue
        old, new = at_fork[0], at_head[0]
        fork_versions[name] = old
        if new != old:
            if not _next(old, new):
                reasons.append(f"{name}: {old} -> {new} is not the next patch, minor or major release")
            else:
                moved[name] = (old, new)
    for path in sorted(changed - owned):
        reasons.append(f"{path} belongs to no versioning.json surface, so nothing checks it")
    for path, owners in changelogs.items():
        if path not in changed:
            continue
        # The one new block names the version being CUT: the version the base carried for a
        # surface owning this changelog (its files move on to the next version; main carried
        # CMake 1.88.0 while the changelog's top block was 1.87.0). The 11747 HOLDs read that
        # gap as a mismatch and the same reviewer later withdrew them.
        ver, why = changelog_insert(show(git_dir, fork, path) or "", show(git_dir, head, path) or "", repo)
        if why:
            reasons.append(f"{path}: {why}")
            continue
        carried = {fork_versions[n] for n in owners if n in fork_versions}
        if ver not in carried:
            reasons.append(f"{path} releases {ver}, but the base carried {sorted(carried) or 'no version'}")
    return {"check": "HOLD" if reasons else "APPROVE", "reasons": reasons,
            "moved": {n: f"{a}->{b}" for n, (a, b) in moved.items()}}


# ------------------------------------------------------------------------ shadow
def shadow(git_dir: str, log: str, repo: str, lines: Iterable[str], base: str = "origin/main",
           excluded: Iterable[str] | None = None) -> list[dict]:
    """Record a script verdict for each version-only entry. Decides nothing."""
    out, done = [], set()
    try:
        with open(log) as f:
            done = {(r.get("pr"), r.get("head")) for r in map(json.loads, f) if isinstance(r, dict)}
    except (OSError, ValueError):
        pass
    for line in lines:
        parts = line.split()
        if len(parts) < 2 or not parts[0].startswith(repo + "#"):
            continue
        head = parts[1]
        if (parts[0], head) in done:
            continue
        try:
            c = classify(git_dir, base, head, excluded, repo)
            if c["class"] != "version-only":
                continue
            rec = {"at": int(time.time()), "pr": parts[0], "head": head,
                   "base": git(git_dir, "rev-parse", base).strip(), **check(git_dir, base, head, repo)}
        except Exception as e:  # noqa: BLE001 - one bad entry must not cost the batch
            # Not recorded: the mirror may not have the head yet; the next pass retries it.
            print(f"version_only shadow {parts[0]} {head[:12]}: {type(e).__name__}: {e}", file=sys.stderr)
            continue
        with open(log, "a") as f:
            f.write(json.dumps(rec, sort_keys=True) + "\n")
        done.add((parts[0], head))
        out.append(rec)
    return out


def shadow_report(log: str, verdicts: dict) -> dict:
    """Compare logged script verdicts with counted verdicts ``{"<pr> <head>": "APPROVE"|"HOLD"}``.
    The class may decide only once ``agree >= 10`` and ``disagree == 0``."""
    seen, agree, disagree, pending = {}, 0, [], []
    with open(log) as f:
        for raw in f:
            r = json.loads(raw)
            seen[f"{r['pr']} {r['head']}"] = r["check"]
    for key, script in seen.items():
        human = verdicts.get(key)
        if human is None:
            pending.append(key)
        elif human == script:
            agree += 1
        else:
            disagree.append({"entry": key, "script": script, "reviewer": human})
    return {"agree": agree, "disagree": disagree, "pending": pending,
            "may_decide": agree >= 10 and not disagree}


def counted_verdicts(log: str) -> dict:
    """The counted verdict at each logged head, read the way the reviewer queue reads them."""
    import importlib.util
    import os
    spec = importlib.util.spec_from_file_location(
        "must_review", os.path.join(os.path.dirname(os.path.abspath(__file__)), "must-review.py"))
    mr = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mr)
    out = {}
    with open(log) as f:
        for raw in f:
            r = json.loads(raw)
            repo, n = r["pr"].split("#")
            v, _ = mr.verdict_at(mr.comments(repo, int(n)), r["head"])
            if v:
                out[f"{r['pr']} {r['head']}"] = v
    return out


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("classify", "check"):
        p = sub.add_parser(name)
        p.add_argument("--git-dir", required=True)
        p.add_argument("--base", required=True)
        p.add_argument("--head", required=True)
        p.add_argument("--excluded", nargs="*", default=None)
        p.add_argument("--repo", default=None, help="owner/name the changelog links must name")
    p = sub.add_parser("shadow")
    p.add_argument("--git-dir", required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--repo", required=True)
    p.add_argument("--base", default="origin/main")
    p.add_argument("--excluded", nargs="*", default=None)
    p = sub.add_parser("shadow-report")
    p.add_argument("--log", required=True)
    p.add_argument("--verdicts", help="JSON {\"<repo>#<n> <head>\": verdict}; default: read the "
                   "counted verdicts with must-review.py (GitHub reads)")
    a = ap.parse_args(argv)
    if a.cmd == "classify":
        res = classify(a.git_dir, a.base, a.head, a.excluded, a.repo)
    elif a.cmd == "check":
        res = check(a.git_dir, a.base, a.head, a.repo)
    elif a.cmd == "shadow":
        res = shadow(a.git_dir, a.log, a.repo, sys.stdin.read().splitlines(), a.base, a.excluded)
    else:
        res = shadow_report(a.log, json.load(open(a.verdicts)) if a.verdicts else counted_verdicts(a.log))
    print(json.dumps(res, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
