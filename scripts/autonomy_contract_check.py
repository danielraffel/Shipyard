#!/usr/bin/env python3
"""Check that the autonomy contract names enforceable, testable obligations."""
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
CONTRACT = ROOT / "docs" / "autonomy-contract.md"
REQUIRED_IDS = [f"A{i}" for i in range(1, 13)]
REQUIRED_SURFACES = (
    ROOT / "src" / "queue_request.rs",
    ROOT / "src" / "ship_state.rs",
    ROOT / "src" / "recovery_worker.rs",
    ROOT / "scripts" / "test_autonomy_contract.py",
)
REQUIRED_FAILURES = (
    "original agent loss", "coordinator loss", "host reboot/offline",
    "dependency outage and return", "duplicate dispatch",
    "lost external acknowledgement", "stale worker mutation",
    "restart replay", "retry exhaustion",
    "authoritative post-remediation progress read",
)


def main() -> int:
    if not CONTRACT.is_file():
        raise SystemExit("missing docs/autonomy-contract.md")
    text = CONTRACT.read_text(encoding="utf-8")
    folded = re.sub(r"\s+", " ", text.lower())
    missing = [item for item in REQUIRED_IDS if not re.search(rf"\b{item}\b", text)]
    if missing:
        raise SystemExit(f"missing autonomy invariant(s): {', '.join(missing)}")
    for path in (ROOT / "AGENTS.md", ROOT / "CLAUDE.md"):
        if "docs/autonomy-contract.md" not in path.read_text(encoding="utf-8"):
            raise SystemExit(f"{path} does not reference docs/autonomy-contract.md")
    for surface in REQUIRED_SURFACES:
        if not surface.is_file():
            raise SystemExit(f"contract names missing enforcement surface: {surface.relative_to(ROOT)}")
    rows = [line for line in text.splitlines() if re.match(r"^\| A(?:[1-9]|1[0-2]) \|", line)]
    if len(rows) != len(REQUIRED_IDS):
        raise SystemExit(f"expected one evidence row per invariant, found {len(rows)}")
    for row in rows:
        columns = [column.strip() for column in row.strip().strip("|").split("|")]
        if len(columns) != 4 or any(not column for column in columns[1:]):
            raise SystemExit(f"incomplete evidence row: {row}")
    for phrase in REQUIRED_FAILURES + ("authoritative post-remediation progress", "controller restart", "duplicate effects"):
        if phrase not in folded:
            raise SystemExit(f"contract missing required recovery proof: {phrase}")
    print("autonomy contract: OK (A1-A12, enforcement surfaces, evidence rows, recovery proof requirements)")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
