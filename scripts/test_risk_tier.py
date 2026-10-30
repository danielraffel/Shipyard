"""The ``risk-tier`` status: the tier a pull request's paths, dependencies,
version bump, size and labels put it in, posted report-only."""

from __future__ import annotations

import importlib.util
import json
import contextlib
import io
import os
import pathlib
import sys
import unittest
from unittest import mock
from typing import Any

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import ghapp_queue_arm_guard as guard  # noqa: E402
import version_bump_check  # noqa: E402

spec = importlib.util.spec_from_file_location("risk_tier", HERE / "risk_tier.py")
assert spec is not None and spec.loader is not None
rt = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = rt
spec.loader.exec_module(rt)

WORKFLOW = HERE.parent / ".github" / "workflows" / "risk-tier.yml"
VERSIONING = json.loads((HERE / "versioning.json").read_text(encoding="utf-8"))


def changed(*paths: str, lines: int = 10, **extra: Any) -> list[dict[str, Any]]:
    return [{"filename": path, "additions": lines, "deletions": 0, **extra} for path in paths]


def tier(*paths: str, labels: tuple[str, ...] = (), **kwargs: Any) -> str:
    return rt.assess(changed(*paths), list(labels), rt.Rules(), **kwargs).tier


class Paths(unittest.TestCase):
    def test_docs_tests_skills_and_internal_code_are_low(self) -> None:
        for paths in (["README.md", "docs/merge-steward.md"],
                      ["skills/ci/SKILL.md", "commands/ship.md"],
                      ["src/changelog.rs", "src/changelog/tests.rs"],
                      ["CHANGELOG.md", "scripts/test_ci_matrix.py"],
                      ["tests/queue_hold.rs"]):
            with self.subTest(paths=paths):
                self.assertEqual(tier(*paths), rt.LOW)

    def test_high_paths(self) -> None:
        for path in (".github/workflows/ci.yml", ".github/actions/setup/action.yml",
                     ".shipyard/config.toml", ".agents/contract.toml",
                     "tools/ci/governed-build.sh", "scripts/ghapp",
                     "scripts/ghapp_queue_arm_guard.py", "scripts/ghapp_merge_guard.sh",
                     "src/head_approval.rs", "src/head_approval/carry.rs",
                     "src/queue_arm_gate.rs", "src/auto_arm/mod.rs", "src/ghapp_guards.rs",
                     "src/fleet_guards.rs", "src/app/ship_cmd/auto_arm.rs",
                     "src/app/merge_steward_cmd/native_arm.rs",
                     "scripts/ci_macos_signing_keychain.py", "tools/codesign-app.sh",
                     "scripts/notarize.py", "install.sh", "tools/install.sh",
                     "scripts/release.sh", "scripts/release-macos-local.sh",
                     "release-please-config.json", "src/auth_generation.rs",
                     "src/app/auth_cmd.rs", "scripts/shipyard-github-app-token",
                     "src/fleet/credentials.rs"):
            with self.subTest(path=path):
                self.assertEqual(tier(path), rt.HIGH)

    def test_moderate_paths(self) -> None:
        for path in ("src/merge_steward.rs", "src/merge_steward/policy.rs",
                     "src/app/merge_steward_cmd.rs", "src/queue.rs",
                     "src/queue_scheduler.rs", "src/app/queue_cmd/hold.rs",
                     "src/landing.rs", "src/landability/rules.rs",
                     "src/merge_train.rs", "src/merge_carrier/mod.rs",
                     "src/daemon_runtime.rs", "src/app/daemon_cmd.rs"):
            with self.subTest(path=path):
                self.assertEqual(tier(path), rt.MODERATE)

    def test_queue_and_daemon_docs_stay_low(self) -> None:
        self.assertEqual(tier("docs/merge-queue.md", "docs/daemon.md"), rt.LOW)

    def test_a_rename_is_judged_by_both_names(self) -> None:
        files = changed("docs/old-workflow.yml",
                        previous_filename=".github/workflows/old.yml")
        out = rt.assess(files, [], rt.Rules())
        self.assertEqual(out.tier, rt.HIGH)
        self.assertIn(".github/workflows/old.yml", out.owner)

    def test_the_owner_subset(self) -> None:
        owner = {".github/workflows/ci.yml", "scripts/ci_macos_signing_keychain.py",
                 "src/auth_generation.rs", "scripts/shipyard-github-app-token"}
        not_owner = {"scripts/ghapp_queue_arm_guard.py", ".shipyard/config.toml",
                     "tools/ci/x.sh", "install.sh"}
        out = rt.assess(changed(*sorted(owner | not_owner)), [], rt.Rules())
        self.assertEqual(set(out.owner), owner)
        self.assertEqual(out.requires(), "would require 2 review-App approvals + owner approval")
        self.assertEqual(rt.assess(changed(*not_owner), [], rt.Rules()).requires(),
                         "would require 2 review-App approvals")


