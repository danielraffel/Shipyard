import datetime as dt
import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import selection_recall as sr  # noqa: E402

D = dt.date
WF = ".github/workflows/build.yml"


def sel(total=1000, selected=30, select_all=False, reason="graph", failed=0, outside=0, names=(), title="affected-tests-shadow"):
    return {"title": title, "message": json.dumps({
        "total": total, "selected": selected, "select_all": select_all, "reason": reason, "failed": failed,
        "failed_outside_selection": outside, "failed_outside_names": list(names)})}


def rcpt(selected=1000, would_skip=10, failed=0, names=()):
    return {"title": "test-receipts-shadow", "message": json.dumps({
        "selected": selected, "would_skip": would_skip, "would_skip_failed": failed,
        "would_skip_failed_names": list(names)})}


def exo(tests):
    return {"title": "flake-exoneration", "message": json.dumps({"tests": tests})}


def gate(i, conclusion="success"):
    return {"job_id": i, "run_id": 1000 + i, "head_sha": f"s{i}", "conclusion": conclusion}


def report(gates_and_ann, partial=()):
    data = {"gate": [g for g, _ in gates_and_ann], "annotations": {g["job_id"]: a for g, a in gates_and_ann},
            "partial": list(partial), "skipped": {}, "reads": 0, "errors": 0}
    return sr.compute(data, "acme/repo", D(2026, 11, 11), D(2026, 11, 17), WF, "macos")


