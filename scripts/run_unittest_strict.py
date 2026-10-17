#!/usr/bin/env python3
"""Run unittest discovery and pass only when the run provably finished.

`python3 -m unittest discover` exits 0 whenever its process ends with status
0, including when a test (or something it leaves behind, like a pending
alarm whose handler calls os._exit(0)) ends the process mid-suite: no
summary is printed, the remaining tests never run, and earlier failures are
lost. This runs the same discovery and passes only when the exit status is 0
AND the last "Ran N tests" line in the output counts exactly the tests
discovery found AND is followed by an "OK" status line.

The count matters because a test may run a child unittest whose own
"Ran 1 test ... OK" lands in the same output; if the suite then ends with
status 0, that child summary would otherwise read as the suite's. The count
is taken in a separate process, so importing the test modules to count them
cannot affect the run.

    run_unittest_strict.py [-s DIR] [-p PATTERN]
"""
import argparse
import re
import subprocess
import sys

RAN = re.compile(r"^Ran (\d+) tests? in ", re.M)
OK = re.compile(r"^OK(?: \(.*\))?$", re.M)


COUNT = (
    "import sys, unittest\n"
    "suite = unittest.defaultTestLoader.discover(sys.argv[1], pattern=sys.argv[2])\n"
    "print(f'discovered {suite.countTestCases()}')\n"
)


def discovered(start: str, pattern: str) -> int:
    """How many tests `unittest discover` will run, counted in its own process."""
    proc = subprocess.run([sys.executable, "-c", COUNT, start, pattern],
                          capture_output=True, text=True, check=False)
    found = re.findall(r"^discovered (\d+)$", proc.stdout, re.M)
    if proc.returncode != 0 or not found:
        raise SystemExit(f"run_unittest_strict: FAIL: could not count the discovered tests "
                         f"(exit {proc.returncode}): {proc.stderr.strip()[-500:]}")
    return int(found[-1])


def verdict(returncode: int, output: str, expected: int) -> tuple[bool, str]:
    ran = RAN.findall(output)
    if returncode != 0:
        return False, f"unittest exited {returncode}"
    if not ran:
        return False, "no 'Ran N tests' line: the run ended before unittest summarised it"
    if int(ran[-1]) != expected:
        return False, (f"the last summary ran {ran[-1]} tests but discovery found {expected}: "
                       "the run ended early and the summary is not the suite's")
    tail = output[output.rfind("\nRan "):]
    if not OK.search(tail):
        return False, "the summary does not end in OK"
    return True, f"ran {ran[-1]} tests, OK"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("-s", "--start", default="scripts")
    parser.add_argument("-p", "--pattern", default="test_*.py")
    args = parser.parse_args(argv)
    expected = discovered(args.start, args.pattern)
    proc = subprocess.Popen(
        [sys.executable, "-m", "unittest", "discover", "-s", args.start, "-p", args.pattern],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    chunks = []
    assert proc.stdout is not None
    for line in proc.stdout:
        sys.stdout.write(line)
        chunks.append(line)
    passed, why = verdict(proc.wait(), "".join(chunks), expected)
    print(f"run_unittest_strict: {'PASS' if passed else 'FAIL'}: {why}", flush=True)
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
