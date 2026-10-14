from __future__ import annotations

import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPT = pathlib.Path(__file__).with_name("ghapp_verdict_session_stamp.py")
SPEC = importlib.util.spec_from_file_location("ghapp_verdict_session_stamp", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
stamp = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(stamp)

ARM = pathlib.Path(__file__).with_name("ghapp_queue_arm_guard.py")
ARM_SPEC = importlib.util.spec_from_file_location("ghapp_queue_arm_guard", ARM)
assert ARM_SPEC is not None and ARM_SPEC.loader is not None
guard = importlib.util.module_from_spec(ARM_SPEC)
ARM_SPEC.loader.exec_module(guard)

HEAD = "a" * 40
VERDICT = f"Looks right.\n\nverdict: APPROVE head:{HEAD}\n"


class Stamp(unittest.TestCase):
    def setUp(self) -> None:
        patcher = mock.patch.dict(os.environ, {"CLAUDE_CODE_SESSION_ID": "sess-A"}, clear=False)
        patcher.start()
        self.addCleanup(patcher.stop)
        os.environ.pop("CODEX_COMPANION_SESSION_ID", None)
        os.environ.pop("SHIPYARD_AGENT_ID", None)

    def test_a_verdict_body_gets_the_stamp_above_its_marker(self) -> None:
        out = stamp.stamped(VERDICT)
        assert out is not None
        lines = [line for line in out.split("\n") if line.strip()]
        self.assertRegex(lines[-2], r"^<!-- session: [^/\s]+/sess-A -->$")
        # Every verdict reader still reads the same verdict.
        self.assertEqual(guard.marker_verdict(out), ("APPROVE", HEAD))
        self.assertEqual(guard.stamped_session(out), "sess-A")

    def test_an_agent_id_is_stamped_when_set(self) -> None:
        with mock.patch.dict(os.environ, {"SHIPYARD_AGENT_ID": "reviewer-2"}):
            out = stamp.stamped(VERDICT)
        assert out is not None
        self.assertEqual(guard.stamped_session(out), "sess-A")
        self.assertEqual(guard.stamped_agent(out), "reviewer-2")
        self.assertEqual(guard.marker_verdict(out), ("APPROVE", HEAD))
        self.assertIsNone(guard.stamped_agent(stamp.stamped(VERDICT)))

    def test_only_verdicts_are_stamped_and_never_twice(self) -> None:
        self.assertIsNone(stamp.stamped("just a note"))
        self.assertIsNone(stamp.stamped(f"verdict: APPROVE head:{HEAD}\nthen prose"))
        once = stamp.stamped(VERDICT)
        assert once is not None
        self.assertIsNone(stamp.stamped(once))

    def test_an_unknown_session_is_stamped_unattributed(self) -> None:
        with mock.patch.dict(os.environ, {"CLAUDE_CODE_SESSION_ID": ""}):
            out = stamp.stamped(VERDICT)
        assert out is not None
        self.assertEqual(guard.stamped_session(out), "unattributed")

    def test_the_comment_forms_ghapp_posts_are_rewritten(self) -> None:
        for args, index in (
            (["pr", "comment", "5", "--body", VERDICT], 4),
            (["issue", "comment", "5", "-b", VERDICT], 4),
            (["api", "repos/o/r/issues/5/comments", "-f", f"body={VERDICT}"], 3),
            (["api", "-X", "PATCH", "repos/o/r/issues/comments/9", "-F", f"body={VERDICT}"], 5),
        ):
            with self.subTest(args=args[:2]):
                out = stamp.rewrite(args)
                self.assertIn("<!-- session:", out[index])
                self.assertEqual(len(out), len(args))
        # Not a comment endpoint, not a verdict: unchanged.
        for args in (["api", "repos/o/r/pulls/5/merge", "-f", f"body={VERDICT}"],
                     ["pr", "comment", "5", "--body", "hello"],
                     ["pr", "merge", "5", "--auto"]):
            self.assertEqual(stamp.rewrite(args), args)

    def test_an_input_file_body_becomes_a_field_and_no_file_is_written(self) -> None:
        with tempfile.TemporaryDirectory() as scratch, \
                tempfile.TemporaryDirectory() as tmp, \
                mock.patch.dict(os.environ, {"TMPDIR": tmp}):
            tempfile.tempdir = None
            original = pathlib.Path(scratch) / "body.json"
            original.write_text(json.dumps({"body": VERDICT}), encoding="utf-8")
            args = ["api", "-X", "POST", "repos/o/r/issues/5/comments", "--input", str(original)]
            out = stamp.rewrite(args)
            self.assertEqual(out[:4], args[:4])
            self.assertEqual(out[4], "--raw-field")
            self.assertTrue(out[5].startswith("body="))
            self.assertEqual(guard.stamped_session(out[5][len("body="):]), "sess-A")
            self.assertEqual(len(out), 6)
            self.assertEqual(json.loads(original.read_text())["body"], VERDICT)
            self.assertEqual(os.listdir(tmp), [], "the stamper wrote a file")
        tempfile.tempdir = None
        # A payload with more than the body is left for gh to send unstamped.
        with tempfile.TemporaryDirectory() as scratch:
            other = pathlib.Path(scratch) / "body.json"
            other.write_text(json.dumps({"body": VERDICT, "x": 1}), encoding="utf-8")
            args = ["api", "repos/o/r/issues/5/comments", "--input", str(other)]
            self.assertEqual(stamp.rewrite(args), args)

    def test_the_output_starts_with_the_sentinel_and_keeps_every_argument(self) -> None:
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "pr", "comment", "5", "--body", VERDICT],
            capture_output=True, check=True,
            env={"PATH": "/usr/bin:/bin", "CLAUDE_CODE_SESSION_ID": "sess-A"},
        )
        items = result.stdout.split(b"\0")[:-1]
        self.assertEqual(items[0], b"SHIPYARD-STAMP-V1")
        self.assertEqual(items[1:4], [b"pr", b"comment", b"5"])
        self.assertIn(b"<!-- session:", items[5])
        self.assertEqual(items[-1], b"SHIPYARD-STAMP-END")
        self.assertEqual(len(items), 7)

    def test_a_failure_prints_nothing_so_ghapp_posts_unstamped(self) -> None:
        with mock.patch.object(stamp, "rewrite", side_effect=RuntimeError("boom")):
            with mock.patch.object(sys, "stdout") as out:
                self.assertEqual(stamp.main(["pr", "comment"]), 0)
                out.buffer.write.assert_not_called()

    def test_a_stuck_stamper_stops_itself_and_prints_nothing(self) -> None:
        import time
        probe = (
            "import importlib.util, sys, time\n"
            f"spec = importlib.util.spec_from_file_location('s', {str(SCRIPT)!r})\n"
            "m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)\n"
            "m.DEADLINE_SECONDS = 1\n"
            "m.rewrite = lambda args: time.sleep(30)\n"
            "sys.exit(m.main(['pr', 'comment']))\n"
        )
        started = time.monotonic()
        result = subprocess.run([sys.executable, "-c", probe], capture_output=True, timeout=20)
        self.assertLess(time.monotonic() - started, 10)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, b"")

    def test_the_marker_pattern_is_the_arm_guards(self) -> None:
        self.assertEqual(stamp._MARKER.pattern, guard._MARKER.pattern)


