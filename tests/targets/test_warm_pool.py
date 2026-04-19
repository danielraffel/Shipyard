"""Unit tests for the warm-pool module.

Covers: entry lifecycle (insert / lookup / evict / drain / prune),
TTL math, env / config opt-outs, backend eligibility, and atomic
file writes.
"""

from __future__ import annotations

import json
import time
from typing import TYPE_CHECKING

import pytest

from shipyard.targets.warm_pool import (
    DEFAULT_POOL_FILENAME,
    PoolEntry,
    WarmPool,
    compute_expires_at,
    default_pool_path,
    extract_warm_keepalive_seconds,
    is_backend_eligible,
    warm_host_key,
    warm_reuse_disabled_by_env,
)

if TYPE_CHECKING:
    from pathlib import Path


@pytest.fixture
def pool_path(tmp_path: Path) -> Path:
    return tmp_path / "warm_pool.json"


def _entry(
    *,
    target: str = "ubuntu",
    host: str = "ubuntu",
    backend: str = "ssh",
    workdir: str = "~/repo",
    sha: str = "a" * 40,
    ttl: float = 600.0,
    now: float | None = None,
) -> PoolEntry:
    base = now if now is not None else time.time()
    return PoolEntry(
        target=target,
        host=host,
        backend=backend,
        workdir=workdir,
        sha=sha,
        expires_at=base + ttl,
        created_at=base,
    )


class TestPoolEntry:
    def test_is_expired_before_ttl(self) -> None:
        entry = _entry(ttl=60.0)
        assert not entry.is_expired()

    def test_is_expired_past_ttl(self) -> None:
        entry = _entry(ttl=-1.0)
        assert entry.is_expired()

    def test_ttl_remaining_positive(self) -> None:
        now = 1000.0
        entry = _entry(now=now, ttl=120.0)
        assert entry.ttl_remaining_secs(now=now) == 120.0

    def test_ttl_remaining_clamped_to_zero(self) -> None:
        now = 2000.0
        entry = _entry(now=now, ttl=-10.0)
        assert entry.ttl_remaining_secs(now=now) == 0.0

    def test_roundtrip_dict(self) -> None:
        entry = _entry()
        round_tripped = PoolEntry.from_dict(entry.to_dict())
        assert round_tripped == entry


class TestWarmPool:
    def test_missing_file_reads_as_empty(self, pool_path: Path) -> None:
        pool = WarmPool(pool_path)
        assert pool.all_entries() == []
        assert pool.get("anything", "host") is None

    def test_upsert_and_get(self, pool_path: Path) -> None:
        pool = WarmPool(pool_path)
        entry = _entry()
        pool.upsert(entry)
        got = pool.get("ubuntu", "ubuntu")
        assert got is not None
        assert got == entry

    def test_upsert_replaces_same_key(self, pool_path: Path) -> None:
        pool = WarmPool(pool_path)
        pool.upsert(_entry(workdir="~/old"))
        pool.upsert(_entry(workdir="~/new"))
        entries = pool.all_entries()
        assert len(entries) == 1
        assert entries[0].workdir == "~/new"

    def test_different_hosts_are_distinct(self, pool_path: Path) -> None:
        pool = WarmPool(pool_path)
        pool.upsert(_entry(host="host-a", workdir="~/a"))
        pool.upsert(_entry(host="host-b", workdir="~/b"))
        assert len(pool.all_entries()) == 2
        assert pool.get("ubuntu", "host-a").workdir == "~/a"  # type: ignore[union-attr]
        assert pool.get("ubuntu", "host-b").workdir == "~/b"  # type: ignore[union-attr]

    def test_get_returns_none_when_expired(self, pool_path: Path) -> None:
        pool = WarmPool(pool_path)
        pool.upsert(_entry(ttl=-1.0))
        assert pool.get("ubuntu", "ubuntu") is None

    def test_evict(self, pool_path: Path) -> None:
        pool = WarmPool(pool_path)
        pool.upsert(_entry())
        assert pool.evict("ubuntu", "ubuntu") is True
        assert pool.get("ubuntu", "ubuntu") is None
        # second evict is a no-op
        assert pool.evict("ubuntu", "ubuntu") is False

    def test_drain(self, pool_path: Path) -> None:
        pool = WarmPool(pool_path)
        pool.upsert(_entry(target="t1"))
        pool.upsert(_entry(target="t2"))
        assert pool.drain() == 2
        assert pool.all_entries() == []

    def test_prune_expired(self, pool_path: Path) -> None:
        pool = WarmPool(pool_path)
        pool.upsert(_entry(target="fresh", ttl=600.0))
        pool.upsert(_entry(target="stale", ttl=-1.0))
        pruned = pool.prune_expired()
        assert pruned == 1
        targets = {e.target for e in pool.all_entries()}
        assert targets == {"fresh"}

    def test_corrupt_file_ignored(self, pool_path: Path) -> None:
        pool_path.write_text("{ not valid json")
        pool = WarmPool(pool_path)
        # corrupt file doesn't crash lookups
        assert pool.all_entries() == []
        # next write overwrites it cleanly
        pool.upsert(_entry())
        data = json.loads(pool_path.read_text())
        assert "entries" in data and len(data["entries"]) == 1

    def test_atomic_write_leaves_no_tmp(self, pool_path: Path) -> None:
        pool = WarmPool(pool_path)
        pool.upsert(_entry())
        siblings = list(pool_path.parent.iterdir())
        # only the real file should remain; .tmp must be renamed away
        assert [p.name for p in siblings if p.suffix == ".tmp"] == []

    def test_default_pool_path(self, tmp_path: Path) -> None:
        assert default_pool_path(tmp_path).name == DEFAULT_POOL_FILENAME
        assert default_pool_path(tmp_path).parent == tmp_path