class SizeAndLabels(unittest.TestCase):
    def test_more_than_400_lines_is_moderate(self) -> None:
        self.assertEqual(rt.assess(changed("src/a.rs", lines=400), [], rt.Rules()).tier, rt.LOW)
        out = rt.assess(changed("src/a.rs", lines=401), [], rt.Rules())
        self.assertEqual(out.tier, rt.MODERATE)
        self.assertIn("401 lines > 400", out.reasons[rt.MODERATE])

    def test_lockfile_lines_are_not_counted(self) -> None:
        files = changed("Cargo.lock", lines=5000) + changed("src/a.rs", lines=10)
        self.assertEqual(rt.assess(files, [], rt.Rules()).lines, 10)

    def test_more_than_15_files_is_moderate(self) -> None:
        paths = [f"docs/{n}.md" for n in range(16)]
        self.assertEqual(rt.assess(changed(*paths[:15], lines=1), [], rt.Rules()).tier, rt.LOW)
        self.assertEqual(rt.assess(changed(*paths, lines=1), [], rt.Rules()).tier, rt.MODERATE)

    def test_the_pull_requests_own_file_count_wins(self) -> None:
        out = rt.assess(changed("docs/a.md"), [], rt.Rules(), changed_files=40)
        self.assertEqual(out.tier, rt.MODERATE)

    def test_labels_raise(self) -> None:
        self.assertEqual(tier("docs/a.md", labels=("risk:moderate",)), rt.MODERATE)
        self.assertEqual(tier("docs/a.md", labels=("risk:high",)), rt.HIGH)
        self.assertEqual(tier("docs/a.md", labels=("RISK:HIGH",)), rt.HIGH)

    def test_labels_never_lower(self) -> None:
        self.assertEqual(tier(".github/workflows/ci.yml", labels=("risk:moderate",)), rt.HIGH)
        self.assertEqual(tier(".github/workflows/ci.yml", labels=("risk:low",)), rt.HIGH)
        self.assertEqual(tier("src/queue.rs", labels=("risk:low",)), rt.MODERATE)


class BaseConfig(unittest.TestCase):
    def test_no_config_or_no_table_is_the_defaults(self) -> None:
        for text in (None, "", "[project]\nname = 'x'\n"):
            self.assertEqual(rt.load_rules(text), rt.Rules())

    def test_replace_and_extra(self) -> None:
        rules = rt.load_rules(
            '[review.tiers]\nhigh_paths = ["vendor/**"]\nhigh_paths_extra = ["ops/**"]\n'
            'moderate_paths_extra = ["engine/**"]\nmax_changed_lines = 50\n'
            'max_changed_files = 3\n')
        self.assertEqual(rt.assess(changed("vendor/x.c"), [], rules).tier, rt.HIGH)
        self.assertEqual(rt.assess(changed("ops/run.sh"), [], rules).tier, rt.HIGH)
        self.assertEqual(rt.assess(changed("tools/ci/x.sh"), [], rules).tier, rt.LOW)
        self.assertEqual(rt.assess(changed("engine/a.cpp"), [], rules).tier, rt.MODERATE)
        self.assertEqual(rt.assess(changed("src/queue.rs"), [], rules).tier, rt.MODERATE)
        self.assertEqual(rt.assess(changed("a.md", lines=51), [], rules).tier, rt.MODERATE)
        self.assertEqual(rt.assess(changed("a", "b", "c", "d", lines=1), [], rules).tier,
                         rt.MODERATE)

    def test_a_replacement_cannot_drop_the_rules_own_inputs(self) -> None:
        rules = rt.load_rules('[review.tiers]\nhigh_paths = []\n')
        for path in (".github/workflows/risk-tier.yml", ".shipyard/config.toml"):
            self.assertEqual(rt.assess(changed(path), [], rules).tier, rt.HIGH)

    def test_malformed_config_is_an_error(self) -> None:
        for text in ("[review.tiers]\nhigh_paths = 'x'\n",
                     "[review.tiers]\nmax_changed_lines = -1\n",
                     "[review.tiers]\nmax_changed_files = true\n",
                     "[review\n"):
            with self.subTest(text=text), self.assertRaises(rt.TierError):
                rt.load_rules(text)


