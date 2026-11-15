from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SCRIPT = pathlib.Path(__file__).with_name("ghapp_queue_removal_guard.py")
SPEC = importlib.util.spec_from_file_location("ghapp_queue_removal_guard", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"unable to load guard module from {SCRIPT}")
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)


def issue_nonce(directory: pathlib.Path, *purposes: str) -> dict[str, str]:
    """What Shipyard does for one of its own calls: a fresh nonce and a 0600
    record per purpose in a 0700 directory. Returns the variables naming them."""
    import secrets

    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    directory.chmod(0o700)
    nonce = secrets.token_hex(32)
    for purpose in purposes:
        path = directory / guard.nonce_record_name(nonce, purpose)
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as record:
            record.write(purpose)
    return {guard.NONCE_ENV: nonce, guard.NONCE_DIR_ENV: str(directory)}

# A case that clears the environment drops HOME, and Path.home() then falls back
# to the account's real home: the guard appended every such case to the real
# ~/.local/state/shipyard/queue-removals.jsonl on whatever host ran the suite. Every case
# must give the guard a HOME; one that does not fails here instead of writing.
HOMELESS_READS: list[str] = []
_REAL_HOME = pathlib.Path.home


def _home_or_fail(cls: type[pathlib.Path]) -> pathlib.Path:
    home = os.environ.get("HOME")
    if not home:
        HOMELESS_READS.append(os.environ.get("PYTEST_CURRENT_TEST", "a case"))
        raise AssertionError("a test cleared HOME; the guard would write the real home")
    return cls(home)


def setUpModule() -> None:  # noqa: N802 - unittest's name
    patcher = mock.patch.object(pathlib.Path, "home", classmethod(_home_or_fail))
    patcher.start()
    unittest.addModuleCleanup(patcher.stop)


def tearDownModule() -> None:  # noqa: N802 - unittest's name
    if HOMELESS_READS:
        raise AssertionError(f"{len(HOMELESS_READS)} home reads with HOME cleared")


def scratch_home(case: unittest.TestCase) -> str:
    """A throwaway HOME for one case, removed after it."""
    home = tempfile.mkdtemp()
    case.addCleanup(shutil.rmtree, home, True)
    return home


