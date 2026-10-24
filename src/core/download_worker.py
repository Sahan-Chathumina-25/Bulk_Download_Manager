"""Single-download streaming engine.

httpx streaming to ``<dest>.part`` with:

- manual redirect loop (max hops, every hop re-validated for SSRF safety),
- Accept-Ranges detection and HTTP 206 resume with ``Range`` + ``If-Range``,
- restart-from-zero when the server ignores Range or the validators changed
  (the ``.part`` file is truncated — bytes are never appended across a gap),
- throttled :class:`DownloadProgress` callbacks (~4/s) with speed and ETA,
- retry/backoff decisions delegated to :mod:`src.core.retry_policy`,
- HTML-instead-of-file detection, Content-Length verification and optional
  SHA-256 verification,
- cooperative cancel/pause through :class:`threading.Event`.

The module never imports GUI code and never touches the database directly —
persistence happens through an injected repository (may be ``None``).
"""

from __future__ import annotations

import contextlib
import hashlib
import logging
import os
import pathlib
import threading
import time
import urllib.parse
from collections.abc import Callable

import httpx

from src.core import filename_utils, security
from src.core.retry_policy import (
    AuthRequiredError,
    DownloadCancelled,
    DownloadError,
    DownloadFailed,
    DownloadPaused,
    RetryPolicy,
    TransientHttpError,
)
from src.core.security import UnsafeUrlError
from src.database.models import DownloadProgress, DownloadRecord, DownloadState, Settings
from src.database.repository import DownloadRepository

__all__ = [
    "MAX_REDIRECT_HOPS",
    "PROGRESS_INTERVAL_SECONDS",
    "DownloadWorker",
]

log = logging.getLogger(__name__)

MAX_REDIRECT_HOPS = 10
PROGRESS_INTERVAL_SECONDS = 0.25  # ≈ 4 progress callbacks per second
_MAX_DELAY_SECONDS = 60.0

_REDIRECT_STATUSES = frozenset({301, 302, 303, 307, 308})


class _FreshRestart(Exception):
    """Internal: the response cannot extend the .part file; start over."""


class _PartComplete(Exception):
    """Internal: the .part file already holds the entire remote file.

    Carries the authoritative total size (from ``Content-Range``) so the
    outer loop can finalize via ``os.replace`` instead of restarting.
    """

    def __init__(self, total: int) -> None:
        super().__init__(f"the .part file already holds all {total} bytes")
        self.total = total


