#!/usr/bin/env python3
"""Build ShipyardLauncher.app; sign, notarize and staple it on the maintainer's Mac.

  build_launcher.py --dry-run            build an unsigned bundle, print the signing plan
  build_launcher.py --sign               build and sign (Developer ID, hardened runtime)
  build_launcher.py --sign --notarize    also notarize, staple and gate on spctl

Signing uses the same environment as scripts/release_macos_local.py
(SHIPYARD_SIGNING_IDENTITY, optional SHIPYARD_SIGNING_P12/_PASSWORD or
SHIPYARD_SIGNING_KEYCHAIN, and the notarization variables), loaded from the
same --env-file defaults, and the same keychain and notarytool helpers in
scripts/package_release.py. It never runs in CI and never uploads anything.

The result is <output>/ShipyardLauncher.app plus
<output>/ShipyardLauncher-macos-arm64.zip and its .sha256. Stage the app on a
host at ~/.local/share/shipyard/launcher/staged/ShipyardLauncher.app and
fleet-deploy installs it (README.md).
"""

from __future__ import annotations

import argparse
import hashlib
import os
import plistlib
import shlex
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
BUNDLE_ID = "com.danielraffel.shipyard.launcher"
APP_NAME = "ShipyardLauncher.app"
EXECUTABLE = "shipyard-launcher"
ZIP_NAME = "ShipyardLauncher-macos-arm64.zip"
DEVELOPER_ID_REQUIREMENT = (
    f'identifier "{BUNDLE_ID}" and anchor apple generic'
    " and certificate 1[field.1.2.840.113635.100.6.2.6] exists"
    " and certificate leaf[field.1.2.840.113635.100.6.1.13] exists"
)


def say(message: str) -> None:
    print(f"build-launcher: {message}", flush=True)


def run(args: list[str], **kwargs) -> subprocess.CompletedProcess:
    say("$ " + shlex.join(args))
    return subprocess.run(args, check=True, **kwargs)


def build_bundle(stage: Path) -> Path:
    """Compile main.swift into an unsigned bundle under `stage`."""
    app = stage / APP_NAME
    (app / "Contents" / "MacOS").mkdir(parents=True)
    shutil.copy(HERE / "Info.plist", app / "Contents" / "Info.plist")
    run(
        [
            "xcrun", "swiftc", "-O", "-whole-module-optimization",
            "-target", "arm64-apple-macos13", "-framework", "Security",
            str(HERE / "main.swift"), "-o", str(app / "Contents" / "MacOS" / EXECUTABLE),
        ]
    )
    check_structure(app)
    return app


def check_structure(app: Path) -> None:
    info = plistlib.loads((app / "Contents" / "Info.plist").read_bytes())
    if info.get("CFBundleIdentifier") != BUNDLE_ID or info.get("CFBundleExecutable") != EXECUTABLE:
        raise SystemExit("Info.plist does not name the launcher")
    executable = app / "Contents" / "MacOS" / EXECUTABLE
    if not os.access(executable, os.X_OK):
        raise SystemExit(f"{executable} is not executable")
    output = subprocess.run(
        [str(executable), "--launcher-version"], check=True, capture_output=True, text=True
    ).stdout
    if not output.startswith("shipyard-launcher "):
        raise SystemExit(f"unexpected --launcher-version output: {output!r}")


def signing_plan(app: Path, notarize: bool) -> list[str]:
    identity = os.environ.get("SHIPYARD_SIGNING_IDENTITY", "<SHIPYARD_SIGNING_IDENTITY>")
    plan = [
        shlex.join(["codesign", "--force", "--timestamp", "--options", "runtime",
                    "--identifier", BUNDLE_ID, "--sign", identity, str(app)]),
        shlex.join(["codesign", "--verify", "--strict", "--deep", "--verbose=2",
                    f"-R={DEVELOPER_ID_REQUIREMENT}", str(app)]),
    ]
    if notarize:
        plan += [
            shlex.join(["ditto", "-c", "-k", "--keepParent", str(app), "<tmp>/submit.zip"]),
            "xcrun notarytool submit <tmp>/submit.zip <credentials> --wait",
            shlex.join(["xcrun", "stapler", "staple", str(app)]),
            shlex.join(["xcrun", "stapler", "validate", str(app)]),
            shlex.join(["spctl", "--assess", "--type", "execute", "-vv", str(app)]),
        ]
    plan.append(shlex.join(["ditto", "-c", "-k", "--keepParent", str(app), f"<output>/{ZIP_NAME}"]))
    return plan


def load_release_tooling():
    sys.path.insert(0, str(ROOT / "scripts"))
    import package_release  # noqa: PLC0415
    import release_macos_local  # noqa: PLC0415

    return package_release, release_macos_local


