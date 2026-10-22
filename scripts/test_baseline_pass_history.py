#!/usr/bin/env python3
from __future__ import annotations

import json
import sys
import tempfile
import unittest
from datetime import UTC, datetime, timedelta
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import baseline_pass_history as history


CADENCE = 24 * 60 * 60
REPO = "owner/repository"
SOURCE = "provider:fixture:v1"
CONFIG = "config:sha256:abc"
START = datetime(2026, 10, 9, tzinfo=UTC)


def observation(day: int, *, status: str = "empty", imported_count: int = 0, error: str | None = None) -> history.PassObservation:
    return history.PassObservation(
        repo=REPO,
        observed_at=START + timedelta(days=day),
        expected_interval_secs=CADENCE,
        status=status,
        source_identity=SOURCE,
        config_identity=CONFIG,
        imported_count=imported_count,
        error=error,
    )


class PassHistoryTests(unittest.TestCase):
    def make_history(self, **kwargs: object) -> history.PassHistory:
        return history.PassHistory.create(
            repository=REPO,
            source_identity=SOURCE,
            config_identity=CONFIG,
            expected_interval_secs=CADENCE,
            **kwargs,
        )

    def test_empty_success_is_retained(self) -> None:
        store = self.make_history()
        store.append(observation(0), now=START)
        self.assertEqual(len(store.observations), 1)
        self.assertEqual(store.observations[0].status, "empty")
        self.assertTrue(store.observations[0].completed)

    def test_failure_is_retained_with_error(self) -> None:
        store = self.make_history()
        store.append(
            observation(0, status="failed", error="provider unavailable"), now=START
        )
        self.assertEqual(store.observations[0].error, "provider unavailable")
        self.assertFalse(store.observations[0].completed)

    def test_missing_cadence_interval_is_detected(self) -> None:
        store = self.make_history()
        store.append(observation(0), now=START)
        store.append(observation(2), now=START + timedelta(days=2))
        report = store.summarize(START, START + timedelta(days=2))
        self.assertTrue(report["missing_intervals"])
        self.assertFalse(report["full_window_coverage"])

    def test_default_slack_allows_small_controller_completion_jitter(self) -> None:
        store = history.PassHistory.create(
            repository=REPO, source_identity=SOURCE, config_identity=CONFIG,
            expected_interval_secs=3600,
        )
        store.append(
            history.PassObservation(repo=REPO, observed_at=START, expected_interval_secs=3600,
                                    status="empty", source_identity=SOURCE, config_identity=CONFIG),
            now=START,
        )
        jittered = START + timedelta(hours=1, seconds=5)
        store.append(
            history.PassObservation(repo=REPO, observed_at=jittered, expected_interval_secs=3600,
                                    status="empty", source_identity=SOURCE, config_identity=CONFIG),
            now=jittered,
        )
        report = store.summarize(START, jittered)
        self.assertFalse(report["missing_intervals"])

    def test_five_elapsed_days_span_weekend(self) -> None:
        store = self.make_history()
        for day in range(6):
            store.append(observation(day), now=START + timedelta(days=day))
        report = store.summarize(START, START + timedelta(days=5))
        self.assertTrue(report["five_continuous_24h_days"])
        self.assertEqual(report["continuous_elapsed_secs"], 5 * CADENCE)

    def test_default_bound_retains_five_days_at_twenty_minute_cadence(self) -> None:
        cadence = 20 * 60
        store = history.PassHistory.create(
            repository=REPO,
            source_identity=SOURCE,
            config_identity=CONFIG,
            expected_interval_secs=cadence,
        )
        for sample in range(5 * 24 * 3 + 1):
            store.append(
                history.PassObservation(
                    repo=REPO,
                    observed_at=START + timedelta(seconds=sample * cadence),
                    expected_interval_secs=cadence,
                    status="empty",
                    source_identity=SOURCE,
                    config_identity=CONFIG,
                ),
                now=START + timedelta(seconds=sample * cadence),
            )
        self.assertEqual(len(store.observations), 361)

    def test_short_cadence_retains_anchor_with_jitter(self) -> None:
        cadence = 15 * 60
        store = history.PassHistory.create(
            repository=REPO, source_identity=SOURCE, config_identity=CONFIG,
            expected_interval_secs=cadence, cadence_slack_secs=300,
        )
        first = START
        for sample in range((14 * 24 * 60 * 60) // cadence + 2):
            point = first + timedelta(seconds=sample * cadence + (60 if sample % 2 else -60))
            store.append(history.PassObservation(
                repo=REPO, observed_at=point, expected_interval_secs=cadence,
                status="empty", source_identity=SOURCE, config_identity=CONFIG,
            ), now=point)
        until = store.observations[-1].observed_at
        report = store.summarize(
            datetime.fromisoformat(until.replace("Z", "+00:00")) - timedelta(days=14),
            datetime.fromisoformat(until.replace("Z", "+00:00")),
        )
        self.assertFalse(report["missing_intervals"])

    def test_partial_history_does_not_claim_full_window_coverage(self) -> None:
        store = self.make_history()
        store.append(observation(2), now=START + timedelta(days=2))
        report = store.summarize(START, START + timedelta(days=5))
        self.assertFalse(report["full_window_coverage"])
        self.assertIn("history does not begin before the requested window", report["uncertainty"])
        self.assertTrue(report["trailing_interval_unobserved"])
        self.assertEqual(report["exhaustive_provider_window_coverage"], "unproven")

    def test_identity_mismatch_is_rejected(self) -> None:
        store = self.make_history()
        with self.assertRaises(history.HistoryValidationError):
            store.append(
                history.PassObservation(
                    repo=REPO,
                    observed_at=START,
                    expected_interval_secs=CADENCE,
                    status="empty",
                    source_identity="provider:other",
                    config_identity=CONFIG,
                )
            )

    def test_retention_is_bounded_by_count_and_window(self) -> None:
        store = self.make_history(retention_window_secs=2 * CADENCE, max_observations=2)
        for day in range(6):
            store.append(observation(day), now=START + timedelta(days=day))
        self.assertEqual(len(store.observations), 2)
        self.assertEqual(store.observations[0].observed_at, observation(4).observed_at)

    def test_atomic_write_and_read_round_trip(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "nested" / "history.json"
            store = self.make_history()
            store.append(observation(0), now=START)
            store.write_atomic(path)
            loaded = history.PassHistory.load(
                path, repository=REPO, source_identity=SOURCE, config_identity=CONFIG
            )
            self.assertEqual(loaded.to_dict(), store.to_dict())

    def test_append_entry_point_stamps_and_round_trips(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "history.json"
            history.append_observation(path, observation(0), now=START)
            loaded = history.PassHistory.load(path)
            self.assertEqual(len(loaded.observations), 1)
            self.assertTrue(path.with_name("history.json.lock").exists())

    def test_clock_poisoning_is_rejected_before_persist(self) -> None:
        store = self.make_history()
        with self.assertRaises(history.HistoryValidationError):
            store.append(observation(0), now=START + timedelta(hours=1))
        self.assertEqual(store.observations, [])

    def test_subsecond_order_survives_load(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "history.json"
            store = self.make_history()
            store.append(observation(0), now=START)
            later = history.PassObservation(
                repo=REPO,
                observed_at=START + timedelta(microseconds=500_000),
                expected_interval_secs=CADENCE,
                status="empty",
                source_identity=SOURCE,
                config_identity=CONFIG,
            )
            store.append(later, now=START + timedelta(microseconds=500_000))
            store.write_atomic(path)
            loaded = history.PassHistory.load(path)
            self.assertEqual(
                [item.observed_at for item in loaded.observations],
                [item.observed_at for item in store.observations],
            )

    def test_malformed_and_duplicate_histories_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "history.json"
            path.write_text("{not json", encoding="utf-8")
            with self.assertRaises(history.HistoryValidationError):
                history.PassHistory.load(path)
            duplicate = self.make_history()
            duplicate.append(observation(0), now=START)
            payload = duplicate.to_dict()
            payload["observations"].append(payload["observations"][0])
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaises(history.HistoryValidationError):
                history.PassHistory.load(path)

    def test_failed_pass_breaks_continuity(self) -> None:
        store = self.make_history()
        store.append(observation(0), now=START)
        store.append(observation(1, status="failed", error="timeout"), now=START + timedelta(days=1))
        store.append(observation(2), now=START + timedelta(days=2))
        report = store.summarize(START, START + timedelta(days=2))
        self.assertTrue(report["failing_intervals"])
        self.assertFalse(report["full_window_coverage"])
        self.assertEqual(report["continuous_elapsed_secs"], 0)

    def test_failure_overlapping_window_start_is_reported(self) -> None:
        store = self.make_history()
        store.append(
            history.PassObservation(
                repo=REPO,
                observed_at=START - timedelta(hours=12),
                expected_interval_secs=CADENCE,
                status="failed",
                source_identity=SOURCE,
                config_identity=CONFIG,
                error="timeout",
            ),
            now=START - timedelta(hours=12),
        )
        store.append(observation(0), now=START)
        store.append(observation(1), now=START + timedelta(days=1))
        report = store.summarize(START, START + timedelta(days=1))
        self.assertEqual(len(report["failing_intervals"]), 1)
        self.assertFalse(report["full_window_coverage"])

    def test_failure_ending_at_window_start_does_not_overlap(self) -> None:
        store = self.make_history()
        store.append(observation(-1, status="failed", error="timeout"), now=START - timedelta(days=1))
        store.append(observation(0), now=START)
        store.append(observation(1), now=START + timedelta(days=1))
        report = store.summarize(START, START + timedelta(days=1))
        self.assertEqual(report["failing_intervals"], [])


if __name__ == "__main__":
    unittest.main()
