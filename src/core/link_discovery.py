"""Link discovery: scan one web page and report downloadable-file candidates.

The scanner downloads **nothing**: it fetches at most one HTML document
(starting URL + up to 10 redirects), optionally issues a bounded number of
HEAD requests for metadata, and reports candidates for the caller —
typically the GUI's "add from page" flow that feeds
:class:`src.core.download_manager.DownloadManager`.

Safety posture mirrors the download engine:

- the page URL and every redirect hop are re-validated through
  :func:`src.core.security.validate_url_for_network` /
  :func:`src.core.security.validate_redirect` (honouring
  ``settings.allow_private_addresses``, refusing https→http downgrades),
- the page body is capped at 5 MiB (streamed read, aborted beyond),
- ``robots.txt`` is honoured when ``settings.respect_rate_limit`` is set;
  robots verdicts are cached per origin for the lifetime of the
  :class:`HttpLinkDiscovery` instance — **reuse one instance across
  pages** to benefit from that cache and the shared connection pool.

Malformed HTML is tolerated (BeautifulSoup ``html.parser``); network and
validation problems are appended to ``DiscoveryResult.errors`` as
actionable messages instead of raising.
"""

from __future__ import annotations

import email.utils
import logging
import threading
import time
import urllib.parse
import urllib.robotparser
from collections.abc import Iterator
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Protocol

import httpx
from bs4 import BeautifulSoup

from src.core import filename_utils, security
from src.core.security import UnsafeUrlError
from src.database.models import Settings

__all__ = [
    "MAX_CANDIDATES",
    "MAX_HEAD_REQUESTS",
    "MAX_PAGE_BODY_BYTES",
    "MAX_REDIRECT_HOPS",
    "DiscoveryAdapter",
    "DiscoveryCandidate",
    "DiscoveryResult",
    "HttpLinkDiscovery",
    "LinkDiscovery",
]

log = logging.getLogger(__name__)

MAX_REDIRECT_HOPS = 10
MAX_PAGE_BODY_BYTES = 5 * 1024 * 1024  # 5 MiB page-body cap
MAX_HEAD_REQUESTS = 15                 # hard cap on optional HEAD metadata calls
MAX_CANDIDATES = 500                   # hard cap on reported candidates
MAX_RETRY_AFTER_SECONDS = 30.0         # never wait longer than this on Retry-After
REDIRECT_STATUSES = frozenset({301, 302, 303, 307, 308})
_HEAD_HOPS = 3                         # redirect hops allowed per HEAD request


@dataclass
class DiscoveryCandidate:
    """One link the scanner reports as a plausible download candidate."""

    candidate_url: str
    filename: str
    extension: str | None
    source_page_url: str
    content_type: str | None = None
    size: int | None = None
    is_likely_file: bool = False


@dataclass
class DiscoveryResult:
    """Outcome of one page scan (scan problems never raise)."""

    source_url: str
    candidates: list[DiscoveryCandidate] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)
    elapsed_seconds: float = 0.0


class DiscoveryAdapter(Protocol):
    """Structural interface the facade (:class:`LinkDiscovery`) drives."""

    def scan(
        self,
        page_url: str,
        settings: Settings,
        cancel_token: threading.Event | None = None,
    ) -> DiscoveryResult:
        """Scan ``page_url`` and return candidates, errors and timing."""
        ...