CARGO_BASE = """
[package]
name = "shipyard"
version = "0.1.0"
[dependencies]
serde = "1.0.200"
toml = { version = "0.8" }
[target.'cfg(unix)'.dependencies]
libc = "0.2"
"""

LOCK_BASE = """
version = 4
[[package]]
name = "shipyard"
version = "0.1.0"
[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"
"""


class Dependencies(unittest.TestCase):
    def findings(self, path: str, base: str | None, head: str | None) -> list[tuple[str, str]]:
        return [(f.tier, f.reason) for f in rt.dependency_findings(path, base, head)]

    def test_cargo_manifest(self) -> None:
        self.assertEqual(self.findings("Cargo.toml", CARGO_BASE, CARGO_BASE), [])
        bumped = CARGO_BASE.replace('serde = "1.0.200"', 'serde = "1.0.210"')
        self.assertEqual(self.findings("Cargo.toml", CARGO_BASE, bumped),
                         [(rt.MODERATE, "bumps serde (Cargo.toml)")])
        added = CARGO_BASE + 'flate2 = "1"\n'
        self.assertEqual(self.findings("Cargo.toml", CARGO_BASE, added),
                         [(rt.HIGH, "new dependency flate2 (Cargo.toml)")])
        git = CARGO_BASE.replace('toml = { version = "0.8" }',
                                 'toml = { version = "0.8", git = "https://example.com/toml" }')
        self.assertEqual(self.findings("Cargo.toml", CARGO_BASE, git),
                         [(rt.HIGH, "new source for toml (Cargo.toml)")])
        target = CARGO_BASE.replace('libc = "0.2"', 'libc = "0.2"\nnix = "0.29"')
        self.assertEqual(self.findings("Cargo.toml", CARGO_BASE, target),
                         [(rt.HIGH, "new dependency nix (Cargo.toml)")])
        removed = CARGO_BASE.replace('serde = "1.0.200"\n', "")
        self.assertEqual(self.findings("Cargo.toml", CARGO_BASE, removed), [])
        own_version = CARGO_BASE.replace('version = "0.1.0"', 'version = "0.2.0"')
        self.assertEqual(self.findings("Cargo.toml", CARGO_BASE, own_version), [])

    def test_cargo_lock(self) -> None:
        crate = ('\n[[package]]\nname = "flate2"\nversion = "1.0.30"\n'
                 'source = "registry+https://github.com/rust-lang/crates.io-index"\n')
        self.assertEqual(self.findings("Cargo.lock", LOCK_BASE, LOCK_BASE + crate),
                         [(rt.HIGH, "1 new package in Cargo.lock: flate2")])
        bumped = LOCK_BASE.replace('"1.0.200"', '"1.0.210"')
        self.assertEqual(self.findings("Cargo.lock", LOCK_BASE, bumped),
                         [(rt.MODERATE, "bumps 1 in Cargo.lock: serde")])
        own = LOCK_BASE.replace('name = "shipyard"\nversion = "0.1.0"',
                                'name = "shipyard"\nversion = "0.2.0"')
        self.assertEqual(self.findings("Cargo.lock", LOCK_BASE, own), [])
        member = LOCK_BASE + '\n[[package]]\nname = "shipyard-core"\nversion = "0.1.0"\n'
        self.assertEqual(self.findings("Cargo.lock", LOCK_BASE, member), [])
        git = bumped.replace("registry+https://github.com/rust-lang/crates.io-index",
                             "git+https://example.com/serde?rev=1#abc")
        found = self.findings("Cargo.lock", LOCK_BASE, git)
        self.assertIn((rt.HIGH, "new package source git+https://example.com/serde (Cargo.lock)"),
                      found)

    def test_many_new_packages_are_one_finding(self) -> None:
        crates = "".join(f'\n[[package]]\nname = "c{n}"\nversion = "1.0.0"\n'
                         'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
                         for n in range(7))
        self.assertEqual(self.findings("Cargo.lock", LOCK_BASE, LOCK_BASE + crates),
                         [(rt.HIGH, "7 new packages in Cargo.lock: c0, c1, c2 +4")])

    def test_pyproject(self) -> None:
        base = ('[project]\nname = "x"\ndependencies = ["requests>=2.31", "PyYAML==6.0"]\n'
                '[dependency-groups]\ndev = ["pytest>=8"]\n')
        self.assertEqual(self.findings("pyproject.toml", base, base), [])
        self.assertEqual(
            self.findings("pyproject.toml", base, base.replace("requests>=2.31", "requests>=2.32")),
            [(rt.MODERATE, "bumps requests (pyproject.toml)")])
        self.assertEqual(
            self.findings("pyproject.toml", base, base.replace('"pytest>=8"', '"pytest>=8", "hypothesis"')),
            [(rt.HIGH, "new dependency hypothesis (pyproject.toml)")])
        self.assertEqual(
            self.findings("pyproject.toml", base, base.replace("PyYAML==6.0", "pyyaml==6.0")), [])
        sourced = base + '[tool.uv.sources]\nrequests = { git = "https://example.com/requests" }\n'
        self.assertEqual(self.findings("pyproject.toml", base, sourced),
                         [(rt.HIGH, "new dependency requests (pyproject.toml)")])
        url = base.replace('"requests>=2.31"', '"requests @ https://example.com/r.whl"')
        self.assertEqual(self.findings("pyproject.toml", base, url),
                         [(rt.HIGH, "new source for requests (pyproject.toml)")])

    def test_uv_lock(self) -> None:
        base = ('version = 1\n[[package]]\nname = "x"\nversion = "0.1.0"\n'
                'source = { editable = "." }\n[[package]]\nname = "requests"\n'
                'version = "2.31.0"\nsource = { registry = "https://pypi.org/simple" }\n')
        head = base.replace('"2.31.0"', '"2.32.0"') + (
            '[[package]]\nname = "idna"\nversion = "3.7"\n'
            'source = { registry = "https://mirror.example.com/simple" }\n')
        self.assertEqual(self.findings("uv.lock", base, head), [
            (rt.HIGH, "new package source registry=https://mirror.example.com/simple (uv.lock)"),
            (rt.HIGH, "1 new package in uv.lock: idna"),
            (rt.MODERATE, "bumps 1 in uv.lock: requests")])

    def test_an_unreadable_file_is_high(self) -> None:
        self.assertEqual(self.findings("Cargo.toml", CARGO_BASE, "[dependencies\n"),
                         [(rt.HIGH, "Cargo.toml is unreadable (TOMLDecodeError)")])

    def test_other_files_are_not_parsed(self) -> None:
        self.assertEqual(self.findings("docs/Cargo.toml.md", None, "not toml ["), [])

    def test_findings_set_the_tier_and_the_owner_subset(self) -> None:
        found = rt.dependency_findings("Cargo.toml", CARGO_BASE, CARGO_BASE + 'tar = "0.4"\n')
        out = rt.assess(changed("Cargo.toml"), [], rt.Rules(), dependencies=found)
        self.assertEqual(out.tier, rt.HIGH)
        self.assertEqual(out.owner, ["new dependency tar (Cargo.toml)"])


