#!/usr/bin/env python3
"""Fast source safety lints for changed Rust files."""
from __future__ import annotations
import re
import sys
from pathlib import Path

INCLUDE_RS = re.compile(r"include_str!\s*\(\s*[\"'][^\"']+\.rs[\"']\s*\)")
SHELL = 'Command::new("/bin/sh")'
CFG_TEST_IF = re.compile(r"^\s*#\[cfg\(test\)\]\s*$")
IF_LINE = re.compile(r"^\s*if\b")
RETURN_LINE = re.compile(r"\breturn\b")

def lint(path: Path) -> list[str]:
    if not path.is_file() or path.suffix != ".rs" or not str(path).startswith("src/"):
        return []
    lines = path.read_text(encoding="utf-8").splitlines()
    failures: list[str] = []
    in_tests = False
    for number, line in enumerate(lines, 1):
        if "#[cfg(test)]" in line:
            in_tests = True
        if in_tests and INCLUDE_RS.search(line):
            failures.append(f"{path}:{number}: test code must not include Rust source")
        if SHELL in line and not any(CFG_TEST_IF.match(previous) for previous in lines[:number-1]):
            failures.append(f"{path}:{number}: /bin/sh from an ungated test/production path")
        if CFG_TEST_IF.match(line) and number < len(lines):
            following = lines[number:number + 2]
            if following and IF_LINE.match(following[0]) and any(RETURN_LINE.search(item) for item in following):
                failures.append(f"{path}:{number}: cfg(test) if/return must not be embedded in production")
    return failures

def main(argv: list[str]) -> int:
    failures = [failure for raw in argv[1:] for failure in lint(Path(raw))]
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    return 0

if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
