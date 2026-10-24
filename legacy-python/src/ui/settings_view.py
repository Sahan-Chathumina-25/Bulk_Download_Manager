"""Settings page: edit every persisted ``Settings`` field and save safely.

On save the widget first builds a fresh :class:`~src.database.models.Settings`
— its validation is the single source of truth, and a ``ValueError`` shows a
warning instead of touching the store. A valid snapshot is persisted via
:meth:`DownloadRepository.save_settings` and then handed to the MainWindow
``apply_settings`` callback so live workers pick it up immediately.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Callable

from PySide6.QtWidgets import (
    QApplication,
    QCheckBox,
    QComboBox,
    QDoubleSpinBox,
    QFormLayout,
    QGroupBox,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QMessageBox,
    QPushButton,
    QScrollArea,
    QSpinBox,
    QVBoxLayout,
    QWidget,
)

from src.app.dialogs import pick_folder
from src.app.theme import apply_theme
from src.database.models import Settings

if TYPE_CHECKING:  # pragma: no cover - typing only
    from src.app.main_window import CoreSignals
    from src.core.download_manager import DownloadManager
    from src.database.repository import DownloadRepository

__all__ = ["SettingsPage"]

#: ``bandwidth_limit_bps`` is stored per byte; the editor works in KB/s.
_KIB = 1024


class SettingsPage(QWidget):
    """Form over all ``Settings`` fields with validated save + live apply."""

    def __init__(
        self,
        manager: "DownloadManager",  # noqa: ARG002 - uniform page signature
        repository: "DownloadRepository",
        relay: "CoreSignals",  # noqa: ARG002 - uniform page signature
        on_saved: Callable[[Settings], None],
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self._repo = repository
        self._on_saved = on_saved

        loaded = repository.get_settings()
        self._loaded: Settings = loaded if loaded is not None else Settings()

        form = self._build_form(self._loaded)

        buttons = QHBoxLayout()
        buttons.addStretch(1)
        self._save_btn = QPushButton("Save Settings")
        self._save_btn.setObjectName("primary")
        self._save_btn.clicked.connect(self._on_save)
        buttons.addWidget(self._save_btn)

        inner = QWidget()
        inner_layout = QVBoxLayout(inner)
        inner_layout.setContentsMargins(0, 0, 0, 0)
        inner_layout.setSpacing(12)
        heading = QLabel("Settings")
        heading.setObjectName("heading")
        inner_layout.addWidget(heading)
        inner_layout.addWidget(form)
        inner_layout.addLayout(buttons)
        inner_layout.addStretch(1)

        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setFrameShape(QScrollArea.Shape.NoFrame)
        scroll.setWidget(inner)

        root = QVBoxLayout(self)
        root.setContentsMargins(24, 20, 24, 20)
        root.addWidget(scroll)

    # -------------------------------------------------------- widget factory
    def _build_form(self, settings: Settings) -> QGroupBox:
        """Create every field from ``settings`` and wrap them in a form group."""
        group = QGroupBox("Application settings")
        form = QFormLayout(group)
        form.setFieldGrowthPolicy(QFormLayout.FieldGrowthPolicy.AllNonFixedFieldsGrow)

        # -- destination -----------------------------------------------------
        row = QHBoxLayout()
        self._destination_edit = QLineEdit(settings.default_destination_dir)
        self._destination_edit.setPlaceholderText("e.g. C:\\Users\\you\\Downloads")
        browse = QPushButton("Browse…")
        browse.clicked.connect(self._browse_destination)
        row.addWidget(self._destination_edit, 1)
        row.addWidget(browse)
        form.addRow(QLabel("Destination folder"), row)

        # -- concurrency (executor sized at startup) --------------------------
        row = QHBoxLayout()
        self._max_simultaneous = QSpinBox()
        self._max_simultaneous.setRange(1, 10)
        self._max_simultaneous.setValue(settings.max_simultaneous)
        hint = QLabel("↑ restart required to apply a change")
        hint.setObjectName("muted")
        row.addWidget(self._max_simultaneous)
        row.addWidget(hint, 1)
        form.addRow(QLabel("Max simultaneous downloads"), row)

        # -- network -----------------------------------------------------------
        self._timeout = QDoubleSpinBox()
        self._timeout.setRange(1.0, 600.0)
        self._timeout.setDecimals(1)
        self._timeout.setSingleStep(5.0)
        self._timeout.setSuffix(" s")
        self._timeout.setValue(settings.connection_timeout)
        form.addRow(QLabel("Connection timeout"), self._timeout)

        self._retries = QSpinBox()
        self._retries.setRange(0, 20)
        self._retries.setValue(settings.retry_count)
        form.addRow(QLabel("Retry count"), self._retries)

        self._retry_delay = QDoubleSpinBox()
        self._retry_delay.setRange(0.0, 600.0)
        self._retry_delay.setDecimals(1)
        self._retry_delay.setSingleStep(1.0)
        self._retry_delay.setSuffix(" s")
        self._retry_delay.setValue(settings.retry_delay)
        form.addRow(QLabel("Retry delay"), self._retry_delay)

        self._buffer_kib = QSpinBox()
        self._buffer_kib.setRange(1, 65536)
        self._buffer_kib.setSuffix(" KiB")
        self._buffer_kib.setValue(max(1, settings.buffer_size // _KIB))
        form.addRow(QLabel("Buffer size"), self._buffer_kib)

        self._bandwidth = QSpinBox()
        self._bandwidth.setRange(0, 1_048_576)
        self._bandwidth.setSuffix(" KB/s")
        self._bandwidth.setValue(settings.bandwidth_limit_bps // _KIB)
        self._bandwidth.setSpecialValueText("unlimited")
        self._bandwidth.setToolTip("0 (unlimited) disables the bandwidth cap.")
        form.addRow(QLabel("Bandwidth limit"), self._bandwidth)

        # -- collision / notifications ----------------------------------------
        self._collision = QComboBox()
        for value, label in (
            ("keep_existing", "Keep existing file"),
            ("rename", "Rename the new file"),
            ("overwrite", "Overwrite existing file"),
            ("skip", "Skip (decline the download)"),
        ):
            self._collision.addItem(label, value)
        index = self._collision.findData(settings.filename_collision)
        self._collision.setCurrentIndex(index if index >= 0 else 0)
        form.addRow(QLabel("Filename collision"), self._collision)

        self._notifications = QCheckBox("Show finish/failure notifications")
        self._notifications.setChecked(settings.notifications)
        form.addRow(QLabel("Notifications"), self._notifications)

        # -- theme -------------------------------------------------------------
        self._theme = QComboBox()
        for value, label in (
            ("dark", "Dark (default)"),
            ("light", "Light — partial: falls back to the platform palette"),
            ("system", "System — partial: follows the platform palette"),
        ):
            self._theme.addItem(label, value)
        index = self._theme.findData(settings.theme)
        self._theme.setCurrentIndex(index if index >= 0 else 0)
        form.addRow(QLabel("Theme"), self._theme)

        # -- HTTP identity / discovery -----------------------------------------
        self._user_agent = QLineEdit(settings.user_agent)
        form.addRow(QLabel("User agent"), self._user_agent)

        self._unknown_types = QCheckBox("Include unknown types in Discovery")
        self._unknown_types.setChecked(settings.include_unknown_types)
        form.addRow(QLabel("Discovery"), self._unknown_types)

        self._extensions_edit = QLineEdit(", ".join(sorted(settings.allowed_extensions)))
        self._extensions_edit.setPlaceholderText("zip, pdf, mp4, … (comma-separated, no dots)")
        self._extensions_edit.setToolTip(
            "Extensions Discovery treats as file candidates. Comma-separated, "
            "dots are stripped automatically."
        )
        form.addRow(QLabel("Allowed extensions"), self._extensions_edit)

        self._rate_limit = QCheckBox("Respect robots.txt / rate limits when scanning")
        self._rate_limit.setChecked(settings.respect_rate_limit)
        form.addRow(QLabel("Respect rate limits"), self._rate_limit)

        self._private_addresses = QCheckBox(
            "Allow private/local addresses (LAN testing only — unsafe in production)"
        )
        self._private_addresses.setChecked(settings.allow_private_addresses)
        form.addRow(QLabel("Allow private addresses"), self._private_addresses)

        return group

    # ------------------------------------------------------------------ dialog
    def _browse_destination(self) -> None:
        chosen = pick_folder(
            self,
            self._destination_edit.text().strip() or "",
            "Select default destination folder",
        )
        if chosen:
            self._destination_edit.setText(chosen)

    # -------------------------------------------------------------------- save
    def _collect(self) -> Settings:
        """Build one validated ``Settings`` snapshot from the form widgets."""
        extensions = {
            part.strip().lstrip(".").lower()
            for part in self._extensions_edit.text().split(",")
        }
        return Settings(
            default_destination_dir=self._destination_edit.text().strip(),
            max_simultaneous=self._max_simultaneous.value(),
            connection_timeout=self._timeout.value(),
            retry_count=self._retries.value(),
            retry_delay=self._retry_delay.value(),
            buffer_size=self._buffer_kib.value() * _KIB,
            bandwidth_limit_bps=self._bandwidth.value() * _KIB,
            filename_collision=self._collision.currentData(),
            notifications=self._notifications.isChecked(),
            theme=self._theme.currentData(),
            user_agent=self._user_agent.text().strip(),
            include_unknown_types=self._unknown_types.isChecked(),
            allowed_extensions=extensions,
            respect_rate_limit=self._rate_limit.isChecked(),
            allow_private_addresses=self._private_addresses.isChecked(),
        )

    def _on_save(self) -> None:
        """Validate → persist → hand to the MainWindow ``apply_settings`` hook."""
        try:
            settings = self._collect()
        except ValueError as exc:
            QMessageBox.warning(self, "Invalid settings", str(exc))
            return

        restart_needed = settings.max_simultaneous != self._loaded.max_simultaneous
        try:
            self._repo.save_settings(settings)
        except Exception as exc:  # noqa: BLE001 - sqlite errors surface nicely
            QMessageBox.critical(self, "Save settings", f"Could not save settings:\n{exc}")
            return

        self._loaded = settings
        self._apply_theme_now(settings.theme)
        self._on_saved(settings)

        message = "Settings saved and applied."
        if restart_needed:
            message += (
                "\n\nNote: “Max simultaneous downloads” changed — restart the "
                "application for that to take effect."
            )
        QMessageBox.information(self, "Settings", message)

    @staticmethod
    def _apply_theme_now(theme: str) -> None:
        """Re-style the running app: dark QSS, or the platform palette otherwise."""
        app = QApplication.instance()
        if app is None:
            return
        if theme == "dark":
            apply_theme(app)
        else:
            # light / system are documented as partial: plain platform styling.
            app.setStyleSheet("")
