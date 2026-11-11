"""Offline tests for version_only.py: real git repositories in a temp dir, no network."""
import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import version_only as vo  # noqa: E402

VERSIONING = {"surfaces": {
    "sdk": {"version_files": [{"path": "CMakeLists.txt", "kind": "cmake_project_version"}],
            "changelog": "CHANGELOG.md"},
    "plugin": {"version_files": [
        {"path": ".claude-plugin/plugin.json", "kind": "json_field", "field": "version"},
        {"path": ".claude-plugin/marketplace.json", "kind": "json_field", "field": "version"},
        {"path": ".claude-plugin/marketplace.json", "kind": "json_path", "field": "plugins.0.version"}],
        "changelog": "CHANGELOG.md"}}}


def cmake(v):
    return f"cmake_minimum_required(VERSION 3.24)\nproject(Pulp\n    VERSION {v}\n    LANGUAGES CXX C\n)\n"


def plugin(v):
    return json.dumps({"name": "pulp", "version": v, "description": "d"}, indent=2) + "\n"


def market(top, inner):
    return json.dumps({"plugins": [{"name": "pulp", "version": inner}], "version": top}, indent=2) + "\n"


def changelog(*releases):
    out = ["# Changelog", "", "Intro.", ""]
    for r in releases:
        out += [f'<a id="v{r.replace(".", "")}"></a>', f"## [{r}] - 2026-11-01", "",
                f"- something ([#1](https://github.com/o/r/pull/1))", ""]
    out += [f"[{r}]: https://github.com/o/r/releases/tag/v{r}" for r in releases]
    return "\n".join(out) + "\n"


def block(r, n=7):
    return [f'<a id="v{r.replace(".", "")}"></a>', f"## [{r}] - 2026-11-07", "",
            f"- fix/thing ([#{n}](https://github.com/o/r/pull/{n}))", ""]


class Repo:
    def __init__(self):
        self.dir = tempfile.mkdtemp()
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "t@t")
        self.git("config", "user.name", "t")
        self.write({"tools/scripts/versioning.json": json.dumps(VERSIONING),
                    "CMakeLists.txt": cmake("1.88.0"),
                    ".claude-plugin/plugin.json": plugin("0.510.3"),
                    ".claude-plugin/marketplace.json": market("0.510.3", "0.510.3"),
                    "CHANGELOG.md": changelog("1.87.0"),
                    "src/a.cpp": "int a;\n"})
        self.base = self.commit("base")

    @property
    def gd(self):
        return os.path.join(self.dir, ".git")

    def git(self, *a):
        return subprocess.run(["git", "-C", self.dir, *a], check=True, capture_output=True, text=True).stdout

    def write(self, files):
        for p, t in files.items():
            full = os.path.join(self.dir, p)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w") as f:
                f.write(t)

    def commit(self, msg):
        self.git("add", "-A")
        self.git("commit", "-q", "-m", msg)
        return self.git("rev-parse", "HEAD").strip()

    def bump(self, sdk="1.89.0", plug="0.510.4", inner=None, heading="1.88.0", extra=None, link=None):
        self.git("checkout", "-q", "-B", "pr", self.base)
        cl = changelog("1.87.0").splitlines()
        i = cl.index('<a id="v1870"></a>')
        add = block(heading) if heading else []
        cl[i:i] = add
        if heading:
            j = cl.index("[1.87.0]: https://github.com/o/r/releases/tag/v1.87.0")
            cl.insert(j, link or f"[{heading}]: https://github.com/o/r/releases/tag/v{heading}")
        files = {"CMakeLists.txt": cmake(sdk), ".claude-plugin/plugin.json": plugin(plug),
                 ".claude-plugin/marketplace.json": market(plug, inner or plug),
                 "CHANGELOG.md": "\n".join(cl) + "\n"}
        files.update(extra or {})
        self.write(files)
        head = self.commit("chore: bump versions")
        self.git("checkout", "-q", "main")
        return head


