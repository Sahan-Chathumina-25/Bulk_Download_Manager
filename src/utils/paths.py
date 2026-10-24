"""Per-user data, log, database and default-download locations.

All user-writable data lives under the per-user application-data directory
(``%LOCALAPPDATA%\\BulkDownloadManager`` on Windows). When the app is frozen
with PyInstaller, ``sys._MEIPASS`` points at the (possibly read-only) bundle
extraction directory — it is deliberately **ignored** here: nothing a user
generates is ever written into the bundle.
"""

from __future__ import annotations

import os
import pathlib
import sys

__all__ = [
    "APP_DIR_NAME",
    "db_path",
    "default_download_dir",
    "log_dir",
    "user_data_dir",
]

APP_DIR_NAME = "BulkDownloadManager"


def _frozen() -> bool:
    """True when running from a PyInstaller-style frozen bundle."""
    return getattr(sys, "frozen", False) or hasattr(sys, "_MEIPASS")


def _local_app_data() -> pathlib.Path:
    """Resolve the per-user application-data base directory cross-platform.

    Windows uses ``%LOCALAPPDATA%``; other platforms fall back to the XDG
    data home (or ``~/.local/share``). The frozen-bundle directory
    (``sys._MEIPASS``) is never used as a base.
    """
    if os.name == "nt":
        raw = os.environ.get("LOCALAPPDATA") or ""
        if raw.strip():
            return pathlib.Path(raw)
        return pathlib.Path.home() / "AppData" / "Local"
    xdg = os.environ.get("XDG_DATA_HOME") or ""
    if xdg.strip():
        return pathlib.Path(xdg)
    return pathlib.Path.home() / ".local" / "share"


def user_data_dir() -> pathlib.Path:
    """Directory for everything the user generates (settings, db, logs).

    Guaranteed to be outside any PyInstaller bundle: ``sys._MEIPASS`` and the
    frozen executable directory are explicitly ignored, so user data is
    writable and survives application updates.
    """
    if _frozen():
        # Defensive: even under exotic embedders that export a custom user
        # data override, prefer that over the read-only bundle directory.
        override = os.environ.get("BDM_USER_DATA_DIR") or ""
        if override.strip():
            path = pathlib.Path(override)
            path.mkdir(parents=True, exist_ok=True)
            return path
    path = _local_app_data() / APP_DIR_NAME
    path.mkdir(parents=True, exist_ok=True)
    return path


def log_dir() -> pathlib.Path:
    """Directory for rotating log files (created on demand)."""
    path = user_data_dir() / "logs"
    path.mkdir(parents=True, exist_ok=True)
    return path


def db_path() -> pathlib.Path:
    """Path of the SQLite database (parent directory created)."""
    return user_data_dir() / "bulk_download_manager.db"


def default_download_dir() -> pathlib.Path:
    """The user's Downloads folder (or ``~/Downloads`` fallback).

    The directory is probed but never created: if the user has no Downloads
    folder, the conventional ``home/Downloads`` path is returned for callers
    to diagnose when used.
    """
    home = pathlib.Path.home()
    candidate = home / "Downloads"
    if candidate.is_dir():
        return candidate
    return candidate  # home/Downloads is the documented fallback
