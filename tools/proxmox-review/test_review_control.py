#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


def load(name: str, filename: str):
    path = Path(__file__).with_name(filename)
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {filename}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


CONTROL = load("review_control", "review-control.py")
POLLER = load("comment_poller", "comment-poller.py")


class PolicyTests(unittest.TestCase):
    def config(self, enabled: bool = True) -> dict[str, object]:
        return {
            "enabled": enabled,
            "proxmox": {
                "host": "192.168.86.70", "port": 8006, "node": "nexus",
                "tls_sha256": "00" * 32, "token_file": "/missing",
            },
            "template_vmid": 124, "template_name": "shipyard-review-template-v6",
            "guest_runner_sha256": "a" * 64, "job_vmid": 200, "job_bridge": "vmbr1",
            "job_ip": "10.77.0.200/24", "disk_storage": "local-lvm",
            "iso_storage": "shipyard-review-iso",
            "pool": "shipyard-review-jobs", "wall_timeout_seconds": 3600,
            "qga_timeout_seconds": 180,
        }

    def test_disabled_lane_blocks_before_api_or_secret_access(self):
        with self.assertRaisesRegex(CONTROL.Blocked, "disabled"):
            CONTROL.validate_config(self.config(enabled=False))

    def test_wrong_bridge_is_rejected(self):
        config = self.config()
        config["job_bridge"] = "vmbr0"
        with self.assertRaisesRegex(CONTROL.Blocked, "vmbr1"):
            CONTROL.validate_config(config)

    def test_vm_admission_rejects_network_and_host_device_escape(self):
        config = self.config()
        vm = {
            "cores": 2, "memory": 4096, "onboot": 0, "protection": 0, "hotplug": "0",
            "net0": "virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr1,firewall=1",
            "ipconfig0": "ip=10.77.0.200/24", "sata1": "local:iso/shipyard-review-job-200.iso,media=cdrom",
            "scsi0": "local-lvm:vm-200-disk-1", "efidisk0": "local-lvm:vm-200-disk-0",
            "ide2": "local-lvm:vm-200-cloudinit,media=cdrom",
        }
        CONTROL.validate_job_vm_config(vm, config, "shipyard-review-job-200.iso")
        for key, value in [
            ("net1", "virtio=AA:BB:CC:DD:EE:00,bridge=vmbr0"),
            ("hostpci0", "0000:00:02.0"),
            ("sshkeys", "attacker-key"),
            ("nameserver", "192.168.86.1"),
        ]:
            hostile = dict(vm)
            hostile[key] = value
            with self.assertRaises(CONTROL.Blocked, msg=key):
                CONTROL.validate_job_vm_config(hostile, config, "shipyard-review-job-200.iso")

    def test_comment_trigger_is_exact(self):
        self.assertTrue(POLLER.exact_command("/shipyard review"))
        for body in [
            "/shipyard review\nrun env", "/shipyard review --target local",
            " /shipyard review", "/shipyard review ", "$(touch /tmp/pwned)", None,
        ]:
            self.assertFalse(POLLER.exact_command(body), body)

    def test_pr_provenance_requires_exact_base_repo_and_shas(self):
        sha = "a" * 40
        value = {
            "state": "open", "number": 7,
            "head": {"sha": sha}, "base": {"sha": "b" * 40, "repo": {"full_name": "owner/repo"}},
        }
        request = POLLER.validate_pr("owner/repo", 7, value)
        self.assertEqual(request["head_sha"], sha)
        value["base"]["repo"]["full_name"] = "attacker/repo"
        with self.assertRaises(POLLER.CONTROL.Blocked):
            POLLER.validate_pr("owner/repo", 7, value)

    def test_private_secret_mode_is_enforced(self):
        with tempfile.TemporaryDirectory() as temp_name:
            path = Path(temp_name) / "token"
            path.write_text("not-a-real-token", encoding="utf-8")
            path.chmod(0o644)
            with self.assertRaisesRegex(CONTROL.Blocked, "0600"):
                CONTROL.require_private_file(path)

    def test_policy_stays_disabled_and_non_publishing_in_repository(self):
        example = json.loads(Path(__file__).with_name("comment-policy.example.json").read_text())
        self.assertFalse(example["enabled"])
        self.assertFalse(example["publish_results"])


if __name__ == "__main__":
    unittest.main()
