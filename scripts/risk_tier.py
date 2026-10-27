#!/usr/bin/env python3
"""Post the ``risk-tier`` commit status on a pull request's head.

The tier says how much review a pull request needs before it lands (the fleet
plan of record, section 4):

- **Low**: everything not below (docs, tests, skill text, internal fixes).
  Needs CI only.
- **Moderate**: a minor or major version bump by the base's versioning
  heuristic; steward, queue, landing or daemon code; bumping an existing
  dependency; more than ``max_changed_lines`` lines or ``max_changed_files``
  files; label ``risk:moderate``. Would need one approving review from the
  review App on the exact head.
- **High**: workflows, the Shipyard and agent contracts, ``tools/ci``, guard
  and arm code, signing and notarization, the installer, release scripts,
  auth and token code, a new dependency or package source; label
  ``risk:high``. Would need two review-App approvals, and an owner approval
  when it touches the owner subset (workflows, credentials, signing, new
  supply-chain sources).

Labels raise a tier, never lower it. Path lists come from the BASE branch's
``.shipyard/config.toml`` (``[review.tiers]``) with the built-in defaults
below; nothing is read from the head except the dependency manifests and
lockfiles it changes, which are parsed as data and never run.

The status is REPORT-ONLY: it is always ``success`` and its description says
the tier and what that tier would require. A ``GITHUB_TOKEN`` status is not
proof of who posted it (any workflow in the repository can post one), so
before a repository makes ``risk-tier`` required, the status must come from a
separate App whose key no pull request job holds (see docs/risk-tier.md).

The script is self-contained (standard library, Python 3.11+, ``gh`` on
``PATH``) so other repositories can copy it with the workflow.

Usage:

- In a GitHub Actions job triggered by ``pull_request_target``: no arguments;
  reads ``GITHUB_EVENT_PATH``, ``GITHUB_REPOSITORY`` and ``GH_TOKEN``.
- Locally: ``risk_tier.py --repo OWNER/NAME --pr N [--gh ghapp] [--json]``.
  Prints the assessment; posts nothing unless ``--post`` is given.
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import os
import pathlib
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass, field
from typing import Any
from urllib.parse import quote

CONTEXT = "risk-tier"
CHECK_NAME = "risk-tier report"
# GitHub refuses a status description over 140 characters.
DESCRIPTION_LIMIT = 140
CONFIG_PATH = ".shipyard/config.toml"
VERSIONING_CANDIDATES = ("scripts/versioning.json", "tools/scripts/versioning.json")

LOW, MODERATE, HIGH = "low", "moderate", "high"
TIERS = (LOW, MODERATE, HIGH)
LABELS = {"risk:moderate": MODERATE, "risk:high": HIGH}
REQUIRES = {
    LOW: "needs CI only",
    MODERATE: "would require 1 review-App approval",
    HIGH: "would require 2 review-App approvals",
}

# Paths whose change makes a pull request High. ``**`` is any number of whole
# path segments, ``*`` any run within one segment (src/head_approval.rs
# ``glob_match``). Guard and arm code is the carry list's
# (src/head_approval.rs PROTECTED_CARRY_PATHS).
DEFAULT_HIGH_PATHS = (
    ".github/workflows/**",
    ".github/actions/**",
    ".github/CODEOWNERS",
    ".shipyard/config.toml",
    ".agents/contract.toml",
    "tools/ci/**",
    "scripts/ghapp*",
    "scripts/*_guard.py",
    "scripts/*_guard.sh",
    "scripts/install-guarded-gh.sh",
    "src/head_approval*",
    "src/head_approval/**",
    "src/queue_arm_gate*",
    "src/queue_arm_gate/**",
    "src/auto_arm*",
    "src/auto_arm/**",
    "src/ghapp_guards*",
    "src/fleet_guards*",
    "src/fleet_guards/**",
    "src/app/guards_cmd*",
    "src/app/ship_cmd/auto_arm*",
    "src/app/merge_steward_cmd/native_arm*",
    "**/*codesign*",
    "**/*notariz*",
    "**/*signing*",
    "install.sh",
    "**/install.sh",
    "**/install.ps1",
    "**/release*.sh",
    "release-please-config.json",
    ".release-please-manifest.json",
    "**/auth_*",
    "**/*_auth.*",
    "**/auth.*",
    "**/auth/**",
    "**/*token*",
    "**/*credential*",
    "**/*secret*",
)
# High paths no base config can drop: the rule's own inputs.
PROTECTED_HIGH_PATHS = (".github/workflows/**", ".shipyard/config.toml")

# Paths whose change makes a pull request at least Moderate: steward, queue,
# landing and daemon code.
DEFAULT_MODERATE_PATHS = tuple(
    pattern
    for name in ("steward", "queue", "landing", "landability", "merge_train",
                 "merge_carrier", "daemon")
    for pattern in (f"src/**/*{name}*", f"src/**/*{name}*/**")
)

# The High subset that would also need the owner's approval: workflows,
# credential and signing code. A new dependency or package source is in it
# too (see ``assess``).
DEFAULT_OWNER_PATHS = (
    ".github/workflows/**",
    ".github/actions/**",
    "**/*codesign*",
    "**/*notariz*",
    "**/*signing*",
    "**/auth_*",
    "**/*_auth.*",
    "**/auth.*",
    "**/auth/**",
    "**/*token*",
    "**/*credential*",
    "**/*secret*",
)

# Lockfiles say nothing about review size; their lines are not counted.
DEFAULT_SIZE_EXCLUDED_PATHS = (
    "**/Cargo.lock",
    "**/uv.lock",
    "**/package-lock.json",
    "**/yarn.lock",
    "**/pnpm-lock.yaml",
)

DEFAULT_MAX_LINES = 400
DEFAULT_MAX_FILES = 15


class TierError(Exception):
    """The pull request cannot be read well enough to classify."""


# ── Rules ───────────────────────────────────────────────────────────────


@dataclass
class Rules:
    high_paths: list[str] = field(default_factory=lambda: list(DEFAULT_HIGH_PATHS))
    moderate_paths: list[str] = field(default_factory=lambda: list(DEFAULT_MODERATE_PATHS))
    owner_paths: list[str] = field(default_factory=lambda: list(DEFAULT_OWNER_PATHS))
    size_excluded_paths: list[str] = field(
        default_factory=lambda: list(DEFAULT_SIZE_EXCLUDED_PATHS))
    max_lines: int = DEFAULT_MAX_LINES
    max_files: int = DEFAULT_MAX_FILES
    versioning_config: str | None = None


def _globs(table: dict[str, Any], key: str, default: list[str]) -> list[str]:
    """``key`` replaces ``default``; ``key_extra`` adds to whichever applies."""
    def read(name: str) -> list[str] | None:
        value = table.get(name)
        if value is None:
            return None
        if not isinstance(value, list) or not all(isinstance(v, str) for v in value):
            raise TierError(f"[review.tiers] {name} must be a list of strings")
        return value
    replace = read(key)
    extra = read(f"{key}_extra") or []
    return list(replace if replace is not None else default) + extra


def load_rules(config_text: str | None) -> Rules:
    """Rules from the base branch's ``.shipyard/config.toml`` text (or None)."""
    rules = Rules()
    if not config_text:
        return rules
    try:
        config = tomllib.loads(config_text)
    except tomllib.TOMLDecodeError as error:
        raise TierError(f"the base {CONFIG_PATH} is not valid TOML: {error}") from error
    table = (config.get("review") or {}).get("tiers") or {}
    if not isinstance(table, dict):
        raise TierError("[review.tiers] must be a table")
    rules.high_paths = _globs(table, "high_paths", rules.high_paths)
    for path in PROTECTED_HIGH_PATHS:
        if path not in rules.high_paths:
            rules.high_paths.append(path)
    rules.moderate_paths = _globs(table, "moderate_paths", rules.moderate_paths)
    rules.owner_paths = _globs(table, "owner_paths", rules.owner_paths)
    rules.size_excluded_paths = _globs(table, "size_excluded_paths", rules.size_excluded_paths)
    for key, attr in (("max_changed_lines", "max_lines"), ("max_changed_files", "max_files")):
        value = table.get(key)
        if value is not None:
            if not isinstance(value, int) or isinstance(value, bool) or value < 0:
                raise TierError(f"[review.tiers] {key} must be a non-negative integer")
            setattr(rules, attr, value)
    versioning = table.get("versioning_config")
    if versioning is not None:
        if not isinstance(versioning, str):
            raise TierError("[review.tiers] versioning_config must be a path")
        rules.versioning_config = versioning
    return rules


