"""The queue-arm guard's rollup rule (``rollup_acceptance``), the twin of
src/rollup.rs ``accept``. A scratch git repository stands in for GitHub, so
every rollup below is built from real merge commits."""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import pathlib
import re
import subprocess
import tempfile
import unittest
from typing import Any
from unittest import mock

SCRIPT = pathlib.Path(__file__).with_name("ghapp_queue_arm_guard.py")
SPEC = importlib.util.spec_from_file_location("ghapp_queue_arm_guard_rollup", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)

REPO = "o/r"
DATE = "2026-10-01T00:00:00Z"
ON = guard.train_config('[train]\nenabled = true\nmode = "rollup"\n')


class Scratch:
    """A repository with a base on ``main`` and plumbing to make commits."""

    def __init__(self, path: pathlib.Path) -> None:
        self.path = path
        path.mkdir(parents=True)
        self.git("init", "-q", "-b", "main")
        base = self.commit([], None, {"a.txt": "a\n", "b.txt": "b\n", "c.txt": "c\n",
                                      "shared.txt": "one\ntwo\nthree\nfour\nfive\nsix\n"})
        self.git("update-ref", "refs/heads/main", base)

    def git(self, *args: str, env: dict[str, str] | None = None, check: bool = True) -> str:
        full = {"PATH": "/usr/bin:/bin:/opt/homebrew/bin", "HOME": str(self.path),
                "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
                "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.com",
                "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.com",
                "GIT_AUTHOR_DATE": DATE, "GIT_COMMITTER_DATE": DATE, **(env or {})}
        done = subprocess.run(["git", *args], cwd=self.path, env=full, capture_output=True,
                              text=True, check=False)
        if check and done.returncode != 0:
            raise AssertionError(f"git {args}: {done.stderr}")
        return done.stdout.strip()

    def commit(self, parents: list[str], tree_of: str | None, files: dict[str, str | None],
               executable: tuple[str, ...] = ()) -> str:
        index = {"GIT_INDEX_FILE": str(self.path / ".git" / "scratch-index")}
        (self.path / ".git" / "scratch-index").unlink(missing_ok=True)
        if tree_of:
            self.git("read-tree", tree_of, env=index)
        for name, content in files.items():
            if content is None:
                self.git("update-index", "--force-remove", name, env=index)
                continue
            blob_path = self.path / ".git" / "scratch-blob"
            blob_path.write_text(content)
            blob = self.git("hash-object", "-w", str(blob_path))
            self.git("update-index", "--add", "--cacheinfo", f"100644,{blob},{name}", env=index)
        for name in executable:
            blob = self.git("ls-files", "-s", name, env=index).split(" ")[1]
            self.git("update-index", "--cacheinfo", f"100755,{blob},{name}", env=index)
        tree = self.git("write-tree", env=index)
        args = ["commit-tree", tree, "-m", "c"]
        for parent in parents:
            args += ["-p", parent]
        return self.git(*args)

    def on(self, parent: str, files: dict[str, str | None]) -> str:
        return self.commit([parent], parent, files)

    def advance_main(self, files: dict[str, str | None]) -> str:
        head = self.on(self.git("rev-parse", "main"), files)
        self.git("update-ref", "refs/heads/main", head)
        return head

    def build(self, base: str, heads: list[str]) -> list[str]:
        """What `shipyard rollup build` commits: merge-tree, then commit-tree."""
        merges, previous = [], base
        for head in heads:
            tree = self.git("merge-tree", "--write-tree", "--no-messages", previous, head)
            previous = self.git("commit-tree", tree.splitlines()[0], "-p", previous, "-p", head,
                                "-m", "merge")
            merges.append(previous)
        return merges

    def compare(self, range_: str) -> dict[str, Any]:
        start, end = (self.git("rev-parse", side) for side in range_.split("..."))
        base = self.git("merge-base", start, end)
        files = []
        for line in self.git("diff", "--raw", "--no-abbrev", "-M", base, end).splitlines():
            meta, _, paths = line.partition("\t")
            _, _, old, new, letter = meta.split(" ")
            names = paths.split("\t")
            status = {"A": "added", "D": "removed", "R": "renamed"}.get(letter[0], "modified")
            item = {"filename": names[-1], "status": status,
                    "sha": old if letter[0] == "D" else new, "patch": "@@"}
            if letter[0] == "R":
                item["previous_filename"] = names[0]
            files.append(item)
        return {"merge_base_commit": {"sha": base}, "files": files}

    def tree(self, expression: str) -> dict[str, Any]:
        commit, _, directory = expression.partition(":")
        listing = self.git("ls-tree", f"{commit}:{directory}" if directory else commit)
        entries = []
        for line in listing.splitlines():
            meta, _, name = line.partition("\t")
            entries.append({"name": name, "mode": int(meta.split(" ")[0], 8)})
        return {"entries": entries}


