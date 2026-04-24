"""Decode PowerShell CLIXML error envelopes surfaced by remote
Windows commands over SSH.

When a Windows PowerShell session writes to any non-stdout stream
(``Write-Error``, uncaught exceptions, ``Write-Warning``, etc.) and
the stream is relayed back to the SSH client, the receiving side
sees a payload that starts with the literal bytes ``#< CLIXML``
followed by a PowerShell CLIXML (Common Language Infrastructure
XML) document. The actual human-readable diagnostic — "file in
use," "git exit 1," "missing dependency," etc. — is buried inside
``<S S="Error">…</S>`` elements in that XML, not in plain text.

Pre-decoder, shipyard's summary row looked like::

    ✗ windows: Bundle apply failed: Remote bundle apply failed: #< CLIXML

Which is exactly as useful as the bare ``windows  error  ssh-windows``
row we fixed in v0.28.0. This module closes that gap (#188): if the
stderr buffer starts with the CLIXML sentinel, we extract the error
and warning stream text, decode PowerShell's ``_x000D_``/``_x000A_``
escape sequences back to CR/LF, and hand the caller plain text that
names the actual failure.

Malformed or partially-transferred envelopes fall through to the
original raw text — the pre-decoder behaviour — so we never make a
failure case worse.
"""

from __future__ import annotations

import re
from xml.etree import ElementTree as ET

# PowerShell's CLIXML stream-header sentinel. Case-insensitive; older
# hosts sometimes send the header with a different whitespace between
# `#<` and `CLIXML`. Any stderr buffer whose first non-whitespace
# characters match this gets routed through `_decode`.
_CLIXML_SENTINEL = "#< CLIXML"

# Powershell serializes the XML 1.0 "restricted" character set by
# encoding forbidden code points as ``_xHHHH_`` (four-digit hex).
# Most commonly ``_x000D_`` / ``_x000A_`` / ``_x0009_`` for CR / LF /
# tab in multi-line error messages. Decode them back so the printed
# text preserves readable line breaks.
_ESCAPE_RE = re.compile(r"_x([0-9A-Fa-f]{4})_")

# Cap the decoded payload so a multi-kilobyte traceback doesn't
# balloon the terminal when surfaced beneath the summary table. The
# per-target log file still carries the full envelope.
_MAX_DECODED_CHARS = 800


def is_clixml(text: str) -> bool:
    """True iff ``text`` starts with a CLIXML envelope header."""
    return text.lstrip().startswith(_CLIXML_SENTINEL)


def maybe_decode_clixml(text: str) -> str:
    """Best-effort: if ``text`` looks like a CLIXML envelope, return
    the decoded human-readable error. Otherwise return ``text`` as-is.

    Never raises — malformed CLIXML falls back to the original text
    so callers' existing error surfaces keep working on partial or
    non-standard envelopes.

    Text that appears *before* the CLIXML sentinel is also surfaced
    (#210). PowerShell's progress-stream CLIXML can arrive AFTER
    unrelated stderr lines written by child processes (git, cmake,
    ctest, etc.) — those pre-sentinel bytes used to be dropped
    because the decoder started at the sentinel. Now they're
    prefixed to the decoded envelope (or returned alone if the
    envelope body is empty), so "error: could not open '...'"
    style messages survive even when wrapped in a later CLIXML
    progress object.
    """
    # Fast path: no sentinel anywhere → plain text, nothing to do.
    # If the sentinel IS present (whether at the start or mid-string
    # after pre-sentinel stderr), fall through to _split_and_decode.
    if not is_clixml(text) and _CLIXML_SENTINEL not in text:
        return text
    try:
        prefix, decoded = _split_and_decode(text)
    except (ET.ParseError, ValueError):
        return text
    # Join pre-sentinel text and decoded body; either can be empty.
    parts = [p for p in (prefix.strip(), decoded.strip()) if p]
    joined = "\n".join(parts)
    return joined or text