class QueueRemovalGuardTests(unittest.TestCase):
    def run_guard(self, args: list[str], **env: str) -> tuple[int, str]:
        stderr = io.StringIO()
        env = {"HOME": scratch_home(self), **env}
        with mock.patch.dict(os.environ, env, clear=True), contextlib.redirect_stderr(stderr):
            return guard.main(args), stderr.getvalue()

    def test_harmless_commands_are_allowed(self) -> None:
        self.assertEqual(self.run_guard(["pr", "view", "7476"])[0], 0)
        self.assertEqual(
            self.run_guard(["api", "graphql", "-f", "query=query { viewer { login } }"])[0],
            0,
        )
        self.assertEqual(self.run_guard(["pr", "merge", "7476", "--auto"])[0], 0)

    def test_disable_auto_is_refused_without_explicit_authority(self) -> None:
        code, message = self.run_guard(["pr", "merge", "7476", "--disable-auto"])
        self.assertEqual(code, 1)
        self.assertIn("refusing unaudited", message)
        for name in ("GHAPP_ALLOW_QUEUE_REMOVAL", "SHIPYARD_INTERNAL_QUEUE_MUTATION"):
            self.assertNotIn(name, message)

    def test_raw_dequeue_and_disable_mutations_are_refused(self) -> None:
        for mutation in ("dequeuePullRequest", "disablePullRequestAutoMerge"):
            with self.subTest(mutation=mutation):
                code, _ = self.run_guard(
                    [
                        "api",
                        "graphql",
                        "-f",
                        f"query=mutation($id:ID!) {{{mutation}(input:{{id:$id}}) {{ clientMutationId }} }}",
                    ]
                )
                self.assertEqual(code, 1)

    def test_input_file_mutation_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            request = pathlib.Path(directory) / "request.json"
            request.write_text(
                r'{"query":"mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }"}'
            )
            self.assertEqual(self.run_guard(["api", "graphql", "--input", str(request)])[0], 1)

    def test_input_file_decodes_escaped_mutation_name(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            request = pathlib.Path(directory) / "request.json"
            request.write_text(
                r'{"query":"mutation { dequeuePull\u0052equest(input:{id:\"x\"}) { clientMutationId } }"}'
            )
            self.assertEqual(self.run_guard(["api", "graphql", "--input", str(request)])[0], 1)

    def test_graphql_endpoint_is_found_after_flags(self) -> None:
        args = [
            "api",
            "--method",
            "POST",
            "graphql",
            "-fquery=mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }",
        ]
        self.assertEqual(self.run_guard(args)[0], 1)

    def test_graphql_stdin_bodies_fail_closed(self) -> None:
        for args in (
            ["api", "graphql", "--input", "-"],
            ["api", "graphql", "--input=-"],
            ["api", "graphql", "-Fquery=@-"],
        ):
            with self.subTest(args=args):
                code, message = self.run_guard(args)
                self.assertEqual(code, 1)
                self.assertIn("stdin", message)

    def test_endpoint_query_mutation_is_refused(self) -> None:
        endpoint = (
            "graphql?query=mutation%20%7B%20disablePullRequestAutoMerge"
            "%28input:%7BpullRequestId:%22x%22%7D%29%20%7BclientMutationId%7D%20%7D"
        )
        self.assertEqual(self.run_guard(["api", endpoint])[0], 1)

    def test_ambiguous_graphql_endpoint_spellings_fail_closed(self) -> None:
        mutation = "query=mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }"
        for endpoint in ("%67raphql", "./graphql"):
            with self.subTest(endpoint=endpoint):
                code, message = self.run_guard(["api", endpoint, "-f", mutation])
                self.assertEqual(code, 1)
                self.assertIn("refusing unaudited", message)

    def test_encoded_unrelated_rest_endpoint_is_allowed(self) -> None:
        self.assertEqual(self.run_guard(["api", "repos/o/r/contents/a%20b"])[0], 0)

    def test_trusted_absolute_github_endpoint_is_allowed(self) -> None:
        self.assertEqual(
            self.run_guard(["api", "https://api.github.com/repos/o/r"])[0], 0
        )

    def test_unrelated_rest_input_body_is_not_parsed_as_graphql(self) -> None:
        self.assertEqual(self.run_guard(["api", "repos/o/r/issues", "--input", "-"])[0], 0)

    def test_endpoint_placeholder_with_removal_mutation_fails_closed(self) -> None:
        args = [
            "api",
            "{owner}",
            "-f",
            "query=mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }",
        ]
        code, message = self.run_guard(args, GH_REPO="graphql/x")
        self.assertEqual(code, 1)
        self.assertIn("refusing unaudited", message)

    def test_repository_placeholders_fall_back_to_git_remote(self) -> None:
        completed = subprocess.CompletedProcess(
            ["/opt/homebrew/bin/gh", "repo", "view", "--json", "nameWithOwner"],
            0,
            stdout='{"nameWithOwner":"o/r"}\n',
            stderr="",
        )
        with mock.patch.object(guard.subprocess, "run", return_value=completed) as run:
            self.assertEqual(self.run_guard(["api", "repos/{owner}/{repo}"])[0], 0)
        self.assertEqual(run.call_args.kwargs["timeout"], 30)

    def test_repository_placeholder_resolution_timeout_fails_closed(self) -> None:
        with mock.patch.object(
            guard.subprocess,
            "run",
            side_effect=subprocess.TimeoutExpired(["gh", "repo", "view"], 30),
        ):
            code, message = self.run_guard(["api", "repos/{owner}/{repo}"])
        self.assertEqual(code, 1)
        self.assertIn("cannot resolve current repository identity", message)

    def test_override_refuses_ambiguous_input_it_cannot_read(self) -> None:
        code, message = self.run_guard(
            ["api", "graphql", "--input", "-"], GHAPP_ALLOW_QUEUE_REMOVAL="1"
        )
        self.assertEqual(code, 1)
        self.assertIn("ambiguous", message)

    def test_compact_field_and_query_file_mutations_are_refused(self) -> None:
        mutation = "mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }"
        self.assertEqual(
            self.run_guard(["api", "graphql", f"-fquery={mutation}"])[0],
            1,
        )
        with tempfile.TemporaryDirectory() as directory:
            request = pathlib.Path(directory) / "mutation.graphql"
            request.write_text(mutation)
            self.assertEqual(
                self.run_guard(["api", "graphql", "-f", f"query=@{request}"])[0],
                1,
            )

    def nonce(self, *purposes: str, directory: pathlib.Path | None = None) -> dict[str, str]:
        return issue_nonce(directory or pathlib.Path(scratch_home(self)) / "nonces", *purposes)

    def test_internal_marker_is_honoured_once_with_shipyards_nonce(self) -> None:
        args = ["api", "graphql", "-f", "query=mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }"]
        issued = self.nonce(guard.NONCE_INTERNAL_REMOVAL)
        self.assertEqual(self.run_guard(args, SHIPYARD_INTERNAL_QUEUE_MUTATION="1", **issued)[0], 0)
        loose = self.nonce(guard.NONCE_INTERNAL_REMOVAL)
        (pathlib.Path(loose[guard.NONCE_DIR_ENV]) / guard.nonce_record_name(
            loose[guard.NONCE_ENV], guard.NONCE_INTERNAL_REMOVAL)).chmod(0o644)
        for label, env in (
            ("no nonce", {}),
            ("replayed", issued),
            ("wrong", {**self.nonce(guard.NONCE_INTERNAL_REMOVAL), guard.NONCE_ENV: "1" * 64}),
            ("another purpose", self.nonce(guard.NONCE_INTERNAL_ARM)),
            ("readable by others", loose),
        ):
            with self.subTest(label):
                code, message = self.run_guard(args, SHIPYARD_INTERNAL_QUEUE_MUTATION="1", **env)
                self.assertEqual(code, 1)
                self.assertIn("no valid one-shot nonce", message)

    def test_a_stale_nonce_record_is_not_honoured(self) -> None:
        args = ["api", "graphql", "-f", "query=mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }"]
        issued = self.nonce(guard.NONCE_INTERNAL_REMOVAL)
        record = pathlib.Path(issued[guard.NONCE_DIR_ENV]) / guard.nonce_record_name(
            issued[guard.NONCE_ENV], guard.NONCE_INTERNAL_REMOVAL)
        old = record.stat().st_mtime - guard.NONCE_MAX_AGE_SECONDS - 5
        os.utime(record, (old, old))
        self.assertEqual(self.run_guard(args, SHIPYARD_INTERNAL_QUEUE_MUTATION="1", **issued)[0], 1)


def pr_answer(number: int = 9706, queued: bool = True) -> dict:
    entry = {"state": "AWAITING_CHECKS", "position": 2} if queued else None
    return {"data": {"node": {"number": number, "headRefOid": "a" * 40, "isInMergeQueue": queued,
                              "mergeQueueEntry": entry, "repository": {"nameWithOwner": "Generous-Corp/pulp"}}}}


def dequeue(node: str = "PR_node9706") -> list[str]:
    return ["api", "graphql", "-f", "query=mutation($id:ID!){dequeuePullRequest(input:{id:$id}){clientMutationId}}",
            "-f", f"id={node}"]


OVERRIDE = {"GHAPP_ALLOW_QUEUE_REMOVAL": "1"}


class OverrideReasonThroughFakeGh(unittest.TestCase):
    """The override is judged by its stated reason, reading the PR through GHAPP_REAL_GH."""

    def run_with_answer(self, args: list[str], answer: dict, **env: str) -> tuple[int, str, list[str], list[dict]]:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / "answer.json").write_text(json.dumps(answer))
            fake = root / "gh"
            fake.write_text(
                "#!/bin/sh\n"
                f"printf '%s\\n' \"$*\" >> '{root}/calls.log'\n"
                f"cat '{root}/answer.json'\n"
            )
            fake.chmod(0o755)
            log = root / "removals.jsonl"
            environment = {"PATH": "/usr/bin:/bin", "GHAPP_REAL_GH": str(fake), "GH_REPO": "Generous-Corp/pulp",
                           "GHAPP_QUEUE_REMOVAL_LOG": str(log), **env}
            result = subprocess.run(
                [sys.executable, str(SCRIPT), *args], env=environment, capture_output=True, text=True
            )
            calls_file = root / "calls.log"
            calls = calls_file.read_text().splitlines() if calls_file.exists() else []
            records = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            return result.returncode, result.stderr, calls, records

    def test_rebase_is_refused_even_with_the_override(self) -> None:
        code, message, _, records = self.run_with_answer(
            dequeue(), pr_answer(), **OVERRIDE, GHAPP_QUEUE_REMOVAL_REASON="rebase",
            GHAPP_QUEUE_REMOVAL_NOTE="main moved")
        self.assertEqual(code, 1, message)
        self.assertIn("A queued PR does not need a rebase; the queue merges it on top of current main.", message)
        self.assertEqual(records, [])

    def test_reorder_is_refused_with_a_pointer_to_the_jump_enqueue(self) -> None:
        code, message, _, _ = self.run_with_answer(
            dequeue(), pr_answer(), **OVERRIDE, GHAPP_QUEUE_REMOVAL_REASON="reorder",
            GHAPP_QUEUE_REMOVAL_NOTE="let #9800 go first")
        self.assertEqual(code, 1, message)
        self.assertIn("jump", message)

    def test_missing_unknown_or_unexplained_reason_is_refused(self) -> None:
        for env in ({}, {"GHAPP_QUEUE_REMOVAL_REASON": "other", "GHAPP_QUEUE_REMOVAL_NOTE": "x"},
                    {"GHAPP_QUEUE_REMOVAL_REASON": "defect-fix"}):
            with self.subTest(env=env):
                code, message, calls, records = self.run_with_answer(dequeue(), pr_answer(), **OVERRIDE, **env)
                self.assertEqual(code, 1, message)
                self.assertEqual(records, [])
                self.assertEqual(calls, [], "a refused reason reads nothing")

    def test_defect_fix_is_allowed_and_logged(self) -> None:
        code, message, calls, records = self.run_with_answer(
            dequeue(), pr_answer(), **OVERRIDE, GHAPP_QUEUE_REMOVAL_REASON="defect-fix",
            GHAPP_QUEUE_REMOVAL_NOTE="local build shows the cache key ignores the arch")
        self.assertEqual(code, 0, message)
        self.assertIn("WARNING", message)
        self.assertTrue(any("id=PR_node9706" in call for call in calls), calls)
        self.assertEqual(len(records), 1)
        record = records[0]
        self.assertEqual(record["reason"], "defect-fix")
        self.assertEqual(record["note"], "local build shows the cache key ignores the arch")
        self.assertEqual(record["targets"], [{"repo": "Generous-Corp/pulp", "pr": 9706, "head": "a" * 40,
                                              "queue_state": "AWAITING_CHECKS"}])

    def test_disable_auto_by_number_is_logged_against_the_named_pr(self) -> None:
        answer = {"data": {"repository": {"nameWithOwner": "Generous-Corp/pulp",
                                          "pullRequest": pr_answer(9834)["data"]["node"]}}}
        code, message, calls, records = self.run_with_answer(
            ["pr", "merge", "9834", "--disable-auto"], answer, **OVERRIDE,
            GHAPP_QUEUE_REMOVAL_REASON="defect-fix", GHAPP_QUEUE_REMOVAL_NOTE="bug")
        self.assertEqual(code, 0, message)
        self.assertTrue(any("number=9834" in call and "owner=Generous-Corp" in call for call in calls), calls)
        self.assertEqual(records[0]["targets"][0]["pr"], 9834)

    def test_disable_auto_by_url_reads_the_pr_the_url_names(self) -> None:
        answer = {"data": {"repository": {"nameWithOwner": "Generous-Corp/pulp",
                                          "pullRequest": pr_answer(9827)["data"]["node"]}}}
        code, message, calls, records = self.run_with_answer(
            ["pr", "merge", "https://github.com/Generous-Corp/pulp/pull/9827", "--disable-auto"],
            answer, **OVERRIDE, GH_REPO="someone/else",
            GHAPP_QUEUE_REMOVAL_REASON="defect-fix", GHAPP_QUEUE_REMOVAL_NOTE="bug")
        self.assertEqual(code, 0, message)
        self.assertFalse(any(call.startswith("pr view") for call in calls), calls)
        self.assertTrue(
            any("owner=Generous-Corp" in call and "name=pulp" in call and "number=9827" in call
                for call in calls),
            calls,
        )
        self.assertEqual(records[0]["targets"][0]["pr"], 9827)

    def test_main_red_fix_may_remove_only_the_fix_pr(self) -> None:
        env = {**OVERRIDE, "GHAPP_QUEUE_REMOVAL_REASON": "reorder-main-red-fix",
               "GHAPP_QUEUE_REMOVAL_NOTE": "main red on view-widgets; jump the fix"}
        code, message, _, records = self.run_with_answer(
            dequeue(), pr_answer(9706), **env, GHAPP_QUEUE_REMOVAL_FIX_PR="9706")
        self.assertEqual(code, 0, message)
        self.assertEqual(records[0]["fix_pr"], "9706")
        for fix_pr in ("9800", ""):
            with self.subTest(fix_pr=fix_pr):
                code, message, _, records = self.run_with_answer(
                    dequeue(), pr_answer(9706), **env, GHAPP_QUEUE_REMOVAL_FIX_PR=fix_pr)
                self.assertEqual(code, 1, message)
                self.assertIn("only the PR that fixes main", message)
                self.assertEqual(records, [])

    def test_unreadable_target_refuses_the_override(self) -> None:
        code, message, _, records = self.run_with_answer(
            dequeue(), {"errors": [{"message": "boom"}]}, **OVERRIDE,
            GHAPP_QUEUE_REMOVAL_REASON="defect-fix", GHAPP_QUEUE_REMOVAL_NOTE="bug")
        self.assertEqual(code, 1)
        self.assertIn("cannot read the PR it targets", message)
        self.assertEqual(records, [])

    def test_unwritable_log_refuses_the_removal(self) -> None:
        code, message, _, _ = self.run_with_answer(
            dequeue(), pr_answer(), **OVERRIDE, GHAPP_QUEUE_REMOVAL_REASON="defect-fix",
            GHAPP_QUEUE_REMOVAL_NOTE="bug", GHAPP_QUEUE_REMOVAL_LOG="/dev/null/nope.jsonl")
        self.assertEqual(code, 1)
        self.assertIn("cannot record the removal", message)

    def test_without_the_override_nothing_is_read(self) -> None:
        code, _, calls, _ = self.run_with_answer(
            dequeue(), pr_answer(), GHAPP_QUEUE_REMOVAL_REASON="defect-fix", GHAPP_QUEUE_REMOVAL_NOTE="bug")
        self.assertEqual(code, 1)
        self.assertEqual(calls, [])

    def test_spoofed_marker_from_this_test_process_is_refused(self) -> None:
        code, message, calls, _ = self.run_with_answer(dequeue(), pr_answer(), SHIPYARD_INTERNAL_QUEUE_MUTATION="1")
        self.assertEqual(code, 1)
        self.assertIn("no valid one-shot nonce", message)
        self.assertEqual(calls, [])


