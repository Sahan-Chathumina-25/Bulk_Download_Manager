"""History page: searchable, sortable table over terminal-state downloads.

Rows are loaded from :meth:`DownloadRepository.history` — filtering, search
and sorting happen inside SQL so the table only mirrors one query window.
Header clicks re-issue the query with a whitelisted sort column; *Retry*,
*Open destination* and *Clear History* act on the selection.
"""

from __future__ import annotations

import pathlib
from typing import TYPE_CHECKING, Any

from PySide6.QtCore import QAbstractTableModel, QModelIndex, Qt, QTimer, QUrl, Signal
from PySide6.QtGui import QDesktopServices
from PySide6.QtWidgets import (
    QAbstractItemView,
    QComboBox,
    QHBoxLayout,
    QHeaderView,
    QLabel,
    QLineEdit,
    QMessageBox,
    QPushButton,
    QStackedWidget,
    QTableView,
    QVBoxLayout,
    QWidget,
)

from src.app.dialogs import ConfirmDialog
from src.app.utils_format import format_bytes
from src.app.worker_threads import FunctionRunner
from src.database.models import DownloadRecord, DownloadState
from src.database.repository import SORTABLE_COLUMNS

if TYPE_CHECKING:  # pragma: no cover - typing only
    from src.app.main_window import CoreSignals
    from src.core.download_manager import DownloadManager
    from src.database.repository import DownloadRepository

__all__ = ["HistoryPage", "HistoryTableModel"]

#: History query window (rows are sortable in SQL; pagination later if needed).
_HISTORY_LIMIT = 500

#: Header column -> whitelisted repository sort column (``None`` = not sortable).
_SORT_BY_COLUMN: dict[int, str | None] = {
    0: "display_name",
    1: "state",
    2: "downloaded_bytes",
    3: "destination_path",
    4: "created_at",
    5: "completed_at",
    6: None,  # error text is free-form
}


class HistoryTableModel(QAbstractTableModel):
    """Columns: file, status, size, destination, created, completed, error."""

    #: Emitted as ``(sort_column, "ASC" | "DESC")`` when a header is clicked.
    sortRequested = Signal(str, str)

    _HEADERS = ("File", "Status", "Size", "Destination", "Created", "Completed", "Error")

    def __init__(self, parent: Any = None) -> None:
        super().__init__(parent)
        self._rows: list[DownloadRecord] = []

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
        record = self._rows[index.row()]
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
                return record.state.value
            case 2:
                return format_bytes(record.downloaded_bytes)
            case 3:
                return record.destination_path
            case 4:
                return _short_date(record.created_at)
            case 5:
                return _short_date(record.completed_at or "")
            case 6:
                return record.error_summary or "—"
        return None

    # -- updates -------------------------------------------------------------
    def row_id(self, row: int) -> int | None:
        """Download id of ``row`` or ``None``."""
        if row < 0 or row >= len(self._rows):
            return None
        return self._rows[row].id

    def set_records(self, records: list[DownloadRecord]) -> None:
        """Replace the result window in one reset (sorted SQL output)."""
        self.beginResetModel()
        self._rows = list(records)
        self.endResetModel()

    def sort(self, column: int, order: Qt.SortOrder) -> None:  # noqa: N802
        """Map a header click onto a repository sort (server-side reorder)."""
        sort_column = _SORT_BY_COLUMN.get(column, None)
        if sort_column is None:
            return
        assert sort_column in SORTABLE_COLUMNS  # keep in sync with repository
        direction = "ASC" if order == Qt.SortOrder.AscendingOrder else "DESC"
        self.sortRequested.emit(sort_column, direction)


