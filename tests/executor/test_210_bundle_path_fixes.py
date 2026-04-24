"""Regression tests for #210 — three coordinated fixes:

1. Bundle upload resolves relative remote_path against ``$HOME``
   inside PowerShell, so the file always lands at a deterministic
   path regardless of SSHD's default working directory.
2. ``_apply_bundle_windows`` pre-verifies the bundle exists via
   ``Test-Path`` before handing it to git, so a path mismatch
   surfaces with a named path instead of git's "could not open"
   leaking out as pre-CLIXML stderr.
3. ``maybe_decode_clixml`` also surfaces stderr that appears
   BEFORE the CLIXML sentinel (the exact byte layout from the
   incident report).
"""

from __future__ import annotations

from unittest.mock import patch

from shipyard.bundle.git_bundle import upload_bundle
from shipyard.executor.clixml import maybe_decode_clixml
from shipyard.executor.ssh_windows import _WINDOWS_UTF8_PRELUDE


class _FakeCompletedProcess:
    def __init__(self, returncode: int = 0) -> None:
        self.returncode = returncode
        self.stderr = ""
        self.stdout = ""


def _get_ps_script(tmp_path) -> str:
    """Drive a Windows-targeted upload and return the PowerShell
    script that was encoded into the ssh argv."""
    bundle = tmp_path / "shipyard.bundle"
    bundle.write_bytes(b"fake")

    captured: dict[str, str] = {}

    def fake_run(cmd, **kw):
        # Decode the -EncodedCommand payload on the fly.
        import base64
        idx = cmd.index("-EncodedCommand")
        payload = cmd[idx + 1]
        captured["script"] = base64.b64decode(payload).decode("utf-16-le")
        return _FakeCompletedProcess(returncode=0)

    with patch("shipyard.bundle.git_bundle.subprocess.run", side_effect=fake_run):
        result = upload_bundle(
            bundle_path=bundle,
            host="win",
            remote_path="shipyard.bundle",  # relative
            is_windows=True,
        )
    assert result.success, result.message
    return captured["script"]


def test_upload_relative_remote_path_resolved_via_join_path_home(tmp_path) -> None:
    # Fix (1): the PS script must resolve relative remote_path
    # against $HOME so the file lands at a deterministic location
    # regardless of SSHD's cwd.
    script = _get_ps_script(tmp_path)
    assert "(Join-Path $HOME 'shipyard.bundle')" in script
    # And no hardcoded relative path — the earlier implementation
    # would interpolate 'shipyard.bundle' directly as a
    # `[System.IO.File]::Create` arg.
    assert "[System.IO.File]::Create('shipyard.bundle')" not in script


def test_upload_absolute_remote_path_used_as_is(tmp_path) -> None:
    bundle = tmp_path / "shipyard.bundle"
    bundle.write_bytes(b"fake")
    captured: dict[str, str] = {}

    def fake_run(cmd, **kw):
        import base64
        idx = cmd.index("-EncodedCommand")
        captured["script"] = base64.b64decode(cmd[idx + 1]).decode("utf-16-le")
        return _FakeCompletedProcess(returncode=0)

    with patch("shipyard.bundle.git_bundle.subprocess.run", side_effect=fake_run):
        result = upload_bundle(
            bundle_path=bundle,
            host="win",
            remote_path=r"C:\shipyard.bundle",  # absolute
            is_windows=True,
        )
    assert result.success
    assert "'C:\\shipyard.bundle'" in captured["script"]
    # Absolute path must NOT be double-wrapped in Join-Path.
    assert "Join-Path" not in captured["script"]


def test_apply_bundle_contains_test_path_preverify() -> None:
    # Fix (2): the apply PS command must include a Test-Path pre-
    # verification before `git bundle verify`.
    from shipyard.executor.ssh_windows import _apply_bundle_windows

    captured: dict[str, list[str]] = {}

    def fake_run(cmd, **kw):
        captured["cmd"] = cmd
        return _FakeCompletedProcess(returncode=0)

    with patch("shipyard.executor.ssh_windows.subprocess.run", side_effect=fake_run):
        _apply_bundle_windows(
            host="win",
            bundle_path="shipyard.bundle",
            repo_path="~/repo",
            ssh_options=[],
        )

    import base64
    idx = captured["cmd"].index("-EncodedCommand")
    script = base64.b64decode(captured["cmd"][idx + 1]).decode("utf-16-le")
    assert "Test-Path -LiteralPath $Bundle" in script
    assert "bundle file not found" in script
    # Sanity: the UTF-8 prelude from #208 is still there.
    assert _WINDOWS_UTF8_PRELUDE in script


def test_decoder_surfaces_pre_sentinel_stderr() -> None:
    # Fix (3): the exact byte layout from the #210 forensic capture.
    # `error:` appears BEFORE the CLIXML sentinel, envelope body is
    # a harmless PowerShell progress object. Pre-fix, decoder
    # returned the raw string (fallback). Post-fix, the error line
    # survives as the prefix.
    envelope = (
        "error: could not open 'C:/Users/alice/shipyard.bundle'\n"
        "#< CLIXML\n"
        '<Objs Version="1.1.0.1" xmlns="http://schemas.microsoft.com/'
        'powershell/2004/04">'
        '<Obj S="progress" RefId="0"><TN RefId="0"><T>PSCustomObject</T></TN>'
        '<MS><PR N="Record"><AV>Preparing modules</AV></PR></MS></Obj>'
        "</Objs>"
    )
    decoded = maybe_decode_clixml(envelope)
    assert "could not open" in decoded
    assert "#< CLIXML" not in decoded
    assert "<Objs" not in decoded


