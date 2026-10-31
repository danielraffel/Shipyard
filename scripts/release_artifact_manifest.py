#!/usr/bin/env python3
"""Create and verify release artifact identity manifests.

The manifest binds release bytes to the tag commit and the GitHub Actions run
that produced them.  It is intentionally small and stdlib-only so the release
workflow can verify artifacts before it grants publication permissions.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
from typing import Any


SCHEMA_VERSION = 1


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _source(args: argparse.Namespace) -> dict[str, Any]:
    return {
        "repository": args.repository,
        "tag": args.tag,
        "commit": args.commit,
        "run_id": int(args.run_id),
        "run_attempt": int(args.run_attempt),
    }


def write_manifest(args: argparse.Namespace) -> None:
    root = args.root.resolve()
    if not root.is_dir():
        raise SystemExit(f"artifact root does not exist: {root}")
    files = []
    for path in sorted(root.iterdir()):
        if not path.is_file() or path.name == "release-manifest.json":
            continue
        files.append(
            {"name": path.name, "bytes": path.stat().st_size, "sha256": sha256(path)}
        )
    if not files:
        raise SystemExit(f"artifact root has no files: {root}")
    document = {
        "schema_version": SCHEMA_VERSION,
        "source": _source(args),
        "artifact": {
            "target": args.target,
            "artifact_id": args.artifact_id,
            "artifact_digest": args.artifact_digest,
            "files": files,
        },
    }
    if not args.artifact_id or not args.artifact_digest:
        raise SystemExit("artifact upload did not return an id and digest")
    args.output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def _load(path: Path) -> dict[str, Any]:
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SystemExit(f"invalid artifact manifest {path}: {error}") from error
    if not isinstance(document, dict) or document.get("schema_version") != SCHEMA_VERSION:
        raise SystemExit(f"unsupported artifact manifest schema: {path}")
    return document


def verify_manifest(
    path: Path,
    root: Path,
    *,
    source: dict[str, Any],
) -> dict[str, Any]:
    document = _load(path)
    actual_source = document.get("source")
    if not isinstance(actual_source, dict):
        raise SystemExit(f"artifact manifest has no source: {path}")
    comparable = {key: actual_source.get(key) for key in source if key != "run_attempt"}
    expected_comparable = {key: source.get(key) for key in source if key != "run_attempt"}
    attempt = actual_source.get("run_attempt")
    if comparable != expected_comparable or not isinstance(attempt, int) or not 1 <= attempt <= source["run_attempt"]:
        raise SystemExit(
            f"artifact manifest source mismatch in {path}: "
            f"expected repository/tag/commit/run {expected_comparable!r}, got {actual_source!r}"
        )
    artifact = document.get("artifact")
    if not isinstance(artifact, dict) or not artifact.get("target"):
        raise SystemExit(f"artifact manifest has no target: {path}")
    if not artifact.get("artifact_id") or not artifact.get("artifact_digest"):
        raise SystemExit(f"artifact manifest has no upload identity: {path}")
    files = artifact.get("files")
    if not isinstance(files, list) or not files:
        raise SystemExit(f"artifact manifest has no files: {path}")
    for entry in files:
        if not isinstance(entry, dict) or set(entry) != {"name", "bytes", "sha256"}:
            raise SystemExit(f"malformed file entry in {path}")
        file_path = root / str(entry["name"])
        if not file_path.is_file():
            raise SystemExit(f"manifest file is missing: {file_path}")
        if file_path.stat().st_size != entry["bytes"] or sha256(file_path) != entry["sha256"]:
            raise SystemExit(f"manifest digest mismatch: {file_path}")
    return document


def aggregate(args: argparse.Namespace) -> None:
    source = _source(args)
    records = []
    seen_targets: set[str] = set()
    for path in args.manifests:
        loaded = _load(path)
        target = loaded.get("artifact", {}).get("target")
        if not isinstance(target, str) or not target:
            raise SystemExit(f"artifact manifest has no target: {path}")
        artifact_base = args.artifacts_root / f"release-{target}"
        candidates = [artifact_base / args.tag, artifact_base]
        candidates.extend(candidate for candidate in artifact_base.rglob("*") if candidate.is_dir())
        artifact_root = next(
            (
                candidate
                for candidate in candidates
                if all(
                    (candidate / str(entry.get("name"))).is_file()
                    for entry in loaded.get("artifact", {}).get("files", [])
                    if isinstance(entry, dict)
                )
            ),
            None,
        )
        if artifact_root is None:
            raise SystemExit(f"could not locate downloaded artifact files for {target}")
        document = verify_manifest(
            path,
            artifact_root,
            source=source,
        )
        artifact = document["artifact"]
        if target in seen_targets:
            raise SystemExit(f"duplicate artifact target in manifests: {target}")
        seen_targets.add(target)
        records.append(artifact)
    if not records:
        raise SystemExit("no release artifact manifests found")
    args.output.write_text(
        json.dumps({"schema_version": SCHEMA_VERSION, "source": source, "artifacts": records}, indent=2, sort_keys=True)
        + "\n",
        encoding="utf-8",
    )


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    subparsers = parser.add_subparsers(dest="command", required=True)
    write = subparsers.add_parser("write")
    write.add_argument("--output", type=Path, required=True)
    write.add_argument("--root", type=Path, required=True)
    write.add_argument("--repository", required=True)
    write.add_argument("--tag", required=True)
    write.add_argument("--commit", required=True)
    write.add_argument("--run-id", required=True)
    write.add_argument("--run-attempt", required=True)
    write.add_argument("--target", required=True)
    write.add_argument("--artifact-id", required=True)
    write.add_argument("--artifact-digest", required=True)
    aggregate_parser = subparsers.add_parser("aggregate")
    aggregate_parser.add_argument("--output", type=Path, required=True)
    aggregate_parser.add_argument("--artifacts-root", type=Path, required=True)
    aggregate_parser.add_argument("--repository", required=True)
    aggregate_parser.add_argument("--tag", required=True)
    aggregate_parser.add_argument("--commit", required=True)
    aggregate_parser.add_argument("--run-id", required=True)
    aggregate_parser.add_argument("--run-attempt", required=True)
    aggregate_parser.add_argument("--manifest", dest="manifests", type=Path, action="append", required=True)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.command == "write":
        write_manifest(args)
    else:
        aggregate(args)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
