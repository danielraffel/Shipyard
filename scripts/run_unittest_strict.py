#!/usr/bin/env python3
"""Run unittest discovery and pass only when the run provably finished.

`python3 -m unittest discover` exits 0 whenever its process ends with status
0, including when a test (or something it leaves behind, like a pending
alarm whose handler calls os._exit(0)) ends the process mid-suite: no
summary is printed, the remaining tests never run, and earlier failures are
lost. This runs the same discovery and passes only when the exit status is 0
AND the output carries unittest's own "Ran N tests" line followed by an "OK"
status line.

    run_unittest_strict.py [-s DIR] [-p PATTERN]
"""
import argparse
import re
import subprocess
import sys

RAN = re.compile(r"^Ran (\d+) tests? in ", re.M)
OK = re.compile(r"^OK(?: \(.*\))?$", re.M)


def verdict(returncode: int, output: str) -> tuple[bool, str]:
    ran = RAN.findall(output)
    if returncode != 0:
        return False, f"unittest exited {returncode}"
    if not ran:
        return False, "no 'Ran N tests' line: the run ended before unittest summarised it"
    tail = output[output.rfind("\nRan "):]
    if not OK.search(tail):
        return False, "the summary does not end in OK"
    return True, f"ran {ran[-1]} tests, OK"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("-s", "--start", default="scripts")
    parser.add_argument("-p", "--pattern", default="test_*.py")
    args = parser.parse_args(argv)
    proc = subprocess.Popen(
        [sys.executable, "-m", "unittest", "discover", "-s", args.start, "-p", args.pattern],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    chunks = []
    assert proc.stdout is not None
    for line in proc.stdout:
        sys.stdout.write(line)
        chunks.append(line)
    passed, why = verdict(proc.wait(), "".join(chunks))
    print(f"run_unittest_strict: {'PASS' if passed else 'FAIL'}: {why}", flush=True)
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