class HttpLinkDiscovery:
    """HTTP(S) page scanner implementing :class:`DiscoveryAdapter`.

    Instantiate once and reuse: robots.txt verdicts are cached per origin
    and the underlying ``httpx.Client`` keeps connections alive. The
    instance is meant for serial use; a lock guards the bookkeeping.
    """

    def __init__(self) -> None:
        self._client: httpx.Client | None = None
        self._lock = threading.Lock()
        # origin -> robots verdict cache (or None when robots was absent/failing)
        self._robots_cache: dict[str, urllib.robotparser.RobotFileParser | None] = {}
        # host -> monotonic timestamp of the last optional HEAD request
        self._last_head_at: dict[str, float] = {}

    # ------------------------------------------------------------------ scan
    def scan(
        self,
        page_url: str,
        settings: Settings,
        cancel_token: threading.Event | None = None,
    ) -> DiscoveryResult:
        """Scan a page for downloadable-file candidates.

        All failure paths land on ``result.errors`` with actionable
        messages; this method only raises for programming errors, never
        for network/validation/robots problems.
        """
        started = time.monotonic()
        result = DiscoveryResult(source_url=page_url)
        try:
            self._scan_inner(page_url, settings, cancel_token, result)
        except _ScanError as exc:
            result.errors.append(str(exc))
        result.elapsed_seconds = time.monotonic() - started
        return result

    def _scan_inner(
        self,
        page_url: str,
        settings: Settings,
        cancel_token: threading.Event | None,
        result: DiscoveryResult,
    ) -> None:
        """Fetch the page, extract and optionally enrich candidates."""
        validated = security.validate_url_for_network(
            page_url, allow_private=settings.allow_private_addresses
        )
        result.source_url = validated.url
        if cancel_token is not None and cancel_token.is_set():
            raise _ScanCancelled("scan cancelled before any request was made")

        if settings.respect_rate_limit and not self._robots_allows(validated, settings):
            result.errors.append(
                f"robots.txt for {validated.scheme}://{validated.url.split('/', 3)[2]} "
                "disallows scanning this URL; ask the site owner for permission "
                "before using an automated scanner here"
            )
            return

        body, final_url = self._fetch_page(validated, settings, cancel_token)
        result.source_url = final_url

        candidates = self._build_candidates(
            self._extract_links(body, final_url, settings), final_url, settings
        )
        result.candidates = candidates

        if candidates and settings.respect_rate_limit:
            self._enrich_with_head(candidates, settings, cancel_token)

    # -------------------------------------------------------------- fetch page
    def _fetch_page(
        self,
        validated: security.ValidatedUrl,
        settings: Settings,
        cancel_token: threading.Event | None,
    ) -> tuple[bytes, str]:
        """GET the page through manually-validated redirects, capped at 5 MiB.

        Returns:
            ``(page_body_bytes, final_url_after_redirects)``.

        Raises:
            _ScanError: network failure, bad status, body over the cap,
                redirect loop, or a cancelled scan.
        """
        current = validated.url
        client = self._http_client(settings)
        for _hop in range(MAX_REDIRECT_HOPS + 1):
            _check_cancel(cancel_token)
            try:
                # ``client.stream()`` returns a context manager, not a
                # Response; the manual ``send(stream=True)`` API yields a
                # live Response that the ``finally`` below always closes.
                response = client.send(client.build_request("GET", current), stream=True)
            except (httpx.HTTPError, TimeoutError, ConnectionError) as exc:
                raise _ScanError(
                    f"could not fetch the page {security.redact_url(current)} "
                    f"({type(exc).__name__}: {exc}); check the address and your "
                    "connection, or raise settings.connection_timeout"
                ) from exc
            try:
                status = response.status_code
                if status in REDIRECT_STATUSES:
                    location = response.headers.get("location")
                    try:
                        validated = security.validate_redirect(
                            current,
                            location or "",
                            allow_private=settings.allow_private_addresses,
                        )
                    except UnsafeUrlError as exc:
                        raise _ScanError(
                            f"redirect refused for safety reasons: {exc}"
                        ) from exc
                    current = validated.url
                    continue
                if not 200 <= status < 300:
                    raise _ScanError(
                        f"page fetch failed with HTTP {status} for "
                        f"{security.redact_url(current)}; verify the page URL is "
                        "openable in a browser"
                    )
                return self._read_capped(response, cancel_token), current
            finally:
                response.close()
        raise _ScanError(
            f"page URL exceeds {MAX_REDIRECT_HOPS} redirects; it may be a redirect "
            "loop — open it in a browser to confirm"
        )

    def _read_capped(
        self, response: httpx.Response, cancel_token: threading.Event | None
    ) -> bytes:
        """Stream the body, aborting past :data:`MAX_PAGE_BODY_BYTES`."""
        chunks: list[bytes] = []
        received = 0
        try:
            for chunk in response.iter_bytes(chunk_size=65536):
                _check_cancel(cancel_token)
                received += len(chunk)
                if received > MAX_PAGE_BODY_BYTES:
                    raise _ScanError(
                        "page body exceeds the 5 MiB scan cap; oversize "
                        "documents are not scanned"
                    )
                chunks.append(chunk)
        except (httpx.HTTPError, TimeoutError, ConnectionError) as exc:
            raise _ScanError(
                f"page download interrupted ({type(exc).__name__}: {exc}); retry the scan"
            ) from exc
        return b"".join(chunks)

    # ---------------------------------------------------------------- parsing
    def _extract_links(
        self, body: bytes, page_url: str, settings: Settings
    ) -> Iterator[str]:
        """Yield unique, safety-filtered absolute URLs from anchor tags.

        First-seen order is preserved (ordered set via ``dict``). Only
        ``http``/``https`` links pass; links that fail structural parsing
        or the network-safety re-check (private/loopback refusal honouring
        ``settings.allow_private_addresses``) are dropped silently — they
        are non-candidates, not scan errors. The post-redirect page URL is
        the base for relative links, honouring ``<base href>``.
        """
        try:
            soup = BeautifulSoup(body, "html.parser")
        except Exception as exc:  # html.parser is forgiving; this is defensive
            raise _ScanError(
                f"page markup could not be parsed ({type(exc).__name__}: {exc}); "
                "the page may be binary or severely corrupt"
            ) from exc

        base = page_url
        base_tag = soup.find("base", href=True)
        if base_tag is not None and str(base_tag["href"]).strip():
            try:
                base = urllib.parse.urljoin(page_url, str(base_tag["href"]).strip())
            except ValueError:
                base = page_url

        seen: dict[str, None] = {}
        for anchor in soup.find_all("a", href=True):
            href = str(anchor["href"]).strip()
            if not href:
                continue
            try:
                absolute = urllib.parse.urldefrag(urllib.parse.urljoin(base, href))[0]
            except ValueError:
                continue
            if not absolute or absolute in seen:
                continue
            try:
                parsed = urllib.parse.urlsplit(absolute)
            except ValueError:
                continue
            if parsed.scheme.lower() not in ("http", "https") or not parsed.hostname:
                continue
            try:
                validated = security.validate_url_for_network(
                    absolute, allow_private=settings.allow_private_addresses
                )
            except (UnsafeUrlError, ValueError):
                continue
            if validated.url in seen:
                continue
            seen[validated.url] = None
            yield validated.url

    def _build_candidates(
        self, links: Iterator[str], page_url: str, settings: Settings
    ) -> list[DiscoveryCandidate]:
        """Turn raw links into capped, classified candidates."""
        candidates: list[DiscoveryCandidate] = []
        for candidate_url in links:
            if len(candidates) >= MAX_CANDIDATES:
                log.warning("link discovery capped at %d candidates", MAX_CANDIDATES)
                break
            ext = filename_utils.classify_extension(candidate_url)
            likely = (
                ext in settings.allowed_extensions
                or (settings.include_unknown_types and ext is not None)
            )
            candidates.append(
                DiscoveryCandidate(
                    candidate_url=candidate_url,
                    filename=filename_utils.filename_from_url(candidate_url),
                    extension=ext,
                    source_page_url=page_url,
                    is_likely_file=likely,
                )
            )
        return candidates

    # -------------------------------------------------------- optional HEADs
    def _enrich_with_head(
        self,
        candidates: list[DiscoveryCandidate],
        settings: Settings,
        cancel_token: threading.Event | None,
    ) -> None:
        """Optionally fill ``content_type``/``size`` via bounded HEAD calls.

        Politeness: requests are spaced out with a per-host inter-request
        gap of ``settings.retry_delay`` seconds (``Settings`` has no
        dedicated politeness knob; ``retry_delay`` doubles as the
        rate-limit courtesy). A ``Retry-After`` up to
        :data:`MAX_RETRY_AFTER_SECONDS` is honoured once per request; a
        slower source simply keeps its unknown metadata. Only
        ``is_likely_file`` candidates are probed, at most
        :data:`MAX_HEAD_REQUESTS` calls per scan.
        """
        limit = MAX_HEAD_REQUESTS
        for candidate in candidates:
            if limit <= 0:
                break
            if not candidate.is_likely_file:
                continue
            _check_cancel(cancel_token)
            host = urllib.parse.urlsplit(candidate.candidate_url).hostname or ""
            self._wait_politely(host, settings)
            try:
                content_type, size = self._head_request(
                    candidate.candidate_url, settings, cancel_token
                )
            except (httpx.HTTPError, TimeoutError, ConnectionError) as exc:
                # metadata is best-effort: never fail the scan over it
                log.debug(
                    "HEAD metadata unavailable for %s: %s",
                    security.redact_url(candidate.candidate_url), exc,
                )
                continue
            candidate.content_type = content_type
            candidate.size = size
            limit -= 1
            self._last_head_at[host] = time.monotonic()

    def _wait_politely(self, host: str, settings: Settings) -> None:
        """Sleep out the remaining per-host politeness gap before the next call."""
        if settings.retry_delay <= 0:
            return
        last = self._last_head_at.get(host)
        if last is None:
            return
        remaining = settings.retry_delay - (time.monotonic() - last)
        if remaining > 0:
            time.sleep(remaining)

    def _head_request(
        self,
        url: str,
        settings: Settings,
        cancel_token: threading.Event | None,
    ) -> tuple[str | None, int | None]:
        """One bounded, re-validated HEAD request: ``(content_type, size)``.

        Network exceptions propagate to the (best-effort) caller.
        """
        client = self._http_client(settings)
        current = url
        for _hop in range(_HEAD_HOPS + 1):
            _check_cancel(cancel_token)
            response = client.head(current)
            try:
                status = response.status_code
                if status in REDIRECT_STATUSES:
                    location = response.headers.get("location") or ""
                    try:
                        current = security.validate_redirect(
                            current, location, allow_private=settings.allow_private_addresses
                        ).url if location else ""
                    except (UnsafeUrlError, ValueError):
                        return None, None
                    if not current:
                        return None, None
                    continue
                if status in (429, 503):
                    retry_after = _parse_retry_after(response.headers.get("Retry-After"))
                    if retry_after is not None and retry_after <= MAX_RETRY_AFTER_SECONDS:
                        time.sleep(retry_after)  # one polite retry, same hop budget
                        continue
                if 200 <= status < 300:
                    return (
                        _content_type(response.headers.get("Content-Type")),
                        _content_size(response.headers),
                    )
                return None, None
            finally:
                response.close()
        return None, None

    # ----------------------------------------------------------------- robots
    def _robots_allows(
        self, validated: security.ValidatedUrl, settings: Settings
    ) -> bool:
        """Consult (and cache) robots.txt for the page origin.

        Returns:
            ``True`` when scanning is allowed (or no verdict exists). A
            failed robots fetch is permissive — many sites simply 403 the
            file, which shall not block a human-initiated scan of an
            openable page.

        Raises:
            _ScanCancelled: when the scan is cancelled.
        """
        origin = urllib.parse.urlunsplit(
            (validated.scheme, f"{validated.host}:{validated.port}", "", "", "")
        )
        with self._lock:
            known = origin in self._robots_cache
            cached = self._robots_cache.get(origin)
        if not known:
            cached = self._fetch_robots(origin, settings)
            with self._lock:
                self._robots_cache[origin] = cached
        if cached is None:
            return True  # no verdict available -> permissive
        return cached.can_fetch("*", validated.url) or cached.can_fetch(
            str(settings.user_agent), validated.url
        )

    def _fetch_robots(
        self, origin: str, settings: Settings
    ) -> urllib.robotparser.RobotFileParser | None:
        """Fetch + parse ``origin/robots.txt``.

        Returns:
            A parser, or ``None`` when the file is unreachable/disallowed
            statuses (permissive: no verdict == allowed).
        """
        robots_url = urllib.parse.urljoin(origin, "/robots.txt")
        client = self._http_client(settings)
        try:
            response = client.get(robots_url)
        except (httpx.HTTPError, TimeoutError, ConnectionError, OSError):
            return None
        try:
            if not 200 <= response.status_code < 300:
                return None
            parser = urllib.robotparser.RobotFileParser()
            parser.parse(response.text.splitlines())
            return parser
        finally:
            response.close()

    # ----------------------------------------------------------------- shared
    def _http_client(self, settings: Settings) -> httpx.Client:
        """One shared client per adapter instance (lazy creation)."""
        if self._client is None:
            timeout = httpx.Timeout(
                settings.connection_timeout,
                connect=settings.connection_timeout,
                read=max(settings.connection_timeout, 30.0),
                write=settings.connection_timeout,
            )
            self._client = httpx.Client(
                timeout=timeout,
                follow_redirects=False,  # manual loops re-validate every hop
                headers={"User-Agent": settings.user_agent, "Accept": "*/*"},
            )
        return self._client

    @property
    def robots_verdicts(self) -> dict[str, urllib.robotparser.RobotFileParser | None]:
        """Origin->verdict cache (documented reuse: share one adapter)."""
        with self._lock:
            return dict(self._robots_cache)


