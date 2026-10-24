"""Bounded in-flight slot tracker used by :class:`src.core.download_manager.DownloadManager`.

Deliberately tiny: it knows nothing about threads, executors or retries —
it only answers "may download *n* start right now?" and "what is running
at the moment?". All methods are thread-safe via a single
:class:`threading.Lock` and are cheap enough to call from worker threads.
"""

from __future__ import annotations

import threading

__all__ = ["QueueScheduler"]


class QueueScheduler:
    """Tracks in-flight download ids against a maximum-concurrency limit.

    A slot is acquired with :meth:`try_acquire` and must be released
    exactly once with :meth:`release` (idempotent for foreign ids, so
    defensive double-release is harmless). The id space is the download
    database id (positive ints); every other integer is accepted too,
    which keeps this helper usable with synthetic test ids.
    """

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._in_flight: set[int] = set()

    def try_acquire(self, download_id: int, max_concurrent: int) -> bool:
        """Atomically grab a slot for ``download_id`` if one is free.

        Args:
            download_id: identifier of the download to admit (never a
                duplicate while it is already in flight).
            max_concurrent: concurrency ceiling for this call; values
                below 1 admit nothing.

        Returns:
            ``True`` when the id was admitted under the ceiling; ``False``
            when it is already running or every slot is taken.
        """
        with self._lock:
            if download_id in self._in_flight:
                return False
            if max_concurrent < 1 or len(self._in_flight) >= max_concurrent:
                return False
            self._in_flight.add(download_id)
            return True

    def release(self, download_id: int) -> None:
        """Free the slot held by ``download_id`` (no-op if it held none)."""
        with self._lock:
            self._in_flight.discard(download_id)

    def pending(self) -> list[int]:
        """Snapshot of the currently in-flight (acquired) ids, sorted ascending."""
        with self._lock:
            return sorted(self._in_flight)

    def is_running(self, download_id: int) -> bool:
        """True while ``download_id`` holds a slot."""
        with self._lock:
            return download_id in self._in_flight
