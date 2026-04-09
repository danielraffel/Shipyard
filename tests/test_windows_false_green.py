"""Regression tests for the Windows false-green bug.

Stage 1 attempt 5 against Pulp reported `windows: pass` in 0.9 seconds
with an empty log. Two stacked bugs:

1. The PowerShell mutex wrapper's try/finally structure could fall
   through to an implicit exit 0 when the wrapped body raised an
   exception before reaching `$__ShipyardExit = $LASTEXITCODE`.
   Any command-not-found error (e.g. running `./setup.sh` on
   Windows PowerShell) would surface as "pass".

2. `_resolve_target_validation` read platform overrides from
   `config.validation["overrides"]` at the TOP of validation, but
   Pulp declares them at `[validation.default.overrides.windows]`
   — nested inside the mode subtable. Windows was running with
   Pulp's POSIX stage commands (`./setup.sh`, cmake without -G,
   etc.) which immediately tripped the first bug.
"""

from __future__ import annotations

from shipyard.core.config import Config
from shipyard.core.job import ValidationMode

# ── Fix 1: mutex wrapper fails loud on PowerShell exceptions ──────────


def test_mutex_wrapper_forces_explicit_exit() -> None:
    """The wrapped PS script must always end with `exit $__ShipyardExit`."""
    from shipyard.executor.windows_toolchain import wrap_powershell_with_host_mutex

    wrapped = wrap_powershell_with_host_mutex("Write-Host ok")
    # Every code path must reach the explicit exit — no fall-through
    assert wrapped.rstrip().endswith("exit $__ShipyardExit")
    # The previous false-green pattern is gone
    assert "if ($null -ne $__ShipyardExit)" not in wrapped


def test_mutex_wrapper_catches_body_exceptions() -> None:
    """A PS exception in the body must be caught and reported as exit 1."""
    from shipyard.executor.windows_toolchain import wrap_powershell_with_host_mutex

    wrapped = wrap_powershell_with_host_mutex("echo hi")
    # The body runs inside its own try/catch; the catch sets exit 1
    assert "catch {" in wrapped or "} catch {" in wrapped
    assert "$__ShipyardExit = 1" in wrapped
    # Default is also 1, so a `finally` jump without body completion
    # still fails loud
    assert "$__ShipyardExit = 1" in wrapped.split("try {")[0]


def test_mutex_wrapper_elevates_errors_to_terminating() -> None:
    """ErrorActionPreference must be Stop so unknown-command errors catch."""
    from shipyard.executor.windows_toolchain import wrap_powershell_with_host_mutex

    wrapped = wrap_powershell_with_host_mutex("echo hi")
    assert "$ErrorActionPreference = 'Stop'" in wrapped


def test_mutex_wrapper_body_exit_code_still_wins_on_success() -> None:
    """When the body finishes cleanly, its $LASTEXITCODE is preserved."""
    from shipyard.executor.windows_toolchain import wrap_powershell_with_host_mutex

    wrapped = wrap_powershell_with_host_mutex("Write-Host ok")
    # The success path assigns from $LASTEXITCODE, with fallback to 0
    # when the body ran no external process (so $LASTEXITCODE is null)
    assert "$__ShipyardExit = $LASTEXITCODE" in wrapped


# ── Fix 2: nested platform overrides resolve correctly ───────────────


def test_resolve_target_validation_applies_nested_platform_overrides() -> None:
    """Overrides inside `[validation.default.overrides.windows]` must win.

    This is the Pulp config shape that was silently ignored by the
    old `config.validation.get("overrides", {})` lookup.
    """
    from shipyard.cli import _resolve_target_validation, _resolve_validation

    config = Config(data={
        "validation": {
            "default": {
                "setup": "./setup.sh",
                "configure": "cmake -S . -B build",
                "build": "cmake --build build",
                "test": "ctest --test-dir build",
                "overrides": {
                    "windows": {
                        "configure": 'cmake -S . -B build -G "Visual Studio 17 2022" -A x64',
                        "build": "cmake --build build --config Release",
                        "test": "ctest --test-dir build -C Release",
                    },
                },
            },
        },
        "targets": {
            "windows": {
                "backend": "ssh-windows",
                "platform": "windows-x64",
            },
        },
    })

    base = _resolve_validation(config, ValidationMode.FULL)
    resolved = _resolve_target_validation(config, "windows", base)

    assert "Visual Studio 17 2022" in resolved["configure"]
    assert "Config Release" in resolved["build"] or "Release" in resolved["build"]
    assert "-C Release" in resolved["test"]
    # setup had no override, so the POSIX default survives
    assert resolved["setup"] == "./setup.sh"
    # The `overrides` key is stripped so it doesn't leak into the
    # downstream validation_config
    assert "overrides" not in resolved


def test_resolve_target_validation_respects_legacy_top_level_overrides() -> None:
    """Older config shape `[validation.overrides.<os>]` still works."""
    from shipyard.cli import _resolve_target_validation, _resolve_validation

    config = Config(data={
        "validation": {
            "default": {"setup": "./setup.sh"},
            "overrides": {
                "linux": {"setup": "./setup.sh --linux"},
            },
        },
        "targets": {
            "ubuntu": {"backend": "ssh", "platform": "linux-x64"},
        },
    })

    base = _resolve_validation(config, ValidationMode.FULL)
    resolved = _resolve_target_validation(config, "ubuntu", base)
    assert resolved["setup"] == "./setup.sh --linux"


def test_resolve_target_validation_mode_nested_wins_over_top_level() -> None:
    """When both are declared, the mode-nested override wins."""
    from shipyard.cli import _resolve_target_validation, _resolve_validation

    config = Config(data={
        "validation": {
            "default": {
                "setup": "./setup.sh",
                "overrides": {
                    "windows": {"setup": "mode nested"},
                },
            },
            "overrides": {
                "windows": {"setup": "top level"},
            },
        },
        "targets": {
            "windows": {"backend": "ssh-windows", "platform": "windows-x64"},
        },
    })

    base = _resolve_validation(config, ValidationMode.FULL)
    resolved = _resolve_target_validation(config, "windows", base)
    assert resolved["setup"] == "mode nested"


def test_resolve_target_validation_no_overrides_unchanged() -> None:
    """Configs without any overrides resolve to the base mode dict."""
    from shipyard.cli import _resolve_target_validation, _resolve_validation

    config = Config(data={
        "validation": {
            "default": {"setup": "./setup.sh", "build": "make"},
        },
        "targets": {
            "mac": {"backend": "local", "platform": "macos-arm64"},
        },
    })

    base = _resolve_validation(config, ValidationMode.FULL)
    resolved = _resolve_target_validation(config, "mac", base)
    assert resolved["setup"] == "./setup.sh"
    assert resolved["build"] == "make"
    assert "overrides" not in resolved