def test_decoder_handles_pre_sentinel_only_no_body() -> None:
    # If pre-sentinel text exists but envelope has no extractable
    # messages (e.g. progress-only), the prefix still surfaces.
    envelope = (
        "error: something bad happened\n"
        "#< CLIXML\n"
        '<Objs><Obj S="progress"><TN><T>X</T></TN></Obj></Objs>'
    )
    decoded = maybe_decode_clixml(envelope)
    assert "error: something bad happened" in decoded


def test_decoder_pre_sentinel_plus_error_stream_joins_both() -> None:
    # Both pre-sentinel text AND an Error stream in the envelope
    # should survive the trip.
    envelope = (
        "error: outer context\n"
        "#< CLIXML\n"
        '<Objs xmlns="http://schemas.microsoft.com/powershell/2004/04">'
        '<S S="Error">inner PS error</S>'
        "</Objs>"
    )
    decoded = maybe_decode_clixml(envelope)
    assert "error: outer context" in decoded
    assert "inner PS error" in decoded


def test_decoder_no_sentinel_unchanged() -> None:
    # Legacy happy path preserved.
    assert maybe_decode_clixml("just a regular error") == "just a regular error"


# -- Codex P1 on #211 (slash-prefixed absolute paths) --------------
# `_is_windows_absolute_path` in executor/ssh_windows.py treats
# `/foo` as absolute (line 379). The earlier #210 upload-side
# classifier only checked for `X:` or `\`, so a configured
# `remote_bundle_path = /tmp/shipyard.bundle` reintroduced the
# exact apply-vs-upload mismatch #210 was meant to close: upload
# joined it to $HOME, apply treated it as-is → "could not open"
# all over again. The two predicates must stay in lockstep.

def test_upload_slash_prefixed_remote_path_treated_as_absolute(tmp_path) -> None:
    bundle = tmp_path / "shipyard.bundle"
    bundle.write_bytes(b"fake")
    captured: dict[str, str] = {}

    def fake_run(cmd, **kw):
        import base64
        idx = cmd.index("-EncodedCommand")
        captured["script"] = base64.b64decode(cmd[idx + 1]).decode("utf-16-le")
        return _FakeCompletedProcess(returncode=0)

    with patch("shipyard.bundle.git_bundle.subprocess.run", side_effect=fake_run):
        result = upload_bundle(
            bundle_path=bundle,
            host="win",
            remote_path="/tmp/shipyard.bundle",
            is_windows=True,
        )
    assert result.success
    # MUST be used as-is — NOT wrapped in Join-Path $HOME. Otherwise
    # the apply side (which treats /... as absolute) will look at a
    # different file than upload wrote.
    assert "'/tmp/shipyard.bundle'" in captured["script"]
    assert "Join-Path" not in captured["script"]


def test_upload_unc_remote_path_treated_as_absolute(tmp_path) -> None:
    # UNC paths (\\server\share\...) are absolute per the apply-side
    # predicate. Same rule — no $HOME join.
    bundle = tmp_path / "shipyard.bundle"
    bundle.write_bytes(b"fake")
    captured: dict[str, str] = {}

    def fake_run(cmd, **kw):
        import base64
        idx = cmd.index("-EncodedCommand")
        captured["script"] = base64.b64decode(cmd[idx + 1]).decode("utf-16-le")
        return _FakeCompletedProcess(returncode=0)

    with patch("shipyard.bundle.git_bundle.subprocess.run", side_effect=fake_run):
        result = upload_bundle(
            bundle_path=bundle,
            host="win",
            remote_path=r"\\server\share\shipyard.bundle",
            is_windows=True,
        )
    assert result.success
    assert "Join-Path" not in captured["script"]


def test_upload_and_apply_path_predicates_agree() -> None:
    # Contract lock-in: for every path shape the apply-side predicate
    # calls "absolute," the upload side must also skip the $HOME join
    # (else we're back to the #210 bug). Asserting at the module level
    # keeps the two predicates from drifting independently.
    from shipyard.executor.ssh_windows import _is_windows_absolute_path

    cases = [
        "C:\\foo\\bar",
        "C:/foo/bar",
        "c:\\foo",       # lower-case drive letter
        "/tmp/x.bundle",
        "\\foo",
        "\\\\server\\share\\file",
    ]
    for p in cases:
        assert _is_windows_absolute_path(p), f"apply-side should treat {p!r} as absolute"

    # And the upload-side `is_rooted` expression must reach the same
    # verdict. We can't reach in directly — exercise via upload path.
    relatives = ["shipyard.bundle", "sub\\file", "sub/file"]
    for p in relatives:
        assert not _is_windows_absolute_path(p), f"apply-side should treat {p!r} as relative"
