"""ThreadPoolExecutor-backed manager coordinating many download workers.

One :class:`DownloadManager` owns:

- a bounded :class:`concurrent.futures.ThreadPoolExecutor` whose thread
  count mirrors ``settings.max_simultaneous`` (clamped to 1..10),
- a :class:`src.core.queue_scheduler.QueueScheduler` granting in-flight
  slots so no download id ever runs twice,
- per-id :class:`src.core.download_worker.DownloadWorker` instances built
  lazily at dispatch time, each with its own cancel/pause events,
- persistence of every lifecycle transition through the injected
  :class:`src.database.repository.DownloadRepository`.

GUI integration happens via the ``on_progress`` / ``on_state_change``
callback attributes — this module never imports PySide6 and is fully
thread-safe: every mutable map is guarded by one re-entrant lock.
"""

from __future__ import annotations

import contextlib
import logging
import threading
from concurrent.futures import Future, ThreadPoolExecutor
from typing import Callable

from src.core import filename_utils, security
from src.core.download_worker import DownloadWorker
from src.core.queue_scheduler import QueueScheduler
from src.database.models import DownloadProgress, DownloadRecord, DownloadState, Settings
from src.database.repository import DownloadRepository

__all__ = ["DownloadManager"]

log = logging.getLogger(__name__)

_MIN_CONCURRENT = 1
_MAX_CONCURRENT = 10