class Calc(unittest.TestCase):
    def test_select_all_share_and_reasons_are_ranked_with_counts_in_the_reason_folded(self):
        rows = [(gate(1), [sel(select_all=True, selected=1000, reason="cmake changed")]),
                (gate(2), [sel(select_all=True, selected=1000, reason="cmake changed")]),
                (gate(3), [sel(select_all=True, selected=1000, reason="1 changed file(s) no edge reads")]),
                (gate(4), [sel(select_all=True, selected=1000, reason="2 changed file(s) no edge reads")]),
                (gate(5), [sel(select_all=True, selected=1000, reason="2 changed file(s) no edge reads")]),
                (gate(6), [sel()])]
        r = report(rows)
        self.assertEqual((r["select_all"]["jobs"], r["narrowed"]["jobs"]), (5, 1))
        self.assertAlmostEqual(r["select_all"]["share"], 5 / 6, places=3)
        self.assertEqual([(x["reason"], x["jobs"]) for x in r["select_all"]["reasons"]],
                         [("N changed file(s) no edge reads", 3), ("cmake changed", 2)])

    def test_selected_fraction_median_and_p90_are_over_narrowed_jobs_only(self):
        rows = [(gate(i), [sel(selected=s)]) for i, s in enumerate([10, 20, 30, 40, 100], 1)]
        rows.append((gate(9), [sel(select_all=True, selected=1000, reason="cmake changed")]))
        n = report(rows)["narrowed"]
        self.assertEqual(n["selected_fraction_median_pct"], 3.0)
        self.assertEqual(n["selected_fraction_p90_pct"], 7.6)

    def test_misses_count_failed_outside_selection_in_narrowed_jobs_and_name_the_verdict(self):
        rows = [(gate(1, "failure"), [sel(failed=3, outside=2, names=["gpu a", "timing b"]),
                                      exo({"gpu a": {"excused": True, "reason": "fails on other heads"},
                                           "timing b": {"excused": False, "reason": "main evidence unread"}})]),
                (gate(2, "failure"), [sel(failed=1, outside=1, names=["c"])]),
                (gate(3), [sel()]), (gate(4), [sel()])]
        n = report(rows)["narrowed"]
        self.assertEqual((n["failing_tests"], n["misses"], n["misses_per_100_narrowed"]), (4, 3, 75.0))
        self.assertEqual((n["exonerated_misses"], n["unexplained_misses"], n["unexplained_per_100_narrowed"]), (1, 2, 50.0))
        self.assertEqual([(m["job_id"], m["test"], m["verdict"]) for m in n["miss_list"]],
                         [(1, "gpu a", "exonerated"), (1, "timing b", "not excused: main evidence unread"),
                          (2, "c", "no verdict")])

    def test_the_shadow_exoneration_title_with_would_exonerate_is_read_too(self):
        shadow = {"title": "flake-exoneration-shadow", "message": json.dumps({"tests": {"t": {"would_exonerate": True}}})}
        n = report([(gate(1, "failure"), [sel(failed=1, outside=1, names=["t"]), shadow]), (gate(2), [sel()])])["narrowed"]
        self.assertEqual((n["miss_list"][0]["verdict"], n["unexplained_misses"]), ("exonerated", 0))

    def test_failures_in_select_all_jobs_are_counted_overall_but_never_as_misses(self):
        rows = [(gate(1, "failure"), [sel(select_all=True, selected=1000, reason="cmake changed", failed=2)]),
                (gate(2), [sel()])]
        r = report(rows)
        self.assertEqual((r["failures"]["jobs_with_failing_test"], r["failures"]["failing_tests"]), (1, 2))
        self.assertEqual((r["narrowed"]["failing_tests"], r["narrowed"]["misses"]), (0, 0))

    def test_receipts_would_skip_failed_and_median_share(self):
        rows = [(gate(1), [sel(), rcpt(1000, 10)]), (gate(2), [sel(), rcpt(1000, 30, failed=1, names=["slow x"])]),
                (gate(3), [sel(), rcpt(1000, 20)])]
        c = report(rows)["receipts"]
        self.assertEqual((c["jobs"], c["would_skip_median_pct"], c["would_skip_failed"], c["would_skip_failed_names"]),
                         (3, 2.0, 1, ["slow x"]))

    def test_the_shipyard_schema_title_is_read_like_the_pulp_one(self):
        r = report([(gate(1), [sel(title="shipyard-selection-shadow", failed=1, outside=1, names=["t"])])])
        self.assertEqual((r["jobs_read"], r["narrowed"]["misses"]), (1, 1))

    def test_a_missing_or_unparseable_selection_annotation_is_unread_and_partial(self):
        rows = [(gate(1), [sel()]), (gate(2), []), (gate(3), [{"title": "affected-tests-shadow", "message": "not json"}]),
                (gate(4), [{"title": "affected-tests-shadow", "message": json.dumps({"total": 5})}]),
                (gate(5), None)]
        r = report(rows)
        self.assertEqual((r["gate_jobs"], r["jobs_read"], r["jobs_unread"]), (5, 1, 4))
        self.assertTrue(r["partial"])
        self.assertEqual(r["narrowed"]["jobs"], 1)
        self.assertEqual(sorted(u["job_id"] for u in r["unread_jobs"]), [2, 3, 4, 5])
        self.assertIn("Unread gate jobs", sr.render_md(r))

    def test_malformed_selection_values_are_unread_not_computed(self):
        def raw(**kw):
            m = {"total": 100, "selected": 5, "select_all": False, "reason": "graph", "failed": 1,
                 "failed_outside_selection": 1, "failed_outside_names": ["t"]}
            m.update(kw)
            return [{"title": "affected-tests-shadow", "message": json.dumps(m)}]
        bad = [raw(select_all="false"), raw(select_all=0), raw(total="100"), raw(selected=True), raw(selected=-1),
               raw(selected=500), raw(failed=0, failed_outside_selection=1), raw(total=1.5),
               raw(failed_outside_names="t"), raw(failed_outside_names=[1])]
        r = report([(gate(1), raw())] + [(gate(i + 2), a) for i, a in enumerate(bad)])
        self.assertEqual((r["jobs_read"], r["jobs_unread"], r["narrowed"]["jobs"]), (1, len(bad), 1))
        self.assertTrue(r["partial"])
        self.assertTrue(all(u["why"] == "selection annotation invalid" for u in r["unread_jobs"]))

    def test_a_malformed_receipts_notice_leaves_the_job_unread(self):
        bad = {"title": "test-receipts-shadow", "message": json.dumps({"selected": 10, "would_skip": 50})}
        r = report([(gate(1), [sel(), bad]), (gate(2), [sel()])])
        self.assertEqual((r["jobs_read"], r["jobs_unread"]), (1, 1))

    def test_under_95_percent_coverage_prints_partial_with_the_share(self):
        rows = [(gate(i), [sel()]) for i in range(1, 19)] + [(gate(30), []), (gate(31), [])]
        r = report(rows)
        self.assertEqual(r["coverage"], 0.9)
        self.assertTrue(any("floor 95%" in x for x in r["partial_reasons"]))
        self.assertIn("PARTIAL", sr.render_md(r))

    def test_one_unread_job_in_a_hundred_is_still_partial(self):
        r = report([(gate(i), [sel()]) for i in range(1, 100)] + [(gate(200), [])])
        self.assertEqual(r["coverage"], 0.99)
        self.assertTrue(r["partial"])
        self.assertTrue(any("1 of 100 gate jobs unread" in x for x in r["partial_reasons"]))

    def test_a_fully_read_window_is_not_partial(self):
        r = report([(gate(i), [sel()]) for i in range(1, 6)])
        self.assertFalse(r["partial"])
        self.assertNotIn("PARTIAL", sr.render_md(r))


