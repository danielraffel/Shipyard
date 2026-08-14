#!/usr/bin/env python3
"""Build and package Shipyard release artifacts."""

from __future__ import annotations

import argparse
import hashlib
import os
import platform
import secrets
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_DIST_DIR = ROOT / "dist" / "release"
BIN_NAME = "shipyard"
NOTARY_WAIT_TIMEOUT = "45m"
SENSITIVE_FLAGS = {"--password", "-p"}
SENSITIVE_ENV_NAMES = (
    "SHIPYARD_NOTARIZE_APP_PASSWORD",
    "SHIPYARD_SIGNING_IDENTITY",
)


@dataclass(frozen=True)
class ReleaseTarget:
    name: str
    os: str
    arch: str
    exe_suffix: str = ""

    @property
    def is_macos(self) -> bool:
        return self.os == "macos"


TARGETS: dict[str, ReleaseTarget] = {
    "macos-arm64": ReleaseTarget("macos-arm64", "macos", "arm64"),
    "linux-x64": ReleaseTarget("linux-x64", "linux", "x64"),
    "linux-arm64": ReleaseTarget("linux-arm64", "linux", "arm64"),
    "windows-x64": ReleaseTarget("windows-x64", "windows", "x64", ".exe"),
}

SIGNING_ENV = (
    "SHIPYARD_SIGNING_IDENTITY",
    "SHIPYARD_NOTARIZE_APPLE_ID",
    "SHIPYARD_NOTARIZE_TEAM_ID",
    "SHIPYARD_NOTARIZE_APP_PASSWORD",
)


class CommandFailed(SystemExit):
    """Clean command failure that callers may handle without traceback noise."""


def redaction_values(extra_values: tuple[str, ...] = ()) -> tuple[str, ...]:
    values = [value for value in extra_values if value]
    values.extend(
        value
        for name in SENSITIVE_ENV_NAMES
        if (value := os.environ.get(name))
    )
    return tuple(sorted(set(values), key=len, reverse=True))


def redact_text(text: str, extra_values: tuple[str, ...] = ()) -> str:
    redacted = text
    for value in redaction_values(extra_values):
        redacted = redacted.replace(value, "<redacted>")
    return redacted


def redact_args(args: list[str], extra_values: tuple[str, ...] = ()) -> list[str]:
    redacted: list[str] = []
    redact_next = False
    for arg in args:
        if redact_next:
            redacted.append("<redacted>")
            redact_next = False
            continue
        redacted.append(redact_text(arg, extra_values))
        if arg in SENSITIVE_FLAGS:
            redact_next = True
    return redacted


def run(
    args: list[str],
    *,
    cwd: Path = ROOT,
    capture: bool = False,
    redact_values: tuple[str, ...] = (),
    env: dict[str, str] | None = None,
) -> str:
    result = subprocess.run(
        args,
        cwd=cwd,
        check=False,
        text=True,
        capture_output=capture,
        env=env,
    )
    if result.returncode != 0:
        detail = f"command failed ({result.returncode}): {' '.join(redact_args(args, redact_values))}"
        if capture and result.stderr:
            detail = f"{detail}\n{redact_text(result.stderr.strip(), redact_values)}"
        raise CommandFailed(detail)
    return result.stdout.strip() if capture else ""


def detect_host_target() -> str:
    system = platform.system().lower()
    machine = platform.machine().lower()
    if system == "darwin" and machine in {"arm64", "aarch64"}:
        return "macos-arm64"
    if system == "linux" and machine in {"x86_64", "amd64"}:
        return "linux-x64"
    if system == "linux" and machine in {"arm64", "aarch64"}:
        return "linux-arm64"
    if system == "windows" and machine in {"amd64", "x86_64"}:
        return "windows-x64"
    raise SystemExit(f"Unsupported host platform: {platform.system()} {platform.machine()}")


def artifact_filename(prefix: str, target: ReleaseTarget) -> str:
    return f"{prefix}-{target.name}{target.exe_suffix}"


def default_binary_path(target: ReleaseTarget, cargo_target: str | None) -> Path:
    release_dir = ROOT / "target"
    if cargo_target:
        release_dir = release_dir / cargo_target
    release_dir = release_dir / "release"
    return release_dir / f"{BIN_NAME}{target.exe_suffix}"


def require_commands(names: list[str]) -> None:
    missing = [name for name in names if shutil.which(name) is None]
    if missing:
        raise SystemExit(f"Missing required command(s): {', '.join(missing)}")


def require_signing_env(*, notarize: bool) -> None:
    required = SIGNING_ENV if notarize else ("SHIPYARD_SIGNING_IDENTITY",)
    missing = [name for name in required if not os.environ.get(name)]
    if missing:
        raise SystemExit(f"Missing required environment variable(s): {', '.join(missing)}")


