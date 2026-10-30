#!/usr/bin/env python3
from __future__ import annotations

import io
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import ci_matrix


class CiMatrixTests(unittest.TestCase):
    def test_github_hosted_is_safe_default_without_repo_vars(self) -> None:
        row = ci_matrix.resolve_runs_on("linux", {})
        self.assertEqual(row["provider"], "github-hosted")
        self.assertEqual(json.loads(row["runs_on_json"]), "ubuntu-latest")

    def test_github_hosted_provider_uses_hosted_labels(self) -> None:
        row = ci_matrix.resolve_runs_on("windows", {"REQUESTED_PROVIDER": "github-hosted"})
        self.assertEqual(row["provider"], "github-hosted")
        self.assertEqual(json.loads(row["runs_on_json"]), "windows-latest")

    def test_explicit_selector_wins_over_provider_default(self) -> None:
        row = ci_matrix.resolve_runs_on(
            "macos-arm64",
            {
                "REQUESTED_PROVIDER": "namespace",
                "EXPLICIT_MACOS_ARM64_RUNNER_SELECTOR_JSON": '["self-hosted","macos","arm64"]',
                "NAMESPACE_MACOS_ARM64_RUNS_ON_JSON": '"namespace-fallback"',
            },
        )
        self.assertEqual(
            json.loads(row["runs_on_json"]),
            ["self-hosted", "macos", "arm64"],
        )

    def test_namespace_repo_var_overrides_builtin_profile(self) -> None:
        row = ci_matrix.resolve_runs_on(
            "linux",
            {
                "REQUESTED_PROVIDER": "namespace",
                "NAMESPACE_LINUX_RUNS_ON_JSON": '"namespace-profile-custom"',
            },
        )
        self.assertEqual(json.loads(row["runs_on_json"]), "namespace-profile-custom")

    def test_invalid_selector_errors_before_workflow_dispatch(self) -> None:
        with self.assertRaises(SystemExit) as ctx:
            ci_matrix.resolve_runs_on(
                "linux",
                {"EXPLICIT_LINUX_RUNNER_SELECTOR_JSON": "{nope"},
            )
        self.assertIn("not valid JSON", str(ctx.exception))

    def test_package_smoke_matrix_carries_package_metadata(self) -> None:
        matrix = ci_matrix.workflow_matrix("package-smoke", {})
        rows = {row["key"]: row for row in matrix["include"]}
        self.assertEqual(rows["macos-arm64"]["package_args"], "--dmg --ci-mode")
        self.assertEqual(rows["windows"]["binary"], "target/release/shipyard.exe")
        self.assertEqual(
            rows["windows"]["companion_binary"],
            "target/release/shipyard-workstream-provider.exe",
        )
        self.assertEqual(
            rows["linux"]["companion_binary"],
            "target/release/shipyard-workstream-provider",
        )
        self.assertEqual(rows["linux"]["package_target"], "linux-x64")

    def test_release_matrix_carries_all_release_platforms(self) -> None:
        matrix = ci_matrix.workflow_matrix("release", {})
        rows = {row["key"]: row for row in matrix["include"]}
        self.assertEqual(
            set(rows),
            {"macos-arm64", "linux", "linux-arm64", "windows"},
        )
        self.assertEqual(rows["linux-arm64"]["package_target"], "linux-arm64")
        self.assertEqual(rows["linux-arm64"]["provider"], "github-hosted")
        self.assertEqual(
            json.loads(rows["linux-arm64"]["runs_on_json"]),
            "ubuntu-24.04-arm",
        )
        self.assertEqual(rows["macos-arm64"]["package_target"], "macos-arm64")
        self.assertNotIn("macos-x64", rows)

    def test_linux_arm64_supports_explicit_namespace_selector(self) -> None:
        row = ci_matrix.resolve_runs_on(
            "linux-arm64",
            {
                "REQUESTED_PROVIDER": "namespace",
                "EXPLICIT_LINUX_ARM64_RUNNER_SELECTOR_JSON": '["self-hosted","linux","arm64"]',
            },
        )
        self.assertEqual(
            json.loads(row["runs_on_json"]),
            ["self-hosted", "linux", "arm64"],
        )

    def test_github_output_writes_matrix_and_single_target_outputs(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "github-output"
            with mock.patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}, clear=True):
                self.assertEqual(ci_matrix.main(["--workflow", "sandbox-e2e", "--github-output"]), 0)
            values = dict(
                line.split("=", 1)
                for line in output.read_text(encoding="utf-8").splitlines()
            )
        self.assertEqual(len(json.loads(values["matrix_json"])["include"]), 2)
        self.assertEqual(json.loads(values["linux_runs_on_json"]), "ubuntu-latest")
        self.assertEqual(values["linux_provider"], "github-hosted")

    def test_local_provider_routes_macos_to_self_hosted_local_mac(self) -> None:
        row = ci_matrix.resolve_runs_on(
            "macos-arm64", {"REQUESTED_PROVIDER": "local"}
        )
        self.assertEqual(row["provider"], "local")
        self.assertEqual(
            json.loads(row["runs_on_json"]),
            ["self-hosted", "local-mac"],
        )

    def test_local_provider_falls_back_to_hosted_for_targets_without_local_box(
        self,
    ) -> None:
        for target_key, hosted in (
            ("linux", "ubuntu-latest"),
            ("linux-arm64", "ubuntu-24.04-arm"),
            ("windows", "windows-latest"),
        ):
            row = ci_matrix.resolve_runs_on(
                target_key, {"REQUESTED_PROVIDER": "local"}
            )
            self.assertEqual(row["provider"], "github-hosted", target_key)
            self.assertEqual(json.loads(row["runs_on_json"]), hosted, target_key)

    def test_local_repo_var_overrides_builtin_local_label(self) -> None:
        row = ci_matrix.resolve_runs_on(
            "macos-arm64",
            {
                "REQUESTED_PROVIDER": "local",
                "LOCAL_MACOS_ARM64_RUNS_ON_JSON": '["self-hosted","studio"]',
            },
        )
        self.assertEqual(row["provider"], "local")
        self.assertEqual(
            json.loads(row["runs_on_json"]),
            ["self-hosted", "studio"],
        )

    def test_explicit_selector_wins_over_local_provider(self) -> None:
        row = ci_matrix.resolve_runs_on(
            "macos-arm64",
            {
                "REQUESTED_PROVIDER": "local",
                "EXPLICIT_MACOS_ARM64_RUNNER_SELECTOR_JSON": '["self-hosted","macos","arm64"]',
            },
        )
        self.assertEqual(
            json.loads(row["runs_on_json"]),
            ["self-hosted", "macos", "arm64"],
        )

    def test_release_matrix_local_provider_routes_only_macos(self) -> None:
        matrix = ci_matrix.workflow_matrix(
            "release", {"REQUESTED_PROVIDER": "local"}
        )
        rows = {row["key"]: row for row in matrix["include"]}
        self.assertEqual(rows["macos-arm64"]["provider"], "local")
        self.assertEqual(
            json.loads(rows["macos-arm64"]["runs_on_json"]),
            ["self-hosted", "local-mac"],
        )
        self.assertEqual(rows["linux"]["provider"], "github-hosted")
        self.assertEqual(rows["linux-arm64"]["provider"], "github-hosted")
        self.assertEqual(rows["windows"]["provider"], "github-hosted")

    PUSH_TO_MAIN = {"GITHUB_EVENT_NAME": "push", "GITHUB_REF": "refs/heads/main"}

    def test_sandbox_local_macos_requires_explicit_m3_canary_capability(self) -> None:
        matrix = ci_matrix.workflow_matrix(
            "sandbox-e2e", dict(self.PUSH_TO_MAIN, REQUESTED_PROVIDER="local")
        )
        rows = {row["key"]: row for row in matrix["include"]}
        self.assertEqual(
            json.loads(rows["macos-arm64"]["runs_on_json"]),
            ["self-hosted", "shipyard-sandbox-m3"],
        )

    def test_sandbox_local_override_cannot_escape_m3_capability(self) -> None:
        matrix = ci_matrix.workflow_matrix(
            "sandbox-e2e",
            dict(
                self.PUSH_TO_MAIN,
                REQUESTED_PROVIDER="local",
                LOCAL_MACOS_ARM64_RUNS_ON_JSON='["self-hosted","studio"]',
            ),
        )
        rows = {row["key"]: row for row in matrix["include"]}
        self.assertEqual(
            json.loads(rows["macos-arm64"]["runs_on_json"]),
            ["self-hosted", "shipyard-sandbox-m3"],
        )

    FORK_PR = {
        "REQUESTED_PROVIDER": "local",
        "GITHUB_EVENT_NAME": "pull_request",
        "GITHUB_REPOSITORY": "danielraffel/Shipyard",
        "PR_HEAD_REPO": "someone/Shipyard",
    }

    def test_fork_pull_request_skips_self_hosted_legs(self) -> None:
        for workflow in ("ci", "sandbox-e2e"):
            with self.subTest(workflow=workflow):
                outputs = ci_matrix.workflow_outputs(workflow, self.FORK_PR)
                rows = json.loads(outputs["matrix_json"])["include"]
                # Skipped, not moved: no macOS row at all, hosted or local.
                self.assertNotIn("macos-arm64", [row["key"] for row in rows])
                self.assertNotIn("self-hosted", outputs["matrix_json"])
                self.assertNotIn("macos-15", outputs["matrix_json"])
                self.assertNotIn(
                    ci_matrix.SANDBOX_M3_CAPABILITY_LABEL, outputs["matrix_json"]
                )
                self.assertEqual(outputs["macos_arm64_provider"], "skipped")
                self.assertEqual(outputs["macos_arm64_runs_on_json"], "")
                # Legs that never used a self-hosted runner run as today.
                self.assertEqual(outputs["linux_provider"], "github-hosted")
                self.assertEqual(json.loads(outputs["linux_runs_on_json"]), "ubuntu-latest")

    def test_fork_pull_request_skips_any_self_hosted_selector(self) -> None:
        for env in (
            dict(self.FORK_PR, LOCAL_MACOS_ARM64_RUNS_ON_JSON='"studio"'),
            dict(
                self.FORK_PR,
                REQUESTED_PROVIDER="github-hosted",
                EXPLICIT_LINUX_RUNNER_SELECTOR_JSON='["Self-Hosted","linux"]',
            ),
            dict(
                self.FORK_PR,
                REQUESTED_PROVIDER="namespace",
                NAMESPACE_LINUX_RUNS_ON_JSON='["self-hosted","tart"]',
            ),
        ):
            key = "linux" if "LOCAL_MACOS_ARM64_RUNS_ON_JSON" not in env else "macos-arm64"
            with self.subTest(env=env):
                row = ci_matrix.resolve_runs_on(key, env)
                self.assertEqual(row["provider"], "skipped")
                self.assertEqual(row["runs_on_json"], "")

    def test_fork_pull_request_keeps_non_self_hosted_providers(self) -> None:
        row = ci_matrix.resolve_runs_on(
            "macos-arm64", dict(self.FORK_PR, REQUESTED_PROVIDER="namespace")
        )
        self.assertEqual(row["provider"], "namespace")
        self.assertEqual(
            json.loads(row["runs_on_json"]), "namespace-profile-generouscorp-macos"
        )

    def test_pull_request_with_unknown_head_repo_counts_as_fork(self) -> None:
        for head in ("", "   "):
            with self.subTest(head=head):
                row = ci_matrix.resolve_runs_on(
                    "macos-arm64", dict(self.FORK_PR, PR_HEAD_REPO=head)
                )
                self.assertEqual(row["provider"], "skipped")

    def test_same_repo_pull_request_runs_on_local_fleet_as_before(self) -> None:
        # Case differences in the slug are the same repository.
        env = dict(self.FORK_PR, PR_HEAD_REPO="DanielRaffel/shipyard")
        self.assertFalse(ci_matrix.is_fork_pull_request(env))
        without_context = {"REQUESTED_PROVIDER": "local"}
        self.assertEqual(
            ci_matrix.workflow_outputs("ci", env),
            ci_matrix.workflow_outputs("ci", without_context),
        )
        self.assertEqual(ci_matrix.skipped_notices("ci", env), [])
        # The Sandbox M3 canary is the exception: no pull request runs it.
        rows = ci_matrix.workflow_matrix("sandbox-e2e", env)["include"]
        self.assertEqual([row["key"] for row in rows], ["linux"])

    def test_sandbox_m3_canary_runs_only_for_push_to_main_or_dispatch(self) -> None:
        local = {
            "REQUESTED_PROVIDER": "local",
            "GITHUB_REPOSITORY": "danielraffel/Shipyard",
            "PR_HEAD_REPO": "danielraffel/Shipyard",
        }
        trusted = (
            {"GITHUB_EVENT_NAME": "push", "GITHUB_REF": "refs/heads/main"},
            {"GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_REF": "refs/heads/main"},
        )
        untrusted = (
            {"GITHUB_EVENT_NAME": "pull_request", "GITHUB_REF": "refs/pull/7/merge"},
            # A head ref named main is still pull request code.
            {"GITHUB_EVENT_NAME": "pull_request", "GITHUB_REF": "refs/heads/main"},
            {"GITHUB_EVENT_NAME": "pull_request_target", "GITHUB_REF": "refs/heads/main"},
            {"GITHUB_EVENT_NAME": "push", "GITHUB_REF": "refs/heads/topic"},
            # A dispatch can run any branch's code; only main is trusted.
            {"GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_REF": "refs/heads/topic"},
            {"GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_REF": "refs/tags/v1.0.0"},
            {"GITHUB_EVENT_NAME": "workflow_dispatch"},
            {"GITHUB_EVENT_NAME": "merge_group", "GITHUB_REF": "refs/heads/main"},
            {},
        )
        for context in trusted:
            with self.subTest(context=context):
                env = dict(local, **context)
                self.assertTrue(ci_matrix.is_trusted_sandbox_event(env))
                rows = {
                    row["key"]: row
                    for row in ci_matrix.workflow_matrix("sandbox-e2e", env)["include"]
                }
                self.assertEqual(
                    json.loads(rows["macos-arm64"]["runs_on_json"]),
                    ["self-hosted", "shipyard-sandbox-m3"],
                )
                self.assertEqual(ci_matrix.skipped_notices("sandbox-e2e", env), [])
        for context in untrusted:
            with self.subTest(context=context):
                env = dict(local, **context)
                self.assertFalse(ci_matrix.is_trusted_sandbox_event(env))
                outputs = ci_matrix.workflow_outputs("sandbox-e2e", env)
                rows = json.loads(outputs["matrix_json"])["include"]
                # Dropped, not moved: the Linux leg runs as before.
                self.assertEqual([row["key"] for row in rows], ["linux"])
                self.assertEqual(rows[0]["provider"], "github-hosted")
                self.assertNotIn("self-hosted", outputs["matrix_json"])
                self.assertNotIn("macos-15", outputs["matrix_json"])
                notices = ci_matrix.skipped_notices("sandbox-e2e", env)
                self.assertEqual(len(notices), 1)
                self.assertTrue(
                    notices[0].startswith(
                        "::notice title=Sandbox M3 canary::Skipped macOS ARM64"
                    )
                )

    def test_sandbox_untrusted_gate_covers_any_self_hosted_selector(self) -> None:
        pr = {"GITHUB_EVENT_NAME": "pull_request"}
        for env in (
            dict(pr, EXPLICIT_MACOS_ARM64_RUNNER_SELECTOR_JSON='["self-hosted","studio"]'),
            dict(pr, REQUESTED_PROVIDER="namespace",
                 NAMESPACE_MACOS_ARM64_RUNS_ON_JSON='["Self-Hosted","m3"]'),
        ):
            with self.subTest(env=env):
                rows = ci_matrix.workflow_matrix("sandbox-e2e", env)["include"]
                self.assertEqual([row["key"] for row in rows], ["linux"])

    def test_sandbox_pull_request_keeps_hosted_macos_leg(self) -> None:
        # Only the self-hosted canary is gated; a hosted macOS leg is isolated.
        env = {"GITHUB_EVENT_NAME": "pull_request", "REQUESTED_PROVIDER": "github-hosted"}
        rows = {
            row["key"]: row
            for row in ci_matrix.workflow_matrix("sandbox-e2e", env)["include"]
        }
        self.assertEqual(json.loads(rows["macos-arm64"]["runs_on_json"]), "macos-15")
        self.assertEqual(ci_matrix.skipped_notices("sandbox-e2e", env), [])

    def test_ci_workflow_ignores_the_sandbox_trust_gate(self) -> None:
        env = {
            "GITHUB_EVENT_NAME": "pull_request",
            "REQUESTED_PROVIDER": "local",
            "GITHUB_REPOSITORY": "danielraffel/Shipyard",
            "PR_HEAD_REPO": "danielraffel/Shipyard",
        }
        rows = {row["key"]: row for row in ci_matrix.workflow_matrix("ci", env)["include"]}
        self.assertEqual(rows["macos-arm64"]["provider"], "local")

    def test_push_and_dispatch_ignore_pr_head_context(self) -> None:
        for event in ("push", "workflow_dispatch", ""):
            with self.subTest(event=event):
                env = dict(self.FORK_PR, GITHUB_EVENT_NAME=event)
                self.assertFalse(ci_matrix.is_fork_pull_request(env))
                row = ci_matrix.resolve_runs_on("macos-arm64", env)
                self.assertEqual(row["provider"], "local")
                self.assertEqual(
                    json.loads(row["runs_on_json"]), ["self-hosted", "local-mac"]
                )

    def test_fork_pull_request_prints_a_notice_per_skipped_leg(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "github-output"
            env = dict(self.FORK_PR, GITHUB_OUTPUT=str(output))
            with mock.patch.dict(os.environ, env, clear=True), mock.patch(
                "sys.stdout", new_callable=io.StringIO
            ) as stdout:
                self.assertEqual(ci_matrix.main(["--workflow", "ci", "--github-output"]), 0)
        lines = stdout.getvalue().splitlines()
        self.assertEqual(len(lines), 1)
        self.assertTrue(lines[0].startswith("::notice title=Fork pull request::Skipped macOS ARM64"))

    def test_pull_request_workflows_pass_the_head_repo_to_the_resolver(self) -> None:
        root = Path(__file__).resolve().parents[1]
        for name in ("ci.yml", "sandbox-e2e.yml"):
            with self.subTest(workflow=name):
                workflow = (root / ".github/workflows" / name).read_text(encoding="utf-8")
                resolve = workflow.split("      - id: resolve\n", 1)[1].split("\n\n", 1)[0]
                self.assertIn(
                    "PR_HEAD_REPO: ${{ github.event.pull_request.head.repo.full_name }}",
                    resolve,
                )
        ci = (root / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        guard = "    if: needs.resolve-runners.outputs.linux_provider != 'skipped'\n"
        self.assertEqual(
            ci.count("runs-on: ${{ fromJSON(needs.resolve-runners.outputs.linux_runs_on_json) }}"),
            ci.count(guard),
        )

    def test_workflows_do_not_implicitly_route_macos_to_local_runner(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow_dir = root / ".github" / "workflows"
        for path in workflow_dir.glob("*.yml"):
            text = path.read_text(encoding="utf-8")
            self.assertNotIn("MACOS_ARM64_LOCAL_SELECTOR_JSON", text, path.name)

    def test_matrix_check_names_say_which_workflow_ran_them(self) -> None:
        # GitHub reports a check by job name alone, so two workflows whose
        # matrix legs render the same name ("macOS ARM64 [local]") cannot be
        # told apart by anything that reads check runs.
        root = Path(__file__).resolve().parents[1]
        names = {}
        for path in sorted((root / ".github" / "workflows").glob("*.yml")):
            for line in path.read_text(encoding="utf-8").splitlines():
                stripped = line.strip()
                if stripped.startswith("name:") and "${{ matrix." in stripped:
                    names.setdefault(stripped, []).append(path.name)
        duplicated = {name: files for name, files in names.items() if len(files) > 1}
        self.assertEqual(duplicated, {})
        self.assertIn("name: Test ${{ matrix.name }}", names)
        self.assertIn("name: Sandbox ${{ matrix.name }} [${{ matrix.provider }}]", names)

    def test_rust_cache_never_runs_the_macos_save_hook(self) -> None:
        root = Path(__file__).resolve().parents[1]
        guarded_save = (
            "save-if: ${{ github.ref == 'refs/heads/main' && runner.os != 'macOS' }}"
        )
        for relative, expected in (
            (".github/workflows/ci.yml", 2),
            (".github/workflows/sandbox-e2e.yml", 1),
            # Release-profile builds never restored a cache entry, so they
            # carry none at all.
            (".github/workflows/package-smoke.yml", 0),
            (".github/workflows/release.yml", 0),
        ):
            workflow = (root / relative).read_text(encoding="utf-8")
            self.assertEqual(workflow.count("uses: Swatinem/rust-cache@"), expected)
            self.assertEqual(workflow.count(guarded_save), expected)
            self.assertEqual(
                workflow.count("if: runner.environment == 'github-hosted'"), expected
            )

    def test_ci_rust_tests_do_not_inherit_runner_shipyard_configuration(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertEqual(workflow.count("SHIPYARD_TEST_HOME: ${{ runner.temp }}"), 3)
        self.assertEqual(workflow.count("--features ci-test-home"), 3)
        self.assertIn("Never let a self-hosted runner's production Shipyard", workflow)
        self.assertIn("Coverage executes the same tests", workflow)

    def test_local_validation_matches_ci_home_isolation_contract(self) -> None:
        root = Path(__file__).resolve().parents[1]
        config = (root / ".shipyard/config.toml").read_text(encoding="utf-8")
        self.assertIn('SHIPYARD_TEST_HOME=$(mktemp -d', config)
        self.assertIn("export SHIPYARD_TEST_HOME RUST_MIN_STACK=8388608", config)
        self.assertIn("--features ci-test-home", config)
        self.assertIn("trap 'rm -rf -- ", config)

    def test_sandbox_m3_bootstrap_is_fenced_by_target_provider_and_host(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/sandbox-e2e.yml").read_text(
            encoding="utf-8"
        )
        predicate = (
            "matrix.key == 'macos-arm64' && matrix.provider == 'local' && "
            "startsWith(runner.name, 'Shipyard-studio-')"
        )
        self.assertEqual(workflow.count(predicate), 4)
        self.assertNotIn("startsWith(runner.name, 'pulp-studio')", workflow)
        self.assertIn(
            "            Shipyard-studio-02|Shipyard-studio-02-sandbox) ;;\n", workflow
        )
        self.assertIn('if [ "$host_id" != "studio" ]; then', workflow)
        self.assertIn("sandbox-e2e must run on M3: tartci host.id", workflow)
        self.assertNotIn('case "$(hostname)" in', workflow)
        self.assertIn('test "$($installed runner tag)" = "studio"', workflow)

    def test_sandbox_m3_host_identity_comes_from_the_fleet_profile(self) -> None:
        """The M3 check reads tartci's host.id, so a hostname rename cannot fail it."""
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/sandbox-e2e.yml").read_text(encoding="utf-8")
        start = workflow.index('host_id="$(awk ')
        program = workflow[start:workflow.index("' \"$profile\"", start)]
        program = program[program.index("'") + 1:]
        profiles = {
            # The live m3 profile's shape.
            '[host]\nid = "studio"\nssh = "m3"\n[[lane]]\nid = "gate"\n': "studio",
            '# comment\n[runner]\nid = "other"\n[host]  # identity\n  id="studio"\n': "studio",
            '[host.limits]\nid = "studio"\n[host]\nssh = "m3"\n': "",
            '[host]\nssh = "m5"\n[lane]\nid = "studio"\n': "",
            '[host]\nid = "m5"\n': "m5",
        }
        with tempfile.TemporaryDirectory() as directory:
            for text, expected in profiles.items():
                path = Path(directory) / "profile.toml"
                path.write_text(text, encoding="utf-8")
                with self.subTest(profile=text):
                    found = subprocess.run(
                        ["awk", program, str(path)], capture_output=True, text=True, check=True
                    ).stdout.strip()
                    self.assertEqual(found, expected)

    def test_sandbox_m3_candidate_is_exact_and_production_is_restored(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/sandbox-e2e.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn('canary_root="/tmp/shipyard-sandbox-m3-', workflow)
        self.assertIn('"$candidate" --mode isolated', workflow)
        self.assertIn('--global-dir "$canary_root/global"', workflow)
        self.assertIn('--state-dir "$canary_root/state"', workflow)
        self.assertIn('.tunnel.backend == "inactive"', workflow)
        self.assertIn('SHIPYARD_BINARY_FOR_TEST="$candidate"', workflow)
        self.assertEqual(workflow.count("sandbox-audit-exec"), 2)
        self.assertEqual(workflow.count('--authority-sha "$GITHUB_SHA"'), 2)
        self.assertIn(
            'sandbox-e2e-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}-${RUNNER_NAME}',
            workflow,
        )
        self.assertIn('candidate_sha256:$candidate_hash', workflow)
        self.assertIn('sandbox_passed:true', workflow)
        self.assertIn('.real_home == env.HOME', workflow)
        self.assertIn('.mode == "shipyard"', workflow)
        self.assertIn('(.active_runs == [])', workflow)
        self.assertIn('"$canary_root/admission-deferred.json"', workflow)
        self.assertIn(
            'scripts/sandbox_admission_deferral.py',
            workflow,
        )
        self.assertEqual(workflow.count('ps -p "$production_pid" -o lstart='), 2)
        self.assertEqual(
            workflow.count(
                'jq -r .old_production_start_time "$canary_root/admission-deferred.json'
            ),
            2,
        )
        self.assertEqual(workflow.count('jq -r .mutation_probe_output'), 2)
        self.assertIn(
            '::notice::Sandbox E2E safely deferred because production workers are active',
            workflow,
        )
        self.assertIn('.final_production_pid', workflow)
        self.assertIn("legacy-lifetime-lock-quiesce-restore", workflow)
        self.assertIn("corrected-idle-preserve-fence", workflow)
        self.assertIn('"$canary_root/mutation-fence.json"', workflow)
        self.assertIn('returncode == 75', workflow)
        self.assertIn('overlap_classification == "sandbox_writer_domain_overlap"', workflow)
        self.assertIn('.production_identity_preserved == true', workflow)
        self.assertIn('.final_production_pid == .old_production_pid', workflow)
        self.assertIn('grep -F -- "--mode shipyard"', workflow)

    def test_sandbox_candidate_has_host_owned_cleanup_guardian(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/sandbox-e2e.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn('launchctl bootstrap "gui/$(id -u)" "$guardian_plist"', workflow)
        self.assertIn(
            '<string>--launchd-label</string><string>$guardian_label</string>',
            workflow,
        )
        self.assertIn(
            '<string>--legacy-launchd-migration</string><string>${{ '
            "github.event_name == 'workflow_dispatch' && "
            "inputs.migrate_legacy_launchd && 'apply' || 'report' }}</string>",
            workflow,
        )
        self.assertEqual(
            workflow.count(
                "terminal sandbox guardian remained registered after self-bootout"
            ),
            2,
        )
        self.assertEqual(workflow.count("launchd_deadline=$((SECONDS + 10))"), 2)
        self.assertEqual(
            workflow.count(
                "could not inventory launchd after terminal sandbox guardian"
            ),
            2,
        )
        self.assertEqual(
            workflow.count(
                ".launchd_cleanup_error == null and "
                "(.launchd_recovery.errors | length) == 0"
            ),
            2,
        )
        self.assertIn("<key>KeepAlive</key><false/>", workflow)
        self.assertIn(
            'cp "$GITHUB_WORKSPACE/scripts/sandbox_daemon_guardian.py" "$guardian"',
            workflow,
        )
        self.assertIn('<string>--owner-pid</string><string>$$</string>', workflow)
        self.assertIn('if kill -0 "$candidate_pid"', workflow)
        self.assertIn("name: Verify M3 guardian and production daemon invariants", workflow)
        self.assertEqual(workflow.count("inherited Actions orphan tracking"), 1)
        self.assertIn(
            "((.configured_repos // []) == [])", workflow
        )
        self.assertNotIn(
            '"$candidate" daemon refresh --repo Generous-Corp/pulp', workflow
        )
        self.assertNotIn('mkdir "$lease_dir"', workflow)
        self.assertIn(".production_quiesced == true", workflow)
        self.assertIn(".production_restored == true", workflow)
        self.assertIn(".production_preserved == true", workflow)
        self.assertIn(".mutation_fence_proved == true", workflow)
        self.assertIn(".production_identity_verified == true", workflow)
        self.assertIn(".lease_removed == true", workflow)
        self.assertIn('"$installed" --mode shipyard --json daemon status', workflow)
        self.assertIn('test "$current_pid" = "$production_pid"', workflow)
        self.assertIn(".configured_repos // []) | sort) == $expected", workflow)

    def test_sandbox_failure_artifacts_stay_in_explicit_temp_roots(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/sandbox-e2e.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "/tmp/sye2e-${{ github.run_id }}-${{ github.run_attempt }}/**",
            workflow,
        )
        self.assertIn(
            'export TMPDIR="/tmp/sye2e-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}"',
            workflow,
        )
        self.assertNotIn("/private/var/folders/**", workflow)
        self.assertNotIn("/var/folders/**", workflow)


if __name__ == "__main__":
    unittest.main()
