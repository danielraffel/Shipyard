"""Tests for shipyard.targets.TargetConfig and helpers."""

from __future__ import annotations

import pytest

from shipyard.targets import TargetConfig, extract_requires, parse_target


class TestParseTarget:
    def test_minimal_target(self) -> None:
        t = parse_target("mac", {"platform": "macos-arm64", "backend": "local"})
        assert t.name == "mac"
        assert t.platform == "macos-arm64"
        assert t.backend == "local"
        assert t.requires == []
        assert t.fallback == []

    def test_requires_parsed(self) -> None:
        t = parse_target(
            "cuda-build",
            {
                "platform": "linux-x64",
                "requires": ["gpu", "x86_64"],
                "fallback": [{"type": "cloud", "provider": "namespace"}],
            },
        )
        assert t.requires == ["gpu", "x86_64"]
        assert len(t.fallback) == 1

    def test_requires_strips_whitespace(self) -> None:
        t = parse_target("x", {"requires": [" gpu ", "", "  "]})
        assert t.requires == ["gpu"]

    def test_requires_must_be_list(self) -> None:
        with pytest.raises(ValueError, match="requires must be a list"):
            parse_target("x", {"requires": "gpu"})

    def test_legacy_type_key_maps_to_backend(self) -> None:
        t = parse_target("x", {"type": "ssh", "platform": "linux-x64"})
        assert t.backend == "ssh"

    def test_raw_preserved(self) -> None:
        raw = {"platform": "linux-x64", "host": "ubuntu", "custom_key": 42}
        t = parse_target("ubuntu", raw)
        assert t.raw is raw
        assert t.raw["custom_key"] == 42


class TestExtractRequires:
    def test_missing_key_returns_empty(self) -> None:
        assert extract_requires({}) == []

    def test_empty_list(self) -> None:
        assert extract_requires({"requires": []}) == []

    def test_normal_case(self) -> None:
        assert extract_requires({"requires": ["gpu", "arm64"]}) == ["gpu", "arm64"]

    def test_malformed_returns_empty(self) -> None:
        # Malformed values are tolerated — missing requires is safer than crashing.
        assert extract_requires({"requires": "gpu"}) == []
