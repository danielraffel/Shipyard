"""First-run disclosure visibility + ack-idempotency tests."""

from __future__ import annotations

import io
from pathlib import Path

from shipyard.daemon import disclosure


def test_notice_mentions_tailscale_and_opt_out(tmp_path: Path) -> None:
    text = disclosure.render(repos=["org/repo"])
    # These are the core claims the notice must make — if they ever
    # regress, the test will catch it.
    assert "Tailscale Funnel" in text
    assert "GitHub webhook" in text
    assert "HMAC secret" in text
    assert "polling" in text  # explicit fallback path
    assert "org/repo" in text


def test_notice_lists_no_repos_gracefully() -> None:
    text = disclosure.render(repos=[])
    assert "(none detected" in text


def test_shown_once_per_state_dir(tmp_path: Path) -> None:
    stream = io.StringIO()
    first = disclosure.show_if_first_run(tmp_path, ["org/repo"], stream=stream)
    assert first is True
    assert "Tailscale Funnel" in stream.getvalue()

    # Second call: already-acked, no output.
    stream2 = io.StringIO()
    second = disclosure.show_if_first_run(tmp_path, ["org/repo"], stream=stream2)
    assert second is False
    assert stream2.getvalue() == ""


def test_ack_marker_at_expected_path(tmp_path: Path) -> None:
    assert not disclosure.has_been_shown(tmp_path)
    disclosure.mark_shown(tmp_path)
    assert disclosure.has_been_shown(tmp_path)
    assert (tmp_path / "daemon" / ".first-run-acked").exists()