def glob_match(pattern: str, path: str) -> bool:
    """``**`` is any number of whole path segments, ``*`` any run of
    characters within one segment, ``?`` one. Same as
    scripts/ghapp_queue_arm_guard.py ``glob_match``."""
    def segments(pats: list[str], parts: list[str]) -> bool:
        if not pats:
            return not parts
        if pats[0] == "**":
            return any(segments(pats[1:], parts[index:]) for index in range(len(parts) + 1))
        return (bool(parts) and fnmatch.fnmatchcase(parts[0], pats[0].replace("[", "[[]"))
                and segments(pats[1:], parts[1:]))

    return segments(pattern.split("/"), path.split("/"))


def first_match(path: str, patterns: list[str] | tuple[str, ...]) -> str | None:
    return next((pattern for pattern in patterns if glob_match(pattern, path)), None)


# ── Dependencies ────────────────────────────────────────────────────────

# A dependency map is name -> (requirement, source). A lock map is
# name -> {(version, source)}.
DepMap = dict[str, tuple[str, str]]
LockMap = dict[str, set[tuple[str, str]]]

CARGO_DEP_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


def _cargo_entry(value: Any) -> tuple[str, str]:
    if isinstance(value, str):
        return value, "registry"
    if not isinstance(value, dict):
        return str(value), "registry"
    version = str(value.get("version", ""))
    if value.get("workspace") is True:
        return version, "workspace"
    for key in ("git", "path", "registry", "registry-index"):
        if key in value:
            return version, f"{key}={value[key]}"
    return version, "registry"


