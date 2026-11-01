#!/usr/bin/env python3
"""Focused acceptance tests for version-at-land and the PR gate."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent


def run(cwd: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, cwd=cwd, text=True, capture_output=True, check=check)


class VersionAtLandTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self.tmp.name)
        run(self.repo, "git", "init", "-q", "-b", "main")
        run(self.repo, "git", "config", "user.email", "test@example.com")
        run(self.repo, "git", "config", "user.name", "Test")
        (self.repo / "scripts").mkdir()
        for name in ("version_bump_check.py", "version_at_land.py", "versioning.json"):
            shutil.copy(HERE / name, self.repo / "scripts" / name)
        (self.repo / "Cargo.toml").write_text('[package]\nname = "shipyard"\nversion = "0.292.0"\n')
        (self.repo / "Cargo.lock").write_text('version = 3\n\n[[package]]\nname = "shipyard"\nversion = "0.292.0"\n')
        (self.repo / ".claude-plugin").mkdir()
        (self.repo / ".claude-plugin/plugin.json").write_text('{"version":"0.111.0"}\n')
        (self.repo / ".claude-plugin/marketplace.json").write_text('{"plugins":[{"version":"0.111.0"}]}\n')
        run(self.repo, "git", "add", ".")
        run(self.repo, "git", "commit", "-qm", "base")
        self.base = run(self.repo, "git", "rev-parse", "HEAD").stdout.strip()

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def gate(self) -> subprocess.CompletedProcess[str]:
        return run(self.repo, "python3", "scripts/version_bump_check.py", "--base", self.base,
                   "--config", "scripts/versioning.json", "--mode=report", check=False)

    def test_pr_without_bump_passes(self) -> None:
        (self.repo / "src").mkdir()
        (self.repo / "src/change.rs").write_text("pub fn change() {}\n")
        run(self.repo, "git", "add", "src/change.rs")
        run(self.repo, "git", "commit", "-qm", "fix: keep release counter out of PR")
        result = self.gate()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("assigned post-merge", result.stdout)

    def test_pr_with_version_edit_is_refused(self) -> None:
        path = self.repo / "Cargo.toml"
        path.write_text(path.read_text().replace("0.292.0", "0.292.1"))
        run(self.repo, "git", "add", "Cargo.toml")
        run(self.repo, "git", "commit", "-qm", "chore: bump version")
        result = self.gate()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("owned by version-at-land", result.stdout)

    def test_cargo_lock_edit_is_refused(self) -> None:
        path = self.repo / "Cargo.lock"
        path.write_text(path.read_text().replace("0.292.0", "0.292.1"))
        run(self.repo, "git", "add", "Cargo.lock")
        run(self.repo, "git", "commit", "-qm", "chore: edit generated lock")
        result = self.gate()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cargo.lock", result.stdout)

    def test_release_exception_is_explicit(self) -> None:
        path = self.repo / "Cargo.toml"
        path.write_text(path.read_text().replace("0.292.0", "0.292.1"))
        run(self.repo, "git", "add", "Cargo.toml")
        run(self.repo, "git", "commit", "-qm", "release: recover\n\nRelease: allow-version-files reason=\"recovery\"")
        result = self.gate()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_empty_release_reason_does_not_bypass_gate(self) -> None:
        path = self.repo / "Cargo.toml"
        path.write_text(path.read_text().replace("0.292.0", "0.292.1"))
        run(self.repo, "git", "add", "Cargo.toml")
        run(self.repo, "git", "commit", "-qm", "release: malformed\n\nRelease: allow-version-files reason=\"\"")
        result = self.gate()
        self.assertNotEqual(result.returncode, 0)

    def test_level_none_change_is_a_noop(self) -> None:
        (self.repo / "commands").mkdir()
        (self.repo / "commands/whitespace.md").write_text("old\n")
        run(self.repo, "git", "add", "commands/whitespace.md")
        run(self.repo, "git", "commit", "-qm", "base plugin file")
        base2 = run(self.repo, "git", "rev-parse", "HEAD").stdout.strip()
        (self.repo / "commands/whitespace.md").write_text("old   \n")
        run(self.repo, "git", "add", "commands/whitespace.md")
        run(self.repo, "git", "commit", "-qm", "docs: whitespace only")
        result = run(self.repo, "python3", "scripts/version_at_land.py", "--base", base2,
                     "--head", "HEAD", "--config", "scripts/versioning.json", "--json")
        self.assertEqual(json.loads(result.stdout)["plan"], [])

    def test_marker_drain_starts_after_applied_writer_commit(self) -> None:
        (self.repo / "src").mkdir()
        (self.repo / "src/first.rs").write_text("pub fn first() {}\n")
        run(self.repo, "git", "add", "src/first.rs")
        run(self.repo, "git", "commit", "-qm", "fix: first")
        first = run(self.repo, "git", "rev-parse", "HEAD").stdout.strip()
        cargo = self.repo / "Cargo.toml"
        cargo.write_text(cargo.read_text().replace("0.292.0", "0.292.1"))
        lock = self.repo / "Cargo.lock"
        lock.write_text(lock.read_text().replace("0.292.0", "0.292.1"))
        run(self.repo, "git", "add", "Cargo.toml", "Cargo.lock")
        run(self.repo, "git", "commit", "-qm", f"chore: assign versions at land\n\nVersion-Bump-Applied: {first}")
        (self.repo / "src/second.rs").write_text("pub fn second() {}\n")
        run(self.repo, "git", "add", "src/second.rs")
        run(self.repo, "git", "commit", "-qm", "fix: second")
        result = run(self.repo, "python3", "scripts/version_at_land.py", "--base", self.base,
                     "--head", "HEAD", "--config", "scripts/versioning.json", "--json")
        payload = json.loads(result.stdout)
        self.assertEqual(payload["plan"][0]["current"], "0.292.1")
        self.assertEqual(payload["plan"][0]["assigned"], "0.292.2")

    def test_release_workflows_fail_closed_without_bot_token(self) -> None:
        for name in ("version-at-land.yml", "auto-release.yml"):
            workflow = (HERE.parent / ".github/workflows" / name).read_text()
            self.assertNotIn("|| secrets.GITHUB_TOKEN", workflow)
            self.assertIn("RELEASE_BOT_TOKEN is required", workflow)

    def test_writer_workflow_lands_through_the_bump_pr(self) -> None:
        workflow = (HERE.parent / ".github/workflows/version-at-land.yml").read_text()
        self.assertIn("--push --pr-branch release/version-at-land", workflow)
        # Events from the default token start no workflows, so the PR's
        # required checks would never run.
        self.assertIn("GH_TOKEN: ${{ secrets.RELEASE_BOT_TOKEN }}", workflow)
        self.assertNotIn("contents: write", workflow)

    def test_gate_has_no_apply_mode(self) -> None:
        result = run(self.repo, "python3", "scripts/version_bump_check.py",
                     "--mode=apply", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid choice", result.stderr)

    def test_two_landed_changes_coalesce_without_version_conflict(self) -> None:
        (self.repo / "src").mkdir()
        (self.repo / "src/cli.rs").write_text("pub fn cli() {}\n")
        run(self.repo, "git", "add", "src/cli.rs")
        run(self.repo, "git", "commit", "-qm", "fix: cli")
        (self.repo / "commands").mkdir()
        (self.repo / "commands/new.md").write_text("new command\n")
        run(self.repo, "git", "add", "commands/new.md")
        run(self.repo, "git", "commit", "-qm", "feat: plugin")
        result = run(self.repo, "python3", "scripts/version_at_land.py", "--base", self.base,
                     "--head", "HEAD", "--config", "scripts/versioning.json", "--json")
        payload = json.loads(result.stdout)
        self.assertEqual(payload["status"], "dry-run")
        self.assertEqual({item["surface"] for item in payload["plan"]}, {"cli", "plugin"})
        self.assertEqual({item["assigned"] for item in payload["plan"]}, {"0.292.1", "0.112.0"})

    def _land(self, path: str, body: str, message: str) -> None:
        target = self.repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(body)
        run(self.repo, "git", "add", path)
        run(self.repo, "git", "commit", "-qm", message)

    def _plan(self) -> dict:
        result = run(self.repo, "python3", "scripts/version_at_land.py", "--base", self.base,
                     "--head", "HEAD", "--config", "scripts/versioning.json", "--json")
        return json.loads(result.stdout)

    def test_skip_trailer_covers_only_its_own_change(self) -> None:
        self._land("src/telemetry.rs", "pub fn shadow() {}\n",
                   'feat: internal telemetry\n\nVersion-Bump: cli=skip reason="internal"')
        self._land("src/cli.rs", "pub fn cli() {}\n", "fix: cli")
        payload = self._plan()
        self.assertEqual(payload["status"], "dry-run")
        self.assertEqual([(item["surface"], item["assigned"]) for item in payload["plan"]],
                         [("cli", "0.292.1")])

    def test_skip_trailer_alone_still_assigns_nothing(self) -> None:
        self._land("src/telemetry.rs", "pub fn shadow() {}\n",
                   'feat: internal telemetry\n\nVersion-Bump: cli=skip reason="internal"')
        self.assertEqual(self._plan()["plan"], [])

    def test_skipped_side_branch_merged_after_marker_does_not_block_later_landings(self) -> None:
        run(self.repo, "git", "checkout", "-qb", "side")
        self._land("src/telemetry.rs", "pub fn shadow() {}\n",
                   'feat: internal telemetry\n\nVersion-Bump: cli=skip reason="internal"')
        run(self.repo, "git", "checkout", "-q", "main")
        run(self.repo, "git", "merge", "-q", "--no-ff", "side", "-m", "Merge side")
        self._land("src/cli.rs", "pub fn cli() {}\n", "fix: cli")
        self.assertEqual([item["assigned"] for item in self._plan()["plan"]], ["0.292.1"])


FAKE_GH = r"""#!/usr/bin/env python3
import json, os, sys
state_path = os.environ["FAKE_GH_STATE"]
state = json.load(open(state_path)) if os.path.exists(state_path) else {"pulls": [], "calls": []}
args = sys.argv[1:]
assert args[0] == "api", args
args = args[1:]
method = "GET"
if args[:1] == ["-X"]:
    method, args = args[1], args[2:]
