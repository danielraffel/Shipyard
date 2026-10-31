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
import release_artifact_manifest
import release_env


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_REPO = "danielraffel/Shipyard"
GHAPP_ENV = "SHIPYARD_GHAPP_PATH"
DEFAULT_LOCAL_ENV_FILES = (
    Path.home() / ".config" / "pulp" / "secrets" / "keychain.env",
    Path.home() / ".config" / "pulp" / "secrets" / "notary.env",
)
PUBLIC_ASSET_VISIBILITY_TIMEOUT_SECS = 90
PUBLIC_ASSET_VISIBILITY_POLL_SECS = 3
# Distinct from every earlier failure: the release is public and installable,
# but at least one fleet host did not verify at it.
FLEET_ROLLOUT_FAILED_EXIT = 6
# `shipyard runner fleet-update` exit codes the stage treats specially.
FLEET_CONTROLLER_BUSY_EXIT = 75
FLEET_ROLLBACK_FAILED_EXIT = 7


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
    def __init__(self, *, ci_mode: bool = False) -> None:
        self.ci_gh = require_ci_gh() if ci_mode else None

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
        if self.ci_gh and args[0] == "gh":
            args = [self.ci_gh, *args[1:]]
            # Explicit job identity wins over any inherited CLI authentication.
            merged_env["GH_TOKEN"] = os.environ["GITHUB_TOKEN"]
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


def ci_prepared_signing_keychain() -> bool:
    return (
        os.environ.get("CI") == "true"
        and os.environ.get("SHIPYARD_SIGNING_KEYCHAIN_READY") == "1"
    )


def require_ci_gh() -> str:
    """Use only the Actions job token and the host's native CLI in CI."""
    if os.environ.get("GITHUB_ACTIONS") != "true" or not os.environ.get("GITHUB_TOKEN"):
        raise SystemExit("CI publication requires a GitHub Actions job token")
    native = shutil.which("gh", path="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
    if not native:
        raise SystemExit("CI publication requires native gh in a system installation path")
    return native


def require_ghapp() -> str:
    """Return the configured or PATH-resolved App wrapper.

    Release runners may run under a service account whose home directory is
    unrelated to the managed wrapper installation.  An explicit path wins;
    otherwise resolve the command from the runner's authenticated PATH.  Do
    not silently fall back to an owner's ambient ``gh`` login.
    """
    configured = os.environ.get(GHAPP_ENV)
    candidates: list[Path] = []
    if configured:
        candidates.append(Path(configured))
    else:
        discovered = shutil.which("ghapp")
        if discovered:
            candidates.append(Path(discovered))
    for candidate in candidates:
        if not candidate.is_absolute():
            continue
        try:
            if candidate.is_file() and os.access(candidate, os.X_OK):
                return str(candidate)
        except OSError:
            continue
    detail = configured or "ghapp on PATH"
    raise SystemExit(
        "the configured authenticated ghapp wrapper is missing or not executable: "
        f"{detail} (set {GHAPP_ENV} or install ghapp on PATH)"
    )


def resolve_environment_files(requested: list[Path]) -> list[Path]:
    if requested:
        return requested
    # A CI job carries its own credentials. The host's local release secrets
    # (present on a self-hosted runner that is also a developer machine) would
    # only fill the gaps with a different certificate.
    if ci_prepared_signing_keychain():
        return []
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


def expected_release_assets(
    artifact_prefix: str, focus: str | None = None
) -> tuple[str, ...]:
    """Assets a release must carry before it is published.

    `focus` follows the CI_PLATFORM_FOCUS repo variable (read from the
    environment when not given): `macos` (default) expects macOS and
    Linux x64/arm64; `all` also expects windows-x64.
    """
    companion = package_release.COMPANION_BIN_NAME
    focus = (focus or os.environ.get("CI_PLATFORM_FOCUS") or "macos").lower()
    assets = [
        f"{artifact_prefix}-linux-x64",
        f"{companion}-linux-x64",
        f"{artifact_prefix}-linux-arm64",
        f"{companion}-linux-arm64",
    ]
    if focus == "all":
        assets += [
            f"{artifact_prefix}-windows-x64.exe",
            f"{companion}-windows-x64.exe",
        ]
    assets += [
        f"{artifact_prefix}-macos-arm64.dmg",
        "shipyard-release-manifest.json",
        "checksums.sha256",
    ]
    return tuple(assets)


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
        if name not in {"checksums.sha256", "shipyard-release-manifest.json"} and name not in covered
    ]


