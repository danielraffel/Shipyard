import datetime as dt
import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("collect_baseline", Path(__file__).with_name("collect_baseline.py"))
MOD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MOD)
import baseline_pass_history as history


class BaselineScorecardTests(unittest.TestCase):
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
        pass_history = {"five_continuous_24h_days": True, "missing_intervals": [], "failing_intervals": []}
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
            self.assertEqual(result["strata"]["rerun"]["status"], "unavailable")
            self.assertEqual(commands[0][commands[0].index("--project") + 1], "Generous-Corp/pulp")

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

if __name__ == "__main__":
    unittest.main()
