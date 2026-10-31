#!/usr/bin/env python3
"""Check that the autonomy contract names enforceable, testable obligations."""
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
CONTRACT = ROOT / "docs" / "autonomy-contract.md"
REQUIRED_IDS = [f"A{i}" for i in range(1, 13)]
REQUIRED_SURFACES = (
    ROOT / "src" / "queue_request.rs",
    ROOT / "src" / "ship_state.rs",
    ROOT / "src" / "recovery_worker.rs",
    ROOT / "scripts" / "test_autonomy_contract.py",
)
REQUIRED_FAILURES = (
    "original agent loss", "coordinator loss", "host reboot/offline",
    "dependency outage and return", "duplicate dispatch",
    "lost external acknowledgement", "stale worker mutation",
    "restart replay", "retry exhaustion",
    "authoritative post-remediation progress read",
)
REQUIRED_FAILURE_CASES = [f"F{i}" for i in range(1, 9)]
REQUIRED_BLOCKED_MARKERS = (
    "## Blocked-state invariants",
    "a deliberate `HOLD`, owner-held PR, or unresolved exact-head approval",
    "a queue-removal or merge-group guard",
    "missing, stale, contradictory, or unauthenticated provider evidence",
    "uncertain worker liveness, request identity, or host ownership",
    "Re-admission or redispatch is permitted only after a fresh authoritative read",
    "reconciliation preserves `blocked` and records the next deadline",
)
REQUIRED_EVIDENCE_SYMBOLS = (
    "dead_receipt_retirement_serializes_concurrent_replacement",
    "daemon_restart_adopts_exact_live_worker_without_replay",
    "drain_owner_recovers_stale_running_jobs",
    "crash_points_leave_no_partial_record_and_restart_recovers",
    "exact_head_match_refuses_duplicates",
    "preclaim_repository_error_durably_rotates_to_the_next_request",
)


def _evidence_rows(text: str) -> list[tuple[str, str]]:
    rows = []
    for line in text.splitlines():
        match = re.match(r"^\| (A(?:[1-9]|1[0-2])) \|", line)
        if not match:
            continue
        columns = [column.strip() for column in line.strip().strip("|").split("|")]
        if len(columns) == 6:
            rows.append((match.group(1), columns[5]))
    return rows


def _validate_evidence_rows(text: str, root: Path) -> None:
    """Require every evidence cell to name a real test or an explicit gap."""
    rows = _evidence_rows(text)
    if len(rows) != len(REQUIRED_IDS):
        raise SystemExit("every A1-A12 row must have an executable evidence cell")
    source_cache = {
        path.relative_to(root).as_posix(): path.read_text(encoding="utf-8", errors="replace")
        for path in root.joinpath("src").rglob("*.rs")
    }
    seen_symbols = set()
    for row_id, cell in rows:
        entries = [entry.strip() for entry in cell.split(";") if entry.strip()]
        if not entries:
            raise SystemExit(f"{row_id} executable evidence cell is empty")
        for entry in entries:
            if entry.startswith("gap:"):
                if not entry[4:].strip():
                    raise SystemExit(f"{row_id} has an empty gap evidence tag")
                continue
            if not entry.startswith("test:"):
                raise SystemExit(
                    f"{row_id} evidence must use 'test: path::symbol' or 'gap: reason': {entry}"
                )
            reference = entry[5:].strip()
            path_text, separator, symbol = reference.partition("::")
            if not separator or not path_text or not symbol or not re.fullmatch(r"[A-Za-z0-9_]+", symbol):
                raise SystemExit(f"{row_id} has malformed test evidence reference: {reference}")
            source = source_cache.get(path_text)
            if source is None:
                raise SystemExit(f"{row_id} cites missing evidence source: {path_text}")
            _validate_test_reference(row_id, path_text, symbol, source)
            seen_symbols.add(symbol)
    missing = [symbol for symbol in REQUIRED_EVIDENCE_SYMBOLS if symbol not in seen_symbols]
    if missing:
        raise SystemExit(
            "required evidence symbols must be cited in an A-row evidence cell: "
            + ", ".join(missing)
        )


