"""URL intake: parse bulk lines of URLs, validate, reject unsafe input safely.

Thin wrapper over :mod:`src.core.security` — this module never executes,
transforms or passes URL text to a shell; a URL string is data, not code.
"""

from __future__ import annotations

import urllib.parse
from dataclasses import dataclass, field

from src.core import security
from src.core.security import UnsafeUrlError

__all__ = ["ParseResult", "parse_url_lines"]


@dataclass
class ParseResult:
    """Outcome of ingesting a block of pasted/imported URL lines.

    Attributes:
        accepted: validated, safe URLs (order of first appearance).
        rejected: ``(line_as_given, reason)`` pairs for unusable lines.
        duplicates: lines that exactly repeated an earlier URL.
    """

    accepted: list[str] = field(default_factory=list)
    rejected: list[tuple[str, str]] = field(default_factory=list)
    duplicates: list[str] = field(default_factory=list)


def _extract_url(raw_line: str) -> str:
    """Return the URL part of a line (first whitespace token)."""
    url = raw_line.strip()
    if "\t" in url:  # tab-separated "url<TAB>name" imports
        url = url.split("\t", 1)[0]
    return url.strip()


def parse_url_lines(text: str, *, allow_private: bool = False) -> ParseResult:
    """Parse whitespace-separated lines of URLs into accepted/rejected/dups.

    Handles CRLF, LF and bare-CR line endings, strips surrounding whitespace,
    skips blank lines and deduplicates exact repeats (first occurrence wins,
    order preserved). Every URL must be http/https and network-safe; private
    addresses are rejected unless ``allow_private`` is set.

    This function returns strings only — callers own anything that would
    interpret a URL as a command.
    """
    result = ParseResult()
    if not text:
        return result

    seen: set[str] = set()
    for raw_line in text.replace("\r\n", "\n").replace("\r", "\n").split("\n"):
        line = raw_line.strip()
        if not line:
            continue
        candidate = _extract_url(line)
        if not candidate:
            result.rejected.append((line, "no URL found on line"))
            continue
        if candidate in seen:
            result.duplicates.append(candidate)
            continue
        reason = _validate_candidate(candidate, allow_private=allow_private)
        if reason is not None:
            result.rejected.append((line, reason))
            continue
        seen.add(candidate)
        result.accepted.append(candidate)
    return result


def _validate_candidate(candidate: str, *, allow_private: bool) -> str | None:
    """Return a rejection reason for ``candidate`` or ``None`` if acceptable."""
    parsed = urllib.parse.urlsplit(candidate)
    if parsed.scheme.lower() not in ("http", "https"):
        return "only http and https URLs are supported"
    try:
        security.validate_url_for_network(candidate, allow_private=allow_private)
    except UnsafeUrlError as exc:
        return f"unsafe URL: {exc}"
    return None
