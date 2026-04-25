"""End-to-end sandbox scenarios for the Shipyard CLI.

Every test here runs in an isolated ``Sandbox`` (see
``shipyard_sandbox.py``) and must pass the contamination audit at
teardown — any write to ``~/Library/Application Support/shipyard/``,
``~/.config/shipyard/``, ``~/.local/state/shipyard/``,
``~/AppData/Local/shipyard/``, or ``~/.local/bin/`` fails the test.

Scenarios mirror the acceptance criteria on
`Shipyard #248 <https://github.com/danielraffel/Shipyard/issues/248>`_.
Grouped by ``@pytest.mark`` so CI can run a fast subset on PR gates and
the full suite on release tags. See ``pytest.ini`` for marker docs.

The file is named ``test_swap.py`` to keep the harness shape parallel
to the Pulp sibling at
`pulp#732 <https://github.com/danielraffel/pulp/issues/732>`_; ignore
the historical "swap" connotation (Pulp's harness was written during
a C++→Rust binary swap) — for Shipyard "swap" is just "the file where
the scenarios live."
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from shipyard_sandbox import (
    REQUIRED_STATUS_KEYS,
    Sandbox,
    enumerate_shipyard_commands,
    parse_status_json,
)


# -----------------------------------------------------------------------------
# Shared fixtures / helpers
# -----------------------------------------------------------------------------


#: Subcommand tokens we deliberately don't exercise via ``--help``.
#: These either invoke destructive flows (caught by Sandbox.run's guard
#: regardless), or are noise the surface enumerator picks up from
#: docs prose. Each entry has a one-line reason — adding to this set
#: should be a deliberate decision, not a quick fix to get green.
SURFACE_SKIPS: dict[str, str] = {
    # Destructive (also blocked by Sandbox.run's DESTRUCTIVE_COMMANDS)
    "ship": "actually merges PRs",
    "pr": "opens a real GitHub PR",
    "upgrade": "replaces the running binary",
    # Long-lived processes
    "daemon": "the parent group is fine but `daemon run`/`start` block — "
              "covered by a dedicated daemon-status scenario instead",
    "watch": "live view, hangs when there's no active ship",
    "wait": "blocks on a github condition; sub-commands need network",
    # Network / system probes
    "release-bot": "guides through GitHub PAT provisioning",
    "auto-merge": "hits GitHub to attempt a real merge",
    # Doc-prose tokens that don't correspond to real subcommands
    "version": "no real `shipyard version` subcommand; this comes from "
               "prose like 'the Shipyard version pin'",
}


@pytest.fixture(scope="session")
def shipyard_surface(surface_roots: list[Path]) -> set[str]:
    """The set of ``shipyard <subcommand>`` tokens advertised by the
    project's user-facing surface (slash commands, README, docs, CI)."""
    return enumerate_shipyard_commands(surface_roots)


# -----------------------------------------------------------------------------
# Scenario 1 — Smoke: help banner + version exit non-silent
# -----------------------------------------------------------------------------


@pytest.mark.smoke
def test_help_banner_exits_zero_with_usage_output(
    sandbox_with_shipyard: Sandbox,
) -> None:
    """``shipyard --help`` must exit 0 and print a usage banner. This
    is the cheapest possible "is the binary usable at all?" probe."""
    result = sandbox_with_shipyard.run(["--help"]).expect_success()
    # Click's banner format: "Usage: shipyard [OPTIONS] COMMAND ..."
    assert "Usage:" in result.stdout, (
        f"--help didn't print a usage banner: {result.stdout!r}"
    )
    assert "Commands:" in result.stdout, (
        f"--help didn't list subcommands: {result.stdout!r}"
    )


@pytest.mark.smoke
def test_version_exits_zero_with_version_string(
    sandbox_with_shipyard: Sandbox,
) -> None:
    """``shipyard --version`` must exit 0 and print a recognisable
    version string. Catches a release that ships a binary that can't
    even introspect itself (the v0.15→v0.18 silent-fail class of bug)."""
    result = sandbox_with_shipyard.run(["--version"]).expect_success()
    combined = result.stdout + result.stderr
    assert "shipyard" in combined.lower() or "version" in combined.lower(), (
        f"--version output doesn't mention shipyard or 'version': "
        f"{combined!r}"
    )


# -----------------------------------------------------------------------------
# Scenario 2 — Mandatory regression: unknown subcommand is non-silent
# -----------------------------------------------------------------------------
#
# This is the canonical silent-exit-0 anti-pattern. Pulp shipped a real
# release where `pulp ship sign` printed "Unknown command: ship" and
# exited 0, silently breaking every plugin slash-command. The same
# class of bug could ship in Shipyard if Click's behavior ever drifts
# (e.g. a bare-`@click.group()` somewhere starts swallowing unknowns).
# This test guards against that drift.