class VersionBump(unittest.TestCase):
    def level(self, files: list[dict[str, Any]], commits: list[str] = ()) -> str:
        return rt.version_level(VERSIONING, files, list(commits))[0]

    def test_public_api_is_minor_and_internal_is_patch(self) -> None:
        self.assertEqual(self.level(changed("src/app/cli.rs", patch="+let x = 1;")), "minor")
        self.assertEqual(self.level(changed("src/changelog.rs", patch="+let x = 1;")), "patch")
        self.assertEqual(self.level(changed("docs/a.md", patch="+x")), "none")
        out = rt.assess(changed("src/app/cli.rs"), [], rt.Rules(),
                        version=rt.version_level(VERSIONING, changed("src/app/cli.rs"), []))
        self.assertEqual((out.tier, out.reasons[rt.MODERATE]),
                         (rt.MODERATE, ["minor version bump (cli)"]))

    def test_a_comment_only_change_asks_for_nothing(self) -> None:
        # As version_bump_check: a surface whose touched paths change only
        # comments or whitespace needs no bump.
        self.assertEqual(self.level(changed("src/app/cli.rs", patch="@@\n+// note\n-// old")),
                         "none")

    def test_commit_subjects_and_trailers_raise_never_lower(self) -> None:
        internal = changed("src/changelog.rs", patch="+x")
        self.assertEqual(self.level(internal, ["feat: add a thing"]), "minor")
        self.assertEqual(self.level(internal, ["fix!: drop a flag"]), "major")
        self.assertEqual(self.level(internal, ["fix: x\n\nVersion-Bump: cli=minor reason=\"y\""]),
                         "minor")
        public = changed("src/app/cli.rs", patch="+x")
        self.assertEqual(self.level(public, ["fix: x\n\nVersion-Bump: cli=skip reason=\"y\""]),
                         "minor")
        self.assertEqual(self.level(public, ["fix: x\n\nVersion-Bump: cli=patch reason=\"y\""]),
                         "minor")
        self.assertEqual(self.level(internal, ['Revert "feat: add a thing"']), "patch")

    def test_a_subject_does_not_bump_an_untouched_surface(self) -> None:
        self.assertEqual(self.level(changed("docs/a.md"), ["feat: docs"]), "none")

    def test_version_files_and_generated_files_ask_for_nothing(self) -> None:
        self.assertEqual(self.level(changed(".claude-plugin/plugin.json", "Cargo.toml",
                                            "Cargo.lock", "target/debug/x.rs")), "none")

    def test_no_versioning_config_is_none(self) -> None:
        self.assertEqual(rt.version_level(None, changed("src/app/cli.rs"), []), ("none", None))

    def test_conventional_classification_matches_version_bump_check(self) -> None:
        for subject in ("feat: x", "feat(scope): x", "fix: x", "perf: x", "chore: x",
                        "fix!: x", "feat(a)!: x", "BREAKING: x", "docs: BREAKING change",
                        "Merge pull request #1", "refactor: x", "FEAT: x"):
            self.assertEqual(rt.classify_conventional(subject),
                             version_bump_check.classify_conventional(subject), subject)