def solo_entry(number: int, position: int | None, head: str = "a" * 40, base: str = "main",
               state: str = "OPEN") -> dict:
    entry = {"state": "QUEUED", "position": position} if position is not None else None
    return {"number": number, "state": state, "headRefOid": head, "baseRefName": base,
            "isInMergeQueue": entry is not None, "mergeQueueEntry": entry}


class SoloRemoval(unittest.TestCase):
    """A `solo` removal: only entries ahead of a live reservation's PR."""

    RESERVED = 451

    def write_reservation(self, directory: pathlib.Path, **changes: object) -> pathlib.Path:
        from datetime import datetime, timedelta, timezone

        now = datetime.now(timezone.utc)
        record = {
            "schema": 1, "kind": "shipyard.merge-queue-solo", "repo": "Generous-Corp/vellum",
            "base": "main", "pr": self.RESERVED, "head": "c" * 40, "reason": "release",
            "note": "release coalesce lands alone", "approval": "verdict comment 1",
            "created_at": now.isoformat().replace("+00:00", "Z"),
            "expires_at": (now + timedelta(hours=2)).isoformat().replace("+00:00", "Z"),
            "seen_in_queue": False,
        }
        record.update(changes)
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        path = directory / "generous-corp__vellum.json"
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            json.dump(record, handle)
        return path

    def run_solo(self, args: list[str], targets: dict[str, dict], reserved: dict | None,
                 reservation: dict | None = None, **env: str) -> tuple[int, str, list[dict]]:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            solo_dir = root / "solo"
            if reservation is not None:
                self.write_reservation(solo_dir, **reservation)
            answers = {f"id={node}": {"data": {"node": {
                **entry, "repository": {"nameWithOwner": "Generous-Corp/vellum"}}}}
                for node, entry in targets.items()}
            if reserved is not None:
                answers[f"number={self.RESERVED}"] = {"data": {"repository": {
                    "nameWithOwner": "Generous-Corp/vellum", "pullRequest": reserved}}}
            (root / "answers.json").write_text(json.dumps(answers))
            fake = root / "gh"
            fake.write_text(
                f"#!{sys.executable}\n"
                "import json, sys\n"
                f"answers = json.load(open({str(root / 'answers.json')!r}))\n"
                "args = ' '.join(sys.argv[1:])\n"
                "for key, answer in answers.items():\n"
                "    if key in sys.argv[1:] or args.endswith(key):\n"
                "        print(json.dumps(answer)); sys.exit(0)\n"
                "print(json.dumps({'errors': [{'message': 'unknown ' + args}]}))\n"
            )
            fake.chmod(0o755)
            log = root / "removals.jsonl"
            environment = {"PATH": "/usr/bin:/bin", "HOME": str(root), "GHAPP_REAL_GH": str(fake),
                           "GH_REPO": "Generous-Corp/vellum", "GHAPP_QUEUE_REMOVAL_LOG": str(log),
                           "SHIPYARD_SOLO_RESERVATION_DIR": str(solo_dir), **OVERRIDE,
                           "GHAPP_QUEUE_REMOVAL_REASON": "solo",
                           "GHAPP_QUEUE_REMOVAL_NOTE": "clear the queue for the v1.4 release coalesce",
                           **env}
            result = subprocess.run([sys.executable, str(SCRIPT), *args], env=environment,
                                    capture_output=True, text=True)
            records = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            return result.returncode, result.stderr, records

    def test_an_entry_ahead_of_the_reserved_pr_may_be_removed_and_is_audited(self) -> None:
        code, message, records = self.run_solo(
            dequeue("PR_node418"), {"PR_node418": solo_entry(418, 1)},
            solo_entry(self.RESERVED, 2, head="c" * 40), reservation={})
        self.assertEqual(code, 0, message)
        self.assertIn("WARNING", message)
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0]["reason"], "solo")
        self.assertEqual(records[0]["solo"]["reserved_pr"], self.RESERVED)
        self.assertEqual(records[0]["solo"]["positions"], {"418": 1})

    def test_only_entries_ahead_of_the_reserved_pr_may_be_removed(self) -> None:
        reserved = solo_entry(self.RESERVED, 2, head="c" * 40)
        cases = {
            "behind": {"PR_node500": solo_entry(500, 3)},
            "the reserved pr itself": {"PR_node451": solo_entry(self.RESERVED, 2, head="c" * 40)},
            "not queued": {"PR_node500": solo_entry(500, None)},
            "another base": {"PR_node500": solo_entry(500, 1, base="release")},
        }
        for name, targets in cases.items():
            with self.subTest(name):
                node = next(iter(targets))
                code, message, records = self.run_solo(dequeue(node), targets, reserved, reservation={})
                self.assertEqual(code, 1, message)
                self.assertEqual(records, [])

    def test_a_reservation_that_is_missing_expired_unreadable_or_stale_grants_nothing(self) -> None:
        ahead = {"PR_node418": solo_entry(418, 1)}
        queued = solo_entry(self.RESERVED, 2, head="c" * 40)
        cases = [
            ("no reservation", None, queued),
            ("expired", {"expires_at": "2020-01-01T00:00:00Z"}, queued),
            ("wrong kind", {"kind": "something-else"}, queued),
            ("reserved pr not queued", {}, solo_entry(self.RESERVED, None, head="c" * 40)),
            ("reserved pr moved head", {}, solo_entry(self.RESERVED, 2, head="d" * 40)),
            ("reserved pr merged", {}, solo_entry(self.RESERVED, 2, head="c" * 40, state="MERGED")),
            ("reserved pr unreadable", {}, None),
        ]
        for name, reservation, reserved in cases:
            with self.subTest(name):
                code, message, records = self.run_solo(
                    dequeue("PR_node418"), ahead, reserved, reservation=reservation)
                self.assertEqual(code, 1, message)
                self.assertEqual(records, [])

    def test_a_reservation_others_can_write_grants_nothing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = self.write_reservation(pathlib.Path(directory) / "solo")
            path.chmod(0o644)
            env = {"HOME": directory, guard.SOLO_DIR_ENV: str(path.parent)}
            with mock.patch.dict(os.environ, env, clear=True):
                with self.assertRaises(guard.GuardError):
                    guard.read_solo_reservation("Generous-Corp/vellum", guard.datetime.now(guard.timezone.utc))
            path.chmod(0o600)
            link = path.with_name("generous-corp__pulp.json")
            link.symlink_to(path)
            with mock.patch.dict(os.environ, env, clear=True):
                with self.assertRaises(guard.GuardError):
                    guard.read_solo_reservation("Generous-Corp/pulp", guard.datetime.now(guard.timezone.utc))

    def test_rust_nanosecond_times_parse(self) -> None:
        parsed = guard._parse_time("2026-11-14T10:00:00.123456789Z")
        self.assertEqual(parsed.microsecond, 123456)

    def test_the_default_directory_is_shipyards_state_root(self) -> None:
        with mock.patch.dict(os.environ, {"HOME": "/h"}, clear=True), \
                mock.patch.object(guard.sys, "platform", "darwin"):
            self.assertEqual(str(guard.solo_reservation_dir()),
                             "/h/Library/Application Support/shipyard/merge_queue/solo")
        with mock.patch.dict(os.environ, {"HOME": "/h"}, clear=True), \
                mock.patch.object(guard.sys, "platform", "linux"):
            self.assertEqual(str(guard.solo_reservation_dir()), "/h/.local/state/shipyard/merge_queue/solo")