@pytest.mark.smoke
def test_unknown_subcommand_exits_nonzero_with_stderr(
    sandbox_with_shipyard: Sandbox,
) -> None:
    """An unknown top-level subcommand must (a) exit non-zero AND
    (b) print something on stderr or stdout. Silent-exit-0 is the
    failure mode this test exists to prevent."""
    result = sandbox_with_shipyard.run(["frobnicate-totally-fake"], timeout=30.0)
    assert result.returncode != 0, (
        f"unknown subcommand exited 0 — silent-exit anti-pattern. "
        f"stdout: {result.stdout!r}\nstderr: {result.stderr!r}"
    )
    combined = result.stdout + result.stderr
    assert combined.strip(), (
        f"unknown subcommand exited {result.returncode} with NO output "
        f"on either stream — also silent-failure anti-pattern."
    )
    assert "frobnicate-totally-fake" in combined or "no such command" in combined.lower(), (
        f"unknown-subcommand error doesn't name the command or hint "
        f"at the failure mode: {combined!r}"
    )


@pytest.mark.smoke
def test_unknown_nested_subcommand_exits_nonzero_with_stderr(
    sandbox_with_shipyard: Sandbox,
) -> None:
    """Same contract as the top-level test, but for a nested group
    (``shipyard config bogus-subcmd``). Click's nested-group dispatch
    is a different code path so it needs its own probe."""
    result = sandbox_with_shipyard.run(
        ["config", "bogus-subcmd-totally-fake"], timeout=30.0
    )
    assert result.returncode != 0, (
        f"unknown nested subcommand exited 0. "
        f"stdout: {result.stdout!r}\nstderr: {result.stderr!r}"
    )
    combined = result.stdout + result.stderr
    assert combined.strip(), (
        f"unknown nested subcommand was silent. rc={result.returncode}"
    )


# -----------------------------------------------------------------------------
# Scenario 3 — Surface enumeration sanity
# -----------------------------------------------------------------------------


def test_shipyard_surface_is_non_empty(shipyard_surface: set[str]) -> None:
    """Sanity-check the regex extraction itself."""
    assert shipyard_surface, (
        "enumerate_shipyard_commands found no shipyard invocations "
        "anywhere — regex broken or surface roots empty"
    )
    # Spot-check tokens we know must be present given the docs we
    # shipped.
    for expected in {"status", "doctor", "queue"}:
        assert expected in shipyard_surface, (
            f"missing {expected!r} from surface enumeration: "
            f"{sorted(shipyard_surface)}"
        )


# -----------------------------------------------------------------------------
# Scenario 4 — Plugin / docs surface: every advertised command is non-silent
# -----------------------------------------------------------------------------


def _exercisable_commands(surface: set[str]) -> list[str]:
    return sorted(cmd for cmd in surface if cmd not in SURFACE_SKIPS)


@pytest.mark.surface
def test_every_advertised_command_help_is_nonsilent(
    sandbox_with_shipyard: Sandbox,
    shipyard_surface: set[str],
) -> None:
    """Every subcommand the user-facing surface advertises must respond
    to ``--help`` either by printing usage (rc=0) or by failing loudly
    (rc!=0 + something on stderr / "no such command" on stdout). The
    failure mode we're guarding against is the same silent-exit-0 bug
    as Scenario 2, but this time discovered against the real surface
    list rather than a synthetic name."""
    exercisable = _exercisable_commands(shipyard_surface)
    assert exercisable, "no exercisable commands after exclusions"

    failures: list[str] = []
    for cmd in exercisable:
        result = sandbox_with_shipyard.run([cmd, "--help"], timeout=30.0)
        if result.returncode == 0:
            # Happy path — Click printed usage.
            if not (result.stdout or result.stderr):
                failures.append(
                    f"{cmd} --help exited 0 but printed nothing — "
                    f"silent-success anti-pattern"
                )
            continue
        # Non-zero is OK iff something explained why.
        combined = result.stdout + result.stderr
        if combined.strip():
            continue
        failures.append(
            f"{cmd} --help exited {result.returncode} with no output "
            f"on either stream — silent-failure anti-pattern"
        )
    assert not failures, "\n".join(failures)


# -----------------------------------------------------------------------------
# Scenario 5 — Config layer: read-only access in an empty sandbox
# -----------------------------------------------------------------------------


@pytest.mark.config
def test_config_show_in_empty_sandbox_is_nonsilent(
    sandbox_with_shipyard: Sandbox,
) -> None:
    """``shipyard config show`` runs against zero ``.shipyard/`` and
    zero global config — it must either print an empty/default config
    or exit non-zero with a "no project config" hint. Silent-exit-0
    with no output is the failure mode."""
    result = sandbox_with_shipyard.run(["config", "show"], timeout=30.0)
    combined = result.stdout + result.stderr
    assert combined.strip(), (
        f"config show in an empty sandbox produced no output at all "
        f"(rc={result.returncode}) — silent-exit pattern"
    )


