"""Queue page: live table over queued/running/paused/interrupted downloads.

The model refreshes on a ~500 ms timer that merges repository rows with the
latest :class:`DownloadProgress` snapshots relayed by the manager, updating
cells in place (no reset flicker) when the row set is unchanged.
"""

from __future__ import annotations

import urllib.parse
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from PySide6.QtCore import QAbstractTableModel, QModelIndex, Qt, QTimer, QUrl, Signal
from PySide6.QtGui import QDesktopServices
from PySide6.QtWidgets import (
    QAbstractItemView,
    QHeaderView,
    QHBoxLayout,
    QLabel,
    QMessageBox,
    QPushButton,
    QStackedWidget,
    QTableView,
    QVBoxLayout,
    QWidget,
)

from src.app.dialogs import DetailsDialog, confirm
from src.app.utils_format import format_bytes, format_eta, format_speed
from src.database.models import DownloadProgress, DownloadRecord, DownloadState

if TYPE_CHECKING:  # pragma: no cover - typing only
    from PySide6.QtCore import QObject

    from src.app.main_window import CoreSignals
    from src.core.download_manager import DownloadManager
    from src.database.repository import DownloadRepository

__all__ = ["QueuePage", "QueueTableModel"]

_LIVE_STATES = (
    DownloadState.QUEUED,
    DownloadState.CONNECTING,
    DownloadState.DOWNLOADING,
    DownloadState.PAUSED,
    DownloadState.INTERRUPTED,
)


@dataclass
class _QueueRow:
    """Model payload for one queue row."""

    record: DownloadRecord
    progress: DownloadProgress | None = field(default=None)


class QueueTableModel(QAbstractTableModel):
    """Columns: filename, host, size, downloaded, %, speed, ETA, state, destination."""

    _HEADERS = ("Filename", "Host", "Size", "Downloaded", "Progress", "Speed", "ETA", "Status", "Destination")

    def __init__(self, parent: Any = None) -> None:
        super().__init__(parent)
        self._rows: list[_QueueRow] = []

    # -- Qt plumbing -------------------------------------------------------
    def rowCount(self, parent: QModelIndex = QModelIndex()) -> int:  # noqa: N802
        return 0 if parent.isValid() else len(self._rows)

    def columnCount(self, parent: QModelIndex = QModelIndex()) -> int:  # noqa: N802
        return 0 if parent.isValid() else len(self._HEADERS)

    def headerData(self, section: int, orientation: Qt.Orientation, role: int = Qt.ItemDataRole.DisplayRole) -> Any:  # noqa: N802
        if orientation == Qt.Orientation.Horizontal and role == Qt.ItemDataRole.DisplayRole:
            return self._HEADERS[section]
        return None

    def flags(self, index: QModelIndex) -> Qt.ItemFlag:
        return Qt.ItemFlag.ItemIsEnabled | Qt.ItemFlag.ItemIsSelectable

    def data(self, index: QModelIndex, role: int = Qt.ItemDataRole.DisplayRole) -> Any:
        if not index.isValid():
            return None
        row = self._rows[index.row()]
        record = row.record
        progress = row.progress
        column = index.column()
        if role == Qt.ItemDataRole.ToolTipRole:
            tooltip = f"URL: {record.source_url}"
            if record.error_summary:
                tooltip += f"\nError: {record.error_summary}"
            return tooltip
        if role != Qt.ItemDataRole.DisplayRole:
            return None
        match column:
            case 0:
                return record.display_name
            case 1:
                return urllib.parse.urlsplit(record.source_url).hostname or ""
            case 2:
                total = progress.total_bytes if progress and progress.total_bytes else record.expected_size
                return format_bytes(total)
            case 3:
                return format_bytes(record.downloaded_bytes)
            case 4:
                total = progress.total_bytes if progress and progress.total_bytes else record.expected_size
                if total:
                    return f"{min(100.0, record.downloaded_bytes * 100 / total):.1f}%"
                return "—"
            case 5:
                return format_speed(progress.speed_bps if progress else 0.0)
            case 6:
                return format_eta(progress.eta_seconds if progress else None)
            case 7:
                return record.state.value
            case 8:
                return record.destination_path
        return None

    # -- updates -------------------------------------------------------------
    def row_id(self, row: int) -> int | None:
        """Download id of ``row`` or ``None``."""
        if row < 0 or row >= len(self._rows):
            return None
        return self._rows[row].record.id

    def set_records(self, records: list[DownloadRecord]) -> None:
        """Diff-apply a fresh snapshot of queue records (in-place when stable)."""
        fresh_ids = [record.id for record in records]
        current_ids = [row.record.id for row in self._rows]
        if fresh_ids != current_ids:
            self.beginResetModel()
            previous = {row.record.id: row for row in self._rows}
            self._rows = [previous.get(record.id, _QueueRow(record=record)) for record in records]
            self.endResetModel()
            return
        for index, record in enumerate(records):
            row = self._rows[index]
            if (
                record.downloaded_bytes != row.record.downloaded_bytes
                or record.state != row.record.state
                or record.expected_size != row.record.expected_size
                or record.error_summary != row.record.error_summary
            ):
                self._rows[index] = _QueueRow(record=record, progress=row.progress)
                self.dataChanged.emit(
                    self.index(index, 0), self.index(index, self.columnCount() - 1)
                )

    def apply_progress(self, progress: DownloadProgress) -> None:
        """Update one row's speed/ETA/size from a relayed progress snapshot."""
        for index, row in enumerate(self._rows):
            if row.record.id == progress.download_id:
                self._rows[index] = _QueueRow(record=row.record, progress=progress)
                self.dataChanged.emit(
                    self.index(index, 2), self.index(index, 6)
                )
                return

    def selected_ids(self, view: QTableView) -> list[int]:
        """Download ids of the currently selected rows (ascending)."""
        ids: list[int] = []
        for model_index in sorted(view.selectionModel().selectedRows()):
            row_id = self.row_id(model_index.row())
            if row_id is not None:
                ids.append(row_id)
        return ids


