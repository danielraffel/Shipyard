"""Workflow renderer tests — stable structure + parse-back assertions."""

from __future__ import annotations

from shipyard.changelog.workflow import (
    DEFAULT_SHIPYARD_VERSION,
    WorkflowOptions,
    render_workflow,
)


def test_render_default_is_parseable_and_has_key_fields() -> None:
    body = render_workflow()
    assert body.startswith("name: Post-tag docs sync\n")
    assert 'tags: ["v*"]' in body
    assert f'SHIPYARD_VERSION: "{DEFAULT_SHIPYARD_VERSION}"' in body
    assert "shipyard release-bot hook run" in body
    assert "fetch-depth: 0" in body
    assert "fetch-tags: true" in body
    # Token fallback.
    assert "secrets.RELEASE_BOT_TOKEN || secrets.GITHUB_TOKEN" in body


def test_render_custom_tag_pattern_and_version() -> None:
    body = render_workflow(
        WorkflowOptions(tag_pattern="cli-v*", shipyard_version="1.2.3")
    )
    assert 'tags: ["cli-v*"]' in body
    assert 'SHIPYARD_VERSION: "1.2.3"' in body


def test_render_parses_as_valid_yaml() -> None:
    try:
        import yaml  # type: ignore[import-not-found]
    except ImportError:
        import pytest

        pytest.skip("pyyaml not installed")
        return
    parsed = yaml.safe_load(render_workflow())
    assert parsed["name"] == "Post-tag docs sync"
    assert parsed["on"]["push"]["tags"] == ["v*"]
    assert parsed["jobs"]["sync"]["runs-on"] == "ubuntu-latest"
    steps = parsed["jobs"]["sync"]["steps"]
    assert any("checkout" in (s.get("uses") or "") for s in steps)
    assert any(
        "shipyard release-bot hook run" in (s.get("run") or "") for s in steps
    )


def test_render_stable_line_count() -> None:
    """Quick canary so an accidental change to the template shows up in diff."""
    body = render_workflow()
    # If this trips legitimately, bump the number and note the change.
    assert body.count("\n") > 20
    assert body.count("\n") < 60


def test_install_step_pipes_to_bash_not_sh() -> None:
    """Regression guard: install.sh uses `set -o pipefail`, which dash
    (Ubuntu's /bin/sh) rejects. The install pipe MUST end in `bash`.
    """
    body = render_workflow()
    assert "| SHIPYARD_VERSION=" in body
    assert " bash\n" in body
    # No bare ` sh\n` in the install line — would silently regress.
    install_lines = [
        line for line in body.splitlines() if "SHIPYARD_VERSION=" in line and "curl" in line
    ]
    assert install_lines, "install line not found in rendered workflow"
    for line in install_lines:
        assert line.rstrip().endswith("bash"), (
            f"install pipe must end in bash, got: {line!r}"
        )


def test_default_shipyard_version_tracks_package_version() -> None:
    """Avoids the chicken-and-egg where a stale hardcoded version
    pins consumers to a previous (possibly broken) shipyard release.
    """
    from shipyard import __version__

    from shipyard.changelog.workflow import DEFAULT_SHIPYARD_VERSION

    assert DEFAULT_SHIPYARD_VERSION == __version__
