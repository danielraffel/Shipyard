import os
from pathlib import Path
from unittest import mock

from shipyard_sandbox import (
    _is_queue_temp_from_pre_existing_process,
    _process_start_identity,
)


def test_current_process_start_identity_is_precise_and_stable() -> None:
    first = _process_start_identity(os.getpid())

    assert first is not None
    assert _process_start_identity(os.getpid()) == first


def test_pre_existing_queue_writer_is_not_attributed_to_sandbox() -> None:
    path = Path(".queue-85829-1785087066142925000-0.json.tmp")

    with mock.patch(
        "shipyard_sandbox._process_start_identity", return_value="process-start-a"
    ):
        assert _is_queue_temp_from_pre_existing_process(
            path, {85829: "process-start-a"}
        )


def test_new_queue_writer_remains_attributable_to_sandbox() -> None:
    path = Path(".queue-85829-1785087066142925000-0.json.tmp")

    assert not _is_queue_temp_from_pre_existing_process(path, {42: "process-start-a"})


def test_reused_queue_writer_pid_remains_attributable_to_sandbox() -> None:
    path = Path(".queue-85829-1785087066142925000-0.json.tmp")

    with mock.patch(
        "shipyard_sandbox._process_start_identity", return_value="process-start-b"
    ):
        assert not _is_queue_temp_from_pre_existing_process(
            path, {85829: "process-start-a"}
        )


def test_only_exact_queue_temp_names_are_exempt() -> None:
    assert not _is_queue_temp_from_pre_existing_process(
        Path("queue-85829.json"), {85829: "process-start-a"}
    )