class GhappStampWiring(unittest.TestCase):
    """The ghapp snippet that applies the stamper keeps the arguments intact."""

    def run_snippet(self, stamper_body: str, deadline: float = 30) -> list[str]:
        ghapp = (SCRIPT.parent / "ghapp").read_text(encoding="utf-8")
        start = ghapp.index('if [[ -x "$guards/verdict-session-stamp" ]]; then')
        end = ghapp.index("\nfi\n", start) + 4
        with tempfile.TemporaryDirectory() as scratch:
            guards = pathlib.Path(scratch)
            stamper = guards / "verdict-session-stamp"
            stamper.write_text(stamper_body, encoding="utf-8")
            stamper.chmod(0o755)
            script = ("set -euo pipefail\nguards=\"$1\"; shift\ntrusted_path=/usr/bin:/bin\n"
                      + ghapp[start:end] + 'printf "%s\\0" "$@"\n')
            result = subprocess.run(
                ["/bin/bash", "-c", script, "ghapp", str(guards), "pr", "comment", "5",
                 "--body", "a b\nverdict: APPROVE head:x"],
                capture_output=True, check=True, timeout=deadline,
            )
        return [item.decode() for item in result.stdout.split(b"\0")[:-1]]

    def test_the_rewritten_arguments_replace_the_originals(self) -> None:
        out = self.run_snippet(
            "#!/bin/sh\nprintf 'SHIPYARD-STAMP-V1\\0pr\\0comment\\0'\n"
            "printf '5\\0--body\\0stamped body\\0SHIPYARD-STAMP-END\\0'\n")
        self.assertEqual(out, ["pr", "comment", "5", "--body", "stamped body"])

    def test_without_both_sentinels_the_originals_stand(self) -> None:
        truncated = "#!/bin/sh\nprintf 'SHIPYARD-STAMP-V1\\0pr\\0comment\\0'\n"
        for body in ("#!/bin/sh\nexit 3\n", "#!/bin/sh\nprintf 'x\\0y\\0'\n", truncated):
            with self.subTest(body=body):
                out = self.run_snippet(body)
                self.assertEqual(out, ["pr", "comment", "5", "--body",
                                       "a b\nverdict: APPROVE head:x"])

    def test_a_hung_stamper_posts_unstamped_within_the_deadline(self) -> None:
        import time
        started = time.monotonic()
        # Starts its answer, then hangs.
        out = self.run_snippet(
            "#!/bin/sh\nprintf 'SHIPYARD-STAMP-V1\\0pr\\0'\nexec sleep 60\n", deadline=20)
        self.assertLess(time.monotonic() - started, 15)
        self.assertEqual(out, ["pr", "comment", "5", "--body", "a b\nverdict: APPROVE head:x"])


if __name__ == "__main__":
    unittest.main()
