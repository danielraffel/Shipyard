"""Windows toolchain helpers for the ssh-windows executor.

Two pieces of Pulp's local_ci.py that the cross-platform SSH executor
needs to match for capability parity:

1. **Host mutex** — when two validation jobs target the same Windows
   machine, they can stomp on each other (shared repo checkout,
   shared build tree, shared VS installation locks). Pulp wraps each
   remote validation in a `System.Threading.Mutex` and blocks if
   another run holds it. Shipyard ports the same pattern here.

2. **Visual Studio instance detection** — on a machine with multiple
   VS installations (Community + BuildTools + Preview, for example),
   CMake picks one non-deterministically. Pulp runs `vswhere.exe`
   before the build and passes the chosen installation path via
   `-DCMAKE_GENERATOR_INSTANCE=...`. It also derives the CMake
   platform (`ARM64` vs `x64`) from `$env:PROCESSOR_ARCHITECTURE` so
   ARM64 Windows hosts build native ARM64 binaries rather than
   defaulting to x64 cross-compilation.

Both helpers are pure string builders — they produce PowerShell
snippets that the ssh-windows executor splices into its remote
command. Running the detection is a separate SSH call, so the result
is cached per-host in the executor instance.
"""

from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Sequence


DEFAULT_MUTEX_NAME = "Global\\ShipyardValidate"


@dataclass(frozen=True)
class VsToolchain:
    """Resolved Visual Studio toolchain for a Windows host."""

    cmake_platform: str  # "ARM64" or "x64" (or whatever vswhere found)
    cmake_generator_instance: str  # filesystem path to the selected VS install


def wrap_powershell_with_host_mutex(
    ps_body: str,
    *,
    mutex_name: str = DEFAULT_MUTEX_NAME,
) -> str:
    """Wrap a PowerShell command in a host-wide mutex block.

    Only one validation holding `mutex_name` runs at a time. If the
    mutex is busy, the run emits `__SHIPYARD_WAIT__:host-lock` and
    `__SHIPYARD_PHASE__:waiting-lock` markers so log consumers can
    distinguish "waiting" from "stuck". An abandoned mutex from a
    crashed prior run is recovered automatically.

    The resulting script exits with the same exit code as `ps_body`.
    """
    # Escape single quotes in the mutex name so it can be dropped into
    # a PowerShell single-quoted string literal safely.
    safe_mutex = mutex_name.replace("'", "''")
    return f"""
$MutexName = '{safe_mutex}'
$Mutex = New-Object System.Threading.Mutex($false, $MutexName)
$LockAcquired = $false
try {{
    try {{
        if ($Mutex.WaitOne(0)) {{
            $LockAcquired = $true
        }} else {{
            Write-Host "__SHIPYARD_WAIT__:host-lock"
            Write-Host "__SHIPYARD_PHASE__:waiting-lock"
            Write-Host "Waiting for host validation lock: $MutexName"
            $null = $Mutex.WaitOne()
            $LockAcquired = $true
        }}
    }} catch [System.Threading.AbandonedMutexException] {{
        Write-Host "Recovered abandoned host validation lock: $MutexName"
        $LockAcquired = $true
    }}

    {ps_body}
    $__ShipyardExit = $LASTEXITCODE
}} finally {{
    if ($LockAcquired) {{
        try {{
            $Mutex.ReleaseMutex() | Out-Null
        }} catch [System.ApplicationException] {{
        }}
    }}
    $Mutex.Dispose()
}}
if ($null -ne $__ShipyardExit) {{
    exit $__ShipyardExit
}}
""".strip()


_VS_DETECT_SCRIPT = r"""
function Resolve-CMakePlatform {
    if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
        return 'ARM64'
    }
    return 'x64'
}

function Resolve-VisualStudioInstance {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) {
        return ''
    }
    try {
        $raw = (& $vswhere -latest -products * -format json) -join "`n"
        if (-not $raw) {
            return ''
        }
        $instances = $raw | ConvertFrom-Json
        if ($instances -isnot [System.Array]) {
            $instances = @($instances)
        }
        # Prefer a full VS install over BuildTools when both exist.
        $preferred = $instances | Where-Object {
            $_.productId -and $_.productId -ne 'Microsoft.VisualStudio.Product.BuildTools'
        } | Select-Object -First 1
        if (-not $preferred) {
            $preferred = $instances | Select-Object -First 1
        }
        if ($preferred -and $preferred.installationPath) {
            return $preferred.installationPath.Replace('\', '/')
        }
    } catch {
    }
    return ''
}

$resolved = @{
    platform = Resolve-CMakePlatform
    generator_instance = Resolve-VisualStudioInstance
}
$resolved | ConvertTo-Json -Compress
""".strip()


def detect_vs_toolchain(
    host: str,
    ssh_options: Sequence[str],
    *,
    timeout: int = 60,
) -> VsToolchain | None:
    """Run vswhere on the remote Windows host to resolve the VS toolchain.

    Returns None on any failure (vswhere missing, SSH failure, malformed
    JSON). Callers should treat None as "fall back to CMake defaults",
    not "error" — Windows hosts without multiple VS installations work
    fine without this hint.
    """
    cmd = [
        "ssh",
        *list(ssh_options),
        host,
        "powershell",
        "-NoProfile",
        "-Command",
        "-",
    ]
    try:
        run = subprocess.run(
            cmd,
            input=_VS_DETECT_SCRIPT,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except (subprocess.SubprocessError, OSError):
        return None

    if run.returncode != 0:
        return None

    # vswhere's script prints one JSON line; some environments prepend
    # banner text, so scan from the bottom for the first object.
    for line in reversed(run.stdout.splitlines()):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            data = json.loads(line)
        except json.JSONDecodeError:
            continue
        platform = (data.get("platform") or "").strip()
        instance = (data.get("generator_instance") or "").strip()
        if not platform and not instance:
            return None
        return VsToolchain(
            cmake_platform=platform,
            cmake_generator_instance=instance,
        )
    return None


def toolchain_env_exports(toolchain: VsToolchain | None) -> str:
    """PowerShell snippet that exports the resolved toolchain as env vars.

    Stage commands can reference `$env:SHIPYARD_CMAKE_PLATFORM` and
    `$env:SHIPYARD_CMAKE_GENERATOR_INSTANCE` to pass the detected
    values to CMake, e.g.::

        cmake -S . -B build -G "Visual Studio 17 2022" `
            -A $env:SHIPYARD_CMAKE_PLATFORM `
            "-DCMAKE_GENERATOR_INSTANCE=$env:SHIPYARD_CMAKE_GENERATOR_INSTANCE"

    When `toolchain` is None, the env vars are set to empty strings so
    stage commands can guard on them with `if ($env:... ) { ... }`.
    """
    if toolchain is None:
        return (
            "$env:SHIPYARD_CMAKE_PLATFORM = ''; "
            "$env:SHIPYARD_CMAKE_GENERATOR_INSTANCE = ''"
        )
    safe_platform = toolchain.cmake_platform.replace("'", "''")
    safe_instance = toolchain.cmake_generator_instance.replace("'", "''")
    return (
        f"$env:SHIPYARD_CMAKE_PLATFORM = '{safe_platform}'; "
        f"$env:SHIPYARD_CMAKE_GENERATOR_INSTANCE = '{safe_instance}'"
    )