@pytest.mark.config
def test_config_profiles_in_empty_sandbox_is_nonsilent(
    sandbox_with_shipyard: Sandbox,
) -> None:
    """``shipyard config profiles`` with no profiles defined must
    print *something* (typically a "no profiles defined" hint) rather
    than silently exit."""
    result = sandbox_with_shipyard.run(["config", "profiles"], timeout=30.0)
    combined = result.stdout + result.stderr
    assert combined.strip(), (
        f"config profiles in an empty sandbox was silent. "
        f"rc={result.returncode}"
    )


# -----------------------------------------------------------------------------
# Scenario 6 — Queue / targets readback in an empty sandbox
# -----------------------------------------------------------------------------


@pytest.mark.queue
def test_queue_in_empty_sandbox_is_nonsilent(
    sandbox_with_shipyard: Sandbox,
) -> None:
    """An empty queue must still produce observable output ("No
    pending jobs" or similar). Silent-exit on an empty queue is the
    failure mode that masks "did the queue load at all?" bugs."""
    result = sandbox_with_shipyard.run(["queue"], timeout=30.0).expect_success()
    combined = result.stdout + result.stderr
    assert combined.strip(), (
        f"queue in an empty sandbox produced no output at all"
    )


@pytest.mark.queue
def test_targets_list_in_empty_sandbox_is_nonsilent(
    sandbox_with_shipyard: Sandbox,
) -> None:
    """``shipyard targets list`` against zero configured targets must
    print a hint (typically a header + "No targets" or an empty
    table). Catches a regression where the renderer crashes on an
    empty target set."""
    result = sandbox_with_shipyard.run(["targets", "list"], timeout=30.0)
    # Empty config can legitimately exit non-zero ("no project
    # config"); what matters is that it explained itself.
    combined = result.stdout + result.stderr
    assert combined.strip(), (
        f"targets list in an empty sandbox was silent. "
        f"rc={result.returncode}"
    )


# -----------------------------------------------------------------------------
# Scenario 7 — Daemon: status doesn't accidentally start the daemon
# -----------------------------------------------------------------------------


@pytest.mark.daemon
def test_daemon_status_in_empty_sandbox_does_not_start_daemon(
    sandbox_with_shipyard: Sandbox,
) -> None:
    """``shipyard daemon status`` must be a pure read; it must NOT
    spawn a long-lived daemon process or open a socket file. The
    contamination audit at teardown catches accidental writes; here we
    additionally verify the daemon socket file under the sandbox HOME
    was never created (which would also imply a server is bound to
    it)."""
    result = sandbox_with_shipyard.run(
        ["daemon", "status"], timeout=30.0
    )
    # Status must exit cleanly and report "not running" — but the
    # exact string varies by version, so just assert non-silent.
    combined = result.stdout + result.stderr
    assert combined.strip(), (
        f"daemon status was silent. rc={result.returncode}"
    )

    # Belt-and-braces: walk the per-platform daemon socket locations
    # under the sandbox $HOME and assert nothing was created.
    sandbox_home = sandbox_with_shipyard.home
    daemon_socket_locations = [
        sandbox_home / "Library" / "Application Support" / "shipyard" / "daemon" / "daemon.sock",
        sandbox_home / ".local" / "state" / "shipyard" / "daemon" / "daemon.sock",
        sandbox_home / "AppData" / "Local" / "shipyard" / "daemon" / "daemon.sock",
    ]
    for sock in daemon_socket_locations:
        assert not sock.exists(), (
            f"daemon status accidentally created a socket at {sock} — "
            f"a server may be running. Stop it manually before "
            f"re-running tests."
        )


# -----------------------------------------------------------------------------
# Scenario 8 — JSON parity: --json status is parseable + has stable keys
# -----------------------------------------------------------------------------


@pytest.mark.parity
def test_json_status_payload_is_well_formed(
    sandbox_with_shipyard: Sandbox,
) -> None:
    """``shipyard --json status`` must be parseable JSON and must
    include the schema keys that downstream consumers (Pulp's `/status`
    slash command, the daemon, CI tooling) depend on. A release that
    drops one of these keys is a breaking change that should land
    with a release-note bump, not silently."""
    result = sandbox_with_shipyard.run(
        ["--json", "status"], timeout=45.0
    ).expect_success()
    payload = parse_status_json(result.stdout)
    missing = [k for k in REQUIRED_STATUS_KEYS if k not in payload]
    assert not missing, (
        f"--json status is missing required keys {missing!r}; "
        f"payload keys: {sorted(payload)}"
    )
    # Sanity: the documented schema_version is an integer.
    assert isinstance(payload["schema_version"], int), (
        f"schema_version is not an int: {payload['schema_version']!r}"
    )


