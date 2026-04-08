"""JSON output for agents and automation.

Every command's result is wrapped in an OutputEnvelope with a stable
schema_version. Agents parse this without needing to understand the
human-readable format.
"""

from __future__ import annotations

import json
import sys
from typing import Any

from shipyard.output.schema import OutputEnvelope


def render_json(envelope: OutputEnvelope) -> None:
    """Print an OutputEnvelope as formatted JSON to stdout."""
    json.dump(envelope.to_json_dict(), sys.stdout, indent=2, default=str)
    sys.stdout.write("\n")
    sys.stdout.flush()


def render_json_raw(data: dict[str, Any]) -> None:
    """Print raw dict as JSON (for simple cases)."""
    json.dump(data, sys.stdout, indent=2, default=str)
    sys.stdout.write("\n")
    sys.stdout.flush()
