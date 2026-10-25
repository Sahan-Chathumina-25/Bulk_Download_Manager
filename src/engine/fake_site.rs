//! Minimal loopback HTTP/1.1 fixture used by the engine integration tests.
//!
//! Deliberately hand-rolled instead of pulling in a web framework: the tests
//! need exact, deterministic control over status codes, `Content-Length`
//! mismatches, chunked framing, `Retry-After`, redirect chains, and partial
//! content. Every response is `Connection: close`; requests are read until
//! the header terminator.
//!
//! Routes:
//!
//! | path | behaviour |
//! |------|-----------|
//! | `/files/<name>` | range-capable file (200/206/416) |
//! | `/norange/<name>` | always a full 200, no `Accept-Ranges` |
//! | `/etag/<name>` | range-capable file with a stable `ETag` |
//! | `/chunked/<name>` | file sent with `Transfer-Encoding: chunked` |
//! | `/status/<code>` | synthetic status code |
//! | `/retry-after` | 429 with `Retry-After: 1` |
//! | `/redirect/<n>` | `n` chained 302s ending at `/files/data.bin` |
//! | `/redirect-ftp` | 302 to an `ftp://` URL |
//! | `/redirect-private` | 302 to a link-local metadata address |
//! | `/slow` | 100 bytes in ten 10-byte chunks, 50 ms apart |
//! | `/html-as-file` | 200 `text/html` landing page |
//! | `/mismatch` | `Content-Length` larger than the body actually sent |
//! | `/page` | HTML page linking to files, pages and fragments |
//! | `/robots.txt` | configurable robots file (404 by default) |
//! | `/huge` | serves the file named `huge` (set by the test) |
//! | `/stall` | sleeps 2 s then responds 200 |
//! | anything else | 404 |
//!
//! `HEAD` requests are answered headers-only (with the full `Content-Length`)
//! and count towards [`FakeSite::hits`] alongside `GET`.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One in-process HTTP server shared by a test.
pub struct FakeSite {
    addr: SocketAddr,
    files: Mutex<HashMap<String, Vec<u8>>>,
    etags: Mutex<HashMap<String, String>>,
    robots: Mutex<Option<String>>,
    hits: Mutex<HashMap<String, usize>>,
    inflight: AtomicUsize,
    max_inflight: AtomicUsize,
}

impl FakeSite {
    /// Bind an ephemeral loopback port and start serving.
    pub async fn spawn() -> Arc<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fake site");
        let addr = listener.local_addr().expect("fake site address");
        let site = Arc::new(Self {
            addr,
            files: Mutex::new(HashMap::new()),
            etags: Mutex::new(HashMap::new()),
            robots: Mutex::new(None),
            hits: Mutex::new(HashMap::new()),
            inflight: AtomicUsize::new(0),
            max_inflight: AtomicUsize::new(1),
        });
        site.set_file(
            "data.bin",
            (0u8..=255).cycle().take(2048).collect::<Vec<u8>>(),
        );
        site.set_file(
            "small.zip",
            [b"PK\x03\x04".as_slice(), &[b'Z'; 64]].concat(),
        );
        let mut big = Vec::with_capacity(16_384);
        for _ in 0..16 {
            for _ in 0..64 {
                big.extend_from_slice(b"0123456789abcdef");
            }
        }
        site.set_file("big.zip", big);

        let serving = Arc::clone(&site);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _peer)) = listener.accept().await else {
                    continue;
                };
                let site = Arc::clone(&serving);
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(10),
                        handle_connection(site, stream),
                    )
                    .await;
                });
            }
        });
        site
    }

    /// Base URL for `path` (a leading `/` is optional).
    pub fn url(&self, path: &str) -> String {
        format!("http://{}/{}", self.addr, path.trim_start_matches('/'))
    }

    /// Install or replace a file body.
    pub fn set_file(&self, name: &str, bytes: impl Into<Vec<u8>>) {
        lock(&self.files).insert(name.to_string(), bytes.into());
    }

    /// Current body for `name`.
    pub fn file_bytes(&self, name: &str) -> Option<Vec<u8>> {
        lock(&self.files).get(name).cloned()
    }

    /// Pin the `ETag` served for `name`.
    pub fn set_etag(&self, name: &str, etag: &str) {
        lock(&self.etags).insert(name.to_string(), etag.to_string());
    }

    /// Serve `content` at `/robots.txt` (default is a 404).
    pub fn set_robots(&self, content: &str) {
        *lock(&self.robots) = Some(content.to_string());
    }

    /// Requests seen for `path` (query string ignored).
    pub fn hits(&self, path: &str) -> usize {
        let key = normalise_path(path);
        lock(&self.hits).get(&key).copied().unwrap_or(0)
    }

    /// Reset the hit counters.
    pub fn clear_hits(&self) {
        lock(&self.hits).clear();
    }

    /// Highest number of requests this fixture served concurrently.
    pub fn max_parallel(&self) -> usize {
        self.max_inflight.load(Ordering::SeqCst)
    }

    fn record_hit(&self, path: &str) {
        *lock(&self.hits).entry(path.to_string()).or_insert(0) += 1;
    }

    fn begin_request(&self) {
        let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_inflight.fetch_max(now, Ordering::SeqCst);
    }

    fn end_request(&self) {
        self.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn normalise_path(path: &str) -> String {
    let stripped = path.split(['?', '#']).next().unwrap_or(path);
    if stripped.starts_with('/') {
        stripped.to_string()
    } else {
        format!("/{stripped}")
    }
}

/// Parsed request head.
struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    fn is_head(&self) -> bool {
        self.method.eq_ignore_ascii_case("HEAD")
    }
}

