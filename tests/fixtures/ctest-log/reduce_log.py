#!/usr/bin/env python3
"""Reduce a captured GitHub Actions job log to the lines CTest readers use.

usage: reduce_log.py <captured.log> <reduced.log> [--cap N] [--keep-test NAME]...

Kept, unchanged and in order: every `##[group]Run` step header, every
`##[notice]` line, every "The following tests FAILED" block (heading, entries
and the `Errors while running CTest` line), and these CTest result lines:
every one that did not pass, every `--repeat` retry (no `k/N` prefix), every
one naming a --keep-test, and the first N (default 120) of each step section.
Dropping the other passing results keeps a full suite of 22,000 tests run as
evidence from making a megabyte fixture; the section header stays, so step
mapping is unchanged.
"""
import argparse
import re

KEEP = re.compile(
    r"##\[group\]Run |##\[notice\]|The following tests FAILED|Errors while running CTest"
    r"|^\S+Z\s+\d+ - "
)
RESULT = re.compile(r"^\S+Z\s+(\d+/\d+\s+)?Test\s+#\d+: (.+) \.{3,}\s*(?:\*{3})?(\S.*?)\s+[\d.]+ sec\s*$")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("source")
    parser.add_argument("target")
    parser.add_argument("--cap", type=int, default=120)
    parser.add_argument("--keep-test", action="append", default=[])
    args = parser.parse_args()
    keep_tests = set(args.keep_test)
    kept = []
    in_section = 0
    with open(args.source, encoding="utf-8", errors="surrogateescape", newline="") as handle:
        for line in handle.read().splitlines(keepends=True):
            if "##[group]Run " in line:
                in_section = 0
            result = RESULT.match(line.rstrip("\r\n"))
            if result:
                in_section += 1
                retry = result.group(1) is None
                passed = result.group(3) == "Passed"
                named = result.group(2).strip() in keep_tests
                if retry or not passed or named or in_section <= args.cap:
                    kept.append(line)
            elif KEEP.search(line):
                kept.append(line)
    with open(args.target, "w", encoding="utf-8", errors="surrogateescape", newline="") as handle:
        handle.writelines(kept)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
