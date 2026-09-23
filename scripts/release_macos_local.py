#!/usr/bin/env python3
"""Build, sign, notarize, upload, and verify the macOS Shipyard release DMG."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

import package_release
import release_env


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_REPO = "danielraffel/Shipyard"
DEFAULT_LOCAL_ENV_FILES = (
    Path.home() / ".config" / "pulp" / "secrets" / "keychain.env",
    Path.home() / ".config" / "pulp" / "secrets" / "notary.env",
)
PUBLIC_ASSET_VISIBILITY_TIMEOUT_SECS = 90
PUBLIC_ASSET_VISIBILITY_POLL_SECS = 3
# Distinct from every earlier failure: the release is public and installable,
# but at least one fleet host did not verify at it.
FLEET_ROLLOUT_FAILED_EXIT = 6


@dataclass(frozen=True)
class ReleaseConfig:
    tag: str
    repo: str
    artifact_prefix: str
    dist_dir: Path
    upload: bool
    ci_mode: bool
    skip_build: bool
    binary: Path | None
    cargo_target: str | None
    companion_binary: Path | None = None
    rollback_tag: str | None = None
    install_sh: Path = ROOT / "install.sh"


class CommandRunner:
    def run(
        self,
        args: list[str],
        *,
        capture: bool = False,
        env: dict[str, str] | None = None,
        cwd: Path = ROOT,
    ) -> str:
        merged_env = os.environ.copy()
        if env:
            merged_env.update(env)
        result = subprocess.run(
            args,
            cwd=cwd,
            env=merged_env,
            check=False,
            text=True,
            capture_output=capture,
        )
        if result.returncode != 0:
            detail = f"command failed ({result.returncode}): {' '.join(args)}"
            if result.stderr:
                detail = f"{detail}\n{result.stderr.strip()}"
            raise SystemExit(detail)
        return result.stdout.strip() if capture else ""

    def run_status(self, args: list[str]) -> tuple[int, str, str]:
        """Run a command whose failure the caller reports itself."""
        result = subprocess.run(
            args, cwd=ROOT, check=False, text=True, capture_output=True
        )
        return result.returncode, result.stdout, result.stderr


def require_env() -> None:
    try:
        package_release.require_signing_env(notarize=True)
    except SystemExit as error:
        raise SystemExit(f"{error}\nsee scripts/release_macos_local.py --help") from error


def require_private_file(path: Path, *, label: str) -> None:
    try:
        mode = stat.S_IMODE(path.stat().st_mode)
    except OSError as error:
        raise SystemExit(f"could not access {label} {path}: {error}") from error
    if not path.is_file():
        raise SystemExit(f"{label} is not a file: {path}")
    if mode != 0o600:
        raise SystemExit(f"{label} must have mode 0600, found {mode:04o}: {path}")


def load_release_environment(env_files: list[Path]) -> None:
    environ = dict(os.environ)
    for env_file in env_files:
        require_private_file(env_file, label="release environment file")
        try:
            dotenv = release_env.parse_dotenv(env_file)
        except OSError as error:
            raise SystemExit(f"could not read --env-file {env_file}: {error}") from error
        environ, _sources = release_env.apply_dotenv_aliases(environ, dotenv)
    os.environ.update(
        {name: environ[name] for name in release_env.SHIPYARD_RELEASE_ENV if environ.get(name)}
    )


def resolve_environment_files(requested: list[Path]) -> list[Path]:
    if requested:
        return requested
    if all(path.is_file() for path in DEFAULT_LOCAL_ENV_FILES):
        return list(DEFAULT_LOCAL_ENV_FILES)
    return []


def check_unattended_auth() -> str:
    require_env()
    package_release.require_commands(["codesign", "clang", "security", "xcrun"])
    mode = package_release.notarization_mode()
    if mode == "api-key":
        require_private_file(
            package_release.expanded_env_path("SHIPYARD_NOTARIZE_KEY_PATH"),
            label="notarization API key",
        )
    if os.environ.get("SHIPYARD_SIGNING_P12"):
        require_private_file(
            package_release.expanded_env_path("SHIPYARD_SIGNING_P12"),
            label="signing certificate",
        )
    with package_release.prepared_signing_keychain():
        package_release.verify_signing_probe()
    return mode


def resolve_tag(tag: str | None, runner: CommandRunner) -> str:
    if tag:
        return tag
    try:
        discovered = runner.run(
            ["git", "describe", "--tags", "--exact-match"],
            capture=True,
        )
    except SystemExit:
        discovered = ""
    if not discovered:
        raise SystemExit(
            "--tag is required when the current commit is not an exact release tag"
        )
    return discovered


def require_arm64(arch: str) -> None:
    if arch != "arm64":
        raise SystemExit(
            "Intel Mac (x86_64) support is intentionally not produced. "
            "Use --arch arm64."
        )


def expected_release_assets(artifact_prefix: str) -> tuple[str, ...]:
    companion = package_release.COMPANION_BIN_NAME
    return (
        f"{artifact_prefix}-linux-x64",
        f"{companion}-linux-x64",
        f"{artifact_prefix}-linux-arm64",
        f"{companion}-linux-arm64",
        f"{artifact_prefix}-windows-x64.exe",
        f"{companion}-windows-x64.exe",
        f"{artifact_prefix}-macos-arm64.dmg",
        "checksums.sha256",
    )


def package_signed_dmg(config: ReleaseConfig) -> Path:
    args = [
        "--target",
        "macos-arm64",
        "--tag",
        config.tag,
        "--dist-dir",
        str(config.dist_dir),
        "--artifact-prefix",
        config.artifact_prefix,
        "--dmg",
        "--sign-macos",
        "--notarize",
    ]
    # Publication is never allowed to soften the mounted-DMG launch gate.
    if config.ci_mode and not config.upload:
        args.append("--ci-mode")
    if config.skip_build:
        args.append("--skip-build")
    if config.binary:
        args.extend(["--binary", str(config.binary)])
    if config.companion_binary:
        args.extend(["--companion-binary", str(config.companion_binary)])
    if config.cargo_target:
        args.extend(["--cargo-target", config.cargo_target])

    artifacts = package_release.package(package_release.parse_args(args))
    if len(artifacts) != 1 or artifacts[0].suffix != ".dmg":
        raise SystemExit(f"expected one DMG artifact, got: {artifacts}")
    return artifacts[0]


def release_asset_names(config: ReleaseConfig, runner: CommandRunner) -> list[str]:
    output = runner.run(
        [
            "gh",
            "release",
            "view",
            "--repo",
            config.repo,
            config.tag,
            "--json",
            "assets",
            "--jq",
            ".assets[].name",
        ],
        capture=True,
    )
    return [line.strip() for line in output.splitlines() if line.strip()]


def release_is_draft(config: ReleaseConfig, runner: CommandRunner) -> bool:
    output = runner.run(
        [
            "gh",
            "release",
            "view",
            "--repo",
            config.repo,
            config.tag,
            "--json",
            "isDraft",
            "--jq",
            ".isDraft",
        ],
        capture=True,
    )
    return output == "true"


def missing_release_checksums(
    config: ReleaseConfig,
    expected_assets: tuple[str, ...],
    runner: CommandRunner,
) -> list[str]:
    with tempfile.TemporaryDirectory(prefix="shipyard-release-checksums-proof-") as temp:
        checksums = Path(temp) / "checksums.sha256"
        runner.run(
            [
                "gh",
                "release",
                "download",
                "--repo",
                config.repo,
                config.tag,
                "--pattern",
                "checksums.sha256",
                "--output",
                str(checksums),
                "--clobber",
            ]
        )
        covered = {
            fields[1].lstrip("*")
            for line in checksums.read_text(encoding="utf-8").splitlines()
            if len(fields := line.split(maxsplit=1)) == 2
            and len(fields[0]) == 64
            and all(character in "0123456789abcdefABCDEF" for character in fields[0])
        }
    return [
        name
        for name in expected_assets
        if name != "checksums.sha256" and name not in covered
    ]


def upload_artifact_and_checksums(
    config: ReleaseConfig,
    artifact: Path,
    runner: CommandRunner,
) -> Path:
    runner.run(
        [
            "gh",
            "release",
            "upload",
            "--repo",
            config.repo,
            config.tag,
            str(artifact),
            "--clobber",
        ]
    )
    checksums = merge_release_checksum(config, artifact, runner)
    runner.run(
        [
            "gh",
            "release",
            "upload",
            "--repo",
            config.repo,
            config.tag,
            str(checksums),
            "--clobber",
        ]
    )
    return checksums


def merge_release_checksum(
    config: ReleaseConfig,
    artifact: Path,
    runner: CommandRunner,
) -> Path:
    checksum_line = f"{package_release.sha256(artifact)}  {artifact.name}"
    temp = Path(tempfile.mkdtemp(prefix="shipyard-release-checksums-"))
    checksums = temp / "checksums.sha256"
    if "checksums.sha256" in release_asset_names(config, runner):
        runner.run(
            [
                "gh",
                "release",
                "download",
                "--repo",
                config.repo,
                config.tag,
                "--pattern",
                "checksums.sha256",
                "--output",
                str(checksums),
                "--clobber",
            ]
        )
        lines = [
            line
            for line in checksums.read_text(encoding="utf-8").splitlines()
            if not line.endswith(f"  {artifact.name}")
        ]
    else:
        lines = []
    lines.append(checksum_line)
    checksums.write_text("\n".join(sorted(lines)) + "\n", encoding="utf-8")
    return checksums


def publish_if_ready(config: ReleaseConfig, runner: CommandRunner) -> str:
    expected_assets = expected_release_assets(config.artifact_prefix)
    assets = set(release_asset_names(config, runner))
    missing = [
        name for name in expected_assets
        if name not in assets
    ]
    if not missing:
        missing = [
            f"checksum:{name}"
            for name in missing_release_checksums(config, expected_assets, runner)
        ]
    if missing:
        if not release_is_draft(config, runner):
            runner.run(
                [
                    "gh",
                    "release",
                    "edit",
                    "--repo",
                    config.repo,
                    config.tag,
                    "--draft=true",
                ]
            )
        print("keeping release draft; missing release asset(s): " + ", ".join(missing))
        return "partial"

    was_draft = release_is_draft(config, runner)
    did_publish = False
    if was_draft:
        runner.run(
            [
                "gh",
                "release",
                "edit",
                "--repo",
                config.repo,
                config.tag,
                "--draft=false",
            ]
        )
        did_publish = True

    try:
        wait_for_public_release_assets(config, runner)
        run_install_e2e(config, runner)
    except SystemExit:
        runner.run(
            [
                "gh",
                "release",
                "edit",
                "--repo",
                config.repo,
                config.tag,
                "--draft=true",
            ]
        )
        raise SystemExit(4)

    return "published" if did_publish else "already-public"


def wait_for_public_release_assets(
    config: ReleaseConfig,
    runner: CommandRunner,
    *,
    timeout_secs: int = PUBLIC_ASSET_VISIBILITY_TIMEOUT_SECS,
    poll_secs: int = PUBLIC_ASSET_VISIBILITY_POLL_SECS,
) -> None:
    expected = set(expected_release_assets(config.artifact_prefix))
    url = f"https://api.github.com/repos/{config.repo}/releases/tags/{config.tag}"
    deadline = time.monotonic() + timeout_secs
    last_detail = "not checked"
    while True:
        try:
            raw = runner.run(release_api_curl_args(url), capture=True)
            payload = json.loads(raw)
            assets = payload.get("assets", [])
            names = {
                asset.get("name")
                for asset in assets
                if isinstance(asset, dict) and isinstance(asset.get("name"), str)
            }
            missing = sorted(expected.difference(names))
            if not missing:
                return
            last_detail = "missing asset(s): " + ", ".join(missing)
        except (SystemExit, json.JSONDecodeError) as error:
            last_detail = str(error)

        if time.monotonic() >= deadline:
            raise SystemExit(
                "release assets were not visible through the public GitHub "
                f"release API after {timeout_secs}s: {last_detail}"
            )
        time.sleep(poll_secs)


def release_api_curl_args(url: str) -> list[str]:
    args = ["curl", "-fsSL"]
    token = os.environ.get("SHIPYARD_GITHUB_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        args.extend(["-H", f"Authorization: Bearer {token}"])
    args.append(url)
    return args


def _install_env(config: ReleaseConfig, install_dir: Path, tag: str) -> dict[str, str]:
    env = {
        "SHIPYARD_REPO": config.repo,
        "SHIPYARD_VERSION": tag,
        "SHIPYARD_INSTALL_DIR": str(install_dir),
        "SHIPYARD_ARTIFACT_PREFIX": config.artifact_prefix,
    }
    return env


def _provider_expected_for_tag(tag: str) -> bool:
    if tag == "latest":
        return True
    value = tag.removeprefix("v")
    try:
        major, minor, patch = (int(part) for part in value.split(".", 2))
    except ValueError:
        return True
    return (major, minor, patch) >= (0, 127, 0)


def run_install_e2e(config: ReleaseConfig, runner: CommandRunner) -> str:
    with tempfile.TemporaryDirectory(prefix="shipyard-install-e2e-") as temp:
        install_dir = Path(temp) / "bin"
        binary = install_dir / package_release.BIN_NAME
        companion = install_dir / package_release.COMPANION_BIN_NAME
        observed: list[str] = []

        def install_and_probe(tag: str, phase: str) -> None:
            runner.run(
                ["bash", str(config.install_sh)],
                env=_install_env(config, install_dir, tag),
            )
            output = runner.run([str(binary), "--version"], capture=True)
            primary_version = package_release.parse_binary_version(
                output,
                package_release.BIN_NAME,
                source=f"{phase} installed CLI",
            )
            if _provider_expected_for_tag(tag):
                provider_output = runner.run([str(companion), "--version"], capture=True)
                provider_version = package_release.parse_binary_version(
                    provider_output,
                    package_release.COMPANION_BIN_NAME,
                    source=f"{phase} installed provider",
                )
                if provider_version != primary_version:
                    raise SystemExit(
                        f"{phase} installed binary version mismatch: "
                        f"{primary_version} != {provider_version}"
                    )
                observed.append(f"{phase}:{tag}:{output}:{provider_output}")
            else:
                if companion.exists():
                    raise SystemExit(
                        f"{phase} rollback left a newer provider binary installed"
                    )
                observed.append(f"{phase}:{tag}:{output}:provider-absent")

        if config.rollback_tag:
            install_and_probe(config.rollback_tag, "baseline")
            install_and_probe(config.tag, "upgrade")
            install_and_probe(config.rollback_tag, "rollback")
        else:
            install_and_probe(config.tag, "install")

        return "\n".join(observed)


def _json_documents(text: str) -> list[dict]:
    decoder = json.JSONDecoder()
    documents: list[dict] = []
    index = 0
    while index < len(text):
        while index < len(text) and text[index].isspace():
            index += 1
        if index >= len(text):
            break
        try:
            value, index = decoder.raw_decode(text, index)
        except json.JSONDecodeError:
            break
        if isinstance(value, dict):
            documents.append(value)
    return documents


def run_fleet_rollout(config: ReleaseConfig, runner: CommandRunner, shipyard: str | None) -> None:
    """Roll the just-published tag out to every configured host and verify it.

    The release is not reverted on failure: it is public, verified installable,
    and other consumers may already hold it. The failure is loud and names the
    hosts that did not verify, and the fleet-reconcile backstop retries later.
    """
    if not shipyard:
        print(
            "FLEET ROLLOUT FAILED: no `shipyard` controller binary on PATH; "
            "pass --fleet-shipyard or --no-fleet-rollout",
            file=sys.stderr,
        )
        raise SystemExit(FLEET_ROLLOUT_FAILED_EXIT)
    # The controller binary is the one already installed, i.e. the previous
    # release. A controller that predates verified rollouts (no fleet-reconcile)
    # or a Mac with no [host_class] cannot run this stage; neither makes the
    # release incomplete, so hand over to the backstop with a warning.
    code, _, _ = runner.run_status([shipyard, "runner", "fleet-reconcile", "--help"])
    if code != 0:
        print(
            f"WARNING: {shipyard} predates verified fleet rollouts; the fleet was NOT "
            f"updated to {config.tag} here. Once the controller runs a release with "
            "`runner fleet-reconcile`, its agent rolls the fleet after the soak.",
            file=sys.stderr,
        )
        return
    plan = [shipyard, "--json", "runner", "fleet-update", "--to", config.tag, "--all-hosts"]
    code, _, stderr = runner.run_status(plan)
    if code != 0 and "No [host_class." in stderr:
        print(
            "WARNING: this Mac declares no [host_class.*], so it is not the fleet "
            f"controller; the fleet was NOT updated to {config.tag} here. The "
            "controller's fleet-reconcile agent rolls it out after the soak.",
            file=sys.stderr,
        )
        return
    if code != 0:
        print(
            f"FLEET ROLLOUT FAILED for {config.tag}: the rollout plan was refused; "
            "the release stays published.",
            file=sys.stderr,
        )
        if stderr.strip():
            print(stderr.strip(), file=sys.stderr)
        raise SystemExit(FLEET_ROLLOUT_FAILED_EXIT)
    command = [*plan, "--apply"]
    print(f"fleet rollout: {' '.join(command)}")
    code, stdout, stderr = runner.run_status(command)
    summaries = [
        document
        for document in _json_documents(stdout)
        if document.get("event") == "fleet_summary"
    ]
    summary = summaries[-1] if summaries else None
    if code == 0 and summary and summary.get("verdict") == "verified":
        hosts = ", ".join(summary.get("verified_hosts") or [])
        print(f"fleet rollout verified at {config.tag}: {hosts}")
        return
    failed = (summary or {}).get("failed_host") or {}
    lagging = [
        name
        for name in [failed.get("host_class"), *((summary or {}).get("not_attempted_hosts") or [])]
        if name
    ]
    print(
        f"FLEET ROLLOUT FAILED for {config.tag} (exit {code}); the release stays "
        "published. Hosts not verified: "
        + (", ".join(lagging) if lagging else "UNKNOWN (no fleet summary was produced)"),
        file=sys.stderr,
    )
    if failed.get("reason"):
        print(f"  cause: {failed['reason']}", file=sys.stderr)
    if stderr.strip():
        print(stderr.strip(), file=sys.stderr)
    raise SystemExit(FLEET_ROLLOUT_FAILED_EXIT)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", help="Release tag, for example v0.1.0")
    parser.add_argument("--repo", default=os.environ.get("SHIPYARD_REPO", DEFAULT_REPO))
    parser.add_argument("--arch", default="arm64", help="Only arm64 is supported")
    parser.add_argument("--upload", action="store_true", help="Upload DMG and checksum to the GitHub release")
    parser.add_argument(
        "--check-auth",
        action="store_true",
        help="Validate unattended credentials and run a disposable signing probe, then exit",
    )
    parser.add_argument(
        "--ci-mode",
        action="store_true",
        help=(
            "Allow same-runner DMG mount skips only for non-upload diagnostics; "
            "publication still requires mounted-DMG and public install E2E proof"
        ),
    )
    parser.add_argument("--skip-build", action="store_true", help="Use an existing --binary instead of building")
    parser.add_argument("--binary", type=Path, help="Existing shipyard binary to package")
    parser.add_argument(
        "--companion-binary", type=Path, help="Existing provider binary to package"
    )
    parser.add_argument("--cargo-target", help="Optional Rust target triple")
    parser.add_argument(
        "--rollback-tag",
        help=(
            "Optional previous known-good tag. When set, post-publish install "
            "E2E verifies previous -> current -> previous inside an isolated "
            "install directory."
        ),
    )
    parser.add_argument(
        "--no-fleet-rollout",
        action="store_true",
        help=(
            "Skip the final governed fleet rollout + verification. The release "
            "is then not done until the fleet verifies; fleet-reconcile catches up."
        ),
    )
    parser.add_argument(
        "--fleet-shipyard",
        default=None,
        help="Controller shipyard binary for the fleet rollout (default: shipyard on PATH)",
    )
    parser.add_argument("--dist-dir", type=Path, default=package_release.DEFAULT_DIST_DIR)
    parser.add_argument("--artifact-prefix", default=package_release.BIN_NAME)
    parser.add_argument(
        "--env-file",
        type=Path,
        action="append",
        help=(
            "Repeatable dotenv file with Shipyard release credentials; supports "
            "Apple-ID aliases and M5 PULP_SIGN_*/PULP_NOTARY_* aliases."
        ),
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    runner = CommandRunner()
    args = parse_args(argv or sys.argv[1:])
    load_release_environment(resolve_environment_files(args.env_file or []))
    require_arm64(args.arch)
    mode = check_unattended_auth()
    if args.check_auth:
        print(f"unattended release authentication ready: notarization={mode}")
        return 0
    tag = resolve_tag(args.tag, runner)
    config = ReleaseConfig(
        tag=tag,
        repo=args.repo,
        artifact_prefix=args.artifact_prefix,
        dist_dir=args.dist_dir,
        upload=args.upload,
        ci_mode=args.ci_mode,
        skip_build=args.skip_build,
        binary=args.binary,
        cargo_target=args.cargo_target,
        companion_binary=args.companion_binary,
        rollback_tag=args.rollback_tag,
    )
    dmg = package_signed_dmg(config)
    if not config.upload:
        print(f"signed + notarized DMG ready: {dmg}")
        print(f"rerun with --upload to attach it to {config.repo} {config.tag}")
        return 0
    upload_artifact_and_checksums(config, dmg, runner)
    outcome = publish_if_ready(config, runner)
    print(f"release outcome: {outcome}")
    if outcome in ("published", "already-public"):
        if args.no_fleet_rollout:
            print(
                "WARNING: --no-fleet-rollout: the fleet was NOT updated to "
                f"{config.tag}; the release is not done until every host verifies "
                "(shipyard runner fleet-reconcile will catch up after the soak).",
                file=sys.stderr,
            )
        elif config.ci_mode:
            print(
                "WARNING: --ci-mode has no fleet to roll out to; the controller's "
                "shipyard runner fleet-reconcile will update the fleet after the soak.",
                file=sys.stderr,
            )
        else:
            run_fleet_rollout(
                config, runner, args.fleet_shipyard or shutil.which("shipyard")
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
