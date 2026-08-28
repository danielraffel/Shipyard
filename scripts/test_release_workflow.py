#!/usr/bin/env python3
from __future__ import annotations

import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"
AUTO_RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "auto-release.yml"


class ReleaseWorkflowTests(unittest.TestCase):
    def test_release_workflow_keeps_macos_local_signing_gate(self) -> None:
        text = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("draft: true", text)
        self.assertIn("macOS release artifacts are intentionally not uploaded", text)
        self.assertIn("scripts/release-macos-local.sh", text)
        self.assertIn("--ci-mode", text)
        self.assertIn("CI_MACOS_SIGNING_ENABLED == 'true'", text)

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

    def test_auto_release_workflow_supports_doctor_release_chain(self) -> None:
        text = AUTO_RELEASE_WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("workflow_dispatch:", text)
        self.assertIn("secrets.RELEASE_BOT_TOKEN || secrets.GITHUB_TOKEN", text)
        self.assertIn("actions/checkout@v5", text)
        self.assertIn("Cargo.toml", text)
        self.assertIn("version unchanged", text)
        self.assertIn("git tag -a", text)
        self.assertNotIn("pyproject.toml", text)


if __name__ == "__main__":
    unittest.main()