async fn handle_connection(site: Arc<FakeSite>, mut stream: TcpStream) -> io::Result<()> {
    site.begin_request();
    let result = serve(&site, &mut stream).await;
    site.end_request();
    result
}

async fn serve(site: &Arc<FakeSite>, stream: &mut TcpStream) -> io::Result<()> {
    let Some(request) = read_request(stream).await? else {
        return Ok(());
    };
    site.record_hit(&request.path);
    route(site, stream, &request).await
}

async fn read_request(stream: &mut TcpStream) -> io::Result<Option<Request>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_headers_end(&buffer) {
            break Some(index);
        }
        if buffer.len() > 64 * 1024 {
            break None;
        }
    };
    let Some(head_end) = head_end else {
        return Ok(None);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]);
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or("/");
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Ok(Some(Request {
        method: method.to_string(),
        path: normalise_path(target),
        headers,
    }))
}

fn find_headers_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

async fn route(site: &Arc<FakeSite>, stream: &mut TcpStream, request: &Request) -> io::Result<()> {
    let path = request.path.as_str();

    if let Some(name) = path.strip_prefix("/files/") {
        return serve_file(site, stream, request, name, true, false).await;
    }
    if let Some(name) = path.strip_prefix("/norange/") {
        return serve_file(site, stream, request, name, false, false).await;
    }
    if let Some(name) = path.strip_prefix("/etag/") {
        return serve_file(site, stream, request, name, true, true).await;
    }
    if let Some(name) = path.strip_prefix("/chunked/") {
        return serve_chunked(site, stream, name).await;
    }
    if let Some(name) = path.strip_prefix("/flaky/") {
        // The hit counter already includes this request: fail the first two
        // attempts so a retrying worker succeeds on the third.
        if site.hits(path) <= 2 {
            return write_response(
                stream,
                503,
                "Service Unavailable",
                &[("Content-Type", "text/plain".to_string())],
                b"try again",
            )
            .await;
        }
        return serve_file(site, stream, request, name, true, false).await;
    }
    if let Some(code) = path.strip_prefix("/status/") {
        return serve_status(stream, code).await;
    }
    if path == "/retry-after" {
        return write_response(
            stream,
            429,
            "Too Many Requests",
            &[
                ("Content-Type", "text/plain".to_string()),
                ("Retry-After", "1".to_string()),
            ],
            b"too many requests",
        )
        .await;
    }
    if let Some(step) = path.strip_prefix("/redirect/") {
        let step = step.parse::<u32>().unwrap_or(0);
        let location = if step <= 1 {
            "/files/data.bin".to_string()
        } else {
            format!("/redirect/{}", step - 1)
        };
        return write_response(stream, 302, "Found", &[("Location", location)], b"").await;
    }
    if path == "/redirect-ftp" {
        return write_response(
            stream,
            302,
            "Found",
            &[("Location", "ftp://evil.example/x".to_string())],
            b"",
        )
        .await;
    }
    if path == "/redirect-private" {
        return write_response(
            stream,
            302,
            "Found",
            &[(
                "Location",
                "http://169.254.169.254/latest/meta-data/".to_string(),
            )],
            b"",
        )
        .await;
    }
    if path == "/page" {
        return serve_page(site, stream).await;
    }
    if path == "/robots.txt" {
        let body = lock(&site.robots).clone();
        return match body {
            Some(content) => {
                write_response(
                    stream,
                    200,
                    "OK",
                    &[("Content-Type", "text/plain; charset=utf-8".to_string())],
                    content.as_bytes(),
                )
                .await
            }
            None => {
                write_response(
                    stream,
                    404,
                    "Not Found",
                    &[("Content-Type", "text/plain".to_string())],
                    b"no robots.txt",
                )
                .await
            }
        };
    }
    if path == "/huge" {
        return serve_file(site, stream, request, "huge", false, false).await;
    }
    if path == "/stall" {
        return serve_stall(stream).await;
    }
    if path == "/slow" {
        return serve_slow(stream).await;
    }
    if path == "/html-as-file" {
        return write_response(
            stream,
            200,
            "OK",
            &[("Content-Type", "text/html; charset=utf-8".to_string())],
            b"<html><body><h1>this is html, not a zip</h1></body></html>",
        )
        .await;
    }
    if path == "/mismatch" {
        let body = b"short-body";
        return write_raw(
            stream,
            b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 110\r\nConnection: close\r\n\r\n",
            body,
        )
        .await;
    }
    write_response(
        stream,
        404,
        "Not Found",
        &[("Content-Type", "text/plain".to_string())],
        b"not found",
    )
    .await
}

