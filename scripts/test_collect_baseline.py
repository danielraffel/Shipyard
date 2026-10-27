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
    def test_readiness_gate_is_opt_in_and_requires_optimization_eligibility(self):
        self.assertEqual(MOD.readiness_gate_exit({"status": "insufficient", "optimization_eligible": False}, False), 0)
        self.assertEqual(MOD.readiness_gate_exit({"status": "insufficient", "optimization_eligible": False}, True), 3)
        self.assertEqual(MOD.readiness_gate_exit({"status": "complete_insufficient", "optimization_eligible": False}, True), 3)
        self.assertEqual(MOD.readiness_gate_exit({"status": "ready", "optimization_eligible": True}, True), 0)
        self.assertEqual(MOD.readiness_gate_exit({}, True), 3)

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

    def test_unavailable_handoff_masks_stale_local_receipt_metrics(self):
        card = {
            "safely_merged_changes": {"status": "available", "value": 2},
            "collector_safe_merge_denominator": {"status": "available", "value": 2},
            "wasted_compute_per_safely_merged_change": {"status": "partial", "value": 4},
        }
        MOD.force_receipt_metrics_unavailable(card, "handoff is stale")
        for key in ("safely_merged_changes", "collector_safe_merge_denominator",
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

if __name__ == "__main__":
    unittest.main()