def _validate_test_reference(row_id: str, path_text: str, symbol: str, source: str) -> None:
    function_match = re.search(
        rf"(?P<attrs>(?:\s*#\[[^\]]+\]\s*)*)"
        rf"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+{re.escape(symbol)}\s*\(",
        source,
    )
    if function_match is None:
        raise SystemExit(f"{row_id} cites missing evidence test: {path_text}::{symbol}")
    attrs = function_match.group("attrs")
    if not re.search(r"#\[(?:[A-Za-z_][A-Za-z0-9_]*::)?test(?:\s*=|\])", attrs):
        raise SystemExit(f"{row_id} cites a non-test function: {path_text}::{symbol}")
    if re.search(r"#\[ignore(?:\s*=|\])", attrs):
        raise SystemExit(f"{row_id} cites an ignored test: {path_text}::{symbol}")


def validate_contract(text: str, root: Path = ROOT) -> None:
    """Validate the contract against its source tree; raises SystemExit on drift."""
    folded = re.sub(r"\s+", " ", text.lower())
    missing = [item for item in REQUIRED_IDS if not re.search(rf"\b{item}\b", text)]
    if missing:
        raise SystemExit(f"missing autonomy invariant(s): {', '.join(missing)}")
    missing_blocked = [marker for marker in REQUIRED_BLOCKED_MARKERS if marker not in text]
    if missing_blocked:
        raise SystemExit(
            "blocked-state contract is missing required invariant(s): "
            + ", ".join(missing_blocked)
        )
    for path in (root / "AGENTS.md", root / "CLAUDE.md"):
        if "docs/autonomy-contract.md" not in path.read_text(encoding="utf-8"):
            raise SystemExit(f"{path} does not reference docs/autonomy-contract.md")
    for relative_surface in REQUIRED_SURFACES:
        surface = root / relative_surface.relative_to(ROOT)
        if not surface.is_file():
            raise SystemExit(f"contract names missing enforcement surface: {relative_surface.relative_to(ROOT)}")
    rust_sources = "\n".join(
        path.read_text(encoding="utf-8", errors="replace")
        for path in (root / "src").rglob("*.rs")
    )
    missing_symbols = [
        symbol for symbol in REQUIRED_EVIDENCE_SYMBOLS
        if not re.search(rf"\b(?:async\s+)?fn\s+{re.escape(symbol)}\s*\(", rust_sources)
    ]
    if missing_symbols:
        raise SystemExit(f"contract references missing executable evidence: {', '.join(missing_symbols)}")
    rows = [line for line in text.splitlines() if re.match(r"^\| A(?:[1-9]|1[0-2]) \|", line)]
    if len(rows) != len(REQUIRED_IDS):
        raise SystemExit(f"expected one evidence row per invariant, found {len(rows)}")
    row_ids = [row.split("|", 2)[1].strip() for row in rows]
    if set(row_ids) != set(REQUIRED_IDS) or len(row_ids) != len(set(row_ids)):
        raise SystemExit(f"invariant evidence rows must contain each A1-A12 exactly once: {row_ids}")
    for row in rows:
        columns = [column.strip() for column in row.strip().strip("|").split("|")]
        if len(columns) != 6 or any(not column for column in columns[1:]):
            raise SystemExit(f"incomplete evidence row: {row}")
    matrix_rows = [line for line in text.splitlines() if re.match(r"^\| F[1-8] \|", line)]
    if len(matrix_rows) != len(REQUIRED_FAILURE_CASES):
        raise SystemExit(f"expected one integrated failure row per case, found {len(matrix_rows)}")
    matrix_ids = [row.split("|", 2)[1].strip() for row in matrix_rows]
    if set(matrix_ids) != set(REQUIRED_FAILURE_CASES) or len(matrix_ids) != len(set(matrix_ids)):
        raise SystemExit(f"failure matrix must contain each F1-F8 exactly once: {matrix_ids}")
    for row in matrix_rows:
        columns = [column.strip() for column in row.strip().strip("|").split("|")]
        if len(columns) != 5 or any(not column for column in columns[1:]):
            raise SystemExit(f"incomplete failure/liveness row: {row}")
    for phrase in REQUIRED_FAILURES + ("authoritative post-remediation progress", "controller restart", "duplicate effects"):
        if phrase not in folded:
            raise SystemExit(f"contract missing required recovery proof: {phrase}")
    _validate_evidence_rows(text, root)


def main() -> int:
    if not CONTRACT.is_file():
        raise SystemExit("missing docs/autonomy-contract.md")
    text = CONTRACT.read_text(encoding="utf-8")
    validate_contract(text)
    print("autonomy contract: OK (A1-A12, enforcement surfaces, evidence rows, F1-F8 liveness matrix, recovery proof requirements)")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
