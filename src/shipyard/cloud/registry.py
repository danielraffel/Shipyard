"""Workflow discovery and dispatch defaults for `shipyard cloud`."""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any

from shipyard.providers.github_hosted import GitHubHostedProvider
from shipyard.providers.namespace import NamespaceProvider

if TYPE_CHECKING:
    from shipyard.core.config import Config

_ALIAS_MAP = {
    "ci": "build",
}


@dataclass(frozen=True)
class WorkflowDefinition:
    """Discovered cloud-dispatchable workflow."""

    key: str
    file: str
    name: str
    description: str
    inputs: tuple[str, ...]

    def to_dict(self) -> dict[str, Any]:
        return {
            "key": self.key,
            "file": self.file,
            "name": self.name,
            "description": self.description,
            "inputs": list(self.inputs),
        }


@dataclass(frozen=True)
class CloudDispatchPlan:
    """Resolved dispatch settings for a workflow run."""

    workflow: WorkflowDefinition
    repository: str | None
    ref: str
    provider: str
    dispatch_fields: dict[str, str]
    sources: dict[str, str]

    def to_dict(self) -> dict[str, Any]:
        return {
            "workflow": self.workflow.to_dict(),
            "repository": self.repository,
            "ref": self.ref,
            "provider": self.provider,
            "dispatch_fields": dict(self.dispatch_fields),
            "sources": dict(self.sources),
        }


def discover_workflows(repo_root: Path | None = None) -> dict[str, WorkflowDefinition]:
    """Discover workflow-dispatchable GitHub Actions workflows in the repo."""
    root = (repo_root or Path.cwd()).resolve()
    workflow_dir = root / ".github" / "workflows"
    discovered: dict[str, WorkflowDefinition] = {}
    if not workflow_dir.exists():
        return discovered

    for path in sorted(workflow_dir.glob("*.y*ml")):
        inputs = _discover_workflow_inputs(path)
        key = path.stem
        name = _discover_workflow_name(path) or _titleize(key)
        description = f"{name} ({path.name})"
        definition = WorkflowDefinition(
            key=key,
            file=path.name,
            name=name,
            description=description,
            inputs=tuple(inputs),
        )
        discovered[key] = definition
        alias = _ALIAS_MAP.get(key)
        if alias and alias not in discovered:
            discovered[alias] = WorkflowDefinition(
                key=alias,
                file=definition.file,
                name=definition.name,
                description=definition.description,
                inputs=definition.inputs,
            )

    return discovered


def resolve_cloud_dispatch_plan(
    *,
    config: Config,
    workflows: dict[str, WorkflowDefinition],
    workflow_key: str,
    ref: str,
    provider_override: str | None = None,
    runner_selector: str | None = None,
    linux_runner_selector: str | None = None,
    windows_runner_selector: str | None = None,
    macos_runner_selector: str | None = None,
) -> CloudDispatchPlan:
    """Resolve provider and selector inputs for a cloud workflow dispatch."""
    if workflow_key not in workflows:
        raise ValueError(f"Unknown workflow '{workflow_key}'")

    workflow = workflows[workflow_key]
    repository = config.get("cloud.repository")

    sources: dict[str, str] = {}
    provider = provider_override
    if provider:
        sources["provider"] = "cli"
    else:
        provider = config.get(f"cloud.workflows.{workflow_key}.provider")
        if provider:
            sources["provider"] = f"config:cloud.workflows.{workflow_key}.provider"
        else:
            provider = config.get("cloud.provider", "github-hosted")
            sources["provider"] = "config:cloud.provider" if config.get("cloud.provider") else "default"

    dispatch_fields: dict[str, str] = {}
    if "runner_provider" in workflow.inputs:
        dispatch_fields["runner_provider"] = provider

    overrides = {
        "linux-x64": linux_runner_selector,
        "windows-x64": windows_runner_selector,
        "macos-arm64": macos_runner_selector,
    }
    provider_config = config.get(f"cloud.providers.{provider}", {}) or {}
    workflow_config = config.get(f"cloud.workflows.{workflow_key}", {}) or {}

    if runner_selector and "runner_selector" in workflow.inputs:
        dispatch_fields["runner_selector"] = runner_selector
        sources["runner_selector"] = "cli"
    elif "runner_selector" in workflow.inputs:
        resolved = workflow_config.get("runner_selector") or provider_config.get("runner_selector")
        if resolved:
            dispatch_fields["runner_selector"] = resolved
            sources["runner_selector"] = (
                f"config:cloud.workflows.{workflow_key}.runner_selector"
                if workflow_config.get("runner_selector")
                else f"config:cloud.providers.{provider}.runner_selector"
            )

    resolved_overrides = _resolve_runner_overrides(
        provider=provider,
        provider_config=provider_config,
        workflow_config=workflow_config,
        cli_overrides=overrides,
    )
    if resolved_overrides and "runner_overrides" in workflow.inputs:
        import json

        dispatch_fields["runner_overrides"] = json.dumps(resolved_overrides)
        sources["runner_overrides"] = "cli/config/provider"
    else:
        _apply_platform_specific_inputs(
            workflow=workflow,
            dispatch_fields=dispatch_fields,
            resolved_overrides=resolved_overrides,
        )

    return CloudDispatchPlan(
        workflow=workflow,
        repository=repository,
        ref=ref,
        provider=provider,
        dispatch_fields=dispatch_fields,
        sources=sources,
    )


