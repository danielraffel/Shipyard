#!/usr/bin/env python3
"""The verdict-contradiction guard, run as `ghapp` runs it: installed names
next to the managed queue-arm and reviewer-claim guards, a real subprocess, and
a fake native `gh` that answers the pull request, its comments and reviews."""

from __future__ import annotations

import importlib.util
import json
import os
import pathlib
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone

HERE = pathlib.Path(__file__).resolve().parent
GUARD = HERE / "ghapp_verdict_contradiction_guard.py"
ARM = HERE / "ghapp_queue_arm_guard.py"
CLAIM = HERE / "ghapp_reviewer_claim_guard.py"
HEAD = "e" * 40
OTHER = "b" * 40
REPO = "Generous-Corp/pulp"  # review required by default: the App's verdicts count
NOW = datetime(2026, 11, 9, 5, 13, 50, tzinfo=timezone.utc)
HOST = socket.gethostname().split(".")[0]
APP = {"login": "shipyard-local[bot]", "type": "Bot"}


def _stamp(session: str = "team-lead", agent: str | None = "pulp-review",
           host: str = HOST) -> str:
    tail = f" agent: {agent}" if agent else ""
    return f"<!-- session: {host}/{session}{tail} -->"


def comment(verdict: str, head: str = HEAD, *, ident: int = 1, user: dict | None = None,
            association: str = "NONE", at: datetime = NOW - timedelta(seconds=5),
            stamp: str | None = None, signer: str = "pulp-review") -> dict:
    stamp = _stamp() if stamp is None else stamp
    return {
        "id": ident,
        "html_url": f"https://github.com/{REPO}/pull/7#issuecomment-{ident}",
        "user": user or APP,
        "author_association": association,
        "created_at": at.strftime("%Y-%m-%dT%H:%M:%SZ"),
        "body": f"review notes\nreviewer: {signer}\n{stamp}\nverdict: {verdict} head:{head}",
    }


