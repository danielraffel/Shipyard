#!/usr/bin/env python3
from __future__ import annotations

import re
import unittest
from pathlib import Path
import sys


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import package_release  # noqa: E402


RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"
AUTO_RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "auto-release.yml"
VERSION_AT_LAND_WORKFLOW = ROOT / ".github" / "workflows" / "version-at-land.yml"
POST_TAG_SYNC_WORKFLOW = ROOT / ".github" / "workflows" / "post-tag-sync.yml"
ATTEST = "uses: actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6 # v4.2.2"
CHECKOUT = "actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09 # v5.1.0"


class ReleaseWorkflowTests(unittest.TestCase):
    def test_release_workflow_keeps_macos_local_signing_gate(self) -> None:
        text = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("draft: true", text)
        self.assertIn("macOS release artifacts are intentionally not uploaded", text)
        self.assertIn("scripts/release-macos-local.sh", text)
        self.assertIn("--ci-mode", text)
        self.assertIn("CI_MACOS_SIGNING_ENABLED == 'true'", text)

    def test_release_is_gated_on_the_live_github_smoke(self) -> None:
        text = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("  live-smoke:\n    needs: build\n", text)
        self.assertIn("  release:\n    needs: [build, live-smoke]\n", text)
        self.assertIn("scripts/release_live_smoke.py --binary", text)
        self.assertIn("name: release-linux-x64", text)
        self.assertIn("Generous-Corp/pulp", text)
        live = text.split("  live-smoke:", 1)[1].split("\n  release:", 1)[0]
        self.assertNotIn("--apply", live)
        self.assertNotIn("continue-on-error", live)
        self.assertIn("    permissions:\n      contents: read\n", live)
        self.assertNotIn("write", live.split("steps:", 1)[0])

    def test_release_workflow_uses_current_ci_signing_secret_names_only(self) -> None:
        text = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("MACOS_SIGN_P12_BASE64", text)
        self.assertIn("MACOS_SIGN_P12_PASSWORD", text)
        self.assertIn("MACOS_NOTARIZE_APPLE_ID", text)
        self.assertIn("MACOS_NOTARIZE_APP_PASSWORD", text)
        self.assertIn("MACOS_NOTARIZE_TEAM_ID", text)
        self.assertNotIn("secrets.APPLE_ID", text)
        self.assertNotIn("secrets.SHIPYARD_NOTARIZE_APPLE_ID", text)
        self.assertNotIn("SIGNING_CERTIFICATE", text)

    def test_release_workflow_resolves_ci_signing_identity_by_team_id(self) -> None:
        text = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        self.assertIn('SIGN_TEAM_ID: ${{ secrets.MACOS_NOTARIZE_TEAM_ID }}', text)
        self.assertIn("required secret missing when CI_MACOS_SIGNING_ENABLED=true", text)
        self.assertIn('awk -v team="(${SIGN_TEAM_ID})"', text)
        self.assertIn('/Developer ID Application/ && index($0, team)', text)
        self.assertIn("no Developer ID Application identity for Team ID", text)

    def test_release_workflow_always_restores_keychain_state(self) -> None:
        text = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("ci_macos_signing_keychain.py prepare", text)
        self.assertIn("ci_macos_signing_keychain.py restore", text)
        self.assertIn("Restore user keychains and delete ephemeral signing keychain", text)
        self.assertIn("if: always()", text)
        self.assertIn("SHIPYARD_SIGNING_KEYCHAIN_STATE", text)
        self.assertIn("SHIPYARD_SIGNING_HOME", text)
        self.assertIn("SHIPYARD_SIGNING_KEYCHAIN_READY=1", text)
        self.assertIn("apple-tool:,apple:,codesign:", text)
        self.assertIn("shipyard-ci-home-${GITHUB_RUN_ID}", text)
        self.assertNotIn("existing_keychains", text)
        self.assertNotIn('security default-keychain -s "$KEYCHAIN"', text)

    def test_release_workflow_covers_release_platform_set(self) -> None:
        text = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("linux_arm64_runner_selector_json", text)
        self.assertIn("NAMESPACE_LINUX_ARM64_RUNS_ON_JSON", text)
        self.assertIn("linux-arm64|macos-arm64", text)
        self.assertIn("linux-x64|windows-x64", text)
        self.assertNotIn("macos-x64", text)

    def test_release_workflow_builds_and_packages_the_binary_pair(self) -> None:
        text = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        self.assertIn(
            "--bin shipyard --bin shipyard-workstream-provider",
            text,
        )
        self.assertIn('--companion-binary "${{ matrix.companion_binary }}"', text)
        self.assertIn("target/release/shipyard-workstream-provider --version", text)

    def test_release_workflow_attests_every_executable_release_asset(self) -> None:
        text = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        dist_dir = package_release.DEFAULT_DIST_DIR.relative_to(ROOT).as_posix()
        self.assertIn("attestations: write", text)
        self.assertIn("id-token: write", text)
        # Two subjects, three bounded attempts each.
        self.assertEqual(text.count(ATTEST), 6)
        self.assertIn("release/shipyard-linux-*", text)
        self.assertIn("release/shipyard-windows-*.exe", text)
        self.assertIn("release/shipyard-workstream-provider-linux-*", text)
        self.assertIn("release/shipyard-workstream-provider-windows-*.exe", text)
        self.assertIn(
            f"{dist_dir}/${{{{ github.ref_name }}}}/shipyard-macos-arm64.dmg",
            text,
        )

    def _steps(self, job: str) -> list[str]:
        """The text of each step in `job`, in order (no YAML dependency)."""
        text = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        start = text.index(f"\n  {job}:\n")
        following = re.search(r"\n  [A-Za-z0-9_-]+:\n", text[start + 1 :])
        body = text[start : start + 1 + following.start()] if following else text[start:]
        return re.split(r"\n      - ", body)[1:]

    def _job_permissions(self, job: str) -> dict[str, str]:
        """The job-level `permissions:` block of `job` (empty when absent)."""
        text = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        start = text.index(f"\n  {job}:\n")
        header = text[start:].split("\n    steps:\n", 1)[0]
        block = re.search(r"\n    permissions:\n((?:      \S.*\n?)+)", header)
        if not block:
            return {}
        return dict(
            line.strip().split(": ", 1) for line in block.group(1).splitlines()
        )

    def test_release_workflow_grants_no_permissions_by_default(self) -> None:
        text = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("\npermissions: {}\n", text)
        top = text.split("\njobs:\n", 1)[0]
        self.assertNotIn("write", top)

    def test_release_jobs_get_only_the_permissions_they_use(self) -> None:
        signing = {"attestations": "write", "contents": "write", "id-token": "write"}
        expected = {
            "resolve-runners": {"contents": "read"},
            "build": {"contents": "read"},
            "live-smoke": {"contents": "read"},
            "release": signing,
            "sign-and-upload-macos": signing,
        }
        text = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        jobs = re.findall(r"^  ([A-Za-z0-9_-]+):\n", text.split("\njobs:\n", 1)[1], re.M)
        self.assertEqual(sorted(jobs), sorted(expected))
        for job, permissions in expected.items():
            with self.subTest(job=job):
                self.assertEqual(self._job_permissions(job), permissions)

    def test_release_checkouts_do_not_persist_the_job_token(self) -> None:
        text = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        checkouts = text.count(f"uses: {CHECKOUT}")
        self.assertGreater(checkouts, 0)
        self.assertEqual(
            text.count(
                f"uses: {CHECKOUT}\n        with:\n          persist-credentials: false\n"
            ),
            checkouts,
        )
        # Nothing after checkout reuses a persisted git credential.
        self.assertNotRegex(text, r"\bgit (push|fetch|pull|ls-remote)\b")

    def test_attestation_retries_only_after_a_failed_attempt(self) -> None:
        for job, prefix in (
            ("release", "attest_nonmac"),
            ("sign-and-upload-macos", "attest_macos"),
        ):
            with self.subTest(job=job):
                attests = [
                    step for step in self._steps(job)
                    if ATTEST in step
                ]
                self.assertEqual(len(attests), 3)
                first, second, third = attests
                self.assertNotIn("if:", first)
                self.assertIn("continue-on-error: true", first)
                self.assertIn(f"if: steps.{prefix}_1.outcome == 'failure'", second)
                self.assertIn("continue-on-error: true", second)
                self.assertIn(f"if: steps.{prefix}_2.outcome == 'failure'", third)
                # The last attempt must be able to fail the job.
                self.assertNotIn("continue-on-error", third)

    def test_macos_release_is_published_only_after_attestation(self) -> None:
        steps = self._steps("sign-and-upload-macos")
        upload = next(step for step in steps if "upload macOS DMG" in step)
        self.assertIn("--defer-publish", upload)
        self.assertNotIn("--publish-only", upload)
        publish_index = next(
            index for index, step in enumerate(steps)
            if step.startswith("name: Publish the attested release")
        )
        self.assertIn("--publish-only", steps[publish_index])
        self.assertNotIn("if:", steps[publish_index])
        last_attest = max(
            index for index, step in enumerate(steps)
            if ATTEST in step
        )
        self.assertGreater(publish_index, last_attest)

    def test_macos_signing_job_builds_with_fresh_rust_homes(self) -> None:
        steps = self._steps("sign-and-upload-macos")
        names = [step.split("\n", 1)[0] for step in steps]
        isolate = names.index("name: Isolate the build from shared Rust homes")
        # Before the toolchain install and every cargo invocation.
        self.assertLess(isolate, names.index("name: Install Rust"))
        self.assertLess(isolate, next(
            index for index, step in enumerate(steps)
            if "scripts/release-macos-local.sh" in step
        ))
        step = steps[isolate]
        self.assertIn('root="${RUNNER_TEMP:?}/shipyard-rust-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}"', step)
        self.assertIn('rm -rf "$root"', step)
        self.assertIn('echo "CARGO_HOME=$root/cargo" >> "$GITHUB_ENV"', step)
        self.assertIn('echo "RUSTUP_HOME=$root/rustup" >> "$GITHUB_ENV"', step)
        self.assertNotIn("if:", step)
        # Runner selection is unchanged by the isolation.
        job = RELEASE_WORKFLOW.read_text(encoding="utf-8").split("\n  sign-and-upload-macos:\n", 1)[1]
        self.assertIn(
            "runs-on: ${{ fromJSON(needs.resolve-runners.outputs.macos_arm64_runs_on_json) }}",
            job.split("steps:", 1)[0],
        )

    def test_rust_home_isolation_is_specific_to_the_signing_job(self) -> None:
        # Negative control: the unsigned build matrix keeps its runner homes.
        for job in ("build", "live-smoke", "release"):
            with self.subTest(job=job):
                self.assertFalse(any(
                    "CARGO_HOME" in step or "RUSTUP_HOME" in step
                    for step in self._steps(job)
                ))

    def test_release_path_actions_are_pinned_to_commit_shas(self) -> None:
        uses = re.compile(r"^\s*(?:- )?uses: ([^\s@]+)@(\S+)(.*)$")
        for workflow in (
            RELEASE_WORKFLOW,
            AUTO_RELEASE_WORKFLOW,
            VERSION_AT_LAND_WORKFLOW,
            POST_TAG_SYNC_WORKFLOW,
        ):
            text = workflow.read_text(encoding="utf-8")
            found = 0
            for line in text.splitlines():
                match = uses.match(line)
                if not match:
                    continue
                found += 1
                action, ref, comment = match.groups()
                with self.subTest(workflow=workflow.name, action=action):
                    self.assertRegex(ref, r"^[0-9a-f]{40}$")
                    self.assertRegex(comment, r"^ # \S")
            self.assertGreater(found, 0, workflow.name)

    def test_workflow_run_scripts_never_interpolate_inputs(self) -> None:
        # An `${{ inputs.* }}` inside `run:` becomes shell source; pass it via env.
        cloud = ROOT / ".github" / "workflows" / "cloud-live-smoke.yml"
        text = cloud.read_text(encoding="utf-8")
        self.assertIn("NOTE_INPUT: ${{ inputs.note }}", text)
        self.assertIn('echo "note=$NOTE_INPUT"', text)
        for workflow in sorted((ROOT / ".github" / "workflows").glob("*.yml")):
            with self.subTest(workflow=workflow.name):
                lines = workflow.read_text(encoding="utf-8").splitlines()
                in_run, indent = False, 0
                for line in lines:
                    stripped = line.lstrip()
                    current = len(line) - len(stripped)
                    if in_run and stripped and current <= indent:
                        in_run = False
                    if stripped.startswith(("run: |", "- run: |")):
                        in_run, indent = True, current
                        continue
                    if in_run or stripped.startswith(("run: ", "- run: ")):
                        self.assertNotRegex(line, r"\$\{\{\s*inputs\.", line)

    def test_auto_release_workflow_supports_doctor_release_chain(self) -> None:
        text = AUTO_RELEASE_WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("workflow_dispatch:", text)
        self.assertIn("secrets.RELEASE_BOT_TOKEN", text)
        self.assertIn("RELEASE_BOT_TOKEN is required", text)
        self.assertNotIn("secrets.GITHUB_TOKEN", text)
        self.assertIn(CHECKOUT, text)
        self.assertIn("Cargo.toml", text)
        self.assertIn("version unchanged", text)
        self.assertIn("git tag -a", text)
        self.assertNotIn("pyproject.toml", text)


if __name__ == "__main__":
    unittest.main()