class Hub:
    """Answers the guard's GitHub reads from a Scratch repository."""

    def __init__(self, scratch: Scratch) -> None:
        self.scratch = scratch
        self.pulls: list[dict[str, Any]] = []

    def pull(self, number: int, branch: str, head: str, head_repo: str = REPO) -> None:
        self.pulls.append({"number": number, "state": "open", "draft": False, "head": {
            "ref": branch, "sha": head, "repo": {"full_name": head_repo}},
            "base": {"ref": "main"}, "user": {"login": "author"}})

    def find(self, number: int) -> dict[str, Any]:
        return next(pull for pull in self.pulls if pull["number"] == number)

    def __call__(self, args: list[str]) -> Any:
        if "graphql" in args:
            query = next(arg for arg in args if arg.startswith("query="))
            expressions = [json.loads(match) for match in
                           re.findall(r'object\(expression:("(?:[^"\\]|\\.)*")\)', query)]
            return {"data": {"repository": {f"d{index}": self.scratch.tree(expression)
                                            for index, expression in enumerate(expressions)}}}
        path = args[-1].removeprefix(f"repos/{REPO}/")
        parts = path.split("/")
        if parts[0] == "compare":
            return self.scratch.compare(parts[1])
        if parts[:3] == ["git", "ref", "heads"]:
            return {"object": {"sha": self.scratch.git("rev-parse", "main")}}
        if parts[0] == "commits" and len(parts) == 2:
            line = self.scratch.git("rev-list", "--parents", "-n", "1", parts[1])
            return {"parents": [{"sha": sha} for sha in line.split(" ")[1:]]}
        if parts[0] == "commits" and parts[2] == "pulls":
            return [pull for pull in self.pulls if pull["head"]["sha"] == parts[1]]
        if parts[0] == "pulls":
            return self.find(int(parts[1]))
        raise AssertionError(f"unexpected read {args}")


def approve_all(_repo: str, _number: int, _head: str, _policy: Any, _counted: bool) -> None:
    return None


