# Installation

## Canonical install location

Every supported install path writes the `shipyard` binary to the same
place by default:

```
~/.local/bin/shipyard
```

| Path | Lands at |
|---|---|
| `curl … install.sh` | `~/.local/bin/shipyard` |
| Claude Code plugin (auto-installs on first session if missing) | `~/.local/bin/shipyard` |
| Codex one-liner (same `install.sh`) | `~/.local/bin/shipyard` |
| Project pinners (see "Pin a specific version" below) | `~/.local/bin/shipyard` (recommended) |

Make sure `~/.local/bin` is on your `PATH` and every install method
reaches the same binary. No PATH juggling, no "which shipyard am I
running" confusion.

## Quick install

```bash
curl -fsSL https://generouscorp.com/Shipyard/install.sh | sh
```

Downloads the right binary for your platform and installs it at
`~/.local/bin/shipyard`.

## Pin a specific version

Pass `SHIPYARD_VERSION` to install an exact release instead of the
latest. Useful for project-pinning so every teammate + agent runs
the same shipyard build.

```bash
SHIPYARD_VERSION="v0.22.1" curl -fsSL https://generouscorp.com/Shipyard/install.sh | bash
# or if you've already fetched the script:
SHIPYARD_VERSION="v0.22.1" bash install.sh
```

Accepts `"v0.22.1"`, `"0.22.1"`, or `"latest"` (default).

Project-level pinning pattern: keep the desired version in a small
pin file (e.g. `tools/shipyard.toml` with `version = "0.22.1"`), read
it in a wrapper script, and call `install.sh` with
`SHIPYARD_VERSION="$(read-version)"`. Nothing more complicated is
needed — every teammate ends up with the same binary at
`~/.local/bin/shipyard`.

## Install to a different directory

Pass `SHIPYARD_INSTALL_DIR`. Only override when you have a specific
reason; the default keeps every install path aligned.

```bash
SHIPYARD_INSTALL_DIR="${HOME}/mytools/bin" bash install.sh
```

## Platform binaries

| OS | Architecture | Binary |
|----|-------------|--------|
| macOS | Apple Silicon (ARM64) | `shipyard-macos-arm64` |
| macOS | Intel (x64) | `shipyard-macos-x64` |
| Windows | x64 | `shipyard-windows-x64.exe` |
| Linux | x64 | `shipyard-linux-x64` |
| Linux | ARM64 | `shipyard-linux-arm64` |

## Build from source

Two patterns depending on what you want.

### A. Isolated dev install (recommended for active development)

Your dev build lives in a venv; the system `shipyard` at
`~/.local/bin/shipyard` is unaffected. Activate the venv to use your
dev build, deactivate to use the system one.

```bash
git clone https://github.com/danielraffel/Shipyard.git
cd Shipyard
python3 -m venv .venv && source .venv/bin/activate
pip install -e ".[dev]"
pytest                         # verify everything works
```

Or with `uv`:

```bash
uv sync --extra dev
uv run pytest
```

### B. "My dev build is my system shipyard"

If you want your local checkout to take over at
`~/.local/bin/shipyard` (same location `install.sh` uses), use
[`pipx`](https://pipx.pypa.io):

```bash
pipx install .
pipx install . --force         # re-install after changes
```

Or `pip install --user .` achieves the same on most systems. Both
land at `~/.local/bin/shipyard`, so the Claude Code plugin + any
project pinners treat your dev build as the canonical install.

## Optional dependencies

You don't need everything — just what matches your setup. See the
[main README requirements table](../README.md#requirements) for
details.