def cargo_manifest_deps(text: str | None) -> DepMap:
    """Every dependency a ``Cargo.toml`` declares, keyed by table and name."""
    if not text:
        return {}
    data = tomllib.loads(text)
    out: DepMap = {}

    def collect(prefix: str, holder: dict[str, Any]) -> None:
        for table in CARGO_DEP_TABLES:
            for name, value in (holder.get(table) or {}).items():
                package = value.get("package", name) if isinstance(value, dict) else name
                out[f"{prefix}{table}:{package}"] = _cargo_entry(value)

    collect("", data)
    for target, holder in (data.get("target") or {}).items():
        if isinstance(holder, dict):
            collect(f"target.{target}.", holder)
    workspace = data.get("workspace") or {}
    for name, value in (workspace.get("dependencies") or {}).items():
        out[f"workspace.dependencies:{name}"] = _cargo_entry(value)
    for registry, value in (data.get("patch") or {}).items():
        for name, entry in (value or {}).items():
            out[f"patch.{registry}:{name}"] = _cargo_entry(entry)
    return out


def _lock_packages(text: str | None, source_of) -> LockMap:
    if not text:
        return {}
    data = tomllib.loads(text)
    out: LockMap = {}
    for package in data.get("package") or []:
        if isinstance(package, dict) and isinstance(package.get("name"), str):
            out.setdefault(package["name"], set()).add(
                (str(package.get("version", "")), source_of(package)))
    return out


def cargo_lock_packages(text: str | None) -> LockMap:
    def source(package: dict[str, Any]) -> str:
        raw = str(package.get("source") or "local")
        # A git source pins a commit after '#'; the source is the URL.
        return raw.split("#", 1)[0].split("?", 1)[0]
    return _lock_packages(text, source)


def uv_lock_packages(text: str | None) -> LockMap:
    def source(package: dict[str, Any]) -> str:
        raw = package.get("source") or {}
        if not isinstance(raw, dict) or not raw:
            return "local"
        kind, value = sorted(raw.items())[0]
        value = str(value).split("#", 1)[0].split("?", 1)[0]
        return "local" if kind in ("editable", "virtual", "directory") else f"{kind}={value}"
    return _lock_packages(text, source)


_PEP508_NAME = re.compile(r"^\s*([A-Za-z0-9][A-Za-z0-9._-]*)\s*(\[[^\]]*\])?\s*(.*)$")


def _pep508(requirement: str) -> tuple[str, str] | None:
    match = _PEP508_NAME.match(requirement)
    if not match:
        return None
    return re.sub(r"[-_.]+", "-", match.group(1)).lower(), match.group(3).strip()


