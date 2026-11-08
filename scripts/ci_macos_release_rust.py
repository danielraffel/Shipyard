#!/usr/bin/env python3
"""Install and reuse the pinned Rust toolchain for the macOS release job.

The signing job runs on self-hosted Macs that sit behind an interactive
outbound firewall (LuLu). The firewall approves a connection per executable
path, so a toolchain installed into a fresh per-run directory is a new,
unapproved binary on every release and stalls until a human clicks Allow.

This script keeps the toolchain at one fixed per-runner-user location, so a
single approval per host covers every later release, while keeping the build
isolated from the runner user's shared ``~/.cargo`` and ``~/.rustup``:

* Only the toolchain is shared. ``rustup`` and the toolchain live under
  ``--root``; the crate registry (``CARGO_HOME``) is a fresh per-run
  directory, and ``target/`` is the job's clean checkout.
* ``rustup-init`` is fetched at a pinned version and checked against a pinned
  SHA-256 before it runs. An installed ``rustup`` is reused only when it is
  byte-identical to that pin and reports the pinned version, so a pin bump
  reinstalls it.
* rustup's download servers are pinned to the official ones; inherited
  ``RUSTUP_DIST_SERVER`` / ``RUSTUP_DIST_ROOT`` / ``RUSTUP_UPDATE_ROOT`` are
  ignored.
* The toolchain is pinned to an exact version, verified with
  ``rustc --version`` / ``cargo --version``, made read-only, and its file tree
  is digested. A later run reuses it only when the digest still matches. A
  complete install whose digest changed fails the job (drift or tampering);
  only an incomplete install is removed and installed again.
* Installation holds an exclusive ``flock`` on ``<root>/.install.lock``, so
  two jobs on one host never install into the same directory at once.
* The build calls the toolchain's own ``cargo``/``rustc`` from ``PATH`` (not
  the rustup proxies), so a shared ``~/.cargo/bin`` cannot shadow them.

Every job running as the same OS user can still write ``--root``; the digest
detects a change between runs but is not a boundary against that user. The
boundary is that fork pull requests never reach the self-hosted runners.
"""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path

RUSTUP_INIT_URL = (
    "https://static.rust-lang.org/rustup/archive/{version}/{host}/rustup-init"
)
# rustup reads these to choose where it downloads from; the job pins them to
# the official servers so an inherited value cannot redirect the fetch.
OFFICIAL_RUSTUP_ENV = {
    "RUSTUP_DIST_SERVER": "https://static.rust-lang.org",
    "RUSTUP_UPDATE_ROOT": "https://static.rust-lang.org/rustup",
}
DROPPED_RUSTUP_ENV = ("RUSTUP_DIST_ROOT", "RUSTUP_TOOLCHAIN")
LOCK_WAIT_SECONDS = 30 * 60
ATTEMPT_TIMEOUT_SECONDS = 10 * 60


def log(message: str) -> None:
    print(f"ci-macos-release-rust: {message}", flush=True)


def fail(message: str) -> None:
    raise SystemExit(f"ci-macos-release-rust: {message}")


def annotate(level: str, message: str) -> None:
    print(f"::{level}::ci-macos-release-rust: {message}", flush=True)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def tree_digest(root: Path) -> str:
    """Digest of every path, file mode bit for exec, file content and link target."""
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        rel = path.relative_to(root).as_posix()
        info = path.lstat()
        if stat.S_ISLNK(info.st_mode):
            digest.update(f"L {rel} -> {os.readlink(path)}\n".encode())
        elif stat.S_ISDIR(info.st_mode):
            digest.update(f"D {rel}\n".encode())
        elif stat.S_ISREG(info.st_mode):
            executable = "x" if info.st_mode & stat.S_IXUSR else "-"
            digest.update(f"F {rel} {executable} {sha256_file(path)}\n".encode())
        else:
            fail(f"unexpected file type in toolchain: {path}")
    return digest.hexdigest()


def set_writable(root: Path, writable: bool) -> None:
    if not root.exists():
        return
    for path in [root, *root.rglob("*")]:
        info = path.lstat()
        if stat.S_ISLNK(info.st_mode):
            continue
        mode = stat.S_IMODE(info.st_mode)
        mode = mode | stat.S_IWUSR if writable else mode & ~(stat.S_IWUSR | stat.S_IWGRP | stat.S_IWOTH)
        os.chmod(path, mode)


def remove_tree(root: Path) -> None:
    if root.is_symlink() or root.is_file():
        root.unlink()
    elif root.exists():
        set_writable(root, True)
        shutil.rmtree(root)


