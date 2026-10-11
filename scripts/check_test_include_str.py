#!/usr/bin/env python3
"""Reject test-only include_str! calls that embed Rust source files."""
from __future__ import annotations
import re
import sys
from pathlib import Path

PATTERN = re.compile(r"include_str!\s*\(\s*[\"'][^\"']+\.rs[\"']\s*\)")

def main(argv: list[str]) -> int:
    failures = []
    for raw in argv[1:]:
        path = Path(raw)
        if not path.is_file() or path.suffix != ".rs" or not str(path).startswith("src/"):
            continue
        in_tests = False
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if "#[cfg(test)]" in line:
                in_tests = True
            if in_tests and PATTERN.search(line):
                failures.append(f"{path}:{number}: test code must not include Rust source")
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    return 0

if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
