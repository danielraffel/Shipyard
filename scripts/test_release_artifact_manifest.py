from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import release_artifact_manifest as manifest


class ReleaseArtifactManifestTests(unittest.TestCase):
    def _write(self, root: Path, output: Path, *, attempt: int = 1) -> None:
        (root / "shipyard-linux-x64").write_bytes(b"binary")
        args = manifest.parse_args(
            [
                "write",
                "--output",
                str(output),
                "--root",
                str(root),
                "--repository",
                "danielraffel/Shipyard",
                "--tag",
                "v1.0.0",
                "--commit",
                "abc",
                "--run-id",
                "42",
                "--run-attempt",
                str(attempt),
                "--target",
                "linux-x64",
                "--artifact-id",
                "7",
                "--artifact-digest",
                "d" * 64,
            ]
        )
        manifest.write_manifest(args)

    def test_write_and_aggregate_verify_file_digest_and_source(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            package_root = root / "artifacts" / "release-linux-x64" / "v1.0.0"
            package_root.mkdir(parents=True)
            first = root / "first.json"
            self._write(package_root, first)
            output = root / "aggregate.json"
            args = manifest.parse_args(
                [
                    "aggregate",
                    "--output",
                    str(output),
                    "--artifacts-root",
                    str(root / "artifacts"),
                    "--repository",
                    "danielraffel/Shipyard",
                    "--tag",
                    "v1.0.0",
                    "--commit",
                    "abc",
                    "--run-id",
                    "42",
                    "--run-attempt",
                    "3",
                    "--manifest",
                    str(first),
                ]
            )
            manifest.aggregate(args)
            document = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(document["source"]["run_attempt"], 3)
            self.assertEqual(document["artifacts"][0]["target"], "linux-x64")

    def test_aggregate_refuses_tampered_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            package_root = root / "artifacts" / "release-linux-x64" / "v1.0.0"
            package_root.mkdir(parents=True)
            first = root / "first.json"
            self._write(package_root, first)
            (package_root / "shipyard-linux-x64").write_bytes(b"tampered")
            args = manifest.parse_args(
                [
                    "aggregate",
                    "--output",
                    str(root / "aggregate.json"),
                    "--artifacts-root",
                    str(root / "artifacts"),
                    "--repository",
                    "danielraffel/Shipyard",
                    "--tag",
                    "v1.0.0",
                    "--commit",
                    "abc",
                    "--run-id",
                    "42",
                    "--run-attempt",
                    "1",
                    "--manifest",
                    str(first),
                ]
            )
            with self.assertRaises(SystemExit):
                manifest.aggregate(args)

    def test_aggregate_accepts_action_uploads_flattened_at_artifact_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            package_root = root / "artifacts" / "release-linux-x64"
            package_root.mkdir(parents=True)
            first = root / "first.json"
            self._write(package_root, first)
            args = manifest.parse_args(
                [
                    "aggregate",
                    "--output",
                    str(root / "aggregate.json"),
                    "--artifacts-root",
                    str(root / "artifacts"),
                    "--repository",
                    "danielraffel/Shipyard",
                    "--tag",
                    "v1.0.0",
                    "--commit",
                    "abc",
                    "--run-id",
                    "42",
                    "--run-attempt",
                    "1",
                    "--manifest",
                    str(first),
                ]
            )
            manifest.aggregate(args)


if __name__ == "__main__":
    unittest.main()
