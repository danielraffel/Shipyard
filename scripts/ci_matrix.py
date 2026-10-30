#!/usr/bin/env python3
"""Resolve GitHub Actions runner matrices for Shipyard workflows.

GitHub-hosted runners are the safe default. Namespace remains an explicit
opt-in provider for repos/accounts that still have access. The `local`
provider routes to the maintainer's self-hosted Mac (label set
`["self-hosted","local-mac"]`) for the macOS release/build leg, falling back
to github-hosted for targets with no local box. Workflow inputs or repository
variables can override defaults without editing YAML. A pull request from a
fork never resolves to a self-hosted runner: those legs are skipped. The
self-hosted macOS Sandbox leg (the M3 canary) runs only for trusted code: a
push to main or a workflow_dispatch of main. Every pull request skips it.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping


VALID_PROVIDERS = ("namespace", "github-hosted", "local")
SANDBOX_M3_CAPABILITY_LABEL = "shipyard-sandbox-m3"
# Provider reported for a leg a fork pull request may not run. Its runs-on
# selector is empty, so a job that reads it without a guard fails closed.
SKIPPED_PROVIDER = "skipped"


@dataclass(frozen=True)
class RunnerTarget:
    key: str
    display_name: str
    env_suffix: str
    github_hosted_label: str
    namespace_label: str | None
    # Built-in runs-on selector for the `local` (self-hosted) provider. macOS
    # is the only target with a local Mac to land on; every other target has no
    # local box and falls back to github-hosted. May be a single label string
    # or a list of labels (AND-matched by GitHub Actions). None = no local box.
    local_label: str | list[str] | None = None


TARGETS: dict[str, RunnerTarget] = {
    "linux": RunnerTarget(
        key="linux",
        display_name="Linux",
        env_suffix="LINUX",
        github_hosted_label="ubuntu-latest",
        namespace_label="namespace-profile-generouscorp",
    ),
    "linux-arm64": RunnerTarget(
        key="linux-arm64",
        display_name="Linux ARM64",
        env_suffix="LINUX_ARM64",
        github_hosted_label="ubuntu-24.04-arm",
        namespace_label=None,
    ),
    "macos-arm64": RunnerTarget(
        key="macos-arm64",
        display_name="macOS ARM64",
        env_suffix="MACOS_ARM64",
        github_hosted_label="macos-15",
        namespace_label="namespace-profile-generouscorp-macos",
        local_label=["self-hosted", "local-mac"],
    ),
    "windows": RunnerTarget(
        key="windows",
        display_name="Windows",
        env_suffix="WINDOWS",
        github_hosted_label="windows-latest",
        namespace_label="namespace-profile-generouscorp-windows",
    ),
}


WORKFLOW_TARGETS = {
    "ci": ("linux", "macos-arm64", "windows"),
    "sandbox-e2e": ("linux", "macos-arm64"),
    "package-smoke": ("linux", "macos-arm64", "windows"),
    "release": ("macos-arm64", "linux", "linux-arm64", "windows"),
}


PACKAGE_ROWS = {
    "linux": {
        "package_target": "linux-x64",
        "binary": "target/release/shipyard",
        "companion_binary": "target/release/shipyard-workstream-provider",
        "python": "python3",
        "package_args": "",
    },
    "linux-arm64": {
        "package_target": "linux-arm64",
        "binary": "target/release/shipyard",
        "companion_binary": "target/release/shipyard-workstream-provider",
        "python": "python3",
        "package_args": "",
    },
    "macos-arm64": {
        "package_target": "macos-arm64",
        "binary": "target/release/shipyard",
        "companion_binary": "target/release/shipyard-workstream-provider",
        "python": "python3",
        "package_args": "--dmg --ci-mode",
    },
    "windows": {
        "package_target": "windows-x64",
        "binary": "target/release/shipyard.exe",
        "companion_binary": "target/release/shipyard-workstream-provider.exe",
        "python": "python",
        "package_args": "",
    },
}


def _env(env: Mapping[str, str], name: str) -> str:
    return (env.get(name) or "").strip()


def _load_selector(raw: str, *, target: RunnerTarget, source: str) -> str:
    try:
        decoded = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise SystemExit(
            f"{source} for {target.display_name} is not valid JSON: {exc}"
        ) from exc
    if not isinstance(decoded, (str, list)):
        raise SystemExit(
            f"{source} for {target.display_name} must decode to a string or "
            "array accepted by GitHub Actions runs-on."
        )
    return json.dumps(decoded, separators=(",", ":"))


def requested_provider(env: Mapping[str, str]) -> str:
    provider = _env(env, "REQUESTED_PROVIDER") or "github-hosted"
    if provider not in VALID_PROVIDERS:
        raise SystemExit(
            f"Unsupported runner provider {provider!r}; expected one of "
            f"{', '.join(VALID_PROVIDERS)}."
        )
    return provider


def is_fork_pull_request(env: Mapping[str, str]) -> bool:
    """True for a pull request whose head is not this repository.

    Repository variables such as DEFAULT_RUNNER_PROVIDER reach fork pull
    requests too, so they alone would put fork code on the persistent
    self-hosted Macs. An unknown head repository (deleted fork) is a fork.
    """
    if _env(env, "GITHUB_EVENT_NAME") not in {"pull_request", "pull_request_target"}:
        return False
    head = _env(env, "PR_HEAD_REPO").lower()
    return not head or head != _env(env, "GITHUB_REPOSITORY").lower()


def is_trusted_sandbox_event(env: Mapping[str, str]) -> bool:
    """True when the run may use the M3 Sandbox canary runner.

    That runner is the owner's GUI session beside the production daemon, so
    only code on main may land there: a push to main, or a workflow_dispatch
    of main (a dispatch can name any branch). Any other event or ref, same-repo
    pull requests included, is untrusted.
    """
    return (
        _env(env, "GITHUB_EVENT_NAME") in {"push", "workflow_dispatch"}
        and _env(env, "GITHUB_REF") == "refs/heads/main"
    )


def _is_self_hosted(provider: str, selector: str) -> bool:
    if provider == "local":
        return True
    decoded = json.loads(selector)
    labels = [decoded] if isinstance(decoded, str) else decoded
    return any(str(label).lower() == "self-hosted" for label in labels)


def resolve_runs_on(target_key: str, env: Mapping[str, str] = os.environ) -> dict[str, str]:
    target = TARGETS[target_key]
    provider = requested_provider(env)
    explicit_env = f"EXPLICIT_{target.env_suffix}_RUNNER_SELECTOR_JSON"

    explicit = _env(env, explicit_env)
    if explicit:
        selector = _load_selector(explicit, target=target, source=explicit_env)
    elif provider == "github-hosted":
        selector = json.dumps(target.github_hosted_label)
    elif provider == "namespace":
        namespace_env = f"NAMESPACE_{target.env_suffix}_RUNS_ON_JSON"
        namespace = _env(env, namespace_env)
        if namespace:
            selector = _load_selector(
                namespace,
                target=target,
                source=namespace_env,
            )
        elif target.namespace_label is not None:
            selector = json.dumps(target.namespace_label)
        else:
            provider = "github-hosted"
            selector = json.dumps(target.github_hosted_label)
    else:  # local — self-hosted runner on the maintainer's machine(s)
        local_env = f"LOCAL_{target.env_suffix}_RUNS_ON_JSON"
        local = _env(env, local_env)
        if local:
            selector = _load_selector(local, target=target, source=local_env)
        elif target.local_label is not None:
            selector = json.dumps(target.local_label)
        else:
            # No local box for this target (Linux/Windows) — degrade to hosted.
            provider = "github-hosted"
            selector = json.dumps(target.github_hosted_label)

    if is_fork_pull_request(env) and _is_self_hosted(provider, selector):
        # Skip rather than fall back to GitHub-hosted: CI runs on local
        # hardware, and the resolver never selects a host runner for a fork
        # PR. A fork that edits the workflow itself is stopped only by the
        # repo's fork-PR approval setting.
        provider = SKIPPED_PROVIDER
        selector = ""

    return {
        "key": target.key,
        "name": target.display_name,
        "provider": provider,
        "runs_on_json": selector,
    }


def _is_untrusted_sandbox_canary(
    workflow: str, row: Mapping[str, str], env: Mapping[str, str]
) -> bool:
    return (
        workflow == "sandbox-e2e"
        and row["key"] == "macos-arm64"
        and row["provider"] != SKIPPED_PROVIDER
        and _is_self_hosted(row["provider"], row["runs_on_json"])
        and not is_trusted_sandbox_event(env)
    )


VALID_FOCUS = ("macos", "all")
# Platforms kept when the focus is `macos`. linux-x64 stays in the release set
# because other repositories install the Shipyard linux-x64 binary on
# ubuntu-latest.
MACOS_FOCUS_TARGETS = {
    "ci": frozenset({"macos-arm64"}),
    "release": frozenset({"macos-arm64", "linux"}),
}


def platform_focus(env: Mapping[str, str]) -> str:
    """`macos` (default) or `all`, from the CI_PLATFORM_FOCUS repo variable or
    an explicit workflow_dispatch input, passed in as PLATFORM_FOCUS."""
    focus = _env(env, "PLATFORM_FOCUS").lower() or "macos"
    if focus not in VALID_FOCUS:
        raise SystemExit(
            f"Unsupported platform focus {focus!r}; expected one of {', '.join(VALID_FOCUS)}."
        )
    return focus


def workflow_matrix(workflow: str, env: Mapping[str, str] = os.environ) -> dict[str, list[dict[str, str]]]:
    try:
        target_keys = WORKFLOW_TARGETS[workflow]
    except KeyError as exc:
        raise SystemExit(f"Unsupported workflow {workflow!r}") from exc

    rows = []
    focus_keep = MACOS_FOCUS_TARGETS.get(workflow)
    focus = platform_focus(env)
    for target_key in target_keys:
        if focus == "macos" and focus_keep is not None and target_key not in focus_keep:
            continue
        row = resolve_runs_on(target_key, env)
        if row["provider"] == SKIPPED_PROVIDER:
            continue
        if _is_untrusted_sandbox_canary(workflow, row, env):
            # Dropped, not moved to GitHub-hosted: no check run is created,
            # so nothing is left pending. Push to main or dispatch main to run it.
            continue
        if workflow == "sandbox-e2e" and target_key == "macos-arm64" and row["provider"] == "local":
            selector = json.loads(row["runs_on_json"])
            if not isinstance(selector, list) or "self-hosted" not in selector:
                raise SystemExit(
                    "the local Sandbox M3 canary requires a self-hosted label array"
                )
            # Exactly these two labels: the M3 canary runner carries only the
            # shipyard-sandbox* labels, not local-mac or a host override.
            row["runs_on_json"] = json.dumps(
                ["self-hosted", SANDBOX_M3_CAPABILITY_LABEL], separators=(",", ":")
            )
        if workflow in {"package-smoke", "release"}:
            row.update(PACKAGE_ROWS[target_key])
            row["name"] = f"{row['name']} package"
        rows.append(row)
    return {"include": rows}


def workflow_outputs(workflow: str, env: Mapping[str, str] = os.environ) -> dict[str, str]:
    outputs = {
        "matrix_json": json.dumps(workflow_matrix(workflow, env), separators=(",", ":")),
        "platform_focus": platform_focus(env),
    }
    for target_key in TARGETS:
        row = resolve_runs_on(target_key, env)
        output_key = target_key.replace("-", "_")
        outputs[f"{output_key}_runs_on_json"] = row["runs_on_json"]
        outputs[f"{output_key}_provider"] = row["provider"]
    return outputs


def skipped_notices(workflow: str, env: Mapping[str, str] = os.environ) -> list[str]:
    notices = []
    for row in (resolve_runs_on(key, env) for key in WORKFLOW_TARGETS[workflow]):
        name = row["name"]
        if row["provider"] == SKIPPED_PROVIDER:
            notices.append(
                f"::notice title=Fork pull request::Skipped {name}: the resolver never "
                "selects a self-hosted runner for a fork pull request. A maintainer can "
                "push the reviewed commits to a branch of this repository to run it."
            )
        elif _is_untrusted_sandbox_canary(workflow, row, env):
            notices.append(
                f"::notice title=Sandbox M3 canary::Skipped {name}: the self-hosted "
                "Sandbox canary runs only for a push to main or a workflow_dispatch "
                "of main, never for a pull request or another branch."
            )
    return notices


def write_outputs(outputs: Mapping[str, str], path: Path) -> None:
    with path.open("a", encoding="utf-8") as handle:
        for key, value in outputs.items():
            handle.write(f"{key}={value}\n")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument(
        "--workflow",
        choices=sorted(WORKFLOW_TARGETS),
        required=True,
        help="Workflow matrix to emit.",
    )
    parser.add_argument(
        "--github-output",
        action="store_true",
        help="Append outputs to $GITHUB_OUTPUT instead of printing JSON.",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    outputs = workflow_outputs(args.workflow)
    for notice in skipped_notices(args.workflow):
        print(notice, file=sys.stdout if args.github_output else sys.stderr)
    if args.github_output:
        output_path = os.environ.get("GITHUB_OUTPUT")
        if not output_path:
            raise SystemExit("--github-output requires GITHUB_OUTPUT")
        write_outputs(outputs, Path(output_path))
    else:
        print(outputs["matrix_json"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