class Globs(unittest.TestCase):
    def test_glob_matching_matches_the_guard(self) -> None:
        patterns = list(rt.DEFAULT_HIGH_PATHS) + list(rt.DEFAULT_MODERATE_PATHS) + ["a/*/b", "?x"]
        paths = [".github/workflows/ci.yml", ".github/workflows/sub/x.yml", "src/queue.rs",
                 "src/app/queue_cmd/a.rs", "scripts/ghapp", "scripts/ghapp_x.py",
                 "deep/a/b/release-x.sh", "install.sh", "a/x/b", "a/x/y/b", "ax", "x",
                 "src/auth_generation.rs", "docs/merge-queue.md", "src/x[1].rs"]
        for pattern in patterns:
            for path in paths:
                self.assertEqual(rt.glob_match(pattern, path), guard.glob_match(pattern, path),
                                 (pattern, path))


class Description(unittest.TestCase):
    def test_low(self) -> None:
        out = rt.assess(changed("docs/a.md"), [], rt.Rules())
        self.assertEqual(rt.describe(out), "report-only: Low; needs CI only")

    def test_moderate(self) -> None:
        out = rt.assess(changed("src/queue.rs"), [], rt.Rules())
        self.assertEqual(rt.describe(out),
                         "report-only: Moderate (src/queue.rs); would require 1 review-App approval")

    def test_always_within_githubs_limit(self) -> None:
        for count in range(1, 30):
            paths = [f".github/workflows/a-very-long-workflow-name-number-{n:03}.yml"
                     for n in range(count)]
            text = rt.describe(rt.assess(changed(*paths), [], rt.Rules()))
            self.assertLessEqual(len(text), rt.DESCRIPTION_LIMIT, text)
            self.assertTrue(text.startswith("report-only: High ("), text)
            self.assertTrue(text.endswith("+ owner approval"), text)
            if count > 1:
                self.assertRegex(text, r" \+\d+\); ")

    def test_labels_and_direct_dependencies_come_first_and_tests_last(self) -> None:
        found = rt.dependency_findings("Cargo.toml", CARGO_BASE, CARGO_BASE + 'tar = "0.4"\n')
        out = rt.assess(changed("scripts/test_ghapp_x_guard.py", "scripts/ghapp_x_guard.py"),
                        ["risk:high"], rt.Rules(), dependencies=found)
        self.assertTrue(rt.describe(out).startswith(
            "report-only: High (label risk:high, new dependency tar (Cargo.toml)"),
            rt.describe(out))