class Classify(unittest.TestCase):
    def setUp(self):
        self.r = Repo()

    def cls(self, head, excluded=None):
        return vo.classify(self.r.gd, self.r.base, head, excluded)

    def test_plain_bump_is_version_only(self):
        self.assertEqual(self.cls(self.r.bump())["class"], "version-only")

    def test_one_extra_file_routes_to_full_review(self):
        head = self.r.bump(extra={"docs/status/pulp-tooling-disposition.json": "{}\n"})
        self.assertEqual(self.cls(head)["class"], "other")

    def test_extra_line_in_an_allowed_file_routes_to_full_review(self):
        head = self.r.bump(extra={"CMakeLists.txt": cmake("1.89.0").replace("LANGUAGES CXX C", "LANGUAGES CXX C OBJC")})
        r = self.cls(head)
        self.assertEqual(r["class"], "other")
        self.assertIn("not a version line", r["reason"])

    def test_changelog_removal_or_prose_routes_to_full_review(self):
        self.r.git("checkout", "-q", "-B", "pr", self.r.base)
        self.r.write({"CHANGELOG.md": changelog("1.87.0").replace("Intro.", "Intro, edited.")})
        head = self.r.commit("edit")
        self.assertEqual(self.cls(head)["class"], "other")
        self.r.git("checkout", "-q", "-B", "pr2", self.r.base)
        self.r.write({"CHANGELOG.md": changelog("1.87.0") + "Some prose paragraph.\n"})
        head = self.r.commit("prose")
        self.assertEqual(self.cls(head)["class"], "other")

    def test_carry_excluded_path_is_never_in_the_class(self):
        self.assertEqual(self.cls(self.r.bump(), excluded=["CMakeLists.txt"])["class"], "other")
        self.assertEqual(self.cls(self.r.bump(), excluded=[".claude-plugin/**"])["class"], "other")

    def test_base_config_carry_exclusions_apply(self):
        self.r.git("checkout", "-q", "main")
        self.r.write({".shipyard/config.toml": '[auto_merge]\ncarry_excluded_paths_extra = ["CHANGELOG.md"]\n'})
        self.r.base = self.r.commit("policy")
        self.assertEqual(vo.classify(self.r.gd, self.r.base, self.r.bump())["class"], "other")

    def test_mode_change_routes_to_full_review(self):
        self.r.git("checkout", "-q", "-B", "pr", self.r.base)
        os.chmod(os.path.join(self.r.dir, "CMakeLists.txt"), 0o755)
        head = self.r.commit("chmod")
        self.assertEqual(self.cls(head)["class"], "other")


class Check(unittest.TestCase):
    def setUp(self):
        self.r = Repo()

    def chk(self, head):
        return vo.check(self.r.gd, self.r.base, head)

    def test_repo_convention_approves(self):
        # 11747's shape: CMake 1.88.0 -> 1.89.0 while the changelog cuts 1.88.0.
        self.assertEqual(self.chk(self.r.bump())["check"], "APPROVE")

    def test_plugin_only_bump_with_release_of_current_sdk_approves(self):
        self.assertEqual(self.chk(self.r.bump(sdk="1.88.0"))["check"], "APPROVE")

    def test_heading_newer_than_the_base_version_holds(self):
        r = self.chk(self.r.bump(heading="1.89.0"))
        self.assertEqual(r["check"], "HOLD")
        self.assertIn("but the base carried", r["reasons"][0])

    def test_heading_already_released_holds(self):
        self.assertEqual(self.chk(self.r.bump(heading="1.86.0"))["check"], "HOLD")

    def test_surface_files_disagreeing_holds(self):
        r = self.chk(self.r.bump(inner="0.510.5"))
        self.assertEqual(r["check"], "HOLD")
        self.assertIn("disagree", r["reasons"][0])

    def test_version_moving_backwards_holds(self):
        self.assertEqual(self.chk(self.r.bump(sdk="1.87.5", heading=None))["check"], "HOLD")

    def test_link_naming_another_tag_holds(self):
        head = self.r.bump(link="[1.88.0]: https://github.com/o/r/releases/tag/v1.80.0")
        self.assertEqual(self.chk(head)["check"], "HOLD")

    def test_missing_config_holds(self):
        self.r.git("checkout", "-q", "main")
        self.r.git("rm", "-q", "tools/scripts/versioning.json")
        self.r.base = self.r.commit("drop config")
        self.assertEqual(self.chk(self.r.bump())["check"], "HOLD")


