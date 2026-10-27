#!/usr/bin/env python3
"""Check that the permanent autonomy contract remains complete and referenced."""
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
CONTRACT = ROOT / "docs" / "autonomy-contract.md"
REQUIRED_IDS = [f"A{i}" for i in range(1, 13)]

def main() -> int:
    text = CONTRACT.read_text(encoding="utf-8")
    folded = text.lower()
    missing = [item for item in REQUIRED_IDS if not re.search(rf"\b{item}\b", text)]
    if missing:
        raise SystemExit(f"missing autonomy invariant(s): {', '.join(missing)}")
    for path in (ROOT / "AGENTS.md", ROOT / "CLAUDE.md"):
        if "docs/autonomy-contract.md" not in path.read_text(encoding="utf-8"):
            raise SystemExit(f"{path} does not reference docs/autonomy-contract.md")
    for phrase in ("authoritative post-remediation progress", "controller restart", "duplicate effects"):
        if phrase not in folded:
            raise SystemExit(f"contract missing required recovery proof: {phrase}")
    print("autonomy contract: OK (A1-A12, startup references, recovery proof requirements)")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
