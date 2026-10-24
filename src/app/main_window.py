"""Main window: sidebar navigation, stacked pages, signal relay, status bar.

The window owns one :class:`DownloadManager` (injected by ``main``) and one
:class:`DownloadRepository`. Core observer callbacks (invoked on worker
threads) are bridged into Qt via a :class:`CoreSignals` relay — emitting a
Qt signal from a foreign thread is safe and connections default to queued
delivery onto the GUI thread.
"""

from __future__ import annotations

import logging

from PySide6.QtCore import QObject, QSize, Qt, QTimer, Signal
from PySide6.QtGui import QCloseEvent, QKeySequence, QShortcut
from PySide6.QtWidgets import (
    QApplication,
    QButtonGroup,
    QFrame,
    QHBoxLayout,
    QLabel,
    QMainWindow,
    QStackedWidget,
    QStyle,
    QToolButton,
    QVBoxLayout,
    QWidget,
)

from src.app.dialogs import confirm
from src.app.theme import MUTED
from src.database.models import Settings
from src.database.repository import DownloadRepository
from src.ui.add_downloads import AddDownloadsPage
from src.ui.dashboard import DashboardPage
from src.ui.discovery_view import DiscoveryPage
from src.ui.history_view import HistoryPage
from src.ui.queue_view import QueuePage
from src.ui.settings_view import SettingsPage
from src.app.utils_format import format_speed
from src.core.download_manager import DownloadManager

__all__ = ["CoreSignals", "MainWindow"]

log = logging.getLogger(__name__)


class CoreSignals(QObject):
    """Thread-safe relay from core callbacks into Qt signal space."""

    #: :class:`~src.database.models.DownloadProgress` snapshot from a worker.
    progressEvent = Signal(object)
    #: :class:`~src.database.models.DownloadRecord` after a persisted state change.
    stateChangedEvent = Signal(object)


class _NavItem:
    """One sidebar entry: label, QStyle icon enum name, page factory tag."""

    __slots__ = ("label", "icon", "index")

    def __init__(self, label: str, icon: str) -> None:
        self.label = label
        self.icon = icon
        self.index = 0


