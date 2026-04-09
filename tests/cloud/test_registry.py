from __future__ import annotations

from pathlib import Path

from shipyard.cloud.registry import default_workflow_key, discover_workflows, resolve_cloud_dispatch_plan
from shipyard.core.config import Config


def test_discover_workflows_reads_inputs_and_aliases(tmp_path: Path) -> None:
    workflow_dir = tmp_path / ".github" / "workflows"
    workflow_dir.mkdir(parents=True)
    (workflow_dir / "ci.yml").write_text(
        """
name: CI
on:
  workflow_dispatch:
    inputs:
      runner_provider:
        description: provider
      runner_overrides:
        description: overrides
"""
    )

    workflows = discover_workflows(tmp_path)

    assert "ci" in workflows
    assert "build" in workflows
    assert workflows["ci"].inputs == ("runner_provider", "runner_overrides")
    assert default_workflow_key(Config(data={}), workflows) == "build"


def test_resolve_cloud_dispatch_plan_uses_provider_resolution(tmp_path: Path) -> None:
    workflow_dir = tmp_path / ".github" / "workflows"
    workflow_dir.mkdir(parents=True)
    (workflow_dir / "ci.yml").write_text(
        """
name: CI
on:
  workflow_dispatch:
    inputs:
      runner_provider:
        description: provider
      runner_overrides:
        description: overrides
"""
    )
    workflows = discover_workflows(tmp_path)
    config = Config(
        data={
            "cloud": {
                "provider": "namespace",
                "providers": {
                    "namespace": {
                        "runner_overrides": {
                            "linux-x64": "namespace-profile-linux",
                            "windows-x64": "namespace-profile-windows",
                        }
                    }
                },
            }
        }
    )

    plan = resolve_cloud_dispatch_plan(
        config=config,
        workflows=workflows,
        workflow_key="build",
        ref="feature/test",
    )

    assert plan.provider == "namespace"
    assert plan.dispatch_fields["runner_provider"] == "namespace"
    assert "runner_overrides" in plan.dispatch_fields