def pyproject_deps(text: str | None) -> DepMap:
    """Every requirement and package source a ``pyproject.toml`` declares."""
    if not text:
        return {}
    data = tomllib.loads(text)
    out: DepMap = {}

    def add(group: str, requirements: Any) -> None:
        for requirement in requirements or []:
            if not isinstance(requirement, str):
                continue
            parsed = _pep508(requirement)
            if parsed is None:
                continue
            name, spec = parsed
            source = "registry"
            if "@" in requirement.split(";", 1)[0]:
                source = "url=" + requirement.split("@", 1)[1].split(";", 1)[0].strip()
            out[f"{group}:{name}"] = (spec, source)

    project = data.get("project") or {}
    add("project", project.get("dependencies"))
    for extra, requirements in (project.get("optional-dependencies") or {}).items():
        add(f"extra.{extra}", requirements)
    for group, requirements in (data.get("dependency-groups") or {}).items():
        add(f"group.{group}", [r for r in requirements or [] if isinstance(r, str)])
    add("build-system", (data.get("build-system") or {}).get("requires"))
    uv = (data.get("tool") or {}).get("uv") or {}
    add("tool.uv.dev", uv.get("dev-dependencies"))
    # A uv source or index redirects where a package comes from: always a
    # source, never a version.
    for name, value in (uv.get("sources") or {}).items():
        out[f"source:{name}"] = ("", json.dumps(value, sort_keys=True))
    for index in uv.get("index") or []:
        if isinstance(index, dict):
            out[f"index:{index.get('name') or index.get('url')}"] = ("", str(index.get("url")))
    return out


@dataclass
class DepFinding:
    tier: str
    reason: str
    owner: bool = False


def diff_manifest(path: str, base: DepMap, head: DepMap) -> list[DepFinding]:
    findings: list[DepFinding] = []
    for key, (spec, source) in sorted(head.items()):
        name = key.split(":", 1)[1]
        if key not in base:
            findings.append(DepFinding(HIGH, f"new dependency {name} ({path})", owner=True))
        elif base[key][1] != source:
            findings.append(DepFinding(HIGH, f"new source for {name} ({path})", owner=True))
        elif base[key][0] != spec:
            findings.append(DepFinding(MODERATE, f"bumps {name} ({path})"))
    return findings


def _named(names: list[str], limit: int = 3) -> str:
    shown = ", ".join(names[:limit])
    return shown + (f" +{len(names) - limit}" if len(names) > limit else "")


def diff_lock(path: str, base: LockMap, head: LockMap) -> list[DepFinding]:
    """New sources, new packages and bumped packages in a lockfile, one
    finding each (a lockfile change can list dozens)."""
    findings: list[DepFinding] = []
    base_sources = {source for entries in base.values() for _, source in entries}
    new_sources = sorted({source for entries in head.values() for _, source in entries
                          if source not in base_sources and source != "local"})
    for source in new_sources:
        findings.append(DepFinding(HIGH, f"new package source {source} ({path})", owner=True))
    added: list[str] = []
    bumped: list[str] = []
    for name in sorted(head):
        if all(source == "local" for _, source in head[name]):
            # A crate or package of the repository itself, not a dependency.
            continue
        if name not in base:
            added.append(name)
        elif {v for v, _ in head[name]} != {v for v, _ in base[name]}:
            bumped.append(name)
    if added:
        findings.append(DepFinding(
            HIGH, f"{len(added)} new package{'s' * (len(added) > 1)} in {path}: "
            f"{_named(added)}", owner=True))
    if bumped:
        findings.append(DepFinding(
            MODERATE, f"bumps {len(bumped)} in {path}: {_named(bumped)}"))
    return findings


DEPENDENCY_FILES = {
    "Cargo.toml": (cargo_manifest_deps, diff_manifest),
    "pyproject.toml": (pyproject_deps, diff_manifest),
    "Cargo.lock": (cargo_lock_packages, diff_lock),
    "uv.lock": (uv_lock_packages, diff_lock),
}


def dependency_findings(path: str, base_text: str | None, head_text: str | None
                        ) -> list[DepFinding]:
    """What a change to a dependency file adds: [] for any other file."""
    kind = DEPENDENCY_FILES.get(path.rsplit("/", 1)[-1])
    if kind is None:
        return []
    parse, diff = kind
    try:
        return diff(path, parse(base_text), parse(head_text))
    except (tomllib.TOMLDecodeError, AttributeError, TypeError) as error:
        return [DepFinding(HIGH, f"{path} is unreadable ({type(error).__name__})", owner=True)]


