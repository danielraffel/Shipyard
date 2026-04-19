"""Tests for locality-routing / capability-filtering in FallbackChain."""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any
from unittest.mock import MagicMock

import pytest

from shipyard.core.job import TargetResult, TargetStatus
from shipyard.failover.chain import FallbackChain, filter_backends_by_requires
from shipyard.providers.base import ProviderProfile
from shipyard.providers.github_hosted import GitHubHostedProvider
from shipyard.providers.namespace import NamespaceProvider


def _make_result(
    status: TargetStatus,
    backend: str = "mock",
    error_message: str | None = None,
) -> TargetResult:
    return TargetResult(
        target_name="cuda-build",
        platform="linux-x64",
        status=status,
        backend=backend,
        started_at=datetime.now(timezone.utc),
        completed_at=datetime.now(timezone.utc),
        duration_secs=1.0,
        error_message=error_message,
    )


def _mock_executor(
    validate_result: TargetResult,
    probe_ok: bool = True,
) -> MagicMock:
    executor = MagicMock()
    executor.validate.return_value = validate_result
    executor.probe.return_value = probe_ok
    return executor


@pytest.fixture
def validation_config() -> dict[str, Any]:
    return {"command": "make test"}


@pytest.fixture
def profile_registry() -> dict[str, dict[str, ProviderProfile]]:
    """Profile registry mirroring the built-in defaults."""
    return {
        "namespace": NamespaceProvider().profiles(),
        "github-hosted": GitHubHostedProvider().profiles(),
    }


class TestFilterBackendsByRequires:
    """Unit tests for the pure capability-filter helper."""

    def test_empty_requires_is_backward_compatible(self) -> None:
        backends = [
            {"type": "ssh", "host": "ubuntu"},
            {"type": "cloud", "provider": "namespace"},
        ]
        assert filter_backends_by_requires(backends, []) == backends

    def test_inline_capabilities_on_ssh_backend(self) -> None:
        backends = [
            {"type": "ssh", "host": "ubuntu"},  # no caps declared
            {
                "type": "ssh",
                "host": "gpu-box",
                "capabilities": ["gpu", "x86_64", "linux"],
            },
        ]
        filtered = filter_backends_by_requires(backends, ["gpu"])
        assert len(filtered) == 1
        assert filtered[0]["host"] == "gpu-box"

    def test_cloud_backend_resolves_from_profile_registry(
        self, profile_registry: dict[str, dict[str, ProviderProfile]]
    ) -> None:
        backends = [
            {"type": "cloud", "provider": "namespace", "profile": "gpu"},
            {"type": "cloud", "provider": "namespace"},  # default profile
        ]
        # gpu required -> only the explicit gpu profile survives
        filtered = filter_backends_by_requires(
            backends, ["gpu"], profile_registry
        )
        assert len(filtered) == 1
        assert filtered[0].get("profile") == "gpu"

    def test_cloud_default_profile_satisfies_common_caps(
        self, profile_registry: dict[str, dict[str, ProviderProfile]]
    ) -> None:
        backends = [{"type": "cloud", "provider": "namespace"}]
        filtered = filter_backends_by_requires(
            backends, ["linux", "x86_64"], profile_registry
        )
        assert len(filtered) == 1

    def test_github_hosted_builtin_profiles(
        self, profile_registry: dict[str, dict[str, ProviderProfile]]
    ) -> None:
        backends = [
            {
                "type": "cloud",
                "provider": "github-hosted",
                "profile": "ubuntu-latest",
            },
            {
                "type": "cloud",
                "provider": "github-hosted",
                "profile": "macos-15",
            },
        ]
        filtered = filter_backends_by_requires(
            backends, ["arm64", "macos"], profile_registry
        )
        assert len(filtered) == 1
        assert filtered[0]["profile"] == "macos-15"

    def test_unknown_capability_string_treated_as_opaque(
        self, profile_registry: dict[str, dict[str, ProviderProfile]]
    ) -> None:
        # User-defined "tee" capability — matcher is pure set containment.
        backends = [
            {
                "type": "ssh",
                "host": "enclave",
                "capabilities": ["tee", "linux"],
            },
            {"type": "cloud", "provider": "namespace"},
        ]
        filtered = filter_backends_by_requires(backends, ["tee"])
        assert len(filtered) == 1
        assert filtered[0]["host"] == "enclave"

    def test_missing_profile_registry_drops_cloud_backend(self) -> None:
        # If we don't pass profiles, a cloud backend without inline caps
        # can't be proven to satisfy anything and is filtered out.
        backends = [{"type": "cloud", "provider": "namespace"}]
        assert filter_backends_by_requires(backends, ["linux"]) == []


