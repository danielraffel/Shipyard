"""Tests for the doc-sync gate script (#101 Phase C)."""

from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path

import pytest


def _load_script() -> object:
    """Load scripts/doc_sync_check.py as a module."""
    script = (
        Path(__file__).resolve().parent.parent
        / "scripts"
        / "doc_sync_check.py"
    )
    spec = importlib.util.spec_from_file_location("doc_sync_check", script)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["doc_sync_check"] = module
    spec.loader.exec_module(module)
    return module


doc_sync_check = _load_script()


def test_load_doc_map_reads_valid_config(tmp_path: Path) -> None:
    map_path = tmp_path / "doc_sync_map.json"
    map_path.write_text(json.dumps({
        "docs": {
            "docs/x.md": {
                "description": "X audit",
                "paths": ["src/x/**", "src/shared/x.py"],
            }
        }
    }))
    doc_map = doc_sync_check.load_doc_map(map_path)
    assert len(doc_map.docs) == 1
    assert doc_map.docs[0].doc == "docs/x.md"
    assert "src/x/**" in doc_map.docs[0].paths


def test_load_doc_map_strips_underscore_keys(tmp_path: Path) -> None:
    map_path = tmp_path / "doc_sync_map.json"
    map_path.write_text(json.dumps({
        "_comment": "explanatory JSON comment — should be ignored",
        "schema_version": 1,
        "docs": {"docs/x.md": {"paths": ["a.py"]}},
    }))
    doc_map = doc_sync_check.load_doc_map(map_path)
    assert len(doc_map.docs) == 1


def test_load_doc_map_rejects_bad_shape(tmp_path: Path) -> None:
    map_path = tmp_path / "bad.json"
    map_path.write_text(json.dumps({"docs": "not an object"}))
    with pytest.raises(ValueError):
        doc_sync_check.load_doc_map(map_path)


def test_matches_any_handles_recursive_globs() -> None:
    patterns = ["src/shipyard/ship/**", "src/shipyard/core/ship_state.py"]
    assert doc_sync_check._matches_any("src/shipyard/ship/pr.py", patterns)
    assert doc_sync_check._matches_any(
        "src/shipyard/ship/subdir/deep.py", patterns
    )
    assert doc_sync_check._matches_any(
        "src/shipyard/core/ship_state.py", patterns
    )
    assert not doc_sync_check._matches_any(
        "src/shipyard/cli.py", patterns
    )


def test_find_violations_clean_when_doc_updated_with_code(
    tmp_path: Path,
) -> None:
    doc_map = doc_sync_check.DocMap(docs=[
        doc_sync_check.DocEntry(
            doc="docs/x.md",
            description="",
            paths=["src/x/**"],
        )
    ])
    findings = doc_sync_check.find_violations(
        doc_map,
        changed=["src/x/changed.py", "docs/x.md"],
        trailers={},
    )
    assert len(findings) == 1
    assert findings[0].doc_modified is True
    assert findings[0].bypass_reason is None


def test_find_violations_flags_untouched_doc(tmp_path: Path) -> None:
    doc_map = doc_sync_check.DocMap(docs=[
        doc_sync_check.DocEntry(
            doc="docs/x.md",
            description="",
            paths=["src/x/**"],
        )
    ])
    findings = doc_sync_check.find_violations(
        doc_map,
        changed=["src/x/changed.py"],  # doc not in diff
        trailers={},
    )
    assert len(findings) == 1
    assert findings[0].doc_modified is False
    assert findings[0].bypass_reason is None


def test_find_violations_honors_skip_trailer() -> None:
    doc_map = doc_sync_check.DocMap(docs=[
        doc_sync_check.DocEntry(
            doc="docs/x.md",
            description="",
            paths=["src/x/**"],
        )
    ])
    findings = doc_sync_check.find_violations(
        doc_map,
        changed=["src/x/changed.py"],
        trailers={
            "doc-update": [
                'skip doc=docs/x.md reason="mechanical rename, no audit change"'
            ]
        },
    )
    assert len(findings) == 1
    assert findings[0].doc_modified is False
    assert findings[0].bypass_reason == "mechanical rename, no audit change"


def test_find_violations_skips_only_named_doc() -> None:
    """A skip trailer for docs/x.md doesn't bypass docs/y.md."""
    doc_map = doc_sync_check.DocMap(docs=[
        doc_sync_check.DocEntry(doc="docs/x.md", description="", paths=["src/x/**"]),
        doc_sync_check.DocEntry(doc="docs/y.md", description="", paths=["src/y/**"]),
    ])
    findings = doc_sync_check.find_violations(
        doc_map,
        changed=["src/x/a.py", "src/y/b.py"],
        trailers={
            "doc-update": ['skip doc=docs/x.md reason="minor"'],
        },
    )
    assert len(findings) == 2
    by_doc = {f.doc: f for f in findings}
    assert by_doc["docs/x.md"].bypass_reason == "minor"
    assert by_doc["docs/y.md"].bypass_reason is None


def test_format_findings_flags_unresolved_as_unresolved() -> None:
    findings = [
        doc_sync_check.Finding(
            doc="docs/x.md",
            touched_paths=["src/x/changed.py"],
            doc_modified=False,
            bypass_reason=None,
        ),
    ]
    report, unresolved = doc_sync_check.format_findings(findings)
    assert unresolved is True
    assert "✗ docs/x.md" in report
    assert "src/x/changed.py" in report


def test_format_findings_clean_when_doc_modified() -> None:
    findings = [
        doc_sync_check.Finding(
            doc="docs/x.md",
            touched_paths=["src/x/changed.py"],
            doc_modified=True,
            bypass_reason=None,
        ),
    ]
    report, unresolved = doc_sync_check.format_findings(findings)
    assert unresolved is False
    assert "✓ docs/x.md" in report


def test_format_findings_clean_when_bypassed() -> None:
    findings = [
        doc_sync_check.Finding(
            doc="docs/x.md",
            touched_paths=["src/x/changed.py"],
            doc_modified=False,
            bypass_reason="mechanical rename",
        ),
    ]
    report, unresolved = doc_sync_check.format_findings(findings)
    assert unresolved is False
    assert "↷ docs/x.md" in report
    assert "mechanical rename" in report


def test_shipped_doc_sync_map_maps_ship_state_to_audit_doc() -> None:
    """Regression: the shipped doc_sync_map.json must wire up the
    ship-state audit doc to its code paths. If someone removes this
    mapping without updating the audit, Phase C's protection is gone."""
    map_path = (
        Path(__file__).resolve().parent.parent
        / "scripts"
        / "doc_sync_map.json"
    )
    doc_map = doc_sync_check.load_doc_map(map_path)
    audit = next(
        (d for d in doc_map.docs if d.doc == "docs/ship-state-machine.md"),
        None,
    )
    assert audit is not None, "ship-state-machine.md must remain in doc_sync_map"
    assert "src/shipyard/core/ship_state.py" in audit.paths
    assert any(p.startswith("src/shipyard/ship/") for p in audit.paths)