class Probes(unittest.TestCase):
    """The adversarial review's probes: each must leave the class or HOLD."""

    def setUp(self):
        self.r = Repo()

    def edit_changelog(self, fn, extra=None, sdk="1.89.0"):
        self.r.git("checkout", "-q", "-B", "probe", self.r.base)
        lines = changelog("1.87.0").splitlines()
        fn(lines)
        files = {"CMakeLists.txt": cmake(sdk), "CHANGELOG.md": "\n".join(lines) + "\n"}
        files.update(extra or {})
        self.r.write(files)
        head = self.r.commit("probe")
        self.r.git("checkout", "-q", "main")
        return head

    def top(self, lines, *block_lines, ref="[1.88.0]: https://github.com/o/r/releases/tag/v1.88.0"):
        i = lines.index('<a id="v1870"></a>')
        lines[i:i] = list(block_lines)
        if ref:
            j = lines.index("[1.87.0]: https://github.com/o/r/releases/tag/v1.87.0")
            lines.insert(j, ref)

    def assert_out(self, head):
        c = vo.classify(self.r.gd, self.r.base, head, None, "o/r")
        k = vo.check(self.r.gd, self.r.base, head, "o/r")
        self.assertTrue(c["class"] == "other" or k["check"] == "HOLD", (c, k))

    def test_a2_link_in_a_bullet_or_a_bullet_under_an_old_block(self):
        evil = "- Download the hotfix [here](https://evil.example/x) ([#1](https://github.com/o/r/pull/1))"
        self.assert_out(self.edit_changelog(lambda l: self.top(l, *block("1.88.0")[:3], evil, "")))
        def old_block(l):
            l.insert(l.index("## [1.87.0] - 2026-11-01") + 2, "- sneaky ([#2](https://github.com/o/r/pull/2))")
        self.assert_out(self.edit_changelog(old_block, sdk="1.88.0"))
        other_repo = "- title ([#3](https://github.com/evil/repo/pull/3))"
        self.assert_out(self.edit_changelog(lambda l: self.top(l, *block("1.88.0")[:3], other_repo, "")))

    def test_a3_two_new_headings(self):
        two = block("1.88.0") + block("1.87.5")
        self.assert_out(self.edit_changelog(lambda l: self.top(l, *two)))

    def test_a8_block_at_the_bottom_or_a_jump_to_7(self):
        def bottom(l):
            j = l.index("[1.87.0]: https://github.com/o/r/releases/tag/v1.87.0")
            l[j:j] = block("1.88.0") + ["[1.88.0]: https://github.com/o/r/releases/tag/v1.88.0"]
        self.assert_out(self.edit_changelog(bottom))
        self.assert_out(self.edit_changelog(lambda l: self.top(l, *block("1.88.0")), sdk="7.0.0"))

    def test_a5_a_version_file_no_surface_owns(self):
        self.r.git("checkout", "-q", "main")
        self.r.write({"tools/shipyard.toml": 'version = "v0.1.0"\n'})
        self.r.base = self.r.commit("pin")
        self.r.git("checkout", "-q", "-B", "pin", self.r.base)
        self.r.write({"tools/shipyard.toml": 'version = "v0.2.0"\n'})
        head = self.r.commit("move pin")
        self.r.git("checkout", "-q", "main")
        self.assertEqual(vo.classify(self.r.gd, self.r.base, head)["class"], "version-only")
        k = vo.check(self.r.gd, self.r.base, head)
        self.assertEqual(k["check"], "HOLD")
        self.assertIn("no versioning.json surface", k["reasons"][0])

    def test_crlf_version_line_is_not_a_version_line(self):
        self.r.git("checkout", "-q", "-B", "crlf", self.r.base)
        self.r.write({"CMakeLists.txt": cmake("1.89.0").replace("VERSION 1.89.0", "VERSION 1.89.0\r")})
        head = self.r.commit("crlf")
        self.r.git("checkout", "-q", "main")
        self.assertEqual(vo.classify(self.r.gd, self.r.base, head)["class"], "other")

    def test_the_repo_convention_still_passes_both(self):
        head = self.edit_changelog(lambda l: self.top(l, *block("1.88.0")))
        self.assertEqual(vo.classify(self.r.gd, self.r.base, head, None, "o/r")["class"], "version-only")
        self.assertEqual(vo.check(self.r.gd, self.r.base, head, "o/r")["check"], "APPROVE")


class Shadow(unittest.TestCase):
    def test_logs_only_version_only_entries_once(self):
        r = Repo()
        vhead = r.bump()
        ohead = r.bump(extra={"src/a.cpp": "int b;\n"})
        log = os.path.join(r.dir, "shadow.jsonl")
        lines = [f"o/r#1 {vhead} EARLY", f"o/r#2 {ohead}", f"x/y#3 {vhead}"]
        out = vo.shadow(r.gd, log, "o/r", lines, base="main")
        self.assertEqual([(o["pr"], o["check"]) for o in out], [("o/r#1", "APPROVE")])
        self.assertEqual(vo.shadow(r.gd, log, "o/r", lines, base="main"), [])
        with open(log) as f:
            self.assertEqual(len(f.readlines()), 1)

    def test_report_decides_only_after_ten_agreements_and_no_disagreement(self):
        log = os.path.join(tempfile.mkdtemp(), "s.jsonl")
        with open(log, "w") as f:
            for i in range(10):
                f.write(json.dumps({"pr": f"o/r#{i}", "head": "h", "check": "APPROVE"}) + "\n")
        verdicts = {f"o/r#{i} h": "APPROVE" for i in range(10)}
        self.assertTrue(vo.shadow_report(log, verdicts)["may_decide"])
        self.assertFalse(vo.shadow_report(log, dict(verdicts, **{"o/r#0 h": "HOLD"}))["may_decide"])
        del verdicts["o/r#9 h"]
        rep = vo.shadow_report(log, verdicts)
        self.assertFalse(rep["may_decide"])
        self.assertEqual(rep["pending"], ["o/r#9 h"])


if __name__ == "__main__":
    unittest.main()
