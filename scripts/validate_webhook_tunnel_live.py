#!/usr/bin/env python3
"""Validate Rust daemon Tailscale Funnel plus GitHub webhook delivery.

Default mode is preflight-only and non-mutating. Pass both ``--apply`` and
``--allow-funnel-reset`` to start the Rust daemon with the real Tailscale
backend, create a transient GitHub webhook through ``gh``, ping it, observe the
daemon IPC event, and clean up.
"""

from __future__ import annotations

import argparse
import json
import os
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_REPO = "danielraffel/Shipyard"
FUNNEL_CAP_KEYS = ("https://tailscale.com/cap/funnel", "funnel")
TAILSCALE_CANDIDATE_BINARIES = (
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/opt/homebrew/bin/tailscale",
    "/usr/local/bin/tailscale",
    "/usr/bin/tailscale",
)


@dataclass(frozen=True)
class Step:
    name: str
    ok: bool
    detail: str

    def to_json(self) -> dict[str, object]:
        return {"name": self.name, "ok": self.ok, "detail": self.detail}


class ValidationError(RuntimeError):
    pass


class ValidationReport:
    def __init__(self) -> None:
        self.steps: list[Step] = []
        self.extra: dict[str, object] = {}

    def add(self, name: str, ok: bool, detail: str) -> None:
        self.steps.append(Step(name, ok, detail))

    @property
    def ok(self) -> bool:
        return all(step.ok for step in self.steps)

    def to_json(self) -> dict[str, object]:
        payload: dict[str, object] = {
            "ok": self.ok,
            "steps": [step.to_json() for step in self.steps],
        }
        payload.update(self.extra)
        return payload