class DownloadManager:
    """Adds downloads to the database and runs them on an executor.

    All mutating methods are safe to call from any thread (GUI thread or
    worker callbacks). The manager itself performs no network I/O — that
    is delegated to one :class:`DownloadWorker` per download.
    """

    def __init__(self, settings: Settings, repository: DownloadRepository) -> None:
        """Create a manager.

        Args:
            settings: validated application settings; ``max_simultaneous``
                is clamped to 1..10 to size the executor and scheduler.
            repository: persistence sink for records and state changes.
        """
        self._settings = settings
        self._repo = repository
        self._lock = threading.RLock()
        self._max_concurrent = max(_MIN_CONCURRENT, min(_MAX_CONCURRENT, int(settings.max_simultaneous)))
        self._scheduler = QueueScheduler()
        self._executor = ThreadPoolExecutor(
            max_workers=self._max_concurrent, thread_name_prefix="bdm-worker"
        )
        # per-id bookkeeping (all guarded by ``self._lock``)
        self._workers: dict[int, DownloadWorker] = {}
        self._futures: dict[int, Future[DownloadRecord]] = {}
        self._cancel_events: dict[int, threading.Event] = {}
        self._pause_events: dict[int, threading.Event] = {}
        self._live_speeds: dict[int, float] = {}
        # observer hooks (invoked on worker threads; keep them fast)
        self.on_progress: Callable[[DownloadProgress], None] | None = None
        self.on_state_change: Callable[[DownloadRecord], None] | None = None

    # ------------------------------------------------------------------ API
    def add_download(
        self,
        url: str,
        destination_dir: str,
        filename: str | None = None,
        checksum: str | None = None,
    ) -> DownloadRecord:
        """Validate, sanitise and persist a new download as ``QUEUED``.

        The URL is network-validated (honouring
        ``settings.allow_private_addresses``), the filename is sanitised
        (derived from the URL when not given) and the collision policy is
        applied pre-flight so the record's final path is authoritative.

        Args:
            url: source URL (http/https).
            destination_dir: target directory; when empty,
                ``settings.default_destination_dir`` is used.
            filename: optional desired filename (sanitised first).
            checksum: optional SHA-256 digest as bare hex or
                ``sha256:hex`` for post-download verification.

        Returns:
            The persisted record, including its database id.

        Raises:
            ValueError: blank destination directory, unusable checksum, or
                a ``skip`` collision policy that declines an existing file.
        """
        validated = security.validate_url_for_network(
            url, allow_private=self._settings.allow_private_addresses
        )
        checksum = _normalize_checksum(checksum)

        directory = destination_dir or self._settings.default_destination_dir
        if not directory:
            raise ValueError(
                "no destination directory given and settings."
                "default_destination_dir is empty"
            )

        name = (
            filename_utils.sanitize_filename(filename)
            if filename
            else filename_utils.filename_from_url(validated.url)
        )
        destination = filename_utils.resolve_destination(directory, name)
        resolved = filename_utils.unique_path(destination, self._settings.filename_collision)
        if resolved is None:
            raise ValueError(
                f"destination {destination.name!r} already exists and the "
                "collision policy is 'skip'; choose another filename or policy"
            )
        if resolved != destination:
            name = resolved.name
            destination = resolved

        record = DownloadRecord(
            source_url=url.strip(),
            display_name=name,
            destination_path=str(destination),
            state=DownloadState.QUEUED,
            checksum_sha256=checksum,
        )
        return self._repo.add(record)

    def start_all(self) -> int:
        """Dispatch every ``QUEUED`` record that has a free slot.

        Returns:
            How many downloads were actually dispatched this call.
        """
        dispatched = 0
        for record in self._repo.list_by_state(DownloadState.QUEUED):
            if self._dispatch(record):
                dispatched += 1
        return dispatched

    def pause(self, download_id: int) -> None:
        """Pause a download: running ones pause cooperatively, queued ones
        are marked ``PAUSED`` in place.

        Raises:
            ValueError: unknown download id.
        """
        with self._lock:
            if download_id in self._futures:
                self._pause_events[download_id].set()
                return
        record = self._require(download_id)
        if record.state is DownloadState.QUEUED:
            record.state = DownloadState.PAUSED
            self._repo.update(record)
            self._notify_state(record)

    def resume(self, download_id: int) -> None:
        """Resume a ``PAUSED``/``INTERRUPTED`` download (partial ``.part``
        files are extended when the server supports ranges).

        Raises:
            ValueError: unknown id or a state that cannot be resumed.
            RuntimeError: every concurrent slot is taken — the record is
                queued instead and can be picked up by :meth:`start_all`.
        """
        with self._lock:
            if download_id in self._futures:
                return  # already running
        record = self._require(download_id)
        if record.state not in (DownloadState.PAUSED, DownloadState.INTERRUPTED, DownloadState.QUEUED):
            raise ValueError(
                f"download {download_id} is {record.state.value}; only paused, "
                "interrupted or queued downloads can be resumed"
            )
        if not self._dispatch(record):
            record.state = DownloadState.QUEUED
            self._repo.update(record)
            self._notify_state(record)
            raise RuntimeError(
                f"no free download slot; download {download_id} was queued"
            )

    def retry(self, download_id: int) -> None:
        """Reset retry bookkeeping of a settled download to ``QUEUED`` and dispatch it.

        Raises:
            ValueError: unknown id or a download that is currently running.
        """
        with self._lock:
            if download_id in self._futures:
                raise ValueError(f"download {download_id} is currently running; pause or cancel it first")
        record = self._require(download_id)
        record.retry_count = 0
        record.error_summary = None
        record.state = DownloadState.QUEUED
        self._repo.update(record)
        self._notify_state(record)
        if not self._dispatch(record):
            raise RuntimeError(
                f"no free download slot; download {download_id} stays QUEUED "
                "and will start when a slot frees up"
            )

    def cancel(self, download_id: int) -> None:
        """Cancel a download (running ones settle to ``CANCELLED`` cooperatively)."""
        with self._lock:
            if download_id in self._futures:
                self._cancel_events[download_id].set()
                return
        record = self._repo.get(download_id)
        if record is None or record.state in DownloadState.TERMINAL_STATES:
            return
        record.state = DownloadState.CANCELLED
        self._repo.update(record)
        self._notify_state(record)

    def remove(self, download_id: int) -> bool:
        """Delete a download's row — allowed when idle, or when a cancel
        has already been requested for a running one.

        Returns:
            ``True`` when the row was deleted, ``False`` if it was gone already.

        Raises:
            ValueError: the download is actively running and has not been
                cancelled yet.
        """
        with self._lock:
            future = self._futures.get(download_id)
            if future is not None:
                cancel_event = self._cancel_events.get(download_id)
                if cancel_event is None or not cancel_event.is_set():
                    raise ValueError(
                        f"download {download_id} is actively downloading; "
                        "cancel it before removing"
                    )
                # Cancel already in flight: the settling worker's writes are
                # no-ops once the row is gone; its done-callback cleans up.
        return self._repo.delete(download_id)

    def shutdown(self, wait: bool = True) -> None:
        """Stop the executor.

        Pending (not yet started) worker submissions still execute once a
        thread frees up unless ``cancel_futures`` semantics are wanted;
        running downloads settle to their natural state. Call
        :meth:`pause` / :meth:`cancel` first for a controlled stop.
        """
        self._executor.shutdown(wait=wait)
        with self._lock:
            self._futures.clear()
            self._workers.clear()
            self._cancel_events.clear()
            self._pause_events.clear()
            self._live_speeds.clear()
        log.info("download manager shut down (wait=%s)", wait)

    def total_speed_bps(self) -> float:
        """Sum of the live per-worker speeds (lock-protected snapshot)."""
        with self._lock:
            return sum(self._live_speeds.values())

    def get(self, download_id: int) -> DownloadRecord | None:
        """Fetch one record from the repository or ``None``."""
        return self._repo.get(download_id)

    def list_active(self) -> list[DownloadRecord]:
        """Records whose ids currently hold an in-flight slot."""
        with self._lock:
            ids = list(self._futures)
        records: list[DownloadRecord] = []
        for download_id in sorted(ids):
            record = self._repo.get(download_id)
            if record is not None:
                records.append(record)
        return records

    @property
    def max_simultaneous(self) -> int:
        """Effective concurrency ceiling (settings clamped to 1..10)."""
        return self._max_concurrent

    # -------------------------------------------------------------- internals
    def _dispatch(self, record: DownloadRecord) -> bool:
        """Send one record to the executor if it holds a free slot.

        Returns ``False`` when the id is already running or no slot is
        free (the record is left untouched, still eligible for a later
        :meth:`start_all` / pump).
        """
        record_id = record.id
        if record_id is None:
            return False
        with self._lock:
            if record_id in self._futures:
                return False
            if not self._scheduler.try_acquire(record_id, self._max_concurrent):
                return False
            cancel_event = threading.Event()
            pause_event = threading.Event()
            self._cancel_events[record_id] = cancel_event
            self._pause_events[record_id] = pause_event
            worker = DownloadWorker(
                record,
                self._settings,
                repository=self._repo,
                on_progress=lambda p, _id=record_id: self._on_worker_progress(_id, p),
                on_state_change=lambda rec, _id=record_id: self._on_worker_state(_id, rec),
                cancel_event=cancel_event,
                pause_event=pause_event,
            )
            self._workers[record_id] = worker
            try:
                future = self._executor.submit(worker.run)
            except RuntimeError:
                # Executor already shut down: restore the slot and bail out.
                self._workers.pop(record_id, None)
                self._cancel_events.pop(record_id, None)
                self._pause_events.pop(record_id, None)
                self._scheduler.release(record_id)
                return False
            self._futures[record_id] = future
            future.add_done_callback(lambda _f, _id=record_id: self._on_worker_done(_id, _f))
            log.info("download %s dispatched (%d/%d slots in use)", record_id, len(self._futures), self._max_concurrent)
            return True

    def _on_worker_done(self, download_id: int, future: Future[DownloadRecord]) -> None:
        """Executor callback: free the slot and per-id bookkeeping."""
        _exc = future.exception()  # worker.run() never raises for expected outcomes;
        if _exc is not None:  # an escaping exception means a manager-level bug.
            log.error("download %d worker crashed: %s", download_id, _exc)
        with self._lock:
            self._futures.pop(download_id, None)
            self._workers.pop(download_id, None)
            self._cancel_events.pop(download_id, None)
            self._pause_events.pop(download_id, None)
            self._live_speeds.pop(download_id, None)
        self._scheduler.release(download_id)
        self._pump()

    def _pump(self) -> None:
        """Keep slots busy: dispatch queued records while slots are free."""
        with self._lock:
            if len(self._futures) >= self._max_concurrent:
                return
        for record in self._repo.list_by_state(DownloadState.QUEUED):
            if not self._dispatch(record):
                break

    def _on_worker_progress(self, download_id: int, progress: DownloadProgress) -> None:
        """Track live speed, then forward to the observer (never raises)."""
        with self._lock:
            self._live_speeds[download_id] = progress.speed_bps
        observer = self.on_progress
        if observer is not None:
            with contextlib.suppress(Exception):
                observer(progress)

    def _on_worker_state(self, download_id: int, record: DownloadRecord) -> None:
        """Forward a persisted state change to the observer (never raises)."""
        if record.state not in (DownloadState.DOWNLOADING, DownloadState.CONNECTING):
            with self._lock:
                self._live_speeds.pop(download_id, None)
        observer = self.on_state_change
        if observer is not None:
            with contextlib.suppress(Exception):
                observer(record)

    def _require(self, download_id: int) -> DownloadRecord:
        """Fetch a record or raise ``ValueError`` with an actionable message."""
        record = self._repo.get(download_id)
        if record is None:
            raise ValueError(f"unknown download id {download_id}")
        return record

    def _notify_state(self, record: DownloadRecord) -> None:
        observer = self.on_state_change
        if observer is not None:
            with contextlib.suppress(Exception):
                observer(record)


def _normalize_checksum(value: str | None) -> str | None:
    """Accept ``sha256:<hex>`` or bare 64-char hex; reject anything else."""
    if value is None:
        return None
    text = value.strip().lower()
    if text.startswith("sha256:"):
        text = text.split(":", 1)[1].strip()
    if not text:
        return None
    if len(text) != 64 or any(ch not in "0123456789abcdef" for ch in text):
        raise ValueError(
            "checksum must be a 64-character hex SHA-256 digest, "
            "optionally prefixed with 'sha256:'"
        )
    return text
