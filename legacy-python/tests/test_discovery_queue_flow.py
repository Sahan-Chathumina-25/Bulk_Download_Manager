"""Tests for the Discover-page queue flow: planner decisions + batch execution.

Pure-Python suite (no PySide6 import): the selection/planning logic lives
in :mod:`src.ui.discovery_queue_planner` and is exercised headless — the
planner itself is Qt-free by design, and ``execute_queue_plan`` runs
against a real :class:`src.core.download_manager.DownloadManager` for the
integration test.
"""

from __future__ import annotations

import pathlib
import time
from collections.abc import Callable

import pytest

from src.core import security
from src.core.download_manager import DownloadManager
from src.core.link_discovery import DiscoveryCandidate
from src.database.connection import Database
from src.database.models import TERMINAL_STATES, DownloadRecord, DownloadState, Settings
from src.database.repository import DownloadRepository
from src.ui.discovery_queue_planner import (
    QueuePlan,
    checked_candidates,
    execute_queue_plan,
    plan_discovery_queue,
)

PAGE_URL = "http://page.example/downloads.html"


def _cand(url: str, filename: str) -> DiscoveryCandidate:
    """A ticked (``is_likely_file=True``) discovery candidate for ``url``."""
    return DiscoveryCandidate(
        candidate_url=url,
        filename=filename,
        extension=None,
        source_page_url=PAGE_URL,
        is_likely_file=True,
    )


def _unchecked(url: str, filename: str) -> DiscoveryCandidate:
    """An unticked discovery candidate for ``url``."""
    return DiscoveryCandidate(
        candidate_url=url,
        filename=filename,
        extension=None,
        source_page_url=PAGE_URL,
        is_likely_file=False,
    )


def _wait_terminal(
    repo: DownloadRepository, ids: list[int], timeout: float = 30.0
) -> None:
    """Block until every id reaches a terminal state (or fail the test)."""
    remaining = dict.fromkeys(ids)
    deadline = time.monotonic() + timeout
    while remaining and time.monotonic() < deadline:
        for download_id in list(remaining):
            record = repo.get(download_id)
            assert record is not None, f"record {download_id} vanished"
            if record.state in TERMINAL_STATES:
                del remaining[download_id]
        if remaining:
            time.sleep(0.05)
    assert not remaining, f"downloads never settled: {sorted(remaining)}"


# -- selection ------------------------------------------------------------------------


def test_checked_candidates_returns_only_ticked_rows() -> None:
    # Arrange
    candidates = [
        _cand("http://files.example/a.zip", "a.zip"),
        _unchecked("http://files.example/b.zip", "b.zip"),
        _cand("http://files.example/c.zip", "c.zip"),
    ]

    # Act
    checked = checked_candidates(candidates)

    # Assert: order preserved, unticked rows dropped
    assert [c.candidate_url for c in checked] == [
        "http://files.example/a.zip",
        "http://files.example/c.zip",
    ]


def test_zero_checked_rows_produce_an_empty_plan() -> None:
    # Arrange: nothing ticked (and an entirely empty batch)
    unchecked = [
        _unchecked("http://files.example/a.zip", "a.zip"),
        _unchecked("http://files.example/b.zip", "b.zip"),
    ]

    # Act
    empty_plan = plan_discovery_queue([])
    plan = plan_discovery_queue(checked_candidates(unchecked))

    # Assert
    assert empty_plan.is_empty
    assert empty_plan.to_add == []
    assert plan.is_empty
    assert plan.to_add == []


# -- duplicates -----------------------------------------------------------------------


def test_batch_and_preexisting_duplicates_are_flagged_not_added() -> None:
    # Arrange: url A twice in the selection; url B already in the repository
    candidates = [
        _cand("http://files.example/a.zip", "a.zip"),
        _cand("http://files.example/a.zip", "a-copy.zip"),
        _cand("http://files.example/b.zip", "b.zip"),
        _cand("http://files.example/c.zip", "c.zip"),
    ]

    # Act
    plan = plan_discovery_queue(
        candidates,
        existing_urls=[" http://files.example/B.zip?x=1 ", "http://files.example/b.zip"],
        allow_private=True,
    )

    # Assert: batch repeat + pre-existing URL are skipped, first occurrence wins
    assert [c.candidate_url for c in plan.to_add] == [
        "http://files.example/a.zip",
        "http://files.example/c.zip",
    ]
    labels = {label for label, _reason in plan.duplicates}
    assert labels == {"a-copy.zip", "b.zip"}
    assert all(reason for _label, reason in plan.duplicates)
    assert any("repeated within the same selection" in reason
               for _label, reason in plan.duplicates)
    assert any("already in the download list or history" in reason
               for _label, reason in plan.duplicates)


# -- unusable URLs ----------------------------------------------------------------------


@pytest.mark.parametrize("bad_url", ["ftp://host.example/file.zip", "not a url", ""])
def test_unusable_urls_are_rejected_with_actionable_reason(bad_url: str) -> None:
    # Arrange: one ticked candidate with a deliberately unusable URL
    candidate = _cand(bad_url, "broken.bin")

    # Act
    plan = plan_discovery_queue([candidate], allow_private=True)

    # Assert: rejected with a non-empty, user-actionable reason; never added
    assert plan.to_add == []
    assert not plan.is_empty
    assert plan.duplicates == []
    assert len(plan.rejected) == 1
    label, reason = plan.rejected[0]
    assert label == "broken.bin"
    assert reason


