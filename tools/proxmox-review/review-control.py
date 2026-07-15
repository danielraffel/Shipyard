#!/usr/bin/env python3
"""Fail-closed lifecycle controller for disposable Proxmox review VMs."""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import shutil
import ssl
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.parse
import uuid

MAX_SOURCE_BYTES = 64 * 1024 * 1024
MAX_ISO_BYTES = 80 * 1024 * 1024
MAX_RESULT_BYTES = 256 * 1024
FORBIDDEN_CONFIG_PREFIXES = (
    "args", "audio", "hostpci", "ivshmem", "parallel", "tpmstate", "usb", "virtiofs",
)


class Blocked(RuntimeError):
    """A fail-closed admission or infrastructure failure."""


def sha256(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def load_json(path: Path) -> object:
    return json.loads(path.read_text(encoding="utf-8"))


def require_private_file(path: Path) -> None:
    if path.is_symlink() or not path.is_file():
        raise Blocked(f"secret path is missing, non-regular, or a symlink: {path}")
    stat = path.stat()
    if stat.st_mode & 0o077:
        raise Blocked(f"secret file permissions must be 0600 or stricter: {path}")
    if stat.st_uid != os.geteuid():
        raise Blocked(f"secret file is not owned by the controller user: {path}")
    parent = path.parent.stat()
    if parent.st_uid != os.geteuid() or parent.st_mode & 0o077:
        raise Blocked(f"secret directory must be service-owned with mode 0700: {path.parent}")


def require_root_protected_file(path: Path) -> None:
    if path.is_symlink() or not path.is_file():
        raise Blocked(f"protected file is missing, non-regular, or a symlink: {path}")
    stat = path.stat()
    if stat.st_uid != 0 or stat.st_mode & 0o022:
        raise Blocked(f"protected file must be root-owned and not group/other-writable: {path}")


class ProxmoxApi:
    """Small PVE REST client with an explicit TLS certificate pin."""

    def __init__(self, config: dict[str, object]):
        self.host = str(config["host"])
        self.port = int(config.get("port", 8006))
        self.node = str(config["node"])
        self.pin = str(config["tls_sha256"]).replace(":", "").lower()
        token_path = Path(str(config["token_file"]))
        require_private_file(token_path)
        self.token = token_path.read_text(encoding="utf-8").strip()
        if not self.token.startswith("PVEAPIToken=") or "=" not in self.token[12:]:
            raise Blocked("Proxmox token file has invalid format")

    def request(
        self,
        method: str,
        path: str,
        fields: dict[str, object] | None = None,
        upload: tuple[str, Path] | None = None,
    ) -> object:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
        connection = http.client.HTTPSConnection(self.host, self.port, context=context, timeout=60)
        headers = {"Authorization": self.token, "Accept": "application/json"}
        body: bytes | None = None
        target = "/api2/json" + path
        if upload:
            field_name, file_path = upload
            boundary = "shipyard-" + uuid.uuid4().hex
            parts: list[bytes] = []
            for key, value in (fields or {}).items():
                parts.append(
                    f"--{boundary}\r\nContent-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n".encode()
                )
            file_bytes = file_path.read_bytes()
            parts.append(
                f"--{boundary}\r\nContent-Disposition: form-data; name=\"{field_name}\"; filename=\"{file_path.name}\"\r\nContent-Type: application/octet-stream\r\n\r\n".encode()
                + file_bytes + b"\r\n"
            )
            parts.append(f"--{boundary}--\r\n".encode())
            body = b"".join(parts)
            headers["Content-Type"] = f"multipart/form-data; boundary={boundary}"
        elif fields:
            encoded_fields = urllib.parse.urlencode(fields, doseq=True)
            if method in {"GET", "DELETE"}:
                target += "?" + encoded_fields
            else:
                body = encoded_fields.encode()
                headers["Content-Type"] = "application/x-www-form-urlencoded"
        connection.connect()
        certificate = connection.sock.getpeercert(binary_form=True) if connection.sock else None
        if not certificate or hashlib.sha256(certificate).hexdigest() != self.pin:
            connection.close()
            raise Blocked("Proxmox TLS certificate pin mismatch")
        connection.request(method, target, body=body, headers=headers)
        response = connection.getresponse()
        payload = response.read(8 * 1024 * 1024 + 1)
        status = response.status
        connection.close()
        if len(payload) > 8 * 1024 * 1024:
            raise Blocked("Proxmox API response exceeded limit")
        try:
            decoded = json.loads(payload)
        except json.JSONDecodeError as error:
            raise Blocked(f"Proxmox API {method} {path} returned invalid JSON ({status})") from error
        if not 200 <= status < 300:
            message = decoded.get("message", "request failed") if isinstance(decoded, dict) else "request failed"
            if isinstance(decoded, dict) and isinstance(decoded.get("errors"), dict):
                message += ": " + json.dumps(decoded["errors"], sort_keys=True)
            raise Blocked(f"Proxmox API {method} {path} failed ({status}): {message}")
        if not isinstance(decoded, dict) or "data" not in decoded:
            raise Blocked("Proxmox API response is missing data")
        return decoded["data"]

    def wait_task(self, upid: str, timeout: int = 300) -> None:
        deadline = time.monotonic() + timeout
        encoded = urllib.parse.quote(upid, safe="")
        while time.monotonic() < deadline:
            status = self.request("GET", f"/nodes/{self.node}/tasks/{encoded}/status")
            if isinstance(status, dict) and status.get("status") == "stopped":
                if status.get("exitstatus") != "OK":
                    raise Blocked(f"Proxmox task failed: {status.get('exitstatus', 'unknown')}")
                return
            time.sleep(1)
        raise Blocked("Proxmox task timed out")


def validate_config(config: dict[str, object]) -> None:
    expected = {
        "enabled", "proxmox", "template_vmid", "template_name", "guest_runner_sha256",
        "job_vmid", "job_bridge", "job_ip",
        "disk_storage", "iso_storage", "pool", "wall_timeout_seconds", "qga_timeout_seconds",
    }
    extra = set(config) - expected
    if extra:
        raise Blocked(f"controller config has unexpected keys: {sorted(extra)}")
    if config.get("enabled") is not True:
        raise Blocked("untrusted review lane is disabled")
    if not isinstance(config.get("proxmox"), dict):
        raise Blocked("proxmox configuration is missing")
    if config.get("job_bridge") != "vmbr1":
        raise Blocked("untrusted job bridge must be vmbr1")
    if config.get("disk_storage") != "local-lvm":
        raise Blocked("untrusted job disk storage must be local-lvm")
    if not isinstance(config.get("template_name"), str) or not config["template_name"]:
        raise Blocked("golden template name is missing")
    if not re_full_sha256(str(config.get("guest_runner_sha256", ""))):
        raise Blocked("guest runner SHA-256 is invalid")
    if int(config.get("job_vmid", 0)) == int(config.get("template_vmid", 0)):
        raise Blocked("job VMID must differ from template VMID")
    if int(config.get("wall_timeout_seconds", 0)) not in range(1, 7201):
        raise Blocked("wall timeout must be 1..7200 seconds")


def re_full_sha256(value: str) -> bool:
    return len(value) == 64 and all(character in "0123456789abcdef" for character in value)


def validate_job_vm_config(vm: dict[str, object], config: dict[str, object], iso_name: str) -> None:
    failures: list[str] = []
    if vm.get("template"):
        failures.append("clone is still a template")
    if str(vm.get("onboot", "0")) not in {"0", "False", "false"}:
        failures.append("onboot is enabled")
    if str(vm.get("protection", "0")) not in {"0", "False", "false"}:
        failures.append("job VM is protected and cannot be guaranteed to tear down")
    if int(vm.get("cores", 0)) > 2 or int(vm.get("memory", 0)) > 4096:
        failures.append("resource cap exceeds policy")
    if str(vm.get("hotplug", "")) != "0":
        failures.append("hotplug is not disabled")
    network_keys = sorted(key for key in vm if key.startswith("net"))
    if network_keys != ["net0"] or f"bridge={config['job_bridge']}" not in str(vm.get("net0", "")):
        failures.append("job has an unexpected network device or bridge")
    ipconfig = str(vm.get("ipconfig0", ""))
    if ipconfig != f"ip={config['job_ip']}" or "gw=" in ipconfig:
        failures.append(f"job IP configuration includes a gateway or is unexpected: {ipconfig!r}")
    for key in vm:
        if key.startswith(FORBIDDEN_CONFIG_PREFIXES):
            failures.append(f"forbidden device/config field present: {key}")
    if "sshkeys" in vm or "cipassword" in vm or "nameserver" in vm or "searchdomain" in vm:
        failures.append("job received an interactive credential or resolver")
    sata = str(vm.get("sata1", ""))
    if iso_name not in sata or "media=cdrom" not in sata:
        failures.append(f"immutable input ISO is not attached as CD-ROM: {sata!r}")
    for disk in ["scsi0", "efidisk0", "ide2"]:
        if not str(vm.get(disk, "")).startswith(f"{config['disk_storage']}:"):
            failures.append(f"job disk {disk} is not on the expected storage")
    if failures:
        raise Blocked("job VM admission failed: " + "; ".join(failures))


def build_iso(source: Path, recipe: Path, request: dict[str, object], destination: Path) -> dict[str, object]:
    if source.stat().st_size > MAX_SOURCE_BYTES:
        raise Blocked("source archive exceeds limit")
    recipe_value = load_json(recipe)
    if not isinstance(recipe_value, dict) or set(recipe_value) != {"schema", "commands"}:
        raise Blocked("protected recipe has invalid schema")
    request_fields = {"repo", "pr", "head_sha", "base_sha"}
    if set(request) != request_fields or not all(request[field] for field in request_fields):
        raise Blocked("request provenance is incomplete")
    manifest = {
        "schema": 1,
        "source_sha256": sha256(source),
        "recipe_sha256": sha256(recipe),
        "request": request,
    }
    with tempfile.TemporaryDirectory(prefix="shipyard-review-iso-") as temp_name:
        temp = Path(temp_name)
        shutil.copyfile(source, temp / "source.tar.gz")
        shutil.copyfile(recipe, temp / "recipe.json")
        (temp / "manifest.json").write_text(
            json.dumps(manifest, sort_keys=True, separators=(",", ":")) + "\n", encoding="utf-8"
        )
        completed = subprocess.run(
            [
                "xorriso", "-as", "mkisofs", "-quiet", "-r", "-J",
                "-V", "SHIPYARD_REVIEW", "-o", str(destination),
                str(temp / "manifest.json"), str(temp / "recipe.json"), str(temp / "source.tar.gz"),
            ],
            check=False, capture_output=True, text=True, timeout=120,
        )
        if completed.returncode != 0:
            raise Blocked(f"failed to construct immutable input ISO: {completed.stderr[-1000:]}")
    if destination.stat().st_size > MAX_ISO_BYTES:
        destination.unlink(missing_ok=True)
        raise Blocked("input ISO exceeds limit")
    return manifest


class ReviewLifecycle:
    def __init__(self, config: dict[str, object]):
        validate_config(config)
        self.config = config
        self.api = ProxmoxApi(config["proxmox"])
        self.node = str(config["proxmox"]["node"])
        self.vmid = int(config["job_vmid"])
        self.template = int(config["template_vmid"])
        self.iso_name = f"shipyard-review-job-{self.vmid}.iso"
        self.iso_volid = f"{config['iso_storage']}:iso/{self.iso_name}"
        self.created_vm = False
        self.uploaded_iso = False
        self.last_storage_check = 0.0

    def assert_storage_headroom(self, force: bool = False) -> None:
        now = time.monotonic()
        if not force and now - self.last_storage_check < 5:
            return
        self.last_storage_check = now
        for storage, minimum_free, maximum_used in [
            (str(self.config["disk_storage"]), 20 * 1024**3, 0.70),
            (str(self.config["iso_storage"]), 1024**3, 0.95),
        ]:
            value = self.api.request("GET", f"/nodes/{self.node}/storage/{storage}/status")
            if not isinstance(value, dict) or not value.get("active") or not value.get("enabled"):
                raise Blocked(f"required storage is unavailable: {storage}")
            total = int(value.get("total", 0))
            used = int(value.get("used", total))
            available = int(value.get("avail", 0))
            if total <= 0 or available < minimum_free or used / total >= maximum_used:
                raise Blocked(f"storage headroom policy blocks execution: {storage}")

    def assert_template(self) -> None:
        vm = self.api.request("GET", f"/nodes/{self.node}/qemu/{self.template}/config")
        if not isinstance(vm, dict):
            raise Blocked("golden template config response is invalid")
        failures: list[str] = []
        if not vm.get("template") or not vm.get("protection"):
            failures.append("template/protection flags are missing")
        if vm.get("name") != self.config["template_name"]:
            failures.append("template name does not match pin")
        if str(vm.get("hotplug", "")) != "0" or str(vm.get("onboot", "0")) not in {"0", "false", "False"}:
            failures.append("template hotplug/onboot policy is wrong")
        if sorted(key for key in vm if key.startswith("net")) != ["net0"] or "bridge=vmbr1" not in str(vm.get("net0", "")):
            failures.append("template network boundary is wrong")
        if str(vm.get("sata1", "")) != "none,media=cdrom":
            failures.append("template immutable-input slot is wrong")
        if any(key.startswith(FORBIDDEN_CONFIG_PREFIXES) for key in vm):
            failures.append("template has a forbidden device/config field")
        if any(key in vm for key in ["sshkeys", "cipassword", "nameserver", "searchdomain"]):
            failures.append("template has a credential or resolver")
        if failures:
            raise Blocked("golden template admission failed: " + "; ".join(failures))

    def _task(self, method: str, path: str, fields: dict[str, object] | None = None) -> None:
        result = self.api.request(method, path, fields)
        if not isinstance(result, str) or not result.startswith("UPID:"):
            raise Blocked("expected asynchronous Proxmox task identifier")
        self.api.wait_task(result)

    def run(self, iso: Path, manifest: dict[str, object]) -> dict[str, object]:
        if iso.name != self.iso_name:
            raise Blocked(f"ISO must be named {self.iso_name}")
        started = time.time()
        try:
            self.assert_storage_headroom(force=True)
            self.assert_template()
            self.api.request(
                "POST", f"/nodes/{self.node}/storage/{self.config['iso_storage']}/upload",
                {"content": "iso", "checksum-algorithm": "sha256", "checksum": sha256(iso)},
                upload=("filename", iso),
            )
            self.uploaded_iso = True
            self._task(
                "POST", f"/nodes/{self.node}/qemu/{self.template}/clone",
                {
                    "newid": self.vmid, "name": f"shipyard-review-job-{self.vmid}",
                    "pool": self.config["pool"], "full": 0,
                    "description": "Disposable Shipyard untrusted review job; controller-owned; destroy after run.",
                },
            )
            self.created_vm = True
            # Protected templates produce protected clones. Remove protection in
            # a dedicated call before any other mutation so teardown remains
            # possible if a later admission step fails.
            protection_update = self.api.request(
                "POST", f"/nodes/{self.node}/qemu/{self.vmid}/config", {"protection": 0},
            )
            if isinstance(protection_update, str) and protection_update.startswith("UPID:"):
                self.api.wait_task(protection_update)
            config_update = self.api.request(
                "POST", f"/nodes/{self.node}/qemu/{self.vmid}/config",
                {
                    "tags": "disposable;network-deny;shipyard-review;untrusted",
                    "ipconfig0": f"ip={self.config['job_ip']}",
                    "sata1": f"{self.iso_volid},media=cdrom",
                },
            )
            if isinstance(config_update, str) and config_update.startswith("UPID:"):
                self.api.wait_task(config_update)
            vm = self.api.request("GET", f"/nodes/{self.node}/qemu/{self.vmid}/config")
            if not isinstance(vm, dict):
                raise Blocked("job VM config response is invalid")
            validate_job_vm_config(vm, self.config, self.iso_name)
            self._task("POST", f"/nodes/{self.node}/qemu/{self.vmid}/status/start")
            deadline = time.monotonic() + int(self.config["qga_timeout_seconds"])
            while True:
                try:
                    self.api.request("POST", f"/nodes/{self.node}/qemu/{self.vmid}/agent/ping")
                    break
                except Blocked:
                    if time.monotonic() >= deadline:
                        raise Blocked("guest agent did not become ready")
                    time.sleep(2)
            admission_deadline = time.monotonic() + int(self.config["qga_timeout_seconds"])
            while True:
                admission = self.api.request(
                    "POST", f"/nodes/{self.node}/qemu/{self.vmid}/agent/exec",
                    {"command": ["/usr/bin/test", "-f", "/run/shipyard-review/unprivileged-ready"]},
                )
                admission_status = self._wait_guest_exec(admission, 30)
                if admission_status.get("exitcode") == 0:
                    break
                if time.monotonic() >= admission_deadline:
                    raise Blocked("guest hardening admission marker is missing")
                time.sleep(2)
            group_check = self.api.request(
                "POST", f"/nodes/{self.node}/qemu/{self.vmid}/agent/exec",
                {"command": ["/usr/bin/id", "-nG", "shipyard"]},
            )
            group_status = self._wait_guest_exec(group_check, 30)
            if group_status.get("exitcode") != 0 or str(group_status.get("out-data", "")).strip() != "shipyard":
                raise Blocked("guest user retains unexpected supplementary groups")
            for key_path in ["/root/.ssh/authorized_keys", "/home/shipyard/.ssh/authorized_keys"]:
                key_check = self.api.request(
                    "POST", f"/nodes/{self.node}/qemu/{self.vmid}/agent/exec",
                    {"command": ["/usr/bin/test", "!", "-s", key_path]},
                )
                key_status = self._wait_guest_exec(key_check, 30)
                if key_status.get("exitcode") != 0:
                    raise Blocked(f"guest image contains an SSH authorization key: {key_path}")
            runner_check = self.api.request(
                "POST", f"/nodes/{self.node}/qemu/{self.vmid}/agent/exec",
                {"command": ["/usr/bin/sha256sum", "/usr/local/sbin/shipyard-review-guest-runner"]},
            )
            runner_status = self._wait_guest_exec(runner_check, 30)
            runner_digest = str(runner_status.get("out-data", "")).split(maxsplit=1)[0]
            if runner_status.get("exitcode") != 0 or runner_digest != self.config["guest_runner_sha256"]:
                raise Blocked("guest runner digest does not match protected policy")
            execution = self.api.request(
                "POST", f"/nodes/{self.node}/qemu/{self.vmid}/agent/exec",
                {"command": ["/usr/local/sbin/shipyard-review-guest-runner"]},
            )
            self._wait_guest_exec(execution, int(self.config["wall_timeout_seconds"]))
            result = self.api.request(
                "GET", f"/nodes/{self.node}/qemu/{self.vmid}/agent/file-read",
                {"file": "/run/shipyard-review/result.json", "count": MAX_RESULT_BYTES, "decode": 1},
            )
            if not isinstance(result, dict) or not isinstance(result.get("content"), str):
                raise Blocked("guest result transport is invalid")
            value = json.loads(result["content"])
            if not isinstance(value, dict) or value.get("request") != manifest["request"]:
                detail = value.get("reason", value.get("status", "unknown")) if isinstance(value, dict) else "invalid result"
                raise Blocked(f"guest result provenance mismatch: {str(detail)[:1000]}")
            if (
                value.get("schema") != 1
                or value.get("source_sha256") != manifest["source_sha256"]
                or value.get("recipe_sha256") != manifest["recipe_sha256"]
                or value.get("status") not in {"pass", "fail"}
                or value.get("standing_secrets") != "none"
                or value.get("network") != "none"
                or not isinstance(value.get("commands"), list)
            ):
                raise Blocked("guest result attestation fields are incomplete or contradictory")
            value["controller"] = {
                "boundary": "proxmox-disposable-vm", "vmid": self.vmid,
                "template_vmid": self.template, "teardown": "pending",
                "duration_seconds": round(time.time() - started, 3),
            }
            return value
        finally:
            primary_error = sys.exception()
            try:
                self.teardown()
            except Blocked as teardown_error:
                if primary_error is not None:
                    raise Blocked(
                        f"job failed: {primary_error}; additionally {teardown_error}"
                    ) from teardown_error
                raise

    def _wait_guest_exec(self, response: object, timeout: int) -> dict[str, object]:
        if not isinstance(response, dict) or not isinstance(response.get("pid"), int):
            raise Blocked("guest exec did not return a pid")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.assert_storage_headroom()
            status = self.api.request(
                "GET", f"/nodes/{self.node}/qemu/{self.vmid}/agent/exec-status",
                {"pid": response["pid"]},
            )
            if isinstance(status, dict) and status.get("exited"):
                if status.get("exitcode") not in {0, None}:
                    # The structured result distinguishes test failure from infrastructure failure.
                    return status
                return status
            time.sleep(1)
        raise Blocked("guest command exceeded wall timeout")

    def teardown(self) -> None:
        errors: list[str] = []
        if self.created_vm:
            try:
                status = self.api.request("GET", f"/nodes/{self.node}/qemu/{self.vmid}/status/current")
                if isinstance(status, dict) and status.get("status") != "stopped":
                    self._task("POST", f"/nodes/{self.node}/qemu/{self.vmid}/status/stop", {"timeout": 30})
            except Blocked as error:
                errors.append(str(error))
            try:
                self._task(
                    "DELETE", f"/nodes/{self.node}/qemu/{self.vmid}",
                    {"purge": 1, "destroy-unreferenced-disks": 1},
                )
                self.created_vm = False
            except Blocked as error:
                errors.append(str(error))
        if self.uploaded_iso:
            encoded = urllib.parse.quote(self.iso_volid, safe="")
            try:
                self.api.request("DELETE", f"/nodes/{self.node}/storage/{self.config['iso_storage']}/content/{encoded}")
                self.uploaded_iso = False
            except Blocked as error:
                errors.append(str(error))
        if errors:
            raise Blocked("teardown was not confirmed: " + "; ".join(errors))


def create_source_archive(source_dir: Path, destination: Path) -> None:
    with tarfile.open(destination, "w:gz", format=tarfile.PAX_FORMAT) as archive:
        archive.add(source_dir, arcname="source", recursive=True)


def run_cli() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", type=Path, required=True)
    subparsers = parser.add_subparsers(dest="command", required=True)
    verify = subparsers.add_parser("verify")
    verify.add_argument("--iso-name", default="shipyard-review-job-200.iso")
    smoke = subparsers.add_parser("smoke")
    smoke.add_argument("--source-dir", type=Path, required=True)
    smoke.add_argument("--recipe", type=Path, required=True)
    smoke.add_argument("--repo", default="local/offline-smoke")
    args = parser.parse_args()
    config = load_json(args.config)
    if not isinstance(config, dict):
        raise Blocked("controller config must be a JSON object")
    validate_config(config)
    require_root_protected_file(args.config)
    lifecycle = ReviewLifecycle(config)
    if args.command == "verify":
        lifecycle.assert_storage_headroom(force=True)
        lifecycle.assert_template()
        print(json.dumps({"status": "ready", "template_vmid": lifecycle.template}, sort_keys=True))
        return 0
    request = {"repo": args.repo, "pr": 1, "head_sha": "offline-smoke", "base_sha": "offline-smoke"}
    with tempfile.TemporaryDirectory(prefix="shipyard-review-smoke-") as temp_name:
        temp = Path(temp_name)
        source = temp / "source.tar.gz"
        iso = temp / lifecycle.iso_name
        create_source_archive(args.source_dir.resolve(), source)
        manifest = build_iso(source, args.recipe.resolve(), request, iso)
        result = lifecycle.run(iso, manifest)
    # A result is only marked teardown-confirmed after the finally block succeeds.
    result["controller"]["teardown"] = "confirmed"
    print(json.dumps(result, sort_keys=True))
    return 0 if result.get("status") == "pass" else 1


if __name__ == "__main__":
    try:
        raise SystemExit(run_cli())
    except Blocked as error:
        print(json.dumps({"status": "blocked", "reason": str(error)}), file=sys.stderr)
        raise SystemExit(3)
