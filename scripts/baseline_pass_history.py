#!/usr/bin/env python3
"""Bounded, prospective pass observations for baseline continuity.

This module deliberately stores controller observation times.  A source record's
historical timestamp, a job count, or the latest successful import cannot be
used as a substitute for an observation made when a pass was attempted.

The file format is a single JSON document, replaced atomically after each
append.  It is intentionally standalone and uses only the Python standard
library so a collector can import it without coupling to Shipyard's metrics
store.
"""

from __future__ import annotations

import json
import math
import os
import tempfile
from contextlib import contextmanager
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any, Iterable, Mapping

SCHEMA_VERSION = 1
PASS_STATUSES = frozenset({"success", "empty"})
STATUSES = PASS_STATUSES | {"failed"}
DAY_SECONDS = 24 * 60 * 60
FIVE_DAYS_SECONDS = 5 * DAY_SECONDS
DEFAULT_RETENTION_WINDOW_SECS = 14 * DAY_SECONDS
# The effective default is raised for tighter cadences below.  This minimum is
# enough for a fourteen-day window at a twenty-minute cadence (1,009 samples).
DEFAULT_MAX_OBSERVATIONS = 1024
# Controller completion is the observation boundary, so small scheduler and
# import jitter must not turn an otherwise continuous cadence into a gap.
DEFAULT_CADENCE_SLACK_SECS = 300
DEFAULT_CLOCK_SKEW_SECS = 5 * 60


class HistoryValidationError(ValueError):
    """Raised when history or an observation is malformed or inconsistent."""


def _parse_timestamp(value: str | datetime, *, field_name: str = "observed_at") -> datetime:
    if isinstance(value, datetime):
        parsed = value
    elif isinstance(value, str):
        text = value.strip()
        if text.endswith("Z"):
            text = f"{text[:-1]}+00:00"
        try:
            parsed = datetime.fromisoformat(text)
        except ValueError as exc:
            raise HistoryValidationError(f"{field_name} must be an ISO-8601 timestamp") from exc
    else:
        raise HistoryValidationError(f"{field_name} must be an aware datetime or ISO-8601 string")
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        raise HistoryValidationError(f"{field_name} must include a timezone")
    return parsed.astimezone(UTC)


def _timestamp(value: str | datetime, *, field_name: str = "observed_at") -> str:
    return _parse_timestamp(value, field_name=field_name).isoformat().replace("+00:00", "Z")