# -- safety net ----------------------------------------------------------------------


def test_unsafe_url_error_is_a_value_error() -> None:
    # Assert the contract the planner's error handling relies on
    assert issubclass(security.UnsafeUrlError, ValueError)


# -- execute_queue_plan: failure containment -------------------------------------------


class _ReplayAdd:
    """``add_fn`` stub: raises for selected URLs, records every requested file."""

    def __init__(self, failing_urls: set[str]) -> None:
        self.failing_urls = failing_urls
        self.urls: list[str] = []

    def __call__(
        self, url: str, destination_dir: str, filename: str | None = None
    ) -> DownloadRecord:
        self.urls.append(url)
        if url in self.failing_urls:
            raise ValueError(f"destination rejected for {url}")
        return DownloadRecord(
            source_url=url,
            display_name=filename or url,
            destination_path=str(pathlib.Path(destination_dir) / (filename or "file")),
            state=DownloadState.QUEUED,
        )


def _start_counter() -> tuple[list[int], Callable[[], int]]:
    """A ``start_fn`` stub plus the list that collects its call count."""
    calls: list[int] = []

    def start_fn() -> int:
        calls.append(1)
        return len(calls)

    return calls, start_fn


def test_execute_queue_plan_contains_add_failures() -> None:
    # Arrange: two good URLs and two that the intake function refuses
    good = "http://files.example/good.zip"
    also_good = "http://files.example/alsogood.zip"
    bad = "http://files.example/bad.zip"
    failing2 = "http://files.example/bad2.zip"
    plan = QueuePlan(
        to_add=[
            _cand(good, "good.zip"),
            _cand(bad, "bad.zip"),
            _cand(also_good, "alsogood.zip"),
            _cand(failing2, "bad2.zip"),
        ]
    )
    add_fn = _ReplayAdd(failing_urls={bad, failing2})
    calls, start_fn = _start_counter()

    # Act: must not raise even though add_fn refuses half the batch
    report = execute_queue_plan(
        plan, str(pathlib.Path("tmp") / "out"), add_fn, start_fn=start_fn
    )

    # Assert: only the good rows were added (in plan order, deduplicated by URL)
    assert len(report.added) == 2
    added_urls = {rec.source_url for rec in report.added}
    assert added_urls == {good, also_good}
    assert [label for label, _reason in report.failed] == [
        "bad.zip",
        "bad2.zip",
    ]
    assert all(reason for _label, reason in report.failed)
    # every plan.to_add URL reached add_fn
    assert sorted(add_fn.urls) == sorted({good, also_good, bad, failing2})
    # start happens because at least one download was added
    assert len(calls) == 1


def test_execute_queue_plan_skips_start_when_nothing_added() -> None:
    # Arrange: the entire batch fails at add time
    plan = QueuePlan(
        to_add=[
            _cand("http://files.example/bad.zip", "bad.zip"),
            _cand("http://files.example/bad2.zip", "bad2.zip"),
        ]
    )
    add_fn = _ReplayAdd(failing_urls={c.candidate_url for c in plan.to_add})
    calls, start_fn = _start_counter()

    # Act
    report = execute_queue_plan(
        plan, str(pathlib.Path("tmp") / "out"), add_fn, start_fn=start_fn
    )

    # Assert
    assert report.added == []
    assert len(report.failed) == 2
    assert calls == []  # start_fn never invoked when nothing was added


# -- full integration: real manager, real downloads --------------------------------------


def test_plan_then_execute_with_real_manager(
    settings: Settings,
    db: Database,
    http_server,  # FakeSite
    tmp_path: pathlib.Path,
) -> None:
    # Arrange: a real manager + repository, and two ticked candidates served locally
    repo = DownloadRepository(db)
    manager = DownloadManager(settings, repo)
    try:
        candidates = [
            _cand(http_server.url("/files/data.bin"), "data.bin"),
            _unchecked("http://files.example/ignored.zip", "ignored.zip"),
            _cand(http_server.url("/files/small.zip"), "small.zip"),
        ]

        # Act: select -> plan -> execute against the real intake API
        plan = plan_discovery_queue(
            checked_candidates(candidates), allow_private=True
        )
        report = execute_queue_plan(
            plan,
            str(tmp_path / "dl"),
            manager.add_download,
            start_fn=manager.start_all,
        )
        _wait_terminal(repo, [rec.id for rec in report.added])

        # Assert: both files downloaded COMPLETED, on-disk payloads match
        assert len(report.added) == 2
        assert report.failed == []
        for record, expected_name in (
            (report.added[0], "data.bin"),
            (report.added[1], "small.zip"),
        ):
            settled = repo.get(record.id) if record.id is not None else None
            assert settled is not None
            assert settled.state is DownloadState.COMPLETED
            final = pathlib.Path(settled.destination_path)
            assert final.is_file()
            assert final.read_bytes() == http_server.file_bytes(expected_name)
            assert not pathlib.Path(str(final) + ".part").exists()

        # Re-planning the same selection is a no-op: the URLs are now duplicates
        replan = plan_discovery_queue(
            checked_candidates(candidates),
            existing_urls=[rec.source_url for rec in report.added],
            allow_private=True,
        )
        assert replan.to_add == []
        assert {label for label, _reason in replan.duplicates} == {
            "data.bin",
            "small.zip",
        }
    finally:
        manager.shutdown(wait=False)