def default_workflow_key(config: Config, workflows: dict[str, WorkflowDefinition]) -> str | None:
    configured = config.get("cloud.default_workflow")
    if configured and configured in workflows:
        return configured
    if "build" in workflows:
        return "build"
    if workflows:
        return sorted(workflows)[0]
    return None


def _resolve_runner_overrides(
    *,
    provider: str,
    provider_config: dict[str, Any],
    workflow_config: dict[str, Any],
    cli_overrides: dict[str, str | None],
) -> dict[str, str]:
    overrides: dict[str, str] = {}
    workflow_overrides = workflow_config.get("runner_overrides", {}) or {}
    provider_overrides = provider_config.get("runner_overrides", {}) or {}

    for platform, cli_value in cli_overrides.items():
        if cli_value:
            overrides[platform] = cli_value
            continue
        if platform in workflow_overrides:
            overrides[platform] = workflow_overrides[platform]
            continue
        if platform in provider_overrides:
            overrides[platform] = provider_overrides[platform]
            continue
        resolved = _resolve_provider_selector(provider, platform, provider_config)
        if resolved:
            overrides[platform] = resolved

    return overrides


def _resolve_provider_selector(
    provider: str,
    platform: str,
    provider_config: dict[str, Any],
) -> str | None:
    try:
        if provider == "namespace":
            return NamespaceProvider().resolve_selector(platform, provider_config)
        if provider == "github-hosted":
            return GitHubHostedProvider().resolve_selector(platform, provider_config)
    except ValueError:
        return None
    return None


def _apply_platform_specific_inputs(
    *,
    workflow: WorkflowDefinition,
    dispatch_fields: dict[str, str],
    resolved_overrides: dict[str, str],
) -> None:
    mapping = {
        "linux-x64": "linux_runner_selector",
        "windows-x64": "windows_runner_selector",
        "macos-arm64": "macos_runner_selector",
    }
    for platform, input_name in mapping.items():
        if input_name in workflow.inputs and platform in resolved_overrides:
            dispatch_fields[input_name] = resolved_overrides[platform]


def _discover_workflow_name(path: Path) -> str | None:
    for line in path.read_text().splitlines():
        match = re.match(r"^name:\s*(.+)$", line.strip())
        if match:
            return match.group(1).strip().strip("'\"")
    return None


def _discover_workflow_inputs(path: Path) -> list[str]:
    inputs: list[str] = []
    lines = path.read_text().splitlines()
    in_workflow_dispatch = False
    in_inputs = False
    workflow_indent = None
    inputs_indent = None

    for raw_line in lines:
        if not raw_line.strip() or raw_line.lstrip().startswith("#"):
            continue

        indent = len(raw_line) - len(raw_line.lstrip(" "))
        stripped = raw_line.strip()

        if stripped.startswith("workflow_dispatch:"):
            in_workflow_dispatch = True
            in_inputs = False
            workflow_indent = indent
            continue

        if in_workflow_dispatch and workflow_indent is not None and indent <= workflow_indent and ":" in stripped:
            in_workflow_dispatch = False
            in_inputs = False

        if in_workflow_dispatch and stripped.startswith("inputs:"):
            in_inputs = True
            inputs_indent = indent
            continue

        if in_inputs and inputs_indent is not None and indent <= inputs_indent:
            in_inputs = False

        if in_inputs and inputs_indent is not None and indent == inputs_indent + 2 and stripped.endswith(":"):
            inputs.append(stripped[:-1])

    return inputs


def _titleize(key: str) -> str:
    return key.replace("-", " ").replace("_", " ").title()