def _positive_int(value: Any, field_name: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise HistoryValidationError(f"{field_name} must be a positive integer")
    return value


def _nonnegative_int(value: Any, field_name: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise HistoryValidationError(f"{field_name} must be a non-negative integer")
    return value


def _identity(value: Any, field_name: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise HistoryValidationError(f"{field_name} must be a non-empty string")
    return value


@contextmanager
def _exclusive_lock(path: Path):
    """Serialize read/append/write cycles with a sidecar lock file."""

    path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        if os.name == "nt":
            try:
                import msvcrt
            except ImportError as exc:
                raise HistoryValidationError("sidecar locking is unsupported on this platform") from exc

            if os.fstat(fd).st_size == 0:
                os.write(fd, b"\0")
            os.lseek(fd, 0, os.SEEK_SET)
            msvcrt.locking(fd, msvcrt.LK_LOCK, 1)
            try:
                yield
            finally:
                os.lseek(fd, 0, os.SEEK_SET)
                msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)
        else:
            try:
                import fcntl
            except ImportError as exc:
                raise HistoryValidationError("sidecar locking is unsupported on this platform") from exc

            fcntl.flock(fd, fcntl.LOCK_EX)
            try:
                yield
            finally:
                fcntl.flock(fd, fcntl.LOCK_UN)
    finally:
        os.close(fd)


@dataclass(frozen=True)
class PassObservation:
    """One prospective attempt observed by the controller.

    ``observed_at`` is the controller-side attempt time.  It must not be
    populated from a source's retrospective activity date.  ``empty`` is a
    successful, completed pass with zero imported rows and is retained just
    like a non-empty success.
    """

    repo: str
    observed_at: str | datetime
    expected_interval_secs: int
    status: str
    source_identity: str
    config_identity: str
    imported_count: int = 0
    error: str | None = None
    attempted: bool = True
    completed: bool | None = None

    def __post_init__(self) -> None:
        object.__setattr__(self, "repo", _identity(self.repo, "repo"))
        object.__setattr__(self, "observed_at", _timestamp(self.observed_at))
        object.__setattr__(
            self,
            "expected_interval_secs",
            _positive_int(self.expected_interval_secs, "expected_interval_secs"),
        )
        if not isinstance(self.status, str) or self.status not in STATUSES:
            raise HistoryValidationError(
                f"status must be one of {sorted(STATUSES)}, got {self.status!r}"
            )
        object.__setattr__(self, "source_identity", _identity(self.source_identity, "source_identity"))
        object.__setattr__(self, "config_identity", _identity(self.config_identity, "config_identity"))
        object.__setattr__(self, "imported_count", _nonnegative_int(self.imported_count, "imported_count"))
        if not isinstance(self.attempted, bool) or not self.attempted:
            raise HistoryValidationError("attempted must always be true")

        expected_completed = self.status in PASS_STATUSES
        if self.completed is None:
            object.__setattr__(self, "completed", expected_completed)
        elif not isinstance(self.completed, bool) or self.completed != expected_completed:
            raise HistoryValidationError(
                "completed must be true for success/empty and false for failed"
            )

        if self.status == "empty" and self.imported_count != 0:
            raise HistoryValidationError("empty observations must have imported_count=0")
        if self.status == "failed":
            if not isinstance(self.error, str) or not self.error.strip():
                raise HistoryValidationError("failed observations require a non-empty error")
        elif self.error is not None:
            raise HistoryValidationError("success and empty observations cannot carry an error")

    @classmethod
    def success(
        cls,
        *,
        repo: str,
        observed_at: str | datetime,
        expected_interval_secs: int,
        source_identity: str,
        config_identity: str,
        imported_count: int = 0,
    ) -> "PassObservation":
        status = "empty" if imported_count == 0 else "success"
        return cls(
            repo=repo,
            observed_at=observed_at,
            expected_interval_secs=expected_interval_secs,
            status=status,
            source_identity=source_identity,
            config_identity=config_identity,
            imported_count=imported_count,
        )

    @classmethod
    def failed(
        cls,
        *,
        repo: str,
        observed_at: str | datetime,
        expected_interval_secs: int,
        source_identity: str,
        config_identity: str,
        error: str,
    ) -> "PassObservation":
        return cls(
            repo=repo,
            observed_at=observed_at,
            expected_interval_secs=expected_interval_secs,
            status="failed",
            source_identity=source_identity,
            config_identity=config_identity,
            error=error,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "repo": self.repo,
            "observed_at": self.observed_at,
            "expected_interval_secs": self.expected_interval_secs,
            "status": self.status,
            "attempted": self.attempted,
            "completed": self.completed,
            "imported_count": self.imported_count,
            "error": self.error,
            "source_identity": self.source_identity,
            "config_identity": self.config_identity,
        }

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> "PassObservation":
        if not isinstance(value, Mapping):
            raise HistoryValidationError("each observation must be an object")
        expected = {
            "repo",
            "observed_at",
            "expected_interval_secs",
            "status",
            "attempted",
            "completed",
            "imported_count",
            "error",
            "source_identity",
            "config_identity",
        }
        if set(value) != expected:
            missing = sorted(expected - set(value))
            extra = sorted(set(value) - expected)
            raise HistoryValidationError(
                f"observation fields mismatch (missing={missing}, extra={extra})"
            )
        return cls(
            repo=value["repo"],
            observed_at=value["observed_at"],
            expected_interval_secs=value["expected_interval_secs"],
            status=value["status"],
            attempted=value["attempted"],
            completed=value["completed"],
            imported_count=value["imported_count"],
            error=value["error"],
            source_identity=value["source_identity"],
            config_identity=value["config_identity"],
        )


@dataclass
class PassHistory:
    """A bounded observation history for one repository/source/config tuple."""

    repository: str
    source_identity: str
    config_identity: str
    expected_interval_secs: int
    retention_window_secs: int = DEFAULT_RETENTION_WINDOW_SECS
    max_observations: int | None = None
    cadence_slack_secs: int = DEFAULT_CADENCE_SLACK_SECS
    observations: list[PassObservation] = field(default_factory=list)

    def __post_init__(self) -> None:
        self.repository = _identity(self.repository, "repository")
        self.source_identity = _identity(self.source_identity, "source_identity")
        self.config_identity = _identity(self.config_identity, "config_identity")
        self.expected_interval_secs = _positive_int(
            self.expected_interval_secs, "expected_interval_secs"
        )
        self.retention_window_secs = _positive_int(
            self.retention_window_secs, "retention_window_secs"
        )
        if self.max_observations is None:
            self.max_observations = max(
                DEFAULT_MAX_OBSERVATIONS,
                math.ceil(self.retention_window_secs / self.expected_interval_secs) + 1,
                math.ceil(FIVE_DAYS_SECONDS / self.expected_interval_secs) + 1,
            )
        self.max_observations = _positive_int(self.max_observations, "max_observations")
        self.cadence_slack_secs = _nonnegative_int(
            self.cadence_slack_secs, "cadence_slack_secs"
        )
        normalized: list[PassObservation] = []
        for observation in self.observations:
            self._validate_observation_identity(observation)
            normalized.append(observation)
        normalized.sort(key=lambda item: _parse_timestamp(item.observed_at))
        self._reject_duplicate_timestamps(normalized)
        self.observations = normalized
        self._trim()

    @classmethod
    def create(
        cls,
        *,
        repository: str,
        source_identity: str,
        config_identity: str,
        expected_interval_secs: int,
        retention_window_secs: int = DEFAULT_RETENTION_WINDOW_SECS,
        max_observations: int | None = None,
        cadence_slack_secs: int = DEFAULT_CADENCE_SLACK_SECS,
    ) -> "PassHistory":
        return cls(
            repository=repository,
            source_identity=source_identity,
            config_identity=config_identity,
            expected_interval_secs=expected_interval_secs,
            retention_window_secs=retention_window_secs,
            max_observations=max_observations,
            cadence_slack_secs=cadence_slack_secs,
        )

    def _validate_observation_identity(self, observation: PassObservation) -> None:
        if not isinstance(observation, PassObservation):
            raise HistoryValidationError("history observations must be PassObservation values")
        if observation.repo != self.repository:
            raise HistoryValidationError("observation repository does not match history")
        if observation.source_identity != self.source_identity:
            raise HistoryValidationError("observation source_identity does not match history")
        if observation.config_identity != self.config_identity:
            raise HistoryValidationError("observation config_identity does not match history")
        if observation.expected_interval_secs != self.expected_interval_secs:
            raise HistoryValidationError("observation expected cadence does not match history")

    @staticmethod
    def _reject_duplicate_timestamps(observations: Iterable[PassObservation]) -> None:
        seen: set[str] = set()
        for observation in observations:
            if observation.observed_at in seen:
                raise HistoryValidationError(
                    f"duplicate observation timestamp: {observation.observed_at}"
                )
            seen.add(observation.observed_at)

    def _trim(self) -> None:
        if not self.observations:
            return
        newest = max(_parse_timestamp(item.observed_at) for item in self.observations)
        cutoff = newest - timedelta(seconds=self.retention_window_secs)
        retained = [
            item for item in self.observations if _parse_timestamp(item.observed_at) >= cutoff
        ]
        self.observations = retained[-self.max_observations :]

    def append(
        self,
        observation: PassObservation,
        *,
        now: str | datetime | None = None,
        clock_skew_secs: int = DEFAULT_CLOCK_SKEW_SECS,
    ) -> None:
        self._validate_observation_identity(observation)
        clock_skew_secs = _nonnegative_int(clock_skew_secs, "clock_skew_secs")
        now_time = _parse_timestamp(
            datetime.now(UTC) if now is None else now,
            field_name="now",
        )
        observed_time = _parse_timestamp(observation.observed_at)
        if abs((observed_time - now_time).total_seconds()) > clock_skew_secs:
            raise HistoryValidationError(
                "observed_at must be near the controller observation time; "
                "retrospective or future source timestamps are not accepted"
            )
        if any(item.observed_at == observation.observed_at for item in self.observations):
            raise HistoryValidationError(
                f"duplicate observation timestamp: {observation.observed_at}"
            )
        if self.observations and _parse_timestamp(observation.observed_at) <= _parse_timestamp(
            self.observations[-1].observed_at
        ):
            raise HistoryValidationError(
                "observations must be appended in prospective observed_at order"
            )
        self.observations.append(observation)
        self._trim()

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema_version": SCHEMA_VERSION,
            "repository": self.repository,
            "source_identity": self.source_identity,
            "config_identity": self.config_identity,
            "expected_interval_secs": self.expected_interval_secs,
            "retention_window_secs": self.retention_window_secs,
            "max_observations": self.max_observations,
            "cadence_slack_secs": self.cadence_slack_secs,
            "observations": [item.to_dict() for item in self.observations],
        }

    @classmethod
    def from_dict(
        cls,
        value: Mapping[str, Any],
        *,
        repository: str | None = None,
        source_identity: str | None = None,
        config_identity: str | None = None,
    ) -> "PassHistory":
        if not isinstance(value, Mapping):
            raise HistoryValidationError("history must be a JSON object")
        expected = {
            "schema_version",
            "repository",
            "source_identity",
            "config_identity",
            "expected_interval_secs",
            "retention_window_secs",
            "max_observations",
            "cadence_slack_secs",
            "observations",
        }
        if set(value) != expected:
            missing = sorted(expected - set(value))
            extra = sorted(set(value) - expected)
            raise HistoryValidationError(f"history fields mismatch (missing={missing}, extra={extra})")
        if (
            isinstance(value["schema_version"], bool)
            or not isinstance(value["schema_version"], int)
            or value["schema_version"] != SCHEMA_VERSION
        ):
            raise HistoryValidationError(f"unsupported history schema: {value['schema_version']!r}")
        if repository is not None and value["repository"] != repository:
            raise HistoryValidationError("history repository does not match requested repository")
        if source_identity is not None and value["source_identity"] != source_identity:
            raise HistoryValidationError("history source_identity does not match requested source")
        if config_identity is not None and value["config_identity"] != config_identity:
            raise HistoryValidationError("history config_identity does not match requested config")
        raw_observations = value["observations"]
        if not isinstance(raw_observations, list):
            raise HistoryValidationError("history observations must be an array")
        observations = [PassObservation.from_dict(item) for item in raw_observations]
        return cls(
            repository=value["repository"],
            source_identity=value["source_identity"],
            config_identity=value["config_identity"],
            expected_interval_secs=value["expected_interval_secs"],
            retention_window_secs=value["retention_window_secs"],
            max_observations=value["max_observations"],
            cadence_slack_secs=value["cadence_slack_secs"],
            observations=observations,
        )

    @classmethod
    def load(
        cls,
        path: str | os.PathLike[str],
        *,
        repository: str | None = None,
        source_identity: str | None = None,
        config_identity: str | None = None,
    ) -> "PassHistory":
        with Path(path).open("r", encoding="utf-8") as stream:
            try:
                value = json.load(stream)
            except json.JSONDecodeError as exc:
                raise HistoryValidationError(f"malformed history JSON: {exc}") from exc
        return cls.from_dict(
            value,
            repository=repository,
            source_identity=source_identity,
            config_identity=config_identity,
        )

    def _write_atomic_unlocked(self, path: str | os.PathLike[str]) -> None:
        destination = Path(path)
        destination.parent.mkdir(parents=True, exist_ok=True)
        try:
            directory_fd = os.open(destination.parent, os.O_RDONLY)
        except OSError as exc:
            raise HistoryValidationError(
                "atomic history persistence requires a syncable parent directory"
            ) from exc
        payload = json.dumps(self.to_dict(), indent=2, sort_keys=True) + "\n"
        fd: int | None = None
        temporary: Path | None = None
        try:
            fd, temporary_name = tempfile.mkstemp(
                prefix=f".{destination.name}.", suffix=".tmp", dir=destination.parent
            )
            temporary = Path(temporary_name)
            with os.fdopen(fd, "w", encoding="utf-8") as stream:
                stream.write(payload)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, destination)
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
            if temporary is not None:
                try:
                    temporary.unlink()
                except FileNotFoundError:
                    pass

    def write_atomic(self, path: str | os.PathLike[str]) -> None:
        destination = Path(path)
        with _exclusive_lock(destination.with_name(destination.name + ".lock")):
            self._write_atomic_unlocked(destination)

    def summarize(
        self,
        window_since: str | datetime,
        window_until: str | datetime,
    ) -> dict[str, Any]:
        since = _parse_timestamp(window_since, field_name="window_since")
        until = _parse_timestamp(window_until, field_name="window_until")
        if until <= since:
            raise HistoryValidationError("window_until must be after window_since")
        expected = self.expected_interval_secs
        cadence_limit = expected + self.cadence_slack_secs
        ordered = sorted(self.observations, key=lambda item: _parse_timestamp(item.observed_at))
        in_window = [
            item
            for item in ordered
            if since <= _parse_timestamp(item.observed_at) <= until
        ]
        missing: list[dict[str, Any]] = []
        failures: list[dict[str, Any]] = []
        uncertainty: list[str] = []

        def add_missing(start: datetime, end: datetime, reason: str) -> None:
            clipped_start = max(start, since)
            clipped_end = min(end, until)
            if clipped_end <= clipped_start:
                return
            missing.append(
                {
                    "start": _timestamp(clipped_start),
                    "end": _timestamp(clipped_end),
                    "duration_secs": int((clipped_end - clipped_start).total_seconds()),
                    "reason": reason,
                }
            )

        prior = next(
            (item for item in reversed(ordered) if _parse_timestamp(item.observed_at) <= since),
            None,
        )
        following = next(
            (item for item in ordered if _parse_timestamp(item.observed_at) > until),
            None,
        )
        trailing_interval_unobserved = following is None
        if prior is None:
            uncertainty.append("history does not begin before the requested window")
            first = in_window[0] if in_window else None
            add_missing(since, _parse_timestamp(first.observed_at) if first else until, "history_starts_after_window")
        elif (since - _parse_timestamp(prior.observed_at)).total_seconds() > cadence_limit:
            uncertainty.append("the requested window starts after a cadence gap")
            add_missing(
                _parse_timestamp(prior.observed_at) + timedelta(seconds=cadence_limit),
                since,
                "cadence_gap_at_window_start",
            )

        for previous, current in zip(ordered, ordered[1:]):
            previous_time = _parse_timestamp(previous.observed_at)
            current_time = _parse_timestamp(current.observed_at)
            if current_time <= since or previous_time >= until:
                continue
            if (current_time - previous_time).total_seconds() > cadence_limit:
                add_missing(
                    previous_time + timedelta(seconds=cadence_limit),
                    current_time,
                    "cadence_gap",
                )

        if following is None:
            uncertainty.append("history does not extend through the requested window")
            last = in_window[-1] if in_window else None
            if last is None or _parse_timestamp(last.observed_at) < until:
                add_missing(_parse_timestamp(last.observed_at) if last else since, until, "history_ends_before_window")
        elif (until - _parse_timestamp(following.observed_at)).total_seconds() < 0:
            # Defensive branch; ``following`` is selected at or after ``until``.
            add_missing(until, _parse_timestamp(following.observed_at), "history_ends_before_window")

        for item in ordered:
            if item.status == "failed":
                start = _parse_timestamp(item.observed_at)
                end = min(until, start + timedelta(seconds=expected))
                if end <= since or start >= until:
                    continue
                start = max(start, since)
                failures.append(
                    {
                        "start": _timestamp(start),
                        "end": _timestamp(end),
                        "duration_secs": int((end - start).total_seconds()),
                        "error": item.error,
                    }
                )

        longest_run = 0
        run_start: datetime | None = None
        previous_point: datetime | None = None
        for item in in_window:
            point = _parse_timestamp(item.observed_at)
            if item.status == "failed":
                run_start = None
            elif (
                run_start is None
                or previous_point is None
                or (point - previous_point).total_seconds() > cadence_limit
            ):
                run_start = point
            else:
                longest_run = max(longest_run, int((point - run_start).total_seconds()))
            previous_point = point

        full_window_coverage = bool(in_window) and not missing and not failures
        if not in_window:
            uncertainty.append("no prospective observation exists in the requested window")
        if not failures and not missing and in_window:
            full_window_coverage = True
        return {
            "repository": self.repository,
            "source_identity": self.source_identity,
            "config_identity": self.config_identity,
            "window_since": _timestamp(since),
            "window_until": _timestamp(until),
            "expected_interval_secs": expected,
            "category_present": bool(in_window),
            "observed_pass_history": any(item.status in PASS_STATUSES for item in in_window),
            "full_window_coverage": full_window_coverage,
            "trailing_interval_unobserved": trailing_interval_unobserved,
            "exhaustive_provider_window_coverage": "unproven",
            "continuous_elapsed_secs": longest_run,
            "continuous_elapsed_days": longest_run / DAY_SECONDS,
            "five_continuous_24h_days": longest_run >= FIVE_DAYS_SECONDS,
            "missing_intervals": missing,
            "failing_intervals": failures,
            "uncertainty": uncertainty,
            "observation_count": len(in_window),
        }