class DownloadWorker:
    """Runs one download to completion; ``run()`` blocks until settled."""

    def __init__(
        self,
        record: DownloadRecord,
        settings: Settings,
        *,
        repository: DownloadRepository | None = None,
        on_progress: Callable[[DownloadProgress], None] | None = None,
        on_state_change: Callable[[DownloadRecord], None] | None = None,
        cancel_event: threading.Event | None = None,
        pause_event: threading.Event | None = None,
        policy: RetryPolicy | None = None,
    ) -> None:
        """Create a worker for ``record``.

        Args:
            record: record whose ``source_url`` and ``destination_path`` are
                used (``destination_path`` must be a full, already-sanitised
                path; the caller owns path-traversal defence).
            settings: validated application settings.
            repository: optional persistence sink (None = dry run).
            on_progress: called from the worker thread ~4 times per second.
            on_state_change: called on every lifecycle transition.
            cancel_event: cooperative cancel signal.
            pause_event: cooperative pause signal (honouring it depends on
                server range support; without ranges pausing cancels instead).
            policy: retry/backoff policy override (tests).
        """
        self._rec = record
        self._s = settings
        self._repo = repository
        self._on_progress = on_progress
        self._on_state_change = on_state_change
        self._cancel = cancel_event or threading.Event()
        self._pause = pause_event or threading.Event()
        self._policy = policy or RetryPolicy(
            max_retries=settings.retry_count,
            base_delay=min(settings.retry_delay, _MAX_DELAY_SECONDS),
            max_delay=max(settings.retry_delay, _MAX_DELAY_SECONDS),
        )
        self._client: httpx.Client | None = None
        self._part_path = (
            pathlib.Path(record.destination_path + ".part")
            if record.destination_path
            else None
        )
        # rate/progress tracking
        self._last_emit = 0.0
        self._emit_baseline_bytes = 0
        self._bytes_now = 0

    # ------------------------------------------------------------------ API
    def run(self) -> DownloadRecord:
        """Execute the full lifecycle and return the settled record.

        ``run()`` never raises for pause/cancel/permanent-download-failure:
        those are reflected on the record's ``state``/``error_summary``.
        Collision outcomes are terminal but not failures: ``keep_existing``
        with an existing final file settles as ``COMPLETED`` and ``skip``
        settles as ``CANCELLED`` with an actionable explanation.
        """
        try:
            self._run_inner()
            self._set_state(DownloadState.COMPLETED)
            self._rec.completed_at = self._utc_now()
        except DownloadPaused:
            self._set_state(DownloadState.PAUSED)
        except DownloadCancelled:
            self._set_state(DownloadState.CANCELLED, msg=None)
        except _KeepExisting:
            self._settle_keep_existing()
        except _Skipped as exc:
            self._rec.state = DownloadState.CANCELLED
            self._rec.error_summary = str(exc)
            log.info("download %s skipped: %s", self._rec.id, exc)
        except AuthRequiredError as exc:
            self._fail(str(exc))
        except DownloadError as exc:
            self._fail(str(exc))
        except UnsafeUrlError as exc:
            self._fail(f"unsafe URL refused: {exc}")
        except Exception as exc:  # unexpected — still must settle the record
            log.exception("unexpected error in download %s", self._rec.id)
            self._fail(f"internal error: {exc}")
        finally:
            with contextlib.suppress(Exception):
                if self._client is not None:
                    self._client.close()
        self._persist()
        return self._rec

    # ------------------------------------------------------------- internals
    def _run_inner(self) -> None:
        self._prepare_destination()
        resume_offset = self._resume_offset()
        attempt = 0
        while True:
            self._check_user_stop()
            self._set_state(DownloadState.CONNECTING)
            try:
                response = self._open_stream(resume_offset)
            except _PartComplete as exc:
                self._resume_and_finalize(exc.total)
                return
            except _FreshRestart:  # pragma: no cover - defensive
                resume_offset = 0
                continue
            except (httpx.HTTPError, TransientHttpError, TimeoutError, ConnectionError) as exc:
                delay = self._transient_delay(exc, attempt)
                if delay is None:
                    raise self._to_permanent(exc) from exc
                log.warning(
                    "download %s: transient error (%s); retrying in %.1fs (attempt %d)",
                    self._rec.id, exc, delay, attempt + 1,
                )
                attempt += 1
                self._rec.retry_count = attempt
                self._persist()
                self._sleep(delay)
                continue
            try:
                self._set_state(DownloadState.DOWNLOADING)
                self._transfer(response, resume_offset)
                return
            except _PartComplete as exc:
                response.close()
                self._resume_and_finalize(exc.total)
                return
            except _FreshRestart:
                response.close()
                resume_offset = 0  # never append across a gap: truncate + retry
                attempt = 0
                continue
            except (httpx.HTTPError, TransientHttpError, TimeoutError, ConnectionError) as exc:
                response.close()
                delay = self._transient_delay(exc, attempt)
                if delay is None:
                    raise self._to_permanent(exc) from exc
                attempt += 1
                self._rec.retry_count = attempt
                self._persist()
                self._sleep(delay)

    def _prepare_destination(self) -> None:
        """Validate the URL, honour collision policy, wire up the paths."""
        self._validated = security.validate_url_for_network(
            self._rec.source_url, allow_private=self._s.allow_private_addresses
        )
        final = pathlib.Path(self._rec.destination_path) if self._rec.destination_path else None
        if final is None or final.name == "":
            raise DownloadFailed("download has no destination path")
        final.parent.mkdir(parents=True, exist_ok=True)
        self._final_path = final
        self._part_path = final.with_name(final.name + ".part")

        fresh = not self._part_exists() or self._part_size() <= 0
        if fresh:
            self._apply_collision_policy(final)

    def _apply_collision_policy(self, final: pathlib.Path) -> None:
        resolved = filename_utils.unique_path(final, self._s.filename_collision)
        if resolved is final or resolved == final:
            if resolved.exists() and self._s.filename_collision == "overwrite":
                return  # caller overwrites; proceed
            if resolved.exists() and self._s.filename_collision == "keep_existing":
                # existing file is authoritative: nothing to download
                self._rec.downloaded_bytes = resolved.stat().st_size
                raise _KeepExisting()
            return
        if resolved is None:
            raise _Skipped(final)
        self._final_path = resolved
        self._rec.destination_path = str(resolved)
        self._part_path = resolved.with_name(resolved.name + ".part")
        self._rec.display_name = resolved.name

    def _resume_offset(self) -> int:
        """Bytes to resume from: the authoritative size of the .part file."""
        offset = 0
        if self._part_path is not None and self._part_path.is_file():
            offset = self._part_path.stat().st_size
        if self._rec.downloaded_bytes != offset:
            self._rec.downloaded_bytes = offset
            self._persist()
        return offset

    # -- request construction --------------------------------------------
    def _open_stream(self, resume_offset: int) -> httpx.Response:
        """GET through redirects; return an open response for a 2xx status.

        Every failure path is explicit:

        - a network error while opening propagates to the retry loop in
          :meth:`_run_inner` (the retry policy applies),
        - a redirect response is always closed, then re-validated via
          :meth:`_follow_redirect` before the next hop,
        - every non-redirect status is closed and mapped onto a concrete
          error (auth required, not found, transient, permanent),
        - HTTP 416 with a ``.part`` that already covers the remote size
          raises :class:`_PartComplete` so the caller finalizes instead of
          restart-looping; any other 416 is a restart signal.

        The caller owns ``close()`` on the returned response.
        """
        current = self._validated.url
        client = self._http_client()
        for _hop in range(MAX_REDIRECT_HOPS + 1):
            self._check_user_stop()
            headers = self._request_headers(resume_offset)
            try:
                request = client.build_request("GET", current, headers=headers)
                # ``client.stream()`` returns a context manager, not a
                # Response; the manual ``send(stream=True)`` API yields a
                # live Response the caller owns and closes explicitly.
                response = client.send(request, stream=True)
            except (httpx.HTTPError, TimeoutError, ConnectionError) as exc:
                # No response was produced: there is no stream to close.
                log.warning(
                    "download %s: could not open stream to %s (%s)",
                    self._rec.id, security.redact_url(current), exc,
                )
                raise
            # ``response`` is bound from here on; every branch below must
            # either close it or return it to the caller (2xx case).
            status = response.status_code
            if status in _REDIRECT_STATUSES:
                location = response.headers.get("location")
                response.close()
                self._follow_redirect(current, location)
                current = self._validated.url
                continue
            if status == 401 or status == 403:
                response.close()
                raise AuthRequiredError(
                    f"server requires authentication (HTTP {status}); "
                    "configure credentials/cookies for this source"
                )
            if status in (404, 410):
                response.close()
                raise DownloadFailed(f"file not found on server (HTTP {status})")
            if status == 416 and resume_offset > 0:
                total = self._total_from_416(response)
                response.close()
                if total is not None and resume_offset == total:
                    raise _PartComplete(total)  # part already covers the file
                raise _FreshRestart()  # part incomplete: retry without Range
            if status in {408, 425, 429, 500, 502, 503, 504}:
                retry_after = self._parse_retry_after(response)
                response.close()
                raise TransientHttpError(status, retry_after=retry_after)
            if not 200 <= status < 300:
                response.close()
                raise DownloadFailed(f"unexpected HTTP status {status}")
            return response
        raise DownloadFailed(
            f"too many redirects (more than {MAX_REDIRECT_HOPS}); "
            "the source may have a redirect loop"
        )

    def _follow_redirect(self, current: str, location: str | None) -> None:
        """Re-validate every redirect hop (SSRF + downgrade protection)."""
        try:
            self._validated = security.validate_redirect(
                current, location or "", allow_private=self._s.allow_private_addresses
            )
        except UnsafeUrlError as exc:
            raise DownloadFailed(f"unsafe redirect target refused: {exc}") from exc

    def _request_headers(self, resume_offset: int) -> dict[str, str]:
        headers = {
            "User-Agent": self._s.user_agent,
            "Accept-Encoding": "identity",
        }
        if resume_offset > 0:
            headers["Range"] = f"bytes={resume_offset}-"
            validator = self._rec.etag or self._rec.last_modified
            if validator:
                headers["If-Range"] = validator
        return headers

    def _http_client(self) -> httpx.Client:
        if self._client is None:
            timeout = httpx.Timeout(
                self._s.connection_timeout,
                connect=self._s.connection_timeout,
                read=self._s.connection_timeout,
                write=self._s.connection_timeout,
            )
            self._client = httpx.Client(
                timeout=timeout,
                follow_redirects=False,  # manual loop re-validates every hop
                headers={"Accept": "*/*"},
            )
        return self._client

    # -- transfer ----------------------------------------------------------
    def _transfer(self, response: httpx.Response, resume_offset: int) -> None:
        """Stream the response body into ``.part``; finalize on success."""
        part = self._part_path
        assert part is not None
        content_type = (response.headers.get("Content-Type") or "").strip()

        header_length = self._header_total(response)
        is_resume_response = response.status_code == 206

        if is_resume_response:
            self._validate_206(response, resume_offset)
            # On 206 the Content-Length is the REMAINING size, not the total
            # file size; the authoritative total (when known) comes from
            # Content-Range and was stored in expected_size just above.
            verify_length = header_length
        elif resume_offset > 0:
            # Server ignored our Range (200): restart truncated, never append.
            log.info("download %s: server ignored Range; restarting from zero", self._rec.id)
            raise _FreshRestart()
        else:
            verify_length = header_length

        accept_ranges = is_resume_response or "bytes" in (
            response.headers.get("Accept-Ranges") or ""
        ).lower()

        self._check_html(response, content_type)
        self._capture_validators(response)
        self._rec.content_type = content_type or None
        progress_total = self._rec.expected_size if is_resume_response else header_length
        if not is_resume_response and header_length is not None:
            self._rec.expected_size = header_length

        append_mode = "ab" if (is_resume_response and resume_offset > 0) else "wb"
        digest = hashlib.sha256()
        received = 0
        got_first_chunk = False
        started = time.monotonic()
        try:
            with open(part, append_mode) as fh:
                for chunk in response.iter_bytes(self._s.buffer_size):
                    self._check_user_stop(accept_ranges=accept_ranges)
                    if not got_first_chunk:
                        got_first_chunk = True
                        if resume_offset == 0 and self._looks_like_html(chunk):
                            response.close()
                            raise DownloadFailed(
                                "the server returned an HTML page instead of the "
                                "requested file; the link may be a landing/login "
                                "page — open it in a browser"
                            )
                    fh.write(chunk)
                    digest.update(chunk)
                    received += len(chunk)
                    self._maybe_throttle(started, received)
                    self._maybe_emit_progress(
                        total=progress_total,
                        done=resume_offset + received,
                        speed_done=received,
                        t_started=started,
                    )
        except (httpx.LocalProtocolError, httpx.RemoteProtocolError) as exc:
            # httpcore raises RemoteProtocolError when the peer shuts the
            # connection down before the declared body length was fully
            # received. That is a Content-Length mismatch, not a generic
            # network failure: report it as one, but leave any other
            # protocol error untouched for the retry/network-error path.
            if (
                "peer closed connection without sending complete message body"
                not in str(exc).lower()
            ):
                raise
            response.close()
            if verify_length is not None and verify_length > received:
                raise DownloadFailed(
                    f"size mismatch: received {received} bytes, expected "
                    f"{verify_length} (Content-Length mismatch: the server "
                    "closed the connection early)"
                ) from exc
            raise DownloadFailed(
                "size mismatch: the server closed the connection before "
                f"the complete message body ({received} bytes received); "
                f"{exc}"
            ) from exc
        if verify_length is not None and received != verify_length:
            # exact byte-count verification against Content-Length
            raise DownloadFailed(
                f"server sent {received} bytes but promised {verify_length} "
                "(Content-Length mismatch)"
            )
        response.close()

        expected_digest = self._normalize_checksum(self._rec.checksum_sha256)
        if expected_digest is not None:
            actual = digest.hexdigest()
            if actual != expected_digest:
                raise DownloadFailed(
                    f"SHA-256 checksum mismatch: expected {expected_digest}, got {actual}; "
                    "the file is corrupt or the server delivered different content"
                )
            self._rec.checksum_verified = True

        total_size = resume_offset + received
        self._finalize(part, self._final_path, total_size)
        self._rec.downloaded_bytes = total_size
        self._emit_progress(
            total=progress_total,
            done=total_size,
            speed_bps=self._instance_speed(total_size, started),
            eta=0,
        )

    def _validate_206(self, response: httpx.Response, resume_offset: int) -> None:
        """Check ``Content-Range`` offset consistency for a resumed transfer.

        Raises:
            _FreshRestart: the server acknowledged a resume but the
                ``Content-Range`` offset disagrees with the local ``.part``
                (overlap or gap) — appending would corrupt the file.
            _PartComplete: the offset already equals the total — the local
                ``.part`` holds the entire file and must be finalized
                instead of transferred again.
        """
        raw = response.headers.get("Content-Range", "")
        start = self._content_range_start(raw)
        if start is None:
            log.warning("download %s: unusable Content-Range %r", self._rec.id, raw)
            raise _FreshRestart()
        if start != resume_offset:
            # Overlap or gap: resuming would corrupt the file -> truncate.
            log.warning(
                "download %s: 206 offset %d != expected %d; restarting",
                self._rec.id, start, resume_offset,
            )
            raise _FreshRestart()
        total = self._content_range_total(raw)
        if total is not None and total > 0:
            self._rec.expected_size = total
            if resume_offset == total:
                raise _PartComplete(total)

    def _finalize(self, part: pathlib.Path, final: pathlib.Path, size: int) -> None:
        """Publish the partial file over the final name (atomic on success)."""
        final.parent.mkdir(parents=True, exist_ok=True)
        os.replace(part, final)
        actual = final.stat().st_size
        if actual != size:
            raise DownloadFailed(
                f"final file size {actual} != downloaded {size} bytes (filesystem error)"
            )

    def _resume_and_finalize(self, total: int) -> None:
        """The ``.part`` already holds the whole file: publish it atomically.

        Called for HTTP 416 / Content-Range outcomes where the resume offset
        equals the remote size — no bytes are transferred again.
        """
        part = self._part_path
        final = self._final_path
        assert part is not None and final is not None
        actual = part.stat().st_size
        if actual != total:
            raise DownloadFailed(
                f"resume covers the whole file but the .part holds {actual} "
                f"bytes (server reported {total}); restarting download"
            )
        log.info(
            "download %s: resume covers the whole file (%d bytes); finalizing",
            self._rec.id, total,
        )
        self._finalize(part, final, total)
        self._rec.downloaded_bytes = total

    # -- helpers -----------------------------------------------------------
    def _check_user_stop(self, *, accept_ranges: bool = False) -> None:
        """Raise DownloadPaused/DownloadCancelled on cooperative signals."""
        if self._cancel.is_set():
            self._persist()
            raise DownloadCancelled()
        if self._pause.is_set():
            if accept_ranges:
                self._rec.downloaded_bytes = self._part_size()
                self._persist()
                raise DownloadPaused()
            # pause requires server range support; without it, cancel and
            # leave the .part file in place (retry can restart from zero)
            self._persist()
            raise DownloadCancelled()

    def _sleep(self, seconds: float) -> None:
        """Interruptible backoff sleep honouring cancel/pause."""
        deadline = time.monotonic() + max(0.0, seconds)
        while time.monotonic() < deadline:
            self._check_user_stop()
            time.sleep(min(0.1, max(0.0, deadline - time.monotonic())))

    def _transient_delay(self, exc: Exception, attempt: int) -> float | None:
        """Retry decision (None = do not retry)."""
        should, delay = self._policy.should_retry(exc, attempt)
        return delay if should else None

    def _to_permanent(self, exc: Exception) -> DownloadFailed:
        """Turn an unretryable exception into an actionable DownloadFailed."""
        if isinstance(exc, AuthRequiredError | DownloadFailed):
            return exc
        return DownloadFailed(f"network failure: {exc}")

    @staticmethod
    def _parse_retry_after(response: httpx.Response) -> float | None:
        value = response.headers.get("Retry-After")
        if value is None:
            return None
        try:
            return max(0.0, float(value))
        except (TypeError, ValueError):
            return None

    @staticmethod
    def _normalize_checksum(value: str | None) -> str | None:
        """Accept ``sha256:<hex>`` or bare hex (64 chars) input."""
        if not value:
            return None
        text = value.strip().lower()
        if text.startswith("sha256:"):
            text = text.split(":", 1)[1].strip()
        return text if len(text) == 64 and all(c in "0123456789abcdef" for c in text) else None

    @staticmethod
    def _utc_now() -> str:
        from datetime import UTC, datetime
        return datetime.now(tz=UTC).isoformat(timespec="seconds")

    def _part_size(self) -> int:
        assert self._part_path is not None
        try:
            return self._part_path.stat().st_size
        except FileNotFoundError:
            return 0

    def _part_exists(self) -> bool:
        return self._part_path is not None and self._part_path.is_file()

    # -- response header helpers -------------------------------------------
    @staticmethod
    def _looks_like_html(chunk: bytes) -> bool:
        """True when the leading ``chunk`` bytes look like an HTML document.

        Second line of defence against HTML error pages returned with a
        missing/lying ``Content-Type`` header. Only the very start of the
        body is inspected, so binary files are never misclassified in
        practice.
        """
        head = chunk.lstrip(b"\xef\xbb\xbf \t\r\n")[:1024].lower()
        return head.startswith(b"<!doctype html") or head.startswith(b"<html")

    @staticmethod
    def _total_from_416(response: httpx.Response) -> int | None:
        """Remote file size from a 416's ``Content-Range: bytes */<total>``."""
        raw = (response.headers.get("Content-Range") or "").strip()
        total = DownloadWorker._content_range_total(raw)
        return total if total is not None and total > 0 else None

    @staticmethod
    def _header_total(response: httpx.Response) -> int | None:
        raw = response.headers.get("Content-Length")
        if raw is None:
            return None
        try:
            value = int(raw)
        except (TypeError, ValueError):
            return None
        return value if value >= 0 else None

    @staticmethod
    def _content_range_start(raw: str) -> int | None:
        value = DownloadWorker._content_range_parts(raw)
        if not value:
            return None
        start_end, total = value
        try:
            return int(start_end.split("-")[0])
        except (TypeError, ValueError):
            return None

    @staticmethod
    def _content_range_total(raw: str) -> int | None:
        value = DownloadWorker._content_range_parts(raw)
        if not value:
            return None
        _start_end, total = value
        if total == "*":
            return None
        try:
            return int(total)
        except (TypeError, ValueError):
            return None

    @staticmethod
    def _content_range_parts(raw: str) -> tuple[str, str] | None:
        text = (raw or "").strip()
        if not text.lower().startswith("bytes "):
            return None
        body = text.split(" ", 1)[1]
        if "/" not in body:
            return None
        span, _, total = body.partition("/")
        return span.strip(), total.strip()

    def _capture_validators(self, response: httpx.Response) -> None:
        etag = response.headers.get("ETag")
        last_modified = response.headers.get("Last-Modified")
        if etag:
            self._rec.etag = etag
        if last_modified:
            self._rec.last_modified = last_modified

    def _check_html(self, response: httpx.Response, content_type: str) -> None:
        """Refuse HTML pages (wrong link / login page) before writing bytes."""
        lowered = content_type.split(";")[0].strip().lower()
        if lowered == "text/html" or lowered.startswith("text/html;"):
            raise DownloadFailed(
                "the server returned an HTML page instead of the requested file; "
                "the link may be a landing/login page — open it in a browser"
            )

    def _maybe_throttle(self, started: float, received: int) -> None:
        if self._s.bandwidth_limit_bps <= 0:
            return
        elapsed = time.monotonic() - started
        target = received / self._s.bandwidth_limit_bps
        if target > elapsed:
            time.sleep(min(target - elapsed, 5.0))

    # -- progress + state ----------------------------------------------------
    def _maybe_emit_progress(
        self, *, total: int | None, done: int, speed_done: int, t_started: float
    ) -> None:
        now = time.monotonic()
        if now - self._last_emit >= PROGRESS_INTERVAL_SECONDS:
            self._last_emit = now
            self._emit_progress(
                total=total,
                done=done,
                speed_bps=self._instance_speed(speed_done, t_started),
                eta=self._eta(total, done, speed_done, t_started),
            )

    def _instance_speed(self, done_bytes: int, t_started: float) -> float:
        elapsed = time.monotonic() - t_started
        return done_bytes / elapsed if elapsed > 0 else 0.0

    def _eta(
        self, total: int | None, done: int, speed_done: int, t_started: float
    ) -> int | None:
        if total is None or total <= done:
            return 0 if total is not None else None
        speed = self._instance_speed(speed_done, t_started)
        if speed <= 0:
            return None
        return int((total - done) / speed)

    def _emit_progress(
        self, *, total: int | None, done: int, speed_bps: float, eta: int | None
    ) -> None:
        if self._on_progress is None:
            return
        snapshot = DownloadProgress(
            download_id=int(self._rec.id or -1),
            downloaded_bytes=done,
            total_bytes=total,
            speed_bps=speed_bps,
            eta_seconds=eta,
        )
        with contextlib.suppress(Exception):
            self._on_progress(snapshot)

    def _set_state(self, state: DownloadState, msg: str | None = "") -> None:
        self._rec.state = state
        self._persist()
        if self._on_state_change is not None:
            with contextlib.suppress(Exception):
                self._on_state_change(self._rec)

    def _persist(self) -> None:
        if self._repo is not None and self._rec.id is not None:
            with contextlib.suppress(Exception):
                self._repo.update(self._rec)

    def _fail(self, message: str) -> None:
        self._rec.state = DownloadState.FAILED
        self._rec.error_summary = message
        log.error("download %s failed: %s", self._rec.id, message)

    def _settle_keep_existing(self) -> None:
        """Terminal handler for the ``keep_existing`` collision outcome.

        The existing file on disk is authoritative: when it is still there
        the download settles as ``COMPLETED`` with its real size; only a
        vanished file falls back to ``FAILED``.
        """
        final = (
            pathlib.Path(self._rec.destination_path)
            if self._rec.destination_path
            else None
        )
        if final is not None and final.is_file():
            self._rec.downloaded_bytes = final.stat().st_size
            self._rec.state = DownloadState.COMPLETED
            self._rec.completed_at = self._utc_now()
            self._rec.error_summary = None
            log.info(
                "download %s: existing file kept (keep_existing); marked completed",
                self._rec.id,
            )
        else:
            self._fail(
                "collision policy is 'keep_existing' but the existing file is "
                "no longer present on disk; retry to download it again"
            )


class _KeepExisting(DownloadError):
    """Internal: keep_existing policy hit an already-present file."""


class _Skipped(DownloadError):
    """Internal: skip policy declined to download over an existing file."""

    def __init__(self, path: pathlib.Path) -> None:
        super().__init__(
            f"destination already exists ({path.name}) and the collision policy "
            "is 'skip'; choose another policy to proceed"
        )
