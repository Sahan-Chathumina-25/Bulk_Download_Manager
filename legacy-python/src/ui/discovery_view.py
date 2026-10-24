"""Discover page: scan a page URL for file candidates, select, queue them.

The scan runs in a :class:`DiscoveryWorker` QThread — never the GUI thread.
Cancel is cooperative via a ``threading.Event`` passed to the core scanner.
"""

from __future__ import annotations

import dataclasses
import logging
from pathlib import Path
from typing import TYPE_CHECKING, Any

from PySide6.QtCore import QAbstractTableModel, QModelIndex, QSortFilterProxyModel, Qt, Signal
from PySide6.QtWidgets import (
    QAbstractItemView,
    QCheckBox,
    QGridLayout,
    QHBoxLayout,
    QHeaderView,
    QLabel,
    QLineEdit,
    QMessageBox,
    QProgressBar,
    QPushButton,
    QTableView,
    QVBoxLayout,
    QWidget,
)

from src.app.dialogs import ImportSummaryDialog, confirm, pick_folder
from src.app.worker_threads import DiscoveryWorker
from src.core.link_discovery import MAX_CANDIDATES, DiscoveryCandidate, DiscoveryResult
from src.core.url_validator import ParseResult
from src.ui.discovery_queue_planner import (
    checked_candidates,
    execute_queue_plan,
    plan_discovery_queue,
)

if TYPE_CHECKING:  # pragma: no cover - typing only
    from PySide6.QtCore import QObject

    from src.app.main_window import CoreSignals
    from src.core.download_manager import DownloadManager
    from src.database.models import Settings
    from src.database.repository import DownloadRepository

__all__ = ["CandidatesTableModel", "DiscoveryPage"]

log = logging.getLogger(__name__)

_INCLUDE_COL = 0
_FILENAME_COL = 1
_TYPE_COL = 2
_SIZE_COL = 3
_SOURCE_COL = 4
_URL_COL = 5


class CandidatesTableModel(QAbstractTableModel):
    """Rows of :class:`DiscoveryCandidate` with an include checkbox column."""

    _HEADERS = ("Include", "Filename", "Type", "Size", "Source page", "URL")

    def __init__(self, parent: Any = None) -> None:
        super().__init__(parent)
        self._rows: list[DiscoveryCandidate] = []

    def rowCount(self, parent: QModelIndex = QModelIndex()) -> int:  # noqa: N802
        return 0 if parent.isValid() else len(self._rows)

    def columnCount(self, parent: QModelIndex = QModelIndex()) -> int:  # noqa: N802
        return 0 if parent.isValid() else len(self._HEADERS)

    def headerData(self, section: int, orientation: Qt.Orientation, role: int = Qt.ItemDataRole.DisplayRole) -> Any:  # noqa: N802
        if orientation == Qt.Orientation.Horizontal and role == Qt.ItemDataRole.DisplayRole:
            return self._HEADERS[section]
        return None

    def flags(self, index: QModelIndex) -> Qt.ItemFlag:
        base = Qt.ItemFlag.ItemIsEnabled | Qt.ItemFlag.ItemIsSelectable
        if index.column() == _INCLUDE_COL:
            return base | Qt.ItemFlag.ItemIsUserCheckable
        return base

    def data(self, index: QModelIndex, role: int = Qt.ItemDataRole.DisplayRole) -> Any:
        if not index.isValid():
            return None
        candidate = self._rows[index.row()]
        if role == Qt.ItemDataRole.CheckStateRole and index.column() == _INCLUDE_COL:
            return (
                Qt.CheckState.Checked if candidate.is_likely_file else Qt.CheckState.Unchecked
            )
        if role == Qt.ItemDataRole.ToolTipRole:
            return candidate.candidate_url
        if role != Qt.ItemDataRole.DisplayRole:
            return None
        from src.app.utils_format import format_bytes

        column = index.column()
        if column == _FILENAME_COL:
            return candidate.filename
        if column == _TYPE_COL:
            return candidate.content_type or (f".{candidate.extension}" if candidate.extension else "unknown")
        if column == _SIZE_COL:
            return format_bytes(candidate.size)
        if column == _SOURCE_COL:
            return candidate.source_page_url
        if column == _URL_COL:
            return candidate.candidate_url
        return None

    def setData(self, index: QModelIndex, value: Any, role: int = Qt.ItemDataRole.EditRole) -> bool:  # noqa: N802
        if role == Qt.ItemDataRole.CheckStateRole and index.column() == _INCLUDE_COL and index.isValid():
            candidate = self._rows[index.row()]
            if isinstance(value, bool):
                checked = value
            else:
                try:
                    checked = Qt.CheckState(value) == Qt.CheckState.Checked
                except (TypeError, ValueError):
                    log.warning("unexpected check-state value %r; treating as unchecked", value)
                    checked = False
            candidate.is_likely_file = checked
            self.dataChanged.emit(index, index)
            return True
        return False

    def set_candidates(self, candidates: list[DiscoveryCandidate]) -> None:
        self.beginResetModel()
        self._rows = list(candidates)
        self.endResetModel()

    @property
    def candidates(self) -> list[DiscoveryCandidate]:
        """Every row currently in the model (checked and unchecked alike)."""
        return list(self._rows)

    def selected(self) -> list[DiscoveryCandidate]:
        return [row for row in self._rows if row.is_likely_file]

    def set_all_selected(self, selected: bool) -> None:
        for row in self._rows:
            row.is_likely_file = selected
        if self._rows:
            self.dataChanged.emit(
                self.index(0, _INCLUDE_COL), self.index(len(self._rows) - 1, _INCLUDE_COL)
            )


