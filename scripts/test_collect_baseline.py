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


class BaselineScorecardTests(unittest.TestCase):
    def test_scorecard_delegates_to_rust_and_keeps_strata_unavailable(self):
        with tempfile.TemporaryDirectory() as directory:
            args = type("Args", (), {"db": Path(directory) / "metrics" / "metrics.db", "project": "pulp",
                                      "shipyard": "shipyard", "since_days": 5, "timeout_seconds": 300})()
            original = MOD.run
            MOD.run = lambda *a, **k: subprocess.CompletedProcess(a[0], 0, json.dumps({"job_samples": 4}), "")
            try:
                result = MOD.authoritative_scorecard(args)
            finally:
                MOD.run = original
            self.assertTrue(result["provenance_status"])
            self.assertEqual(result["collector_safe_merge_denominator"]["status"], "unavailable")
            self.assertEqual(result["strata"]["rerun"]["status"], "unavailable")

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
            state = MOD.update_state(path, "pulp", started, {"missing_strata": ["rerun"]}, [{"new_rows": 1}], started)
            self.assertEqual(state["weekday_days_observed"], 1)
            self.assertFalse(state["baseline_exit"])
            self.assertEqual(len(json.loads(path.read_text())["observations"]), 1)


class CollectorInvocationTests(unittest.TestCase):
    def test_collect_uses_selected_db_and_counts_only_new_rows(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); db = root / "metrics" / "metrics.db"; db.parent.mkdir()
            conn = __import__("sqlite3").connect(db); conn.execute("create table jobs(id integer)"); conn.execute("insert into jobs values (1)"); conn.commit(); conn.close()
            shipyard = root / "shipyard"; tartci = root / "tartci"
            shipyard.write_text("#!/bin/sh\nif [ \"$6\" = import ]; then echo '{\"imported\": 1}'; else echo '{\"job_samples\": 1}'; fi\n")
            tartci.write_text("#!/bin/sh\necho '{}'\n")
            shipyard.chmod(0o755); tartci.chmod(0o755)
            args = type("Args", (), {"db": db, "project": "pulp", "repo": "o/p", "shipyard": str(shipyard), "tartci": str(tartci), "limit": 1, "timeout_seconds": 5})()
            original = MOD.count_jobs
            try:
                events = MOD.import_organic(args)
            finally:
                MOD.count_jobs = original
            self.assertEqual(events[-1]["new_rows"], 0)



if __name__ == "__main__":
    unittest.main()