def _split_and_decode(text: str) -> tuple[str, str]:
    """Return ``(pre_sentinel_text, decoded_envelope)``.

    Split on the first occurrence of ``#< CLIXML``; decode everything
    after as one-or-more ``<Objs>…</Objs>`` documents. Pre-sentinel
    text is returned verbatim. Either half may be empty.
    """
    idx = text.find(_CLIXML_SENTINEL)
    if idx < 0:
        return (text, "")
    prefix = text[:idx]
    xml_blob = text[idx + len(_CLIXML_SENTINEL):].lstrip()

    messages: list[str] = []
    for obj_doc in _split_objs(xml_blob):
        messages.extend(_extract_messages(obj_doc))
    if not messages:
        return (prefix, "")

    deduped: list[str] = []
    for m in messages:
        stripped = m.strip()
        if not stripped:
            continue
        if deduped and deduped[-1] == stripped:
            continue
        deduped.append(stripped)

    joined = "\n".join(deduped)
    if len(joined) > _MAX_DECODED_CHARS:
        joined = "…" + joined[-(_MAX_DECODED_CHARS - 1):]
    return (prefix, joined)


def _decode(text: str) -> str:
    # Strip everything up to and including the sentinel; the rest
    # should be a well-formed XML document.
    idx = text.find(_CLIXML_SENTINEL)
    xml_blob = text[idx + len(_CLIXML_SENTINEL):].lstrip()

    # The stream can contain multiple concatenated ``<Objs>…</Objs>``
    # documents back-to-back (one per stderr flush). Parse each
    # independently so a malformed trailing doc doesn't throw away
    # an earlier well-formed one.
    messages: list[str] = []
    for obj_doc in _split_objs(xml_blob):
        messages.extend(_extract_messages(obj_doc))
    if not messages:
        return ""

    # Preserve order, dedupe consecutive exact-same lines (PowerShell
    # tends to emit both a short ``Message`` and a longer
    # ``FullyQualifiedErrorId`` derived from the same exception).
    deduped: list[str] = []
    for m in messages:
        stripped = m.strip()
        if not stripped:
            continue
        if deduped and deduped[-1] == stripped:
            continue
        deduped.append(stripped)

    joined = "\n".join(deduped)
    if len(joined) > _MAX_DECODED_CHARS:
        # Keep the tail — last errors in a multi-line cascade are
        # almost always the proximate cause.
        joined = "…" + joined[-(_MAX_DECODED_CHARS - 1):]
    return joined


def _split_objs(xml_blob: str) -> list[str]:
    """Split a concatenation of ``<Objs>…</Objs>`` documents into a
    list of individual XML strings. Returns ``[xml_blob]`` unchanged
    when only one document is present; empty list when the blob is
    empty or shaped wrong for recovery."""
    out: list[str] = []
    start = 0
    while start < len(xml_blob):
        open_idx = xml_blob.find("<Objs", start)
        if open_idx < 0:
            break
        close_idx = xml_blob.find("</Objs>", open_idx)
        if close_idx < 0:
            break
        end = close_idx + len("</Objs>")
        out.append(xml_blob[open_idx:end])
        start = end
    return out


def _extract_messages(obj_doc: str) -> list[str]:
    """Pull the human-readable text out of a single ``<Objs>``
    document. Looks at both ``<S S="...">`` stream records (the
    common case: ``Write-Error`` / ``Write-Warning`` output) and
    nested ``<Obj>`` records with ``<S N="Message">`` /
    ``<S N="Exception">`` properties (full ``ErrorRecord``
    serializations)."""
    root = ET.fromstring(obj_doc)

    # PowerShell CLIXML uses a default namespace on ``<Objs>``. ElementTree
    # preserves that as ``{ns}tag`` when walking, so match on local
    # names without hardcoding the namespace URI.
    results: list[str] = []
    for elem in root.iter():
        tag = _local_name(elem.tag)
        if tag == "S":
            # Stream record: S="Error" / S="Warning" / S="Verbose" /
            # etc. Or property record: N="Message" / N="Exception".
            stream = elem.get("S", "")
            prop_name = elem.get("N", "")
            if (
                stream in {"Error", "Warning"}
                or prop_name in {"Message", "Exception", "FullyQualifiedErrorId"}
            ):
                results.append(_decode_escapes(elem.text or ""))
    return results


def _local_name(tag: str) -> str:
    # Strip ``{ns}`` prefix from ``ElementTree.Element.tag``.
    if "}" in tag:
        return tag.split("}", 1)[1]
    return tag


def _decode_escapes(raw: str) -> str:
    """Decode PowerShell's ``_xHHHH_`` escapes back to real chars."""
    def _sub(m: re.Match[str]) -> str:
        try:
            return chr(int(m.group(1), 16))
        except ValueError:
            return m.group(0)
    return _ESCAPE_RE.sub(_sub, raw)
