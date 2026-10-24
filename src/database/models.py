"""Data models shared by the engine, database and (later) the GUI.

Contains the ``DownloadState`` state machine, ``DownloadRecord`` and
``DownloadProgress`` dataclasses, and the ``Settings`` configuration model.
This module must stay free of GUI imports.
"""

from __future__ import annotations

import dataclasses
import enum
import json
from dataclasses import dataclass, field
from enum import StrEnum
from typing import Any

__all__ = [
    "DEFAULT_ALLOWED_EXTENSIONS",
    "DownloadProgress",
    "DownloadRecord",
    "DownloadState",
    "Settings",
    "TERMINAL_STATES",
]


class DownloadState(StrEnum):
    """Lifecycle states of a download.

    State machine::

        QUEUED -> CONNECTING -> DOWNLOADING -> COMPLETED
                                 |-> PAUSED
                                 |-> FAILED / CANCELLED
        INTERRUPTED (app restart while CONNECTING/DOWNLOADING)
    """

    QUEUED = "queued"
    CONNECTING = "connecting"
    DOWNLOADING = "downloading"
    PAUSED = "paused"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELLED = "cancelled"
    INTERRUPTED = "interrupted"

    #: States that never resume: the download is settled for good.
    #: ``enum.nonmember`` explicitly marks this as a non-member so enum
    #: machinery never treats it as a ``DownloadState`` member — this works
    #: even under PEP 563 (``from __future__ import annotations``), where a
    #: stringified ``ClassVar`` annotation would not be resolved on 3.14.
    TERMINAL_STATES = enum.nonmember(frozenset({COMPLETED, FAILED, CANCELLED}))


TERMINAL_STATES: frozenset[DownloadState] = frozenset(
    {DownloadState.COMPLETED, DownloadState.FAILED, DownloadState.CANCELLED}
)


@dataclass
class DownloadRecord:
    """One download as persisted in the database and surfaced to the UI."""

    id: int | None = None
    source_url: str = ""
    display_name: str = ""
    destination_path: str = ""
    state: DownloadState = DownloadState.QUEUED
    downloaded_bytes: int = 0
    expected_size: int | None = None
    created_at: str = ""
    completed_at: str | None = None
    retry_count: int = 0
    error_summary: str | None = None
    etag: str | None = None
    last_modified: str | None = None
    checksum_sha256: str | None = None
    checksum_verified: bool = False
    content_type: str | None = None


@dataclass
class DownloadProgress:
    """Throttled progress snapshot (~4/s) emitted by a running worker."""

    download_id: int
    downloaded_bytes: int
    total_bytes: int | None
    speed_bps: float
    eta_seconds: int | None


DEFAULT_ALLOWED_EXTENSIONS: frozenset[str] = frozenset(
    {
        "zip", "rar", "7z", "iso", "pdf", "epub", "exe", "msi", "dmg", "img",
        "tar", "gz", "xz", "bz2", "mp3", "mp4", "mkv", "avi", "mov", "wav",
        "flac", "jpg", "jpeg", "png", "gif", "webp", "svg", "tif", "bmp",
        "doc", "docx", "xls", "xlsx", "ppt", "pptx", "csv", "txt", "json",
        "xml", "apk", "ipa",
    }
)

COLLISION_POLICIES: frozenset[str] = frozenset(
    {"keep_existing", "rename", "overwrite", "skip"}
)

VALID_THEMES: frozenset[str] = frozenset({"dark", "light", "system"})

DEFAULT_USER_AGENT: str = "BulkDownloadManager/0.1 (Windows; +VEEDIZ Studio)"


@dataclass
class Settings:
    """Application settings, persisted via the repository as JSON rows."""

    default_destination_dir: str = ""
    max_simultaneous: int = 3
    connection_timeout: float = 30.0
    retry_count: int = 3
    retry_delay: float = 2.0
    buffer_size: int = 64 * 1024
    bandwidth_limit_bps: int = 0  # 0 = unlimited
    filename_collision: str = "keep_existing"
    notifications: bool = True
    theme: str = "dark"
    user_agent: str = DEFAULT_USER_AGENT
    include_unknown_types: bool = False
    allowed_extensions: set[str] = field(
        default_factory=lambda: set(DEFAULT_ALLOWED_EXTENSIONS)
    )
    respect_rate_limit: bool = True
    allow_private_addresses: bool = False  # escape hatch for LAN/local testing

    def __post_init__(self) -> None:
        """Validate and normalise values, raising ``ValueError`` on junk."""
        self.max_simultaneous = max(1, min(10, int(self.max_simultaneous)))
        self.retry_count = int(self.retry_count)
        if self.retry_count < 0 or self.retry_count > 20:
            raise ValueError("retry_count must be an integer in 0..20")
        if float(self.connection_timeout) <= 0:
            raise ValueError("connection_timeout must be > 0 seconds")
        self.connection_timeout = float(self.connection_timeout)
        self.retry_delay = float(self.retry_delay)
        if self.retry_delay < 0:
            raise ValueError("retry_delay must be >= 0 seconds")
        if int(self.buffer_size) <= 0:
            raise ValueError("buffer_size must be > 0 bytes")
        self.buffer_size = int(self.buffer_size)
        if int(self.bandwidth_limit_bps) < 0:
            raise ValueError("bandwidth_limit_bps must be >= 0 (0 = unlimited)")
        self.bandwidth_limit_bps = int(self.bandwidth_limit_bps)
        if self.filename_collision not in COLLISION_POLICIES:
            raise ValueError(
                "filename_collision must be one of "
                f"{sorted(COLLISION_POLICIES)}, got {self.filename_collision!r}"
            )
        if not isinstance(self.theme, str) or self.theme not in VALID_THEMES:
            raise ValueError(
                f"theme must be one of {sorted(VALID_THEMES)}, got {self.theme!r}"
            )
        if not isinstance(self.user_agent, str) or not self.user_agent.strip():
            raise ValueError("user_agent must be a non-empty string")
        if self.default_destination_dir is None:
            self.default_destination_dir = ""

        normalised: set[str] = set()
        for ext in self.allowed_extensions:
            name = str(ext).strip().lower().lstrip(".")
            if not name or len(name) > 10 or not name.isalnum():
                raise ValueError(f"invalid allowed extension: {ext!r}")
            normalised.add(name)
        self.allowed_extensions = normalised

    def to_json(self) -> str:
        """Serialise all fields to a JSON document for repository storage."""
        payload: dict[str, Any] = dataclasses.asdict(self)
        payload["allowed_extensions"] = sorted(self.allowed_extensions)
        return json.dumps(payload, sort_keys=True)

    @classmethod
    def from_json(cls, raw: str) -> "Settings":
        """Rebuild settings from a stored JSON document.

        Unknown/missing keys fall back to defaults; values are re-validated
        through the constructor, so a corrupt store raises ``ValueError``.
        """
        data: dict[str, Any] = json.loads(raw)
        valid_fields = {f.name for f in dataclasses.fields(cls)}
        kwargs = {k: v for k, v in data.items() if k in valid_fields}
        if isinstance(kwargs.get("allowed_extensions"), list):
            kwargs["allowed_extensions"] = set(str(x) for x in kwargs["allowed_extensions"])
        return cls(**kwargs)