class TestEnvKill:
    def test_env_default_off(self) -> None:
        assert warm_reuse_disabled_by_env({}) is False

    @pytest.mark.parametrize("value", ["1", "true", "yes", "ON", "True"])
    def test_env_truthy_kills(self, value: str) -> None:
        assert warm_reuse_disabled_by_env({"SHIPYARD_NO_WARM_POOL": value}) is True

    @pytest.mark.parametrize("value", ["0", "false", "no", ""])
    def test_env_falsy_allows(self, value: str) -> None:
        assert warm_reuse_disabled_by_env({"SHIPYARD_NO_WARM_POOL": value}) is False


class TestConfigExtractors:
    def test_missing_key_means_off(self) -> None:
        assert extract_warm_keepalive_seconds({}) == 0

    def test_zero_means_off(self) -> None:
        assert extract_warm_keepalive_seconds({"warm_keepalive_seconds": 0}) == 0

    def test_positive_int(self) -> None:
        assert extract_warm_keepalive_seconds(
            {"warm_keepalive_seconds": 600}
        ) == 600

    def test_negative_coerces_to_zero(self) -> None:
        assert extract_warm_keepalive_seconds(
            {"warm_keepalive_seconds": -5}
        ) == 0

    def test_garbage_value_coerces_to_zero(self) -> None:
        assert extract_warm_keepalive_seconds(
            {"warm_keepalive_seconds": "nope"}
        ) == 0

    def test_numeric_string_parses(self) -> None:
        assert extract_warm_keepalive_seconds(
            {"warm_keepalive_seconds": "300"}
        ) == 300


class TestBackendEligibility:
    @pytest.mark.parametrize("backend", ["ssh", "ssh-windows", "local"])
    def test_eligible(self, backend: str) -> None:
        assert is_backend_eligible(backend, {}) is True

    @pytest.mark.parametrize("backend", ["cloud", "github-hosted", "namespace"])
    def test_ineligible(self, backend: str) -> None:
        assert is_backend_eligible(backend, {}) is False

    def test_case_insensitive(self) -> None:
        assert is_backend_eligible("SSH", {}) is True


class TestHostKey:
    def test_uses_host_field(self) -> None:
        assert warm_host_key({"host": "ubuntu"}) == "ubuntu"

    def test_strips_whitespace(self) -> None:
        assert warm_host_key({"host": "  ubuntu  "}) == "ubuntu"

    def test_missing_collapses_to_local(self) -> None:
        assert warm_host_key({}) == "local"

    def test_empty_string_collapses_to_local(self) -> None:
        assert warm_host_key({"host": ""}) == "local"


class TestComputeExpiresAt:
    def test_basic(self) -> None:
        now = 1000.0
        assert compute_expires_at(600, now=now) == 1600.0

    def test_negative_clamped(self) -> None:
        now = 1000.0
        assert compute_expires_at(-10, now=now) == 1000.0
