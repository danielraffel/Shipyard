#!/usr/bin/env python3
"""Fail-closed lifecycle controller for disposable Proxmox review VMs."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import http.client
import json
import math
import os
from pathlib import Path
import shutil
import signal
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
MAX_RESULT_COMMANDS = 32
NETWORKLESS_GUEST_PROBE = """\
import os
import pathlib

interfaces = set(os.listdir('/sys/class/net'))
routes = pathlib.Path('/proc/net/route').read_text(encoding='utf-8').splitlines()[1:]
ipv6_routes = pathlib.Path('/proc/net/ipv6_route').read_text(encoding='utf-8').splitlines()
non_loopback_routes = [line for line in routes if line.split()[0] != 'lo']
non_loopback_ipv6_routes = [line for line in ipv6_routes if line.split()[-1] != 'lo']
if interfaces != {'lo'} or non_loopback_routes or non_loopback_ipv6_routes:
    raise SystemExit(72)
"""
MAX_RESULT_STRING_BYTES = 16 * 1024
FORBIDDEN_CONFIG_PREFIXES = (
    "args", "audio", "hostpci", "ivshmem", "parallel", "tpmstate", "usb", "virtiofs",
)


class Blocked(RuntimeError):
    """A fail-closed admission or infrastructure failure."""


class TeardownBlocked(Blocked):
    """Teardown could not be proven; admission must remain latched closed."""


class ControllerInterrupted(RuntimeError):
    """Graceful service termination; deliberately not a retryable Blocked error."""


def interrupt_for_teardown(signum: int, _frame: object) -> None:
    """Turn graceful service termination into an exception so finally runs."""
    raise ControllerInterrupted(f"controller interrupted by signal {signum}")


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
        timeout: int = 60,
    ) -> object:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
        connection = http.client.HTTPSConnection(self.host, self.port, context=context, timeout=timeout)
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
        "enabled", "proxmox", "template_vmid", "template_name", "template_image_sha256",
        "template_image_manifest", "dependency_inventory_sha256", "dependency_inventory",
        "guest_runner_sha256",
        "job_vmid", "job_bridge",
        "disk_storage", "job_disk_gib", "iso_storage", "pool", "wall_timeout_seconds", "qga_timeout_seconds",
        "result_timeout_seconds", "admission_latch", "admission_lock",
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
    if int(config.get("job_disk_gib", 0)) not in range(1, 81):
        raise Blocked("job disk cap must be 1..80 GiB")
    if not isinstance(config.get("template_name"), str) or not config["template_name"]:
        raise Blocked("golden template name is missing")
    if not re_full_sha256(str(config.get("template_image_sha256", ""))):
        raise Blocked("golden template image identity is invalid")
    if not re_full_sha256(str(config.get("dependency_inventory_sha256", ""))):
        raise Blocked("dependency inventory identity is invalid")
    if not re_full_sha256(str(config.get("guest_runner_sha256", ""))):
        raise Blocked("guest runner SHA-256 is invalid")
    if int(config.get("job_vmid", 0)) == int(config.get("template_vmid", 0)):
        raise Blocked("job VMID must differ from template VMID")
    if int(config.get("wall_timeout_seconds", 0)) not in range(1, 7201):
        raise Blocked("wall timeout must be 1..7200 seconds")
    if int(config.get("result_timeout_seconds", 0)) not in range(1, 61):
        raise Blocked("result timeout must be 1..60 seconds")
    latch = Path(str(config.get("admission_latch", "")))
    if not latch.is_absolute() or latch.name != "admission-latch.json":
        raise Blocked("admission latch path is invalid")
    lock = Path(str(config.get("admission_lock", "")))
    if not lock.is_absolute() or lock.name != "admission.lock" or lock.parent != latch.parent:
        raise Blocked("admission lock path is invalid")


def re_full_sha256(value: str) -> bool:
    return len(value) == 64 and all(character in "0123456789abcdef" for character in value)


def is_nonnegative_number(value: object) -> bool:
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(value)
        and value >= 0
    )


def validate_guest_result(value: object, manifest: dict[str, object]) -> dict[str, object]:
    if not isinstance(value, dict):
        raise Blocked("guest result must be an object")
    required = {
        "schema", "status", "request", "source_sha256", "recipe_sha256",
        "commands", "duration_seconds", "standing_secrets", "network",
    }
    if set(value) != required:
        raise Blocked("guest result has missing or unexpected fields")
    if value.get("request") != manifest["request"]:
        raise Blocked("guest result provenance mismatch")
    if (
        value.get("schema") != 1
        or value.get("source_sha256") != manifest["source_sha256"]
        or value.get("recipe_sha256") != manifest["recipe_sha256"]
        or value.get("status") not in {"pass", "fail"}
        or value.get("standing_secrets") != "none"
        or value.get("network") != "none"
        or not is_nonnegative_number(value.get("duration_seconds"))
    ):
        raise Blocked("guest result attestation fields are incomplete or contradictory")
    commands = value.get("commands")
    if not isinstance(commands, list) or not 1 <= len(commands) <= MAX_RESULT_COMMANDS:
        raise Blocked("guest result command count is invalid")
    allowed = {
        "index", "argv", "cwd", "status", "exit_code", "duration_seconds",
        "log_sha256", "log_bytes", "log_truncated", "log_tail_untrusted",
    }
    required_command = allowed - {"log_tail_untrusted"}
    for expected_index, command in enumerate(commands):
        if (
            not isinstance(command, dict)
            or set(command) - allowed
            or not required_command <= set(command)
        ):
            raise Blocked("guest result command schema is invalid")
        argv = command.get("argv")
        strings = [command.get("cwd"), command.get("log_tail_untrusted", "")]
        if (
            command.get("index") != expected_index
            or not isinstance(argv, list) or not argv
            or not all(isinstance(item, str) and 0 < len(item.encode()) <= 4096 for item in argv)
            or len(argv) > 64
            or not all(isinstance(item, str) and len(item.encode()) <= MAX_RESULT_STRING_BYTES for item in strings)
            or command.get("status") not in {"pass", "fail", "timeout"}
            or not (
                command.get("exit_code") is None
                or isinstance(command.get("exit_code"), int) and not isinstance(command.get("exit_code"), bool)
            )
            or not is_nonnegative_number(command.get("duration_seconds"))
            or not re_full_sha256(str(command.get("log_sha256", "")))
            or not isinstance(command.get("log_bytes"), int)
            or isinstance(command.get("log_bytes"), bool)
            or not 0 <= command["log_bytes"] <= 1024 * 1024
            or not isinstance(command.get("log_truncated"), bool)
        ):
            raise Blocked("guest result command value is invalid")
    command_passes = [
        command["status"] == "pass" and command["exit_code"] == 0
        for command in commands
    ]
    if (value["status"] == "pass") != all(command_passes):
        raise Blocked("guest result status contradicts command outcomes")
    return value


def validate_cloud_init_status(output: object) -> None:
    if not isinstance(output, str) or len(output.encode()) > 64 * 1024:
        raise Blocked("guest initialization status is invalid")
    try:
        value = json.loads(output)
    except json.JSONDecodeError as error:
        raise Blocked("guest initialization status is invalid JSON") from error
    if (
        not isinstance(value, dict)
        or value.get("status") != "done"
        or value.get("extended_status") != "done"
        or value.get("errors") != []
        or value.get("recoverable_errors") != {}
    ):
        raise Blocked("guest initialization did not complete cleanly")


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
    if network_keys:
        failures.append(f"job has a network device: {network_keys}")
    if any(key.startswith("ipconfig") for key in vm):
        failures.append("job has cloud-init network configuration")
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
    if f"size={config['job_disk_gib']}G" not in str(vm.get("scsi0", "")):
        failures.append("job root disk size does not match the hard cap")
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
        image_manifest = Path(str(config["template_image_manifest"]))
        dependency_inventory = Path(str(config["dependency_inventory"]))
        require_root_protected_file(image_manifest)
        require_root_protected_file(dependency_inventory)
        if sha256(image_manifest) != config["template_image_sha256"]:
            raise Blocked("protected template image manifest digest does not match policy")
        if sha256(dependency_inventory) != config["dependency_inventory_sha256"]:
            raise Blocked("protected dependency inventory digest does not match policy")
        self.api = ProxmoxApi(config["proxmox"])
        self.node = str(config["proxmox"]["node"])
        self.vmid = int(config["job_vmid"])
        self.template = int(config["template_vmid"])
        self.iso_name = f"shipyard-review-job-{self.vmid}.iso"
        self.iso_volid = f"{config['iso_storage']}:iso/{self.iso_name}"
        self.created_vm = False
        self.uploaded_iso = False
        self.last_storage_check = 0.0
        self.admission_latch = Path(str(config["admission_latch"]))
        self.admission_lock = Path(str(config["admission_lock"]))
        self.lock_handle: object | None = None

    def acquire_admission_lock(self) -> None:
        self.admission_lock.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(self.admission_lock.parent, 0o700)
        handle = self.admission_lock.open("a+", encoding="utf-8")
        os.chmod(self.admission_lock, 0o600)
        try:
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            handle.close()
            raise Blocked("another untrusted review admission holds the exclusive lock") from error
        self.lock_handle = handle

    def release_admission_lock(self) -> None:
        if self.lock_handle is None:
            return
        handle = self.lock_handle
        self.lock_handle = None
        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
        handle.close()

    def assert_admission_unlatched(self) -> None:
        if self.admission_latch.exists():
            raise Blocked(
                f"admission is latched after unconfirmed teardown; operator reconciliation required: "
                f"{self.admission_latch}"
            )

    def latch_admission(self, reason: str) -> None:
        self.admission_latch.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(self.admission_latch.parent, 0o700)
        temporary = self.admission_latch.with_suffix(".tmp")
        temporary.write_text(
            json.dumps(
                {"schema": 1, "latched_at": int(time.time()), "reason": reason[:4096]},
                sort_keys=True,
            ) + "\n",
            encoding="utf-8",
        )
        os.chmod(temporary, 0o600)
        temporary.replace(self.admission_latch)

    def assert_job_slot_clean(self) -> None:
        guests = self.api.request("GET", f"/nodes/{self.node}/qemu")
        if not isinstance(guests, list):
            raise Blocked("cannot verify clean job slot")
        for guest in guests:
            if not isinstance(guest, dict):
                raise Blocked("cannot verify clean job slot")
            vmid = int(guest.get("vmid", -1))
            if vmid == self.vmid:
                raise Blocked("fixed job VM identity is already present")
            if guest.get("status") != "running" or guest.get("template"):
                continue
            vm = self.api.request("GET", f"/nodes/{self.node}/qemu/{vmid}/config")
            if isinstance(vm, dict) and any(
                f"bridge={self.config['job_bridge']}" in str(value)
                for key, value in vm.items() if key.startswith("net")
            ):
                raise Blocked(f"isolated job bridge already has running guest {vmid}")
        for storage in [str(self.config["disk_storage"]), str(self.config["iso_storage"])]:
            content = self.api.request("GET", f"/nodes/{self.node}/storage/{storage}/content")
            if not isinstance(content, list):
                raise Blocked(f"cannot verify clean job resources on {storage}")
            for item in content:
                if not isinstance(item, dict):
                    raise Blocked(f"cannot verify clean job resources on {storage}")
                volid = str(item.get("volid", ""))
                if f"vm-{self.vmid}-" in volid or volid.endswith(f"/{self.iso_name}"):
                    raise Blocked(f"orphaned job resource is present: {volid}")

    def assert_storage_headroom(self, force: bool = False) -> None:
        now = time.monotonic()
        if not force and now - self.last_storage_check < 5:
            return
        self.last_storage_check = now
        for storage, minimum_free, maximum_used in [
            (
                str(self.config["disk_storage"]),
                (int(self.config["job_disk_gib"]) + 20) * 1024**3,
                0.70,
            ),
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
        description = str(vm.get("description", ""))
        if f"image-manifest-sha256={self.config['template_image_sha256']}" not in description:
            failures.append("template image identity does not match pin")
        if f"dependency-inventory-sha256={self.config['dependency_inventory_sha256']}" not in description:
            failures.append("template dependency identity does not match pin")
        if str(vm.get("hotplug", "")) != "0" or str(vm.get("onboot", "0")) not in {"0", "false", "False"}:
            failures.append("template hotplug/onboot policy is wrong")
        if any(key.startswith(("net", "ipconfig")) for key in vm):
            failures.append("template has a network device or cloud-init network configuration")
        if str(vm.get("sata1", "")) != "none,media=cdrom":
            failures.append("template immutable-input slot is wrong")
        if f"size={self.config['job_disk_gib']}G" not in str(vm.get("scsi0", "")):
            failures.append("template root disk exceeds or differs from the hard cap")
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
        self.acquire_admission_lock()
        try:
            self.assert_admission_unlatched()
            self.assert_job_slot_clean()
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
                    "delete": "net0,ipconfig0",
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
            network_probe = self._guest_exec(
                {"command": ["/usr/bin/python3", "-c", NETWORKLESS_GUEST_PROBE]},
            )
            network_status = self._wait_guest_exec(network_probe, 30)
            if network_status.get("exitcode") != 0:
                raise Blocked("guest has a non-loopback interface or kernel route")
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
            cloud_status = self.api.request(
                "POST", f"/nodes/{self.node}/qemu/{self.vmid}/agent/exec",
                {"command": ["/usr/bin/cloud-init", "status", "--long", "--format", "json"]},
            )
            cloud_result = self._wait_guest_exec(cloud_status, 30)
            if cloud_result.get("exitcode") != 0:
                raise Blocked("guest initialization status command failed")
            validate_cloud_init_status(cloud_result.get("out-data"))
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
            value = self.collect_guest_result(manifest)
            value["controller"] = {
                "boundary": "proxmox-disposable-vm", "vmid": self.vmid,
                "template_vmid": self.template, "teardown": "pending",
                "template_image_sha256": self.config["template_image_sha256"],
                "dependency_inventory_sha256": self.config["dependency_inventory_sha256"],
                "duration_seconds": round(time.time() - started, 3),
            }
            return value
        finally:
            primary_error = sys.exception()
            try:
                self.teardown_or_latch(primary_error)
            finally:
                self.release_admission_lock()

    def collect_guest_result(self, manifest: dict[str, object]) -> dict[str, object]:
        result = self.api.request(
            "GET", f"/nodes/{self.node}/qemu/{self.vmid}/agent/file-read",
            {"file": "/run/shipyard-review/result.json", "count": MAX_RESULT_BYTES, "decode": 1},
            timeout=int(self.config["result_timeout_seconds"]),
        )
        if not isinstance(result, dict) or not isinstance(result.get("content"), str):
            raise Blocked("guest result transport is invalid")
        try:
            decoded = json.loads(result["content"])
        except json.JSONDecodeError as error:
            raise Blocked("guest result is invalid JSON") from error
        return validate_guest_result(decoded, manifest)

    def teardown_or_latch(self, primary_error: BaseException | None) -> None:
        try:
            self.teardown()
        except TeardownBlocked as teardown_error:
            self.latch_admission(str(teardown_error))
            if primary_error is not None:
                raise Blocked(
                    f"job failed: {primary_error}; additionally {teardown_error}"
                ) from teardown_error
            raise

    def reconcile_stranded_job(self) -> None:
        """Adopt only controller-owned fixed resources, destroy, then clear latch."""
        guests = self.api.request("GET", f"/nodes/{self.node}/qemu")
        if not isinstance(guests, list):
            raise Blocked("VM inventory response is invalid")
        if any(isinstance(vm, dict) and vm.get("vmid") == self.vmid for vm in guests):
            vm = self.api.request("GET", f"/nodes/{self.node}/qemu/{self.vmid}/config")
            if not isinstance(vm, dict):
                raise Blocked("stranded job VM config response is invalid")
            tags = set(str(vm.get("tags", "")).split(";"))
            if (
                vm.get("description")
                != "Disposable Shipyard untrusted review job; controller-owned; destroy after run."
                or not {"disposable", "network-deny", "shipyard-review", "untrusted"} <= tags
            ):
                raise Blocked("fixed VMID exists but is not provably a controller-owned review job")
            protection_update = self.api.request(
                "POST", f"/nodes/{self.node}/qemu/{self.vmid}/config", {"protection": 0}
            )
            if isinstance(protection_update, str) and protection_update.startswith("UPID:"):
                self.api.wait_task(protection_update)
            self.created_vm = True
        content = self.api.request(
            "GET", f"/nodes/{self.node}/storage/{self.config['iso_storage']}/content"
        )
        if not isinstance(content, list):
            raise Blocked("ISO inventory response is invalid")
        expected_volid = f"{self.config['iso_storage']}:iso/{self.iso_name}"
        if any(isinstance(item, dict) and item.get("volid") == expected_volid for item in content):
            self.iso_volid = expected_volid
            self.uploaded_iso = True
        self.teardown_or_latch(None)
        self.assert_job_slot_clean()
        self.admission_latch.unlink(missing_ok=True)

    def _guest_exec(self, payload: dict[str, object]) -> object:
        return self.api.request(
            "POST", f"/nodes/{self.node}/qemu/{self.vmid}/agent/exec", payload,
        )

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
        try:
            self.assert_job_slot_clean()
        except Blocked as error:
            errors.append(f"post-delete absence verification failed: {error}")
        if errors:
            raise TeardownBlocked("teardown was not confirmed: " + "; ".join(errors))


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
    subparsers.add_parser("reconcile")
    args = parser.parse_args()
    config = load_json(args.config)
    if not isinstance(config, dict):
        raise Blocked("controller config must be a JSON object")
    validate_config(config)
    require_root_protected_file(args.config)
    lifecycle = ReviewLifecycle(config)
    if args.command == "verify":
        lifecycle.acquire_admission_lock()
        try:
            lifecycle.assert_admission_unlatched()
            lifecycle.assert_job_slot_clean()
            lifecycle.assert_storage_headroom(force=True)
            lifecycle.assert_template()
        finally:
            lifecycle.release_admission_lock()
        print(json.dumps({"status": "ready", "template_vmid": lifecycle.template}, sort_keys=True))
        return 0
    if args.command == "reconcile":
        lifecycle.acquire_admission_lock()
        try:
            lifecycle.reconcile_stranded_job()
        finally:
            lifecycle.release_admission_lock()
        print(json.dumps({"status": "reconciled", "job_vmid": lifecycle.vmid}, sort_keys=True))
        return 0
    request = {"repo": args.repo, "pr": 1, "head_sha": "offline-smoke", "base_sha": "offline-smoke"}
    with tempfile.TemporaryDirectory(prefix="shipyard-review-smoke-") as temp_name:
        temp = Path(temp_name)
        source = temp / "source.tar.gz"
        iso = temp / lifecycle.iso_name
        create_source_archive(args.source_dir.resolve(), source)
        manifest = build_iso(source, args.recipe.resolve(), request, iso)
        previous_handler = signal.signal(signal.SIGTERM, interrupt_for_teardown)
        try:
            result = lifecycle.run(iso, manifest)
        finally:
            signal.signal(signal.SIGTERM, previous_handler)
    # A result is only marked teardown-confirmed after the finally block succeeds.
    result["controller"]["teardown"] = "confirmed"
    print(json.dumps(result, sort_keys=True))
    return 0 if result.get("status") == "pass" else 1


if __name__ == "__main__":
    try:
        raise SystemExit(run_cli())
    except (Blocked, ControllerInterrupted) as error:
        print(json.dumps({"status": "blocked", "reason": str(error)}), file=sys.stderr)
        raise SystemExit(3)
