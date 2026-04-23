"""Regression tests for the lazy-rich import optimization (#28).

The full rich import chain costs ~60ms on a cold PyInstaller bundle,
and many ``shipyard`` invocations never render anything (``--version``,
``--help``, the JSON-mode path, the daemon subprocess path). Deferring
``rich`` until the first render call keeps those paths cheap.
"""

from __future__ import annotations

import subprocess
import sys


def _run_import_only(snippet: str) -> list[str]:
    """Run the snippet in a fresh interpreter and return loaded modules."""
    full = (
        "import sys\n"
        + snippet
        + "\nprint('\\n'.join(sorted(sys.modules.keys())))\n"
    )
    result = subprocess.run(
        [sys.executable, "-c", full],
        capture_output=True, text=True, check=True,
    )
    return [m for m in result.stdout.splitlines() if m.strip()]


def test_importing_output_human_does_not_import_rich() -> None:
    """Importing ``shipyard.output.human`` must not pull in rich."""
    modules = _run_import_only("import shipyard.output.human")
    rich_modules = [m for m in modules if m.startswith("rich")]
    assert rich_modules == [], (
        f"Expected no rich.* modules after import shipyard.output.human; "
        f"got {rich_modules}"
    )


def test_importing_shipyard_cli_does_not_import_rich() -> None:
    """Importing ``shipyard.cli`` must not pull in rich either — the
    rich import was previously transitive through ``output.human``."""
    modules = _run_import_only("import shipyard.cli")
    rich_modules = [m for m in modules if m.startswith("rich")]
    assert rich_modules == [], (
        f"Expected no rich.* modules after import shipyard.cli; "
        f"got {rich_modules}"
    )


def test_first_render_call_loads_rich() -> None:
    """The first ``console.print(...)`` call should trigger the rich
    import (proving the proxy actually resolves, not that it silently
    no-ops)."""
    modules = _run_import_only(
        "from shipyard.output.human import render_message\n"
        "render_message('hello')\n"
    )
    # rich.console must now be loaded.
    assert any(m.startswith("rich") for m in modules), (
        "rich.* should be imported after the first render_message call"
    )