# ── Version bump (the base's versioning.json heuristic) ─────────────────

LEVELS = ("none", "patch", "minor", "major")


def classify_conventional(subject: str) -> str:
    """Same as scripts/version_bump_check.py ``classify_conventional``."""
    s = subject.strip()
    if re.search(r"\bBREAKING\b", s):
        return "major"
    if re.match(r"^[a-zA-Z]+(\([^)]*\))?!:", s):
        return "major"
    match = re.match(r"^([a-zA-Z]+)(\([^)]*\))?:", s)
    if not match:
        return "none"
    kind = match.group(1).lower()
    if kind == "feat":
        return "minor"
    if kind in ("fix", "perf"):
        return "patch"
    return "none"


def _vb_match(path: str, patterns: list[str]) -> bool:
    """version_bump_check's own matching (fnmatch, ``*`` crosses ``/``)."""
    for pattern in patterns:
        if fnmatch.fnmatchcase(path, pattern):
            return True
        if pattern.startswith("**/") and fnmatch.fnmatchcase(path, pattern[3:]):
            return True
    return False


def meaningful_patch(patch: str | None) -> bool:
    """Whether a file's patch changes more than whitespace or comments.

    version_bump_check reads ``git diff --ignore-all-space``; the API patch is
    the plain diff, so a whitespace-only change counts here. A file with no
    patch (binary or too large) counts."""
    if patch is None:
        return True
    for line in patch.splitlines():
        if not line.startswith(("+", "-")) or line.startswith(("+++", "---")):
            continue
        body = line[1:].strip()
        if body and not body.startswith(("//", "#", "/*", "*/", "*")):
            return True
    return False


def version_level(versioning: dict[str, Any] | None, files: list[dict[str, Any]],
                  commits: list[str]) -> tuple[str, str | None]:
    """``(level, surface)``: the highest bump the base's heuristic asks for.

    A surface whose trigger paths the pull request touches is ``minor`` when
    a public-API path has a meaningful diff, else ``patch``; a ``feat:``,
    ``BREAKING`` or ``!:`` commit subject, or a ``Version-Bump:`` trailer,
    raises it. Nothing lowers it: a ``skip`` trailer is ignored here."""
    if not versioning:
        return "none", None
    generated = list(versioning.get("generated_globs") or [])
    # The version files themselves are the bump (version-at-land writes
    # them), not a change that asks for one: version_bump_check's
    # version_file_paths.
    version_files = {vf.get("path") for surface in (versioning.get("surfaces") or {}).values()
                     if isinstance(surface, dict)
                     for vf in surface.get("version_files") or [] if isinstance(vf, dict)}
    if "Cargo.toml" in version_files:
        version_files.add("Cargo.lock")
    changed = [f for f in files if not _vb_match(f["filename"], generated)
               and f["filename"] not in version_files]
    trailer_key = ((versioning.get("trailers") or {}).get("version_bump")
                   or "Version-Bump").lower()
    subjects = []
    trailers: list[str] = []
    for message in commits:
        subject = message.split("\n", 1)[0]
        lines = message.splitlines()
        if subject.lower().startswith("revert") or any(
                line.lower().startswith("revert-of:") for line in lines):
            continue
        subjects.append(subject)
        trailers += [line.split(":", 1)[1] for line in lines
                     if line.lower().startswith(f"{trailer_key}:")]
    best, best_surface = "none", None
    for name, surface in (versioning.get("surfaces") or {}).items():
        if not isinstance(surface, dict):
            continue
        touched = [f for f in changed
                   if _vb_match(f["filename"], surface.get("trigger_paths") or [])]
        if not touched:
            continue
        meaningful = [f for f in touched if meaningful_patch(f.get("patch"))]
        level = "none"
        if meaningful:
            public = surface.get("public_api_paths") or []
            level = "minor" if any(_vb_match(f["filename"], public) for f in meaningful) \
                else "patch"
        for subject in subjects:
            suggested = classify_conventional(subject)
            if LEVELS.index(suggested) > LEVELS.index(level):
                level = suggested
        for value in trailers:
            match = re.search(rf"{re.escape(name)}\s*=\s*([A-Za-z]+)", value)
            if match and match.group(1).lower() in LEVELS \
                    and LEVELS.index(match.group(1).lower()) > LEVELS.index(level):
                level = match.group(1).lower()
        if LEVELS.index(level) > LEVELS.index(best):
            best, best_surface = level, name
    return best, best_surface