class QueuePage(QWidget):
    """Buttons + table wired to the manager; empty-state friendly label."""

    actionFailed = Signal(str)

    def __init__(
        self,
        manager: "DownloadManager",
        repository: "DownloadRepository",
        relay: "CoreSignals",
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self._manager = manager
        self._repo = repository

        self._model = QueueTableModel(self)
        self._view = QTableView()
        self._view.setModel(self._model)
        self._view.setSelectionBehavior(QAbstractItemView.SelectionBehavior.SelectRows)
        self._view.setSelectionMode(QAbstractItemView.SelectionMode.ExtendedSelection)
        self._view.setEditTriggers(QAbstractItemView.EditTrigger.NoEditTriggers)
        self._view.setAlternatingRowColors(True)
        self._view.setShowGrid(False)
        self._view.verticalHeader().setVisible(False)
        self._view.horizontalHeader().setStretchLastSection(True)
        self._view.horizontalHeader().setSectionResizeMode(0, QHeaderView.ResizeMode.Stretch)
        self._view.setColumnWidth(4, 160)
        self._view.setWordWrap(False)

        self._empty = QLabel(
            "Nothing queued yet. Add downloads from the Add Downloads page,\n"
            "or scan a web page under Discover."
        )
        self._empty.setObjectName("muted")
        self._empty.setWordWrap(True)
        self._empty.setAlignment(Qt.AlignmentFlag.AlignCenter)

        self._content = QStackedWidget()
        self._content.addWidget(self._empty)
        self._content.addWidget(self._view)
        self._content.setCurrentIndex(0)

        self._start_all_btn = self._button("Start All", self._start_all, primary=True)
        self._pause_btn = self._button("Pause", self._pause)
        self._resume_btn = self._button("Resume", self._resume)
        self._retry_btn = self._button("Retry", self._retry)
        self._cancel_btn = self._button("Cancel", self._cancel)
        self._remove_btn = self._button("Remove", self._remove)
        self._open_btn = self._button("Open Folder", self._open_folder)
        self._details_btn = self._button("Details", self._show_details)

        buttons = QHBoxLayout()
        buttons.setContentsMargins(0, 0, 0, 0)
        buttons.addWidget(self._start_all_btn)
        buttons.addWidget(self._pause_btn)
        buttons.addWidget(self._resume_btn)
        buttons.addWidget(self._retry_btn)
        buttons.addWidget(self._cancel_btn)
        buttons.addWidget(self._remove_btn)
        buttons.addStretch(1)
        buttons.addWidget(self._open_btn)
        buttons.addWidget(self._details_btn)

        root = QVBoxLayout(self)
        root.setContentsMargins(24, 20, 24, 20)
        root.setSpacing(12)
        heading = QLabel("Queue")
        heading.setObjectName("heading")
        root.addWidget(heading)
        root.addLayout(buttons)
        root.addWidget(self._content, 1)

        relay.stateChangedEvent.connect(lambda _r: self.refresh())
        relay.progressEvent.connect(self._model.apply_progress)

        self._timer = QTimer(self)
        self._timer.setInterval(500)
        self._timer.timeout.connect(lambda: self.refresh())
        self._timer.start()

        self.refresh()

    def _button(self, label: str, handler: Any, *, primary: bool = False) -> QPushButton:
        button = QPushButton(label)
        if primary:
            button.setObjectName("primary")
        button.clicked.connect(handler)
        return button

    # ---------------------------------------------------------------- actions
    def _selected_ids(self) -> list[int]:
        return self._model.selected_ids(self._view)

    def _start_all(self) -> None:
        self._act_checked("Start All", self._manager.start_all)

    def _guarded(self, label: str, action: Any, download_id: int) -> None:
        try:
            action(download_id)
        except (ValueError, RuntimeError) as exc:
            QMessageBox.warning(self, label, str(exc))

    def _act_checked(self, label: str, action: Any) -> None:
        """Run a no-arg manager action with friendly error reporting."""
        try:
            action()
        except (ValueError, RuntimeError) as exc:
            QMessageBox.warning(self, label, str(exc))
        self.refresh()

    def _pause(self) -> None:
        ids = self._selected_ids()
        if not ids:
            self._nothing_selected("Pause")
            return
        for download_id in ids:
            self._guarded("Pause", self._manager.pause, download_id)
        self.refresh()

    def _resume(self) -> None:
        ids = self._selected_ids()
        if not ids:
            self._nothing_selected("Resume")
            return
        for download_id in ids:
            self._guarded("Resume", self._manager.resume, download_id)
        self.refresh()

    def _retry(self) -> None:
        ids = self._selected_ids()
        if not ids:
            self._nothing_selected("Retry")
            return
        retryable = {
            DownloadState.FAILED,
            DownloadState.INTERRUPTED,
            DownloadState.CANCELLED,
            DownloadState.PAUSED,
        }
        selected_states = {self._manager.get(download_id) for download_id in ids}
        if all(record is not None and record.state not in retryable for record in selected_states):
            QMessageBox.information(
                self, "Retry", "Retry applies to failed, interrupted, cancelled or paused downloads."
            )
            return
        for download_id in ids:
            self._guarded("Retry", self._manager.retry, download_id)
        self.refresh()

    def _cancel(self) -> None:
        ids = self._selected_ids()
        if not ids:
            self._nothing_selected("Cancel")
            return
        if not confirm(
            self,
            "Cancel downloads",
            f"Cancel {len(ids)} download(s)? Partial files are kept for resume-capable ones.",
        ):
            return
        for download_id in ids:
            self._guarded("Cancel", self._manager.cancel, download_id)
        self.refresh()

    def _remove(self) -> None:
        ids = self._selected_ids()
        if not ids:
            self._nothing_selected("Remove")
            return
        if not confirm(
            self,
            "Remove downloads",
            f"Remove {len(ids)} download(s) from the queue/history? Files are kept.",
        ):
            return
        for download_id in ids:
            try:
                self._manager.remove(download_id)
            except (ValueError, RuntimeError) as exc:
                QMessageBox.warning(self, "Remove", str(exc))
        self.refresh()

    def _open_folder(self) -> None:
        ids = self._selected_ids()
        if not ids:
            self._nothing_selected("Open Folder")
            return
        record = self._manager.get(ids[0])
        if record is None:
            QMessageBox.information(self, "Open Folder", "The selected download no longer exists.")
            return
        QDesktopServices.openUrl(QUrl.fromLocalFile(record.destination_path))

    def _show_details(self) -> None:
        ids = self._selected_ids()
        if not ids:
            self._nothing_selected("Details")
            return
        record = self._manager.get(ids[0])
        if record is not None:
            DetailsDialog(record, self).exec()

    # -------------------------------------------------------------- helpers
    def _all_ids(self) -> list[int]:
        ids: list[int] = []
        for row in range(self._model.rowCount()):
            row_id = self._model.row_id(row)
            if row_id is not None:
                ids.append(row_id)
        return ids

    def _nothing_selected(self, action: str) -> None:
        QMessageBox.information(self, action, f"Select one or more queue rows first to {action.lower()}.")

    # -------------------------------------------------------------- refresh
    def refresh(self) -> None:
        """Merge active + queued records from the repository into the model."""
        try:
            records = self._repo.list_by_state(*_LIVE_STATES)
        except Exception as exc:  # pragma: no cover - DB hiccups must not crash
            QMessageBox.critical(self, "Queue", f"failed to load the queue: {exc}")
            return
        self._model.set_records(records)
        has_rows = self._model.rowCount() > 0
        self._content.setCurrentIndex(1 if has_rows else 0)
