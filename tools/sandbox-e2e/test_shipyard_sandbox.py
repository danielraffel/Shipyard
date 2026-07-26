from pathlib import Path

from shipyard_sandbox import _is_queue_temp_from_pre_existing_process


def test_pre_existing_queue_writer_is_not_attributed_to_sandbox() -> None:
    path = Path(".queue-85829-1785087066142925000-0.json.tmp")

    assert _is_queue_temp_from_pre_existing_process(path, frozenset({85829}))


def test_new_queue_writer_remains_attributable_to_sandbox() -> None:
    path = Path(".queue-85829-1785087066142925000-0.json.tmp")

    assert not _is_queue_temp_from_pre_existing_process(path, frozenset({42}))


def test_only_exact_queue_temp_names_are_exempt() -> None:
    assert not _is_queue_temp_from_pre_existing_process(
        Path("queue-85829.json"), frozenset({85829})
    )