class TestFallbackChainLocalityRouting:
    """End-to-end tests for the chain's requires filter."""

    def test_requires_filter_picks_matching_backend(
        self,
        validation_config: dict[str, Any],
        profile_registry: dict[str, dict[str, ProviderProfile]],
    ) -> None:
        """A target with requires=[gpu] skips non-GPU backends."""
        cpu_exec = _mock_executor(_make_result(TargetStatus.PASS, backend="ssh"))
        cloud_exec = _mock_executor(_make_result(TargetStatus.PASS, backend="cloud"))

        chain = FallbackChain(
            backends=[
                {"type": "ssh", "host": "ubuntu"},  # no gpu
                {"type": "cloud", "provider": "namespace", "profile": "gpu"},
            ],
            executors={"ssh": cpu_exec, "cloud": cloud_exec},
            profiles=profile_registry,
        )
        target_config = {
            "name": "cuda-build",
            "platform": "linux-x64",
            "requires": ["gpu", "x86_64"],
        }
        result = chain.execute(
            "abc", "main", target_config, validation_config, "/tmp/log"
        )

        assert result.status == TargetStatus.PASS
        cpu_exec.validate.assert_not_called()
        cloud_exec.validate.assert_called_once()

    def test_no_provider_satisfies_requires_is_clear_error(
        self,
        validation_config: dict[str, Any],
        profile_registry: dict[str, dict[str, ProviderProfile]],
    ) -> None:
        """When no backend satisfies requires, the error lists what was tried."""
        chain = FallbackChain(
            backends=[
                {"type": "cloud", "provider": "namespace", "profile": "default"},
                {
                    "type": "cloud",
                    "provider": "github-hosted",
                    "profile": "ubuntu-latest",
                },
            ],
            executors={"cloud": _mock_executor(_make_result(TargetStatus.PASS))},
            profiles=profile_registry,
        )
        target_config = {
            "name": "cuda-build",
            "platform": "linux-x64",
            "requires": ["gpu"],
        }
        result = chain.execute(
            "abc", "main", target_config, validation_config, "/tmp/log"
        )

        assert result.status == TargetStatus.ERROR
        msg = result.error_message or ""
        assert "no provider satisfies requires=['gpu']" in msg
        assert "namespace.default" in msg
        assert "github-hosted.ubuntu-latest" in msg

    def test_missing_requires_is_backward_compatible(
        self,
        validation_config: dict[str, Any],
        profile_registry: dict[str, dict[str, ProviderProfile]],
    ) -> None:
        """A target with no requires runs unmodified against the chain."""
        ssh_exec = _mock_executor(_make_result(TargetStatus.PASS, backend="ssh"))
        chain = FallbackChain(
            backends=[{"type": "ssh", "host": "ubuntu"}],
            executors={"ssh": ssh_exec},
            profiles=profile_registry,
        )
        target_config = {"name": "ubuntu", "platform": "linux-x64"}
        result = chain.execute(
            "abc", "main", target_config, validation_config, "/tmp/log"
        )
        assert result.status == TargetStatus.PASS
        ssh_exec.validate.assert_called_once()

    def test_empty_requires_list_is_backward_compatible(
        self,
        validation_config: dict[str, Any],
        profile_registry: dict[str, dict[str, ProviderProfile]],
    ) -> None:
        """requires = [] behaves identically to no requires key."""
        ssh_exec = _mock_executor(_make_result(TargetStatus.PASS, backend="ssh"))
        chain = FallbackChain(
            backends=[{"type": "ssh", "host": "ubuntu"}],
            executors={"ssh": ssh_exec},
            profiles=profile_registry,
        )
        target_config = {
            "name": "ubuntu",
            "platform": "linux-x64",
            "requires": [],
        }
        result = chain.execute(
            "abc", "main", target_config, validation_config, "/tmp/log"
        )
        assert result.status == TargetStatus.PASS

    def test_filter_preserves_chain_order(
        self,
        validation_config: dict[str, Any],
        profile_registry: dict[str, dict[str, ProviderProfile]],
    ) -> None:
        """Filter keeps original order among matching backends."""
        first_cloud = _mock_executor(
            _make_result(TargetStatus.ERROR, backend="cloud-1", error_message="rate limit")
        )
        second_cloud = _mock_executor(_make_result(TargetStatus.PASS, backend="cloud-2"))

        chain = FallbackChain(
            backends=[
                {"type": "cloud", "provider": "namespace", "profile": "gpu"},
                {"type": "ssh", "host": "cpu-only"},  # filtered out
                {
                    "type": "cloud",
                    "provider": "namespace",
                    "profile": "gpu",
                    "name": "secondary",
                },
            ],
            executors={
                "cloud": MagicMock(
                    validate=MagicMock(side_effect=[
                        first_cloud.validate.return_value,
                        second_cloud.validate.return_value,
                    ]),
                    probe=MagicMock(return_value=True),
                ),
                "ssh": MagicMock(),
            },
            profiles=profile_registry,
        )
        target_config = {
            "name": "cuda-build",
            "platform": "linux-x64",
            "requires": ["gpu"],
        }
        result = chain.execute(
            "abc", "main", target_config, validation_config, "/tmp/log"
        )
        assert result.status == TargetStatus.PASS
        assert result.primary_backend == "cloud:namespace"

    def test_inline_capabilities_without_registry_still_work(
        self, validation_config: dict[str, Any]
    ) -> None:
        """Backends can self-declare capabilities — no registry needed."""
        gpu_exec = _mock_executor(_make_result(TargetStatus.PASS, backend="ssh"))
        chain = FallbackChain(
            backends=[
                {"type": "ssh", "host": "cpu-box", "capabilities": ["linux"]},
                {
                    "type": "ssh",
                    "host": "gpu-box",
                    "capabilities": ["gpu", "linux"],
                },
            ],
            executors={"ssh": gpu_exec},
            profiles={},
        )
        target_config = {
            "name": "cuda-build",
            "platform": "linux-x64",
            "requires": ["gpu"],
        }
        result = chain.execute(
            "abc", "main", target_config, validation_config, "/tmp/log"
        )
        assert result.status == TargetStatus.PASS