# -----------------------------------------------------------------------------
# Scenario 9 — `pin` outside a consumer repo is non-silent
# -----------------------------------------------------------------------------
#
# `shipyard pin` operates on consumer repos like pulp / spectr that
# pin a Shipyard version via tools/shipyard.toml. Inside an empty
# sandbox there's no such file, so `pin show` must fail loudly with a
# clear hint — the failure mode we're preventing is "exits 0 with no
# output, leaving the user wondering why the pin didn't change."


@pytest.mark.smoke
def test_pin_show_outside_consumer_repo_is_nonsilent(
    sandbox_with_shipyard: Sandbox,
) -> None:
    result = sandbox_with_shipyard.run(["pin", "show"], timeout=30.0)
    combined = result.stdout + result.stderr
    assert combined.strip(), (
        f"pin show outside a consumer repo was silent. "
        f"rc={result.returncode}, this is the silent-failure anti-pattern."
    )
    # Should hint at the missing pin file or the right context.
    assert (
        "shipyard.toml" in combined.lower()
        or "consumer" in combined.lower()
        or "pin" in combined.lower()
    ), (
        f"pin show error doesn't hint at the missing pin file or "
        f"context: {combined!r}"
    )


# -----------------------------------------------------------------------------
# Scenario 10 — Contamination audit smoke
# -----------------------------------------------------------------------------
#
# The ``sandbox`` fixture's teardown calls ``assert_no_contamination``
# after every test. That's the canonical audit. This explicit test
# makes the contract visible in reports even when everything else
# passes quietly, and guards against a future refactor where someone
# removes the teardown hook.


def test_contamination_audit_catches_writes_to_protected_paths(
    sandbox: Sandbox,
) -> None:
    """Smoke test for the audit itself. Touch a file under a
    sandboxed extra root, run the audit with that root injected, and
    confirm it flagged the leak. We never write to the real protected
    paths during this — that would be a genuine contamination."""
    fake_root = sandbox.root / "fake-protected"
    fake_root.mkdir()
    fake_file = fake_root / "leak.txt"
    fake_file.write_text("written after the sentinel")

    report = sandbox.audit_contamination(extra_roots=(fake_root,))
    assert not report.clean, "audit failed to detect a deliberate leak"
    assert any(
        str(p).endswith("leak.txt") for p in report.offenders
    ), f"expected leak.txt in offenders, got {report.offenders}"


# -----------------------------------------------------------------------------
# Scenario 11 — Sandbox.run refuses to invoke destructive subcommands
# -----------------------------------------------------------------------------
#
# Defence-in-depth: even if a future contributor writes
# `sandbox.run(["ship"])` by mistake, the harness must refuse to
# actually execute it. The DESTRUCTIVE_COMMANDS guard in
# ``shipyard_sandbox.py`` is the bulkhead. This test asserts the
# bulkhead is wired in.


@pytest.mark.smoke
def test_sandbox_refuses_to_invoke_ship(
    sandbox_with_shipyard: Sandbox,
) -> None:
    with pytest.raises(AssertionError, match="destructive"):
        sandbox_with_shipyard.run(["ship"])


@pytest.mark.smoke
def test_sandbox_refuses_to_invoke_pr(
    sandbox_with_shipyard: Sandbox,
) -> None:
    with pytest.raises(AssertionError, match="destructive"):
        sandbox_with_shipyard.run(["pr"])


@pytest.mark.smoke
def test_sandbox_refuses_to_invoke_cloud_run(
    sandbox_with_shipyard: Sandbox,
) -> None:
    """``cloud run`` dispatches a real GHA workflow. Read-only
    ``cloud workflows`` / ``cloud defaults`` / ``cloud status`` are
    allowed; anything else under ``cloud`` must be refused."""
    with pytest.raises(AssertionError, match="cloud"):
        sandbox_with_shipyard.run(["cloud", "run", "build"])


# -----------------------------------------------------------------------------
# JSON-flag ergonomics
# -----------------------------------------------------------------------------


@pytest.mark.parity
def test_json_status_returns_parseable_json_under_real_jsondecoder(
    sandbox_with_shipyard: Sandbox,
) -> None:
    """Belt-and-braces: not just our forgiving `parse_status_json`
    helper — strict ``json.loads`` on the raw stdout must succeed.
    Catches a regression where the renderer prints a banner before the
    JSON payload, which would silently break downstream consumers
    parsing stdout with the stdlib ``json`` module."""
    result = sandbox_with_shipyard.run(
        ["--json", "status"], timeout=45.0
    ).expect_success()
    try:
        json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        pytest.fail(
            f"--json status output is not strict-JSON: {exc}\n"
            f"stdout: {result.stdout!r}"
        )
