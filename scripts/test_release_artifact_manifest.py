from __future__ import annotations

import hashlib
import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stderr
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


    def _aggregate_args(self, root: Path, *manifests: Path) -> object:
        argv = [
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
        ]
        for path in manifests:
            argv.extend(["--manifest", str(path)])
        return manifest.parse_args(argv)

    def _target_manifest(self, root: Path, target: str, files: dict[str, bytes]) -> Path:
        package_root = root / "artifacts" / f"release-{target}" / "v1.0.0"
        package_root.mkdir(parents=True)
        for name, payload in files.items():
            (package_root / name).write_bytes(payload)
        path = root / f"{target}.json"
        path.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "source": {
                        "repository": "danielraffel/Shipyard",
                        "tag": "v1.0.0",
                        "commit": "abc",
                        "run_id": 42,
                        "run_attempt": 1,
                    },
                    "artifact": {
                        "target": target,
                        "artifact_id": target,
                        "artifact_digest": "d" * 64,
                        "files": [
                            {
                                "name": name,
                                "bytes": len(payload),
                                "sha256": hashlib.sha256(payload).hexdigest(),
                            }
                            for name, payload in sorted(files.items())
                        ],
                    },
                }
            ),
            encoding="utf-8",
        )
        return path

    def test_write_excludes_the_per_target_checksum_file(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "checksums.sha256").write_text("x  shipyard-linux-x64\n", encoding="utf-8")
            output = root.parent / f"{root.name}-manifest.json"
            try:
                self._write(root, output)
                files = json.loads(output.read_text(encoding="utf-8"))["artifact"]["files"]
            finally:
                output.unlink(missing_ok=True)
            self.assertEqual([entry["name"] for entry in files], ["shipyard-linux-x64"])

    def test_aggregate_lists_each_asset_once_across_targets(self) -> None:
        # The v0.383.0 shape: every per-target package carried its own
        # checksums.sha256, so concatenating targets repeated that name.
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            x64 = self._target_manifest(
                root, "linux-x64", {"shipyard-linux-x64": b"x64", "checksums.sha256": b"a  x\n"}
            )
            arm = self._target_manifest(
                root, "linux-arm64", {"shipyard-linux-arm64": b"arm", "checksums.sha256": b"b  y\n"}
            )
            stderr = io.StringIO()
            with redirect_stderr(stderr):
                manifest.aggregate(self._aggregate_args(root, x64, arm))
            document = json.loads((root / "aggregate.json").read_text(encoding="utf-8"))
            names = [entry["name"] for artifact in document["artifacts"] for entry in artifact["files"]]
            self.assertEqual(sorted(names), ["shipyard-linux-arm64", "shipyard-linux-x64"])
            self.assertIn("dropped release metadata entry checksums.sha256", stderr.getvalue())

    def test_identical_repeat_collapses_to_one_entry(self) -> None:
        entry = {"name": "shipyard-linux-x64", "bytes": 3, "sha256": "a" * 64}
        artifacts = [
            {"target": "linux-x64", "files": [dict(entry)]},
            {"target": "linux-x64-retry", "files": [dict(entry)]},
        ]
        notes = manifest.normalize_release_files(artifacts)
        names = [item["name"] for artifact in artifacts for item in artifact["files"]]
        self.assertEqual(names, ["shipyard-linux-x64"])
        self.assertEqual(len(notes), 1)

    def test_conflicting_repeat_names_every_entry(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            first = self._target_manifest(root, "linux-x64", {"shipyard-tool": b"one"})
            second = self._target_manifest(root, "linux-arm64", {"shipyard-tool": b"two!"})
            with self.assertRaises(SystemExit) as ctx:
                manifest.aggregate(self._aggregate_args(root, first, second))
            message = str(ctx.exception)
            self.assertIn("conflicting contents for the same asset", message)
            self.assertIn("shipyard-tool", message)
            self.assertIn("linux-x64: 3 bytes sha256=" + hashlib.sha256(b"one").hexdigest(), message)
            self.assertIn("linux-arm64: 4 bytes sha256=" + hashlib.sha256(b"two!").hexdigest(), message)


if __name__ == "__main__":
    unittest.main()
