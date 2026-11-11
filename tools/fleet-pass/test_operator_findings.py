"""Offline tests for operator-findings.py's steward-report readers."""
import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("of", os.path.join(HERE, "operator-findings.py"))
of = importlib.util.module_from_spec(spec)
spec.loader.exec_module(of)


class InfraOutages(unittest.TestCase):
    def test_prints_one_line_per_steward_finding(self):
        rep = {"infra_rerun": {"mode": "enforce", "findings": [{
            "message": "infra outage suspected: drift-fast failing on 9 PRs at step 'Install Linux dependencies'",
            "check": "drift-fast", "step": "Install Linux dependencies",
            "prs": [1, 2], "capped": True, "first_reported_at": "2026-11-11T14:40:00Z", "new": False}]}}
        lines = of.infra_outages("Generous-Corp/pulp", rep)
        self.assertEqual(len(lines), 1)
        self.assertIn("INFRA-OUTAGE-SUSPECTED infra outage suspected: drift-fast failing on 9 PRs", lines[0])
        self.assertIn("prs=#1,#2", lines[0])
        self.assertTrue(lines[0].endswith(" capped"))

    def test_nothing_without_findings(self):
        # Control: an older steward (no section), shadow with no findings, an unread repo.
        self.assertEqual(of.infra_outages("r", {}), [])
        self.assertEqual(of.infra_outages("r", {"infra_rerun": {"mode": "shadow"}}), [])
        self.assertEqual(of.infra_outages("r", None), [])


if __name__ == "__main__":
    unittest.main()