# ── Assessment ──────────────────────────────────────────────────────────


@dataclass
class Assessment:
    tier: str = LOW
    reasons: dict[str, list[str]] = field(default_factory=lambda: {t: [] for t in TIERS})
    owner: list[str] = field(default_factory=list)
    lines: int = 0
    files: int = 0

    def add(self, tier: str, reason: str) -> None:
        if reason not in self.reasons[tier]:
            self.reasons[tier].append(reason)
        if TIERS.index(tier) > TIERS.index(self.tier):
            self.tier = tier

    def requires(self) -> str:
        text = REQUIRES[self.tier]
        if self.tier == HIGH and self.owner:
            text += " + owner approval"
        return text

    def to_json(self) -> dict[str, Any]:
        return {"tier": self.tier, "requires": self.requires(),
                "reasons": {t: r for t, r in self.reasons.items() if r},
                "owner_approval": self.owner, "counted_lines": self.lines,
                "changed_files": self.files}


def assess(files: list[dict[str, Any]], labels: list[str], rules: Rules, *,
           version: tuple[str, str | None] = ("none", None),
           dependencies: list[DepFinding] = (), changed_files: int | None = None,
           ) -> Assessment:
    """The tier of a pull request changing ``files`` (the API's file list:
    ``filename``, optional ``previous_filename``, ``additions``,
    ``deletions``) with ``labels``."""
    out = Assessment()
    for label in labels:
        tier = LABELS.get(label.lower())
        if tier:
            out.add(tier, f"label {label}")
    out.files = changed_files if changed_files is not None else len(files)
    for item in files:
        paths = [item["filename"]] + ([item["previous_filename"]]
                                      if item.get("previous_filename") else [])
        for path in paths:
            if first_match(path, rules.high_paths):
                out.add(HIGH, path)
                if first_match(path, rules.owner_paths) and path not in out.owner:
                    out.owner.append(path)
            elif first_match(path, rules.moderate_paths):
                out.add(MODERATE, path)
        if not first_match(item["filename"], rules.size_excluded_paths):
            out.lines += int(item.get("additions") or 0) + int(item.get("deletions") or 0)
    for finding in dependencies:
        out.add(finding.tier, finding.reason)
        if finding.owner and finding.reason not in out.owner:
            out.owner.append(finding.reason)
    level, surface = version
    if level in ("minor", "major"):
        out.add(MODERATE, f"{level} version bump ({surface})")
    if out.lines > rules.max_lines:
        out.add(MODERATE, f"{out.lines} lines > {rules.max_lines}")
    if out.files > rules.max_files:
        out.add(MODERATE, f"{out.files} files > {rules.max_files}")
    return out


def _priority(reason: str) -> int:
    """Labels and direct dependencies first, tests last, in a description."""
    if reason.startswith("label ") or reason.startswith("new dependency") \
            or reason.startswith("new source") or reason.startswith("new package source"):
        return 0
    if "test" in reason.rsplit("/", 1)[-1]:
        return 3
    if " in " in reason and reason.split(":", 1)[0].endswith(".lock"):
        return 2
    return 1


def _display_text(value: str) -> str:
    """Render untrusted labels, paths, and dependency names as one safe line.

    These values can come from a pull request's filenames or dependency
    manifests.  They are printed both to Actions stdout (where a newline can
    become a workflow command) and inside Markdown inline-code spans.  Escape
    C0 controls and DEL visibly, and escape backticks so a reason cannot close
    the surrounding Markdown span.
    """
    out: list[str] = []
    for char in value:
        code = ord(char)
        if char == "`":
            out.append(r"\`")
        elif char == "\n":
            out.append(r"\n")
        elif char == "\r":
            out.append(r"\r")
        elif char == "\t":
            out.append(r"\t")
        elif code < 0x20 or code == 0x7F:
            out.append(f"\\x{code:02x}")
        else:
            out.append(char)
    return "".join(out)


