"""Reusable dialogs: import summary, record details, confirm + folder helpers."""

from __future__ import annotations

from PySide6.QtCore import Qt
from PySide6.QtWidgets import (
    QCheckBox,
    QDialog,
    QDialogButtonBox,
    QFileDialog,
    QGridLayout,
    QLabel,
    QMessageBox,
    QScrollArea,
    QTableWidget,
    QTableWidgetItem,
    QVBoxLayout,
    QWidget,
)

from src.core.url_validator import ParseResult
from src.database.models import DownloadRecord, DownloadState

__all__ = [
    "ConfirmDialog",
    "DetailsDialog",
    "ImportSummaryDialog",
    "confirm",
    "pick_folder",
]


def pick_folder(parent: QWidget | None, start: str = "", title: str = "Choose a folder") -> str:
    """Open a native directory picker; returns the chosen path or ``""``."""
    return QFileDialog.getExistingDirectory(parent, title, start)


def confirm(parent: QWidget | None, title: str, text: str) -> bool:
    """Yes/no question box; ``True`` when the user accepted."""
    answer = QMessageBox.question(
        parent, title, text, QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
        QMessageBox.StandardButton.No,
    )
    return answer == QMessageBox.StandardButton.Yes


class ConfirmDialog(QDialog):
    """Confirmation dialog with an optional trailing checkbox.

    Used for destructive actions such as *Clear History* (the checkbox asks
    whether downloaded files should also be deleted from disk).
    """

    def __init__(
        self,
        parent: QWidget | None = None,
        *,
        title: str,
        message: str,
        checkbox_text: str = "",
        checkbox_checked: bool = False,
        accept_label: str = "Confirm",
    ) -> None:
        super().__init__(parent)
        self.setWindowTitle(title)
        self.setModal(True)
        self.setMinimumWidth(420)

        layout = QVBoxLayout(self)
        message_label = QLabel(message)
        message_label.setWordWrap(True)
        layout.addWidget(message_label)

        self.checkbox: QCheckBox | None = None
        if checkbox_text:
            self.checkbox = QCheckBox(checkbox_text)
            self.checkbox.setChecked(checkbox_checked)
            layout.addWidget(self.checkbox)

        buttons = QDialogButtonBox(QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel)
        buttons.button(QDialogButtonBox.StandardButton.Ok).setText(accept_label)
        buttons.accepted.connect(self.accept)
        buttons.rejected.connect(self.reject)
        layout.addWidget(buttons)

    @property
    def checkbox_checked(self) -> bool:
        """Whether the trailing checkbox was ticked at accept time."""
        return bool(self.checkbox is not None and self.checkbox.isChecked())


class ImportSummaryDialog(QDialog):
    """Inspectable report of a paste/import/add-prints run.

    Shows accepted / duplicate / rejected counts and one row per rejected
    line with its reason. A *Copy details* button puts the whole report on
    the clipboard.
    """

    def __init__(self, parent: QWidget | None = None, *, result: ParseResult, title: str = "Import summary") -> None:
        super().__init__(parent)
        self.setWindowTitle(title)
        self.setModal(True)
        self.setMinimumSize(620, 400)
        self._result = result

        layout = QVBoxLayout(self)

        counts = QLabel(
            f"<b>{len(result.accepted)}</b> accepted &nbsp;·&nbsp; "
            f"<b>{len(result.duplicates)}</b> duplicate(s) &nbsp;·&nbsp; "
            f"<b>{len(result.rejected)}</b> rejected"
        )
        layout.addWidget(counts)

        table = QTableWidget(0, 2)
        table.setHorizontalHeaderLabels(["Entry", "Status / reason"])
        table.horizontalHeader().setStretchLastSection(True)
        table.verticalHeader().setVisible(False)
        table.setEditTriggers(QTableWidget.EditTrigger.NoEditTriggers)
        table.setSelectionBehavior(QTableWidget.SelectionBehavior.SelectRows)
        table.setWordWrap(True)

        for url in result.accepted:
            _append_row(table, url, "accepted")
        for url in result.duplicates:
            _append_row(table, url, "duplicate (already listed)")
        for line, reason in result.rejected:
            _append_row(table, line, reason)

        table.resizeColumnsToContents()
        if table.columnWidth(0) > 380:
            table.setColumnWidth(0, 380)
        layout.addWidget(table, 1)

        buttons = QDialogButtonBox(QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Discard)
        buttons.button(QDialogButtonBox.StandardButton.Ok).setText("Copy details")
        buttons.button(QDialogButtonBox.StandardButton.Discard).setText("Close")
        buttons.accepted.connect(self._copy_report)
        buttons.rejected.connect(self.reject)
        layout.addWidget(buttons)

    def _copy_report(self) -> None:
        """Put a plain-text version of the report on the system clipboard."""
        from PySide6.QtWidgets import QApplication

        r = self._result
        lines = [
            "accepted:",
            *(f"  {url}" for url in r.accepted),
            "duplicates:",
            *(f"  {url}" for url in r.duplicates),
            "rejected:",
            *(f"  {line!r} — {reason}" for line, reason in r.rejected),
        ]
        QApplication.clipboard().setText("\n".join(lines))

    @staticmethod
    def from_results(results: list[tuple[str, ParseResult]], parent: QWidget | None) -> "ImportSummaryDialog | None":
        """Merge several ``ParseResult``s (e.g. multiple add batches)."""
        if not results:
            return None
        if len(results) == 1:
            return ImportSummaryDialog(parent, result=results[0][1])
        import copy

        from src.core.url_validator import ParseResult as _PR

        merged = _PR()
        for _label, r in results:
            merged.accepted.extend(r.accepted)
            merged.duplicates.extend(r.duplicates)
            merged.rejected.extend(r.rejected)
        return ImportSummaryDialog(parent, result=merged)


