"""Add Downloads page: paste/import URLs and queue them with one click."""

from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING

from PySide6.QtCore import Signal
from PySide6.QtWidgets import (
    QCheckBox,
    QFileDialog,
    QFormLayout,
    QGroupBox,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QMessageBox,
    QPushButton,
    QPlainTextEdit,
    QVBoxLayout,
    QWidget,
)

from src.app.dialogs import ImportSummaryDialog, pick_folder
from src.core.url_validator import parse_url_lines

if TYPE_CHECKING:  # pragma: no cover - typing only
    from PySide6.QtCore import QObject

    from src.app.main_window import CoreSignals
    from src.core.download_manager import DownloadManager
    from src.database.repository import DownloadRepository

__all__ = ["AddDownloadsPage"]


class AddDownloadsPage(QWidget):
    """Multiline URL intake with import summary, destination and SHA-256 field."""

    #: Emitted after at least one download was queued (other pages refresh).
    downloadsQueued = Signal(int)

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

        root = QVBoxLayout(self)
        root.setContentsMargins(24, 20, 24, 20)
        root.setSpacing(12)
        heading = QLabel("Add Downloads")
        heading.setObjectName("heading")
        root.addWidget(heading)

        hint = QLabel(
            "Paste one URL per line (CRLF or LF; a URL may also be a tab-separated\n"
            "last column of an imported TXT). Blank lines are ignored, exact duplicates\n"
            "are reported, unsafe (non-http/private) URLs are rejected with a reason."
        )
        hint.setObjectName("muted")
        root.addWidget(hint)

        self._urls_edit = QPlainTextEdit()
        self._urls_edit.setPlaceholderText("https://example.com/file1.zip\nhttps://example.com/file2.iso")
        self._urls_edit.setMinimumHeight(140)
        root.addWidget(self._urls_edit, 1)

        destination_group = QGroupBox("Destination")
        form = QFormLayout(destination_group)
        directory_row = QHBoxLayout()
        self._destination_edit = QLineEdit()
        self._destination_edit.setPlaceholderText(str(Path.home() / "Downloads"))
        browse = QPushButton("Browse…")
        browse.clicked.connect(self._browse_destination)
        directory_row.addWidget(self._destination_edit, 1)
        directory_row.addWidget(browse)
        form.addRow(QLabel("Destination folder"), directory_row)

        self._filename_edit = QLineEdit()
        self._filename_edit.setPlaceholderText("only used when exactly one URL is added")
        self._filename_edit.setEnabled(False)
        self._urls_edit.textChanged.connect(self._update_filename_enabled)
        form.addRow(QLabel("Custom filename"), self._filename_edit)

        self._checksum_edit = QLineEdit()
        self._checksum_edit.setPlaceholderText(
            "optional SHA-256 digest (bare hex or sha256:…) — verified after download"
        )
        self._checksum_edit.setToolTip(
            "64-character hex digest. Enabled only when exactly one URL is added."
        )
        self._checksum_edit.setEnabled(False)
        self._urls_edit.textChanged.connect(self._update_checksum_enabled)
        form.addRow(QLabel("SHA-256"), self._checksum_edit)

        root.addWidget(destination_group)
        root.addSpacing(4)

        self._start_now = QCheckBox("Start downloads immediately after adding")
        self._start_now.setChecked(True)
        root.addWidget(self._start_now)

        buttons = QHBoxLayout()
        self._add_btn = self._button("Add URLs", self._on_add_urls, primary=True)
        self._import_btn = self._button("Import TXT…", self._on_import_txt)
        self._clear_btn = self._button("Clear", self._on_clear)
        buttons.addWidget(self._add_btn)
        buttons.addWidget(self._import_btn)
        buttons.addWidget(self._clear_btn)
        buttons.addStretch(1)
        root.addLayout(buttons)

    def _button(self, label: str, handler: object, *, primary: bool = False) -> QPushButton:
        button = QPushButton(label)
        if primary:
            button.setObjectName("primary")
        button.clicked.connect(handler)  # type: ignore[arg-type]
        return button

    # ------------------------------------------------------------- destination
    def _browse_destination(self) -> None:
        chosen = pick_folder(self, self._destination_edit.text().strip() or str(Path.home()), "Select destination folder")
        if chosen:
            self._destination_edit.setText(chosen)

    def refresh(self) -> None:
        """Show the settings default destination when the field is untouched."""
        settings = self._settings_provider()
        default_dest = settings.default_destination_dir or str(Path.home() / "Downloads")
        self._destination_edit.setPlaceholderText(default_dest)

    # ------------------------------------------------------------- validation
    def _destination(self) -> str:
        return self._destination_edit.text().strip()

    def _update_filename_enabled(self) -> None:
        """Custom filename applies only for a single URL."""
        single = len(self._collect_text_lines()) == 1
        self._filename_edit.setEnabled(single)
        self._checksum_edit.setEnabled(single)
        if not single:
            self._filename_edit.clear()
            self._checksum_edit.clear()

    def _update_checksum_enabled(self) -> None:
        self._update_filename_enabled()

    def _collect_text_lines(self) -> list[str]:
        text = self._urls_edit.toPlainText()
        return [line for line in text.replace("\r\n", "\n").split("\n") if line.strip()]

    # ----------------------------------------------------------------- actions
    def _on_import_txt(self) -> None:
        """Read a UTF-8 text file (CRLF/LF tolerant) into the URL box."""
        path, _filter = QFileDialog.getOpenFileName(self, "Import URL list", "", "Text files (*.txt);;All files (*)")
        if not path:
            return
        try:
            raw = Path(path).read_text(encoding="utf-8", errors="replace")
        except OSError as exc:
            QMessageBox.critical(self, "Import TXT", f"Could not read {path}:\n{exc}")
            return
        if self._urls_edit.toPlainText().strip():
            text = self._urls_edit.toPlainText().rstrip() + "\n" + raw
            self._urls_edit.setPlainText(text)
        else:
            self._urls_edit.setPlainText(raw)

    def _on_clear(self) -> None:
        self._urls_edit.clear()
        self._filename_edit.clear()
        self._checksum_edit.clear()

    def _on_add_urls(self) -> None:
        settings = self._settings_provider()
        parse = parse_url_lines(self._urls_edit.toPlainText(), allow_private=settings.allow_private_addresses)
        if not parse.accepted and not parse.duplicates and not parse.rejected:
            QMessageBox.information(self, "Add URLs", "Nothing to add — enter at least one http/https URL per line.")
            return

        destination = self._destination()
        if not destination:
            destination = settings.default_destination_dir
        if not destination and parse.accepted:
            QMessageBox.warning(
                self, "Add URLs",
                "No destination folder set. Set one here (or in Settings → Destination folder).",
            )
            return

        custom_filename = self._filename_edit.text().strip() or None
        checksum = self._checksum_edit.text().strip() or None
        if len(parse.accepted) > 1:
            # a custom name/checksum only makes sense for exactly one URL
            custom_filename = None
            checksum = None

        queued = 0
        errors: list[tuple[str, str]] = []
        for url in parse.accepted:
            try:
                self._manager.add_download(
                    url,
                    destination,
                    filename=custom_filename,
                    checksum=checksum,
                )
                queued += 1
            except (ValueError, RuntimeError) as exc:
                errors.append((url, str(exc)))
                QMessageBox.critical(self, "Unsafe URL", f"{url}\n\n{exc}")

        if queued and self._start_now.isChecked():
            self._manager.start_all()

        summary = parse
        summary.accepted = [url for url in parse.accepted if url not in {u for u, _ in errors}]
        summary.rejected = list(summary.rejected) + errors
        ImportSummaryDialog(self, result=summary, title="Add URLs — import summary").exec()
        self.downloadsQueued.emit(queued)
        self.refresh()
