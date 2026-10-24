"""Retry/backoff decision engine for network failures.

Central place that classifies failures as transient vs permanent and turns a
policy into ``(should_retry, delay_seconds)`` decisions, honouring
``Retry-After`` when the server provides one.
"""

from __future__ import annotations

import email.utils
from datetime import UTC, datetime

__all__ = [
    "AuthRequiredError",
    "DownloadCancelled",
    "DownloadFailed",
    "DownloadPaused",
    "TransientHttpError",
    "RetryPolicy",
    "TRANSIENT_STATUS_CODES",
    "PERMANENT_STATUS_CODES",
]

# Statuses worth retrying with backoff.
TRANSIENT_STATUS_CODES = frozenset({408, 425, 429, 500, 502, 503, 504})
# Statuses that must NEVER be retried and get an actionable message.
PERMANENT_STATUS_CODES = frozenset({400, 401, 403, 404, 405, 406, 410, 451})


class DownloadError(Exception):
    """Base class: download cannot proceed (message is user-actionable)."""


class DownloadFailed(DownloadError):
    """A permanent failure (bad status, bad content, checksum mismatch...)."""


class AuthRequiredError(DownloadFailed):
    """The server demanded authentication (401/403) — distinct from transient."""


class DownloadPaused(Exception):
    """Raised internally when a user pauses a running download."""


class DownloadCancelled(Exception):
    """Raised internally when a user cancels a running download."""


class TransientHttpError(DownloadError):
    """A retryable HTTP failure with optional server-provided ``retry_after``."""

    def __init__(self, status: int, retry_after: float | None = None) -> None:
        super().__init__(f"transient HTTP error {status}")
        self.status = status
        self.retry_after = retry_after


class RetryPolicy:
    """Exponential-backoff policy with a hard cap and Retry-After support."""

    def __init__(
        self,
        max_retries: int = 3,
        base_delay: float = 2.0,
        max_delay: float = 60.0,
    ) -> None:
        if max_retries < 0:
            raise ValueError("max_retries must be >= 0")
        if base_delay < 0:
            raise ValueError("base_delay must be >= 0")
        if max_delay < base_delay:
            raise ValueError("max_delay must be >= base_delay")
        self.max_retries = max_retries
        self.base_delay = base_delay
        self.max_delay = max_delay

    def should_retry(
        self,
        exc_or_status: Exception | int,
        attempt: int,
    ) -> tuple[bool, float]:
        """Decide whether ``exc_or_status`` should be retried after ``attempt``.

        ``attempt`` is the number of failures so far (0-based before the first
        retry). Returns ``(should_retry, delay_seconds)``. Delay comes from a
        ``Retry-After`` header when available, otherwise exponential backoff
        ``base_delay * 2**attempt`` capped at ``max_delay``.
        """
        if attempt >= self.max_retries:
            return (False, 0.0)

        classified = self._classify(exc_or_status)
        if classified is None:
            return (False, 0.0)
        backoff, retry_after = classified
        if not backoff:
            return (False, 0.0)
        delay = _parse_retry_after(retry_after)
        if delay is None:
            delay = min(self.base_delay * (2 ** max(attempt, 0)), self.max_delay)
        return (True, delay)

    def _classify(
        self, exc_or_status: Exception | int
    ) -> tuple[bool, float | None] | None:
        """Map a status code or exception onto (retryable, retry_after) or None."""
        if isinstance(exc_or_status, int):
            status = exc_or_status
            return (status in TRANSIENT_STATUS_CODES, None)
        if isinstance(exc_or_status, TransientHttpError):
            return (True, exc_or_status.retry_after)

        # httpx exceptions: timeouts & network errors are transient.
        name = type(exc_or_status).__name__
        module = type(exc_or_status).__module__ or ""
        if module.startswith("httpx"):
            ancestors = {cls.__name__ for cls in type(exc_or_status).__mro__}
            if "TimeoutException" in ancestors or "NetworkError" in ancestors:
                return (True, None)
            if name == "RemoteProtocolError":
                return (True, None)
            if name == "HTTPStatusError":
                response = getattr(exc_or_status, "response", None)
                status = int(getattr(response, "status_code", 0) or 0)
                retry_after = None
                headers = getattr(response, "headers", None)
                if headers is not None:
                    retry_after = headers.get("Retry-After")
                if status == 429 or status in TRANSIENT_STATUS_CODES:
                    return (True, retry_after)
                return (False, None)
        if isinstance(exc_or_status, (TimeoutError, ConnectionError)):
            return (True, None)
        return None


def _parse_retry_after(value: float | None) -> float | None:
    """Parse a ``Retry-After`` value (delay seconds or an HTTP date)."""
    if value is None:
        return None
    if isinstance(value, (int, float)):
        return max(0.0, float(value))
    text = str(value).strip()
    if not text:
        return None
    if text.isdigit():
        return max(0.0, float(text))
    try:
        when = email.utils.parsedate_to_datetime(text)
    except (TypeError, ValueError):
        return None
    if when is None:
        return None
    if when.tzinfo is None:
        when = when.replace(tzinfo=UTC)
    return max(0.0, (when - datetime.now(tz=UTC)).total_seconds())
