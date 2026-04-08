"""Structured output schema types.

Every CLI command produces an OutputEnvelope that can be rendered as
human-readable text (via rich) or machine-readable JSON.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

SCHEMA_VERSION = 1


@dataclass
class OutputEnvelope:
    """Wrapper for all CLI output.

    The JSON serialization always includes schema_version and command
    so agents can parse reliably.
    """

    command: str
    data: dict[str, Any]

    def to_json_dict(self) -> dict[str, Any]:
        return {
            "schema_version": SCHEMA_VERSION,
            "command": self.command,
            **self.data,
        }
