"""Target configuration model.

A "target" is a named validation lane — a platform/backend pair plus an
optional fallback chain. Targets are defined in TOML and consumed by
the executor and failover chain.

Most of the codebase passes targets around as raw dicts (merged TOML
sections), so this module keeps a lightweight :class:`TargetConfig`
dataclass that mirrors the recognized keys without forcing a full
refactor. Code that wants structured access can call
:func:`parse_target` on a dict; code that only needs one field still
reads it off the dict.

The fields here are descriptive, not exhaustive — backend-specific
keys (``host``, ``vm_name``, ``workflow``, …) stay as free-form entries
on the underlying dict.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass
class TargetConfig:
    """Structured view of a ``[targets.<name>]`` TOML section.

    Attributes:
        name: Target key (e.g. ``"mac"``, ``"cuda-build"``).
        platform: Platform label (e.g. ``"linux-x64"``, ``"macos-arm64"``).
        backend: Backend type (``"local"``, ``"ssh"``, ``"cloud"``,
            ``"vm"``, ``"ssh-windows"``). Mirrors the legacy ``type``
            key as well.
        requires: Declarative capability constraints the chosen runner
            must satisfy (e.g. ``["gpu", "arm64"]``). Empty list means
            "no constraint" (backward-compatible default).
        fallback: Ordered list of backend definitions to try when the
            primary is unreachable. Each entry is a dict with a
            ``type`` key and backend-specific fields.
        raw: The original dict this was parsed from, preserved so
            callers can reach backend-specific keys without a
            round-trip through the dataclass.
    """

    name: str
    platform: str = ""
    backend: str = ""
    requires: list[str] = field(default_factory=list)
    fallback: list[dict[str, Any]] = field(default_factory=list)
    raw: dict[str, Any] = field(default_factory=dict)


def parse_target(name: str, data: dict[str, Any]) -> TargetConfig:
    """Build a :class:`TargetConfig` from a raw TOML dict.

    Tolerant of missing keys — every field has a sensible default.
    The input dict is not mutated; the resulting ``TargetConfig.raw``
    points at the same object so downstream code can still read
    backend-specific keys.
    """
    requires_raw = data.get("requires", []) or []
    if not isinstance(requires_raw, list):
        raise ValueError(
            f"target '{name}': requires must be a list, got {type(requires_raw).__name__}"
        )
    requires = [str(item).strip() for item in requires_raw if str(item).strip()]

    fallback_raw = data.get("fallback", []) or []
    if not isinstance(fallback_raw, list):
        raise ValueError(
            f"target '{name}': fallback must be a list, got {type(fallback_raw).__name__}"
        )

    backend = str(
        data.get("backend") or data.get("type") or ""
    ).strip()

    return TargetConfig(
        name=name,
        platform=str(data.get("platform", "")),
        backend=backend,
        requires=requires,
        fallback=[entry for entry in fallback_raw if isinstance(entry, dict)],
        raw=data,
    )


def extract_requires(target_config: dict[str, Any]) -> list[str]:
    """Extract the normalized ``requires`` list from a target dict.

    Returns an empty list (meaning "no constraint") when the key is
    missing, empty, or malformed. Unknown capability strings are
    preserved verbatim — users may define their own vocabulary.
    """
    raw = target_config.get("requires", []) or []
    if not isinstance(raw, list):
        return []
    return [str(item).strip() for item in raw if str(item).strip()]


__all__ = ["TargetConfig", "parse_target", "extract_requires"]