class RollupAcceptance(unittest.TestCase):
    def setUp(self) -> None:
        root = tempfile.TemporaryDirectory()
        self.addCleanup(root.cleanup)
        self.root = pathlib.Path(root.name)
        self.scratch = Scratch(self.root / "repo")
        self.base = self.scratch.git("rev-parse", "main")
        self.policy = guard.review_policy(None, REPO, False)
        env = {"HOME": str(self.root), "SHIPYARD_GHAPP_GUARDS_DIR": str(self.root / "guards"),
               "PATH": os.environ.get("PATH", "/usr/bin:/bin")}
        patcher = mock.patch.dict(os.environ, env, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)

    def rollup(self, files: list[str]) -> tuple[Hub, list[str], list[str]]:
        heads = [self.scratch.on(self.base, {name: "changed\n"}) for name in files]
        merges = self.scratch.build(self.base, heads)
        hub = Hub(self.scratch)
        for number, head in zip(range(11, 11 + len(heads)), heads):
            hub.pull(number, f"feature-{number}", head)
        hub.pull(20, "shipyard-train/rollup-11", merges[-1])
        return hub, heads, merges

    def accept(self, hub: Hub, head: str, config: dict[str, Any] = ON,
               review: Any = approve_all, policy: dict[str, Any] | None = None
               ) -> tuple[str | None, list[dict[str, Any]]]:
        with mock.patch.object(guard, "run_real_gh", side_effect=hub):
            pull = dict(hub.find(20), head=dict(hub.find(20)["head"], sha=head))
            return guard.rollup_acceptance(REPO, 20, head, pull, config,
                                           policy or {**self.policy, "carry_excluded": []},
                                           review)

    def test_the_train_table_is_off_by_default_and_refuses_bad_values(self) -> None:
        self.assertFalse(guard.train_config(None)["enabled"])
        self.assertEqual(guard.train_config(None)["exclude_paths"], [])
        for bad in ('[train]\nmode = "fast"\n', "[train]\nmax_members = 5\n",
                    "[train]\nmax_members = true\n", '[train]\nlanding = "push"\n',
                    '[train]\nhot_files = "x"\n', "train = 1\n"):
            with self.assertRaises(guard.GuardError, msg=bad):
                guard.train_config(bad)

    def test_the_kill_switch_and_pause_file_stop_the_train(self) -> None:
        for value in ("off", "0", "false", "off:o/r", "OFF:O/R"):
            self.assertIsNotNone(guard.train_stopped(REPO, value, None), value)
        # The merge train's switch scopes only `off:`.
        for value in ("on", "", "off:other/repo", "false:o/r"):
            self.assertIsNone(guard.train_stopped(REPO, value, None), value)
        (self.root / "guards").mkdir()
        (self.root / "guards" / "train.pause").write_text("")
        self.assertIn("paused", guard.train_stopped(REPO, None, self.root / "guards"))

    def test_a_rollup_of_exact_approved_heads_is_accepted(self) -> None:
        hub, heads, merges = self.rollup(["a.txt", "b.txt"])
        refusal, members = self.accept(hub, merges[-1])
        self.assertIsNone(refusal)
        self.assertEqual([(m["pr"], m["head"], m["merge"]) for m in members],
                         [(11, heads[0], merges[0]), (12, heads[1], merges[1])])

    def test_off_or_stopped_refuses(self) -> None:
        hub, _, merges = self.rollup(["a.txt", "b.txt"])
        refusal, _ = self.accept(hub, merges[-1], guard.train_config(None))
        self.assertIn("does not take rollups", refusal)
        with mock.patch.dict(os.environ, {"SHIPYARD_TRAIN": "off"}):
            refusal, _ = self.accept(hub, merges[-1])
        self.assertIn("SHIPYARD_TRAIN=off", refusal)

    def test_a_member_whose_review_does_not_count_is_refused(self) -> None:
        hub, _, merges = self.rollup(["a.txt", "b.txt"])
        def hold_12(_repo: str, number: int, _head: str, _policy: Any, counted: bool) -> Any:
            self.assertTrue(counted)
            return "member #12: HOLD" if number == 12 else None
        refusal, _ = self.accept(hub, merges[-1], review=hold_12)
        self.assertEqual(refusal, "member #12: HOLD")

    def test_member_review_counts_only_an_exact_head_verdict_by_default(self) -> None:
        hub, heads, _ = self.rollup(["a.txt"])
        earlier = "e" * 40
        comment = {"id": 5, "user": {"login": "shipyard-review[bot]", "type": "Bot"},
                   "author_association": "NONE", "created_at": "2026-10-04T00:00:00Z",
                   "body": f"verdict: APPROVE head:{earlier}"}
        policy = guard.review_policy('[auto_merge]\narm_requires_head_approval = true\n'
                                     'reviewer_identities = ["shipyard-review[bot]"]\n',
                                     REPO, False)
        pages = {f"repos/{REPO}/issues/11/comments?per_page=100": [comment],
                 f"repos/{REPO}/pulls/11/reviews?per_page=100": []}
        with mock.patch.object(guard, "run_real_gh", side_effect=hub), \
                mock.patch.object(guard, "_rest_pages", side_effect=pages.__getitem__), \
                mock.patch.object(guard, "merge_only_carry", return_value=None), \
                mock.patch.object(guard, "carry_limits", return_value=None):
            refusal = guard.member_review(REPO, 11, heads[0], policy, True)
            self.assertIn("member #11", refusal)
            # With require_counted_approval off, the carry rule decides.
            self.assertIsNone(guard.member_review(REPO, 11, heads[0], policy, False))
            comment["body"] = f"verdict: APPROVE head:{heads[0]}"
            self.assertIsNone(guard.member_review(REPO, 11, heads[0], policy, True))
            comment["body"] = f"verdict: HOLD head:{heads[0]}"
            self.assertIsNotNone(guard.member_review(REPO, 11, heads[0], policy, True))

    def test_a_merge_that_carries_another_members_change_is_refused(self) -> None:
        # Merge 1 already brings both changes and merge 2 changes nothing: the
        # head's tree is right, so only the per-merge replay check refuses.
        hub, heads, merges = self.rollup(["a.txt", "b.txt"])
        first = self.scratch.commit([self.base, heads[0]], merges[-1], {})
        second = self.scratch.commit([first, heads[1]], merges[-1], {})
        refusal, _ = self.accept(hub, second)
        self.assertIn("the rollup's merge", refusal)
        self.assertIn("b.txt", refusal)

    def test_a_member_head_shared_by_two_open_pull_requests_is_refused(self) -> None:
        hub, heads, merges = self.rollup(["a.txt", "b.txt"])
        hub.pull(30, "feature-copy", heads[1])
        refusal, _ = self.accept(hub, merges[-1])
        self.assertIn("exactly one open pull request", refusal)

    def test_a_draft_member_is_refused(self) -> None:
        hub, _, merges = self.rollup(["a.txt", "b.txt"])
        hub.find(12)["draft"] = True
        refusal, _ = self.accept(hub, merges[-1])
        self.assertIn("member #12 is a draft", refusal)

    def test_a_moved_member_head_or_a_stray_merge_is_refused(self) -> None:
        hub, _, merges = self.rollup(["a.txt", "b.txt"])
        hub.find(12)["head"]["sha"] = "e" * 40
        refusal, _ = self.accept(hub, merges[-1])
        self.assertIn("not the current head", refusal)

    def test_an_added_commit_or_an_edit_inside_a_merge_is_refused(self) -> None:
        hub, heads, merges = self.rollup(["a.txt", "b.txt"])
        added = self.scratch.on(merges[-1], {"c.txt": "sneaked\n"})
        refusal, _ = self.accept(hub, added)
        self.assertIn("not a two-parent merge", refusal)
        edited = self.scratch.commit([merges[0], heads[1]], merges[-1], {"c.txt": "sneaked\n"})
        refusal, _ = self.accept(hub, edited)
        self.assertIn("c.txt", refusal)
        altered = self.scratch.commit([merges[0], heads[1]], merges[-1], {"b.txt": "other\n"})
        refusal, _ = self.accept(hub, altered)
        self.assertIn("b.txt", refusal)
        chmod = self.scratch.commit([merges[0], heads[1]], merges[-1], {}, ("b.txt",))
        refusal, _ = self.accept(hub, chmod)
        self.assertIn("mode", refusal)

    def test_members_that_share_a_file_are_refused(self) -> None:
        first = self.scratch.on(self.base, {"shared.txt": "ONE\ntwo\nthree\nfour\nfive\nsix\n"})
        second = self.scratch.on(self.base, {"shared.txt": "one\ntwo\nthree\nfour\nfive\nSIX\n"})
        merges = self.scratch.build(self.base, [first, second])
        hub = Hub(self.scratch)
        hub.pull(11, "f1", first)
        hub.pull(12, "f2", second)
        hub.pull(20, "shipyard-train/rollup-11", merges[-1])
        refusal, _ = self.accept(hub, merges[-1])
        self.assertIn("both change shared.txt", refusal)

    def test_a_base_change_to_a_member_file_is_refused(self) -> None:
        member = self.scratch.on(self.base, {"shared.txt": "M\ntwo\nthree\nfour\nfive\nsix\n"})
        base = self.scratch.advance_main({"shared.txt": "one\ntwo\nthree\nfour\nfive\nB\n"})
        merges = self.scratch.build(base, [member])
        hub = Hub(self.scratch)
        hub.pull(11, "f1", member)
        hub.pull(20, "shipyard-train/rollup-11", merges[-1])
        refusal, _ = self.accept(hub, merges[-1])
        self.assertIn("base changed shared.txt", refusal)
        # After forming: the base moving onto a member file refuses; elsewhere it does not.
        hub, _, merges = self.rollup(["a.txt"])
        self.scratch.advance_main({"c.txt": "elsewhere\n"})
        self.assertIsNone(self.accept(hub, merges[-1])[0])
        self.scratch.advance_main({"a.txt": "base\n"})
        self.assertIn("form it again", self.accept(hub, merges[-1])[0])

    def test_excluded_hot_and_too_many_members_are_refused(self) -> None:
        hub, _, merges = self.rollup(["a.txt", "b.txt", "c.txt"])
        self.assertIn("within 2 merges", self.accept(hub, merges[-1], {**ON, "max_members": 2})[0])
        self.assertIn("hot file", self.accept(hub, merges[-1], {**ON, "hot_files": ["*.txt"]})[0])
        self.assertIn("a.txt", self.accept(hub, merges[-1], {**ON, "exclude_paths": ["a.txt"]})[0])
        self.assertIsNone(self.accept(hub, merges[-1])[0])
        workflow = self.scratch.on(self.base, {".github/workflows/ci.yml": "on: push\n"})
        merges = self.scratch.build(self.base, [workflow])
        hub = Hub(self.scratch)
        hub.pull(11, "f1", workflow)
        hub.pull(20, "shipyard-train/rollup-11", merges[-1])
        # The guard's carry exclusions apply to a rollup member.
        refusal, _ = self.accept(hub, merges[-1], policy=self.policy)
        self.assertIn(".github/workflows/ci.yml", refusal)
        # So do they for the base's own config: a member never changes the
        # policy its rollup is judged by.
        config = self.scratch.on(self.base, {".shipyard/config.toml": "[train]\n"})
        merges = self.scratch.build(self.base, [config])
        hub = Hub(self.scratch)
        hub.pull(11, "f1", config)
        hub.pull(20, "shipyard-train/rollup-11", merges[-1])
        refusal, _ = self.accept(hub, merges[-1], policy=self.policy)
        self.assertIn(".shipyard/config.toml", refusal)

    def test_a_fork_or_a_non_rollup_branch_is_refused(self) -> None:
        hub, _, merges = self.rollup(["a.txt"])
        hub.find(20)["head"]["repo"]["full_name"] = "fork/r"
        self.assertIn("not in o/r", self.accept(hub, merges[-1])[0])
        hub.find(20)["head"]["repo"]["full_name"] = REPO
        hub.find(20)["head"]["ref"] = "feature"
        self.assertIn("not a shipyard-train", self.accept(hub, merges[-1])[0])


