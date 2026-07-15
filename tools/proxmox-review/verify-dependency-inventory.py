#!/usr/bin/env python3
"""Verify a baked, networkless dependency root against protected provenance."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys


class VerificationError(RuntimeError):
    pass


def git_output(path: Path, *arguments: str) -> bytes:
    completed = subprocess.run(
        ["git", "-c", f"safe.directory={path}", "-C", str(path), *arguments],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=30,
        check=False,
    )
    if completed.returncode != 0:
        raise VerificationError(
            f"git {' '.join(arguments)} failed for {path.name}: "
            + completed.stderr.decode(errors="replace")[-1000:]
        )
    return completed.stdout


def archive_sha256(path: Path) -> str:
    process = subprocess.Popen(
        ["git", "-c", f"safe.directory={path}", "-C", str(path), "archive", "--format=tar", "HEAD"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if process.stdout is None or process.stderr is None:
        process.kill()
        raise VerificationError(f"cannot read git archive for {path.name}")
    digest = hashlib.sha256()
    for chunk in iter(lambda: process.stdout.read(1024 * 1024), b""):
        digest.update(chunk)
    process.stdout.close()
    stderr = process.stderr.read(64 * 1024)
    process.stderr.close()
    returncode = process.wait(timeout=30)
    if returncode != 0:
        raise VerificationError(
            f"git archive failed for {path.name}: " + stderr.decode(errors="replace")[-1000:]
        )
    return digest.hexdigest()


def verify(inventory_path: Path, dependency_root: Path) -> None:
    value = json.loads(inventory_path.read_text(encoding="utf-8"))
    if not isinstance(value, dict) or value.get("schema") != 1:
        raise VerificationError("unsupported dependency inventory")
    policy = value.get("policy")
    if not isinstance(policy, dict) or policy != {
        "controller_fetch": "forbidden",
        "controller_cache_warming": "forbidden",
        "guest_network": "none",
        "missing_dependency": "fail-closed",
    }:
        raise VerificationError("dependency inventory policy is not fail-closed")
    sources = value.get("baked_sources")
    if not isinstance(sources, list) or not sources:
        raise VerificationError("dependency inventory has no baked sources")
    expected_paths: set[str] = set()
    for source in sources:
        if not isinstance(source, dict) or set(source) != {
            "name", "path", "commit", "git_archive_sha256",
        }:
            raise VerificationError("dependency source record has invalid schema")
        relative = str(source["path"])
        if Path(relative).name != relative or not relative.endswith("-src"):
            raise VerificationError(f"unsafe dependency path: {relative!r}")
        expected_paths.add(relative)
        path = dependency_root / relative
        if not path.is_dir() or path.is_symlink():
            raise VerificationError(f"baked dependency is missing or unsafe: {relative}")
        head = git_output(path, "rev-parse", "HEAD").decode().strip()
        if head != source["commit"]:
            raise VerificationError(f"baked dependency commit mismatch: {relative}")
        if archive_sha256(path) != source["git_archive_sha256"]:
            raise VerificationError(f"baked dependency content mismatch: {relative}")
    actual_paths = {path.name for path in dependency_root.glob("*-src") if path.is_dir()}
    if actual_paths != expected_paths:
        raise VerificationError(
            f"baked dependency set mismatch: expected={sorted(expected_paths)} actual={sorted(actual_paths)}"
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument("--dependency-root", type=Path, required=True)
    args = parser.parse_args()
    try:
        verify(args.inventory, args.dependency_root)
    except (OSError, ValueError, json.JSONDecodeError, VerificationError) as error:
        print(f"dependency inventory verification failed: {error}", file=sys.stderr)
        return 1
    print("dependency inventory verified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