def describe(assessment: Assessment) -> str:
    """A status description of at most 140 characters: the tier, as many of
    its reasons as fit, and what the tier would require."""
    head = f"report-only: {assessment.tier.capitalize()}"
    tail = f"; {assessment.requires()}"
    reasons = sorted(assessment.reasons[assessment.tier], key=_priority)
    budget = DESCRIPTION_LIMIT - len(head) - len(tail) - len(" ()")
    shown: list[str] = []
    for index, reason in enumerate(reasons):
        rest = len(reasons) - index - 1
        display_reason = _display_text(reason)
        candidate = ", ".join(shown + [display_reason]) + (f" +{rest}" if rest else "")
        if len(candidate) <= budget:
            shown.append(display_reason)
            continue
        if not shown:
            more = f" +{rest}" if rest else ""
            display_reason = _display_text(reason)
            shown.append(display_reason[: max(1, budget - len(more) - 1)] + "…")
            return f"{head} ({shown[0]}{more}){tail}"[:DESCRIPTION_LIMIT]
        return f"{head} ({', '.join(shown)} +{rest + 1}){tail}"[:DESCRIPTION_LIMIT]
    inner = f" ({', '.join(shown)})" if shown else ""
    return f"{head}{inner}{tail}"[:DESCRIPTION_LIMIT]


def summary(assessment: Assessment, pr: int, head: str) -> str:
    """Markdown for the check run and the local report."""
    lines = [f"**Tier: {assessment.tier.capitalize()}** for #{pr} at `{head[:12]}`: "
             f"{assessment.requires()}.", "",
             "Report-only: this status is always `success` and is not required.", ""]
    for tier in reversed(TIERS):
        if assessment.reasons[tier]:
            lines.append(f"{tier.capitalize()}:")
            lines += [f"- `{_display_text(reason)}`" for reason in assessment.reasons[tier]]
            lines.append("")
    if assessment.owner:
        lines.append("Owner subset: " + ", ".join(f"`{_display_text(r)}`" for r in assessment.owner))
    lines.append(f"Counted {assessment.lines} changed lines in {assessment.files} files.")
    return "\n".join(lines)


# ── GitHub ──────────────────────────────────────────────────────────────


class GitHub:
    def __init__(self, gh: str, repo: str) -> None:
        self.gh, self.repo = gh, repo

    def _run(self, args: list[str], stdin: str | None = None) -> subprocess.CompletedProcess:
        return subprocess.run([self.gh, "api", *args], input=stdin, capture_output=True,
                              text=True, check=False)

    def json(self, path: str) -> Any:
        done = self._run([path])
        if done.returncode != 0:
            raise TierError(f"GET {path} failed: {(done.stderr or done.stdout).strip()[:300]}")
        return json.loads(done.stdout)

    def pages(self, path: str, limit: int = 30) -> list[Any]:
        out: list[Any] = []
        joiner = "&" if "?" in path else "?"
        for page in range(1, limit + 1):
            batch = self.json(f"{path}{joiner}per_page=100&page={page}")
            if not isinstance(batch, list):
                raise TierError(f"GET {path} did not return a list")
            out += batch
            if len(batch) < 100:
                break
        return out

    def raw(self, path: str, ref: str) -> str | None:
        """A file's text at ``ref``, or None when it does not exist there."""
        done = self._run(["-H", "Accept: application/vnd.github.raw",
                          f"repos/{self.repo}/contents/{quote(path)}?ref={quote(ref, safe='')}"])
        if done.returncode == 0:
            return done.stdout
        if "404" in done.stderr or "Not Found" in done.stderr:
            return None
        raise TierError(f"reading {path} at {ref[:12]} failed: {done.stderr.strip()[:300]}")

    def post(self, path: str, body: dict[str, Any]) -> None:
        done = self._run(["-X", "POST", path, "--input", "-"], stdin=json.dumps(body))
        if done.returncode != 0:
            raise TierError(f"POST {path} failed: {(done.stderr or done.stdout).strip()[:300]}")


