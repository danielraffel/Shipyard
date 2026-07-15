#!/usr/bin/env python3
"""Fixed guest-side runner for an immutable Shipyard review ISO.

The Proxmox guest agent invokes this file with no contributor-controlled
arguments. It validates the bundle manifest, safely extracts the source, and
runs only argv arrays from the protected recipe as the unprivileged `shipyard`
account. The final stdout line is bounded JSON for the trusted controller.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import time
import pwd
import grp

ISO_DEVICE = Path("/dev/disk/by-label/SHIPYARD_REVIEW")
MOUNT = Path("/run/shipyard-review/input")
WORK = Path("/var/tmp/shipyard-review-job")
RESULT = Path("/run/shipyard-review/result.json")
MAX_RECIPE_BYTES = 64 * 1024
MAX_LOG_BYTES = 1024 * 1024
MAX_FAILURE_TAIL_BYTES = 16 * 1024
MAX_COMMANDS = 32
MAX_COMMAND_SECONDS = 3600
ALLOWED_ENV = {"CC", "CXX", "CMAKE_BUILD_PARALLEL_LEVEL", "RUST_BACKTRACE"}


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def write_result(value: dict[str, object]) -> None:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":"))
    if len(encoded.encode()) > 256 * 1024:
        encoded = json.dumps({"status": "error", "reason": "result exceeded limit"})
    RESULT.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
    RESULT.write_text(encoded + "\n", encoding="utf-8")
    os.chmod(RESULT, 0o644)
    print(encoded, flush=True)


def validate_relative(path: str) -> Path:
    candidate = Path(path)
    if candidate.is_absolute() or ".." in candidate.parts:
        raise ValueError(f"unsafe relative path: {path!r}")
    return candidate


def safe_tar_filter(member: tarfile.TarInfo, destination: str) -> tarfile.TarInfo | None:
    """Drop links that can resolve outside the extracted source tree."""
    if member.issym() or member.islnk():
        link = Path(member.linkname)
        if link.is_absolute() or ".." in link.parts:
            return None
    return tarfile.data_filter(member, destination)


def main() -> int:
    started = time.time()
    if os.geteuid() != 0:
        raise RuntimeError("guest runner must be launched by qemu-guest-agent as root")
    if not Path("/run/shipyard-review/unprivileged-ready").is_file():
        raise RuntimeError("guest hardening admission marker is missing")

    shutil.rmtree(MOUNT, ignore_errors=True)
    shutil.rmtree(WORK, ignore_errors=True)
    MOUNT.mkdir(mode=0o755, parents=True)
    WORK.mkdir(mode=0o755, parents=True)
    subprocess.run(
        ["mount", "-o", "ro,nosuid,nodev,noexec", str(ISO_DEVICE), str(MOUNT)],
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )
    try:
        manifest_path = MOUNT / "manifest.json"
        recipe_path = MOUNT / "recipe.json"
        source_path = MOUNT / "source.tar.gz"
        if recipe_path.stat().st_size > MAX_RECIPE_BYTES:
            raise RuntimeError("recipe exceeds size limit")
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        if set(manifest) != {"schema", "source_sha256", "recipe_sha256", "request"}:
            raise RuntimeError("manifest has unexpected fields")
        if manifest["schema"] != 1:
            raise RuntimeError("unsupported manifest schema")
        if digest(source_path) != manifest["source_sha256"]:
            raise RuntimeError("source bundle digest mismatch")
        if digest(recipe_path) != manifest["recipe_sha256"]:
            raise RuntimeError("recipe digest mismatch")

        source_dir = WORK / "source"
        source_dir.mkdir(mode=0o700)
        with tarfile.open(source_path, "r:gz") as archive:
            archive.extractall(source_dir, filter=safe_tar_filter)
        roots = list(source_dir.iterdir())
        repo_dir = roots[0] if len(roots) == 1 and roots[0].is_dir() else source_dir
        uid = pwd.getpwnam("shipyard").pw_uid
        gid = grp.getgrnam("shipyard").gr_gid
        os.chown(WORK, uid, gid)
        for root, directories, files in os.walk(WORK):
            for name in directories + files:
                try:
                    os.chown(Path(root) / name, uid, gid, follow_symlinks=False)
                except FileNotFoundError:
                    pass

        recipe = json.loads(recipe_path.read_text(encoding="utf-8"))
        if set(recipe) != {"schema", "commands"} or recipe["schema"] != 1:
            raise RuntimeError("invalid protected recipe schema")
        commands = recipe["commands"]
        if not isinstance(commands, list) or not 1 <= len(commands) <= MAX_COMMANDS:
            raise RuntimeError("protected recipe command count is invalid")

        outcomes: list[dict[str, object]] = []
        overall = "pass"
        for index, command in enumerate(commands):
            if not isinstance(command, dict) or set(command) - {"argv", "cwd", "env", "timeout_seconds"}:
                raise RuntimeError(f"command {index} has unexpected fields")
            argv = command.get("argv")
            if not isinstance(argv, list) or not argv or not all(isinstance(v, str) and v for v in argv):
                raise RuntimeError(f"command {index} argv is invalid")
            if len(argv) > 64 or any(len(value.encode()) > 4096 for value in argv):
                raise RuntimeError(f"command {index} argv exceeds limit")
            cwd = repo_dir / validate_relative(command.get("cwd", "."))
            cwd = cwd.resolve()
            if not cwd.is_relative_to(repo_dir.resolve()) or not cwd.is_dir():
                raise RuntimeError(f"command {index} cwd escapes source")
            requested_env = command.get("env", {})
            if not isinstance(requested_env, dict) or set(requested_env) - ALLOWED_ENV:
                raise RuntimeError(f"command {index} environment is not allowlisted")
            if not all(isinstance(k, str) and isinstance(v, str) for k, v in requested_env.items()):
                raise RuntimeError(f"command {index} environment is invalid")
            timeout = command.get("timeout_seconds", 900)
            if not isinstance(timeout, int) or not 1 <= timeout <= MAX_COMMAND_SECONDS:
                raise RuntimeError(f"command {index} timeout is invalid")

            env = {
                "HOME": "/home/shipyard",
                "LANG": "C.UTF-8",
                "PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                **requested_env,
            }
            command_started = time.time()
            log_path = WORK / f"command-{index:02d}.log"
            try:
                with log_path.open("wb") as log:
                    completed = subprocess.run(
                        [
                            "prlimit",
                            f"--fsize={MAX_LOG_BYTES}",
                            "--nproc=256",
                            "--nofile=256",
                            f"--cpu={timeout + 5}",
                            "--",
                            "runuser", "--user", "shipyard", "--", *argv,
                        ],
                        cwd=cwd,
                        env=env,
                        stdin=subprocess.DEVNULL,
                        stdout=log,
                        stderr=subprocess.STDOUT,
                        timeout=timeout,
                        check=False,
                    )
                exit_code: int | None = completed.returncode
                status = "pass" if completed.returncode == 0 else "fail"
            except subprocess.TimeoutExpired as error:
                exit_code = None
                status = "timeout"
            if status != "pass":
                overall = "fail"
            log_size = log_path.stat().st_size if log_path.exists() else 0
            outcome: dict[str, object] = {
                "index": index,
                "argv": argv,
                "cwd": str(cwd.relative_to(repo_dir)),
                "status": status,
                "exit_code": exit_code,
                "duration_seconds": round(time.time() - command_started, 3),
                "log_sha256": digest(log_path),
                "log_bytes": log_size,
                "log_truncated": log_size >= MAX_LOG_BYTES,
            }
            if status != "pass" and log_path.exists():
                with log_path.open("rb") as log:
                    log.seek(max(0, log_size - MAX_FAILURE_TAIL_BYTES))
                    outcome["log_tail_untrusted"] = log.read(MAX_FAILURE_TAIL_BYTES).decode(errors="replace")
            outcomes.append(outcome)
            if status != "pass":
                break

        write_result({
            "schema": 1,
            "status": overall,
            "request": manifest["request"],
            "source_sha256": manifest["source_sha256"],
            "recipe_sha256": manifest["recipe_sha256"],
            "commands": outcomes,
            "duration_seconds": round(time.time() - started, 3),
            "standing_secrets": "none",
            "network": "none",
        })
        return 0 if overall == "pass" else 1
    finally:
        subprocess.run(["umount", str(MOUNT)], check=False, capture_output=True)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:  # Fail closed while returning inert structured data.
        write_result({"schema": 1, "status": "error", "reason": str(error)[:4096]})
        raise SystemExit(2)