def build_release(cargo_target: str | None) -> None:
    args = ["cargo", "build", "--release", "--locked", "--bin", BIN_NAME]
    if cargo_target:
        args.extend(["--target", cargo_target])
    run(args)


def smoke_binary(binary: Path) -> str:
    output = run([str(binary), "--version"], capture=True)
    if BIN_NAME not in output:
        raise SystemExit(f"Version smoke failed for {binary}: {output!r}")
    return output


def sign_binary(path: Path) -> None:
    identity = os.environ["SHIPYARD_SIGNING_IDENTITY"]
    keychain = os.environ.get("SHIPYARD_SIGNING_KEYCHAIN")
    keychain_args = ["--keychain", keychain] if keychain else []
    signing_env = os.environ.copy()
    if signing_home := os.environ.get("SHIPYARD_SIGNING_HOME"):
        signing_env["HOME"] = signing_home
    run(
        [
            "codesign",
            "--force",
            "--options",
            "runtime",
            "--timestamp",
            "--sign",
            identity,
            *keychain_args,
            str(path),
        ],
        env=signing_env,
    )


def create_dmg(stage_dir: Path, output_dmg: Path, *, volume_name: str) -> None:
    output_dmg.unlink(missing_ok=True)
    run(
        [
            "hdiutil",
            "create",
            "-volname",
            volume_name,
            "-srcfolder",
            str(stage_dir),
            "-ov",
            "-format",
            "UDZO",
            str(output_dmg),
        ]
    )


def sign_dmg(path: Path) -> None:
    identity = os.environ["SHIPYARD_SIGNING_IDENTITY"]
    keychain = os.environ.get("SHIPYARD_SIGNING_KEYCHAIN")
    keychain_args = ["--keychain", keychain] if keychain else []
    signing_env = os.environ.copy()
    if signing_home := os.environ.get("SHIPYARD_SIGNING_HOME"):
        signing_env["HOME"] = signing_home
    run(
        ["codesign", "--force", "--sign", identity, *keychain_args, str(path)],
        env=signing_env,
    )


def create_notary_keychain(temp_dir: Path) -> tuple[Path, str]:
    keychain = temp_dir / "notary.keychain-db"
    password = secrets.token_urlsafe(32)
    redactions = (password,)
    run(
        ["security", "create-keychain", "-p", password, str(keychain)],
        redact_values=redactions,
    )
    run(["security", "set-keychain-settings", "-lut", "21600", str(keychain)])
    run(
        ["security", "unlock-keychain", "-p", password, str(keychain)],
        redact_values=redactions,
    )
    return keychain, password


def delete_notary_keychain(keychain: Path) -> None:
    try:
        run(["security", "delete-keychain", str(keychain)])
    except CommandFailed:
        pass


def notarize_and_staple(path: Path) -> None:
    # Keep the long-running `notarytool submit --wait` process free of the
    # app-specific password. The password is used only to create a temporary
    # keychain profile, then submit waits with that profile.
    with tempfile.TemporaryDirectory(prefix="shipyard-notary-") as temp:
        keychain, _password = create_notary_keychain(Path(temp))
        profile = f"shipyard-notary-{os.getpid()}-{secrets.token_hex(4)}"
        try:
            run(
                [
                    "xcrun",
                    "notarytool",
                    "store-credentials",
                    profile,
                    "--apple-id",
                    os.environ["SHIPYARD_NOTARIZE_APPLE_ID"],
                    "--team-id",
                    os.environ["SHIPYARD_NOTARIZE_TEAM_ID"],
                    "--password",
                    os.environ["SHIPYARD_NOTARIZE_APP_PASSWORD"],
                    "--keychain",
                    str(keychain),
                ],
            )
            output = run(
                [
                    "xcrun",
                    "notarytool",
                    "submit",
                    str(path),
                    "--keychain-profile",
                    profile,
                    "--keychain",
                    str(keychain),
                    "--wait",
                    "--timeout",
                    NOTARY_WAIT_TIMEOUT,
                ],
                capture=True,
            )
        finally:
            delete_notary_keychain(keychain)
    if "status: Accepted" not in output:
        raise SystemExit(f"Notarization was not accepted:\n{output}")
    run(["xcrun", "stapler", "staple", str(path)])
    run(["xcrun", "stapler", "validate", str(path)])