def evaluate(github: GitHub, number: int) -> tuple[dict[str, Any], Assessment]:
    """Read pull request ``number`` and classify it against its base."""
    repo = github.repo
    pull = github.json(f"repos/{repo}/pulls/{number}")
    head = pull["head"]["sha"]
    base_sha = pull["base"]["sha"]
    base_ref = pull["base"]["ref"]
    # Rules come from the base branch as it is now, not the pull request's
    # possibly stale merge base.
    rules = load_rules(github.raw(CONFIG_PATH, base_ref))
    files = github.pages(f"repos/{repo}/pulls/{number}/files")
    changed_files = int(pull.get("changed_files") or len(files))
    assessment_extra: list[DepFinding] = []
    if changed_files > len(files):
        assessment_extra.append(DepFinding(
            HIGH, f"file list truncated ({len(files)} of {changed_files})"))
    for item in files:
        name = item["filename"].rsplit("/", 1)[-1]
        if name not in DEPENDENCY_FILES or item.get("status") == "removed":
            continue
        before = item.get("previous_filename") or item["filename"]
        base_text = None if item.get("status") == "added" else github.raw(before, base_sha)
        assessment_extra += dependency_findings(
            item["filename"], base_text, github.raw(item["filename"], head))
    versioning_text = None
    for candidate in ([rules.versioning_config] if rules.versioning_config
                      else VERSIONING_CANDIDATES):
        versioning_text = github.raw(candidate, base_ref)
        if versioning_text is not None:
            break
    versioning = json.loads(versioning_text) if versioning_text else None
    commits = [c["commit"]["message"] for c in
               github.pages(f"repos/{repo}/pulls/{number}/commits", limit=3)]
    labels = [label["name"] for label in pull.get("labels") or []]
    assessment = assess(files, labels, rules, version=version_level(versioning, files, commits),
                        dependencies=assessment_extra, changed_files=changed_files)
    return pull, assessment


def publish(github: GitHub, pull: dict[str, Any], assessment: Assessment,
            run_url: str | None, check_run: bool) -> None:
    head = pull["head"]["sha"]
    status = {"state": "success", "context": CONTEXT, "description": describe(assessment)}
    if run_url:
        status["target_url"] = run_url
    github.post(f"repos/{github.repo}/statuses/{head}", status)
    if check_run:
        github.post(f"repos/{github.repo}/check-runs", {
            "name": CHECK_NAME, "head_sha": head, "status": "completed",
            "conclusion": "neutral",
            "output": {"title": f"{assessment.tier.capitalize()}: {assessment.requires()}",
                       "summary": summary(assessment, pull["number"], head)},
        })


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--repo", help="OWNER/NAME (default: GITHUB_REPOSITORY)")
    parser.add_argument("--pr", type=int, help="pull request (default: the event's)")
    parser.add_argument("--gh", default=os.environ.get("RISK_TIER_GH", "gh"),
                        help="gh-compatible binary, e.g. ghapp (default: gh)")
    parser.add_argument("--json", action="store_true", help="print the assessment as JSON")
    parser.add_argument("--post", action="store_true",
                        help="post the status (implied in GitHub Actions)")
    parser.add_argument("--check-run", action="store_true",
                        help="also post a neutral check run with the full summary")
    args = parser.parse_args(argv)
    in_actions = os.environ.get("GITHUB_ACTIONS") == "true"
    repo = args.repo or os.environ.get("GITHUB_REPOSITORY", "")
    number = args.pr
    if number is None and os.environ.get("GITHUB_EVENT_PATH"):
        event = json.loads(pathlib.Path(os.environ["GITHUB_EVENT_PATH"]).read_text("utf-8"))
        number = (event.get("pull_request") or {}).get("number")
    if not repo or not isinstance(number, int):
        print("risk-tier: a repository and pull request are required", file=sys.stderr)
        return 2
    github = GitHub(args.gh, repo)
    try:
        pull, assessment = evaluate(github, number)
    except TierError as error:
        print(f"risk-tier: PR #{number}: {error}", file=sys.stderr)
        return 1
    if pull.get("state") != "open" and (args.post or in_actions):
        print(f"risk-tier: PR #{number} is {pull.get('state')}; nothing to post")
        return 0
    if args.json:
        print(json.dumps({"pr": number, "head": pull["head"]["sha"], **assessment.to_json(),
                          "description": describe(assessment)}, indent=2))
    else:
        print(summary(assessment, number, pull["head"]["sha"]))
        print(f"status: {describe(assessment)}")
    if args.post or in_actions:
        server = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
        run_id = os.environ.get("GITHUB_RUN_ID")
        run_url = f"{server}/{repo}/actions/runs/{run_id}" if run_id else None
        try:
            publish(github, pull, assessment, run_url, args.check_run)
        except TierError as error:
            print(f"risk-tier: {error}", file=sys.stderr)
            return 1
        print(f"risk-tier: posted success ({assessment.tier}) on {pull['head']['sha']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