class TestProviderProfileDefaults:
    """Built-in profiles cover the common cases so users don't need config."""

    def test_namespace_default_profile_has_multi_arch(self) -> None:
        profile = NamespaceProvider().profiles()["default"]
        assert profile.satisfies(["linux", "x86_64"])
        assert profile.satisfies(["arm64"])
        assert profile.satisfies(["nested_virt"])
        assert not profile.satisfies(["gpu"])

    def test_namespace_gpu_profile(self) -> None:
        profile = NamespaceProvider().profiles()["gpu"]
        assert profile.satisfies(["gpu", "x86_64", "linux"])
        assert not profile.satisfies(["arm64"])

    def test_github_hosted_profiles_match_actual_images(self) -> None:
        profiles = GitHubHostedProvider().profiles()
        assert profiles["ubuntu-latest"].satisfies(["linux", "x86_64"])
        assert profiles["windows-latest"].satisfies(["windows", "x86_64"])
        assert profiles["macos-15"].satisfies(["macos", "arm64"])
        assert profiles["macos-13"].satisfies(["macos", "x86_64"])
        # GitHub-hosted runners don't have GPUs
        assert not profiles["ubuntu-latest"].satisfies(["gpu"])

    def test_user_profile_overrides_builtin(self) -> None:
        user_config = {
            "profiles": {
                "default": {"capabilities": ["linux", "x86_64", "privileged"]}
            }
        }
        profile = NamespaceProvider().profiles(user_config)["default"]
        assert profile.satisfies(["privileged"])
        # macos/windows from the built-in default are gone
        assert not profile.satisfies(["macos"])

    def test_namespace_platform_mapping_ignored_in_profiles(self) -> None:
        """String-valued entries are the legacy platform->profile map, not profiles."""
        user_config = {
            "profiles": {
                # legacy string mapping — resolve_selector uses this
                "linux-x64": "gpu-linux",
                # new table-valued profile definition
                "custom": {"capabilities": ["gpu", "arm64"]},
            }
        }
        profiles = NamespaceProvider().profiles(user_config)
        assert "custom" in profiles
        assert "linux-x64" not in profiles  # ignored (string value)
        assert profiles["custom"].satisfies(["gpu", "arm64"])