class VerdictContradictionGuard(unittest.TestCase):
    def setUp(self) -> None:
        self.root = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)
        self.guards = self.root / "guards"
        self.guards.mkdir()
        shutil.copy(GUARD, self.guards / "verdict-contradiction-guard")
        shutil.copy(ARM, self.guards / "queue-arm-guard")
        shutil.copy(CLAIM, self.guards / "reviewer-claim-guard")
        self.calls = self.root / "calls"
        self.state = self.root / "state.json"

    def fake_gh(self, comments: list, reviews: list | None = None, *, fail: bool = False,
                head: str = HEAD) -> pathlib.Path:
        self.state.write_text(json.dumps({
            "comments": comments, "reviews": reviews or [], "fail": fail,
            "pull": {"number": 7, "head": {"sha": head}, "base": {"ref": "main"},
                     "user": {"login": "author-agent"}, "state": "open"},
        }), encoding="utf-8")
        fake = self.root / "gh"
        fake.write_text(
            f"#!{sys.executable}\n"
            "import json, sys\n"
            "argv = ' '.join(sys.argv[1:])\n"
            f"open({str(self.calls)!r}, 'a').write(argv + '\\n')\n"
            f"state = json.load(open({str(self.state)!r}))\n"
            "if state['fail']:\n"
            "    sys.stderr.write('HTTP 502: Bad Gateway\\n'); sys.exit(1)\n"
            "if 'contents/.shipyard/config.toml' in argv:\n"
            "    sys.stderr.write('HTTP 404: Not Found\\n'); sys.exit(1)\n"
            "if 'pulls/7/reviews' in argv:\n"
            "    print(json.dumps([state['reviews']])); sys.exit(0)\n"
            "if 'issues/7/comments' in argv and '--paginate' in argv:\n"
            "    print(json.dumps([state['comments']])); sys.exit(0)\n"
            "if argv.endswith('pulls/7'):\n"
            "    print(json.dumps(state['pull'])); sys.exit(0)\n"
            "if 'node(id' in argv:\n"
            f"    print(json.dumps({{'data': {{'node': {{'number': 7, 'repository': {{'nameWithOwner': {REPO!r}}}}}}}}})); sys.exit(0)\n"
            "if argv.startswith('pr view'):\n"
            "    print(json.dumps({'number': 7})); sys.exit(0)\n"
            "sys.stderr.write('unexpected gh call: ' + argv + '\\n'); sys.exit(3)\n",
            encoding="utf-8",
        )
        fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
        return fake

    def run_guard(self, argv: list[str], comments: list, reviews: list | None = None, *,
                  fail: bool = False, env: dict | None = None, stdin=subprocess.DEVNULL,
                  repo: str = REPO) -> subprocess.CompletedProcess:
        environment = {
            "PATH": "/usr/bin:/bin",
            "HOME": str(self.root),
            "GHAPP_REAL_GH": str(self.fake_gh(comments, reviews, fail=fail)),
            "GH_REPO": repo,
            "SHIPYARD_GHAPP_GUARDS_DIR": str(self.guards),
            "SHIPYARD_VERDICT_GUARD_NOW": NOW.isoformat(),
            "SHIPYARD_STAMP_SESSION_ID": "team-lead",
            "SHIPYARD_AGENT_ID": "pulp-review",
            **(env or {}),
        }
        return subprocess.run(
            [sys.executable, str(self.guards / "verdict-contradiction-guard"), *argv],
            env=environment, capture_output=True, text=True, check=False, stdin=stdin,
        )

    @staticmethod
    def post(verdict: str, head: str = HEAD) -> list[str]:
        return ["api", "-X", "POST", f"repos/{REPO}/issues/7/comments", "--raw-field",
                f"body=looks fine\nreviewer: pulp-review\n{_stamp()}\nverdict: {verdict} head:{head}"]

    # ------------------------------------------------ rule 1: held head

    def test_incident_approve_five_seconds_after_a_hold_is_refused(self) -> None:
        hold = comment("HOLD", ident=4321)
        result = self.run_guard(self.post("APPROVE"), [hold])
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(f"head {HEAD} is held by pulp-review at "
                      f"https://github.com/{REPO}/pull/7#issuecomment-4321", result.stderr)
        self.assertIn("a HOLD makes a head un-approvable", result.stderr)
        self.assertIn("the author must push a new head (merge the base)", result.stderr)
        self.assertIn("do not approve this head", result.stderr)

    def test_a_hold_from_another_signer_also_holds(self) -> None:
        hold = comment("HOLD", signer="infra-review2", stamp=_stamp("other", "infra-review2"),
                       at=NOW - timedelta(days=2))
        result = self.run_guard(self.post("APPROVE"), [hold])
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("held by infra-review2", result.stderr)

    def assert_held(self, result: subprocess.CompletedProcess) -> None:
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(f"head {HEAD} is held by", result.stderr)

    def test_reviewed_marker_is_an_approve_too(self) -> None:
        argv = ["api", f"repos/{REPO}/issues/7/comments", "-f", f"body=reviewed: {HEAD}"]
        self.assert_held(self.run_guard(argv, [comment("HOLD")],
                                        env={"SHIPYARD_AGENT_ID": "someone-else"}))

    def test_a_hold_on_another_head_does_not_hold_this_one(self) -> None:
        result = self.run_guard(self.post("APPROVE"), [comment("HOLD", OTHER)])
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_trusted_members_changes_requested_review_holds(self) -> None:
        review = {"id": 9, "html_url": f"https://github.com/{REPO}/pull/7#pullrequestreview-9",
                  "user": {"login": "danielraffel", "type": "User"},
                  "author_association": "MEMBER", "state": "CHANGES_REQUESTED",
                  "commit_id": HEAD, "submitted_at": "2026-11-09T05:00:00Z", "body": ""}
        result = self.run_guard(self.post("APPROVE"), [], [review])
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("#pullrequestreview-9", result.stderr)

    def test_a_drive_by_hold_the_gate_ignores_does_not_hold(self) -> None:
        stranger = comment("HOLD", user={"login": "stranger", "type": "User"},
                           association="NONE")
        result = self.run_guard(self.post("APPROVE"), [stranger])
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_bodyless_review_approve_of_a_held_live_head_is_refused(self) -> None:
        result = self.run_guard(["pr", "review", "7", "--approve"], [comment("HOLD")])
        self.assert_held(result)

    def test_rest_and_graphql_review_approvals_are_judged(self) -> None:
        rest = ["api", "-X", "POST", f"repos/{REPO}/pulls/7/reviews", "-f", "event=APPROVE",
                "-f", f"commit_id={HEAD}"]
        self.assert_held(self.run_guard(rest, [comment("HOLD")]))
        graphql = ["api", "graphql", "-f",
                   "query=mutation{addPullRequestReview(input:{pullRequestId:\"PR_7\","
                   f"event:APPROVE,commitOID:\"{HEAD}\"}}){{clientMutationId}}}}"]
        self.assert_held(self.run_guard(graphql, [comment("HOLD")]))

    def test_editing_a_comment_into_an_approve_of_a_held_head_is_refused(self) -> None:
        # The edited comment's own pull request comes from its issue_url.
        argv = ["api", "-X", "PATCH", f"repos/{REPO}/issues/comments/55", "--raw-field",
                f"body=verdict: APPROVE head:{HEAD}"]
        fake_extra = self.root / "gh"
        result = None
        self.fake_gh([comment("HOLD")])
        source = fake_extra.read_text(encoding="utf-8").replace(
            "if state['fail']:",
            "if 'issues/comments/55' in argv:\n"
            f"    print('https://api.github.com/repos/{REPO}/issues/7'); sys.exit(0)\n"
            "if state['fail']:")
        fake_extra.write_text(source, encoding="utf-8")
        environment = {
            "PATH": "/usr/bin:/bin", "HOME": str(self.root), "GHAPP_REAL_GH": str(fake_extra),
            "GH_REPO": REPO, "SHIPYARD_GHAPP_GUARDS_DIR": str(self.guards),
            "SHIPYARD_VERDICT_GUARD_NOW": NOW.isoformat(),
        }
        result = subprocess.run(
            [sys.executable, str(self.guards / "verdict-contradiction-guard"), *argv],
            env=environment, capture_output=True, text=True, check=False,
            stdin=subprocess.DEVNULL)
        self.assert_held(result)

    def test_an_approve_read_from_spooled_stdin_is_judged_and_stdin_rewound(self) -> None:
        body = self.root / "spooled.json"
        body.write_text(json.dumps({"body": f"verdict: APPROVE head:{HEAD}"}), encoding="utf-8")
        argv = ["api", f"repos/{REPO}/issues/7/comments", "--input", "-"]
        with body.open("rb") as handle:
            result = self.run_guard(argv, [comment("HOLD")], stdin=handle)
            self.assertEqual(handle.tell(), 0, "native gh must still read the whole body")
        self.assert_held(result)

    def test_unreadable_state_refuses_an_approve_but_posts_a_hold(self) -> None:
        approve = self.run_guard(self.post("APPROVE"), [], fail=True)
        self.assertEqual(approve.returncode, 1)
        self.assertIn("an APPROVE is not posted unchecked", approve.stderr)
        hold = self.run_guard(self.post("HOLD"), [], fail=True)
        self.assertEqual(hold.returncode, 0, hold.stderr)
        self.assertIn("it posts anyway", hold.stderr)

    def test_a_hold_is_never_refused_by_rule_one(self) -> None:
        result = self.run_guard(self.post("HOLD"), [comment("HOLD", ident=2)])
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_non_verdict_requests_read_nothing(self) -> None:
        argv = ["api", f"repos/{REPO}/issues/7/comments", "-f", "body=status only"]
        self.assertEqual(self.run_guard(argv, []).returncode, 0)
        self.assertFalse(self.calls.exists(), "a non-verdict request must not read GitHub")

    def test_one_request_with_both_verdicts_is_refused(self) -> None:
        argv = ["pr", "review", "7", "--approve", "--body", f"verdict: HOLD head:{HEAD}"]
        result = self.run_guard(argv, [])
        self.assertEqual(result.returncode, 1)
        self.assertIn("both an APPROVE and a HOLD", result.stderr)

    # ------------------------------------------------ rule 2: own contradiction

    def test_a_hold_right_after_this_sessions_approve_is_refused(self) -> None:
        own = comment("APPROVE", ident=77, at=NOW - timedelta(seconds=30))
        result = self.run_guard(self.post("HOLD"), [own])
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("this session posted an APPROVE of head", result.stderr)
        self.assertIn("#issuecomment-77", result.stderr)
        self.assertIn("SHIPYARD_VERDICT_RETRACT=1", result.stderr)

    def test_a_deliberate_retraction_posts(self) -> None:
        own = comment("APPROVE", at=NOW - timedelta(seconds=30))
        result = self.run_guard(self.post("HOLD"), [own], env={"SHIPYARD_VERDICT_RETRACT": "1"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("retracting", result.stderr)

    def test_the_window_is_ten_minutes(self) -> None:
        inside = comment("APPROVE", at=NOW - timedelta(seconds=600))
        self.assertEqual(self.run_guard(self.post("HOLD"), [inside]).returncode, 1)
        outside = comment("APPROVE", at=NOW - timedelta(seconds=601))
        self.assertEqual(self.run_guard(self.post("HOLD"), [outside]).returncode, 0)

    def test_another_agent_or_host_or_a_forged_stamp_is_not_this_session(self) -> None:
        for other in (comment("APPROVE", stamp=_stamp(agent="pulp-review2")),
                      comment("APPROVE", stamp=_stamp(host="elsewhere")),
                      comment("APPROVE", stamp=_stamp(session="another")),
                      comment("APPROVE", user={"login": "stranger", "type": "User"})):
            with self.subTest(body=other["body"], user=other["user"]):
                result = self.run_guard(self.post("HOLD"), [other])
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_an_unattributed_session_has_no_self_to_contradict(self) -> None:
        own = comment("APPROVE", at=NOW - timedelta(seconds=30))
        result = self.run_guard(self.post("HOLD"), [own],
                                env={"SHIPYARD_STAMP_SESSION_ID": "", "SHIPYARD_AGENT_ID": ""})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_an_approve_after_this_sessions_hold_is_refused_even_where_holds_do_not_count(
            self) -> None:
        # o/r requires no review, so the gate counts no HOLD (rule 1 is
        # silent); rule 2 still stops the contradiction.
        hold = comment("HOLD", at=NOW - timedelta(seconds=5))
        hold["html_url"] = "https://github.com/o/r/pull/7#issuecomment-1"
        argv = ["api", "repos/o/r/issues/7/comments", "-f",
                f"body=reviewer: pulp-review\nverdict: APPROVE head:{HEAD}"]
        result = self.run_guard(argv, [hold], repo="o/r")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("this session posted a HOLD", result.stderr)

    # ------------------------------------------------ parity and wiring

    def test_refuses_exactly_the_approves_the_gate_ignores(self) -> None:
        spec = importlib.util.spec_from_file_location("arm_for_parity", ARM)
        assert spec and spec.loader
        arm = importlib.util.module_from_spec(spec)
        os.environ["SHIPYARD_GHAPP_GUARDS_DIR"] = str(self.guards)
        try:
            spec.loader.exec_module(arm)
            policy = arm.review_policy(None, REPO, True)
        finally:
            os.environ.pop("SHIPYARD_GHAPP_GUARDS_DIR", None)
        cases = [
            [comment("HOLD")],
            [comment("HOLD", OTHER)],
            [comment("HOLD", user={"login": "stranger", "type": "User"})],
            [comment("HOLD", user={"login": "danielraffel", "type": "User"},
                     association="OWNER")],
            [comment("HOLD", user={"login": "shipyard-local", "type": "User"})],
            [comment("APPROVE", ident=1), comment("HOLD", ident=2)],
        ]
        for comments in cases:
            with self.subTest(comments=[c["body"][-60:] for c in comments]):
                held, why = arm.review_verdict(HEAD, "author-agent", comments, [], policy)
                gate_holds = (not held) and "un-approvable" in why
                result = self.run_guard(self.post("APPROVE"), comments,
                                        env={"SHIPYARD_AGENT_ID": "someone-else"})
                self.assertEqual(result.returncode == 1, gate_holds, result.stderr + why)

    def test_ghapp_runs_the_guard_after_the_stamper_and_refuses_on_its_exit(self) -> None:
        wrapper = (HERE / "ghapp").read_text(encoding="utf-8")
        line = 'PATH="$trusted_path" "$guards/verdict-contradiction-guard" "$@" || exit 1'
        self.assertIn(line, wrapper)
        self.assertGreater(wrapper.index(line), wrapper.index('"$guards/verdict-session-stamp"'))


if __name__ == "__main__":
    unittest.main()
