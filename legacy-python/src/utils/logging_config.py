"""Process-wide logging configuration with URL redaction.

:idempotent: ``setup_logging()`` attaches a rotating file handler and a
console stream handler to the root logger exactly once; calling it again is
a no-op (a different ``level`` does not re-configure handlers).

Every handler gets :class:`URLRedactionFilter`, which runs
:func:`src.core.security.redact_url` over any ``http(s)://...`` substring in
the formatted message so auth tokens, api keys and userinfo never reach the
console or the log files.
"""

from __future__ import annotations

import logging
import logging.handlers
import re
from pathlib import Path

from src.core.security import redact_url
from src.utils.paths import log_dir

__all__ = ["URLRedactionFilter", "setup_logging"]

#: 1 MiB per file, 3 rotated backups (app.log, app.log.1, ...).
_MAX_BYTES = 1_048_576
_BACKUPS = 3

_URL_RE = re.compile(r"https?://[^\s<>\"']+")

_configured = False


class URLRedactionFilter(logging.Filter):
    """Redact secret-bearing URLs in a record's rendered message.

    The record's message and args are rendered via ``getMessage()``; any
    ``http(s)://`` substring within it is replaced by
    :func:`src.core.security.redact_url(url)`. A rendering failure (e.g.
    mismatched format args) leaves the record untouched — logging must never
    crash the caller.
    """

    def filter(self, record: logging.LogRecord) -> bool:
        """Mutate ``record`` in place; always returns True (never drop)."""
        try:
            message = record.getMessage()
        except Exception:  # noqa: BLE001 - a malformed record must not crash
            return True
        if "http://" in message or "https://" in message:
            message = _URL_RE.sub(lambda m: redact_url(m.group(0)), message)
            record.msg = message
            record.args = ()
        return True


def _has_url_filter(handler: logging.Handler) -> bool:
    """True if ``handler`` already carries a :class:`URLRedactionFilter`."""
    return any(
        isinstance(existing, URLRedactionFilter) for existing in handler.filters
    )


def setup_logging(level: int = logging.INFO) -> None:
    """Configure root logging once: rotating file + console, both redacted.

    Args:
        level: root logger level for the first (only) configuration call.

    Idempotent: subsequent calls return immediately without adding
    duplicate handlers or changing the level.
    """
    global _configured
    if _configured:
        return

    directory: Path = log_dir()
    file_handler = logging.handlers.RotatingFileHandler(
        directory / "app.log",
        maxBytes=_MAX_BYTES,
        backupCount=_BACKUPS,
        encoding="utf-8",
    )
    console_handler = logging.StreamHandler()
    for handler in (file_handler, console_handler):
        handler.setLevel(level)
        handler.addFilter(URLRedactionFilter())
        handler.setFormatter(
            logging.Formatter(
                "%(asctime)s %(levelname)-8s %(name)s: %(message)s",
                datefmt="%Y-%m-%d %H:%M:%S",
            )
        )

    root = logging.getLogger()
    root.setLevel(level)
    root.addHandler(file_handler)
    root.addHandler(console_handler)
    _configured = True
