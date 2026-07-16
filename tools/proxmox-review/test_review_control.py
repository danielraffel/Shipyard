#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import random
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
from unittest import mock


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
RUNNER = load("guest_runner", "guest-runner.py")
DEPVERIFY = load("dependency_inventory", "verify-dependency-inventory.py")


class PolicyTests(unittest.TestCase):
    def config(self, enabled: bool = True) -> dict[str, object]:
        return {
            "enabled": enabled,
            "proxmox": {
                "host": "192.168.86.70", "port": 8006, "node": "nexus",
                "tls_sha256": "00" * 32, "token_file": "/missing",
            },
            "template_vmid": 127, "template_name": "shipyard-review-template-v9",
            "template_image_sha256": "f" * 64,
            "template_image_manifest": "/etc/shipyard-review/images/template.json",
            "dependency_inventory_sha256": "b" * 64,
            "dependency_inventory": "/etc/shipyard-review/dependencies/pulp-linux.json",
            "guest_runner_sha256": "a" * 64, "job_vmid": 200, "job_bridge": "vmbr1",
            "disk_storage": "local-lvm",
            "job_disk_gib": 80,
            "iso_storage": "shipyard-review-iso",
            "pool": "shipyard-review-jobs",
            "admission_latch": "/var/lib/shipyard-review/admission-latch.json",
            "admission_lock": "/var/lib/shipyard-review/admission.lock",
            "wall_timeout_seconds": 3600,
            "qga_timeout_seconds": 180,
            "result_timeout_seconds": 30,
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
            "sata1": "local:iso/shipyard-review-job-200.iso,media=cdrom",
            "scsi0": "local-lvm:vm-200-disk-1,size=80G", "efidisk0": "local-lvm:vm-200-disk-0",
            "ide2": "local-lvm:vm-200-cloudinit,media=cdrom",
        }
        CONTROL.validate_job_vm_config(vm, config, "shipyard-review-job-200.iso")
        for key, value in [
            ("net0", "virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr1,firewall=1"),
            ("ipconfig0", "ip=10.77.0.200/24"),
            ("net1", "virtio=AA:BB:CC:DD:EE:00,bridge=vmbr0"),
            ("hostpci0", "0000:00:02.0"),
            ("sshkeys", "attacker-key"),
            ("nameserver", "192.168.86.1"),
        ]:
            hostile = dict(vm)
            hostile[key] = value
            with self.assertRaises(CONTROL.Blocked, msg=key):
                CONTROL.validate_job_vm_config(hostile, config, "shipyard-review-job-200.iso")

    def test_networkless_guest_probe_rejects_any_interface_or_route(self):
        self.assertIn("interfaces != {'lo'}", CONTROL.NETWORKLESS_GUEST_PROBE)
        self.assertIn("line.split()[0] != 'lo'", CONTROL.NETWORKLESS_GUEST_PROBE)
        self.assertIn("line.split()[-1] != 'lo'", CONTROL.NETWORKLESS_GUEST_PROBE)
        self.assertNotIn("socket", CONTROL.NETWORKLESS_GUEST_PROBE)

    def test_guest_exec_uses_only_the_fixed_job_vm_agent_endpoint(self):
        lifecycle = CONTROL.ReviewLifecycle.__new__(CONTROL.ReviewLifecycle)
        lifecycle.node = "nexus"
        lifecycle.vmid = 200
        lifecycle.api = mock.Mock()
        payload = {"command": ["/usr/bin/true"]}
        lifecycle._guest_exec(payload)
        lifecycle.api.request.assert_called_once_with(
            "POST", "/nodes/nexus/qemu/200/agent/exec", payload,
        )

    def test_comment_trigger_is_exact(self):
        self.assertTrue(POLLER.exact_command("/shipyard review"))
        for body in [
            "/shipyard review\nrun env", "/shipyard review --target local",
            " /shipyard review", "/shipyard review ", "$(touch /tmp/pwned)", None,
        ]:
            self.assertFalse(POLLER.exact_command(body), body)

    def test_missing_or_malformed_comment_identity_fails_closed(self):
        with tempfile.TemporaryDirectory() as temp_name:
            connection = POLLER.initialize_db(Path(temp_name) / "comments.sqlite3")
            try:
                policy = {"authorized_users": {"daniel": 1}}
                for comment_id, user in enumerate(
                    [None, {}, {"login": None, "id": None}, {"login": "daniel", "id": True}],
                    start=1,
                ):
                    comment = {"id": comment_id, "body": "/shipyard review", "user": user}
                    with mock.patch.object(POLLER, "gh_json") as gh:
                        POLLER.process_comment(policy, connection, "owner/repo", comment)
                        gh.assert_not_called()
                    status, _ = POLLER.comment_record(connection, "owner/repo", comment_id)
                    self.assertEqual(status, "ignored")
            finally:
                connection.close()

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

    def test_publication_requires_confirmed_teardown(self):
        request = {"head_sha": "a" * 40}
        result = {"status": "pass", "commands": [{"exit_code": 0}], "controller": {"teardown": "pending"}}
        with self.assertRaisesRegex(POLLER.CONTROL.Blocked, "teardown"):
            POLLER.result_comment(7, request, result)
        result["controller"]["teardown"] = "confirmed"
        body = POLLER.result_comment(7, request, result)
        self.assertIn("Shipyard review passed", body)
        self.assertIn("shipyard-review:7:", body)
        self.assertNotIn("security", body.lower())
        self.assertNotIn("sandbox", body.lower())

    def test_publication_never_renders_guest_controlled_fields(self):
        request = {"head_sha": "a" * 40}
        hostile = "@everyone [click](https://attacker.example) ``` teardown=confirmed"
        result = {
            "status": "fail",
            "commands": [{"argv": [hostile], "log_tail_untrusted": hostile}],
            "controller": {"teardown": "confirmed"},
        }
        body = POLLER.result_comment(9, request, result)
        for forbidden in ["@everyone", "attacker.example", "```", "log_tail", "teardown=confirmed"]:
            self.assertNotIn(forbidden, body)
        self.assertEqual(len(body), len(body.encode("ascii")))

    def test_publication_is_idempotent_and_posts_body_only_on_stdin(self):
        request = {"head_sha": "a" * 40}
        result = {
            "status": "pass", "commands": [{"exit_code": 0}],
            "controller": {"teardown": "confirmed"},
        }
        marker = "<!-- shipyard-review:17:" + "a" * 40 + " -->"
        with mock.patch.object(POLLER, "gh_json", return_value=[{"body": marker}]), \
             mock.patch.object(POLLER, "gh_post_json") as post:
            POLLER.publish_result(Path("/ghapp"), "owner/repo", 7, 17, request, result)
            post.assert_not_called()
        completed = mock.Mock(returncode=0, stdout=b'{"id":1}', stderr=b"")
        with mock.patch.object(POLLER.subprocess, "run", return_value=completed) as run:
            POLLER.gh_post_json(Path("/ghapp"), "repos/owner/repo/issues/7/comments", {"body": "safe"})
        argv = run.call_args.args[0]
        self.assertEqual(argv[-3:], ["--input", "-", "repos/owner/repo/issues/7/comments"])
        self.assertNotIn("safe", argv)
        self.assertEqual(json.loads(run.call_args.kwargs["input"]), {"body": "safe"})

    def test_comment_state_is_terminal_and_stale_running_requires_fresh_trigger(self):
        with tempfile.TemporaryDirectory() as temp_name:
            connection = POLLER.initialize_db(Path(temp_name) / "comments.sqlite3")
            try:
                comment = {"id": 3, "body": "/shipyard review", "user": {"login": "daniel", "id": 1}}
                policy = {"authorized_users": {"daniel": 1}}
                for status in ["ignored", "completed", "blocked"]:
                    POLLER.record(connection, "owner/repo", 3, status, "terminal")
                    with mock.patch.object(POLLER, "gh_json") as gh:
                        POLLER.process_comment(policy, connection, "owner/repo", comment)
                        gh.assert_not_called()
                POLLER.record(connection, "owner/repo", 3, "running", "old")
                connection.execute(
                    "UPDATE comments SET updated_at = ? WHERE repo = ? AND comment_id = ?",
                    (int(time.time()) - POLLER.RUNNING_STALE_SECONDS - 1, "owner/repo", 3),
                )
                connection.commit()
                POLLER.process_comment(policy, connection, "owner/repo", comment)
                status, _ = POLLER.comment_record(connection, "owner/repo", 3)
                self.assertEqual(status, "blocked")
            finally:
                connection.close()

    def test_archive_filter_drops_escaping_links(self):
        with tempfile.TemporaryDirectory() as temp_name:
            root = Path(temp_name)
            payload = root / "payload"
            payload.write_text("ok", encoding="utf-8")
            absolute = tarfile.TarInfo("repo/absolute-link")
            absolute.type = tarfile.SYMTYPE
            absolute.linkname = "/Users/example/secret"
            escaping = tarfile.TarInfo("repo/escaping-link")
            escaping.type = tarfile.SYMTYPE
            escaping.linkname = "../../secret"
            self.assertIsNone(RUNNER.safe_tar_filter(absolute, str(root)))
            self.assertIsNone(RUNNER.safe_tar_filter(escaping, str(root)))

    def lifecycle_for_state(self, root: Path):
        lifecycle = CONTROL.ReviewLifecycle.__new__(CONTROL.ReviewLifecycle)
        lifecycle.admission_latch = root / "admission-latch.json"
        lifecycle.admission_lock = root / "admission.lock"
        lifecycle.lock_handle = None
        return lifecycle

    def test_admission_lock_is_exclusive_and_releasable(self):
        with tempfile.TemporaryDirectory() as temp_name:
            root = Path(temp_name) / "state"
            first = self.lifecycle_for_state(root)
            second = self.lifecycle_for_state(root)
            first.acquire_admission_lock()
            try:
                with self.assertRaisesRegex(CONTROL.Blocked, "exclusive lock"):
                    second.acquire_admission_lock()
            finally:
                first.release_admission_lock()
            second.acquire_admission_lock()
            second.release_admission_lock()

    def test_teardown_latch_persists_until_operator_removes_it(self):
        with tempfile.TemporaryDirectory() as temp_name:
            lifecycle = self.lifecycle_for_state(Path(temp_name) / "state")
            lifecycle.assert_admission_unlatched()
            lifecycle.latch_admission("destroy failed")
            with self.assertRaisesRegex(CONTROL.Blocked, "latched"):
                lifecycle.assert_admission_unlatched()
            value = json.loads(lifecycle.admission_latch.read_text(encoding="utf-8"))
            self.assertEqual(value["schema"], 1)
            self.assertEqual(value["reason"], "destroy failed")
            self.assertEqual(lifecycle.admission_latch.stat().st_mode & 0o777, 0o600)

    def test_clean_slot_rejects_fixed_vm_bridge_guest_and_orphan(self):
        class FakeApi:
            def __init__(self, guests, configs=None, content=None):
                self.guests = guests
                self.configs = configs or {}
                self.content = content or {}

            def request(self, method, path, fields=None):
                if path == "/nodes/nexus/qemu":
                    return self.guests
                if path.endswith("/config"):
                    return self.configs[int(path.split("/")[-2])]
                if "/storage/" in path and path.endswith("/content"):
                    return self.content.get(path.split("/")[-2], [])
                raise AssertionError(path)

        lifecycle = CONTROL.ReviewLifecycle.__new__(CONTROL.ReviewLifecycle)
        lifecycle.node = "nexus"
        lifecycle.vmid = 200
        lifecycle.iso_name = "shipyard-review-job-200.iso"
        lifecycle.config = {
            "job_bridge": "vmbr1", "disk_storage": "local-lvm",
            "job_disk_gib": 80,
            "iso_storage": "shipyard-review-iso",
        }
        lifecycle.api = FakeApi([])
        lifecycle.assert_job_slot_clean()
        lifecycle.api = FakeApi([{"vmid": 200, "status": "stopped"}])
        with self.assertRaisesRegex(CONTROL.Blocked, "VM identity"):
            lifecycle.assert_job_slot_clean()
        lifecycle.api = FakeApi(
            [{"vmid": 201, "status": "running"}],
            {201: {"net0": "virtio=AA:BB,bridge=vmbr1"}},
        )
        with self.assertRaisesRegex(CONTROL.Blocked, "bridge"):
            lifecycle.assert_job_slot_clean()
        lifecycle.api = FakeApi(
            [], content={"local-lvm": [{"volid": "local-lvm:vm-200-disk-0"}]},
        )
        with self.assertRaisesRegex(CONTROL.Blocked, "orphaned"):
            lifecycle.assert_job_slot_clean()

    def test_guest_result_schema_accepts_known_good_and_rejects_hostile_shapes(self):
        request = {"repo": "owner/repo", "pr": 7, "head_sha": "a" * 40, "base_sha": "b" * 40}
        manifest = {"request": request, "source_sha256": "c" * 64, "recipe_sha256": "d" * 64}
        command = {
            "index": 0, "argv": ["cmake", "--build", "build"], "cwd": ".",
            "status": "pass", "exit_code": 0, "duration_seconds": 1.5,
            "log_sha256": "e" * 64, "log_bytes": 12, "log_truncated": False,
        }
        result = {
            "schema": 1, "status": "pass", "request": request,
            "source_sha256": "c" * 64, "recipe_sha256": "d" * 64,
            "commands": [command], "duration_seconds": 2.0,
            "standing_secrets": "none", "network": "none",
        }
        self.assertIs(CONTROL.validate_guest_result(result, manifest), result)
        cases = []
        extra = dict(result)
        extra["teardown"] = "confirmed"
        cases.append(extra)
        oversized = dict(result)
        oversized["commands"] = [dict(command, log_tail_untrusted="x" * 16385)]
        cases.append(oversized)
        forged = dict(result)
        forged["commands"] = [dict(command, index=9)]
        cases.append(forged)
        nested = dict(result)
        nested["commands"] = [dict(command, surprise={"deep": ["payload"]})]
        cases.append(nested)
        boolean_integer = dict(result)
        boolean_integer["commands"] = [dict(command, exit_code=True)]
        cases.append(boolean_integer)
        contradictory = dict(result)
        contradictory["commands"] = [dict(command, status="fail", exit_code=1)]
        cases.append(contradictory)
        nonfinite = dict(result)
        nonfinite["duration_seconds"] = float("nan")
        cases.append(nonfinite)
        for hostile in cases:
            with self.assertRaises(CONTROL.Blocked):
                CONTROL.validate_guest_result(hostile, manifest)

    def test_archive_download_is_exact_sha_only_without_git_resolution(self):
        with tempfile.TemporaryDirectory() as temp_name:
            root = Path(temp_name)
            ghapp = root / "ghapp"
            ghapp.write_text("stub", encoding="utf-8")
            destination = root / "source.tar.gz"
            completed = mock.Mock(returncode=0, stderr=b"")
            sha = "a" * 40
            with mock.patch.object(POLLER.subprocess, "run", return_value=completed) as run:
                POLLER.gh_archive(ghapp, "owner/repo", sha, destination)
            argv = run.call_args.args[0]
            self.assertEqual(
                argv,
                [str(ghapp), "api", "--method", "GET", "-H",
                 "Accept: application/vnd.github+json", f"repos/owner/repo/tarball/{sha}"],
            )
            self.assertNotIn("git", argv)
            for hostile in ["../repo", "owner/repo?path=.gitmodules"]:
                with self.assertRaisesRegex(POLLER.CONTROL.Blocked, "provenance"):
                    POLLER.gh_archive(ghapp, hostile, sha, destination)
            with self.assertRaisesRegex(POLLER.CONTROL.Blocked, "provenance"):
                POLLER.gh_archive(ghapp, "owner/repo", "HEAD", destination)

    def test_clean_cloud_init_is_required_for_admission(self):
        clean = json.dumps({
            "status": "done", "extended_status": "done",
            "errors": [], "recoverable_errors": {},
        })
        CONTROL.validate_cloud_init_status(clean)
        for hostile in [
            "not-json",
            json.dumps({"status": "done", "extended_status": "degraded done", "errors": [], "recoverable_errors": {"DEPRECATED": ["user"]}}),
            json.dumps({"status": "done", "extended_status": "done", "errors": ["failed"], "recoverable_errors": {}}),
        ]:
            with self.assertRaises(CONTROL.Blocked):
                CONTROL.validate_cloud_init_status(hostile)

    def test_teardown_requires_independent_post_delete_absence(self):
        lifecycle = CONTROL.ReviewLifecycle.__new__(CONTROL.ReviewLifecycle)
        lifecycle.created_vm = False
        lifecycle.uploaded_iso = False
        lifecycle.assert_job_slot_clean = mock.Mock()
        lifecycle.teardown()
        lifecycle.assert_job_slot_clean.assert_called_once_with()
        lifecycle.assert_job_slot_clean.side_effect = CONTROL.Blocked("orphaned job resource")
        with self.assertRaisesRegex(CONTROL.TeardownBlocked, "post-delete absence"):
            lifecycle.teardown()

    def test_teardown_failure_durably_latches_future_admission(self):
        with tempfile.TemporaryDirectory() as temp_name:
            lifecycle = self.lifecycle_for_state(Path(temp_name) / "state")
            lifecycle.teardown = mock.Mock(side_effect=CONTROL.TeardownBlocked("delete failed"))
            with self.assertRaises(CONTROL.TeardownBlocked):
                lifecycle.teardown_or_latch(None)
            self.assertTrue(lifecycle.admission_latch.is_file())
            with self.assertRaisesRegex(CONTROL.Blocked, "latched"):
                lifecycle.assert_admission_unlatched()

    def test_graceful_termination_becomes_teardown_exception(self):
        with self.assertRaisesRegex(CONTROL.ControllerInterrupted, "signal 15"):
            CONTROL.interrupt_for_teardown(15, None)
        self.assertFalse(issubclass(CONTROL.ControllerInterrupted, CONTROL.Blocked))

    def test_reconcile_deletes_only_controller_owned_fixed_resources_then_clears_latch(self):
        with tempfile.TemporaryDirectory() as temp_name:
            lifecycle = self.lifecycle_for_state(Path(temp_name) / "state")
            lifecycle.admission_latch.parent.mkdir(parents=True)
            lifecycle.admission_latch.write_text("{}", encoding="utf-8")
            lifecycle.node = "nexus"
            lifecycle.vmid = 200
            lifecycle.iso_name = "shipyard-review-job-200.iso"
            lifecycle.iso_volid = "shipyard-review-iso:iso/shipyard-review-job-200.iso"
            lifecycle.config = {"iso_storage": "shipyard-review-iso"}
            lifecycle.created_vm = False
            lifecycle.uploaded_iso = False
            lifecycle.api = mock.Mock()
            lifecycle.api.request.side_effect = [
                [{"vmid": 200}],
                {
                    "description": "Disposable Shipyard untrusted review job; controller-owned; destroy after run.",
                    "tags": "disposable;network-deny;shipyard-review;untrusted",
                },
                None,
                [{"volid": lifecycle.iso_volid}],
            ]
            lifecycle.teardown_or_latch = mock.Mock()
            lifecycle.assert_job_slot_clean = mock.Mock()
            lifecycle.reconcile_stranded_job()
            self.assertTrue(lifecycle.created_vm)
            self.assertTrue(lifecycle.uploaded_iso)
            self.assertEqual(
                lifecycle.api.request.call_args_list[2],
                mock.call("POST", "/nodes/nexus/qemu/200/config", {"protection": 0}),
            )
            lifecycle.teardown_or_latch.assert_called_once_with(None)
            lifecycle.assert_job_slot_clean.assert_called_once_with()
            self.assertFalse(lifecycle.admission_latch.exists())

    def test_reconcile_refuses_unknown_fixed_vmid_and_keeps_latch(self):
        with tempfile.TemporaryDirectory() as temp_name:
            lifecycle = self.lifecycle_for_state(Path(temp_name) / "state")
            lifecycle.admission_latch.parent.mkdir(parents=True)
            lifecycle.admission_latch.write_text("{}", encoding="utf-8")
            lifecycle.node = "nexus"
            lifecycle.vmid = 200
            lifecycle.config = {"iso_storage": "shipyard-review-iso"}
            lifecycle.api = mock.Mock()
            lifecycle.api.request.side_effect = [
                [{"vmid": 200}],
                {"description": "someone else's VM", "tags": "production"},
            ]
            with self.assertRaisesRegex(CONTROL.Blocked, "not provably"):
                lifecycle.reconcile_stranded_job()
            self.assertTrue(lifecycle.admission_latch.exists())

    def test_storage_headroom_blocks_low_space_and_accepts_healthy_control(self):
        class StorageApi:
            def __init__(self, available):
                self.available = available

            def request(self, method, path, fields=None):
                total = 200 * 1024**3
                return {"active": 1, "enabled": 1, "total": total, "used": total - self.available, "avail": self.available}

        lifecycle = CONTROL.ReviewLifecycle.__new__(CONTROL.ReviewLifecycle)
        lifecycle.config = {"disk_storage": "local-lvm", "job_disk_gib": 80, "iso_storage": "shipyard-review-iso"}
        lifecycle.node = "nexus"
        lifecycle.last_storage_check = 0.0
        lifecycle.api = StorageApi(120 * 1024**3)
        lifecycle.assert_storage_headroom(force=True)
        lifecycle.api = StorageApi(512 * 1024**2)
        with self.assertRaisesRegex(CONTROL.Blocked, "headroom"):
            lifecycle.assert_storage_headroom(force=True)

    def test_process_death_releases_kernel_admission_lock(self):
        with tempfile.TemporaryDirectory() as temp_name:
            root = Path(temp_name) / "state"
            root.mkdir()
            lock = root / "admission.lock"
            script = (
                "import fcntl, pathlib, sys, time; "
                "h=pathlib.Path(sys.argv[1]).open('a+'); "
                "fcntl.flock(h.fileno(), fcntl.LOCK_EX); "
                "print('READY', flush=True); time.sleep(60)"
            )
            child = subprocess.Popen(
                [sys.executable, "-c", script, str(lock)],
                stdout=subprocess.PIPE, text=True,
            )
            self.assertEqual(child.stdout.readline().strip(), "READY")
            lifecycle = self.lifecycle_for_state(root)
            with self.assertRaises(CONTROL.Blocked):
                lifecycle.acquire_admission_lock()
            child.kill()
            child.wait(timeout=5)
            child.stdout.close()
            lifecycle.acquire_admission_lock()
            lifecycle.release_admission_lock()

    def test_guest_command_timeout_fails_closed(self):
        lifecycle = CONTROL.ReviewLifecycle.__new__(CONTROL.ReviewLifecycle)
        with self.assertRaisesRegex(CONTROL.Blocked, "wall timeout"):
            lifecycle._wait_guest_exec({"pid": 7}, 0)

    def test_result_collection_has_independent_deadline_and_fails_closed(self):
        lifecycle = CONTROL.ReviewLifecycle.__new__(CONTROL.ReviewLifecycle)
        lifecycle.node = "nexus"
        lifecycle.vmid = 200
        lifecycle.config = {"result_timeout_seconds": 17}
        lifecycle.api = mock.Mock()
        lifecycle.api.request.side_effect = CONTROL.Blocked("result read timed out")
        with self.assertRaisesRegex(CONTROL.Blocked, "timed out"):
            lifecycle.collect_guest_result({})
        self.assertEqual(lifecycle.api.request.call_args.kwargs["timeout"], 17)
        self.assertEqual(
            lifecycle.api.request.call_args.args[2],
            {"file": "/run/shipyard-review/result.json", "count": CONTROL.MAX_RESULT_BYTES, "decode": 1},
        )

    def test_malformed_recipe_is_rejected_before_iso_construction(self):
        with tempfile.TemporaryDirectory() as temp_name:
            root = Path(temp_name)
            source = root / "source.tar.gz"
            source.write_bytes(b"source")
            recipe = root / "recipe.json"
            recipe.write_text(json.dumps({"schema": 1, "commands": [], "attacker": True}), encoding="utf-8")
            with self.assertRaisesRegex(CONTROL.Blocked, "recipe"):
                CONTROL.build_iso(
                    source, recipe,
                    {"repo": "owner/repo", "pr": 1, "head_sha": "a" * 40, "base_sha": "b" * 40},
                    root / "job.iso",
                )

    def test_hostile_result_fuzz_corpus_fails_only_as_blocked(self):
        rng = random.Random(6115)
        manifest = {
            "request": {"repo": "owner/repo", "pr": 7, "head_sha": "a" * 40, "base_sha": "b" * 40},
            "source_sha256": "c" * 64, "recipe_sha256": "d" * 64,
        }
        atoms = [None, True, False, -1, 0, 1, "", "x" * 20000]
        for _ in range(250):
            payload = rng.choice(atoms + [[], {}, [rng.choice(atoms)], {"schema": rng.choice(atoms)}])
            try:
                CONTROL.validate_guest_result(payload, manifest)
            except CONTROL.Blocked:
                continue
            self.fail(f"hostile fuzz payload unexpectedly validated: {payload!r}")

    def test_dependency_inventory_accepts_exact_content_and_rejects_extra_cache(self):
        with tempfile.TemporaryDirectory() as temp_name:
            root = Path(temp_name)
            dependencies = root / "deps"
            source = dependencies / "example-src"
            source.mkdir(parents=True)
            subprocess.run(["git", "init", "-q", str(source)], check=True)
            subprocess.run(["git", "-C", str(source), "config", "user.name", "Test"], check=True)
            subprocess.run(["git", "-C", str(source), "config", "user.email", "test@example.invalid"], check=True)
            (source / "file.txt").write_text("reviewed\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(source), "add", "file.txt"], check=True)
            subprocess.run(["git", "-C", str(source), "commit", "-qm", "fixture"], check=True)
            commit = DEPVERIFY.git_output(source, "rev-parse", "HEAD").decode().strip()
            archive_digest = DEPVERIFY.archive_sha256(source)
            inventory = root / "inventory.json"
            inventory.write_text(json.dumps({
                "schema": 1,
                "policy": {
                    "controller_fetch": "forbidden", "controller_cache_warming": "forbidden",
                    "guest_network": "none", "missing_dependency": "fail-closed",
                },
                "baked_sources": [{
                    "name": "example", "path": "example-src", "commit": commit,
                    "git_archive_sha256": archive_digest,
                }],
            }), encoding="utf-8")
            DEPVERIFY.verify(inventory, dependencies)
            (dependencies / "unreviewed-src").mkdir()
            with self.assertRaisesRegex(DEPVERIFY.VerificationError, "set mismatch"):
                DEPVERIFY.verify(inventory, dependencies)


if __name__ == "__main__":
    unittest.main()
