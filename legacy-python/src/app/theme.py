"""Dark QSS theme + palette constants.

``apply_theme(app)`` installs the professional dark stylesheet and the
application font (Segoe UI 9.5pt). A light theme is listed in settings for
forward compatibility but intentionally not styled yet — it falls back to
the platform palette with a documented note on the Settings page.
"""

from __future__ import annotations

from PySide6.QtGui import QFont
from PySide6.QtWidgets import QApplication

__all__ = [
    "ACCENT",
    "ACCENT_HOVER",
    "BG",
    "BORDER",
    "ERROR",
    "MUTED",
    "QSS_DARK",
    "SUCCESS",
    "SURFACE",
    "SURFACE_ALT",
    "TEXT",
    "WARN",
    "apply_theme",
]

# -- palette ---------------------------------------------------------------
BG = "#1e1f24"           # window background (deep charcoal)
SURFACE = "#26272e"      # elevated surfaces: cards, panels, inputs
SURFACE_ALT = "#2e2f38"  # hover / alternate-row tint
BORDER = "#3a3c46"
ACCENT = "#4f7cff"       # indigo/blue accent
ACCENT_HOVER = "#6b93ff"
TEXT = "#e6e8ee"
MUTED = "#9aa0ae"
SUCCESS = "#3fb96d"
WARN = "#e0a53c"
ERROR = "#e5534b"