def sign(app: Path, package_release) -> None:
    identity = os.environ["SHIPYARD_SIGNING_IDENTITY"]
    keychain = os.environ.get("SHIPYARD_SIGNING_KEYCHAIN")
    env = os.environ.copy()
    if signing_home := os.environ.get("SHIPYARD_SIGNING_HOME"):
        env["HOME"] = signing_home
    package_release.run(
        ["codesign", "--force", "--timestamp", "--options", "runtime",
         "--identifier", BUNDLE_ID, "--sign", identity,
         *(["--keychain", keychain] if keychain else []), str(app)],
        env=env, capture=True,
    )
    package_release.run(
        ["codesign", "--verify", "--strict", "--deep", "--verbose=2",
         f"-R={DEVELOPER_ID_REQUIREMENT}", str(app)],
        capture=True,
    )


def notarize(app: Path, package_release) -> None:
    """Submit a zip of the app, then staple the ticket to the app itself."""
    with tempfile.TemporaryDirectory(prefix="shipyard-launcher-notary-") as temp:
        submission = Path(temp) / "submit.zip"
        package_release.run(["ditto", "-c", "-k", "--keepParent", str(app), str(submission)])
        if package_release.notarization_mode() == "api-key":
            package_release.submit_for_notarization(
                submission,
                ["--key", str(package_release.expanded_env_path("SHIPYARD_NOTARIZE_KEY_PATH")),
                 "--key-id", os.environ["SHIPYARD_NOTARIZE_KEY_ID"],
                 "--issuer", os.environ["SHIPYARD_NOTARIZE_ISSUER_ID"]],
            )
        else:
            keychain, _password = package_release.create_notary_keychain(Path(temp))
            profile = f"shipyard-launcher-notary-{os.getpid()}"
            try:
                package_release.store_notary_credentials(profile, keychain)
                package_release.submit_for_notarization(
                    submission, ["--keychain-profile", profile, "--keychain", str(keychain)]
                )
            finally:
                package_release.delete_notary_keychain(keychain)
    package_release.staple(app)
    assessment = subprocess.run(
        ["spctl", "--assess", "--type", "execute", "-vv", str(app)],
        capture_output=True, text=True, check=False,
    )
    detail = (assessment.stdout + assessment.stderr).strip()
    if assessment.returncode != 0 or "Notarized Developer ID" not in detail:
        raise SystemExit(f"spctl did not accept the stapled launcher:\n{detail}")
    say(detail)


def archive(app: Path, output: Path) -> Path:
    archive_path = output / ZIP_NAME
    archive_path.unlink(missing_ok=True)
    run(["ditto", "-c", "-k", "--keepParent", str(app), str(archive_path)])
    digest = hashlib.sha256(archive_path.read_bytes()).hexdigest()
    (output / f"{ZIP_NAME}.sha256").write_text(f"{digest}  {ZIP_NAME}\n")
    say(f"{archive_path} sha256={digest}")
    return archive_path


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--output", type=Path, default=ROOT / "dist" / "launcher")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--dry-run", action="store_true", help="build unsigned; print the signing plan")
    mode.add_argument("--sign", action="store_true", help="sign with SHIPYARD_SIGNING_IDENTITY")
    parser.add_argument("--notarize", action="store_true", help="with --sign: notarize, staple, spctl gate")
    parser.add_argument("--env-file", type=Path, action="append", default=[],
                        help="release environment file(s); defaults as release_macos_local.py")
    args = parser.parse_args(argv)
    if args.notarize and not args.sign:
        parser.error("--notarize needs --sign")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    if sys.platform != "darwin":
        raise SystemExit("the launcher is built on macOS only")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    final = output / APP_NAME
    if final.exists():
        raise SystemExit(f"refusing to replace {final}; move it away first")
    with tempfile.TemporaryDirectory(prefix="shipyard-launcher-build-", dir=output) as stage:
        app = build_bundle(Path(stage))
        if args.dry_run:
            say("dry run: nothing is signed or notarized. The signed build would run:")
            for line in signing_plan(final, notarize=True):
                print(f"  {line}")
        else:
            package_release, release_macos_local = load_release_tooling()
            release_macos_local.load_release_environment(
                release_macos_local.resolve_environment_files(args.env_file)
            )
            package_release.require_signing_env(notarize=args.notarize)
            with package_release.prepared_signing_keychain():
                sign(app, package_release)
            if args.notarize:
                notarize(app, package_release)
        app.rename(final)
    say(f"built {final}{' (unsigned)' if args.dry_run else ''}")
    if not args.dry_run:
        archive(final, output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
