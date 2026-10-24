"""QThread-based runners so network/heavy operations never run on the GUI thread.

- :class:`DiscoveryWorker` runs one :meth:`LinkDiscovery.scan` with a
  cooperative ``threading.Event`` cancel token.
- :class:`FunctionRunner` runs any zero-argument callable off-thread and
  emits its return value or the failure message.

Callers must keep a reference to the runner object while it runs (otherwise
Python garbage-collects the QThread), and connect to ``succeeded`` /
``finished_scan`` / ``failed`` — those signals are emitted from the worker
thread and Qt queues them onto the GUI thread automatically.
"""

from __future__ import annotations

import threading
import traceback
from typing import Any, Callable

from PySide6.QtCore import QThread, Signal

from src.core.link_discovery import DiscoveryResult, LinkDiscovery
from src.database.models import Settings

__all__ = ["DiscoveryWorker", "FunctionRunner"]


class DiscoveryWorker(QThread):
    """Scans one page URL for download candidates on a background thread."""

    #: Emitted with the :class:`DiscoveryResult` on success (errors included
    #: inside the result, mirroring the core contract).
    finished_scan = Signal(object)  # DiscoveryResult
    #: Emitted with a message when the scan itself crashed unexpectedly.
    failed = Signal(str)

    def __init__(self, page_url: str, settings: Settings, parent: Any = None) -> None:
        """Create (but do not start) the scanner thread.

        Args:
            page_url: page to scan.
            settings: validated settings (robots/rate-limit/UA are honoured).
            parent: optional Qt parent keeping the thread alive.
        """
        super().__init__(parent)
        self._page_url = page_url
        self._settings = settings
        self._discovery = LinkDiscovery()
        self._cancel_token = threading.Event()

    def cancel(self) -> None:
        """Request a cooperative stop; the scan raises between requests."""
        self._cancel_token.set()

    def run(self) -> None:  # noqa: D102 - QThread override
        try:
            result: DiscoveryResult = self._discovery.scan(
                self._page_url, self._settings, cancel_token=self._cancel_token
            )
        except Exception as exc:  # noqa: BLE001 - a crash must reach the GUI nicely
            self.failed.emit(
                "Page scan failed: "
                + ("".join(traceback.format_exception_only(type(exc), exc)).strip())
            )
            return
        self.finished_scan.emit(result)


class FunctionRunner(QThread):
    """Runs a zero-argument callable on a background thread."""

    succeeded = Signal(object)
    failed = Signal(str)

    def __init__(
        self, fn: Callable[[], Any], *, description: str = "", parent: Any = None
    ) -> None:
        """Create (but do not start) the runner.

        Args:
            fn: callable to execute; long-running/blocking ops only.
            description: short label used in failure messages.
            parent: optional Qt parent keeping the thread alive.
        """
        super().__init__(parent)
        if not callable(fn):
            raise TypeError("fn must be a zero-argument callable")
        self._fn = fn
        self._description = description or "operation"

    def run(self) -> None:  # noqa: D102 - QThread override
        try:
            result = self._fn()
        except Exception as exc:  # noqa: BLE001 - errors belong to the GUI
            self.failed.emit(f"{self._description} failed: {exc}")
            return
        self.succeeded.emit(result)
