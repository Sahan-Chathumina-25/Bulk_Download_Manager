"""Shared pytest infrastructure: local HTTP fixture server, settings, database.

The ``http_server`` fixture runs a stdlib ``ThreadingHTTPServer`` bound to
``127.0.0.1`` on a random free port. It simulates a scripted download site
with the behaviours the engine has to cope with (Range/206, ETag/If-Range,
redirects, slow streams, malformed responses, an HTML "page" for link
discovery).
"""

from __future__ import annotations

import re
import threading
import time
import urllib.parse
from collections.abc import Iterator
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from src.database.connection import Database, init_db
from src.database.models import Settings

__all__ = ["FAKE_FILES", "FakeSite", "db", "http_server", "settings"]


# Deterministic file payloads served by the fake site. Tests compare against
# ``http_server.file_bytes(name)`` so they never need this dict directly.
FAKE_FILES: dict[str, bytes] = {
    "data.bin": bytes(range(256)) * 8,  # 2048 bytes, every byte value
    "small.zip": b"PK\x03\x04" + b"Z" * 64,  # 68 bytes
    "big.zip": (b"0123456789abcdef" * 64) * 16,  # 16384 bytes
}

_CONTENT_TYPES: dict[str, str] = {
    ".zip": "application/zip",
    ".pdf": "application/pdf",
    ".png": "image/png",
    ".bin": "application/octet-stream",
}

_HTML_AS_FILE_BODY = b"<html><body><h1>this is html, not a zip</h1></body></html>"

_PAGE_TEMPLATE = """<!doctype html>
<html><head><title>downloads page</title></head><body>
<h1 id="intro">Files</h1>
<a href="/files/data.bin">relative file link</a>
<a href="{root}/files/data.bin">absolute file link</a>
<a href="/files/data.bin">duplicate file link</a>
<a href="#intro">same-page fragment</a>
<a href="/page#top">other page fragment</a>
<a href="/about">plain page link</a>
<a href="/downloads/report.zip">zip link</a>
<a href="/docs/manual.pdf">pdf link</a>
<a href="/img/photo.png">png link</a>
<a href="/files/big.zip">another zip link</a>
</body></html>
"""

_RANGE_RE = re.compile(r"bytes=(\d*)-(\d*)$")
#: Well-formed ``bytes=<start>-`` Range whose start lies at/past EOF.
_EOF_RANGE_RE = re.compile(r"bytes=(\d+)-\s*$")


def _content_type(name: str) -> str:
    """Best-effort Content-Type for a stored file name."""
    suffix = "." + name.rsplit(".", 1)[-1].lower() if "." in name else ""
    return _CONTENT_TYPES.get(suffix, "application/octet-stream")


def _parse_range(value: str, total: int) -> tuple[int, int] | None:
    """Parse a single-range ``bytes=s-e``/``s-``/``-n`` header (inclusive end).

    Returns ``(start, end)`` clamped to the file, or ``None`` when the header
    is malformed / unsatisfiable (the caller then serves a full 200 response).
    """
    match = _RANGE_RE.match(value.strip())
    if not match:
        return None
    start_s, end_s = match.groups()
    if start_s:
        start = int(start_s)
        if start >= total:
            return None
        end = int(end_s) if end_s else total - 1
        return (start, min(end, total - 1))
    if end_s:
        n = int(end_s)
        if n == 0:
            return None
        return (max(0, total - n), total - 1)
    return None


class FakeSite(ThreadingHTTPServer):
    """Threaded local HTTP server faking a bulk-download site."""

    daemon_threads = True
    allow_reuse_address = True

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), _FakeSiteHandler)
        self.port: int = self.server_address[1]  # type: ignore[index]
        self._state_lock = threading.Lock()
        self._files: dict[str, bytes] = dict(FAKE_FILES)
        self._etags: dict[str, str] = {}
        self._hits: dict[str, int] = {}
        self._active = 0
        self._max_parallel = 0

    # -- test-facing API ----------------------------------------------------

    def url(self, path: str = "/") -> str:
        """Absolute URL for ``path`` (path may include a query string)."""
        if not path.startswith("/"):
            path = "/" + path
        return f"http://127.0.0.1:{self.port}{path}"

    def set_file(self, name: str, data: bytes) -> None:
        """Add or replace a file served under ``/files/<name>``."""
        with self._state_lock:
            self._files[name] = bytes(data)

    def file_bytes(self, name: str) -> bytes:
        """The exact bytes currently served for ``name``."""
        with self._state_lock:
            return bytes(self._files[name])

    def set_etag(self, name: str, val: str) -> None:
        """Override the ETag served for ``/etag/<name>``."""
        with self._state_lock:
            self._etags[name] = val

    def hits(self, path: str) -> int:
        """Number of GET/HEAD requests received for ``path`` (query ignored)."""
        key = path if path.startswith("/") else "/" + path
        with self._state_lock:
            return self._hits.get(key, 0)

    def clear_hits(self) -> None:
        """Zero the hit counters (e.g. between queue phases)."""
        with self._state_lock:
            self._hits.clear()

    @property
    def max_parallel(self) -> int:
        """Maximum number of simultaneously active request handlers so far."""
        with self._state_lock:
            return self._max_parallel

    # -- internals -----------------------------------------------------------

    def handle_error(self, request: object, client_address: object) -> None:
        """Stay quiet: clients aborting mid-download is an expected scenario."""
        return None