def load_history(
    path: str | os.PathLike[str],
    *,
    repository: str | None = None,
    source_identity: str | None = None,
    config_identity: str | None = None,
) -> PassHistory:
    """Load and validate one history, optionally enforcing its identities."""

    return PassHistory.load(
        path,
        repository=repository,
        source_identity=source_identity,
        config_identity=config_identity,
    )


def append_observation(
    path: str | os.PathLike[str],
    observation: PassObservation,
    *,
    repository: str | None = None,
    source_identity: str | None = None,
    config_identity: str | None = None,
    expected_interval_secs: int | None = None,
    retention_window_secs: int = DEFAULT_RETENTION_WINDOW_SECS,
    max_observations: int | None = None,
    cadence_slack_secs: int = DEFAULT_CADENCE_SLACK_SECS,
    now: str | datetime | None = None,
    clock_skew_secs: int = DEFAULT_CLOCK_SKEW_SECS,
) -> PassHistory:
    """Append and atomically persist an observation, creating the history if absent."""

    destination = Path(path)
    destination.parent.mkdir(parents=True, exist_ok=True)
    with _exclusive_lock(destination.with_name(destination.name + ".lock")):
        if destination.exists():
            history = PassHistory.load(
                destination,
                repository=repository,
                source_identity=source_identity,
                config_identity=config_identity,
            )
        else:
            history = PassHistory.create(
                repository=repository or observation.repo,
                source_identity=source_identity or observation.source_identity,
                config_identity=config_identity or observation.config_identity,
                expected_interval_secs=(
                    observation.expected_interval_secs
                    if expected_interval_secs is None
                    else expected_interval_secs
                ),
                retention_window_secs=retention_window_secs,
                max_observations=max_observations,
                cadence_slack_secs=cadence_slack_secs,
            )
        if (
            expected_interval_secs is not None
            and expected_interval_secs != history.expected_interval_secs
        ):
            raise HistoryValidationError("requested expected cadence does not match history")
        if cadence_slack_secs != history.cadence_slack_secs:
            raise HistoryValidationError("requested cadence slack does not match history")
        history.append(observation, now=now, clock_skew_secs=clock_skew_secs)
        history._write_atomic_unlocked(destination)
        return history