class _ScanError(Exception):
    """Internal: the scan cannot continue; the message is user-actionable."""


class _ScanCancelled(_ScanError):
    """Internal: cooperative cancel during a scan."""

    def __init__(self, message: str = "scan cancelled by the user") -> None:
        super().__init__(message)


def _check_cancel(cancel_token: threading.Event | None) -> None:
    """Raise :class:`_ScanCancelled` when the token is set (``None`` = never)."""
    if cancel_token is not None and cancel_token.is_set():
        raise _ScanCancelled()


def _content_type(header_value: str | None) -> str | None:
    """Media type with parameters stripped, or ``None``."""
    value = (header_value or "").split(";", 1)[0].strip().lower()
    return value or None


def _content_size(headers: httpx.Headers) -> int | None:
    """Non-negative ``Content-Length`` as ``int``, or ``None``."""
    raw = headers.get("Content-Length")
    if raw is None:
        return None
    try:
        size = int(raw)
    except (TypeError, ValueError):
        return None
    return size if size >= 0 else None


def _parse_retry_after(value: str | None) -> float | None:
    """Numeric or HTTP-date ``Retry-After`` in seconds, or ``None``."""
    if value is None or not value.strip():
        return None
    text = value.strip()
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


# == facade ================================================================

class LinkDiscovery:
    """Facade picking the discovery adapter (default: :class:`HttpLinkDiscovery`).

    Extend by passing any object satisfying :class:`DiscoveryAdapter` —
    e.g. a future FTP/metadata-site adapter — without touching callers.
    """

    def __init__(self, adapter: DiscoveryAdapter | None = None) -> None:
        """Create the facade; ``None`` selects the HTTP adapter."""
        self._adapter: DiscoveryAdapter = adapter if adapter is not None else HttpLinkDiscovery()

    @property
    def adapter(self) -> DiscoveryAdapter:
        """The active adapter."""
        return self._adapter

    def scan(
        self,
        page_url: str,
        settings: Settings,
        cancel_token: threading.Event | None = None,
    ) -> DiscoveryResult:
        """Scan ``page_url`` through the configured adapter."""
        return self._adapter.scan(page_url, settings, cancel_token)