class _FakeSiteHandler(BaseHTTPRequestHandler):
    """Request router implementing the scripted endpoints."""

    server: FakeSite  # narrow the type from BaseHTTPRequestHandler's generic

    def log_message(self, format: str, *args: object) -> None:  # noqa: A002
        """Silence per-request logging."""
        return None

    # -- HTTP verbs ----------------------------------------------------------

    def do_GET(self) -> None:
        self._dispatch(head=False)

    def do_HEAD(self) -> None:
        self._dispatch(head=True)

    def _dispatch(self, *, head: bool) -> None:
        """Count the hit, occupy a parallel slot, then route the request."""
        srv = self.server
        path = urllib.parse.urlsplit(self.path).path
        with srv._state_lock:
            srv._hits[path] = srv._hits.get(path, 0) + 1
        with self._parallel_slot():
            try:
                self._route(path, head=head)
            except (BrokenPipeError, ConnectionResetError):
                pass  # client hung up mid-response; expected in cancel tests
            except Exception:
                try:
                    self.send_error(500, "internal error")
                except Exception:  # noqa: S110 - diagnostics are best-effort
                    pass

    @contextmanager
    def _parallel_slot(self) -> Iterator[None]:
        """Context manager tracking concurrent active handlers."""
        srv = self.server
        with srv._state_lock:
            srv._active += 1
            srv._max_parallel = max(srv._max_parallel, srv._active)
        try:
            yield
        finally:
            with srv._state_lock:
                srv._active -= 1

    # -- routing --------------------------------------------------------------

    def _route(self, path: str, *, head: bool) -> None:
        if path.startswith("/files/"):
            self._serve_stored_file(path, prefix="/files/", allow_range=True, head=head)
        elif path.startswith("/norange/"):
            self._serve_stored_file(path, prefix="/norange/", allow_range=False, head=head)
        elif path.startswith("/etag/"):
            self._serve_etag_file(path, head=head)
        elif path.startswith("/status/"):
            self._serve_status(path, head=head)
        elif path == "/retry-after":
            self._send(
                429,
                b"too many requests",
                headers=[("Retry-After", "1")],
                head=head,
            )
        elif path.startswith("/redirect/"):
            self._serve_redirect_chain(path, head=head)
        elif path == "/redirect-private":
            self._send(
                302,
                b"",
                headers=[("Location", "http://169.254.169.254/latest/meta-data/")],
                head=head,
            )
        elif path == "/slow":
            self._serve_slow(head=head)
        elif path == "/html-as-file" or path.startswith("/html-as-file/"):
            self._send(
                200,
                _HTML_AS_FILE_BODY,
                content_type="text/html; charset=utf-8",
                head=head,
            )
        elif path == "/mismatch":
            self._serve_mismatch(head=head)
        elif path == "/page":
            self._serve_page(head=head)
        else:
            self._send(404, b"not found", head=head)

    # -- endpoint implementations ----------------------------------------------

    def _serve_stored_file(
        self, path: str, *, prefix: str, allow_range: bool, head: bool
    ) -> None:
        """Serve ``/files/<name>`` (206-capable) or ``/norange/<name>`` (200-full)."""
        name = urllib.parse.unquote(path[len(prefix):])
        data = self.server._files.get(name)  # noqa: SLF001 - same module family
        if data is None:
            self._send(404, b"no such file", head=head)
            return
        self._stream(data, name=name, etag=None, allow_range=allow_range, head=head)

    def _serve_etag_file(self, path: str, *, head: bool) -> None:
        """Serve ``/etag/<name>`` with ETag header; a mismatching If-Range -> 200 full."""
        name = urllib.parse.unquote(path[len("/etag/"):])
        data = self.server._files.get(name)  # noqa: SLF001
        if data is None:
            self._send(404, b"no such file", head=head)
            return
        etag = self.server._etags.get(name) or f'"{name}-{len(data)}"'
        if not etag.startswith('"'):
            etag = f'"{etag}"'
        allow_range = True
        if_range = self.headers.get("If-Range")
        if if_range is not None and if_range.strip('"') != etag.strip('"'):
            allow_range = False  # stale cached variant: must re-send everything
        self._stream(data, name=name, etag=etag, allow_range=allow_range, head=head)

    def _serve_status(self, path: str, *, head: bool) -> None:
        """Serve ``/status/<code>`` with a synthetic reason body."""
        tail = path.rsplit("/", 1)[-1]
        status = int(tail) if tail.isdigit() else 404
        headers: list[tuple[str, str]] = []
        if status == 401:
            headers.append(("WWW-Authenticate", 'Basic realm="testrealm"'))
        self._send(
            status,
            f"synthetic status {status}".encode("ascii"),
            headers=headers,
            head=head,
        )

    def _serve_redirect_chain(self, path: str, *, head: bool) -> None:
        """``/redirect/<n>`` chains n redirects, then lands on ``/files/data.bin``."""
        tail = path[len("/redirect/"):]
        n = int(tail) if tail.isdigit() else 0
        target = "/files/data.bin" if n <= 1 else f"/redirect/{n - 1}"
        self._send(302, b"", headers=[("Location", target)], head=head)

    def _serve_slow(self, *, head: bool) -> None:
        """Serve 100 bytes in 10-byte chunks with small sleeps between them."""
        payload = b"0123456789" * 10
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        if head:
            return
        step = 10
        for i in range(0, len(payload), step):
            self.wfile.write(payload[i : i + step])
            self.wfile.flush()
            if i + step < len(payload):
                time.sleep(0.05)

    def _serve_mismatch(self, *, head: bool) -> None:
        """Serve a body shorter than the declared Content-Length (truncation)."""
        body = b"short-body"
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(body) + 100))  # lie on purpose
        self.end_headers()
        if head:
            return
        self.wfile.write(body)

    def _serve_page(self, *, head: bool) -> None:
        """Serve the HTML discovery page with mixed anchor types."""
        page = _PAGE_TEMPLATE.format(root=f"http://127.0.0.1:{self.server.port}")
        self._send(200, page.encode("utf-8"), content_type="text/html; charset=utf-8", head=head)

    # -- response helpers -------------------------------------------------------

    def _stream(
        self,
        data: bytes,
        *,
        name: str,
        etag: str | None,
        allow_range: bool,
        head: bool,
    ) -> None:
        """Send ``data`` honouring Range (206 + Content-Range) when allowed."""
        total = len(data)
        rng = self.headers.get("Range") if allow_range else None
        window = _parse_range(rng, total) if rng else None
        if window is None and rng is not None and _EOF_RANGE_RE.match(rng.strip()):
            # A well-formed start-position Range at/past EOF is unsatisfiable
            # (RFC 9110): real servers answer 416 with the authoritative size.
            self._send(
                416,
                b"range not satisfiable",
                headers=[("Content-Range", f"bytes */{total}")],
                head=head,
            )
            return
        if window is None:
            headers: list[tuple[str, str]] = []
            if etag is not None:
                headers.append(("ETag", etag))
            if allow_range:
                headers.append(("Accept-Ranges", "bytes"))
            self._send(200, data, headers=headers, content_type=_content_type(name), head=head)
            return
        start, end = window
        body = data[start : end + 1]
        self.send_response(206)
        if etag is not None:
            self.send_header("ETag", etag)
        self.send_header("Content-Type", _content_type(name))
        self.send_header("Content-Range", f"bytes {start}-{end}/{total}")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if not head:
            self.wfile.write(body)

    def _send(
        self,
        status: int,
        body: bytes,
        *,
        headers: list[tuple[str, str]] | None = None,
        content_type: str = "text/plain; charset=utf-8",
        head: bool = False,
    ) -> None:
        """Send a complete response with the given status/body/extra headers."""
        self.send_response(status)
        for name, value in headers or []:
            self.send_header(name, value)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if not head and body:
            self.wfile.write(body)


# -- fixtures -------------------------------------------------------------------


@pytest.fixture()
def http_server() -> Iterator[FakeSite]:
    """A live :class:`FakeSite` listening on 127.0.0.1 (random free port)."""
    site = FakeSite()
    thread = threading.Thread(
        target=site.serve_forever, name="fake-site-http", daemon=True
    )
    thread.start()
    try:
        yield site
    finally:
        site.shutdown()
        site.server_close()
        thread.join(timeout=5)


@pytest.fixture()
def settings(tmp_path: Path) -> Settings:
    """Engine-friendly Settings: loopback allowed, fast retries, 3 workers."""
    return Settings(
        default_destination_dir=str(tmp_path / "downloads"),
        max_simultaneous=3,
        connection_timeout=5.0,
        retry_count=3,
        retry_delay=0.05,
        buffer_size=8192,
        allow_private_addresses=True,
    )


@pytest.fixture()
def db(tmp_path: Path) -> Iterator[Database]:
    """A Phase-1 schema-initialised SQLite database in a temp directory."""
    database = init_db(tmp_path / "downloads.db")
    try:
        yield database
    finally:
        database.close()
