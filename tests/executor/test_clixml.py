"""CLIXML decoder regression tests (#188).

Pre-fix, ssh-windows backend errors surfaced as literal
``#< CLIXML <Objs>…</Objs>`` under the summary table — gibberish.
These tests pin the decoder to the real shapes PowerShell emits:

- plain ``<S S="Error">`` stream records (Write-Error output)
- mixed Error + Warning streams
- nested ``<Obj>`` with ``<S N="Message">`` (full ErrorRecord)
- concatenated back-to-back envelopes from multiple stderr flushes
- ``_xHHHH_`` character-escape decoding for CR/LF
- malformed payload → fall through to raw (never raise)
"""

from __future__ import annotations

from shipyard.executor.clixml import is_clixml, maybe_decode_clixml


def test_plain_text_passes_through_unchanged() -> None:
    assert maybe_decode_clixml("just a regular error\nline 2") == (
        "just a regular error\nline 2"
    )
    assert is_clixml("just a regular error") is False


def test_write_error_stream_decoded() -> None:
    envelope = (
        '#< CLIXML\r\n'
        '<Objs Version="1.1.0.1" xmlns="http://schemas.microsoft.com/'
        'powershell/2004/04">'
        '<S S="Error">intentional failure_x000D__x000A_</S>'
        '</Objs>'
    )
    out = maybe_decode_clixml(envelope)
    assert "intentional failure" in out
    assert "#< CLIXML" not in out
    assert "<Objs" not in out


def test_error_and_warning_both_extracted() -> None:
    envelope = (
        '#< CLIXML\n'
        '<Objs xmlns="http://schemas.microsoft.com/powershell/2004/04">'
        '<S S="Warning">slow disk detected</S>'
        '<S S="Error">git exit 1: refusing to fetch</S>'
        '</Objs>'
    )
    out = maybe_decode_clixml(envelope)
    assert "slow disk detected" in out
    assert "git exit 1: refusing to fetch" in out


def test_error_record_message_property_extracted() -> None:
    # ErrorRecord serializations nest a <Props> block with a
    # <S N="Message"> giving the exception text. Real-world example
    # shape: System.IO.IOException with "file in use" message.
    envelope = (
        '#< CLIXML\n'
        '<Objs xmlns="http://schemas.microsoft.com/powershell/2004/04">'
        '<Obj>'
        '<Props>'
        '<S N="Message">The process cannot access the file because it '
        'is being used by another process.</S>'
        '<S N="FullyQualifiedErrorId">System.IO.IOException</S>'
        '</Props>'
        '</Obj>'
        '</Objs>'
    )
    out = maybe_decode_clixml(envelope)
    assert "being used by another process" in out


def test_concatenated_envelopes_both_decoded() -> None:
    # PowerShell can flush stderr multiple times; receiver sees two
    # back-to-back envelopes. Decoder must pick up both.
    envelope = (
        '#< CLIXML\n'
        '<Objs xmlns="http://schemas.microsoft.com/powershell/2004/04">'
        '<S S="Error">first error</S>'
        '</Objs>'
        '<Objs xmlns="http://schemas.microsoft.com/powershell/2004/04">'
        '<S S="Error">second error</S>'
        '</Objs>'
    )
    out = maybe_decode_clixml(envelope)
    assert "first error" in out
    assert "second error" in out


def test_xhhhh_escape_sequences_decoded() -> None:  # noqa: N802 — mirror the PowerShell CLIXML escape form
    # PowerShell encodes CR/LF/tab as _x000D_/_x000A_/_x0009_ because
    # the naïve XML 1.0 character set doesn't allow them in content.
    envelope = (
        '#< CLIXML\n'
        '<Objs xmlns="http://schemas.microsoft.com/powershell/2004/04">'
        '<S S="Error">line1_x000D__x000A_line2</S>'
        '</Objs>'
    )
    out = maybe_decode_clixml(envelope)
    assert "line1" in out
    assert "line2" in out
    # Escapes must not leak through literally.
    assert "_x000D_" not in out
    assert "_x000A_" not in out


def test_malformed_xml_falls_back_to_raw() -> None:
    envelope = '#< CLIXML\n<Objs>unclosed'
    # Must not raise; must return the raw envelope unchanged so the
    # caller's original "at least show something" behaviour holds.
    out = maybe_decode_clixml(envelope)
    assert out == envelope


def test_empty_envelope_falls_back_to_raw() -> None:
    envelope = '#< CLIXML\n'
    out = maybe_decode_clixml(envelope)
    # No Objs document, nothing to decode → return unchanged.
    assert out == envelope


def test_envelope_with_leading_whitespace_still_detected() -> None:
    # Sometimes the stream begins with a newline or extra space
    # before the sentinel.
    envelope = (
        '\n   #< CLIXML\n'
        '<Objs xmlns="http://schemas.microsoft.com/powershell/2004/04">'
        '<S S="Error">wrapped</S>'
        '</Objs>'
    )
    assert is_clixml(envelope)
    out = maybe_decode_clixml(envelope)
    assert "wrapped" in out
    assert "<Objs" not in out


def test_very_long_output_truncated_from_head() -> None:
    # 5000-char error chain; output must keep the tail (the proximate
    # cause) and drop the head with an ellipsis marker.
    long_text = " ".join(f"line{i}" for i in range(1000))
    envelope = (
        '#< CLIXML\n'
        '<Objs xmlns="http://schemas.microsoft.com/powershell/2004/04">'
        f'<S S="Error">{long_text}</S>'
        '</Objs>'
    )
    out = maybe_decode_clixml(envelope)
    assert len(out) <= 900  # _MAX_DECODED_CHARS + ellipsis margin
    assert out.startswith("…")
    # The very last line must still be in the output.
    assert "line999" in out