REASON_FOR_CAUSE = {
    # How each recorded removal states itself under the reason classes.
    "rebase_after_main_moved": "rebase",
    "reorder_by_dequeue": "reorder",
    "defect_fix_push": "defect-fix",
    "receipt_text_correction": "other",
    "update_push_reason_unstated": "",
}


class ReplayClassifiedRemovals(unittest.TestCase):
    """The 46 queue removals agents made on Generous-Corp/pulp, 10-03 to 10-09."""

    FIXTURE = pathlib.Path(__file__).with_name("fixtures") / "queue_removals_classified_2026-10.jsonl"

    def test_avoidable_removals_are_refused_and_defect_fixes_allowed(self) -> None:
        rows = [json.loads(line) for line in self.FIXTURE.read_text().splitlines() if line.strip()]
        self.assertEqual(len(rows), 46)
        decisions: dict[str, list[int]] = {"allowed": [], "refused": []}
        refused_avoidable = 0
        for row in rows:
            reason = REASON_FOR_CAUSE[row["cause"]]
            env = {**OVERRIDE, "GHAPP_QUEUE_REMOVAL_REASON": reason,
                   "GHAPP_QUEUE_REMOVAL_NOTE": row["cause"], "HOME": scratch_home(self)}
            with tempfile.TemporaryDirectory() as directory, mock.patch.object(
                guard, "read_target",
                return_value={"repo": "Generous-Corp/pulp", "pr": row["pr"], "head": "a" * 40,
                              "queue_state": "AWAITING_CHECKS"},
            ):
                env["GHAPP_QUEUE_REMOVAL_LOG"] = str(pathlib.Path(directory) / "log.jsonl")
                stderr = io.StringIO()
                with mock.patch.dict(os.environ, env, clear=True), contextlib.redirect_stderr(stderr):
                    code = guard.main(dequeue(f"PR_{row['pr']}"))
            decisions["allowed" if code == 0 else "refused"].append(row["pr"])
            if code != 0 and row["avoidable"] == "yes":
                refused_avoidable += 1
            if row["guard_bypass"].startswith("SHIPYARD_INTERNAL"):
                # The same request as it was actually made: the spoofed marker alone.
                with mock.patch.dict(os.environ, {"SHIPYARD_INTERNAL_QUEUE_MUTATION": "1",
                                                  "HOME": scratch_home(self)}, clear=True), \
                        contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(guard.main(dequeue(f"PR_{row['pr']}")), 1, row)
        defect_fixes = sorted(row["pr"] for row in rows if row["cause"] == "defect_fix_push")
        self.assertEqual(sorted(decisions["allowed"]), defect_fixes)
        self.assertEqual(len(defect_fixes), 7)
        self.assertEqual(refused_avoidable, 36)
        self.assertEqual(len(decisions["refused"]), 39, "36 avoidable + 2 receipt edits + 1 unstated")


if __name__ == "__main__":
    unittest.main()