async fn serve_file(
    site: &Arc<FakeSite>,
    stream: &mut TcpStream,
    request: &Request,
    name: &str,
    ranges: bool,
    with_etag: bool,
) -> io::Result<()> {
    let name = percent_encoding::percent_decode_str(name)
        .decode_utf8_lossy()
        .into_owned();
    let Some(body) = site.file_bytes(&name) else {
        return write_response(
            stream,
            404,
            "Not Found",
            &[("Content-Type", "text/plain".to_string())],
            b"no such file",
        )
        .await;
    };

    let etag = if with_etag {
        let stored = lock(&site.etags).get(&name).cloned();
        Some(stored.unwrap_or_else(|| format!("\"{name}-{}\"", body.len())))
    } else {
        lock(&site.etags).get(&name).cloned()
    };

    if request.is_head() {
        let mut headers = vec![("Content-Type", content_type_for(&name).to_string())];
        if ranges {
            headers.push(("Accept-Ranges", "bytes".to_string()));
        }
        if let Some(etag) = &etag {
            headers.push(("ETag", etag.clone()));
        }
        return write_head_response(stream, 200, "OK", &headers, body.len() as u64).await;
    }

    let content_type = content_type_for(&name);

    if let (Some(stored), Some(if_range)) = (&etag, request.header("if-range"))
        && trim_quotes(if_range) != trim_quotes(stored)
    {
        return write_file_response(stream, 200, &body, content_type, ranges, etag.as_deref())
            .await;
    }

    if !ranges {
        return write_file_response(stream, 200, &body, content_type, false, etag.as_deref()).await;
    }

    let total = body.len() as u64;
    match parse_range(request.header("range"), total) {
        RangeRequest::Full => {
            write_file_response(stream, 200, &body, content_type, true, etag.as_deref()).await
        }
        RangeRequest::Unsatisfiable => {
            let mut headers = vec![("Content-Range", format!("bytes */{total}"))];
            if let Some(etag) = &etag {
                headers.push(("ETag", etag.clone()));
            }
            write_response(stream, 416, "Range Not Satisfiable", &headers, b"").await
        }
        RangeRequest::Partial { start, end } => {
            let slice = &body[start as usize..=end as usize];
            let mut headers = vec![
                ("Content-Range", format!("bytes {start}-{end}/{total}")),
                ("Accept-Ranges", "bytes".to_string()),
            ];
            if let Some(etag) = &etag {
                headers.push(("ETag", etag.clone()));
            }
            write_response(stream, 206, "Partial Content", &headers, slice).await
        }
    }
}

async fn write_file_response(
    stream: &mut TcpStream,
    status: u16,
    body: &[u8],
    content_type: &str,
    accept_ranges: bool,
    etag: Option<&str>,
) -> io::Result<()> {
    let mut headers = vec![("Content-Type", content_type.to_string())];
    if accept_ranges {
        headers.push(("Accept-Ranges", "bytes".to_string()));
    }
    if let Some(etag) = etag {
        headers.push(("ETag", etag.to_string()));
    }
    let reason = if status == 206 {
        "Partial Content"
    } else {
        "OK"
    };
    write_response(stream, status, reason, &headers, body).await
}