path, fields = args[0], {}
rest = args[1:]
while rest:
    assert rest[0] == "-f", rest
    key, _, value = rest[1].partition("=")
    fields.setdefault(key, []).append(value)
    rest = rest[2:]
state["calls"].append({"method": method, "path": path, "fields": fields})
fail = os.environ.get("FAKE_GH_FAIL", "")
if fail and fail.split(" ", 1)[0] == method and fail.split(" ", 1)[1] in path:
    json.dump(state, open(state_path, "w"))
    print(json.dumps({"message": "Resource not accessible by personal access token",
                      "status": "403"}))
    sys.stderr.write("gh: Resource not accessible by personal access token (HTTP 403)\n")
    sys.exit(1)
out = None
repo = "/".join(path.split("/")[1:3])
if method == "GET" and "/pulls?" in path:
    out = [{"number": p["number"], "head": {"ref": p["head"], "repo": {"full_name": repo}}}
           for p in state["pulls"] if p["state"] == "open"]
elif method == "POST" and path.endswith("/pulls"):
    number = 100 + len(state["pulls"])
    state["pulls"].append({"number": number, "state": "open", "head": fields["head"][0],
                           "title": fields["title"][0], "labels": [], "statuses": []})
    out = {"number": number}
elif method == "PATCH" and "/pulls/" in path:
    number = int(path.rsplit("/", 1)[1])
    for p in state["pulls"]:
        if p["number"] == number:
            p["title"] = fields["title"][0]