def verify_release_identity_manifest(
    config: ReleaseConfig,
    expected_assets: tuple[str, ...],
    runner: CommandRunner,
) -> None:
    """Require the published bytes to match the exact tag and commit manifest.

    The manifest is useful only if it is checked against the bytes that GitHub
    will serve.  Checking names and source metadata alone would allow a
    replaced asset to pass the publication gate.
    """
    temp = Path(tempfile.mkdtemp(prefix="shipyard-release-manifest-proof-")) / "manifest.json"
    runner.run(
        [
            "gh",
            "release",
            "download",
            "--repo",
            config.repo,
            config.tag,
            "--pattern",
            "shipyard-release-manifest.json",
            "--output",
            str(temp),
            "--clobber",
        ]
    )
    try:
        document = json.loads(temp.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SystemExit(f"invalid shipyard-release-manifest.json: {error}") from error
    source = document.get("source")
    expected_commit = _release_commit(config, runner)
    if (
        document.get("schema_version") != release_artifact_manifest.SCHEMA_VERSION
        or not isinstance(source, dict)
        or source.get("repository") != config.repo
        or source.get("tag") != config.tag
        or source.get("commit") != expected_commit
    ):
        raise SystemExit("release identity manifest does not match this tag and commit")
    manifest_files: dict[str, dict[str, object]] = {}
    for artifact in document.get("artifacts", []):
        if not isinstance(artifact, dict):
            raise SystemExit("release identity manifest has a malformed artifact")
        for entry in artifact.get("files", []):
            if not isinstance(entry, dict):
                raise SystemExit("release identity manifest has a malformed file")
            name = entry.get("name")
            size = entry.get("bytes")
            digest = entry.get("sha256")
            if (
                not isinstance(name, str)
                or Path(name).name != name
                or not isinstance(size, int)
                or size < 0
                or not isinstance(digest, str)
                or len(digest) != 64
                or any(character not in "0123456789abcdef" for character in digest)
            ):
                raise SystemExit("release identity manifest has an invalid file entry")
            if name in manifest_files:
                raise SystemExit(f"release identity manifest repeats asset: {name}")
            manifest_files[name] = {"bytes": size, "sha256": digest}
    required = set(expected_assets) - {"checksums.sha256", "shipyard-release-manifest.json"}
    missing = sorted(required - set(manifest_files))
    if missing:
        raise SystemExit("release identity manifest is missing: " + ", ".join(missing))

    with tempfile.TemporaryDirectory(prefix="shipyard-release-assets-proof-") as directory:
        root = Path(directory)
        for name, expected in sorted(manifest_files.items()):
            output = root / name
            runner.run(
                [
                    "gh",
                    "release",
                    "download",
                    "--repo",
                    config.repo,
                    config.tag,
                    "--pattern",
                    name,
                    "--output",
                    str(output),
                    "--clobber",
                ]
            )
            if not output.is_file():
                raise SystemExit(f"release asset download produced no file: {name}")
            actual_size = output.stat().st_size
            actual_digest = release_artifact_manifest.sha256(output)
            if actual_size != expected["bytes"] or actual_digest != expected["sha256"]:
                raise SystemExit(
                    f"release asset does not match identity manifest: {name}"
                )

        checksums = root / "checksums.sha256"
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
        if not checksums.is_file():
            raise SystemExit("release checksum asset download produced no file")
        checksum_entries: dict[str, str] = {}
        for line in checksums.read_text(encoding="utf-8").splitlines():
            fields = line.split(maxsplit=1)
            if len(fields) == 2 and len(fields[0]) == 64:
                checksum_entries[fields[1].lstrip("*")] = fields[0].lower()
        for name, expected in manifest_files.items():
            if checksum_entries.get(name) != expected["sha256"]:
                raise SystemExit(f"release checksum does not match manifest: {name}")


def upload_artifact_and_checksums(
    config: ReleaseConfig,
    artifact: Path,
    runner: CommandRunner,
) -> Path:
    publisher = "gh" if config.ci_mode else require_ghapp()
    replace_flag = "--clobber" if config.ci_mode else "--replace-existing"
    runner.run(
        [
            publisher,
            "release",
            "upload",
            "--repo",
            config.repo,
            config.tag,
            str(artifact),
            replace_flag,
        ]
    )
    merge_release_identity_manifest(config, artifact, runner)
    checksums = merge_release_checksum(config, artifact, runner)
    runner.run(
        [
            publisher,
            "release",
            "upload",
            "--repo",
            config.repo,
            config.tag,
            str(checksums),
            replace_flag,
        ]
    )
    return checksums


def _release_commit(config: ReleaseConfig, runner: CommandRunner) -> str:
    return os.environ.get("GITHUB_SHA") or runner.run(
        ["git", "rev-parse", f"{config.tag}^{{commit}}"], capture=True
    )


def merge_release_identity_manifest(
    config: ReleaseConfig,
    artifact: Path,
    runner: CommandRunner,
) -> None:
    """Add the signed DMG to the immutable identity manifest before publish."""
    assets = set(release_asset_names(config, runner))
    current: dict[str, object]
    if "shipyard-release-manifest.json" in assets:
        temp = Path(tempfile.mkdtemp(prefix="shipyard-release-manifest-")) / "manifest.json"
        runner.run(
            [
                "gh",
                "release",
                "download",
                "--repo",
                config.repo,
                config.tag,
                "--pattern",
                "shipyard-release-manifest.json",
                "--output",
                str(temp),
                "--clobber",
            ]
        )
        current = json.loads(temp.read_text(encoding="utf-8"))
        if current.get("schema_version") != release_artifact_manifest.SCHEMA_VERSION:
            raise SystemExit("unsupported shipyard-release-manifest.json schema")
        source = current.get("source")
        expected = {
            "repository": config.repo,
            "tag": config.tag,
            "commit": _release_commit(config, runner),
        }
        if not isinstance(source, dict) or any(source.get(k) != v for k, v in expected.items()):
            raise SystemExit("release identity manifest does not name this tag and commit")
    else:
        current = {
            "schema_version": release_artifact_manifest.SCHEMA_VERSION,
            "source": {
                "repository": config.repo,
                "tag": config.tag,
                "commit": _release_commit(config, runner),
                "run_id": int(os.environ.get("GITHUB_RUN_ID", "0")),
                "run_attempt": int(os.environ.get("GITHUB_RUN_ATTEMPT", "0")),
            },
            "artifacts": [],
        }
    artifacts = current.setdefault("artifacts", [])
    if not isinstance(artifacts, list):
        raise SystemExit("release identity manifest has malformed artifacts")
    artifacts[:] = [
        entry
        for entry in artifacts
        if isinstance(entry, dict) and entry.get("target") != "macos-arm64"
    ]
    artifacts.append(
        {
            "target": "macos-arm64",
            "artifact_id": f"release-asset:{artifact.name}",
            "artifact_digest": package_release.sha256(artifact),
            "files": [
                {
                    "name": artifact.name,
                    "bytes": artifact.stat().st_size,
                    "sha256": package_release.sha256(artifact),
                }
            ],
        }
    )
    manifest = Path(tempfile.mkdtemp(prefix="shipyard-release-manifest-")) / "shipyard-release-manifest.json"
    manifest.write_text(json.dumps(current, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    runner.run(
        [
            "gh" if config.ci_mode else require_ghapp(),
            "release",
            "upload",
            "--repo",
            config.repo,
            config.tag,
            str(manifest),
            "--clobber" if config.ci_mode else "--replace-existing",
        ]
    )


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
            and not line.endswith("  shipyard-release-manifest.json")
        ]
    else:
        lines = []
    lines.append(checksum_line)
    manifest = Path(tempfile.mkdtemp(prefix="shipyard-release-manifest-checksum-")) / "manifest.json"
    runner.run(
        [
            "gh",
            "release",
            "download",
            "--repo",
            config.repo,
            config.tag,
            "--pattern",
            "shipyard-release-manifest.json",
            "--output",
            str(manifest),
            "--clobber",
        ]
    )
    lines.append(f"{package_release.sha256(manifest)}  shipyard-release-manifest.json")
    checksums.write_text("\n".join(sorted(lines)) + "\n", encoding="utf-8")
    return checksums


def keep_draft(config: ReleaseConfig, runner: CommandRunner) -> None:
    """Make sure the release is a draft, so nothing can install it yet."""
    if not release_is_draft(config, runner):
        runner.run(
            ["gh", "release", "edit", "--repo", config.repo, config.tag, "--draft=true"]
        )


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
        if not missing:
            verify_release_identity_manifest(config, expected_assets, runner)
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
    # Only hosts behind the tag: reinstalling a current host restarts its daemon
    # and cannot be rolled back (there is no earlier version to restore).
    command = [*plan, "--apply", "--lagging-only"]
    print(f"fleet rollout: {' '.join(command)}")
    code, stdout, stderr = runner.run_status(command)
    if code == FLEET_CONTROLLER_BUSY_EXIT:
        print(
            f"WARNING: another fleet rollout holds the controller lock; {config.tag} was "
            "not rolled out here. The running rollout or the fleet-reconcile agent "
            "finishes the fleet.",
            file=sys.stderr,
        )
        return
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
    if code == FLEET_ROLLBACK_FAILED_EXIT or (summary or {}).get("verdict") == "rollback_failed":
        print(
            f"FLEET ROLLBACK FAILED for {config.tag}: {failed.get('host_class', 'a host')} "
            "was updated, failed, and could NOT be restored; it needs an operator now.",
            file=sys.stderr,
        )
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
    publish = parser.add_mutually_exclusive_group()
    publish.add_argument(
        "--defer-publish",
        action="store_true",
        help=(
            "With --upload: attach the DMG and checksums but leave the release a "
            "draft, so CI can attest the uploaded DMG before anyone can install it"
        ),
    )
    publish.add_argument(
        "--publish-only",
        action="store_true",
        help=(
            "Do not build or sign: publish an already-uploaded draft release "
            "after its assets and checksums check out and its install E2E passes"
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
    args = parse_args(argv or sys.argv[1:])
    runner = CommandRunner(ci_mode=args.ci_mode and (args.upload or args.publish_only))
    if args.defer_publish and not args.upload:
        raise SystemExit("--defer-publish only applies with --upload")
    if not args.publish_only:
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
        upload=args.upload or args.publish_only,
        ci_mode=args.ci_mode,
        skip_build=args.skip_build,
        binary=args.binary,
        cargo_target=args.cargo_target,
        companion_binary=args.companion_binary,
        rollback_tag=args.rollback_tag,
    )
    if not args.publish_only:
        dmg = package_signed_dmg(config)
        if not config.upload:
            print(f"signed + notarized DMG ready: {dmg}")
            print(f"rerun with --upload to attach it to {config.repo} {config.tag}")
            return 0
        upload_artifact_and_checksums(config, dmg, runner)
        if args.defer_publish:
            keep_draft(config, runner)
            print(f"release outcome: deferred ({config.tag} stays a draft until published)")
            return 0
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
