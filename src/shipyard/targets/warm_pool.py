"""Warm-pool runner reuse across PRs.

A "warm pool" entry lets a subsequent ship within a configurable TTL
skip the expensive pre-stage (clone / sync / deps) by re-entering the
same runner workdir that was left in a known-good state by a recent
PASS. Validate (configure / build / test) is always re-run, so the
feature trades a small, bounded correctness risk (stale workdir on the
same SHA) for a potentially large time saving on SSH and cloud runners.

Design points — mirrors issue #82:

- Opt-in per target via ``warm_keepalive_seconds = <N>`` on the target
  section (default 0 = feature off). See
  :func:`extract_warm_keepalive_seconds`.
- Global kill switch: ``SHIPYARD_NO_WARM_POOL=1`` in the env. See
  :func:`warm_reuse_disabled_by_env`.
- Per-ship CLI flag: ``--no-warm`` passed in by the dispatcher.
- GitHub-hosted cloud targets are silently ineligible (workflow runs
  are ephemeral) and surface a single warn-once message so users can
  reconcile the config. See :func:`is_backend_eligible`.
- Same-SHA reuse only — an entry is consulted only if the current job
  SHA matches the stored SHA. The feature is not a cross-SHA cache; it
  only exists to amortise pre-stage cost for back-to-back ships of the
  exact same commit.
- Any failure during a warm reuse evicts the entry so the pool never
  serves a dirty workdir twice.

The pool state is a single JSON file under the machine-global state
directory. Atomic writes (``<path>.tmp`` → rename) keep concurrent
Shipyard processes from corrupting it.
"""

from __future__ import annotations

import json
import os
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

DEFAULT_POOL_FILENAME = "warm_pool.json"

# Backends whose runner identity survives a shipyard invocation.
# Everything else (cloud/github-hosted) is ephemeral per workflow run.
_ELIGIBLE_BACKENDS = {"ssh", "ssh-windows", "local"}


@dataclass(frozen=True)
class PoolEntry:
    """A single warm-pool record.

    Attributes:
        target: Target name the entry belongs to.
        host: Host identifier (SSH alias / ``user@host`` / ``local``).
            Used as the tuple key along with ``target`` so a target
            that was last warm on host A won't be mis-served when a
            subsequent config switches it to host B.
        backend: Backend the entry was warmed on (``ssh`` /
            ``ssh-windows`` / ``local``).
        workdir: Remote repository path the pre-stage was performed in.
            The executor re-enters this directory instead of re-cloning.
        sha: The exact SHA the pre-stage was run against. Reuse is
            only honored when the next job is on this same SHA.
        expires_at: Monotonic-clock-safe UNIX epoch seconds. Entries
            past this point are ignored and pruned.
        created_at: UNIX epoch seconds when the entry was recorded,
            for observability on ``shipyard targets warm status``.
    """

    target: str
    host: str
    backend: str
    workdir: str
    sha: str
    expires_at: float
    created_at: float

    def is_expired(self, *, now: float | None = None) -> bool:
        """Return True if ``now`` (default: wall clock) is past expiry."""
        return (now if now is not None else time.time()) >= self.expires_at

    def ttl_remaining_secs(self, *, now: float | None = None) -> float:
        """Seconds until expiry; clamped to 0 when past-expiry."""
        gap = self.expires_at - (now if now is not None else time.time())
        return max(0.0, gap)

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> PoolEntry:
        return cls(
            target=str(data["target"]),
            host=str(data["host"]),
            backend=str(data["backend"]),
            workdir=str(data["workdir"]),
            sha=str(data["sha"]),
            expires_at=float(data["expires_at"]),
            created_at=float(data["created_at"]),
        )