class CandidatesFilterModel(QSortFilterProxyModel):
    """Search filter over filename/URL/type + optional hide-unknown toggle."""

    def __init__(self, parent: Any = None) -> None:
        super().__init__(parent)
        self._search = ""
        self._include_unknown = True

    def set_search(self, text: str) -> None:
        self._search = text.strip().lower()
        self.invalidateFilter()

    def set_include_unknown(self, include: bool) -> None:
        self._include_unknown = include
        self.invalidateFilter()

    def filterAcceptsRow(self, source_row: int, source_parent: QModelIndex) -> bool:  # noqa: N802
        model = self.sourceModel()
        assert isinstance(model, CandidatesTableModel)
        candidate = model._rows[source_row]  # noqa: SLF001 - same-module model
        if not self._include_unknown and candidate.extension is None:
            return False
        if not self._search:
            return True
        haystack = " ".join(
            (candidate.filename, candidate.candidate_url, candidate.extension or "")
        ).lower()
        return self._search in haystack


class DiscoveryPage(QWidget):
    """Scan-one-page flow with busy indicator, cancel and add-selected."""

    scanStarted = Signal()
    scanFinished = Signal()
    queuedSome = Signal(int)

    def __init__(
        self,
        manager: "DownloadManager",
        repository: "DownloadRepository",
        relay: "CoreSignals",
        settings_provider: "QObject",
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self._manager = manager
        self._repo = repository
        self._settings_provider = settings_provider.current_settings
        self._worker: DiscoveryWorker | None = None
        self._busy = False
        self._inserting = False

        root = QVBoxLayout(self)
        root.setContentsMargins(24, 20, 24, 20)
        root.setSpacing(12)
        heading = QLabel("Discover")
        heading.setObjectName("heading")
        intro = QLabel(
            "Scan a web page for downloadable files. The scanner fetches only the HTML\n"
            "document (plus bounded HEAD metadata calls) — nothing is downloaded until you queue it."
        )
        intro.setObjectName("muted")
        root.addWidget(heading)
        root.addWidget(intro)

        controls = QGridLayout()
        self._url_edit = QLineEdit()
        self._url_edit.setPlaceholderText("https://example.com/downloads.html")
        self._scan_btn = self._button("Scan", self._start_scan, primary=True)
        self._cancel_btn = self._button("Cancel scan", self._cancel_scan)
        self._cancel_btn.setEnabled(False)
        controls.addWidget(QLabel("Page URL"), 0, 0)
        controls.addWidget(self._url_edit, 0, 1)
        controls.addWidget(self._scan_btn, 0, 2)
        controls.addWidget(self._cancel_btn, 0, 3)

        self._progress = QProgressBar()
        self._progress.setRange(0, 0)  # indeterminate
        self._progress.setVisible(False)
        self._progress.setFixedHeight(14)
        controls.addWidget(self._progress, 1, 1, 1, 3)
        root.addLayout(controls)

        filters = QHBoxLayout()
        filters.addWidget(QLabel("Filter"))
        self._search_edit = QLineEdit()
        self._search_edit.setPlaceholderText("filename, extension or URL contains…")
        self._search_edit.textChanged.connect(self._apply_filter)
        filters.addWidget(self._search_edit, 1)
        self._unknown_check = QCheckBox("Include unknown types")
        self._unknown_check.setChecked(True)
        self._unknown_check.setToolTip(
            "When unchecked, links without a recognised file extension are hidden."
        )
        self._unknown_check.toggled.connect(self._apply_unknown)
        filters.addWidget(self._unknown_check)
        root.addLayout(filters)

        self._model = CandidatesTableModel(self)
        self._proxy = CandidatesFilterModel(self)
        self._proxy.setSourceModel(self._model)
        self._view = QTableView()
        self._view.setModel(self._proxy)
        self._view.setSelectionBehavior(QAbstractItemView.SelectionBehavior.SelectRows)
        self._view.setEditTriggers(QAbstractItemView.EditTrigger.NoEditTriggers)
        self._view.setAlternatingRowColors(True)
        self._view.setShowGrid(False)
        self._view.verticalHeader().setVisible(False)
        header = self._view.horizontalHeader()
        header.setStretchLastSection(True)
        self._view.horizontalHeader().setSectionResizeMode(_FILENAME_COL, QHeaderView.ResizeMode.Stretch)
        self._view.setColumnWidth(_SIZE_COL, 110)
        self._view.setColumnWidth(_TYPE_COL, 120)
        root.addWidget(self._view, 1)

        self._status = QLabel("Paste a page URL above and press Scan.")
        self._status.setObjectName("muted")
        root.addWidget(self._status)

        selection_buttons = QHBoxLayout()
        self._select_all_btn = self._button("Select All", lambda: self._model.set_all_selected(True))
        self._deselect_btn = self._button("Deselect All", lambda: self._model.set_all_selected(False))
        selection_buttons.addWidget(self._select_all_btn)
        selection_buttons.addWidget(self._deselect_btn)
        selection_buttons.addStretch(1)
        self._add_selected_btn = self._button("Add Selected to Queue", self._add_selected, primary=True)
        self._add_selected_btn.setToolTip(
            "Queue every row whose 'Include' checkbox (first column) is ticked.\n"
            "If no destination folder is configured yet you will be asked to pick one."
        )
        selection_buttons.addWidget(self._add_selected_btn)
        root.addLayout(selection_buttons)

    def _button(self, label: str, handler: object, *, primary: bool = False) -> QPushButton:
        button = QPushButton(label)
        if primary:
            button.setObjectName("primary")
        button.clicked.connect(handler)  # type: ignore[arg-type]
        return button

    # ------------------------------------------------------------------ filter
    def _apply_filter(self, text: str) -> None:
        self._proxy.set_search(text)

    def _apply_unknown(self, checked: bool) -> None:
        self._proxy.set_include_unknown(checked)

    # -------------------------------------------------------------------- scan
    def _apply_add_selected_enabled(self) -> None:
        """Enable *Add Selected* only when idle and at least one row exists.

        The destination folder is deliberately NOT required here: fresh
        installs ship with an empty ``default_destination_dir`` and the
        button used to be permanently disabled as a result. The slot
        resolves (and offers to persist / create) the folder lazily.
        """
        self._add_selected_btn.setEnabled(
            not self._busy and not self._inserting and self._model.rowCount() > 0
        )

    def refresh(self) -> None:
        """Sync the unknown-types default from settings when settings change."""
        if not self._model.rowCount():
            self._unknown_check.blockSignals(True)
            self._unknown_check.setChecked(True)
            self._unknown_check.blockSignals(False)
        self._apply_add_selected_enabled()

    def _start_scan(self) -> None:
        if self._worker is not None and self._worker.isRunning():
            return
        page_url = self._url_edit.text().strip()
        if not page_url:
            QMessageBox.information(self, "Discover", "Enter a page URL (http/https) to scan first.")
            return
        settings = self._settings_provider()
        self._model.set_candidates([])
        self._set_busy(True)
        self._status.setText(f"Scanning {page_url} … (robots.txt {'respected' if settings.respect_rate_limit else 'ignored © settings'})")

        worker = DiscoveryWorker(page_url, settings, parent=self)
        worker.finished_scan.connect(self._on_scan_finished)
        worker.failed.connect(self._on_scan_failed)
        self._worker = worker
        worker.start()

    def _cancel_scan(self) -> None:
        if self._worker is not None and self._worker.isRunning():
            self._worker.cancel()
            self._status.setText("Cancelling scan…")

    def _set_busy(self, busy: bool) -> None:
        self._busy = busy
        self._progress.setVisible(busy)
        self._scan_btn.setEnabled(not busy)
        self._cancel_btn.setEnabled(busy)
        self._apply_add_selected_enabled()

    def _on_scan_finished(self, result: object) -> None:
        assert isinstance(result, DiscoveryResult)
        self._set_busy(False)
        self._worker = None
        candidates = list(result.candidates)
        self._model.set_candidates(candidates)
        if result.errors:
            QMessageBox.warning(
                self, "Scan finished with problems",
                "The scan returned no further results for these problems:\n\n"
                + "\n".join(f"• {error}" for error in result.errors),
            )
        likely = sum(1 for c in candidates if c.is_likely_file)
        self._status.setText(
            f"Found {len(candidates)} link(s) ({likely} likely file(s), cap {MAX_CANDIDATES}) "
            f"in {result.elapsed_seconds:.1f}s — tick rows then “Add Selected to Queue”."
        )
        if not candidates and not result.errors:
            self._status.setText(
                "No downloadable links found on that page. Try another page, or relax "
                "Settings → Include unknown types."
            )
        self.scanFinished.emit()

    def _on_scan_failed(self, message: str) -> None:
        self._set_busy(False)
        self._worker = None
        self._status.setText("Scan failed — see the message dialog for details.")
        QMessageBox.critical(self, "Scan error", message)
        self.scanFinished.emit()

    # ------------------------------------------------------------------ queue
    def _add_selected(self) -> None:
        """Queue every ticked candidate; destination resolved lazily.

        The whole flow is guarded so an unexpected error can never wedge the
        page: ``_inserting`` is always cleared in ``finally`` and the button
        re-enabled via :meth:`_apply_add_selected_enabled`.
        """
        if self._inserting:
            return
        try:
            self._inserting = True
            self._add_selected_btn.setEnabled(False)
            self._queue_checked_candidates()
        except Exception as exc:  # noqa: BLE001 - the batch must never crash the page
            log.exception("queueing discovery selection failed")
            QMessageBox.critical(
                self, "Add to queue", f"Could not queue the selection:\n{exc}"
            )
        finally:
            self._inserting = False
            self._apply_add_selected_enabled()

    def _queue_checked_candidates(self) -> None:
        """Plan and execute the batch for the currently ticked rows."""
        checked = checked_candidates(self._model.candidates)
        if not checked:
            QMessageBox.information(
                self, "Add to queue",
                "No files selected — tick the 'Include' checkbox in the first "
                "column for the files you want to download.",
            )
            return

        destination = self._resolve_destination()
        if destination is None:
            return

        settings: Settings = self._settings_provider()
        plan = plan_discovery_queue(
            checked,
            existing_urls={record.source_url for record in self._repo.list_all()},
            allow_private=settings.allow_private_addresses,
        )
        report = execute_queue_plan(
            plan, destination, self._manager.add_download,
            start_fn=self._manager.start_all,
        )

        result = ParseResult(
            accepted=[record.source_url for record in report.added],
            duplicates=[f"{label}: {reason}" for label, reason in plan.duplicates],
            rejected=[(label, reason) for label, reason in plan.rejected]
            + [(label, reason) for label, reason in report.failed],
        )
        ImportSummaryDialog(self, result=result, title="Discover — queue summary").exec()

        queued, skipped = len(report.added), len(result.duplicates) + len(result.rejected)
        if queued:
            self._status.setText(
                f"Queued {queued} new download(s) into {destination}; {skipped} skipped."
            )
        else:
            self._status.setText(
                f"Nothing new queued — {skipped} duplicate(s)/rejected; see the summary."
            )
        self.queuedSome.emit(queued)

    def _resolve_destination(self) -> str | None:
        """A usable destination folder, asked for / created lazily.

        Returns ``None`` when the flow must abort (the user cancelled the
        picker, or declined to create a missing folder and picked nothing).
        """
        picked = self._pick_destination(self._settings_provider())
        if picked is None:
            return None
        destination = Path(picked)
        if destination.exists():
            return picked
        if confirm(
            self, "Add to queue",
            f"The destination folder\n{picked}\ndoes not exist yet. Create it?",
        ):
            try:
                destination.mkdir(parents=True, exist_ok=True)
                return picked
            except OSError as exc:
                log.warning("could not create destination %s: %s", picked, exc)
        # declined / mkdir failed: offer one more picker, empty means abort
        chosen = pick_folder(self, "", "Choose where to save these downloads")
        return chosen or None

    def _pick_destination(self, settings: "Settings") -> str | None:
        """The configured destination, or persist one the user just picked."""
        current = settings.default_destination_dir.strip()
        if current:
            return current
        chosen = pick_folder(self, "", "Choose where to save these downloads")
        if not chosen:
            return None
        new_settings = dataclasses.replace(
            settings, default_destination_dir=chosen
        )
        try:
            self._repo.save_settings(new_settings)
        except Exception as exc:  # noqa: BLE001 - sqlite errors must not sink the batch
            log.warning("could not persist default destination: %s", exc)
        try:
            self._settings_provider.__self__.apply_settings(new_settings)  # type: ignore[attr-defined,union-attr]
        except AttributeError:
            log.debug("settings provider does not expose apply_settings; skipped live apply")
        return chosen
