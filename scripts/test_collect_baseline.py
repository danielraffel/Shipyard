import datetime as dt
import contextlib
import importlib.util
import io
import json
import os
import subprocess
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SPEC = importlib.util.spec_from_file_location("collect_baseline", Path(__file__).with_name("collect_baseline.py"))
MOD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MOD)
import baseline_pass_history as history


class BaselineScorecardTests(unittest.TestCase):
    def test_transient_provider_failure_classifier_does_not_retry_validation_errors(self):
        transient = subprocess.CompletedProcess(
            ["shipyard"], 1, "", "Get https://api.github.com: dial tcp 1.2.3.4:443: i/o timeout",
        )
        validation = subprocess.CompletedProcess(["shipyard"], 1, "", "invalid provider export")
        self.assertTrue(MOD.transient_provider_failure(transient))
        self.assertFalse(MOD.transient_provider_failure(validation))
        self.assertFalse(MOD.transient_provider_failure(subprocess.CompletedProcess(["shipyard"], 0, "", "")))

    def test_transient_provider_failure_classifier_covers_provider_outage_markers(self):
        for message in (
            "gh: HTTP 502 Bad Gateway",
            "HTTP 503 Service Unavailable",
            "HTTP 504 Gateway Timeout",
            "HTTP 429: API rate limit exceeded",
            "GraphQL: API rate limit already exceeded",
            "ssh: connect to host m5s port 22: Operation timed out",
            "ssh: Connection closed by remote host",
            "provider: unexpected EOF",
            "provider: temporarily unavailable",
            "provider: deadline exceeded",
            "provider: rate_limit exceeded",
            "provider: timeout",
            "command timed out after 300s: tartci",
        ):
            with self.subTest(message=message):
                self.assertTrue(MOD.transient_provider_failure(
                    subprocess.CompletedProcess(["provider"], 1, "", message)
                ))

    def test_provider_import_retries_command_timeout(self):
        calls = []

        def run_attempt(command, **kwargs):
            calls.append(command)
            if len(calls) == 1:
                raise RuntimeError("command timed out after 300s: provider")
            return subprocess.CompletedProcess(command, 0, '{"imported": 3}', "")

        with mock.patch.object(MOD, "run", side_effect=run_attempt):
            with mock.patch.object(MOD.time, "sleep") as sleep:
                result = MOD.run_provider_with_retry(["provider"], retries=2, delay_seconds=5)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(len(calls), 2)
        sleep.assert_called_once_with(5)

    def test_provider_retry_configuration_is_bounded_and_finite(self):
        with self.assertRaises(ValueError):
            MOD.run_provider_with_retry(["provider"], retries=MOD.MAX_PROVIDER_RETRIES + 1)
        with self.assertRaises(ValueError):
            MOD.run_provider_with_retry(["provider"], delay_seconds=float("nan"))
        with self.assertRaises(ValueError):
            MOD.run_provider_with_retry(["provider"], delay_seconds=MOD.MAX_PROVIDER_RETRY_DELAY_SECONDS + 1)

    def test_provider_import_retries_transient_failure_with_bounded_backoff(self):
        results = [
            subprocess.CompletedProcess(["provider"], 1, "", "dial tcp 1.2.3.4:443: i/o timeout"),
            subprocess.CompletedProcess(["provider"], 0, '{"imported": 3}', ""),
        ]
        calls = []
        with mock.patch.object(MOD, "run", side_effect=lambda command, **kwargs: calls.append(command) or results.pop(0)):
            with mock.patch.object(MOD.time, "sleep") as sleep:
                result = MOD.run_provider_with_retry(["provider"], retries=2, delay_seconds=5)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(len(calls), 2)
        sleep.assert_called_once_with(5)
    def test_coverage_value_text_is_fail_closed_for_unavailable(self):
        self.assertEqual(
            MOD.coverage_value_text({"status": "unavailable", "value": None}),
            "n/a",
        )
        self.assertEqual(
            MOD.coverage_value_text({"status": "available", "value": 0.25}),
            "0.25",
        )

    def test_readiness_rejects_nonfinite_instrumentation_and_separates_lane_eligibility(self):
        started = dt.datetime(2026, 10, 1, tzinfo=dt.timezone.utc)
        observed = started + dt.timedelta(days=5)
        record = {"observed_jobs": 1, "qualified_jobs": 1, "provenance": "authenticated",
                  "coverage": "complete", "reason": "validated"}
        payload = {"schema": MOD.STRATA_CONTRACT, "repo": "o/pulp", "unit": "job",
                   "window_since": "2026-10-01T00:00:00+00:00",
                   "window_until": "2026-10-06T00:00:00+00:00", "source_head": "a" * 40,
                   "generated_at": "2026-10-06T00:00:00+00:00",
                   "strata": {"normal_pr": record, "merge_group": record,
                              "rerun": record, "unclassified": {**record, "qualified_jobs": 0}},
                   "aggregate": {"observed_jobs": 4, "qualified_jobs": 3}}
        card = {
            "instrumentation_coverage": {"status": "available", "continuous": True,
                                          "continuous_days": float("nan")},
            "important_lanes": {"lane-a": {"status": "insufficient", "completed_jobs": 2}},
            "host_generations": {"host-a": {"status": "insufficient", "completed_jobs": 2}},
            "strata": payload,
        }
        pass_history = {"five_continuous_24h_days": True, "missing_intervals": [], "failing_intervals": [],
                        "window_until": observed.isoformat(), "expected_interval_secs": 3600,
                        "cadence_slack_secs": 300}
        card = MOD.consume_authenticated_strata(card, repo="o/pulp", since_days=5, reference_at=observed)
        readiness = MOD.baseline_readiness(card, started, observed, pass_history)
        self.assertFalse(readiness["report_complete"])
        self.assertFalse(readiness["optimization_eligible"])
        self.assertEqual(card["strata_sufficiency"]["normal_pr"]["status"], "sufficient")
        self.assertIn("outage", card["missing_strata"])

        complete_card = {
            "instrumentation_coverage": {"status": "available", "continuous": True,
                                          "continuous_days": 5.0},
            "important_lanes": {"lane-a": {"status": "insufficient", "completed_jobs": 2}},
            "host_generations": {"host-a": {"status": "insufficient", "completed_jobs": 2}},
            "strata": {name: {"provenance": "authenticated", "coverage": "complete", "qualified_jobs": 1}
                       for name in MOD.REQUIRED_STRATA},
            "missing_strata": [],
            "strata_sufficiency": {name: {"status": "sufficient"} for name in MOD.REQUIRED_STRATA},
        }
        readiness = MOD.baseline_readiness(complete_card, started, observed, pass_history)
        self.assertTrue(readiness["report_complete"])
        self.assertTrue(readiness["exit"])
        self.assertFalse(readiness["optimization_eligible"])
        self.assertEqual(readiness["status"], "complete_insufficient")

    def test_readiness_rejects_stale_pass_history_summary(self):
        started = dt.datetime(2026, 10, 1, tzinfo=dt.timezone.utc)
        observed = started + dt.timedelta(days=5)
        card = {
            "instrumentation_coverage": {"status": "available", "continuous": True, "continuous_days": 5.0},
            "important_lanes": {}, "host_generations": {},
            "strata": {name: {"provenance": "authenticated", "coverage": "complete", "qualified_jobs": 1}
                       for name in MOD.REQUIRED_STRATA},
            "missing_strata": [],
            "strata_sufficiency": {name: {"status": "sufficient"} for name in MOD.REQUIRED_STRATA},
        }
        history_summary = {"five_continuous_24h_days": True, "missing_intervals": [], "failing_intervals": [],
                           "window_until": (observed - dt.timedelta(hours=2)).isoformat(),
                           "expected_interval_secs": 3600, "cadence_slack_secs": 300}
        readiness = MOD.baseline_readiness(card, started, observed, history_summary)
        self.assertFalse(readiness["report_complete"])
        self.assertIn("prospective pass history summary is stale", [r["reason"] for r in readiness["reasons"]])

    def test_pass_history_records_empty_success_at_controller_completion(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            observed_at = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
            args = type("Args", (), {
                "repo": "o/pulp", "state": root / "state.json",
                "pass_history_path": root / "pass-history.json",
                "source_identity": "collector:test:v1", "config_identity": "policy:test:v1",
                "expected_interval_secs": 3600, "cadence_slack_secs": 0,
            })()
            report = MOD.record_pass_observation(args, "success", observed_at)
            payload = json.loads(args.pass_history_path.read_text())
            self.assertEqual(payload["observations"][0]["observed_at"], observed_at.isoformat().replace("+00:00", "Z"))
            self.assertEqual(payload["observations"][0]["status"], "empty")
            self.assertTrue(report["trailing_interval_unobserved"])

    def test_default_pass_history_sidecar_is_identity_keyed(self):
        args = type("Args", (), {"repo": "o/pulp", "source_identity": "collector:v1",
                                  "config_identity": "policy:v1", "expected_interval_secs": 3600, "cadence_slack_secs": 300, "state": Path("/tmp/state.json")})()
        first = MOD.default_pass_history_path(args)
        args.config_identity = "policy:v2"
        self.assertNotEqual(first.name, MOD.default_pass_history_path(args).name)

    def test_unavailable_pass_history_still_writes_state(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            started = dt.datetime(2026, 10, 1, tzinfo=dt.timezone.utc)
            state = MOD.update_state(path, "pulp", started, {"missing_strata": ["normal_pr"]}, [],
                                     observed_at=started + dt.timedelta(days=1), pass_history={"status": "unavailable"})
            self.assertTrue(path.exists())
            self.assertEqual(state["baseline_readiness"]["status"], "unavailable")

    def test_pass_history_records_failed_completion_with_error(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            observed_at = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
            args = type("Args", (), {
                "repo": "o/pulp", "state": root / "state.json",
                "pass_history_path": root / "pass-history.json",
                "source_identity": "collector:test:v1", "config_identity": "policy:test:v1",
                "expected_interval_secs": 3600, "cadence_slack_secs": 0,
            })()
            MOD.record_pass_observation(args, "failed", observed_at, error="provider timeout")
            payload = json.loads(args.pass_history_path.read_text())
            self.assertEqual(payload["observations"][0]["status"], "failed")
            self.assertEqual(payload["observations"][0]["error"], "provider timeout")

    def test_pass_history_summary_clamps_to_retention_window(self):
        started = dt.datetime(2026, 10, 1, tzinfo=dt.timezone.utc)
        observed = started + dt.timedelta(days=20)
        store = history.PassHistory.create(
            repository="o/p", source_identity="collector:test:v1", config_identity="policy:test:v1",
            expected_interval_secs=3600,
        )
        first = observed - dt.timedelta(days=14, seconds=7)
        for index in range(24 * 14 + 1):
            point = first + dt.timedelta(hours=index, seconds=(index % 3) * 30)
            store.append(history.PassObservation.success(
                repo="o/p", observed_at=point, expected_interval_secs=3600,
                source_identity="collector:test:v1", config_identity="policy:test:v1"), now=point)
        observed = point
        report = store.summarize(max(started, observed - dt.timedelta(days=14)), observed)
        self.assertFalse(report["missing_intervals"])

    def test_scorecard_delegates_to_rust_and_keeps_strata_unavailable(self):
        with tempfile.TemporaryDirectory() as directory:
            args = type("Args", (), {"db": Path(directory) / "metrics" / "metrics.db", "project": "pulp", "repo": "Generous-Corp/pulp",
                                      "shipyard": "shipyard", "since_days": 5, "timeout_seconds": 300})()
            original = MOD.run
            commands = []
            MOD.run = lambda *a, **k: (commands.append(a[0]) or subprocess.CompletedProcess(a[0], 0, json.dumps({"job_samples": 4}), ""))
            try:
                result = MOD.authoritative_scorecard(args)
            finally:
                MOD.run = original
            self.assertTrue(result["provenance_status"])
            self.assertEqual(result["collector_safe_merge_denominator"]["status"], "unavailable")
            self.assertEqual(result["collector_safe_merge_rate"]["status"], "unavailable")
            self.assertEqual(result["strata"]["rerun"]["status"], "unavailable")
            self.assertEqual(commands[0][commands[0].index("--project") + 1], "Generous-Corp/pulp")

    def test_scorecard_preserves_producer_safe_merge_denominator(self):
        with tempfile.TemporaryDirectory() as directory:
            args = type("Args", (), {"db": Path(directory) / "metrics" / "metrics.db", "project": "pulp", "repo": "Generous-Corp/pulp",
                                      "shipyard": "shipyard", "since_days": 5, "timeout_seconds": 300})()
            original = MOD.run
            MOD.run = lambda *a, **k: subprocess.CompletedProcess(
                a[0], 0, json.dumps({"safely_merged_changes": {"status": "available", "value": 2}}), "")
            try:
                result = MOD.authoritative_scorecard(args)
            finally:
                MOD.run = original
            self.assertEqual(result["collector_safe_merge_denominator"], {"status": "available", "value": 2})
            self.assertEqual(result["collector_safe_merge_rate"]["status"], "unavailable")

    def test_scorecard_preserves_producer_safe_merge_rate(self):
        with tempfile.TemporaryDirectory() as directory:
            args = type("Args", (), {"db": Path(directory) / "metrics" / "metrics.db", "project": "pulp", "repo": "Generous-Corp/pulp",
                                      "shipyard": "shipyard", "since_days": 5, "timeout_seconds": 300})()
            original = MOD.run
            rate = {"status": "partial", "value": 0.25, "reason": "capacity lower bound"}
            MOD.run = lambda *a, **k: subprocess.CompletedProcess(
                a[0], 0, json.dumps({"safely_merged_changes_per_eligible_slot_hour": rate}), "")
            try:
                result = MOD.authoritative_scorecard(args)
            finally:
                MOD.run = original
            self.assertEqual(result["collector_safe_merge_rate"], rate)

    def test_unavailable_handoff_masks_stale_local_receipt_metrics(self):
        card = {
            "safely_merged_changes": {"status": "available", "value": 2},
            "collector_safe_merge_denominator": {"status": "available", "value": 2},
            "safely_merged_changes_per_eligible_slot_hour": {"status": "partial", "value": 0.5},
            "collector_safe_merge_rate": {"status": "partial", "value": 0.5},
            "wasted_compute_per_safely_merged_change": {"status": "partial", "value": 4},
        }
        MOD.force_receipt_metrics_unavailable(card, "handoff is stale")
        for key in ("safely_merged_changes", "collector_safe_merge_denominator",
                    "safely_merged_changes_per_eligible_slot_hour", "collector_safe_merge_rate",
                    "wasted_compute_per_safely_merged_change"):
            self.assertEqual(card[key]["status"], "unavailable")
            self.assertIsNone(card[key]["value"])
            self.assertEqual(card[key]["reason"], "handoff is stale")

    def test_unknown_contract_keeps_all_strata_unavailable(self):
        card = {"strata": {"schema": "shipyard-metrics-strata/v0"}}
        result = MOD.consume_authenticated_strata(card)
        self.assertEqual(result["missing_strata"], list(MOD.REQUIRED_STRATA))
        self.assertEqual(result["provenance_status"], "unverified_organic_input")

    def test_authenticated_partial_contract_preserves_only_qualified_records(self):
        record = {"observed_jobs": 3, "qualified_jobs": 3, "provenance": "authenticated",
                  "coverage": "complete", "reason": "validated"}
        unavailable = {"observed_jobs": 0, "qualified_jobs": 0, "provenance": "unknown",
                       "coverage": "unavailable", "reason": "unknown"}
        card = {"strata": {"schema": MOD.STRATA_CONTRACT, "repo": "o/pulp",
            "window_since": "2026-10-11T00:00:00+00:00", "window_until": "2026-10-16T00:00:00+00:00",
            "source_head": "a" * 40, "generated_at": "2026-10-16T00:00:00+00:00", "unit": "job",
            "strata": {"normal_pr": record, "merge_group": {**record, "coverage": "partial"},
                        "rerun": unavailable, "unclassified": unavailable},
            "aggregate": {"observed_jobs": 6, "qualified_jobs": 6}}}
        result = MOD.consume_authenticated_strata(card, repo="o/pulp", since_days=5,
                                                  reference_at=dt.datetime(2026, 10, 16, tzinfo=dt.timezone.utc))
        self.assertEqual(result["strata"]["normal_pr"], record)
        self.assertEqual(result["strata"]["merge_group"]["coverage"], "partial")
        self.assertEqual(result["strata"]["outage"]["status"], "unavailable")
        self.assertNotIn("normal_pr", result["missing_strata"])
        self.assertIn("merge_group", result["missing_strata"])

    def test_zero_count_is_preserved_but_not_sufficient(self):
        record = {"observed_jobs": 0, "qualified_jobs": 0, "provenance": "authenticated",
                  "coverage": "complete", "reason": "measured empty"}
        unavailable = {"observed_jobs": 0, "qualified_jobs": 0, "provenance": "unknown",
                       "coverage": "unavailable", "reason": "unknown"}
        result = MOD.consume_authenticated_strata({"strata": {"schema": MOD.STRATA_CONTRACT,
            "repo": "o/pulp", "window_since": "2026-10-11T00:00:00+00:00",
            "window_until": "2026-10-16T00:00:00+00:00", "source_head": "a" * 40,
            "generated_at": "2026-10-16T00:00:00+00:00", "unit": "job",
            "strata": {"normal_pr": record, "merge_group": unavailable, "rerun": unavailable,
                        "unclassified": unavailable}, "aggregate": {"observed_jobs": 0, "qualified_jobs": 0}}},
            repo="o/pulp", since_days=5,
            reference_at=dt.datetime(2026, 10, 16, tzinfo=dt.timezone.utc))
        self.assertEqual(result["strata"]["normal_pr"]["qualified_jobs"], 0)
        self.assertIn("normal_pr", result["missing_strata"])

    def test_raw_event_labels_and_unverified_records_are_rejected(self):
        card = {"strata": {"contract": MOD.STRATA_CONTRACT, "authenticated": False,
                            "records": {"normal_pr": {"event": "pull_request", "count": 99}}}}
        result = MOD.consume_authenticated_strata(card)
        self.assertEqual(result["strata"]["normal_pr"]["status"], "unavailable")
        self.assertEqual(result["missing_strata"], list(MOD.REQUIRED_STRATA))

    def test_unavailable_delta_does_not_record_observation(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            now = dt.datetime(2026, 10, 12, tzinfo=dt.timezone.utc)
            state = MOD.update_state(path, "pulp", now, {"missing_strata": []}, [{"new_rows": None}], now)
            self.assertEqual(state["weekday_days_observed"], 0)
            self.assertEqual(state["observations"], [])

    def test_state_requires_observed_weekdays_and_strata(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            started = dt.datetime(2026, 10, 12, tzinfo=dt.timezone.utc)
            state = MOD.update_state(path, "pulp", started, {"missing_strata": ["rerun"]},
                                     [{"new_rows": 1, "source_activity_dates": ["2026-10-12"]}], started)
            self.assertEqual(state["weekday_days_observed"], 1)
            self.assertFalse(state["baseline_exit"])
            self.assertEqual(len(json.loads(path.read_text())["observations"]), 1)

    def test_historical_import_does_not_count_collection_day(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            started = dt.datetime(2026, 10, 16, tzinfo=dt.timezone.utc)
            state = MOD.update_state(path, "pulp", started, {"missing_strata": ["rerun"]},
                                     [{"new_rows": 2, "source_activity_dates": ["2026-09-01"]}], started)
            self.assertEqual(state["weekday_days_observed"], 0)
            self.assertEqual(state["collection_weekday_days"], 1)

    def test_historical_activity_cannot_exit_without_prospective_readiness(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            started = dt.datetime(2026, 10, 1, tzinfo=dt.timezone.utc)
            state = MOD.update_state(path, "pulp", started, {"missing_strata": [],
                "strata": {name: {"provenance": "authenticated", "coverage": "complete",
                                  "qualified_jobs": 1} for name in MOD.REQUIRED_STRATA}},
                [{"new_rows": 1, "source_activity_dates": ["2026-10-01", "2026-10-02", "2026-10-05",
                                                               "2026-10-06", "2026-10-07"]}],
                dt.datetime(2026, 10, 7, tzinfo=dt.timezone.utc))
            self.assertFalse(state["baseline_exit"])
            self.assertEqual(state["baseline_readiness"]["status"], "unavailable")

    def test_mismatched_identity_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); path = root / "state.json"; db = root / "metrics" / "metrics.db"
            db.parent.mkdir(); path.write_text(json.dumps({"identity": {"project": "old", "repo": "o/r",
                "database": str(db.resolve())}, "observations": [{"at": "2026-10-12T00:00:00+00:00"}]}))
            with self.assertRaises(RuntimeError):
                MOD.validate_state_identity(path, "demo", "o/r", db)


class AutomaticObservationControlTests(unittest.TestCase):
    def test_off_preserves_malformed_files_and_enabled_collection_fails_closed(self):
        for value in ["off", "  OfF\t", None]:
            with self.subTest(value=value), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                db = root / "metrics" / "metrics.db"
                db.parent.mkdir()
                state = root / "status.json"
                history_path = root / "history.json"
                markdown = root / "status.md"
                for path in [db, state, history_path, markdown]:
                    path.write_text("existing evidence must remain unchanged")
                originals = {path: path.read_bytes() for path in [db, state, history_path, markdown]}
                called = root / "provider-called"
                provider = root / "provider"
                provider.write_text(
                    "#!%s\nimport pathlib,sys\npathlib.Path(%r).touch()\nsys.exit(3)\n"
                    % (os.sys.executable, str(called))
                )
                provider.chmod(0o755)
                env = os.environ.copy()
                env.pop("SHIPYARD_OBSERVATIONS", None)
                if value is not None:
                    env["SHIPYARD_OBSERVATIONS"] = value
                result = subprocess.run([
                    os.sys.executable, str(Path(MOD.__file__)), "--collect",
                    "--db", str(db), "--project", "pulp", "--repo", "o/pulp",
                    "--state", str(state), "--pass-history", str(history_path),
                    "--status-markdown", str(markdown), "--shipyard", str(provider),
                    "--tartci", str(provider),
                ], env=env, capture_output=True, text=True, check=False)
                if value is not None:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertFalse(called.exists())
                    self.assertIn("collection disabled", result.stderr)
                    self.assertEqual({path: path.read_bytes() for path in originals}, originals)
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertFalse(called.exists(), result.stderr)
                    self.assertRegex(result.stderr, "baseline state is unreadable|baseline collection failed")
                    self.assertEqual({path: path.read_bytes() for path in originals}, originals)

    def test_enabled_valid_state_invokes_provider_and_records_failed_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            db = root / "metrics" / "metrics.db"
            db.parent.mkdir()
            state = root / "status.json"
            state.write_text(json.dumps({"identity": MOD.state_identity(state, "pulp", "o/pulp", db)}))
            history_path = root / "history.json"
            history_path.write_text(json.dumps(history.PassHistory.create(
                repository="o/pulp", source_identity="shipyard.collect_baseline:v1",
                config_identity="baseline-policy:v1;interval=3600;slack=300",
                expected_interval_secs=3600,
            ).to_dict()))
            original_state = state.read_bytes()
            called = root / "provider-called"
            provider = root / "provider"
            provider.write_text(
                "#!%s\nimport pathlib,sys\npathlib.Path(%r).touch()\n"
                "print('invalid export', file=sys.stderr)\nsys.exit(3)\n"
                % (os.sys.executable, str(called))
            )
            provider.chmod(0o755)
            env = os.environ.copy()
            env.pop("SHIPYARD_OBSERVATIONS", None)
            result = subprocess.run([
                os.sys.executable, str(Path(MOD.__file__)), "--collect",
                "--db", str(db), "--project", "pulp", "--repo", "o/pulp",
                "--state", str(state), "--pass-history", str(history_path),
                "--shipyard", str(provider), "--tartci", str(provider),
            ], env=env, capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertTrue(called.exists(), result.stderr)
            self.assertIn("invalid export", result.stderr)
            self.assertEqual(state.read_bytes(), original_state)
            observations = json.loads(history_path.read_text())["observations"]
            self.assertEqual(len(observations), 1)
            self.assertEqual(observations[0]["status"], "failed")
            self.assertIn("invalid export", observations[0]["error"])

class BaselineStateRetentionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "state.json"
        self.started = dt.datetime(2026, 10, 1, tzinfo=dt.timezone.utc)
        self.observed = self.started + dt.timedelta(days=29)
        self.card = {"missing_strata": ["rerun"]}

    def row(self, at):
        return {"at": at.isoformat(), "source_activity_dates": [at.date().isoformat()],
                "events": [{"new_rows": 1}], "scorecard": self.card}

    def seed(self, rows):
        self.path.write_text(json.dumps({"started_at": self.started.isoformat(), "observations": rows}))

    def update(self, **kwargs):
        return MOD.update_state(self.path, "pulp", self.started,
                                kwargs.pop("card", self.card), kwargs.pop("events", []),
                                observed_at=kwargs.pop("observed_at", self.observed), **kwargs)

    def test_age_boundary_and_count_cap_keep_newest_observations(self):
        cutoff = self.observed - dt.timedelta(days=14)
        old = cutoff - dt.timedelta(seconds=1)
        recent = self.observed - dt.timedelta(days=1)
        self.seed([self.row(recent), self.row(old), self.row(cutoff)])
        state = self.update()
        self.assertEqual([row["at"] for row in state["observations"]],
                         [cutoff.isoformat(), recent.isoformat()])
        self.assertEqual(state["observation_retention"]["max_observations"], 1024)
        state = self.update(state_max_observations=1)
        self.assertEqual(state["observations"], [self.row(recent)])
        self.assertEqual(state["collection_days_observed"], 1)
        self.assertEqual(state["source_activity_days_observed"], 1)
        self.assertEqual(state["observation_retention"]["counter_scope"], "retained_observations")
        self.assertEqual(state["started_at"], self.started.isoformat())

    def test_longer_retention_keeps_older_observations(self):
        self.seed([self.row(self.started), self.row(self.observed)])
        state = self.update(state_retention_days=30, state_max_observations=2000)
        self.assertEqual(len(state["observations"]), 2)
        self.assertEqual(state["observation_retention"]["days"], 30)
        self.assertEqual(state["observation_retention"]["max_observations"], 2000)

    def test_default_count_limit_caps_existing_large_state(self):
        self.seed([self.row(self.observed - dt.timedelta(seconds=index)) for index in range(1025)])
        state = self.update()
        self.assertEqual(len(state["observations"]), 1024)
        self.assertEqual(state["observations"][0]["at"], (self.observed - dt.timedelta(seconds=1023)).isoformat())
        self.assertEqual(state["observations"][-1]["at"], self.observed.isoformat())

    def test_repeated_writes_and_status_only_expiry_remain_bounded(self):
        for index in range(20):
            observed = self.started + dt.timedelta(hours=index)
            state = self.update(observed_at=observed, state_max_observations=3,
                                events=[{"new_rows": 1}], card={"sample": index})
            self.assertLessEqual(len(state["observations"]), 3)
            self.assertEqual(json.loads(self.path.read_text()), state)
        self.assertEqual([row["scorecard"]["sample"] for row in state["observations"]], [17, 18, 19])
        state = self.update(record_observation=False)
        self.assertEqual(state["observations"], [])
        self.assertEqual(state["collection_weekday_days"], 0)
        self.assertEqual(state["weekday_days_observed"], 0)

    def test_retention_does_not_change_authoritative_readiness(self):
        observed = self.started + dt.timedelta(days=5)
        store = history.PassHistory.create(repository="o/pulp", source_identity="test",
                                            config_identity="test", expected_interval_secs=3600)
        for index in range(121):
            at = self.started + dt.timedelta(hours=index)
            store.append(history.PassObservation.success(
                repo="o/pulp", observed_at=at, expected_interval_secs=3600,
                source_identity="test", config_identity="test"), now=at)
        summary = store.summarize(self.started, observed)
        card = {
            "instrumentation_coverage": {"status": "available", "continuous": True, "continuous_days": 5},
            "important_lanes": {"lane": {"status": "available", "completed_jobs": 30}},
            "host_generations": {"host": {"status": "available", "completed_jobs": 10}},
            "strata": {name: {"provenance": "authenticated", "coverage": "complete"}
                       for name in MOD.REQUIRED_STRATA},
            "strata_sufficiency": {name: {"status": "sufficient"} for name in MOD.REQUIRED_STRATA},
        }
        expected = MOD.baseline_readiness(card, self.started, observed, summary)
        self.assertTrue(expected["exit"])
        for days, count in ((1, 1), (30, 2000)):
            self.seed([self.row(self.started), self.row(observed)])
            state = self.update(card=card, observed_at=observed, pass_history=summary,
                                state_retention_days=days, state_max_observations=count)
            self.assertEqual(state["baseline_readiness"], expected)
            self.assertEqual(state["pass_history"], summary)
        failed = dict(summary, failing_intervals=[{"reason": "provider failed"}])
        state = self.update(card=card, observed_at=observed, pass_history=failed,
                            state_retention_days=1, state_max_observations=1)
        self.assertFalse(state["baseline_exit"])
        self.assertEqual(state["baseline_readiness"],
                         MOD.baseline_readiness(card, self.started, observed, failed))

    def test_malformed_state_is_preserved_even_when_row_would_expire(self):
        valid = {"started_at": self.started.isoformat(), "observations": [self.row(self.started)]}
        invalid = [[], {"observations": {}}, {"started_at": "2026-10-01", "observations": []},
                   {"observations": [self.row(self.started)]},
                   dict(valid, last_observed_at=(self.observed + dt.timedelta(seconds=1)).isoformat()),
                   dict(valid, observations=[None]),
                   dict(valid, observations=[{"at": "garbage"}]),
                   dict(valid, observations=[{"at": "2026-10-01T00:00:00"}]),
                   dict(valid, observations=[self.row(self.observed + dt.timedelta(seconds=1))]),
                   dict(valid, observations=[dict(self.row(self.started), source_activity_dates=["invalid"])]),
                   dict(valid, observations=[dict(self.row(self.started), events="invalid")]),
                   dict(valid, observations=[dict(self.row(self.started), scorecard=[])])]
        for value in invalid:
            with self.subTest(value=value):
                original = json.dumps(value)
                self.path.write_text(original)
                with self.assertRaises(ValueError):
                    self.update()
                self.assertEqual(self.path.read_text(), original)

    def test_invalid_retention_options_do_not_replace_state(self):
        self.seed([self.row(self.started)])
        original = self.path.read_bytes()
        for name in ("state_retention_days", "state_max_observations"):
            for value in (0, -1, True, 1.5, "14"):
                with self.subTest(name=name, value=value), self.assertRaises(ValueError):
                    self.update(**{name: value})
                self.assertEqual(self.path.read_bytes(), original)

    def test_cli_options_reject_invalid_values_before_collection(self):
        base = ["collect_baseline.py", "--db", str(self.path.parent / "metrics/metrics.db"),
                "--project", "pulp", "--repo", "o/pulp", "--state", str(self.path), "--collect"]
        for option in ("--state-retention-days", "--state-max-observations"):
            for value in ("0", "-1", "1.5"):
                with self.subTest(option=option, value=value), mock.patch.object(sys, "argv", base + [option, value]), \
                        mock.patch.object(MOD, "import_organic") as collect, contextlib.redirect_stderr(io.StringIO()):
                    with self.assertRaises(SystemExit) as raised:
                        MOD.main()
                    self.assertEqual(raised.exception.code, 2)
                    collect.assert_not_called()
        self.assertFalse(self.path.exists())

    def test_cli_status_only_threads_retention_without_importing(self):
        self.seed([self.row(self.started)])
        argv = ["collect_baseline.py", "--db", str(self.path.parent / "metrics/metrics.db"),
                "--project", "pulp", "--repo", "o/pulp", "--state", str(self.path),
                "--state-retention-days", "30", "--state-max-observations", "2"]
        # Identity is mandatory when old observations are present.
        state = json.loads(self.path.read_text())
        state["identity"] = MOD.state_identity(self.path, "pulp", "o/pulp", self.path.parent / "metrics/metrics.db")
        self.path.write_text(json.dumps(state))
        output = io.StringIO()
        with mock.patch.object(sys, "argv", argv), mock.patch.object(MOD, "import_organic") as collect, \
                mock.patch.object(MOD, "authoritative_scorecard", return_value=self.card), \
                contextlib.redirect_stdout(output):
            self.assertEqual(MOD.main(), 0)
        collect.assert_not_called()
        self.assertEqual(json.loads(output.getvalue())["baseline"]["observation_retention"]["max_observations"], 2)
        self.assertFalse(json.loads(self.path.read_text())["baseline_exit"])

    def test_cli_bad_state_stops_before_imports_or_history_mutation(self):
        self.path.write_text("[]")
        argv = ["collect_baseline.py", "--db", str(self.path.parent / "metrics/metrics.db"),
                "--project", "pulp", "--repo", "o/pulp", "--state", str(self.path), "--collect"]
        with mock.patch.object(sys, "argv", argv), mock.patch.object(MOD, "import_organic") as collect, \
                mock.patch.object(MOD, "record_pass_observation") as record, contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(MOD.main(), 1)
        collect.assert_not_called()
        record.assert_not_called()
        self.assertEqual(self.path.read_text(), "[]")

    def test_atomic_replace_failure_preserves_original_state(self):
        self.seed([self.row(self.started)])
        original = self.path.read_bytes()
        with mock.patch.object(MOD.os, "replace", side_effect=OSError("replace failed")):
            with self.assertRaisesRegex(OSError, "replace failed"):
                self.update()
        self.assertEqual(self.path.read_bytes(), original)

    def test_cli_invalid_identity_preserves_state_and_history_in_both_modes(self):
        db = self.path.parent / "metrics/metrics.db"
        pass_path = self.path.parent / "passes.json"
        pass_path.write_text("existing history must not be touched")
        base = ["collect_baseline.py", "--db", str(db), "--project", "pulp",
                "--repo", "o/pulp", "--state", str(self.path), "--pass-history", str(pass_path)]
        valid_identity = MOD.state_identity(self.path, "pulp", "o/pulp", db)
        for identity in (None, [], "invalid", dict(valid_identity, repo="other/pulp")):
            for collect_flag in ([], ["--collect"]):
                with self.subTest(identity=identity, collect=bool(collect_flag)):
                    state = {"started_at": self.started.isoformat(), "observations": [self.row(self.started)]}
                    if identity is not None:
                        state["identity"] = identity
                    original = json.dumps(state)
                    self.path.write_text(original)
                    with mock.patch.object(sys, "argv", base + collect_flag), \
                            mock.patch.object(MOD, "import_organic") as collect, \
                            mock.patch.object(MOD, "authoritative_scorecard") as scorecard, \
                            mock.patch.object(MOD, "record_pass_observation", wraps=MOD.record_pass_observation) as record, \
                            contextlib.redirect_stderr(io.StringIO()):
                        self.assertEqual(MOD.main(), 1)
                    collect.assert_not_called()
                    scorecard.assert_not_called()
                    record.assert_not_called()
                    self.assertEqual(self.path.read_text(), original)
                    self.assertEqual(pass_path.read_text(), "existing history must not be touched")

    def test_cli_project_repo_mismatch_stops_before_collection_or_history(self):
        argv = ["collect_baseline.py", "--db", str(self.path.parent / "metrics/metrics.db"),
                "--project", "other", "--repo", "o/pulp", "--state", str(self.path)]
        for collect_flag in ([], ["--collect"]):
            with self.subTest(collect=bool(collect_flag)), mock.patch.object(sys, "argv", argv + collect_flag), \
                    mock.patch.object(MOD, "import_organic") as collect, \
                    mock.patch.object(MOD, "authoritative_scorecard") as scorecard, \
                    mock.patch.object(MOD, "record_pass_observation") as record, \
                    contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(MOD.main(), 1)
            collect.assert_not_called()
            scorecard.assert_not_called()
            record.assert_not_called()
            self.assertFalse(self.path.exists())

    def test_provider_failure_after_valid_preflight_records_failed_pass(self):
        db = self.path.parent / "metrics/metrics.db"
        pass_path = self.path.parent / "passes.json"
        state = {"started_at": self.started.isoformat(), "observations": [self.row(self.started)],
                 "identity": MOD.state_identity(self.path, "pulp", "o/pulp", db)}
        original = json.dumps(state)
        self.path.write_text(original)
        argv = ["collect_baseline.py", "--db", str(db), "--project", "pulp", "--repo", "o/pulp",
                "--state", str(self.path), "--pass-history", str(pass_path), "--collect"]
        with mock.patch.object(sys, "argv", argv), \
                mock.patch.object(MOD, "import_organic", side_effect=RuntimeError("provider unavailable")) as collect, \
                mock.patch.object(MOD, "authoritative_scorecard") as scorecard, \
                contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(MOD.main(), 1)
        collect.assert_called_once()
        scorecard.assert_not_called()
        self.assertEqual(self.path.read_text(), original)
        observations = json.loads(pass_path.read_text())["observations"]
        self.assertEqual(len(observations), 1)
        self.assertEqual(observations[0]["status"], "failed")
        self.assertEqual(observations[0]["error"], "provider unavailable")
        self.assertFalse(observations[0]["completed"])


class CollectorInvocationTests(unittest.TestCase):
    def test_project_repo_scope_requires_owner_and_matching_short_name(self):
        for project, repo in [("pulp", "pulp"), ("pulp", "/pulp"), ("pulp", "a/b/pulp"), ("other", "o/pulp")]:
            with self.assertRaises(ValueError):
                MOD.validate_project_repo(project, repo)

    def test_tartci_export_rejects_duplicate_and_non_string_identity(self):
        duplicate = [{"project": "pulp", "repo": "o/pulp", "provider": "tart", "external_id": "x"},
                     {"project": "pulp", "repo": "o/pulp", "provider": "tart", "external_id": "x"}]
        with self.assertRaises(RuntimeError):
            MOD.validate_tartci_export(json.dumps(duplicate), "o/pulp", "pulp")
        bad = {"project": "pulp", "repo": "o/pulp", "provider": "tart", "external_id": 1}
        with self.assertRaises(RuntimeError):
            MOD.validate_tartci_export(json.dumps(bad), "o/pulp", "pulp")

    def test_prepare_tartci_export_excludes_unjoinable_local_attempts(self):
        text = "\n".join(json.dumps(record) for record in [
            {"project": "pulp", "repo": "o/pulp", "provider": "tart", "external_id": "job-1"},
            {"project": "pulp", "repo": "o/pulp", "provider": "tart", "external_id": ""},
        ])
        normalized, excluded = MOD.prepare_tartci_export(text, "o/pulp", "pulp")
        self.assertEqual(excluded, 1)
        self.assertEqual(json.loads(normalized)[0]["external_id"], "job-1")

    def test_tartci_export_command_can_read_a_named_fleet_host(self):
        args = type("Args", (), {"tartci": "tartci", "tartci_host": "m5s",
                                  "tartci_remote_command": "tartci", "repo": "o/pulp",
                                  "since_days": 5})()
        self.assertEqual(MOD.tartci_export_command(args), [
            "ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "m5s", "tartci",
            "runtime", "export", "--repo", "o/pulp", "--since-days", "5",
        ])
        args.tartci_host = "m5s;touch"
        with self.assertRaises(ValueError):
            MOD.tartci_export_command(args)

    def test_collect_uses_selected_db_and_counts_only_new_rows(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); db = root / "metrics" / "metrics.db"; db.parent.mkdir()
            conn = __import__("sqlite3").connect(db)
            conn.executescript("create table runs(id integer primary key, project text, repo text, ts text, status text); create table jobs(id integer primary key, run_id integer, job text, started_at text, completed_at text, status text);")
            conn.execute("insert into runs values (1, 'pulp', 'o/pulp', '2020-01-01T12:00:00Z', 'success')")
            conn.execute("insert into jobs values (1, 1, 'job', '2026-10-16T12:00:00Z', '2026-10-16T12:30:00Z', 'success')")
            conn.commit(); conn.close()
            shipyard = root / "shipyard"; tartci = root / "tartci"
            shipyard.write_text("#!/bin/sh\ncase \"$*\" in *metrics*import*) echo '{\"imported\": 1}';; *) echo '{\"job_samples\": 1}';; esac\n")
            tartci.write_text("#!/bin/sh\necho '{\"project\":\"pulp\",\"repo\":\"o/pulp\",\"provider\":\"tart\",\"external_id\":\"fixture-1\"}'\n")
            shipyard.chmod(0o755); tartci.chmod(0o755)
            args = type("Args", (), {"db": db, "state": root / "state.json", "project": "pulp", "repo": "o/pulp", "shipyard": str(shipyard), "tartci": str(tartci), "limit": 1, "since_days": 5, "timeout_seconds": 5})()
            args.state.write_text("{}")
            original = MOD.count_jobs
            try:
                events = MOD.import_organic(args)
            finally:
                MOD.count_jobs = original
            self.assertEqual(events[-1]["new_rows"], 0)
            self.assertEqual(events[-1]["source_activity_dates"], ["2026-10-16"])
            self.assertEqual([event["source"] for event in events],
                             ["github", "tartci", "merge-receipt", "collection"])
            self.assertEqual(events[2]["status"], "unavailable")

    def test_collect_without_receipts_marks_denominator_unavailable(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); db = root / "metrics" / "metrics.db"; db.parent.mkdir()
            conn = __import__("sqlite3").connect(db)
            conn.executescript("create table runs(id integer primary key, project text, repo text); create table jobs(id integer primary key, run_id integer, started_at text, completed_at text);")
            conn.commit(); conn.close()
            shipyard = root / "shipyard"; tartci = root / "tartci"
            shipyard.write_text("#!/bin/sh\ncase \"$*\" in *metrics*import*) echo '{\"imported\": 1}';; *) echo '{}';; esac\n")
            tartci.write_text("#!/bin/sh\necho '{\"project\":\"p\",\"repo\":\"o/p\",\"provider\":\"tart\",\"external_id\":\"fixture-1\"}'\n")
            shipyard.chmod(0o755); tartci.chmod(0o755)
            args = type("Args", (), {"db": db, "state": root / "state.json", "project": "p", "repo": "o/p", "shipyard": str(shipyard), "tartci": str(tartci), "limit": 1, "since_days": 5, "timeout_seconds": 5, "merge_receipt_unavailable_reason": "handoff check failed: source empty"})()
            args.state.write_text("{}")
            events = MOD.import_organic(args)
            receipt = next(event for event in events if event["source"] == "merge-receipt")
            self.assertEqual(receipt["status"], "unavailable")
            self.assertIn("source empty", receipt["reason"])

    def test_collect_allows_missing_merge_receipt_handoff(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); db = root / "metrics" / "metrics.db"; db.parent.mkdir()
            conn = __import__("sqlite3").connect(db)
            conn.executescript("create table runs(id integer primary key, project text, repo text); create table jobs(id integer primary key, run_id integer, started_at text, completed_at text);")
            conn.commit(); conn.close()
            shipyard = root / "shipyard"; tartci = root / "tartci"
            shipyard.write_text("#!/bin/sh\ncase \"$*\" in *merge-receipt*) echo 'unexpected merge receipt import' >&2; exit 1;; *) echo '{\"imported\": 1}' ;; esac\n")
            tartci.write_text("#!/bin/sh\necho '{\"project\":\"p\",\"repo\":\"o/p\",\"provider\":\"tart\",\"external_id\":\"fixture-1\"}'\n")
            shipyard.chmod(0o755); tartci.chmod(0o755)
            args = type("Args", (), {"db": db, "state": root / "state.json", "project": "p", "repo": "o/p",
                "shipyard": str(shipyard), "tartci": str(tartci), "limit": 1, "since_days": 5,
                "timeout_seconds": 5, "merge_receipts": root / "missing-receipts.jsonl"})()
            args.state.write_text("{}")
            events = MOD.import_organic(args)
            receipt = next(event for event in events if event.get("source") == "merge-receipt")
            self.assertEqual(receipt["source"], "merge-receipt")
            self.assertEqual(receipt["status"], "unavailable")
            self.assertIn("handoff", receipt["reason"])
            self.assertEqual(events[-1]["source"], "collection")

    def test_collect_deduplicates_unavailable_event_from_newer_supervisor(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); db = root / "metrics" / "metrics.db"; db.parent.mkdir()
            args = type("Args", (), {"db": db, "state": root / "state.json", "project": "p",
                "repo": "o/p", "merge_receipts": root / "missing-receipts.jsonl"})()
            args.state.write_text("{}")
            provider_events = [
                {"source": "github", "imported": 1},
                {"source": "merge-receipt", "status": "unavailable", "reason": "wrapper"},
            ]
            with mock.patch.object(MOD, "_import_organic_after_preflight", return_value=provider_events):
                events = MOD.import_organic(args)
            self.assertEqual(sum(event.get("source") == "merge-receipt" for event in events), 1)
            self.assertEqual(events[-1]["reason"], "wrapper")

    def test_collect_does_not_treat_dangling_merge_receipt_link_as_absent(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            handoff = root / "missing-receipts.jsonl"
            os.symlink(root / "does-not-exist.jsonl", handoff)
            args = type("Args", (), {"db": root / "metrics" / "metrics.db", "state": root / "state.json",
                "project": "p", "repo": "o/p", "merge_receipts": handoff})()
            args.state.write_text("{}")
            with self.assertRaisesRegex(RuntimeError, "health sidecar is required"):
                MOD.import_organic(args)

    def test_collect_refuses_unknown_provider_counts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); db = root / "metrics" / "metrics.db"; db.parent.mkdir()
            conn = __import__("sqlite3").connect(db)
            conn.executescript("create table runs(id integer primary key, project text, repo text); create table jobs(id integer primary key, run_id integer, started_at text, completed_at text);")
            conn.commit(); conn.close()
            shipyard = root / "shipyard"; tartci = root / "tartci"
            shipyard.write_text("#!/bin/sh\ncase \"$*\" in *metrics*import*) echo '{\"status\":\"ok\"}';; *) echo '{}';; esac\n")
            tartci.write_text("#!/bin/sh\necho '{\"project\":\"p\",\"repo\":\"o/p\",\"provider\":\"tart\",\"external_id\":\"fixture-1\"}'\n")
            shipyard.chmod(0o755); tartci.chmod(0o755)
            args = type("Args", (), {"db": db, "state": root / "state.json", "project": "p", "repo": "o/p",
                "shipyard": str(shipyard), "tartci": str(tartci), "limit": 1, "since_days": 5,
                "timeout_seconds": 5})()
            args.state.write_text("{}")
            with self.assertRaisesRegex(RuntimeError, "count unavailable"):
                MOD.import_organic(args)

    def test_collect_fails_closed_when_merge_receipt_import_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); db = root / "metrics" / "metrics.db"; db.parent.mkdir()
            conn = __import__("sqlite3").connect(db)
            conn.executescript("create table runs(id integer primary key, project text, repo text); create table jobs(id integer primary key, run_id integer, started_at text, completed_at text);")
            conn.commit(); conn.close()
            shipyard = root / "shipyard"; tartci = root / "tartci"; receipts = root / "receipts.jsonl"
            shipyard.write_text("#!/bin/sh\ncase \"$*\" in *merge-receipt*) echo 'bad receipt' >&2; exit 1;; *metrics*import*) echo '{\"imported\": 1}';; *) echo '{}';; esac\n")
            tartci.write_text("#!/bin/sh\necho '{\"project\":\"p\",\"repo\":\"o/p\",\"provider\":\"tart\",\"external_id\":\"fixture-1\"}'\n")
            receipts.write_text(json.dumps({"repo": "o/p"}) + "\n")
            shipyard.chmod(0o755); tartci.chmod(0o755)
            args = type("Args", (), {"db": db, "state": root / "state.json", "project": "p", "repo": "o/p",
                "shipyard": str(shipyard), "tartci": str(tartci), "limit": 1, "since_days": 5,
                "timeout_seconds": 5, "merge_receipts": receipts})()
            args.state.write_text("{}")
            with self.assertRaisesRegex(RuntimeError, "health sidecar is required"):
                MOD.import_organic(args)

    def test_merge_receipt_health_preflight_runs_before_provider_imports(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); db = root / "metrics" / "metrics.db"; db.parent.mkdir()
            shipyard = root / "shipyard"; tartci = root / "tartci"; receipts = root / "receipts.jsonl"
            marker = root / "provider-ran"
            shipyard.write_text(f"#!/bin/sh\ntouch '{marker}'\nexit 1\n")
            tartci.write_text("#!/bin/sh\ntouch '%s'\nexit 1\n" % marker)
            receipts.write_text(json.dumps({"repo": "o/p"}) + "\n")
            shipyard.chmod(0o755); tartci.chmod(0o755)
            args = type("Args", (), {"db": db, "state": root / "state.json", "project": "p", "repo": "o/p",
                "shipyard": str(shipyard), "tartci": str(tartci), "limit": 1, "since_days": 5,
                "timeout_seconds": 5, "merge_receipts": receipts})()
            args.state.write_text("{}")
            with self.assertRaisesRegex(RuntimeError, "health sidecar is required"):
                MOD.import_organic(args)
            self.assertFalse(marker.exists(), "provider imports must not run before receipt preflight")

    def test_merge_receipt_health_binds_bytes_and_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            handoff = root / "receipts.jsonl"
            health = root / "receipts.health.json"
            value = {"schema": "shipyard-merge-receipt/v1", "receipt_id": "r-1", "repo": "owner/repo",
                     "project": "repo", "base_branch": "main", "pr": 1, "head_sha": "a" * 40,
                     "validated_head_sha": "a" * 40, "merge_commit_sha": "b" * 40,
                     "merged_at": "2026-10-26T00:00:00Z", "safe_merge": True, "source": "shipyard",
                     "required_checks": ["test"], "gate_receipt_sha256": "c" * 64, "evidence": "sha256:r-1"}
            handoff.write_text(json.dumps(value) + "\n")
            payload = handoff.read_bytes()
            generated = MOD.receipt_handoff.health_payload(
                repo="owner/repo", file=handoff, checked_at=dt.datetime.now(dt.timezone.utc),
                status="fresh", exported_at=dt.datetime.now(dt.timezone.utc),
                source_imported_at=dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z"),
                stale_after_seconds=7200,
                receipts=[value], payload=payload,
                producer={"identity": "shipyard", "version": "shipyard 0.366.4", "executable_sha256": "a" * 64})
            health.write_text(json.dumps(generated))
            handoff.write_text(json.dumps({**value, "evidence": "sha256:tampered"}) + "\n")
            with self.assertRaisesRegex(RuntimeError, "(byte count|digest) does not match"):
                MOD.validate_merge_receipt_health(health, handoff, "owner/repo", "2h")

    def test_merge_receipt_health_rejects_stale_before_import(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            handoff = root / "receipts.jsonl"
            health = root / "receipts.health.json"
            value = {"schema": "shipyard-merge-receipt/v1", "receipt_id": "r-1", "repo": "owner/repo",
                     "project": "repo", "base_branch": "main", "pr": 1, "head_sha": "a" * 40,
                     "validated_head_sha": "a" * 40, "merge_commit_sha": "b" * 40,
                     "merged_at": "2026-10-26T00:00:00Z", "safe_merge": True, "source": "shipyard",
                     "required_checks": ["test"], "gate_receipt_sha256": "c" * 64, "evidence": "sha256:r-1"}
            handoff.write_text(json.dumps(value) + "\n")
            payload = handoff.read_bytes()
            old = dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=3)
            generated = MOD.receipt_handoff.health_payload(
                repo="owner/repo", file=handoff, checked_at=old, status="fresh", exported_at=old,
                source_imported_at=old.isoformat().replace("+00:00", "Z"),
                stale_after_seconds=7200,
                receipts=[value], payload=payload,
                producer={"identity": "shipyard", "version": "shipyard 0.366.4", "executable_sha256": "a" * 64})
            health.write_text(json.dumps(generated))
            with self.assertRaisesRegex(RuntimeError, "stale"):
                MOD.validate_merge_receipt_health(health, handoff, "owner/repo", "2h")

    def test_merge_receipt_health_rejects_unknown_fields_and_wrong_binding(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            handoff = root / "receipts.jsonl"
            health = root / "receipts.health.json"
            value = {"schema": "shipyard-merge-receipt/v1", "receipt_id": "r-1", "repo": "owner/repo",
                     "project": "repo", "base_branch": "main", "pr": 1, "head_sha": "a" * 40,
                     "validated_head_sha": "a" * 40, "merge_commit_sha": "b" * 40,
                     "merged_at": "2026-10-26T00:00:00Z", "safe_merge": True, "source": "shipyard",
                     "required_checks": ["test"], "gate_receipt_sha256": "c" * 64, "evidence": "sha256:r-1"}
            handoff.write_text(json.dumps(value) + "\n")
            payload = handoff.read_bytes()
            generated = MOD.receipt_handoff.health_payload(
                repo="owner/repo", file=handoff, checked_at=dt.datetime.now(dt.timezone.utc),
                status="fresh", exported_at=dt.datetime.now(dt.timezone.utc), receipts=[value], payload=payload,
                source_imported_at=dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z"),
                stale_after_seconds=7200,
                source_host="m3",
                producer={"identity": "shipyard", "version": "shipyard 0.366.4", "executable_sha256": "a" * 64})
            generated["unexpected"] = True
            health.write_text(json.dumps(generated))
            with self.assertRaisesRegex(RuntimeError, "unknown fields"):
                MOD.validate_merge_receipt_health(health, handoff, "owner/repo", "2h")
            generated.pop("unexpected")
            generated["base_branch"] = "develop"
            health.write_text(json.dumps(generated))
            with self.assertRaisesRegex(RuntimeError, "base branch"):
                MOD.validate_merge_receipt_health(health, handoff, "owner/repo", "2h")
            generated["base_branch"] = "main"
            health.write_text(json.dumps(generated))
            MOD.validate_merge_receipt_health(health, handoff, "owner/repo", "2h", expected_source_host="m3")
            with self.assertRaisesRegex(RuntimeError, "expected m1"):
                MOD.validate_merge_receipt_health(health, handoff, "owner/repo", "2h", expected_source_host="m1")

    def test_import_uses_immutable_validated_handoff_snapshot(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            handoff = root / "receipts.jsonl"
            health = root / "receipts.health.json"
            value = {"schema": "shipyard-merge-receipt/v1", "receipt_id": "r-1", "repo": "owner/repo",
                     "project": "repo", "base_branch": "main", "pr": 1, "head_sha": "a" * 40,
                     "validated_head_sha": "a" * 40, "merge_commit_sha": "b" * 40,
                     "merged_at": "2026-10-26T00:00:00Z", "safe_merge": True, "source": "shipyard",
                     "required_checks": ["test"], "gate_receipt_sha256": "c" * 64, "evidence": "sha256:r-1"}
            handoff.write_text(json.dumps(value) + "\n")
            payload = handoff.read_bytes()
            generated = MOD.receipt_handoff.health_payload(
                repo="owner/repo", file=handoff, checked_at=dt.datetime.now(dt.timezone.utc),
                status="fresh", exported_at=dt.datetime.now(dt.timezone.utc), receipts=[value], payload=payload,
                source_imported_at=dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z"),
                stale_after_seconds=7200,
                producer={"identity": "shipyard", "version": "shipyard 0.366.4", "executable_sha256": "a" * 64})
            health.write_text(json.dumps(generated))
            args = type("Args", (), {"db": root / "metrics" / "metrics.db", "state": root / "state.json",
                "project": "repo", "repo": "owner/repo", "shipyard": "shipyard", "tartci": "tartci",
                "limit": 1, "since_days": 5, "timeout_seconds": 5, "merge_receipts": handoff,
                "merge_receipt_health": health})()
            args.state.write_text("{}")
            observed_snapshot: list[Path] = []
            original_run = MOD.run
            def fake_run(command, *, stdin=None, timeout=300):
                if "metrics" in command and "import" in command and "merge-receipt" in command:
                    snapshot = Path(command[command.index("--file") + 1]); observed_snapshot.append(snapshot)
                    self.assertEqual(snapshot.read_bytes(), payload)
                    return subprocess.CompletedProcess(command, 0, '{"imported": 1}', "")
                if command[:1] == ["tartci"]:
                    handoff.write_text("tampered after preflight\n")
                    return subprocess.CompletedProcess(command, 0,
                        '{"project":"repo","repo":"owner/repo","provider":"tart","external_id":"id-1"}', "")
                handoff.write_text("tampered after preflight\n")
                return subprocess.CompletedProcess(command, 0, '{"imported": 1}', "")
            MOD.run = fake_run
            try:
                events = MOD.import_organic(args)
            finally:
                MOD.run = original_run
            self.assertEqual(len(observed_snapshot), 1)
            self.assertFalse(observed_snapshot[0].exists())
            self.assertEqual(events[2]["source"], "merge-receipt")

    def test_collect_rejects_merge_receipt_for_another_repository(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "receipts.jsonl"
            path.write_text(json.dumps({"repo": "other/repo"}) + "\n")
            with self.assertRaisesRegex(RuntimeError, "repository other than"):
                MOD.validate_merge_receipt_scope(path, "owner/repo")

class BaselineProviderRetryIntegrationTests(unittest.TestCase):
    GITHUB_TIMEOUT = "metrics GitHub observation timed out"
    TARTCI_TIMEOUT = "ssh: connect to host example port 22: Operation timed out"

    def args(self, root, retries=2):
        db = root / "metrics" / "metrics.db"
        db.parent.mkdir()
        with sqlite3.connect(db) as connection:
            connection.executescript(
                "create table runs(id integer primary key, project text, repo text);"
                "create table jobs(id integer primary key, run_id integer, started_at text, completed_at text);"
            )
        return MOD.argparse.Namespace(
            db=db, state=root / "state.json", project="pulp", repo="o/pulp",
            shipyard="shipyard", tartci="tartci", limit=1, since_days=5,
            timeout_seconds=300, provider_retries=retries, provider_retry_delay_seconds=5,
        )

    def provider_runner(self, failures, calls):
        def run(command, **kwargs):
            if command[:3] == ["tartci", "runtime", "export"]:
                phase = "tartci_export"
            elif command[3:6] == ["metrics", "import", "github"]:
                phase = "github"
            elif command[3:6] == ["metrics", "import", "tartci"]:
                phase = "tartci_import"
            else:
                self.fail(f"unexpected provider command: {command}")
            calls.append(phase)
            if failures.get(phase, 0):
                failures[phase] -= 1
                error = self.GITHUB_TIMEOUT if phase == "github" else self.TARTCI_TIMEOUT
                return subprocess.CompletedProcess(command, 1, "", error)
            if phase == "tartci_export":
                payload = '{"project":"pulp","repo":"o/pulp","provider":"tart","external_id":"fixture-1"}'
            else:
                payload = '{"imported": 1}'
            return subprocess.CompletedProcess(command, 0, payload, "")
        return run

    def test_each_provider_timeout_recovers_through_import_organic(self):
        for provider, expected_calls in (
            ("github", ["github", "github", "tartci_export", "tartci_import"]),
            ("tartci_export", ["github", "tartci_export", "tartci_export", "tartci_import"]),
        ):
            with self.subTest(provider=provider), tempfile.TemporaryDirectory() as directory:
                args = self.args(Path(directory))
                calls = []
                with mock.patch.object(MOD, "run", side_effect=self.provider_runner({provider: 1}, calls)), \
                        mock.patch.object(MOD.time, "sleep") as sleep:
                    events = MOD.import_organic(args)
                self.assertEqual(calls, expected_calls)
                sleep.assert_called_once_with(5)
                self.assertEqual([(e["source"], e["status"], e["imported"]) for e in events[:2]],
                                 [("github", "completed", 1), ("tartci", "completed", 1)])
                self.assertEqual(events[-1]["status"], "completed")

    def test_each_provider_exhaustion_records_failed_history_and_exit_one(self):
        for provider, expected_calls, error in (
            ("github", ["github"] * 3, "GitHub import failed: " + self.GITHUB_TIMEOUT),
            ("tartci_export", ["github"] + ["tartci_export"] * 3, "TartCI export failed: " + self.TARTCI_TIMEOUT),
        ):
            with self.subTest(provider=provider), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                args = self.args(root)
                history_path = root / "pass-history.json"
                argv = ["collect_baseline.py", "--db", str(args.db), "--project", args.project,
                        "--repo", args.repo, "--state", str(args.state), "--collect",
                        "--pass-history", str(history_path)]
                calls, stderr, stdout = [], io.StringIO(), io.StringIO()
                with mock.patch.object(MOD.sys, "argv", argv), \
                        mock.patch.object(MOD.sys, "stderr", stderr), \
                        mock.patch.object(MOD.sys, "stdout", stdout), \
                        mock.patch.object(MOD, "run", side_effect=self.provider_runner({provider: 100}, calls)), \
                        mock.patch.object(MOD.time, "sleep") as sleep, \
                        mock.patch.object(MOD, "authoritative_scorecard") as scorecard, \
                        mock.patch.object(MOD, "update_state") as update_state:
                    exit_code = MOD.main()
                self.assertEqual(exit_code, 1)
                self.assertEqual(calls, expected_calls)
                self.assertEqual(sleep.call_args_list, [mock.call(5), mock.call(10)])
                scorecard.assert_not_called()
                update_state.assert_not_called()
                self.assertFalse(args.state.exists(), "failed pass must not publish successful state")
                self.assertEqual(stdout.getvalue(), "")
                self.assertIn("baseline collection failed: " + error, stderr.getvalue())
                self.assertNotIn("prospective pass history unavailable", stderr.getvalue())
                observations = json.loads(history_path.read_text())["observations"]
                self.assertEqual(len(observations), 1)
                self.assertEqual(observations[0]["status"], "failed")
                self.assertEqual(observations[0]["error"], error)

    def test_zero_retries_stops_each_provider_after_one_attempt(self):
        for provider, expected_calls, error in (
            ("github", ["github"], "GitHub import failed: " + self.GITHUB_TIMEOUT),
            ("tartci_export", ["github", "tartci_export"], "TartCI export failed: " + self.TARTCI_TIMEOUT),
        ):
            with self.subTest(provider=provider), tempfile.TemporaryDirectory() as directory:
                args = self.args(Path(directory), retries=0)
                calls = []
                with mock.patch.object(MOD, "run", side_effect=self.provider_runner({provider: 100}, calls)), \
                        mock.patch.object(MOD.time, "sleep") as sleep:
                    with self.assertRaises(RuntimeError) as caught:
                        MOD.import_organic(args)
                self.assertEqual(str(caught.exception), error)
                self.assertEqual(calls, expected_calls)
                sleep.assert_not_called()

    def test_provider_metadata_does_not_override_a_terminal_error(self):
        for stdout, stderr in (
            ('{"timeout_seconds":300}', "invalid export"),
            ('{"deadline":"2026-10-30T00:00:00Z"}', "authentication failed"),
            ('{"timeout_seconds":300,"deadline":"configured"}', ""),
            ("", "invalid export field timeout_seconds or deadline"),
        ):
            with self.subTest(stdout=stdout, stderr=stderr):
                failed = subprocess.CompletedProcess(["provider"], 1, stdout, stderr)
                self.assertFalse(MOD.transient_provider_failure(failed))
                with mock.patch.object(MOD, "run", return_value=failed) as run, \
                        mock.patch.object(MOD.time, "sleep") as sleep:
                    result = MOD.run_provider_with_retry(["provider"])
                self.assertIs(result, failed)
                run.assert_called_once()
                sleep.assert_not_called()

    def test_stdout_transport_diagnostic_remains_retryable_without_stderr(self):
        for diagnostic in ("HTTP 503 Service Unavailable", "provider: timeout", "context deadline exceeded"):
            with self.subTest(diagnostic=diagnostic):
                result = subprocess.CompletedProcess(["provider"], 1, diagnostic, "")
                self.assertTrue(MOD.transient_provider_failure(result))


if __name__ == "__main__":
    unittest.main()
