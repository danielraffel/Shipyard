#!/usr/bin/env python3
"""The verdict-hold hook, run as `ghapp` runs it: installed names, a real
subprocess, and a fake native `gh` that answers the pull request's live state
and logs every call."""

from __future__ import annotations

import json
import os
import pathlib
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
HOOK = HERE / "ghapp_verdict_hold_hook.py"
REMOVAL = HERE / "ghapp_queue_removal_guard.py"
HEAD = "a" * 40
OLD = "b" * 40


class VerdictHoldHook(unittest.TestCase):
    def setUp(self) -> None:
        self.root = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)
        self.guards = self.root / "guards"
        self.guards.mkdir()
        shutil.copy(HOOK, self.guards / "verdict-hold-hook")
        shutil.copy(REMOVAL, self.guards / "queue-removal-guard")
        self.calls = self.root / "calls"

    def fake_gh(self, state: dict | None, mutation_fails: bool = False) -> pathlib.Path:
        """`state` is the pull request GitHub reports; None fails the read."""
        fake = self.root / "gh"
        fake.write_text(
            f"#!{sys.executable}\n"
            "import json, sys\n"
            "argv = ' '.join(sys.argv[1:])\n"
            f"open({str(self.calls)!r}, 'a').write(argv + '\\n')\n"
            f"state = json.loads({json.dumps(json.dumps(state))})\n"
            f"fails = {mutation_fails!r}\n"
            "if 'mutation' in argv:\n"
            "    if fails:\n"
            "        sys.stderr.write('GraphQL: Resource not accessible by integration\\n'); sys.exit(1)\n"
            "    print(json.dumps({'data': {}})); sys.exit(0)\n"
            "if 'pullRequest(number' in argv:\n"
            "    if state is None:\n"
            "        sys.stderr.write('HTTP 502\\n'); sys.exit(1)\n"
            "    print(json.dumps({'data': {'repository': {'pullRequest': state}}})); sys.exit(0)\n"
            "sys.stderr.write('unexpected gh call: ' + argv + '\\n'); sys.exit(3)\n",
            encoding="utf-8",
        )
        fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
        return fake

    def run_hook(self, argv: list[str], state: dict | None, switch: str | None = "1",
                 mutation_fails: bool = False) -> subprocess.CompletedProcess:
        environment = {
            "PATH": "/usr/bin:/bin",
            "HOME": str(self.root),
            "GHAPP_REAL_GH": str(self.fake_gh(state, mutation_fails)),
            "GH_REPO": "o/r",
            "SHIPYARD_GHAPP_GUARDS_DIR": str(self.guards),
        }
        if switch is not None:
            environment["SHIPYARD_HOLD_DISARMS"] = switch
        return subprocess.run(
            [sys.executable, str(self.guards / "verdict-hold-hook"), *argv],
            env=environment, capture_output=True, text=True, check=False,
            stdin=subprocess.DEVNULL,
        )

    def comment(self, body: str) -> list[str]:
        path = self.root / "comment.json"
        path.write_text(json.dumps({"body": body}), encoding="utf-8")
        return ["api", "repos/o/r/issues/7/comments", "--input", str(path)]

    def logged(self) -> list[str]:
        return self.calls.read_text().splitlines() if self.calls.exists() else []

    def holds(self) -> list[dict]:
        path = self.guards / "holds.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    @staticmethod
    def state(head: str = HEAD, armed: bool = False, queued: bool = False) -> dict:
        return {"id": "PR_7", "headRefOid": head,
                "autoMergeRequest": {"enabledAt": "2026-10-12T22:00:00Z"} if armed else None,
                "mergeQueueEntry": {"id": "MQE_1", "state": "QUEUED"} if queued else None}

    def test_a_hold_on_an_armed_head_disarms_it_before_posting(self) -> None:
        result = self.run_hook(self.comment(f"Delta reviewed.\n\nverdict: HOLD head:{HEAD}"),
                               self.state(armed=True))
        self.assertEqual(result.returncode, 0, result.stderr)
        mutations = [call for call in self.logged() if "mutation" in call]
        self.assertEqual(len(mutations), 1, self.logged())
        self.assertIn("disablePullRequestAutoMerge", mutations[0])
        self.assertIn("id=PR_7", mutations[0])
        self.assertIn("disarmed o/r#7 at aaaaaaaaaaaa", result.stderr)
        self.assertEqual(self.holds()[0]["action"], "disarmed")

    def test_a_hold_on_a_queued_head_dequeues_it(self) -> None:
        result = self.run_hook(self.comment(f"verdict: HOLD head:{HEAD}"),
                               self.state(armed=True, queued=True))
        self.assertEqual(result.returncode, 0, result.stderr)
        mutations = [call for call in self.logged() if "mutation" in call]
        self.assertEqual(len(mutations), 1, self.logged())
        self.assertIn("dequeuePullRequest", mutations[0])
        self.assertEqual(self.holds()[0]["action"], "dequeued")

    def test_a_hold_on_an_older_head_touches_nothing(self) -> None:
        result = self.run_hook(self.comment(f"verdict: HOLD head:{OLD}"),
                               self.state(armed=True, queued=True))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse([call for call in self.logged() if "mutation" in call], self.logged())
        self.assertEqual(self.holds()[0]["why"], "the HOLD names another head")

    def test_an_approve_or_a_quoted_hold_reads_nothing(self) -> None:
        for body in (f"verdict: APPROVE head:{HEAD}",
                     f"verdict: HOLD head:{HEAD}\n\nfixed now, see above",
                     f"> verdict: HOLD head:{HEAD}"):
            result = self.run_hook(self.comment(body), self.state(armed=True))
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.logged(), [], "only a HOLD on the last line acts")

    def test_off_reads_nothing(self) -> None:
        for switch in ("0", None):
            result = self.run_hook(self.comment(f"verdict: HOLD head:{HEAD}"),
                                   self.state(armed=True), switch=switch)
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.logged(), [])
        # The guards-directory file turns it on.
        (self.guards / "hold-disarms").write_text("", encoding="utf-8")
        self.run_hook(self.comment(f"verdict: HOLD head:{HEAD}"), self.state(armed=True),
                      switch=None)
        self.assertTrue(any("disablePullRequestAutoMerge" in call for call in self.logged()))

    def test_a_failure_never_blocks_the_hold_and_says_what_to_do(self) -> None:
        result = self.run_hook(self.comment(f"verdict: HOLD head:{HEAD}"), None)
        self.assertEqual(result.returncode, 0)
        self.assertIn("The HOLD still posts", result.stderr)
        self.assertIn("ghapp pr view 7 --repo o/r", result.stderr)
        self.assertEqual(self.holds()[0]["action"], "failed")
        refused = self.run_hook(self.comment(f"verdict: HOLD head:{HEAD}"),
                                self.state(armed=True), mutation_fails=True)
        self.assertEqual(refused.returncode, 0)
        self.assertIn("could not take o/r#7 out of the landing path", refused.stderr)

    def test_every_posting_form_is_recognised(self) -> None:
        body_file = self.root / "body.md"
        body_file.write_text(f"Holding.\nverdict: HOLD head:{HEAD}\n", encoding="utf-8")
        review = self.root / "review.json"
        review.write_text(json.dumps({"event": "REQUEST_CHANGES", "commit_id": HEAD,
                                      "body": "see inline"}), encoding="utf-8")
        forms = [
            ["api", "-X", "POST", "repos/o/r/issues/7/comments", "-f",
             f"body=verdict: HOLD head:{HEAD}"],
            ["api", "repos/o/r/issues/7/comments", "-F", f"body=@{body_file}"],
            ["pr", "comment", "7", "--repo", "o/r", "--body-file", str(body_file)],
            ["pr", "comment", "7", "--body", f"verdict: HOLD head:{HEAD}"],
            ["api", "repos/o/r/pulls/7/reviews", "--input", str(review)],
        ]
        for form in forms:
            self.calls.unlink(missing_ok=True)
            result = self.run_hook(form, self.state(armed=True))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(any("disablePullRequestAutoMerge" in call for call in self.logged()),
                            (form, self.logged()))
        # A review that approves is not a HOLD.
        review.write_text(json.dumps({"event": "APPROVE", "commit_id": HEAD}), encoding="utf-8")
        self.calls.unlink(missing_ok=True)
        self.run_hook(["api", "repos/o/r/pulls/7/reviews", "--input", str(review)],
                      self.state(armed=True))
        self.assertEqual(self.logged(), [])

    def test_ghapp_runs_the_hook_without_letting_it_refuse(self) -> None:
        wrapper = (HERE / "ghapp").read_text(encoding="utf-8")
        self.assertIn('"$guards/verdict-hold-hook" "$@" || true', wrapper)
        # It runs after the refusing guards, so a refused command never acts.
        self.assertGreater(wrapper.index('"$guards/verdict-hold-hook"'),
                           wrapper.index('"$guards/queue-arm-guard" "$@" || exit 1'))


if __name__ == "__main__":
    unittest.main()