class MainWindow(QMainWindow):
    """Application shell; caller supplies the manager and repository."""

    NAV_TITLES: tuple[tuple[str, str], ...] = (
        ("Dashboard", "SP_ComputerIcon"),
        ("Add Downloads", "SP_ArrowDown"),
        ("Discover", "SP_FileDialogContentsView"),
        ("Queue", "SP_BrowserReload"),
        ("History", "SP_FileDialogListView"),
        ("Settings", "SP_FileDialogInfoView"),
    )

    def __init__(
        self,
        manager: DownloadManager,
        repository: DownloadRepository,
        settings: Settings,
        *,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self._manager = manager
        self._repo = repository
        self.settings: Settings = settings
        self.pending_interrupted: list[int] = []  # ids offered a resume in main()

        self.setWindowTitle("Bulk Download Manager")
        self.setMinimumSize(QSize(1000, 650))
        self.resize(1240, 780)

        self._relay = CoreSignals()
        manager.on_progress = self._relay.progressEvent.emit
        manager.on_state_change = self._relay.stateChangedEvent.emit

        central = QWidget()
        root = QVBoxLayout(central)
        root.setContentsMargins(0, 0, 0, 0)
        root.setSpacing(0)
        body = QHBoxLayout()
        body.setContentsMargins(0, 0, 0, 0)
        body.setSpacing(0)

        self._stack = QStackedWidget()
        self._pages = self._build_pages()
        for page in self._pages:
            self._stack.addWidget(page)
        self._wire_page_refresh()

        body.addWidget(self._build_sidebar())
        separator = QFrame()
        separator.setFixedWidth(1)
        separator.setStyleSheet(f"background: {MUTED};")
        body.addWidget(separator)
        body.addWidget(self._stack, 1)
        root.addLayout(body, 1)
        self.setCentralWidget(central)

        self._status_speed = QLabel("0 B/s")
        self._status_active = QLabel("0 active")
        status_bar = self.statusBar()
        status_bar.addPermanentWidget(self._status_active)
        status_bar.addPermanentWidget(self._status_speed)

        self._status_timer = QTimer(self)
        self._status_timer.setInterval(500)
        self._status_timer.timeout.connect(self._update_status_bar)
        self._status_timer.start()

        self._select_page(0)
        QShortcut(QKeySequence("Ctrl+1"), self, activated=lambda: self._select_page(0))
        QShortcut(QKeySequence("Ctrl+2"), self, activated=lambda: self._select_page(1))
        QShortcut(QKeySequence("Ctrl+3"), self, activated=lambda: self._select_page(2))
        QShortcut(QKeySequence("Ctrl+4"), self, activated=lambda: self._select_page(3))
        QShortcut(QKeySequence("Ctrl+5"), self, activated=lambda: self._select_page(4))
        QShortcut(QKeySequence("Ctrl+6"), self, activated=lambda: self._select_page(5))
        QShortcut(QKeySequence("Ctrl+Q"), self, activated=self.close)

    # ----------------------------------------------------------- construction
    def _build_sidebar(self) -> QWidget:
        """Build the fixed-width sidebar with exclusive checkable buttons."""
        sidebar = QWidget()
        sidebar.setFixedWidth(188)
        sidebar.setStyleSheet(f"background: {SURFACE_OVERRIDE};")
        layout = QVBoxLayout(sidebar)
        layout.setContentsMargins(12, 16, 12, 12)
        layout.setSpacing(4)

        app_title = QLabel("Bulk Download<br/>Manager")
        app_title.setObjectName("heading")
        layout.addWidget(app_title)
        layout.addSpacing(10)

        self._nav_group = QButtonGroup(self)
        self._nav_group.setExclusive(True)
        style = QApplication.style()
        for index, (label, icon_name) in enumerate(self.NAV_TITLES):
            button = QToolButton()
            button.setObjectName("navButton")
            button.setText(label)
            button.setCheckable(True)
            button.setToolButtonStyle(Qt.ToolButtonStyle.ToolButtonTextBesideIcon)
            standard_icon = getattr(QStyle.StandardPixmap, icon_name)
            button.setIcon(style.standardIcon(standard_icon))
            button.setIconSize(QSize(18, 18))
            button.clicked.connect(lambda _checked=False, i=index: self._select_page(i))
            self._nav_group.addButton(button, index)
            layout.addWidget(button)
        layout.addStretch(1)
        return sidebar

    def _build_pages(self) -> list[QWidget]:
        """Instantiate pages in nav bar order (they receive the app context)."""
        relay = self._relay
        manager = self._manager
        repo = self._repo
        return [
            DashboardPage(manager, repo, relay),
            AddDownloadsPage(manager, repo, relay, self),
            DiscoveryPage(manager, repo, relay, self),
            QueuePage(manager, repo, relay),
            HistoryPage(manager, repo, relay),
            SettingsPage(manager, repo, relay, self.apply_settings),
        ]

    def _wire_page_refresh(self) -> None:
        """Bridge pages' queued-downloads signals to immediate Queue page refresh."""
        queue_page = self._pages[3]  # QueuePage slot in _build_pages order
        for source_page, signal_name in (
            (self._pages[1], "downloadsQueued"),  # AddDownloadsPage
            (self._pages[2], "queuedSome"),  # DiscoveryPage
        ):
            signal = getattr(source_page, signal_name, None)
            refresh = getattr(queue_page, "refresh", None)
            if signal is not None and callable(refresh):
                signal.connect(refresh)

    # -------------------------------------------------------------- settings
    def current_settings(self) -> Settings:
        """The settings snapshot pages should honour right now."""
        return self.settings

    def apply_settings(self, settings: Settings) -> None:
        """Store and propagate settings onto the manager.

        Most fields (timeouts, retries, bandwidth, collision policy, UA,
        discovery flags) take effect immediately because workers read them
        per-run. ``max_simultaneous`` sizes the executor at construction and
        therefore needs an app restart to change.
        """
        self.settings = settings
        # The attribute is read by every dispatched worker; replacing the
        # object is safe. Executor concurrency stays as constructed (restart
        # required for that one field).
        self._manager._settings = settings  # noqa: SLF001 - documented above

    # ----------------------------------------------------------------- pages
    def _select_page(self, index: int) -> None:
        """Show page ``index`` and sync the sidebar check state."""
        if 0 <= index < self._stack.count():
            self._stack.setCurrentIndex(index)
            button = self._nav_group.button(index)
            if button is not None:
                button.setChecked(True)
            page = self._pages[index]
            refresh = getattr(page, "refresh", None)
            if callable(refresh):
                refresh()

    # ---------------------------------------------------------------- status
    def _update_status_bar(self) -> None:
        """Aggregate speed + active count pulled from the manager."""
        try:
            speed = self._manager.total_speed_bps()
            active = self._manager.list_active()
        except Exception as exc:  # pragma: no cover - status must never crash
            log.debug("status bar refresh failed: %s", exc)
            return
        self._status_speed.setText(format_speed(speed))
        self._status_active.setText(f"{len(active)} active")

    # ------------------------------------------------------------- shutdown
    def closeEvent(self, event: QCloseEvent) -> None:  # noqa: N802 - Qt override
        """Confirm when downloads are running, then stop the manager + DB."""
        try:
            active = self._manager.list_active()
        except Exception:  # pragma: no cover
            active = []
        if active:
            names = ", ".join(rec.display_name or str(rec.id) for rec in active[:3])
            message = (
                f"{len(active)} download(s) are still active ({names}<br/>…). "
                "Quit anyway? Active downloads will be cancelled."
            )
            if not confirm(self, "Quit with active downloads?", message):
                event.ignore()
                return
            for record in active:
                self._manager.cancel(record.id or 0)

        self._manager.shutdown(wait=True)
        self._repo.close()
        log.info("application closed cleanly")
        event.accept()


SURFACE_OVERRIDE = "#26272e"
