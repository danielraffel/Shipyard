"""render_doctor must show `detail` on failing rows (Codex P2 on #214).

Before the fix, render_doctor only rendered `version` (via
``info.get("version", info.get("error", info.get("detail", "")))``),
so a failing check's actionable recovery text — stored in
``detail`` — never reached the non-JSON user. Doctor would say
``✗ rich-bundle rich bundle read failed (zlib.error)`` with no
reinstall command, leaving users stranded.
"""

from __future__ import annotations

import io
from contextlib import redirect_stdout


def _render(checks: dict) -> str:
    # render_doctor prints via rich.console.Console; redirect stdout
    # and strip rich's style markup by telling Console to render
    # plain. Simpler: capture stdout raw — rich uses ANSI codes but
    # the text content is still assertable.
    from shipyard.output.human import render_doctor
    buf = io.StringIO()
    with redirect_stdout(buf):
        render_doctor(checks, ready=False)
    # Strip ANSI escape sequences for stable assertions.
    import re
    return re.sub(r"\x1b\[[0-9;]*m", "", buf.getvalue())


def test_failure_row_surfaces_detail_below_summary() -> None:
    # A check with ok=False AND a detail must render the detail as
    # an indented follow-up line. Rich-bundle is the motivating case
    # (#214 P2): the reinstall command lives in detail.
    out = _render({
        "Core": {
            "rich-bundle": {
                "ok": False,
                "version": "rich bundle read failed (zlib.error)",
                "detail": (
                    "Error -3 while decompressing data: incorrect header check\n"
                    "  Fix: Reinstall with: curl -fsSL https://example/install.sh | sh"
                ),
            },
        },
    })
    # Summary line present.
    assert "rich bundle read failed (zlib.error)" in out
    # Detail surfaced too — the fix command must be visible.
    assert "incorrect header check" in out
    assert "curl -fsSL https://example/install.sh | sh" in out


def test_passing_row_suppresses_detail() -> None:
    # On ok=True rows, detail stays hidden. Healthy checks don't
    # need multi-line recovery text cluttering the output.
    out = _render({
        "Core": {
            "git": {
                "ok": True,
                "version": "git version 2.45.0",
                "detail": "this should NOT appear",
            },
        },
    })
    assert "git version 2.45.0" in out
    assert "this should NOT appear" not in out


def test_failure_row_without_detail_still_renders() -> None:
    # Backward compat: a failing check that has only `version`
    # (no detail) renders the same one-line shape as before — no
    # crash, no stray "None" text.
    out = _render({
        "Core": {
            "something": {
                "ok": False,
                "version": "broke",
            },
        },
    })
    assert "broke" in out
    assert "None" not in out


def test_detail_same_as_version_is_not_duplicated() -> None:
    # If detail == version (some checks do this), don't double-
    # render. Keeps the output tight.
    out = _render({
        "Core": {
            "x": {
                "ok": False,
                "version": "same text",
                "detail": "same text",
            },
        },
    })
    assert out.count("same text") == 1
