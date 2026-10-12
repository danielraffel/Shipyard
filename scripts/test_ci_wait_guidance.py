#!/usr/bin/env python3
from pathlib import Path

text = (Path(__file__).resolve().parents[1] / "skills/ci/SKILL.md").read_text()
assert "pgrep -f '[g]ates.sh'" in text
assert "give the wait a deadline" in text
assert "timed out waiting for gates.sh" in text
assert "shipyard wait ... --timeout <seconds>" in text
print("ci wait guidance: ok")