class RollupReviewGate(unittest.TestCase):
    """The rollup rule as the review gate applies it to a landing."""

    def test_a_rollup_lands_through_the_review_gate_and_a_hold_stops_it(self) -> None:
        root = tempfile.TemporaryDirectory()
        self.addCleanup(root.cleanup)
        scratch = Scratch(pathlib.Path(root.name) / "repo")
        base = scratch.git("rev-parse", "main")
        head = scratch.on(base, {"a.txt": "changed\n"})
        merges = scratch.build(base, [head])
        hub = Hub(scratch)
        hub.pull(11, "f1", head)
        hub.pull(20, "shipyard-train/rollup-11", merges[-1])
        config = ('[auto_merge]\narm_requires_head_approval = true\n'
                  'reviewer_identities = ["shipyard-review[bot]"]\n'
                  '[train]\nenabled = true\nmode = "rollup"\n')
        verdict = {"id": 5, "user": {"login": "shipyard-review[bot]", "type": "Bot"},
                   "author_association": "NONE", "created_at": "2026-10-04T00:00:00Z",
                   "body": f"verdict: APPROVE head:{head}"}
        pages: dict[str, list[Any]] = {
            f"repos/{REPO}/issues/11/comments?per_page=100": [verdict],
            f"repos/{REPO}/pulls/11/reviews?per_page=100": [],
            f"repos/{REPO}/issues/20/comments?per_page=100": [],
            f"repos/{REPO}/pulls/20/reviews?per_page=100": []}
        target = {"repo": REPO, "pr": 20, "head": merges[-1], "bound_head": merges[-1]}
        env = {"HOME": root.name, "SHIPYARD_GHAPP_GUARDS_DIR": str(pathlib.Path(root.name) / "g"),
               "SHIPYARD_REVIEW_BEFORE_ARM": "1", "PATH": os.environ.get("PATH", "/usr/bin")}
        with mock.patch.dict(os.environ, env, clear=True), \
                mock.patch.object(guard, "run_real_gh", side_effect=hub), \
                mock.patch.object(guard, "base_config_text", return_value=config), \
                mock.patch.object(guard, "_rest_pages", side_effect=lambda path: pages[path]), \
                contextlib.redirect_stderr(io.StringIO()):
            allowed, message, _ = guard.review_gate(dict(target), REPO)
            self.assertTrue(allowed, message)
            self.assertIn("a rollup of #11@", message)
            pages[f"repos/{REPO}/issues/20/comments?per_page=100"] = [
                dict(verdict, id=6, body=f"verdict: HOLD head:{merges[-1]}")]
            allowed, message, _ = guard.review_gate(dict(target), REPO)
            self.assertFalse(allowed, message)
            pages[f"repos/{REPO}/issues/20/comments?per_page=100"] = []
            pages[f"repos/{REPO}/issues/11/comments?per_page=100"] = []
            allowed, message, _ = guard.review_gate(dict(target), REPO)
            self.assertFalse(allowed, message)
            self.assertIn("As a rollup it does not land: member #11", message)


if __name__ == "__main__":
    unittest.main()