def smoke_dmg(path: Path, binary_name: str, *, ci_mode: bool) -> str:
    require_commands(["hdiutil"])
    with tempfile.TemporaryDirectory(prefix="shipyard-dmg-") as temp:
        mount = Path(temp) / "mnt"
        mount.mkdir()
        try:
            run(
                [
                    "hdiutil",
                    "attach",
                    "-nobrowse",
                    "-readonly",
                    "-mountpoint",
                    str(mount),
                    str(path),
                ]
            )
        except CommandFailed as error:
            if ci_mode:
                return f"DMG mount skipped in CI mode: {error}"
            raise
        try:
            return smoke_binary(mount / binary_name)
        finally:
            subprocess.run(
                ["hdiutil", "detach", str(mount)],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                text=True,
            )


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_checksums(output_dir: Path, artifact: Path) -> Path:
    checksums = output_dir / "checksums.sha256"
    existing = []
    if checksums.exists():
        existing = [
            line
            for line in checksums.read_text(encoding="utf-8").splitlines()
            if not line.endswith(f"  {artifact.name}")
        ]
    existing.append(f"{sha256(artifact)}  {artifact.name}")
    checksums.write_text("\n".join(sorted(existing)) + "\n", encoding="utf-8")
    return checksums


def package(args: argparse.Namespace) -> list[Path]:
    target = TARGETS[args.target or detect_host_target()]
    if args.dmg and not target.is_macos:
        raise SystemExit("--dmg is only supported for macOS targets")
    if args.notarize:
        args.sign_macos = True
        args.dmg = True
    if args.sign_macos:
        if not target.is_macos:
            raise SystemExit("--sign-macos is only supported for macOS targets")
        require_commands(["codesign"])
        require_signing_env(notarize=args.notarize)
    if args.dmg:
        require_commands(["hdiutil"])
    if args.notarize:
        require_commands(["security", "xcrun"])

    if not args.skip_build:
        build_release(args.cargo_target)

    binary = args.binary or default_binary_path(target, args.cargo_target)
    if not binary.exists():
        raise SystemExit(f"Built binary not found: {binary}")
    smoke = smoke_binary(binary)

    tag = args.tag or "dev"
    output_dir = args.dist_dir / tag
    output_dir.mkdir(parents=True, exist_ok=True)

    artifact_base = artifact_filename(args.artifact_prefix, target)
    artifacts: list[Path] = []

    if args.dmg:
        with tempfile.TemporaryDirectory(prefix="shipyard-stage-") as temp:
            stage = Path(temp) / "stage"
            stage.mkdir()
            staged_binary = stage / args.artifact_prefix
            shutil.copy2(binary, staged_binary)
            if args.sign_macos:
                sign_binary(staged_binary)
            dmg = output_dir / f"{artifact_base}.dmg"
            create_dmg(stage, dmg, volume_name="Shipyard")
            if args.sign_macos:
                sign_dmg(dmg)
            if args.notarize:
                notarize_and_staple(dmg)
            if not args.no_smoke:
                smoke = smoke_dmg(dmg, args.artifact_prefix, ci_mode=args.ci_mode)
            artifacts.append(dmg)
    else:
        artifact = output_dir / artifact_base
        shutil.copy2(binary, artifact)
        if args.sign_macos:
            sign_binary(artifact)
        if not args.no_smoke:
            smoke = smoke_binary(artifact)
        artifacts.append(artifact)

    for artifact in artifacts:
        write_checksums(output_dir, artifact)

    print(f"packaged target={target.name} tag={tag}")
    print(f"smoke={smoke}")
    for artifact in artifacts:
        print(f"artifact={artifact}")
    print(f"checksums={output_dir / 'checksums.sha256'}")
    return artifacts


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=sorted(TARGETS), help="Release target; defaults to host")
    parser.add_argument("--cargo-target", help="Optional Rust target triple for cross builds")
    parser.add_argument("--binary", type=Path, help="Use an already-built binary")
    parser.add_argument("--skip-build", action="store_true", help="Do not run cargo build")
    parser.add_argument("--tag", help="Release tag label for output layout")
    parser.add_argument("--dist-dir", type=Path, default=DEFAULT_DIST_DIR)
    parser.add_argument("--artifact-prefix", default=BIN_NAME)
    parser.add_argument("--dmg", action="store_true", help="Package macOS target as a DMG")
    parser.add_argument("--sign-macos", action="store_true", help="Developer-ID sign macOS artifact")
    parser.add_argument("--notarize", action="store_true", help="Notarize and staple the macOS DMG")
    parser.add_argument("--ci-mode", action="store_true", help="Treat DMG mount failure as non-fatal")
    parser.add_argument("--no-smoke", action="store_true", help="Skip packaged artifact launch smoke")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    package(parse_args(argv or sys.argv[1:]))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