def require_private_root(root: Path) -> None:
    if not root.is_absolute():
        fail(f"--root must be absolute: {root}")
    if root.is_symlink():
        fail(f"--root must not be a symlink: {root}")
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = root.lstat()
    if info.st_uid != os.getuid():
        fail(f"--root is owned by uid {info.st_uid}, not this user ({os.getuid()}): {root}")
    os.chmod(root, 0o700)


def acquire_lock(root: Path, wait_seconds: float):
    handle = (root / ".install.lock").open("a+")
    deadline = time.monotonic() + wait_seconds
    announced = False
    while True:
        try:
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            return handle
        except BlockingIOError:
            if time.monotonic() >= deadline:
                fail(f"timed out after {wait_seconds:.0f}s waiting for {root}/.install.lock")
            if not announced:
                log("another job is installing the toolchain; waiting for its lock")
                announced = True
            time.sleep(2)


def run(args: list[str], env: dict[str, str], timeout: float | None = None) -> str:
    result = subprocess.run(
        args, env=env, check=True, capture_output=True, text=True, timeout=timeout
    )
    return result.stdout


def run_with_retries(
    args: list[str], env: dict[str, str], backoffs: list[float], what: str
) -> None:
    attempts = len(backoffs) + 1
    for attempt in range(1, attempts + 1):
        try:
            subprocess.run(args, env=env, check=True, timeout=ATTEMPT_TIMEOUT_SECONDS)
            return
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
            log(f"{what} attempt {attempt}/{attempts} failed: {error}")
            if attempt == attempts:
                fail(f"{what} failed after {attempts} attempts")
            delay = backoffs[attempt - 1]
            log(f"retrying in {delay:.0f}s")
            time.sleep(delay)


def tool_env(cargo_home: Path, rustup_home: Path) -> dict[str, str]:
    env = dict(os.environ)
    env["CARGO_HOME"] = str(cargo_home)
    env["RUSTUP_HOME"] = str(rustup_home)
    for name in DROPPED_RUSTUP_ENV:
        env.pop(name, None)
    env.update(OFFICIAL_RUSTUP_ENV)
    return env


def verify_rustup_version(rustup: Path, version: str, env: dict[str, str]) -> None:
    reported = run([str(rustup), "--version"], env).strip()
    if not reported.startswith(f"rustup {version} "):
        fail(f"{rustup} reports {reported!r}, expected rustup {version}")


def ensure_rustup(args, cargo_home: Path, rustup_home: Path, env: dict[str, str]) -> Path:
    """Reuse rustup only when it is the pinned binary; otherwise install the pin.

    rustup-init installs a copy of itself as ``rustup``, so the installed binary
    is compared to the pinned rustup-init SHA-256 directly (no marker file), and
    bumping the pin in the workflow reinstalls it on every host.
    """
    rustup = cargo_home / "bin" / "rustup"
    if rustup.is_file() and not rustup.is_symlink():
        if sha256_file(rustup) == args.rustup_init_sha256:
            verify_rustup_version(rustup, args.rustup_version, env)
            log(f"reusing {rustup}")
            return rustup
        log(f"{rustup} is not the pinned rustup-init {args.rustup_version}; reinstalling")
    remove_tree(cargo_home)
    (Path(args.root) / "rustup.sha256").unlink(missing_ok=True)
    url = args.rustup_init_url or RUSTUP_INIT_URL.format(
        version=args.rustup_version, host=args.host
    )
    with tempfile.TemporaryDirectory(prefix="shipyard-rustup-init-") as temp:
        init = Path(temp) / "rustup-init"
        curl = ["curl", "--fail", "--silent", "--show-error", "--location"]
        if url.startswith("https://"):
            curl += ["--proto", "=https", "--tlsv1.2"]
        run_with_retries(curl + ["--output", str(init), url], env, args.backoff, "rustup-init download")
        actual = sha256_file(init)
        if actual != args.rustup_init_sha256:
            fail(f"rustup-init SHA-256 {actual} != pinned {args.rustup_init_sha256}")
        init.chmod(0o700)
        run([str(init), "-y", "--no-modify-path", "--default-toolchain", "none"], env)
    if not rustup.is_file():
        fail(f"rustup-init did not install {rustup}")
    verify_rustup_version(rustup, args.rustup_version, env)
    run([str(rustup), "set", "auto-self-update", "disable"], env)
    log(f"installed {rustup}")
    return rustup


def verify_versions(bin_dir: Path, toolchain: str, env: dict[str, str]) -> str:
    rustc = run([str(bin_dir / "rustc"), "--version"], env).strip()
    cargo = run([str(bin_dir / "cargo"), "--version"], env).strip()
    if not rustc.startswith(f"rustc {toolchain} "):
        fail(f"{bin_dir / 'rustc'} reports {rustc!r}, expected rustc {toolchain}")
    if not cargo.startswith(f"cargo {toolchain} "):
        fail(f"{bin_dir / 'cargo'} reports {cargo!r}, expected cargo {toolchain}")
    return rustc