class DetailsDialog(QDialog):
    """Full record view for one download: URL, paths, error, checksum."""

    def __init__(self, record: DownloadRecord, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setWindowTitle(f"Download details — {record.display_name or record.id}")
        self.setModal(True)
        self.setMinimumWidth(560)

        outer = QVBoxLayout(self)
        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setFrameShape(QScrollArea.Shape.NoFrame)
        inner = QWidget()
        grid = QGridLayout(inner)
        grid.setSpacing(6)

        grid.addWidget(_make_label("State", bold=True), 0, 0)
        grid.addWidget(_make_label(record.state.value), 0, 1)
        grid.addWidget(_make_label("Source URL", bold=True), 1, 0)
        url_label = _make_label(record.source_url)
        url_label.setTextInteractionFlags(
            Qt.TextInteractionFlag.TextSelectableByMouse | Qt.TextInteractionFlag.TextSelectableByKeyboard
        )
        url_label.setWordWrap(True)
        grid.addWidget(url_label, 1, 1)
        grid.addWidget(_make_label("Filename", bold=True), 2, 0)
        grid.addWidget(_make_label(record.display_name), 2, 1)
        grid.addWidget(_make_label("Destination", bold=True), 3, 0)
        dest_label = _make_label(record.destination_path)
        dest_label.setWordWrap(True)
        grid.addWidget(dest_label, 3, 1)
        grid.addWidget(_make_label("Downloaded", bold=True), 4, 0)
        done = DownloadState(record.state)
        grid.addWidget(_make_label(f"{record.downloaded_bytes:,} / {record.expected_size:,} bytes" if record.expected_size else f"{record.downloaded_bytes:,} bytes"), 4, 1)
        grid.addWidget(_make_label("Content type", bold=True), 5, 0)
        grid.addWidget(_make_label(record.content_type or "—"), 5, 1)
        grid.addWidget(_make_label("Checksum", bold=True), 6, 0)
        checksum_text = record.checksum_sha256 or "—"
        if record.checksum_sha256:
            checksum_text += "  ✓ verified" if record.checksum_verified else "  (not verified)"
        grid.addWidget(_make_label(checksum_text), 6, 1)
        grid.addWidget(_make_label("ETag / Last-Modified", bold=True), 7, 0)
        grid.addWidget(_make_label(f"{record.etag or '—'} / {record.last_modified or '—'}"), 7, 1)
        grid.addWidget(_make_label("Created", bold=True), 8, 0)
        grid.addWidget(_make_label(record.created_at or "—"), 8, 1)
        grid.addWidget(_make_label("Completed", bold=True), 9, 0)
        grid.addWidget(_make_label(record.completed_at or "—"), 9, 1)
        grid.addWidget(_make_label("Retry count", bold=True), 10, 0)
        grid.addWidget(_make_label(str(record.retry_count)), 10, 1)
        grid.addWidget(_make_label("Error", bold=True), 11, 0)
        error_label = _make_label(record.error_summary or "—")
        error_label.setWordWrap(True)
        if record.error_summary:
            error_label.setStyleSheet("color: #e5534b;")
        grid.addWidget(error_label, 11, 1)

        grid.setColumnStretch(1, 1)
        scroll.setWidget(inner)
        outer.addWidget(scroll, 1)

        buttons = QDialogButtonBox(QDialogButtonBox.StandardButton.Close)
        buttons.rejected.connect(self.reject)
        buttons.clicked.connect(lambda *_: self.accept())
        outer.addWidget(buttons)


def _make_label(text: str, *, bold: bool = False) -> QLabel:
    label = QLabel(text)
    if bold:
        label.setStyleSheet("font-weight: 600;")
    return label


def _append_row(table: QTableWidget, left: str, right: str) -> None:
    row = table.rowCount()
    table.insertRow(row)
    table.setItem(row, 0, QTableWidgetItem(left))
    table.setItem(row, 1, QTableWidgetItem(right))
    table.item(row, 0).setToolTip(left)
    table.item(row, 1).setToolTip(right)