/// Content type inferred from a fixture filename (enough for HEAD tests).
fn content_type_for(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("zip") => "application/zip",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("bin") => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

async fn serve_page(site: &Arc<FakeSite>, stream: &mut TcpStream) -> io::Result<()> {
    let port = site.addr.port();
    let page = format!(
        concat!(
            "<!doctype html><html><head><title>fixture page</title></head><body>",
            "<a href=\"/files/data.bin\">data (relative)</a>",
            "<a href=\"/files/data.bin\">data (relative again)</a>",
            "<a href=\"http://127.0.0.1:{port}/files/data.bin\">data (absolute)</a>",
            "<a href=\"#intro\">intro fragment</a>",
            "<a href=\"/page#top\">top of this page</a>",
            "<a href=\"/about\">about</a>",
            "<a href=\"/downloads/report.zip\">report</a>",
            "<a href=\"/docs/manual.pdf\">manual</a>",
            "<a href=\"/img/photo.png\">photo</a>",
            "<a href=\"/files/big.zip\">big</a>",
            "</body></html>"
        ),
        port = port
    );
    write_response(
        stream,
        200,
        "OK",
        &[("Content-Type", "text/html; charset=utf-8".to_string())],
        page.as_bytes(),
    )
    .await
}

async fn serve_stall(stream: &mut TcpStream) -> io::Result<()> {
    tokio::time::sleep(Duration::from_secs(2)).await;
    write_response(
        stream,
        200,
        "OK",
        &[("Content-Type", "text/plain".to_string())],
        b"finally",
    )
    .await
}

async fn write_head_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(&str, String)],
    content_length: u64,
) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {content_length}\r\nConnection: close\r\n\r\n"
    ));
    write_raw(stream, head.as_bytes(), b"").await
}

async fn serve_chunked(site: &Arc<FakeSite>, stream: &mut TcpStream, name: &str) -> io::Result<()> {
    let name = percent_encoding::percent_decode_str(name)
        .decode_utf8_lossy()
        .into_owned();
    let Some(body) = site.file_bytes(&name) else {
        return write_response(
            stream,
            404,
            "Not Found",
            &[("Content-Type", "text/plain".to_string())],
            b"no such file",
        )
        .await;
    };
    let head = b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    write_raw(stream, head, b"").await?;
    for chunk in body.chunks(1024) {
        stream
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .await?;
        stream.write_all(chunk).await?;
        stream.write_all(b"\r\n").await?;
    }
    stream.write_all(b"0\r\n\r\n").await?;
    stream.flush().await
}

async fn serve_status(stream: &mut TcpStream, code: &str) -> io::Result<()> {
    let Ok(code) = code.parse::<u16>() else {
        return write_response(
            stream,
            404,
            "Not Found",
            &[("Content-Type", "text/plain".to_string())],
            b"not found",
        )
        .await;
    };
    let body = format!("synthetic status {code}");
    let mut headers = vec![("Content-Type", "text/plain".to_string())];
    if code == 401 {
        headers.push(("WWW-Authenticate", "Basic realm=\"testrealm\"".to_string()));
    }
    write_response(stream, code, "Synthetic", &headers, body.as_bytes()).await
}

async fn serve_slow(stream: &mut TcpStream) -> io::Result<()> {
    let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 100\r\nConnection: close\r\n\r\n";
    write_raw(stream, head, b"").await?;
    for _ in 0..10 {
        stream.write_all(b"0123456789").await?;
        stream.flush().await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    write_raw(stream, head.as_bytes(), body).await
}

async fn write_raw(stream: &mut TcpStream, head: &[u8], body: &[u8]) -> io::Result<()> {
    stream.write_all(head).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }
    stream.flush().await
}

fn trim_quotes(value: &str) -> &str {
    value.trim().trim_matches('"')
}

/// Parsed single-range request.
#[derive(Debug, PartialEq, Eq)]
enum RangeRequest {
    Full,
    Partial { start: u64, end: u64 },
    Unsatisfiable,
}

fn parse_range(raw: Option<&str>, total: u64) -> RangeRequest {
    let Some(value) = raw else {
        return RangeRequest::Full;
    };
    let text = value.trim();
    if total == 0 || !text.to_ascii_lowercase().starts_with("bytes=") {
        return RangeRequest::Full;
    }
    let spec = text[6..].trim();
    if spec.is_empty() || spec.contains(',') {
        return RangeRequest::Full;
    }
    let Some((start_text, end_text)) = spec.split_once('-') else {
        return RangeRequest::Full;
    };
    let start_text = start_text.trim();
    let end_text = end_text.trim();

    if start_text.is_empty() {
        let Ok(suffix) = end_text.parse::<u64>() else {
            return RangeRequest::Full;
        };
        if suffix == 0 {
            return RangeRequest::Full;
        }
        let start = total.saturating_sub(suffix);
        return RangeRequest::Partial {
            start,
            end: total - 1,
        };
    }

    let Ok(start) = start_text.parse::<u64>() else {
        return RangeRequest::Full;
    };
    if end_text.is_empty() {
        if start >= total {
            return RangeRequest::Unsatisfiable;
        }
        return RangeRequest::Partial {
            start,
            end: total - 1,
        };
    }
    let Ok(end) = end_text.parse::<u64>() else {
        return RangeRequest::Full;
    };
    if start >= total || end < start {
        return RangeRequest::Full;
    }
    RangeRequest::Partial {
        start,
        end: end.min(total - 1),
    }
}
