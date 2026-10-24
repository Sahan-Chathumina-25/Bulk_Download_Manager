"""Dashboard page: aggregate stat cards + recent activity feed."""

from __future__ import annotations

from collections import deque
from datetime import UTC, datetime
from typing import TYPE_CHECKING

from PySide6.QtCore import Qt, QTimer
from PySide6.QtWidgets import (
    QAbstractItemView,
    QFrame,
    QGridLayout,
    QHBoxLayout,
    QHeaderView,
    QLabel,
    QTableWidget,
    QTableWidgetItem,
    QVBoxLayout,
    QWidget,
)

from src.app.utils_format import format_bytes, format_speed
from src.database.models import DownloadRecord, DownloadState

if TYPE_CHECKING:  # pragma: no cover - typing only
    from PySide6.QtCore import QObject

    from src.app.main_window import CoreSignals
    from src.core.download_manager import DownloadManager
    from src.database.repository import DownloadRepository

__all__ = ["DashboardPage"]

_ACTIVITY_LIMIT = 10


class _StatCard(QFrame):
    """One rounded card with a muted label and a big value."""

    def __init__(self, label: str, value: str = "0", parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("card")
        layout = QVBoxLayout(self)
        layout.setContentsMargins(14, 10, 14, 10)
        self._label = QLabel(label)
        self._label.setObjectName("cardLabel")
        self._value = QLabel(value)
        self._value.setObjectName("cardValue")
        layout.addWidget(self._label)
        layout.addWidget(self._value)

    def set_value(self, value: str) -> None:
        """Update the big value label."""
        self._value.setText(value)


class DashboardPage(QWidget):
    """Stat cards (total/active/completed/failed/bytes/speed) + activity table."""

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
        self._activity: deque[tuple[str, str, str]] = deque(maxlen=_ACTIVITY_LIMIT)

        root = QVBoxLayout(self)
        root.setContentsMargins(24, 20, 24, 20)
        root.setSpacing(14)

        heading = QLabel("Dashboard")
        heading.setObjectName("heading")
        root.addWidget(heading)

        cards = QGridLayout()
        cards.setSpacing(10)
        self._total_card = _StatCard("Total downloads")
        self._active_card = _StatCard("Active")
        self._completed_card = _StatCard("Completed")
        self._failed_card = _StatCard("Failed")
        self._bytes_card = _StatCard("Total downloaded")
        self._speed_card = _StatCard("Aggregate speed")
        for column, card in enumerate(
            (self._total_card, self._active_card, self._completed_card,
             self._failed_card, self._bytes_card, self._speed_card)
        ):
            cards.addWidget(card, 0, column)
        root.addLayout(cards)

        self._empty_activity = QLabel("No download activity yet — add something via the Add Downloads page.")
        self._empty_activity.setObjectName("muted")
        self._empty_activity.setWordWrap(True)

        self._table = QTableWidget(0, 4)
        self._table.setHorizontalHeaderLabels(["Time (UTC)", "State", "Download", "Error"])
        self._table.horizontalHeader().setSectionResizeMode(3, QHeaderView.ResizeMode.Stretch)
        self._table.horizontalHeader().setSectionResizeMode(2, QHeaderView.ResizeMode.ResizeToContents)
        self._table.verticalHeader().setVisible(False)
        self._table.setEditTriggers(QAbstractItemView.EditTrigger.NoEditTriggers)
        self._table.setSelectionBehavior(QAbstractItemView.SelectionBehavior.SelectRows)
        self._table.setShowGrid(False)
        self._table.setAlternatingRowColors(True)
        header = self._table.horizontalHeader()
        header.setSectionResizeMode(0, QHeaderView.ResizeMode.ResizeToContents)
        header.setSectionResizeMode(1, QHeaderView.ResizeMode.ResizeToContents)
        header.setSectionResizeMode(2, QHeaderView.ResizeMode.Stretch)
        header.setStretchLastSection(True)

        self._activity_title = QLabel("Recent activity")
        self._activity_title.setObjectName("muted")
        root.addWidget(self._activity_title)
        root.addWidget(self._table, 1)
        root.addWidget(self._empty_activity)


        relay.stateChangedEvent.connect(self._on_state_change)
        relay.progressEvent.connect(lambda _p: self._on_speed_tick())

        self._timer = QTimer(self)
        self._timer.setInterval(1500)
        self._timer.timeout.connect(lambda: self.refresh())
        self._timer.start()

        self.refresh()

    def _on_state_change(self, record: object) -> None:
        """Append a relayed state change to the activity feed."""
        self.add_activity(record)  # type: ignore[arg-type]

    def add_activity(self, record: DownloadRecord) -> None:
        """Record one state change (called via the relay, queued thread-safe)."""
        self._activity.append(
            (
                datetime.now(tz=UTC).strftime("%H:%M:%S"),
                record.state.value,
                record.display_name or str(record.id),
                record.error_summary or "",
            )
        )
        self._render_activity()

    def _on_speed_tick(self) -> None:
        """Cheap speed-only update between full refreshes."""
        self._speed_card.set_value(format_speed(self._manager.total_speed_bps()))

    def refresh(self) -> None:
        """Re-count states from the repository and repaint cards/table."""
        records = self._repo.list_all()
        counts = {"total": len(records), "active": 0, "completed": 0, "failed": 0, "bytes": 0}
        for record in records:
            if record.state is DownloadState.COMPLETED:
                counts["completed"] += 1
                counts["bytes"] += record.downloaded_bytes
            elif record.state is DownloadState.FAILED:
                counts["failed"] += 1
            if record.state in (
                DownloadState.QUEUED,
                DownloadState.CONNECTING,
                DownloadState.DOWNLOADING,
                DownloadState.PAUSED,
                DownloadState.INTERRUPTED,
            ):
                counts["active"] += 1
        self._total_card.set_value(f"{counts['total']:,}")
        self._active_card.set_value(str(counts["active"]))
        self._completed_card.set_value(str(counts["completed"]))
        self._failed_card.set_value(str(counts["failed"]))
        self._bytes_card.set_value(format_bytes(counts["bytes"]))
        self._speed_card.set_value(format_speed(self._manager.total_speed_bps()))
        self._render_activity()

    def _render_activity(self) -> None:
        """Fill the last-N table and toggle the friendly empty state."""
        rows = list(self._activity)
        self._table.setRowCount(len(rows))
        for row_index, (when, state, name, error) in enumerate(rows):
            self._table.setItem(row_index, 0, QTableWidgetItem(when))
            self._table.setItem(row_index, 1, QTableWidgetItem(state))
            self._table.setItem(row_index, 2, QTableWidgetItem(name))
            error_item = QTableWidgetItem(error)
            if error:
                error_item.setToolTip(error)
            self._table.setItem(row_index, 3, error_item)
        for row_index, record in enumerate(rows):
            for column in range(4):
                self._table.item(row_index, column).setFlags(
                    Qt.ItemFlag.ItemIsEnabled | Qt.ItemFlag.ItemIsSelectable
                )
        has_activity = bool(rows)
        self._table.setVisible(has_activity)
        self._activity_title.setVisible(has_activity)
        self._empty_activity.setVisible(not has_activity)
