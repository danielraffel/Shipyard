# Shipyard sandbox E2E test harness

Isolated end-to-end tests for the Shipyard CLI. Every test runs in a
fresh tempdir with a shadowed `$PATH` and isolated `$HOME`; the
contamination audit enforces **zero writes** to the user's real
Shipyard install.

Tracked in [Shipyard#248](https://github.com/danielraffel/Shipyard/issues/248).
Sibling harness pattern for the Pulp CLI lives in
[pulp#732](https://github.com/danielraffel/pulp/issues/732) and
[pulp#736](https://github.com/danielraffel/pulp/pull/736); the two
harnesses are intentionally shaped the same so a contributor who works
on one can read the other (just `s/pulp/shipyard/` on env var + binary
name).

## Why this exists

Installers, upgrades, daemon lifecycle, and unknown-subcommand
handling are the bugs that bite users hardest:

- v0.15→v0.18 silent-fail on MSVC include path (in Pulp; took 24h to
  notice releases were broken because no test exercised the
  end-to-end release pipeline)
- v0.21 / v0.29 / v0.39 incidents — bundle-apply path mismatches,
  PowerShell quoting bugs, etc.
- "Unknown command → exit 0" regressions — the canonical class of
  bug where a CLI silently does nothing on typos or removed
  subcommands. This harness's `test_unknown_subcommand_exits_nonzero_with_stderr`
  is the dedicated guard against that class.

Existing unit + integration tests don't catch these because they test
logic, not "does the installed binary behave correctly when invoked
the way users invoke it."

## What it covers

- **Smoke** — `--help`, `--version`, unknown top-level subcommand,
  unknown nested subcommand all behave non-silently.
- **Surface** — every `shipyard X` invocation in `commands/*.md`,
  `README.md`, `docs/**/*.md`, and `.github/workflows/*.yml` runs
  `--help` non-silently. Source-of-truth-driven: when a new slash-
  command lands, the surface enumerator picks it up automatically.
- **Config** — `shipyard config show` / `config profiles` read-only
  behavior in an empty sandbox.
- **Queue** — empty queue + empty target list still print observable
  output.
- **Daemon** — `shipyard daemon status` does NOT accidentally start
  the daemon, and does NOT create a socket file under the sandbox
  HOME.
- **Parity** — `shipyard --json status` is parseable JSON with
  the documented top-level keys (`schema_version`, `command`,
  `queue`, `targets`).
- **Pin** — `shipyard pin show` outside a consumer repo fails loudly
  with a clear hint, never silent-exit-0.
- **Bulkhead** — `Sandbox.run(["ship"])` / `["pr"]` / `["cloud",
  "run", ...]` raise `AssertionError` rather than executing the
  destructive command.
- **Contamination audit** — runs automatically at every test
  teardown; any write to a `PROTECTED_PATHS` directory fails the
  test.

## What it deliberately does NOT cover

| Excluded | Why |
|---|---|
| `shipyard ship` | actually merges PRs |
| `shipyard pr` | opens a real GitHub PR |
| `shipyard upgrade` | replaces the running binary |
| `shipyard cloud run` | dispatches a real GHA workflow on Namespace / GitHub-hosted |
| `shipyard daemon start`/`run` | long-lived process, opens sockets |
| `shipyard wait`/`watch` | block on a GitHub condition / live ship |
| `shipyard auto-merge` | hits GitHub to attempt a real merge |
| `shipyard release-bot setup` | guides through GitHub PAT provisioning |

These live in `SURFACE_SKIPS` in `test_swap.py` (and in
`DESTRUCTIVE_COMMANDS` / `SAFE_CLOUD_SUBCOMMANDS` in
`shipyard_sandbox.py` for the runtime bulkhead). Each skip carries a
one-line reason. When a new subcommand lands, the default is "add an
entry here and file a follow-up to write a real test."

## Running

```bash
# from the repo root
pytest tools/sandbox-e2e/

# or just the fast subset
pytest tools/sandbox-e2e/ -m "smoke or surface"

# pin a specific binary (useful in CI and when iterating on a branch)
SHIPYARD_BINARY_FOR_TEST=/path/to/shipyard pytest tools/sandbox-e2e/
```

## Binary discovery

`conftest.py` resolves the binary in this order:

1. `SHIPYARD_BINARY_FOR_TEST` env override
2. PyInstaller release artifacts:
   - `dist/shipyard`
   - `build/dist/shipyard`
   - `pyinstaller/dist/shipyard`
3. Installed-binary fallback: `~/.local/bin/shipyard` (the canonical
   install location, copied into the sandbox — never invoked in place)
4. Final fallback: an in-repo wrapper that invokes
   `python -m shipyard.cli` against the source under `src/shipyard/`
   (with `PYTHONPATH` shadowed so the user's site-packages can't
   leak in)

Tests never run against "whatever happens to be on PATH" — that's
what the harness is defending against.

## Contamination audit

Every `sandbox` fixture records a tempfile mtime at setup. At
teardown, `Sandbox.assert_no_contamination()` walks every entry in
`PROTECTED_PATHS` and fails if any file has a strictly-greater mtime.

Protected paths (in `shipyard_sandbox.py`):

- `~/Library/Application Support/shipyard/` — macOS combined dir
- `~/.config/shipyard/` — Linux config
- `~/.local/state/shipyard/` — Linux state (daemon socket lives here)
- `~/AppData/Local/shipyard/` — Windows combined dir
- `~/.local/bin/` — install location
- `~/.shipyard/` — legacy / future-proofing
- `~/.cache/shipyard/`

Adding a new protected path is a one-line PR; **prefer overbroad to
underbroad**. If a test fails the audit, the offender path is in the
failure output — fix the code, not the audit.

## Adding a new scenario

1. Decide which `@pytest.mark` applies: `smoke`, `surface`, `config`,
   `queue`, `daemon`, `parity`. If none fit, add a new mark and
   document it in `pytest.ini`.
2. Add a function in `test_swap.py`:

   ```python
   @pytest.mark.<mark>
   def test_<what_it_proves>(sandbox_with_shipyard: Sandbox) -> None:
       result = sandbox_with_shipyard.run(["your", "subcommand"])
       assert result.stdout or result.stderr, "non-silent contract"
   ```
3. If the scenario needs a stub binary / a fixture project / canned
   JSON, drop it under `fixtures/` and request it via a session-scoped
   pytest fixture in `conftest.py`.
4. If the scenario touches a new state path the audit doesn't cover
   yet, extend `PROTECTED_PATHS` — err on the side of overbroad.

## Layout

```
tools/sandbox-e2e/
├── README.md                  # this file
├── shipyard_sandbox.py        # Sandbox class, binary staging, contamination audit
├── conftest.py                # pytest fixtures: binary, sandbox, surface roots
├── pytest.ini                 # marker registry
├── fixtures/                  # (empty today; reserved for future stubs)
└── test_swap.py               # the scenarios
```

## Dependencies

Just `pytest` (Python 3.10+ for the type hints). The harness uses
only stdlib otherwise so it runs in any CI image that has `python3`.

## CI integration

See `.github/workflows/sandbox-e2e.yml`. Runs on macOS-latest +
ubuntu-latest on every PR that touches:

- `src/shipyard/**`
- `commands/**`
- `.claude-plugin/**`
- `tools/sandbox-e2e/**`
- `install.sh`
- `pyproject.toml`

Required status check for merge. Also a pre-release gate.

## Reusable pattern

The shape is deliberately project-agnostic. Any CLI that:

- Installs to a known prefix
- Has a state dir (config, cache, logs)
- Is wrapped by a plugin that shells out to it

…can mimic this harness with:

1. Rename `Path.home() / …` entries in `PROTECTED_PATHS` to your CLI's
   state locations
2. Rename `"shipyard"` → your binary name in the PATH shadow + binary
   discovery
3. Adapt `enumerate_shipyard_commands` to scan your plugin / IDE
   surface
4. Extend `DESTRUCTIVE_COMMANDS` with the subcommands that mutate
   real-world state

Both the Pulp and Shipyard harnesses ship the same primitives
(`Sandbox`, `RunResult`, `_otool_dylibs`, `_find_newer`,
`enumerate_*_commands`, `parse_*_json`); `s/pulp/shipyard/` on the
identifiers and you have the other one.