def run(
    args: list[str],
    *,
    cwd: Path = ROOT,
    env: Mapping[str, str] | None = None,
    input_text: str | None = None,
    timeout: float = 30.0,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        args,
        cwd=cwd,
        env=dict(env) if env is not None else None,
        input=input_text,
        check=False,
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def command_output(result: subprocess.CompletedProcess[str]) -> str:
    combined = "\n".join(
        part.strip() for part in (result.stdout, result.stderr) if part.strip()
    )
    return combined or f"exit {result.returncode}"


def load_json_output(result: subprocess.CompletedProcess[str]) -> Any:
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ValidationError(f"could not parse JSON output: {error}") from error


def resolve_binary(path: Path) -> Path:
    if path.exists():
        return path
    raise ValidationError(f"binary does not exist: {path}")


def require_command(name: str) -> str:
    resolved = shutil.which(name)
    if not resolved:
        raise ValidationError(f"{name} is not on PATH")
    return resolved


def executable_file(path: Path) -> bool:
    return path.is_file() and os.access(path, os.X_OK)


def tailscale_candidate_binaries() -> list[str]:
    raw_candidates: list[str] = []
    raw_candidates.extend(TAILSCALE_CANDIDATE_BINARIES)
    path_candidate = shutil.which("tailscale")
    if path_candidate:
        raw_candidates.append(path_candidate)

    candidates: list[str] = []
    seen: set[str] = set()
    for raw in raw_candidates:
        path = Path(raw)
        if not executable_file(path):
            continue
        try:
            key = str(path.resolve())
        except OSError:
            key = str(path)
        if key in seen:
            continue
        seen.add(key)
        candidates.append(str(path))
    return candidates


def require_tailscale_candidates() -> list[str]:
    candidates = tailscale_candidate_binaries()
    if not candidates:
        raise ValidationError("no executable Tailscale binary found")
    return candidates


def gh_auth_status() -> str:
    result = run(["gh", "auth", "status"], timeout=30)
    if result.returncode != 0:
        raise ValidationError(command_output(result))
    return command_output(result)


def gh_api(args: list[str], *, timeout: float = 30.0) -> subprocess.CompletedProcess[str]:
    return run(["gh", "api", *args], timeout=timeout)


def list_hooks(repo: str) -> list[dict[str, Any]]:
    result = gh_api([f"repos/{repo}/hooks", "--jq", "."])
    if result.returncode != 0:
        raise ValidationError(command_output(result))
    hooks = load_json_output(result)
    if not isinstance(hooks, list):
        raise ValidationError("GitHub hooks response was not a list")
    return [hook for hook in hooks if isinstance(hook, dict)]


def delete_hook(repo: str, hook_id: int) -> None:
    gh_api(["-X", "DELETE", f"repos/{repo}/hooks/{hook_id}"], timeout=30)


def hook_url(hook: Mapping[str, Any]) -> str | None:
    config = hook.get("config")
    if not isinstance(config, dict):
        return None
    url = config.get("url")
    return url if isinstance(url, str) else None


def parse_tailscale_status(raw_json: str, binary: str | None) -> dict[str, object]:
    try:
        value = json.loads(raw_json)
    except json.JSONDecodeError:
        return {
            "binary": binary,
            "ready": False,
            "backend_state": None,
            "dns_name": None,
            "funnel_permitted": False,
        }
    if not isinstance(value, dict):
        return {
            "binary": binary,
            "ready": False,
            "backend_state": None,
            "dns_name": None,
            "funnel_permitted": False,
        }
    self_info = value.get("Self")
    self_info = self_info if isinstance(self_info, dict) else {}
    cap_map = self_info.get("CapMap")
    cap_map = cap_map if isinstance(cap_map, dict) else {}
    backend_state = value.get("BackendState")
    dns_name = self_info.get("DNSName")
    funnel_permitted = any(key in cap_map for key in FUNNEL_CAP_KEYS)
    ready = (
        bool(binary)
        and backend_state == "Running"
        and isinstance(dns_name, str)
        and bool(dns_name)
        and funnel_permitted
    )
    return {
        "binary": binary,
        "ready": ready,
        "backend_state": backend_state if isinstance(backend_state, str) else None,
        "dns_name": dns_name if isinstance(dns_name, str) else None,
        "funnel_permitted": funnel_permitted,
    }


def probe_tailscale(binary: str) -> dict[str, object]:
    result = run([binary, "status", "--json"], timeout=20)
    if result.returncode != 0:
        raise ValidationError(with_tailscale_variant_hint(binary, command_output(result)))
    status = parse_tailscale_status(result.stdout, binary)
    if not status["ready"]:
        raise ValidationError(
            with_tailscale_variant_hint(
                binary,
                "Tailscale is not ready for Funnel: "
                f"backend={status['backend_state']} "
                f"dns={status['dns_name']} "
                f"funnel_permitted={status['funnel_permitted']}",
            )
        )
    return status


def probe_tailscale_candidates(candidates: list[str]) -> dict[str, object]:
    failures: list[str] = []
    for candidate in candidates:
        try:
            return probe_tailscale(candidate)
        except ValidationError as error:
            failures.append(f"{candidate}: {error}")
    raise ValidationError("no ready Tailscale binary found; tried " + " | ".join(failures))


def with_tailscale_variant_hint(binary: str, detail: str) -> str:
    try:
        resolved = str(Path(binary).resolve())
    except OSError:
        resolved = binary
    if "Tailscale.app" not in resolved:
        return detail
    return (
        f"{detail}; detected macOS Tailscale app binary at {resolved}. "
        "If this was invoked through a symlink, try the app binary directly; "
        "some App Store builds crash before command dispatch when launched "
        "through a broken CLI shim."
    )


def current_funnel_status(binary: str) -> str:
    result = run([binary, "funnel", "status"], timeout=20)
    return command_output(result)


def funnel_status_is_clear(status: str) -> bool:
    return "No serve config" in status


def summarize_funnel_status(status: str) -> str:
    for line in status.splitlines():
        stripped = line.strip()
        if stripped:
            return stripped
    return status.strip() or "(empty)"


def reset_funnel(binary: str) -> str:
    result = run([binary, "funnel", "reset"], timeout=20)
    if result.returncode != 0:
        raise ValidationError(command_output(result))
    status = current_funnel_status(binary)
    if funnel_status_is_clear(status):
        return status

    # App Store builds can leave the Serve config in place after
    # `funnel reset`; `serve reset` clears the backing proxy.
    result = run([binary, "serve", "reset"], timeout=20)
    if result.returncode != 0:
        raise ValidationError(command_output(result))
    return current_funnel_status(binary)


def daemon_cmd(
    binary: Path,
    state_dir: Path,
    args: list[str],
    *,
    env: Mapping[str, str] | None = None,
    timeout: float = 30.0,
) -> subprocess.CompletedProcess[str]:
    return run(
        [str(binary), "--json", "--state-dir", str(state_dir), *args],
        env=env,
        timeout=timeout,
    )


def daemon_status(binary: Path, state_dir: Path) -> dict[str, Any] | None:
    result = daemon_cmd(binary, state_dir, ["daemon", "status"], timeout=10)
    if result.returncode != 0:
        return None
    status = load_json_output(result)
    return status if isinstance(status, dict) else None


def wait_for_registered_tunnel(
    binary: Path,
    state_dir: Path,
    repo: str,
    timeout: float,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    last: dict[str, Any] | None = None
    while time.monotonic() < deadline:
        status = daemon_status(binary, state_dir)
        if status:
            last = status
            tunnel = status.get("tunnel")
            repos = status.get("registered_repos")
            if (
                isinstance(tunnel, dict)
                and tunnel.get("backend") == "tailscale"
                and isinstance(tunnel.get("url"), str)
                and isinstance(repos, list)
                and repo in repos
            ):
                return status
        time.sleep(1.0)
    raise ValidationError(f"daemon did not expose registered Tailscale tunnel: last={last}")


def read_registrations(state_dir: Path) -> dict[str, int]:
    path = state_dir / "daemon" / "registrations.json"
    try:
        records = json.loads(path.read_text(encoding="utf-8"))
    except OSError as error:
        raise ValidationError(f"could not read {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise ValidationError(f"could not parse {path}: {error}") from error
    if not isinstance(records, list):
        raise ValidationError(f"{path} did not contain a registration list")
    registrations: dict[str, int] = {}
    for record in records:
        if not isinstance(record, dict):
            continue
        repo = record.get("repo")
        hook_id = record.get("hook_id")
        if isinstance(repo, str) and isinstance(hook_id, int):
            registrations[repo] = hook_id
    return registrations


def open_ipc_subscription(socket_path: Path) -> socket.socket:
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(5.0)
    sock.connect(str(socket_path))
    reader = sock.makefile("r", encoding="utf-8")
    hello = json.loads(reader.readline())
    if hello.get("type") != "hello":
        raise ValidationError(f"unexpected daemon hello: {hello}")
    sock.sendall(b'{"type":"subscribe"}\n')
    sock.setblocking(False)
    return sock


def wait_for_ipc_event(sock: socket.socket, timeout: float) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    buffer = b""
    while time.monotonic() < deadline:
        remaining = max(0.0, deadline - time.monotonic())
        readable, _, _ = select.select([sock], [], [], min(1.0, remaining))
        if not readable:
            continue
        try:
            chunk = sock.recv(4096)
        except BlockingIOError:
            continue
        if not chunk:
            raise ValidationError("daemon IPC disconnected before webhook event")
        buffer += chunk
        while b"\n" in buffer:
            raw_line, buffer = buffer.split(b"\n", 1)
            if not raw_line.strip():
                continue
            try:
                message = json.loads(raw_line.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue
            if message.get("type") == "event":
                return message
    raise ValidationError(f"no daemon IPC webhook event observed within {timeout}s")


def probe_public_webhook(url: str) -> str:
    result = run(
        [
            "curl",
            "-sS",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code} %{time_total}",
            "--max-time",
            "10",
            url,
        ],
        timeout=15,
    )
    if result.returncode != 0:
        raise ValidationError(command_output(result))
    parts = result.stdout.strip().split()
    status = parts[0] if parts else ""
    duration = parts[1] if len(parts) > 1 else "unknown"
    if status in {"200", "401", "405"}:
        return f"http={status} duration={duration}s"
    raise ValidationError(
        f"unexpected public webhook HTTP status {status or '<empty>'}: "
        f"{command_output(result)}"
    )


def wait_for_public_webhook(url: str, timeout: float) -> str:
    deadline = time.monotonic() + timeout
    last_error: ValidationError | None = None
    while time.monotonic() < deadline:
        try:
            return probe_public_webhook(url)
        except ValidationError as error:
            last_error = error
            time.sleep(2.0)
    detail = f": last={last_error}" if last_error else ""
    raise ValidationError(f"public webhook was not reachable within {timeout}s{detail}")


def wait_for_ping_event(
    repo: str,
    hook_id: int,
    sock: socket.socket,
    timeout: float,
    *,
    ping_interval: float = 30.0,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    buffer = b""
    ping_attempts = 0
    next_ping_at = 0.0
    while time.monotonic() < deadline:
        now = time.monotonic()
        if now >= next_ping_at:
            ping_hook(repo, hook_id)
            ping_attempts += 1
            next_ping_at = now + ping_interval

        remaining = max(0.0, deadline - time.monotonic())
        readable, _, _ = select.select([sock], [], [], min(1.0, remaining))
        if not readable:
            continue
        try:
            chunk = sock.recv(4096)
        except BlockingIOError:
            continue
        if not chunk:
            raise ValidationError("daemon IPC disconnected before webhook event")
        buffer += chunk
        while b"\n" in buffer:
            raw_line, buffer = buffer.split(b"\n", 1)
            if not raw_line.strip():
                continue
            try:
                message = json.loads(raw_line.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue
            if message.get("type") == "event":
                return message
    raise ValidationError(
        f"no daemon IPC webhook event observed within {timeout}s "
        f"after {ping_attempts} ping attempt(s)"
    )


def ping_hook(repo: str, hook_id: int) -> None:
    result = gh_api(["-X", "POST", f"repos/{repo}/hooks/{hook_id}/pings"], timeout=30)
    if result.returncode != 0:
        raise ValidationError(command_output(result))


def hook_deliveries(repo: str, hook_id: int) -> list[dict[str, Any]]:
    result = gh_api([f"repos/{repo}/hooks/{hook_id}/deliveries", "--jq", "."], timeout=30)
    if result.returncode != 0:
        raise ValidationError(command_output(result))
    deliveries = load_json_output(result)
    if not isinstance(deliveries, list):
        raise ValidationError("GitHub hook deliveries response was not a list")
    return [delivery for delivery in deliveries if isinstance(delivery, dict)]


def hook_delivery_detail(repo: str, hook_id: int, delivery_id: int) -> dict[str, Any]:
    result = gh_api(
        [f"repos/{repo}/hooks/{hook_id}/deliveries/{delivery_id}", "--jq", "."],
        timeout=30,
    )
    if result.returncode != 0:
        raise ValidationError(command_output(result))
    detail = load_json_output(result)
    if not isinstance(detail, dict):
        raise ValidationError("GitHub hook delivery detail response was not an object")
    return detail


def summarize_hook_deliveries(deliveries: list[dict[str, Any]]) -> str:
    if not deliveries:
        return "no deliveries recorded"
    parts: list[str] = []
    for delivery in deliveries[:3]:
        guid = delivery.get("guid", "<unknown>")
        event = delivery.get("event", "<unknown>")
        status = delivery.get("status", "<unknown>")
        code = delivery.get("status_code", "<unknown>")
        delivered_at = delivery.get("delivered_at", "<unknown>")
        duration = delivery.get("duration", "<unknown>")
        parts.append(
            f"guid={guid} event={event} status={status} code={code} "
            f"duration={duration} delivered_at={delivered_at}"
        )
    return "; ".join(parts)


def summarize_hook_delivery_details(repo: str, hook_id: int, deliveries: list[dict[str, Any]]) -> str:
    summaries: list[str] = []
    for delivery in deliveries[:2]:
        delivery_id = delivery.get("id")
        if not isinstance(delivery_id, int):
            continue
        try:
            detail = hook_delivery_detail(repo, hook_id, delivery_id)
        except ValidationError as error:
            summaries.append(f"id={delivery_id} detail-error={error}")
            continue
        response = detail.get("response")
        response = response if isinstance(response, dict) else {}
        request = detail.get("request")
        request = request if isinstance(request, dict) else {}
        summaries.append(
            " ".join(
                part
                for part in (
                    f"id={delivery_id}",
                    f"event={detail.get('event', '<unknown>')}",
                    f"status={detail.get('status', '<unknown>')}",
                    f"status_code={detail.get('status_code', '<unknown>')}",
                    f"duration={detail.get('duration', '<unknown>')}",
                    f"url={request.get('url', '<unknown>')}",
                    f"response_status={response.get('status', '<unknown>')}",
                    f"response_message={response.get('message', '<unknown>')}",
                )
            )
        )
    return "; ".join(summaries) if summaries else "no delivery details available"


def daemon_log_tail(state_dir: Path, max_chars: int = 4000) -> str | None:
    path = state_dir / "daemon" / "daemon.log"
    try:
        content = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    content = content.strip()
    if not content:
        return None
    if len(content) <= max_chars:
        return content
    return content[-max_chars:]


def render_text(report: ValidationReport) -> str:
    lines = [f"ok: {str(report.ok).lower()}"]
    for step in report.steps:
        status = "pass" if step.ok else "fail"
        lines.append(f"{status}: {step.name}: {step.detail}")
    return "\n".join(lines)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default=DEFAULT_REPO)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/shipyard")
    parser.add_argument("--timeout", type=float, default=90.0)
    parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    parser.add_argument(
        "--apply",
        action="store_true",
        help="Perform live mutation: start daemon, create transient hook, ping, clean up.",
    )
    parser.add_argument(
        "--allow-funnel-reset",
        action="store_true",
        help=(
            "Allow the Rust daemon to run its Tailscale backend. The "
            "validator's cleanup calls `tailscale funnel reset`, so only use "
            "when no production Funnel is needed."
        ),
    )
    return parser.parse_args(argv)


def stop_daemon(binary: Path, state_dir: Path) -> None:
    daemon_cmd(binary, state_dir, ["daemon", "stop"], timeout=20)


def cleanup_hooks(repo: str, hook_ids: set[int]) -> None:
    for hook_id in sorted(hook_ids):
        delete_hook(repo, hook_id)


def hook_ids_remaining(repo: str, hook_ids: set[int]) -> list[int]:
    if not hook_ids:
        return []
    try:
        hooks = list_hooks(repo)
    except ValidationError:
        return sorted(hook_ids)
    present = {hook.get("id") for hook in hooks if isinstance(hook.get("id"), int)}
    return [hook_id for hook_id in sorted(hook_ids) if hook_id in present]


def validate(args: argparse.Namespace) -> ValidationReport:
    report = ValidationReport()
    cleanup_hook_ids: set[int] = set()

    try:
        binary = resolve_binary(args.binary)
        report.add("rust binary", True, str(binary))
    except ValidationError as error:
        report.add("rust binary", False, str(error))
        return report

    try:
        tailscale_candidates = require_tailscale_candidates()
        gh = require_command("gh")
        curl = require_command("curl")
        report.add(
            "required commands",
            True,
            f"tailscale candidates={', '.join(tailscale_candidates)}; gh={gh}; curl={curl}",
        )
    except ValidationError as error:
        report.add("required commands", False, str(error))
        return report

    try:
        detail = gh_auth_status().splitlines()[0]
        report.add("gh auth", True, detail)
    except ValidationError as error:
        report.add("gh auth", False, str(error))
        return report

    try:
        existing_hooks = list_hooks(args.repo)
        report.add("github hooks read", True, f"{len(existing_hooks)} existing hook(s)")
    except ValidationError as error:
        report.add("github hooks read", False, str(error))
        return report

    try:
        status = probe_tailscale_candidates(tailscale_candidates)
        tailscale = str(status["binary"])
        report.add(
            "tailscale readiness",
            True,
            f"binary={tailscale} dns={status['dns_name']} "
            f"funnel_permitted={status['funnel_permitted']}",
        )
    except ValidationError as error:
        report.add("tailscale readiness", False, str(error))
        return report

    funnel_status = current_funnel_status(tailscale)
    report.add("tailscale funnel status", True, summarize_funnel_status(funnel_status))

    if not args.apply:
        report.add(
            "live mutation",
            True,
            "skipped; rerun with --apply --allow-funnel-reset for end-to-end delivery",
        )
        return report

    if not args.allow_funnel_reset:
        report.add(
            "funnel reset approval",
            False,
            "--apply requires --allow-funnel-reset because the backend resets local Funnel",
        )
        return report
    if not funnel_status_is_clear(funnel_status):
        report.add(
            "pre-existing funnel config",
            False,
            "refusing to reset an existing Funnel/Serve config; stop the active "
            "Shipyard daemon or rerun in a verified reset window",
        )
        return report

    with tempfile.TemporaryDirectory(prefix="shipyard-webhook-") as tempdir:
        state_dir = Path(tempdir)
        env = dict(os.environ)
        env["SHIPYARD_ENABLE_TUNNEL"] = "1"
        start = daemon_cmd(
            binary,
            state_dir,
            ["daemon", "start", "--repo", args.repo],
            env=env,
            timeout=30,
        )
        if start.returncode != 0:
            report.add("daemon start", False, command_output(start))
            return report
        report.add("daemon start", True, command_output(start).splitlines()[0])

        hook_id: int | None = None
        try:
            status = wait_for_registered_tunnel(binary, state_dir, args.repo, args.timeout)
            tunnel = status["tunnel"]
            assert isinstance(tunnel, dict)
            public_url = str(tunnel["url"]).rstrip("/") + "/webhook"
            report.add("daemon tunnel registration", True, public_url)
            ingress = wait_for_public_webhook(public_url, min(args.timeout, 120.0))
            report.add("public webhook ingress", True, ingress)

            registrations = read_registrations(state_dir)
            hook_id = registrations.get(args.repo)
            if hook_id is None:
                raise ValidationError(f"no registration stored for {args.repo}")
            cleanup_hook_ids.add(hook_id)
            hooks = list_hooks(args.repo)
            registered = next((hook for hook in hooks if hook.get("id") == hook_id), None)
            if not registered:
                raise ValidationError(f"GitHub hook {hook_id} was not found")
            if hook_url(registered) != public_url:
                raise ValidationError(
                    f"GitHub hook URL mismatch: {hook_url(registered)} != {public_url}"
                )
            report.add("github hook registration", True, f"id={hook_id}")

            socket_path = state_dir / "daemon" / "daemon.sock"
            sock = open_ipc_subscription(socket_path)
            try:
                event = wait_for_ping_event(args.repo, hook_id, sock, args.timeout)
                if event.get("kind") != "unhandled":
                    raise ValidationError(f"unexpected ping event shape: {event}")
                report.add("github webhook delivery", True, json.dumps(event, sort_keys=True))
            finally:
                sock.close()
        except ValidationError as error:
            if hook_id is not None:
                try:
                    deliveries = hook_deliveries(args.repo, hook_id)
                    report.add(
                        "github hook deliveries",
                        True,
                        summarize_hook_deliveries(deliveries),
                    )
                    report.add(
                        "github hook delivery details",
                        True,
                        summarize_hook_delivery_details(args.repo, hook_id, deliveries),
                    )
                except ValidationError as delivery_error:
                    report.add("github hook deliveries", False, str(delivery_error))
            log_tail = daemon_log_tail(state_dir)
            if log_tail:
                report.add("daemon log tail", True, log_tail)
            report.add("live webhook validation", False, str(error))
        finally:
            stop_daemon(binary, state_dir)
            try:
                cleanup_status = reset_funnel(tailscale)
                report.add(
                    "tailscale funnel cleanup",
                    True,
                    summarize_funnel_status(cleanup_status),
                )
            except ValidationError as error:
                report.add("tailscale funnel cleanup", False, str(error))
            cleanup_hooks(args.repo, cleanup_hook_ids)
            remaining = hook_ids_remaining(args.repo, cleanup_hook_ids)
            if cleanup_hook_ids:
                report.add(
                    "github hook cleanup",
                    not remaining,
                    "removed transient hook(s)"
                    if not remaining
                    else f"still present: {remaining}",
                )

    return report


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    report = validate(args)
    if args.json:
        print(json.dumps(report.to_json(), indent=2, sort_keys=True))
    else:
        print(render_text(report))
    return 0 if report.ok else 2


if __name__ == "__main__":
    raise SystemExit(main())