class FakeGitHub(rt.GitHub):
    """Answers ``gh api`` from a dict and records what is posted."""

    def __init__(self, answers: dict[str, Any], files: dict[tuple[str, str], str]) -> None:
        super().__init__("gh", "o/r")
        self.answers, self.files, self.posted = answers, files, []

    def json(self, path: str) -> Any:
        key = path.split("?", 1)[0]
        if "page=" in path and not path.endswith("page=1"):
            return []
        if key not in self.answers:
            raise rt.TierError(f"GET {path} failed: 404")
        return self.answers[key]

    def raw(self, path: str, ref: str) -> str | None:
        return self.files.get((path, ref))

    def post(self, path: str, body: dict[str, Any]) -> None:
        self.posted.append((path, body))


def pull(**overrides: Any) -> dict[str, Any]:
    return {"number": 7, "state": "open", "head": {"sha": "h" * 40},
            "base": {"sha": "b" * 40, "ref": "main"}, "labels": [], "changed_files": 2,
            **overrides}


class EndToEnd(unittest.TestCase):
    def github(self, files: list[dict[str, Any]], *, raw: dict | None = None,
               **pr: Any) -> FakeGitHub:
        return FakeGitHub({"repos/o/r/pulls/7": pull(**pr),
                           "repos/o/r/pulls/7/files": files,
                           "repos/o/r/pulls/7/commits": [{"commit": {"message": "fix: x"}}]},
                          raw or {})

    def test_rules_come_from_the_base_and_dependencies_from_both_sides(self) -> None:
        files = [{"filename": "Cargo.toml", "status": "modified", "additions": 1, "deletions": 0},
                 {"filename": "vendor/x.c", "status": "added", "additions": 1, "deletions": 0}]
        raw = {(".shipyard/config.toml", "main"): '[review.tiers]\nmoderate_paths = ["vendor/**"]\n',
               ("Cargo.toml", "b" * 40): CARGO_BASE,
               ("Cargo.toml", "h" * 40): CARGO_BASE.replace('"1.0.200"', '"1.0.201"')}
        github = self.github(files, raw=raw)
        _, out = rt.evaluate(github, 7)
        self.assertEqual(out.tier, rt.MODERATE)
        self.assertEqual(out.reasons[rt.MODERATE], ["vendor/x.c", "bumps serde (Cargo.toml)"])
        # A head config naming the same path High changes nothing: only the
        # base is read.
        raw[(".shipyard/config.toml", "h" * 40)] = '[review.tiers]\nhigh_paths = ["vendor/**"]\n'
        self.assertEqual(rt.evaluate(github, 7)[1].tier, rt.MODERATE)

    def test_a_truncated_file_list_is_high(self) -> None:
        github = self.github(changed("docs/a.md"), changed_files=3001)
        out = rt.evaluate(github, 7)[1]
        self.assertEqual(out.tier, rt.HIGH)
        self.assertIn("file list truncated (1 of 3001)", out.reasons[rt.HIGH])

    def test_labels_are_read_from_the_pull_request(self) -> None:
        github = self.github(changed("docs/a.md"), labels=[{"name": "risk:high"}])
        self.assertEqual(rt.evaluate(github, 7)[1].tier, rt.HIGH)

    def test_the_status_is_always_success(self) -> None:
        for files in (changed("docs/a.md"), changed(".github/workflows/ci.yml")):
            github = self.github(files)
            pr, out = rt.evaluate(github, 7)
            rt.publish(github, pr, out, "https://run", check_run=True)
            (status_path, status), (check_path, check) = github.posted
            self.assertEqual(status_path, f"repos/o/r/statuses/{'h' * 40}")
            self.assertEqual(status["state"], "success")
            self.assertEqual(status["context"], "risk-tier")
            self.assertEqual(status["target_url"], "https://run")
            self.assertEqual(check_path, "repos/o/r/check-runs")
            self.assertEqual(check["conclusion"], "neutral")
            self.assertIn(out.tier.capitalize(), check["output"]["title"])


FORGED_PATH = ".github/workflows/x\n::error::forged path.yml"
FORGED_LOCK = LOCK_BASE + (
    '\n[[package]]\nname = "evil\\n::add-mask::secret\\r\\n::stop-commands::t"\n'
    'version = "1.0.0"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\n')