QSS_DARK = f"""
* {{
    color: {TEXT};
    font-family: "Segoe UI", "Segoe UI Variable", sans-serif;
    selection-background-color: {ACCENT};
    selection-color: #ffffff;
}}
QWidget {{
    background-color: {BG};
    font-size: 9.5pt;
}}
QMainWindow, QDialog {{
    background-color: {BG};
}}
QLabel {{
    background: transparent;
}}
QLabel#heading {{
    font-size: 13pt;
    font-weight: 600;
}}
QLabel#muted {{
    color: {MUTED};
}}
QLabel[ui-success="true"] {{ color: {SUCCESS}; }}
QLabel[ui-error="true"]   {{ color: {ERROR}; }}
QLabel[ui-warn="true"]    {{ color: {WARN}; }}

QFrame#card {{
    background-color: {SURFACE};
    border: 1px solid {BORDER};
    border-radius: 6px;
}}
QFrame#card QLabel#cardValue {{
    font-size: 14pt;
    font-weight: 600;
    background: transparent;
}}
QFrame#card QLabel#cardLabel {{
    color: {MUTED};
    font-size: 8.5pt;
    background: transparent;
}}

QLineEdit, QPlainTextEdit, QTextEdit, QSpinBox, QDoubleSpinBox {{
    background-color: {SURFACE};
    border: 1px solid {BORDER};
    border-radius: 4px;
    padding: 4px 6px;
}}
QLineEdit:focus, QPlainTextEdit:focus, QTextEdit:focus,
QSpinBox:focus, QDoubleSpinBox:focus {{
    border: 1px solid {ACCENT};
}}
QLineEdit:disabled, QSpinBox:disabled, QDoubleSpinBox:disabled {{
    color: {MUTED};
}}
QSpinBox::up-button, QSpinBox::down-button,
QDoubleSpinBox::up-button, QDoubleSpinBox::down-button {{
    width: 14px; background: {SURFACE_ALT};
}}
QComboBox {{
    background-color: {SURFACE};
    border: 1px solid {BORDER};
    border-radius: 4px;
    padding: 4px 6px;
}}
QComboBox:hover {{ border-color: {ACCENT}; }}
QComboBox::drop-down {{ border: none; width: 18px; }}
QComboBox::down-arrow {{
    image: none;
    border-left: 5px solid transparent;
    border-right: 5px solid transparent;
    border-top: 6px solid {MUTED};
    margin-right: 6px;
}}
QComboBox QAbstractItemView {{
    background-color: {SURFACE};
    border: 1px solid {BORDER};
    selection-background-color: {ACCENT};
}}

QPushButton {{
    background-color: {SURFACE};
    border: 1px solid {BORDER};
    border-radius: 4px;
    padding: 5px 14px;
}}
QPushButton:hover {{ background-color: {SURFACE_ALT}; border-color: {ACCENT}; }}
QPushButton:pressed {{ background-color: {BG}; }}
QPushButton:disabled {{ color: {MUTED}; border-color: {BORDER}; }}
QPushButton#primary {{
    background-color: {ACCENT};
    border: 1px solid {ACCENT};
    color: #ffffff;
    font-weight: 600;
}}
QPushButton#primary:hover {{ background-color: {ACCENT_HOVER}; }}
QPushButton#primary:pressed {{ background-color: #4670e8; }}
QPushButton#primary:disabled {{
    background-color: {SURFACE};
    color: {MUTED};
    border-color: {BORDER};
}}

QCheckBox {{ background: transparent; spacing: 6px; }}
QCheckBox::indicator {{
    width: 15px; height: 15px;
    border: 1px solid {BORDER};
    border-radius: 3px;
    background: {SURFACE};
}}
QCheckBox::indicator:checked {{ background: {ACCENT}; border-color: {ACCENT}; }}
QCheckBox::indicator:hover {{ border-color: {ACCENT}; }}

QGroupBox {{
    border: 1px solid {BORDER};
    border-radius: 6px;
    margin-top: 10px;
    padding-top: 6px;
}}
QGroupBox::title {{
    subcontrol-origin: margin;
    left: 8px;
    padding: 0 4px;
    color: {MUTED};
}}

QTableView {{
    background-color: {SURFACE};
    alternate-background-color: {BG};
    border: 1px solid {BORDER};
    border-radius: 4px;
    gridline-color: {BORDER};
    selection-background-color: {ACCENT};
    selection-color: #ffffff;
}}
QHeaderView::section {{
    background-color: {SURFACE_ALT};
    color: {MUTED};
    border: none;
    border-bottom: 1px solid {BORDER};
    border-right: 1px solid {BORDER};
    padding: 5px 6px;
}}
QTableCornerButton::section {{ background-color: {SURFACE_ALT}; border: none; }}

QProgressBar {{
    background-color: {SURFACE};
    border: 1px solid {BORDER};
    border-radius: 4px;
    text-align: center;
    color: {TEXT};
    height: 16px;
}}
QProgressBar::chunk {{
    background-color: {ACCENT};
    border-radius: 3px;
}}

QScrollBar:vertical {{
    background: {BG}; width: 11px; margin: 0;
}}
QScrollBar::handle:vertical {{
    background: {BORDER}; min-height: 24px; border-radius: 5px; margin: 1px 2px;
}}
QScrollBar::handle:vertical:hover {{ background: {MUTED}; }}
QScrollBar:horizontal {{
    background: {BG}; height: 11px; margin: 0;
}}
QScrollBar::handle:horizontal {{
    background: {BORDER}; min-width: 24px; border-radius: 5px; margin: 2px 1px;
}}
QScrollBar::add-line, QScrollBar::sub-line {{ width: 0; height: 0; }}

QStatusBar {{
    background: {SURFACE};
    border-top: 1px solid {BORDER};
    color: {MUTED};
}}
QStatusBar::item {{ border: none; }}

QToolButton#navButton {{
    background: transparent;
    border: none;
    border-radius: 4px;
    padding: 8px 12px;
    text-align: left;
    color: {TEXT};
    font-size: 10pt;
}}
QToolButton#navButton:hover {{ background: {SURFACE}; }}
QToolButton#navButton:checked {{
    background: {SURFACE};
    color: {ACCENT_HOVER};
    border-left: 3px solid {ACCENT};
}}
QToolButton#navButton:checked:hover {{ background: {SURFACE}; }}

QToolTip {{
    background-color: {SURFACE_ALT};
    color: {TEXT};
    border: 1px solid {BORDER};
    padding: 3px;
}}

QMessageBox {{ background-color: {BG}; }}
QMessageBox QPushButton {{ min-width: 72px; }}

QMenu {{
    background-color: {SURFACE};
    border: 1px solid {BORDER};
}}
QMenu::item:selected {{ background-color: {ACCENT}; }}

QSplitter::handle {{ background: {BORDER}; width: 1px; height: 1px; }}
"""


def apply_theme(app: QApplication) -> None:
    """Install the dark stylesheet and application font (idempotent)."""
    app.setStyleSheet(QSS_DARK)
    font = QFont("Segoe UI")
    font.setPointSizeF(9.5)
    app.setFont(font)
