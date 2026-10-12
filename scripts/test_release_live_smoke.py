#!/usr/bin/env python3
from __future__ import annotations

import io
import json
import subprocess
import unittest
from contextlib import redirect_stderr, redirect_stdout

import release_live_smoke as smoke

NODE_LIMIT = (
    "GraphQL: By the time this query traverses to the authors connection, it is "
    "requesting up to 1,000,000 possible nodes which exceeds the maximum limit of 500,000."
)


def fake_run(admission: tuple[int, dict | None, str], steward: tuple[int, dict | None, str]):
    calls: list[list[str]] = []

    def run(argv, **kwargs):
        calls.append(list(argv))
        code, document, stderr = admission if "admission-clean" in argv else steward
        stdout = json.dumps(document) if document is not None else ""
        return subprocess.CompletedProcess(argv, code, stdout, stderr)

    return run, calls


CLEAN_STEWARD = (0, {"repos": [{"errors": []}]}, "")


class ReleaseLiveSmokeTests(unittest.TestCase):
    def run_main(self, run, *extra: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = smoke.main(["--binary", "/bin/true", *extra], run=run)
        return code, out.getvalue(), err.getvalue()

    def test_admit_and_clean_steward_pass(self) -> None:
        run, calls = fake_run((0, {"verdict": "admit", "reason": "clean"}, ""), CLEAN_STEWARD)
        code, out, _ = self.run_main(run)
        self.assertEqual(code, 0, out)
        self.assertIn("all 2 live probes passed", out)
        self.assertEqual(len(calls), 2)

    def test_defer_is_an_observation_and_passes(self) -> None:
        run, _ = fake_run((3, {"verdict": "defer", "reason": "observation_in_progress"}, ""), CLEAN_STEWARD)
        self.assertEqual(self.run_main(run)[0], 0)

    def test_node_limit_error_fails_the_release(self) -> None:
        run, _ = fake_run(
            (1, {"verdict": "error", "reason": "observation_failed", "error": NODE_LIMIT}, ""),
            (1, {"repos": [{"errors": [f"open PR list failed: {NODE_LIMIT}"]}]}, ""),
        )
        code, out, err = self.run_main(run)
        self.assertEqual(code, 1)
        self.assertIn("FAIL admission-clean Generous-Corp/pulp", out)
        self.assertIn("FAIL steward (dry-run) Generous-Corp/pulp", out)
        self.assertIn("500,000", out)
        self.assertIn("must not publish", err)

    def test_exit_and_verdict_must_agree(self) -> None:
        run, _ = fake_run((0, {"verdict": "defer"}, ""), CLEAN_STEWARD)
        self.assertEqual(self.run_main(run)[0], 1)

    def test_a_binary_that_prints_no_json_fails_with_its_stderr(self) -> None:
        run, _ = fake_run((2, None, "error: unrecognized subcommand"), CLEAN_STEWARD)
        code, out, _ = self.run_main(run)
        self.assertEqual(code, 1)
        self.assertIn("unrecognized subcommand", out)

    def test_steward_repo_errors_fail_even_on_exit_zero(self) -> None:
        run, _ = fake_run(
            (0, {"verdict": "admit"}, ""), (0, {"repos": [{"errors": ["merge-queue policy failed"]}]}, "")
        )
        self.assertEqual(self.run_main(run)[0], 1)

    def test_probes_are_read_only_isolated_and_cover_every_repo(self) -> None:
        run, calls = fake_run((0, {"verdict": "admit"}, ""), CLEAN_STEWARD)
        code, _, _ = self.run_main(run, "--repo", "o/a", "--repo", "o/b", "--labels", "self-hosted,x")
        self.assertEqual(code, 0)
        self.assertEqual(len(calls), 4)
        for argv in calls:
            self.assertNotIn("--apply", argv)
            self.assertEqual(argv[1:3], ["--mode", "isolated"])
        repos = [argv[argv.index("--repo") + 1] for argv in calls]
        self.assertEqual(repos, ["o/a", "o/a", "o/b", "o/b"])
        admission = [argv for argv in calls if "admission-clean" in argv]
        self.assertTrue(all(argv[argv.index("--labels") + 1] == "self-hosted,x" for argv in admission))

    def test_timeout_fails(self) -> None:
        def run(argv, **kwargs):
            raise subprocess.TimeoutExpired(argv, 1)

        code, out, _ = self.run_main(run)
        self.assertEqual(code, 1)
        self.assertIn("timed out", out)


if __name__ == "__main__":
    unittest.main()