class HistoryPage(QWidget):
    """Search/filter toolbar + table with retry, open and clear actions."""

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
        self._sort_by = "created_at"
        self._sort_dir = "DESC"
        self._clear_runner: FunctionRunner | None = None

        # -- toolbar ---------------------------------------------------------
        self._search_edit = QLineEdit()
        self._search_edit.setPlaceholderText("Search name, file or URL… (Enter)")
        self._search_edit.setClearButtonEnabled(True)
        self._search_timer = QTimer(self)
        self._search_timer.setSingleShot(True)
        self._search_timer.setInterval(350)  # debounce typing
        self._search_timer.timeout.connect(self.refresh)
        self._search_edit.textChanged.connect(lambda _text: self._search_timer.start())
        self._search_edit.returnPressed.connect(self._search_now)

        self._state_combo = QComboBox()
        self._state_combo.addItem("All states", None)
        self._state_combo.addItem("Completed", DownloadState.COMPLETED.value)
        self._state_combo.addItem("Failed", DownloadState.FAILED.value)
        self._state_combo.addItem("Cancelled", DownloadState.CANCELLED.value)
        self._state_combo.addItem("Interrupted", DownloadState.INTERRUPTED.value)
        self._state_combo.currentIndexChanged.connect(lambda _i: self.refresh())

        toolbar = QHBoxLayout()
        toolbar.setContentsMargins(0, 0, 0, 0)
        toolbar.addWidget(QLabel("Search"))
        toolbar.addWidget(self._search_edit, 1)
        toolbar.addWidget(QLabel("State"))
        toolbar.addWidget(self._state_combo)

        # -- table + empty state ---------------------------------------------
        self._model = HistoryTableModel(self)
        self._view = QTableView()
        self._view.setModel(self._model)
        self._view.setSelectionBehavior(QAbstractItemView.SelectionBehavior.SelectRows)
        self._view.setSelectionMode(QAbstractItemView.SelectionMode.ExtendedSelection)
        self._view.setEditTriggers(QAbstractItemView.EditTrigger.NoEditTriggers)
        self._view.setAlternatingRowColors(True)
        self._view.setShowGrid(False)
        self._view.setWordWrap(False)
        self._view.setSortingEnabled(True)
        self._view.verticalHeader().setVisible(False)
        header = self._view.horizontalHeader()
        header.setStretchLastSection(True)
        self._view.horizontalHeader().setSectionResizeMode(0, QHeaderView.ResizeMode.Stretch)
        self._view.horizontalHeader().setSectionResizeMode(3, QHeaderView.ResizeMode.Stretch)
        for column in (1, 2, 4, 5):
            self._view.setColumnWidth(column, 110)
        header.setSortIndicator(4, Qt.SortOrder.DescendingOrder)
        self._model.sortRequested.connect(self._apply_header_sort)
        self._view.doubleClicked.connect(lambda _index: self._open_destination())

        self._empty = QLabel(
            "No history yet. Finished, failed, cancelled and interrupted\n"
            "downloads appear here automatically."
        )
        self._empty.setObjectName("muted")
        self._empty.setWordWrap(True)
        self._empty.setAlignment(Qt.AlignmentFlag.AlignCenter)

        self._content = QStackedWidget()
        self._content.addWidget(self._empty)
        self._content.addWidget(self._view)
        self._content.setCurrentIndex(0)

        # -- buttons -----------------------------------------------------------
        self._retry_btn = self._button("Retry", self._retry_selected, primary=True)
        self._open_btn = self._button("Open destination", self._open_destination)
        self._clear_btn = self._button("Clear History…", self._clear_history)
        buttons = QHBoxLayout()
        buttons.setContentsMargins(0, 0, 0, 0)
        buttons.addWidget(self._retry_btn)
        buttons.addWidget(self._open_btn)
        buttons.addStretch(1)
        buttons.addWidget(self._clear_btn)

        root = QVBoxLayout(self)
        root.setContentsMargins(24, 20, 24, 20)
        root.setSpacing(12)
        heading = QLabel("History")
        heading.setObjectName("heading")
        root.addWidget(heading)
        root.addLayout(toolbar)
        root.addLayout(buttons)
        root.addWidget(self._content, 1)

        relay.stateChangedEvent.connect(lambda _record: self.refresh())

    def _button(self, label: str, handler: Any, *, primary: bool = False) -> QPushButton:
        button = QPushButton(label)
        if primary:
            button.setObjectName("primary")
        button.clicked.connect(handler)
        return button

    # ---------------------------------------------------------------- search
    def _search_now(self) -> None:
        """Field cleared/Enter pressed: query immediately."""
        self._search_timer.stop()
        self.refresh()

    def _apply_header_sort(self, sort_by: str, sort_dir: str) -> None:
        """Re-query with the clicked column's whitelisted sort key."""
        self._sort_by = sort_by
        self._sort_dir = sort_dir
        self.refresh()

    # ---------------------------------------------------------------- helpers
    def _selected_ids(self) -> list[int]:
        ids: list[int] = []
        for model_index in sorted(self._view.selectionModel().selectedRows()):
            record = self._model.row_id(model_index.row())
            if record is not None:
                ids.append(record)
        return ids

    def _guarded(self, label: str, action: Any, download_id: int) -> None:
        try:
            action(download_id)
        except (ValueError, RuntimeError) as exc:
            QMessageBox.warning(self, label, str(exc))

    # ---------------------------------------------------------------- actions
    def _retry_selected(self) -> None:
        """Retry selected rows that are in the FAILED state."""
        ids = self._selected_ids()
        if not ids:
            QMessageBox.information(self, "Retry", "Select one or more history rows first to retry.")
            return
        failed: list[int] = []
        for download_id in ids:
            record = self._manager.get(download_id)
            if record is not None and record.state is DownloadState.FAILED:
                failed.append(download_id)
        if not failed:
            QMessageBox.information(
                self, "Retry",
                "Retry applies to failed downloads only. Select FAILED rows and try again.",
            )
            return
        for download_id in failed:
            self._guarded("Retry", self._manager.retry, download_id)
        self.refresh()

    def _open_destination(self) -> None:
        """Open the selected row's file, or its folder when the file is gone."""
        ids = self._selected_ids()
        if not ids:
            QMessageBox.information(
                self, "Open destination", "Select one or more history rows first."
            )
            return
        record = self._manager.get(ids[0])
        if record is None or not record.destination_path:
            QMessageBox.information(
                self, "Open destination", "This record has no destination path anymore."
            )
            return
        destination = pathlib.Path(record.destination_path)
        target = destination if destination.exists() else destination.parent
        QDesktopServices.openUrl(QUrl.fromLocalFile(str(target)))

    def _clear_history(self) -> None:
        """Confirm, then clear rows (and optionally files) off the GUI thread."""
        dialog = ConfirmDialog(
            self,
            title="Clear history",
            message=(
                "Delete ALL completed, failed, cancelled and interrupted records "
                "from the history?"
            ),
            checkbox_text="Also delete downloaded files from disk",
            checkbox_checked=False,  # safer default: keep files
            accept_label="Clear history",
        )
        if dialog.exec() != ConfirmDialog.DialogCode.Accepted:
            return
        keep_files = not dialog.checkbox_checked

        self._clear_btn.setEnabled(False)
        self._clear_runner = FunctionRunner(
            lambda: self._repo.clear_history(keep_files=keep_files),
            description="Clear history",
            parent=self,
        )
        self._clear_runner.succeeded.connect(self._on_history_cleared)
        self._clear_runner.failed.connect(self._on_clear_failed)
        self._clear_runner.start()

    def _on_history_cleared(self, removed: object) -> None:
        self._clear_btn.setEnabled(True)
        self._clear_runner = None
        self.refresh()
        QMessageBox.information(self, "Clear history", f"Removed {int(removed)} record(s).")

    def _on_clear_failed(self, message: str) -> None:
        self._clear_btn.setEnabled(True)
        self._clear_runner = None
        QMessageBox.critical(self, "Clear history", message)

    # -------------------------------------------------------------- refresh
    def refresh(self) -> None:
        """Re-issue the current search/filter/sort query against the repository."""
        try:
            records = self._repo.history(
                state=self._state_combo.currentData(),
                search=self._search_edit.text().strip() or None,
                sort_by=self._sort_by,
                sort_dir=self._sort_dir,
                limit=_HISTORY_LIMIT,
            )
        except Exception as exc:  # pragma: no cover - DB hiccups must not crash
            QMessageBox.critical(self, "History", f"failed to load history: {exc}")
            return
        self._model.set_records(records)
        has_rows = self._model.rowCount() > 0
        self._content.setCurrentIndex(1 if has_rows else 0)


def _short_date(iso: str) -> str:
    """``2026-10-24T12:34:56+00:00`` → ``2026-10-24 12:34:56``; ``—`` when blank."""
    if not iso:
        return "—"
    return iso[:19].replace("T", " ")