class Fake:
    def __init__(self, runs, jobs, annotations):
        self.runs, self.jobs, self.annotations, self.paths = runs, jobs, annotations, []

    def __call__(self, path):
        self.paths.append(path)
        if "/workflows/" in path:
            day = path.split("created=")[1][:10]
            runs = [r for r in self.runs if r["created_at"][:10] == day]
            return {"total_count": len(runs), "workflow_runs": runs}
        if "/annotations" in path:
            return self.annotations[int(path.split("check-runs/")[1].split("/")[0])]
        rid = int(path.split("/runs/")[1].split("/")[0])
        return {"total_count": len(self.jobs[rid]), "jobs": self.jobs[rid]}


def mrun(i, status="completed", attempt=1, day="2026-11-12"):
    return {"id": i, "name": "Build", "path": WF, "event": "merge_group", "status": status, "conclusion": "success",
            "head_sha": f"s{i}", "created_at": f"{day}T10:00:00Z", "run_attempt": attempt}


def mjob(i, name="macos", labels=("self-hosted", "macOS", "pulp-build"), status="completed", conclusion="success"):
    return {"id": i, "name": name, "status": status, "conclusion": conclusion, "labels": list(labels),
            "runner_name": "r", "run_attempt": 1}


class Collect(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.runs = [mrun(1), mrun(2), mrun(3), mrun(4, status="in_progress")]
        self.jobs = {
            1: [mjob(11), mjob(12, name="classify", labels=("ubuntu-latest",))],
            2: [mjob(21, labels=("ubuntu-latest",))],          # the hosted bootstrap stub of the same name
            3: [mjob(31, conclusion="cancelled"), mjob(32, conclusion="failure")],
            4: [mjob(41, status="in_progress", conclusion=None)]}
        self.ann = {11: [sel(), rcpt()], 32: [sel(failed=1, outside=1, names=["t"]), exo({"t": {"excused": False, "reason": "r"}})]}

    def go(self, fake, cap=1000, **kw):
        out = Path(self.tmp) / "o.json"
        args = ["--repo", "acme/repo", "--since", "2026-11-12", "--until", "2026-11-12", "--cache", self.tmp,
                "--read-cap", str(cap), "--json-out", str(out), "--workers", "1", "--quiet"]
        sr.main(args + kw.get("extra", []), fetch=fake)
        return json.loads(out.read_text())

    def test_only_completed_gate_jobs_on_the_chosen_runner_class_are_read(self):
        r = self.go(Fake(self.runs, self.jobs, self.ann))
        self.assertEqual((r["gate_jobs"], r["jobs_read"]), (2, 2))
        self.assertEqual(r["excluded"], {"run_not_completed": 1, "not_gate_job": 1, "other_runner_class": 1,
                                         "cancelled_or_skipped": 1})
        self.assertFalse(r["partial"])
        self.assertEqual(r["narrowed"]["miss_list"][0]["verdict"], "not excused: r")

    def test_any_runner_class_counts_the_stub_too_and_it_is_unread(self):
        r = self.go(Fake(self.runs, self.jobs, {**self.ann, 21: []}), extra=["--runner-class", "any"])
        self.assertEqual((r["gate_jobs"], r["jobs_read"], r["jobs_unread"]), (3, 2, 1))
        self.assertTrue(r["partial"])

    def test_a_second_run_reads_no_jobs_or_annotations(self):
        self.go(Fake(self.runs, self.jobs, self.ann))
        f = Fake(self.runs, self.jobs, self.ann)
        self.go(f)
        self.assertEqual([p for p in f.paths if "/workflows/" not in p], [])

    def test_a_rerun_attempt_makes_the_cached_jobs_stale(self):
        self.go(Fake(self.runs, self.jobs, self.ann))
        runs = [dict(r, run_attempt=2) if r["id"] == 1 else r for r in self.runs]
        f = Fake(runs, self.jobs, self.ann)
        self.go(f)
        self.assertTrue(any("/runs/1/jobs" in p for p in f.paths))
        self.assertFalse(any("/runs/3/jobs" in p for p in f.paths))

    def test_the_read_cap_leaves_unread_jobs_partial_and_a_rerun_finishes_from_the_cache(self):
        r = self.go(Fake(self.runs, self.jobs, self.ann), cap=4)
        self.assertTrue(r["partial"])
        self.assertTrue(any("cap" in x for x in r["partial_reasons"]))
        r2 = self.go(Fake(self.runs, self.jobs, self.ann), cap=1000)
        self.assertFalse(r2["partial"])
        self.assertEqual(r2["jobs_read"], 2)


if __name__ == "__main__":
    unittest.main()