elif method == "POST" and "/labels" in path:
    number = int(path.split("/")[-2])
    for p in state["pulls"]:
        if p["number"] == number:
            p["labels"] = sorted(set(p["labels"]) | set(fields["labels[]"]))
elif method == "POST" and "/statuses/" in path:
    state.setdefault("statuses", []).append({"sha": path.rsplit("/", 1)[1],
        **{k: v[0] for k, v in fields.items()}})
else:
    sys.exit(f"unexpected call {method} {path}")
json.dump(state, open(state_path, "w"))
if out is not None:
    print(json.dumps(out))
"""

PR_BRANCH = "release/version-at-land"


class VersionAtLandPullRequestTests(unittest.TestCase):
    """The bump reaches main only through one coalesced, steward-managed PR."""

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.dev = root / "dev"
        self.origin = root / "origin.git"
        self.ci = root / "ci"
        self.state = root / "gh-state.json"
        self.fake_gh = root / "fake-gh"
        self.fake_gh.write_text(FAKE_GH)
        self.fake_gh.chmod(0o755)
        self.dev.mkdir()
        run(self.dev, "git", "init", "-q", "-b", "main")
        for repo_cfg in (("user.email", "test@example.com"), ("user.name", "Test")):
            run(self.dev, "git", "config", *repo_cfg)
        (self.dev / "scripts").mkdir()
        for name in ("version_bump_check.py", "version_at_land.py", "versioning.json",
                     "skill_sync_check.py", "skill_path_map.json",
                     "doc_sync_check.py", "doc_sync_map.json"):
            shutil.copy(HERE / name, self.dev / "scripts" / name)
        shutil.copytree(ROOT / "skills", self.dev / "skills")
        (self.dev / "Cargo.toml").write_text('[package]\nname = "shipyard"\nversion = "0.292.0"\n')
        (self.dev / "Cargo.lock").write_text('version = 3\n\n[[package]]\nname = "shipyard"\nversion = "0.292.0"\n')
        (self.dev / ".claude-plugin").mkdir()
        (self.dev / ".claude-plugin/plugin.json").write_text('{"version":"0.111.0"}\n')
        (self.dev / ".claude-plugin/marketplace.json").write_text('{"plugins":[{"version":"0.111.0"}]}\n')
        run(self.dev, "git", "add", ".")
        run(self.dev, "git", "commit", "-qm", "base")
        # Main always carries the marker of the last released bump.
        init = run(self.dev, "git", "rev-parse", "HEAD").stdout.strip()
        run(self.dev, "git", "commit", "-q", "--allow-empty", "-m",
            f"chore: assign versions at land\n\nVersion-Bump-Applied: {init}")
        run(root, "git", "clone", "-q", "--bare", str(self.dev), str(self.origin))
        run(self.dev, "git", "remote", "add", "origin", str(self.origin))
        run(self.dev, "git", "fetch", "-q", "origin")
        run(root, "git", "clone", "-q", str(self.origin), str(self.ci))
        for repo_cfg in (("user.email", "bot@example.com"), ("user.name", "Bot")):
            run(self.ci, "git", "config", *repo_cfg)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def land(self, path: str, body: str, subject: str) -> str:
        run(self.dev, "git", "fetch", "-q", "origin")
        run(self.dev, "git", "reset", "-q", "--hard", "origin/main")
        target = self.dev / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(body)
        run(self.dev, "git", "add", path)
        run(self.dev, "git", "commit", "-qm", subject)
        run(self.dev, "git", "push", "-q", "origin", "HEAD:main")
        return run(self.dev, "git", "rev-parse", "HEAD").stdout.strip()

    def merge_bump(self, ref: str | None = None) -> str:
        """Merge the bump PR the way GitHub's MERGE method does."""
        run(self.dev, "git", "fetch", "-q", "origin")
        run(self.dev, "git", "reset", "-q", "--hard", "origin/main")
        run(self.dev, "git", "merge", "-q", "--no-ff", ref or f"origin/{PR_BRANCH}", "-m",
            f"Merge pull request #100 from owner/{PR_BRANCH}\n\nchore: assign versions at land")
        run(self.dev, "git", "push", "-q", "origin", "HEAD:main")
        return run(self.dev, "git", "rev-parse", "HEAD").stdout.strip()

    def run_writer(self, **extra_env: str) -> subprocess.CompletedProcess[str]:
        env = {**os.environ, "VERSION_AT_LAND_GH": str(self.fake_gh),
               "FAKE_GH_STATE": str(self.state), "GITHUB_REPOSITORY": "owner/shipyard",
               **extra_env}
        return subprocess.run(
            ["python3", "scripts/version_at_land.py", "--push", "--pr-branch", PR_BRANCH,
             "--json"], cwd=self.ci, env=env, text=True, capture_output=True)

    def writer(self) -> dict:
        result = self.run_writer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return json.loads(result.stdout[result.stdout.index("{"):])

    def gh_state(self) -> dict:
        return json.loads(self.state.read_text()) if self.state.exists() else {
            "pulls": [], "calls": [], "statuses": []}

    def origin_rev(self, ref: str) -> str:
        return run(self.origin, "git", "rev-parse", ref).stdout.strip()

    def version_on(self, ref: str, path: str = "Cargo.toml") -> str:
        text = run(self.origin, "git", "show", f"{ref}:{path}").stdout
        return text.split('version = "', 1)[1].split('"', 1)[0]

    def assert_branch_is_one_bump_on_main(self) -> str:
        head = self.origin_rev(PR_BRANCH)
        self.assertEqual(self.origin_rev(f"{PR_BRANCH}^1"), self.origin_rev("main"))
        return head

    def test_opens_one_managed_pr_and_never_pushes_main(self) -> None:
        main_before = self.land("src/a.rs", "pub fn a() {}\n", "fix: a")
        payload = self.writer()
        self.assertEqual(payload["status"], "opened")
        self.assertEqual(self.origin_rev("main"), main_before)
        head = self.assert_branch_is_one_bump_on_main()
        self.assertEqual(self.version_on(PR_BRANCH), "0.292.1")
        self.assertIn('version = "0.292.1"', run(self.origin, "git", "show",
                                                 f"{PR_BRANCH}:Cargo.lock").stdout)
        state = self.gh_state()
        self.assertEqual([(p["number"], p["head"], p["labels"]) for p in state["pulls"]],
                         [(100, PR_BRANCH, ["shipyard:managed"])])
        self.assertEqual([(s["sha"], s["context"], s["state"]) for s in state["statuses"]],
                         [(head, "shipyard/steward-handoff", "success")])
        # The PR gate admits the bump commit through its explicit release trailer.
        run(self.ci, "git", "fetch", "-q", "origin")
        run(self.ci, "git", "checkout", "-q", head)
        gate = run(self.ci, "python3", "scripts/version_bump_check.py", "--base",
                   "origin/main", "--config", "scripts/versioning.json", "--mode=report",
                   check=False)
        self.assertEqual(gate.returncode, 0, gate.stdout + gate.stderr)

    def test_second_landing_updates_the_same_branch_and_pr(self) -> None:
        self.land("src/a.rs", "pub fn a() {}\n", "fix: a")
        self.writer()
        first = self.origin_rev(PR_BRANCH)
        self.land("src/b.rs", "pub fn b() {}\n", "fix: b")
        payload = self.writer()
        self.assertEqual(payload["status"], "updated")
        head = self.assert_branch_is_one_bump_on_main()
        self.assertNotEqual(head, first)
        state = self.gh_state()
        self.assertEqual(len(state["pulls"]), 1)
        self.assertEqual(sum(1 for c in state["calls"] if c["method"] == "POST"
                             and c["path"].endswith("/pulls")), 1)
        self.assertEqual(state["statuses"][-1]["sha"], head)

    def test_two_features_coalesce_into_one_cumulative_bump(self) -> None:
        self.land("src/a.rs", "pub fn a() {}\n", "fix: a")
        self.writer()
        self.land("src/b.rs", "pub fn b() {}\n", "fix: b")
        self.writer()
        # Two patch-level landings are one patch bump from the released
        # version, not a bump stacked on the unmerged one.
        self.assertEqual(self.version_on(PR_BRANCH), "0.292.1")
        self.assertEqual(self.version_on(f"{PR_BRANCH}^1"), "0.292.0")
        self.land("commands/new.md", "new command\n", "feat: plugin command")
        plan = {a["surface"]: a["assigned"] for a in self.writer()["plan"]}
        self.assertEqual(plan, {"cli": "0.292.1", "plugin": "0.112.0"})
        self.assertEqual(self.version_on(PR_BRANCH), "0.292.1")
        self.assertEqual(len(self.gh_state()["pulls"]), 1)

    def test_rerun_without_new_landing_keeps_the_approved_head(self) -> None:
        self.land("src/a.rs", "pub fn a() {}\n", "fix: a")
        self.writer()
        head = self.origin_rev(PR_BRANCH)
        self.assertEqual(self.writer()["status"], "updated")
        self.assertEqual(self.origin_rev(PR_BRANCH), head)

    def test_versionless_landing_keeps_the_approved_head(self) -> None:
        self.land("src/a.rs", "pub fn a() {}\n", "fix: a")
        self.writer()
        head = self.origin_rev(PR_BRANCH)
        self.land("docs/notes.md", "notes\n", "docs: notes")
        self.assertEqual(self.writer()["status"], "updated")
        self.assertEqual(self.origin_rev(PR_BRANCH), head)
        self.assertEqual(self.gh_state()["statuses"][-1]["sha"], head)
        # Merging that head leaves nothing to assign: the docs change needs none.
        self.merge_bump()
        self.assertEqual(self.writer()["status"], "noop")

    def test_bump_pr_merge_is_a_noop_then_next_landing_bumps_from_it(self) -> None:
        self.land("src/a.rs", "pub fn a() {}\n", "fix: a")
        self.writer()
        merged = self.merge_bump()
        state = json.loads(self.state.read_text())
        state["pulls"][0]["state"] = "closed"
        self.state.write_text(json.dumps(state))
        calls = len(state["calls"])
        self.assertEqual(self.version_on("main"), "0.292.1")
        self.assertEqual(self.writer()["status"], "noop")
        self.assertEqual(len(self.gh_state()["calls"]), calls)
        self.assertEqual(self.origin_rev("main"), merged)
        self.land("src/c.rs", "pub fn c() {}\n", "fix: c")
        payload = self.writer()
        self.assertEqual(payload["status"], "opened")
        self.assertEqual([(a["current"], a["assigned"]) for a in payload["plan"]],
                         [("0.292.1", "0.292.2")])

    def bump_message(self, ref: str) -> str:
        return run(self.origin, "git", "log", "-1", "--format=%B", ref).stdout

    def test_bump_commit_carries_skip_trailers_for_mapped_skills(self) -> None:
        self.land("src/a.rs", "pub fn a() {}\n", "fix: a")
        self.land("commands/new.md", "new command\n", "feat: plugin command")
        self.writer()
        body = self.bump_message(PR_BRANCH)
        self.assertIn('Skill-Update: skip skill=ci reason="version-at-land bump: version files only"',
                      body)
        self.assertIn("Release: allow-version-files", body)

    def test_bump_commit_passes_the_skill_doc_and_version_gates(self) -> None:
        self.land("src/a.rs", "pub fn a() {}\n", "fix: a")
        self.land("commands/new.md", "new command\n", "feat: plugin command")
        self.writer()
        run(self.ci, "git", "fetch", "-q", "origin")
        base, head = "origin/main", f"origin/{PR_BRANCH}"
        for script, extra in (("skill_sync_check.py", ["--config", "scripts/versioning.json"]),
                              ("doc_sync_check.py", []),
                              ("version_bump_check.py", ["--config", "scripts/versioning.json"])):
            result = run(self.ci, "python3", f"scripts/{script}", "--base", base,
                         "--head", head, *extra, "--mode=report", check=False)
            self.assertEqual(result.returncode, 0, script + result.stdout + result.stderr)

    def test_existing_head_without_the_trailers_is_rebuilt(self) -> None:
        self.land("src/a.rs", "pub fn a() {}\n", "fix: a")
        self.writer()
        old = self.origin_rev(PR_BRANCH)
        # Recreate the head as an older writer built it: no skip trailers.
        run(self.dev, "git", "fetch", "-q", "origin")
        run(self.dev, "git", "checkout", "-q", "-B", "stale", f"origin/{PR_BRANCH}")
        stripped = "".join(line + "\n" for line in self.bump_message(PR_BRANCH).splitlines()
                           if not line.startswith("Skill-Update:"))
        run(self.dev, "git", "commit", "-q", "--amend", "-m", stripped)
        run(self.dev, "git", "push", "-q", "-f", "origin", f"HEAD:refs/heads/{PR_BRANCH}")
        stale = self.origin_rev(PR_BRANCH)
        self.assertNotEqual(stale, old)
        self.assertNotIn("Skill-Update", self.bump_message(PR_BRANCH))
        self.writer()
        rebuilt = self.origin_rev(PR_BRANCH)
        self.assertNotEqual(rebuilt, stale)
        self.assertIn("Skill-Update: skip skill=ci", self.bump_message(PR_BRANCH))
        # With the trailers present the head is then kept.
        self.writer()
        self.assertEqual(self.origin_rev(PR_BRANCH), rebuilt)

    def test_workflow_can_be_dispatched_by_an_operator(self) -> None:
        workflow = (ROOT / ".github/workflows/version-at-land.yml").read_text()
        self.assertIn("workflow_dispatch:", workflow)

    def test_refused_pull_request_call_prints_ghs_error_and_the_fix(self) -> None:
        self.land("src/a.rs", "pub fn a() {}\n", "fix: a")
        result = self.run_writer(FAKE_GH_FAIL="POST /pulls")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertNotIn("Traceback", result.stderr)
        self.assertIn("gh api POST repos/owner/shipyard/pulls failed", result.stderr)
        self.assertIn("Resource not accessible by personal access token (HTTP 403)",
                      result.stderr)
        self.assertIn("Pull requests: Read and write", result.stderr)
        payload = json.loads(result.stdout[result.stdout.index("{"):])
        self.assertEqual(payload["status"], "github-api-failed")
        self.assertEqual(payload["path"], "repos/owner/shipyard/pulls")
        self.assertIn("HTTP 403", payload["stderr"])

    def test_refused_update_of_an_open_pull_request_is_reported(self) -> None:
        self.land("src/a.rs", "pub fn a() {}\n", "fix: a")
        self.writer()
        self.land("src/b.rs", "pub fn b() {}\n", "feat: b")
        result = self.run_writer(FAKE_GH_FAIL="PATCH /pulls/100")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("gh api PATCH repos/owner/shipyard/pulls/100 failed", result.stderr)
        self.assertIn("Commit statuses: Read and write", result.stderr)

    def test_landing_while_bump_pr_was_open_is_not_lost(self) -> None:
        self.land("src/a.rs", "pub fn a() {}\n", "fix: a")
        self.writer()
        stale = self.origin_rev(PR_BRANCH)
        # Main does not require up-to-date branches: the bump PR can merge
        # before the writer has rebuilt it for a later landing.
        self.land("src/b.rs", "pub fn b() {}\n", "fix: b")
        self.merge_bump(stale)
        payload = self.writer()
        self.assertEqual([(a["current"], a["assigned"]) for a in payload["plan"]],
                         [("0.292.1", "0.292.2")])


if __name__ == "__main__":
    unittest.main()