class UntrustedText(unittest.TestCase):
    """Paths and package names come from the pull request and may hold a
    newline; none may start a line of output, a status or a check run."""

    def run_main(self, files: list[dict[str, Any]], raw: dict, *extra: str,
                 env: dict[str, str] | None = None) -> tuple[str, FakeGitHub]:
        github = FakeGitHub({"repos/o/r/pulls/7": pull(changed_files=len(files)),
                             "repos/o/r/pulls/7/files": files,
                             "repos/o/r/pulls/7/commits": [{"commit": {"message": "fix: x"}}]},
                            raw)
        out = io.StringIO()
        with mock.patch.object(rt, "GitHub", lambda *_: github), \
                mock.patch.dict(os.environ, env or {"GITHUB_ACTIONS": ""}), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
            self.assertEqual(rt.main(["--repo", "o/r", "--pr", "7", "--post", "--check-run",
                                      *extra]), 0)
        return out.getvalue(), github

    def assert_inert(self, output: str, github: FakeGitHub, *, fenced: bool = False) -> None:
        lines = output.splitlines()
        if fenced:
            self.assertRegex(lines[0], r"^::stop-commands::[0-9a-f]{32}$")
            token = lines[0].split("::")[2]
            self.assertEqual(lines[-1], f"::{token}::")
            lines = lines[1:-1]
        self.assertEqual([line for line in lines if line.lstrip().startswith("::")], [])
        (_, status), (_, check) = github.posted
        self.assertIsNone(rt._CONTROL.search(status["description"]), status["description"])
        self.assertIsNone(rt._CONTROL.search(check["output"]["title"]))
        self.assertEqual([line for line in check["output"]["summary"].splitlines()
                          if line.lstrip().startswith("::")], [])

    def test_a_path_with_a_newline(self) -> None:
        output, github = self.run_main(changed(FORGED_PATH), {})
        self.assert_inert(output, github)
        self.assertIn("x\\n::error::forged path.yml", output)

    def test_a_lockfile_package_name_with_a_newline(self) -> None:
        files = [{"filename": "Cargo.lock", "status": "modified", "additions": 5, "deletions": 0}]
        raw = {("Cargo.lock", "b" * 40): LOCK_BASE, ("Cargo.lock", "h" * 40): FORGED_LOCK}
        output, github = self.run_main(files, raw)
        self.assert_inert(output, github)
        self.assertIn("evil\\n::add-mask::secret\\r\\n::stop-commands::t", output)

    def test_json_output_and_a_job_are_inert_too(self) -> None:
        files = changed(FORGED_PATH) + [
            {"filename": "Cargo.lock", "status": "modified", "additions": 5, "deletions": 0}]
        raw = {("Cargo.lock", "b" * 40): LOCK_BASE, ("Cargo.lock", "h" * 40): FORGED_LOCK}
        output, github = self.run_main(files, raw, "--json")
        self.assert_inert(output, github)
        output, github = self.run_main(files, raw, env={"GITHUB_ACTIONS": "true"})
        self.assert_inert(output, github, fenced=True)

    def test_a_backtick_cannot_end_its_code_span(self) -> None:
        self.assertEqual(rt.code("a`b"), "``a`b``")
        self.assertEqual(rt.code("``x"), "``` ``x ```")
        self.assertEqual(rt.code("a\nb"), "`a\\nb`")


class Workflow(unittest.TestCase):
    def test_it_runs_the_default_branchs_rule_and_never_checks_out_the_pull_request(self) -> None:
        text = WORKFLOW.read_text(encoding="utf-8")
        on = text.split("\non:", 1)[1].split("\npermissions:", 1)[0]
        self.assertIn("pull_request_target:", on)
        self.assertNotIn("pull_request:", on.replace("pull_request_target:", ""))
        self.assertNotIn("workflow_run", on)
        self.assertNotIn("edited", on)
        self.assertIn("ref: ${{ github.event.repository.default_branch }}", text)
        self.assertIn("persist-credentials: false", text)
        self.assertNotIn("github.event.pull_request.head", text)
        self.assertNotIn("refs/pull", text)
        permissions = text.split("\npermissions:", 1)[1].split("\n\n", 1)[0]
        self.assertEqual(sorted(line.strip() for line in permissions.strip().splitlines()),
                         ["checks: write", "contents: read", "pull-requests: read",
                          "statuses: write"])


if __name__ == "__main__":
    unittest.main()
