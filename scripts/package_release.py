#!/usr/bin/env python3
"""Build and package Shipyard release artifacts."""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import os
import platform
import re
import secrets
import shlex
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_DIST_DIR = ROOT / "dist" / "release"
BIN_NAME = "shipyard"
COMPANION_BIN_NAME = "shipyard-workstream-provider"
SEMVER_RE = re.compile(
    r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$"
)
NOTARY_WAIT_TIMEOUT = "45m"
SENSITIVE_FLAGS = {"--password", "-p", "-P", "-k"}
SENSITIVE_ENV_NAMES = (
    "SHIPYARD_NOTARIZE_APP_PASSWORD",
    "SHIPYARD_SIGNING_IDENTITY",
    "SHIPYARD_SIGNING_P12_PASSWORD",
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

APPLE_ID_NOTARIZATION_ENV = (
    "SHIPYARD_NOTARIZE_APPLE_ID",
    "SHIPYARD_NOTARIZE_TEAM_ID",
    "SHIPYARD_NOTARIZE_APP_PASSWORD",
)
API_KEY_NOTARIZATION_ENV = (
    "SHIPYARD_NOTARIZE_KEY_PATH",
    "SHIPYARD_NOTARIZE_KEY_ID",
    "SHIPYARD_NOTARIZE_ISSUER_ID",
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


def default_binary_path(
    target: ReleaseTarget, cargo_target: str | None, binary_name: str = BIN_NAME
) -> Path:
    release_dir = ROOT / "target"
    if cargo_target:
        release_dir = release_dir / cargo_target
    release_dir = release_dir / "release"
    return release_dir / f"{binary_name}{target.exe_suffix}"


def require_commands(names: list[str]) -> None:
    missing = [name for name in names if shutil.which(name) is None]
    if missing:
        raise SystemExit(f"Missing required command(s): {', '.join(missing)}")


def require_signing_env(*, notarize: bool) -> None:
    missing = [] if os.environ.get("SHIPYARD_SIGNING_IDENTITY") else ["SHIPYARD_SIGNING_IDENTITY"]
    if notarize:
        try:
            notarization_mode()
        except SystemExit as error:
            if missing:
                detail = ", ".join(missing)
                raise SystemExit(
                    f"Missing required environment variable(s): {detail}; {error}"
                ) from error
            raise
    if missing:
        raise SystemExit(f"Missing required environment variable(s): {', '.join(missing)}")


def notarization_mode() -> str:
    apple_present = [name for name in APPLE_ID_NOTARIZATION_ENV if os.environ.get(name)]
    api_present = [name for name in API_KEY_NOTARIZATION_ENV if os.environ.get(name)]
    if len(api_present) == len(API_KEY_NOTARIZATION_ENV):
        return "api-key"
    if len(apple_present) == len(APPLE_ID_NOTARIZATION_ENV):
        return "apple-id"
    expected = API_KEY_NOTARIZATION_ENV if api_present else APPLE_ID_NOTARIZATION_ENV
    missing = [name for name in expected if not os.environ.get(name)]
    raise SystemExit(
        "Missing required notarization environment variable(s): " + ", ".join(missing)
    )


def expanded_env_path(name: str) -> Path:
    return Path(os.path.expandvars(os.path.expanduser(os.environ[name]))).resolve()


@contextlib.contextmanager
def signing_keychain_first():
    configured = os.environ.get("SHIPYARD_SIGNING_KEYCHAIN")
    if not configured:
        yield
        return

    keychain = str(Path(os.path.expandvars(os.path.expanduser(configured))).resolve())
    if not Path(keychain).is_file():
        raise SystemExit(f"Configured signing keychain does not exist: {keychain}")
    original = shlex.split(
        run(["security", "list-keychains", "-d", "user"], capture=True)
    )
    if not original:
        raise SystemExit(
            "User keychain search list read empty; refusing to replace it for signing"
        )
    desired = [keychain, *(item for item in original if str(Path(item).resolve()) != keychain)]
    run(["security", "list-keychains", "-d", "user", "-s", *desired])
    try:
        yield
    finally:
        run(["security", "list-keychains", "-d", "user", "-s", *original])


def signing_keychain_is_listed(keychain: Path) -> bool:
    """Return whether a keychain is still referenced by the user search list.

    This check deliberately fails closed.  A disposable keychain must remain on
    disk if Shipyard cannot prove the search list was restored; deleting it
    would leave a dangling search-list entry and make the next signing attempt
    less predictable.
    """
    listed = shlex.split(
        run(["security", "list-keychains", "-d", "user"], capture=True)
    )
    resolved = keychain.resolve()
    return any(Path(item).resolve() == resolved for item in listed)


def verify_signing_probe() -> None:
    with tempfile.TemporaryDirectory(prefix="shipyard-signing-probe-") as temp:
        root = Path(temp)
        source = root / "probe.c"
        binary = root / "probe"
        source.write_text("int main(void) { return 0; }\n", encoding="utf-8")
        run(["clang", str(source), "-o", str(binary)])
        sign_binary(binary)
        run(
            ["codesign", "--verify", "--strict", "--verbose=2", str(binary)],
            capture=True,
        )


def build_release(cargo_target: str | None) -> None:
    args = [
        "cargo",
        "build",
        "--release",
        "--locked",
        "--bin",
        BIN_NAME,
        "--bin",
        COMPANION_BIN_NAME,
    ]
    if cargo_target:
        args.extend(["--target", cargo_target])
    run(args)


def smoke_binary(binary: Path, expected_name: str = BIN_NAME) -> str:
    output = run([str(binary), "--version"], capture=True)
    parse_binary_version(output, expected_name, source=str(binary))
    return output


def parse_binary_version(output: str, expected_name: str, *, source: str) -> str:
    fields = output.split()
    if len(fields) != 2 or fields[0] != expected_name:
        raise SystemExit(f"Version smoke failed for {source}: {output!r}")
    version = fields[1].removeprefix("v")
    if not SEMVER_RE.fullmatch(version):
        raise SystemExit(f"Invalid semantic version from {source}: {output!r}")
    return version


def require_matching_pair_versions(
    primary_output: str, companion_output: str
) -> None:
    primary_version = parse_binary_version(
        primary_output, BIN_NAME, source=BIN_NAME
    )
    companion_version = parse_binary_version(
        companion_output, COMPANION_BIN_NAME, source=COMPANION_BIN_NAME
    )
    if primary_version != companion_version:
        raise SystemExit(
            "Release binary version mismatch: "
            f"{BIN_NAME}={primary_version} "
            f"{COMPANION_BIN_NAME}={companion_version}"
        )


def smoke_binary_pair(
    primary: Path,
    companion: Path,
    primary_name: str = BIN_NAME,
    companion_name: str = COMPANION_BIN_NAME,
) -> str:
    primary_output = smoke_binary(primary, primary_name)
    companion_output = smoke_binary(companion, companion_name)
    require_matching_pair_versions(primary_output, companion_output)
    return f"{primary_output}\n{companion_output}"


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
        capture=True,
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
        capture=True,
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


def create_disposable_signing_keychain(temp_dir: Path) -> tuple[Path, str]:
    keychain = temp_dir / "shipyard-signing.keychain-db"
    password = secrets.token_urlsafe(32)
    p12 = expanded_env_path("SHIPYARD_SIGNING_P12")
    if not p12.is_file():
        raise SystemExit(f"Configured signing certificate does not exist: {p12}")
    p12_password = os.environ["SHIPYARD_SIGNING_P12_PASSWORD"]
    redactions = (password, p12_password)
    run(
        ["security", "create-keychain", "-p", password, str(keychain)],
        redact_values=redactions,
    )
    run(["security", "set-keychain-settings", "-lut", "21600", str(keychain)])
    run(
        ["security", "unlock-keychain", "-p", password, str(keychain)],
        redact_values=redactions,
    )
    run(
        [
            "security",
            "import",
            str(p12),
            "-k",
            str(keychain),
            "-P",
            p12_password,
            "-T",
            "/usr/bin/codesign",
            "-T",
            "/usr/bin/security",
        ],
        redact_values=redactions,
        capture=True,
    )
    run(
        [
            "security",
            "set-key-partition-list",
            "-S",
            "apple-tool:,apple:,codesign:",
            "-s",
            "-k",
            password,
            str(keychain),
        ],
        redact_values=redactions,
        capture=True,
    )
    return keychain, password


@contextlib.contextmanager
def prepared_signing_keychain():
    p12 = os.environ.get("SHIPYARD_SIGNING_P12")
    p12_password = os.environ.get("SHIPYARD_SIGNING_P12_PASSWORD")
    if bool(p12) != bool(p12_password):
        missing = (
            "SHIPYARD_SIGNING_P12_PASSWORD"
            if p12
            else "SHIPYARD_SIGNING_P12"
        )
        raise SystemExit(f"Missing required environment variable: {missing}")
    prepared = (
        os.environ.get("CI") == "true"
        and os.environ.get("SHIPYARD_SIGNING_KEYCHAIN_READY") == "1"
    )
    if os.environ.get("SHIPYARD_SIGNING_KEYCHAIN") and not p12 and not prepared:
        raise SystemExit(
            "An explicit signing keychain requires SHIPYARD_SIGNING_P12 and "
            "SHIPYARD_SIGNING_P12_PASSWORD so Shipyard can prepare a disposable, "
            "noninteractive keychain before codesign"
        )
    if not p12:
        with signing_keychain_first():
            yield
        return

    temp = Path(tempfile.mkdtemp(prefix="shipyard-signing-keychain-"))
    keychain, _password = create_disposable_signing_keychain(temp)
    previous = os.environ.get("SHIPYARD_SIGNING_KEYCHAIN")
    os.environ["SHIPYARD_SIGNING_KEYCHAIN"] = str(keychain)
    try:
        with signing_keychain_first():
            yield
    finally:
        if previous is None:
            os.environ.pop("SHIPYARD_SIGNING_KEYCHAIN", None)
        else:
            os.environ["SHIPYARD_SIGNING_KEYCHAIN"] = previous

        # Never delete a disposable keychain while it may still be in the
        # user's search list.  A restore/query failure propagates and leaves
        # the keychain intact for deterministic manual recovery.
        if not signing_keychain_is_listed(keychain):
            delete_notary_keychain(keychain)
            shutil.rmtree(temp, ignore_errors=True)


def delete_notary_keychain(keychain: Path) -> None:
    try:
        run(["security", "delete-keychain", str(keychain)])
    except CommandFailed:
        pass


def notarize_and_staple(path: Path) -> None:
    if notarization_mode() == "api-key":
        output = run(
            [
                "xcrun",
                "notarytool",
                "submit",
                str(path),
                "--key",
                str(expanded_env_path("SHIPYARD_NOTARIZE_KEY_PATH")),
                "--key-id",
                os.environ["SHIPYARD_NOTARIZE_KEY_ID"],
                "--issuer",
                os.environ["SHIPYARD_NOTARIZE_ISSUER_ID"],
                "--wait",
                "--timeout",
                NOTARY_WAIT_TIMEOUT,
            ],
            capture=True,
        )
        if "status: Accepted" not in output:
            raise SystemExit(f"Notarization was not accepted:\n{output}")
        run(["xcrun", "stapler", "staple", str(path)])
        run(["xcrun", "stapler", "validate", str(path)])
        return

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


def smoke_dmg(path: Path, binary_names: tuple[str, ...], *, ci_mode: bool) -> str:
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
            if len(binary_names) != 2:
                raise SystemExit("DMG smoke requires the Shipyard binary pair")
            return smoke_binary_pair(
                mount / binary_names[0],
                mount / binary_names[1],
            )
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
        require_commands(["codesign", "clang"])
        require_signing_env(notarize=args.notarize)
    if args.dmg:
        require_commands(["hdiutil"])
    if args.notarize:
        require_commands(["security", "xcrun"])

    if not args.skip_build:
        build_release(args.cargo_target)

    binary = args.binary or default_binary_path(target, args.cargo_target)
    companion_binary = args.companion_binary or default_binary_path(
        target, args.cargo_target, COMPANION_BIN_NAME
    )
    if not binary.exists():
        raise SystemExit(f"Built binary not found: {binary}")
    if not companion_binary.exists():
        raise SystemExit(f"Built companion binary not found: {companion_binary}")
    smoke = smoke_binary_pair(binary, companion_binary)

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
            staged_companion = stage / args.companion_artifact_prefix
            shutil.copy2(binary, staged_binary)
            shutil.copy2(companion_binary, staged_companion)
            dmg = output_dir / f"{artifact_base}.dmg"
            if args.sign_macos:
                with prepared_signing_keychain():
                    verify_signing_probe()
                    sign_binary(staged_binary)
                    sign_binary(staged_companion)
                    create_dmg(stage, dmg, volume_name="Shipyard")
                    sign_dmg(dmg)
            else:
                create_dmg(stage, dmg, volume_name="Shipyard")
            if args.notarize:
                notarize_and_staple(dmg)
            if not args.no_smoke:
                smoke = smoke_dmg(
                    dmg,
                    (args.artifact_prefix, args.companion_artifact_prefix),
                    ci_mode=args.ci_mode,
                )
            artifacts.append(dmg)
    else:
        artifact = output_dir / artifact_base
        shutil.copy2(binary, artifact)
        if args.sign_macos:
            with prepared_signing_keychain():
                verify_signing_probe()
                sign_binary(artifact)
        if not args.no_smoke:
            primary_smoke = smoke_binary(artifact)
        artifacts.append(artifact)
        companion_artifact = output_dir / artifact_filename(
            args.companion_artifact_prefix, target
        )
        shutil.copy2(companion_binary, companion_artifact)
        if args.sign_macos:
            with prepared_signing_keychain():
                sign_binary(companion_artifact)
        if not args.no_smoke:
            companion_smoke = smoke_binary(
                companion_artifact, COMPANION_BIN_NAME
            )
            require_matching_pair_versions(primary_smoke, companion_smoke)
            smoke = f"{primary_smoke}\n{companion_smoke}"
        artifacts.append(companion_artifact)

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
    parser.add_argument(
        "--companion-binary", type=Path, help="Use an already-built provider binary"
    )
    parser.add_argument("--skip-build", action="store_true", help="Do not run cargo build")
    parser.add_argument("--tag", help="Release tag label for output layout")
    parser.add_argument("--dist-dir", type=Path, default=DEFAULT_DIST_DIR)
    parser.add_argument("--artifact-prefix", default=BIN_NAME)
    parser.add_argument("--companion-artifact-prefix", default=COMPANION_BIN_NAME)
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