def ensure_toolchain(args, rustup: Path, rustup_home: Path, env: dict[str, str]) -> Path:
    name = f"{args.toolchain}-{args.host}"
    toolchain_dir = rustup_home / "toolchains" / name
    bin_dir = toolchain_dir / "bin"
    marker = Path(args.root) / f"toolchain-{name}.sha256"
    if marker.is_file():
        # The marker is written only after a verified, complete install, so a
        # mismatch here is drift or tampering, not a partial install. Never
        # sign with it, and never silently replace it: stop for a human.
        if toolchain_dir.is_dir() and not toolchain_dir.is_symlink() \
                and tree_digest(toolchain_dir) == marker.read_text(encoding="utf-8").strip():
            log(f"reusing {verify_versions(bin_dir, args.toolchain, env)} from {toolchain_dir}")
            return bin_dir
        message = (
            f"{toolchain_dir} changed since it was installed and verified (digest "
            f"mismatch). Refusing to build a signed release with it. Inspect it, "
            f"then remove {toolchain_dir} and {marker} to reinstall."
        )
        annotate("error", message)
        fail(message)
    if toolchain_dir.exists() or toolchain_dir.is_symlink():
        annotate("warning", f"{toolchain_dir} is an incomplete install; reinstalling")
    remove_tree(toolchain_dir)
    (rustup_home / "update-hashes" / name).unlink(missing_ok=True)
    run_with_retries(
        [str(rustup), "toolchain", "install", name, "--profile", "minimal", "--no-self-update"],
        env,
        args.backoff,
        f"rustup toolchain install {name}",
    )
    rustc = verify_versions(bin_dir, args.toolchain, env)
    set_writable(toolchain_dir, False)
    marker.write_text(tree_digest(toolchain_dir) + "\n", encoding="utf-8")
    log(f"installed {rustc} into {toolchain_dir}")
    return bin_dir


def install(args) -> int:
    root = Path(args.root)
    require_private_root(root)
    cargo_home = root / "cargo"
    rustup_home = root / "rustup"
    run_cargo_home = Path(args.run_cargo_home)
    if run_cargo_home == cargo_home or cargo_home in run_cargo_home.parents:
        fail("--run-cargo-home must be outside --root: crates stay per run")
    env = tool_env(cargo_home, rustup_home)
    lock = acquire_lock(root, args.lock_wait)
    try:
        rustup = ensure_rustup(args, cargo_home, rustup_home, env)
        bin_dir = ensure_toolchain(args, rustup, rustup_home, env)
    finally:
        fcntl.flock(lock.fileno(), fcntl.LOCK_UN)
        lock.close()
    remove_tree(run_cargo_home)
    run_cargo_home.mkdir(mode=0o700, parents=True)
    name = f"{args.toolchain}-{args.host}"
    with open(args.github_env, "a", encoding="utf-8") as handle:
        handle.write(f"CARGO_HOME={run_cargo_home}\n")
        handle.write(f"RUSTUP_HOME={rustup_home}\n")
        handle.write(f"RUSTUP_TOOLCHAIN={name}\n")
        handle.write(f"RUSTC={bin_dir / 'rustc'}\n")
    with open(args.github_path, "a", encoding="utf-8") as handle:
        handle.write(f"{bin_dir}\n")
    log(f"toolchain {bin_dir}; per-run CARGO_HOME {run_cargo_home}")
    return 0


def parse_backoff(value: str) -> list[float]:
    return [float(item) for item in value.split(",") if item.strip()]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--root", required=True, help="Fixed per-runner-user toolchain root")
    parser.add_argument("--toolchain", required=True, help="Exact Rust version, e.g. 1.99.1")
    parser.add_argument("--host", required=True, help="Host triple, e.g. aarch64-apple-darwin")
    parser.add_argument("--rustup-version", required=True)
    parser.add_argument("--rustup-init-sha256", required=True)
    parser.add_argument("--run-cargo-home", required=True, help="Fresh per-run CARGO_HOME")
    parser.add_argument("--github-env", required=True)
    parser.add_argument("--github-path", required=True)
    parser.add_argument("--rustup-init-url", help=argparse.SUPPRESS)
    parser.add_argument("--backoff", type=parse_backoff, default=[30.0, 90.0],
                        help="Comma-separated retry delays in seconds")
    parser.add_argument("--lock-wait", type=float, default=LOCK_WAIT_SECONDS)
    return install(parser.parse_args(argv))


if __name__ == "__main__":
    sys.exit(main())