class WarmPool:
    """JSON-backed persistent warm-pool store.

    Not a process-wide singleton — callers construct one against a
    specific state directory. The store is keyed by ``(target, host)``
    so different physical hosts running the same logical target don't
    collide; the first writer wins per key.
    """

    def __init__(self, path: Path) -> None:
        self.path = path

    # ---- load / save -----------------------------------------------

    def _load_raw(self) -> list[dict[str, Any]]:
        """Read the raw list of entries from disk.

        Returns an empty list when the file is missing, unreadable, or
        malformed. The pool is best-effort: a corrupt file must not
        block a ship, so we swallow errors here and let the next save
        overwrite them.
        """
        if not self.path.exists():
            return []
        try:
            payload = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return []
        if not isinstance(payload, dict):
            return []
        entries = payload.get("entries", [])
        if not isinstance(entries, list):
            return []
        return [item for item in entries if isinstance(item, dict)]

    def all_entries(self) -> list[PoolEntry]:
        """Return every entry currently in the pool (expired included)."""
        out: list[PoolEntry] = []
        for raw in self._load_raw():
            try:
                out.append(PoolEntry.from_dict(raw))
            except (KeyError, TypeError, ValueError):
                continue
        return out

    def save_entries(self, entries: list[PoolEntry]) -> None:
        """Atomically rewrite the pool file with the given entries."""
        self.path.parent.mkdir(parents=True, exist_ok=True)
        payload = {"entries": [entry.to_dict() for entry in entries]}
        tmp = self.path.with_suffix(self.path.suffix + ".tmp")
        tmp.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        os.replace(tmp, self.path)

    # ---- lookups ---------------------------------------------------

    def get(self, target: str, host: str) -> PoolEntry | None:
        """Return the unexpired entry for ``(target, host)`` or None.

        Expired entries are ignored here but not pruned; call
        :meth:`prune_expired` if you want the file to shrink.
        """
        now = time.time()
        for entry in self.all_entries():
            if entry.target == target and entry.host == host:
                if entry.is_expired(now=now):
                    return None
                return entry
        return None

    # ---- mutations -------------------------------------------------

    def upsert(self, entry: PoolEntry) -> None:
        """Insert or replace the record for ``(target, host)``."""
        existing = [
            e for e in self.all_entries()
            if not (e.target == entry.target and e.host == entry.host)
        ]
        existing.append(entry)
        self.save_entries(existing)

    def evict(self, target: str, host: str) -> bool:
        """Remove the entry for ``(target, host)``. Return True if removed."""
        kept: list[PoolEntry] = []
        removed = False
        for entry in self.all_entries():
            if entry.target == target and entry.host == host:
                removed = True
                continue
            kept.append(entry)
        if removed:
            self.save_entries(kept)
        return removed

    def drain(self) -> int:
        """Remove every entry. Return count drained."""
        current = self.all_entries()
        self.save_entries([])
        return len(current)

    def prune_expired(self) -> int:
        """Remove expired entries; return how many were pruned."""
        now = time.time()
        kept: list[PoolEntry] = []
        pruned = 0
        for entry in self.all_entries():
            if entry.is_expired(now=now):
                pruned += 1
                continue
            kept.append(entry)
        if pruned:
            self.save_entries(kept)
        return pruned


# ── helpers used by executor / CLI ───────────────────────────────


def default_pool_path(state_dir: Path) -> Path:
    """Canonical location for the warm-pool JSON file."""
    return Path(state_dir) / DEFAULT_POOL_FILENAME


def warm_reuse_disabled_by_env(env: dict[str, str] | None = None) -> bool:
    """Return True when ``SHIPYARD_NO_WARM_POOL=1`` is set.

    Accepts any of ``1|true|yes|on`` (case-insensitive) to match how
    other Shipyard env flags are parsed.
    """
    source = env if env is not None else os.environ
    raw = source.get("SHIPYARD_NO_WARM_POOL", "")
    return raw.strip().lower() in {"1", "true", "yes", "on"}


def extract_warm_keepalive_seconds(target_config: dict[str, Any]) -> int:
    """Return the configured keepalive in seconds; 0 means "off".

    Tolerant of strings / negatives — anything non-positive maps to 0
    so a misconfigured target silently keeps the safe default.
    """
    raw = target_config.get("warm_keepalive_seconds", 0)
    try:
        secs = int(raw)
    except (TypeError, ValueError):
        return 0
    return secs if secs > 0 else 0


def is_backend_eligible(backend: str, target_config: dict[str, Any]) -> bool:
    """Whether ``backend`` is a type where reuse makes sense.

    Cloud + GitHub-hosted runners are excluded because each workflow
    run gets a fresh VM; there's nothing to keep warm. SSH / local
    runners keep the workdir across invocations, which is where the
    whole point of the pool lives.

    ``target_config`` is accepted for future per-target overrides
    (e.g. a Namespace self-managed pool that does persist) but is
    otherwise unused.
    """
    del target_config  # reserved for future overrides
    return backend.lower() in _ELIGIBLE_BACKENDS


def warm_host_key(target_config: dict[str, Any]) -> str:
    """Pick a stable host key for pool indexing.

    SSH backends use their ``host`` field; local backends collapse to
    a literal ``"local"`` so the same machine doesn't spawn dozens of
    identical entries if hostname lookups vary.
    """
    host = target_config.get("host")
    if isinstance(host, str) and host.strip():
        return host.strip()
    return "local"


def compute_expires_at(keepalive_seconds: int, *, now: float | None = None) -> float:
    """Return the absolute expiry time for a new pool entry."""
    base = now if now is not None else time.time()
    return base + max(0, int(keepalive_seconds))


__all__ = [
    "DEFAULT_POOL_FILENAME",
    "PoolEntry",
    "WarmPool",
    "compute_expires_at",
    "default_pool_path",
    "extract_warm_keepalive_seconds",
    "is_backend_eligible",
    "warm_host_key",
    "warm_reuse_disabled_by_env",
]
