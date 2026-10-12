#!/usr/bin/env python3
"""Fast source safety lints for changed Rust files."""
from __future__ import annotations
import re
import subprocess
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
    changed = changed_lines(path)
    test_depths: list[int] = []
    pending_test_module = False
    pending_test_fn = False
    depth = 0
    for number, line in enumerate(lines, 1):
        if CFG_TEST_IF.match(line):
            pending_test_module = True
        if re.search(r"#\[test\]", line):
            pending_test_fn = True
        if pending_test_module and re.search(r"\bmod\s+\w+\s*\{", line):
            test_depths.append(depth + line.count("{") - line.count("}"))
            pending_test_module = False
        elif pending_test_fn and re.search(r"\bfn\s+\w+\s*[^\{]*\{", line):
            test_depths.append(depth + line.count("{") - line.count("}"))
            pending_test_fn = False
        in_tests = bool(test_depths)
        if number in changed and in_tests and INCLUDE_RS.search(line):
            failures.append(f"{path}:{number}: test code must not include Rust source")
        if number in changed and SHELL in line and in_tests:
            failures.append(f"{path}:{number}: /bin/sh is forbidden inside test code")
        if number in changed and CFG_TEST_IF.match(line) and number < len(lines):
            following = lines[number:number + 2]
            if following and IF_LINE.match(following[0]) and any(RETURN_LINE.search(item) for item in following):
                failures.append(f"{path}:{number}: cfg(test) if/return must not be embedded in production")
        depth += line.count("{") - line.count("}")
        test_depths[:] = [test_depth for test_depth in test_depths if depth >= test_depth]
    return failures


def changed_lines(path: Path) -> set[int]:
    base = getattr(changed_lines, "base", None)
    if not base:
        return set(range(1, len(path.read_text(encoding="utf-8").splitlines()) + 1))
    result = subprocess.run(
        ["git", "diff", "--unified=0", str(base), "--", str(path)],
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        # The base is unreadable, so no line can be shown unchanged.
        return set(range(1, len(path.read_text(encoding="utf-8").splitlines()) + 1))
    lines: set[int] = set()
    for hunk in re.finditer(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@", result.stdout, re.MULTILINE):
        start = int(hunk.group(1)); count = int(hunk.group(2) or "1")
        lines.update(range(start, start + count))
    # A deletion-only diff adds no lines, so it changed nothing to lint.
    return lines

def main(argv: list[str]) -> int:
    paths = [raw for raw in argv[1:] if raw != "--base" and not raw.startswith("--base=")]
    if "--base" in argv:
        changed_lines.base = argv[argv.index("--base") + 1]
    else:
        changed_lines.base = None
    failures = [failure for raw in paths for failure in lint(Path(raw))]
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    return 0

if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
