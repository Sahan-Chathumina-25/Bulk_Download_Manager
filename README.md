# Bulk Download Manager

A Windows desktop application for queuing, managing, and verifying bulk HTTP/HTTPS file downloads — built with Python and PySide6.

## Overview

Bulk Download Manager (BDM) is a local, offline-friendly download manager. You add URLs manually or discover them by scanning a webpage, queue them into a persistent SQLite-backed download list, and the engine downloads them concurrently with streaming-to-disk, resume support, checksum verification, and strict URL safety checks. All user data stays on your machine; nothing is uploaded anywhere.

What it is **not**: a flood/rapidshare-style multi-connection downloader, a download accelerator, or an integration with any file-hosting service. It downloads plain direct HTTP/HTTPS links only (see [Supported hosting services](#supported-hosting-services)).

## Features

- **Concurrent downloads** — a thread pool with configurable concurrency (default 3, clamped 1–10), no duplicate scheduling of the same item.
- **Streaming downloads** — data is written to `<destination>.part` while downloading and renamed on completion; no buffering whole files in memory.
- **Manual redirects with re-validation** — redirects (up to 10 hops) are followed hop-by-hop and each hop is safety-checked; HTTPS→HTTP downgrade is refused.
- **Resume support** — HTTP `Range` + `If-Range` (ETag / Last-Modified), with `206` + `Content-Range` offset validation. A gap or overlap in the existing `.part` file truncates and restarts rather than producing a corrupt file. If the server ignores `Range` (full `200`), the download restarts from byte 0.
- **Integrity verification** — `Content-Length` byte-exact verification and optional SHA-256 checksums (`sha256:<hex>` or bare 64-char hex).
- **Transient error handling** — exponential backoff for `408/425/429/500/502/503/504` honoring `Retry-After`; `401/403` raises an auth-required error; `404/410` fail immediately without retry.
- **Collision policies** — `keep_existing`, `rename`, `overwrite`, `skip` (skip fails at add time).
- **Bandwidth throttling** and cooperative pause (pause is only possible when byte ranges are known; otherwise the item is cancelled and the `.part` file kept — see [Pausing / resuming](#pausing-resuming--limitations)).
- **Webpage link discovery** — scan a page URL for file links (with `robots.txt` awareness and optional HEAD pre-checks), review candidates, and queue the ones you select. Nothing is ever queued automatically.
- **Persistent state + crash recovery** — SQLite (WAL mode) tracks `queued/connecting/downloading/paused/completed/failed/cancelled/interrupted`; on startup the database is reconciled against the files on disk and interrupted items can be resumed.
- **URL safety** — SSRF-safe by default: loopback, private, link-local, and metadata addresses are rejected, DNS resolution is fail-closed, redirects are re-validated, credentials in URLs are refused, and log URLs are redacted.
- **Dark-themed PySide6 UI** with add/discover/queue/history/settings pages, empty states, and keyboard shortcuts (`Ctrl+1` … `Ctrl+6`, `Ctrl+Q`).

## Technology stack

| Component | Version |
|---|---|
| Python | >= 3.12 (the development venv uses 3.14) |
| PySide6 (Qt GUI) | 6.12.0 |
| httpx (streaming HTTP client) | 0.28.1 |
| BeautifulSoup4 (link discovery) | 4.15.0 |
| SQLite (stdlib, WAL mode) | — |
| pytest (tests) | 9.1.1 |
| PyInstaller (packaging) | >= 6.0 (installed by `build.ps1` on demand) |

## Requirements

- Windows (the provided scripts are PowerShell; the Python code itself is standard-library portable).
- Python 3.12 or newer.
- No internet access is required to run the tests; they use a local test HTTP server only.

## Setup

### Using the script

```powershell
.\scripts\setup.ps1
```

This finds a Python ≥ 3.12 installation on your system, creates (or reuses and validates) a `.venv` in the repository root, and installs `requirements.txt` into it. Output is colored, and failure messages include actionable follow-ups.

### Manual alternative

```powershell
py -3.12 -m venv .venv
.\.venv\Scripts\python.exe -m pip install -r requirements.txt
```

Any Python ≥ 3.12 works; adjust the launcher command to match your installation.

## Running the application

```powershell
.\scripts\run.ps1
```

This launches `.venv\Scripts\python.exe run.py` from the repository root and propagates the exit code; it complains clearly if the venv does not exist yet (run `setup.ps1` first).

Equivalently, with the venv activated:

```powershell
python run.py
```

`run.py` starts `src.main:main`. On startup, the app reconciles the database against the files on disk and, if any downloads are marked interrupted, shows a recovery prompt offering to resume them.

## Using the application

The left sidebar switches between six pages:

**Dashboard** — at-a-glance status of the queue.

### Add Downloads

- Paste one or many URLs into the multiline text box.
- **Import TXT** loads a text file of URLs. Both CRLF and LF line endings are accepted; tabs are tolerated; `url<TAB>friendly-name` pairs are supported.
- A summary dialog reports how many URLs were accepted, skipped as duplicates, and rejected — each rejection with its reason. Unsafe URLs produce per-URL warnings.
- A **custom filename** and/or **SHA-256 checksum** can be entered only when adding a single URL (they cannot be mapped onto a bulk paste).

### Discover

Scan a webpage URL for downloadable links: the scan runs on a worker thread and fills a candidate table (checkbox, filename, type, size, source page, URL). Use the search filter, the *include unknown type* checkbox, and select-all/none; then add the selected candidates. Reviewing the candidates is a required manual step — the scanner never queues anything by itself. See [Webpage link discovery](#webpage-link-discovery) for limits and behavior.

### Queue

The live download table: filename, host, size, downloaded, %, speed, ETA, status, destination. Actions: Start All, Pause, Resume, Retry, Cancel, Remove, Open Folder, Details.

## Managing the download queue

- **Start All** begins every eligible queued item, respecting the max-simultaneous setting from the previous application start (changing it requires a restart).
- **Pause / Resume / Retry / Cancel / Remove** apply to individual items. Retry is available for failed (and, from History, previously failed) items; Cancel stops the worker and keeps the `.part` file.
- Speed and ETA are shown per item; the engine also tracks aggregate speed (`total_speed_bps`) used by the dashboard.
- Closing the window while downloads are active asks for confirmation, then cancels and shuts the engine down cleanly.
- Duplicate scheduling is prevented: the same download id cannot be queued twice.

## Pausing / resuming — limitations

- Pausing is **cooperative**: the worker finishes the in-flight read and stops only when the next byte range is known. If the current connection has no validated range (e.g., the server does not support ranges), a "pause" degrades to a cancel that leaves the `.part` file in place so it can still be resumed later if ranges become known.
- Resume uses `Range` + `If-Range` against ETag or Last-Modified. If the resource changed since the `.part` was written, the download restarts from byte 0 — never appended across a gap.
- A `416` response with a complete `.part` finalizes the download instead of failing it.

## Download integrity

- When `Content-Length` is known, the completed byte count is verified exactly.
- A SHA-256 checksum can be supplied per URL — passed as `sha256:<hex>` or as a bare 64-character hex string. Once the byte count passes, the file is hashed (SHA-256) and compared against the supplied checksum.
- The distinction matters: an item can be **completed** without being **integrity-verified**. `checksum_verified` is `True` *only* when the computed hash matched the supplied checksum. Downloads without a checksum never claim verification.

## Supported hosting services

**Only direct plain HTTP/HTTPS links are supported.** Exactly one provider adapter is implemented: `DirectHttpAdapter` (`src/providers/direct_http.py`), which handles ordinary `http(s)` URLs.

Other hosting services (file-lockers, YouTube, torrent/magnet, FTP, etc.) are **not implemented** — none are claimed, and no other endpoints exist in the codebase. The extension point is the `ProviderAdapter` protocol (`src/providers/base.py`) with `name`, `can_handle`, and `resolve` → `ResolvedDownload`; third-party packs can implement it without touching the core engine.

## Webpage link discovery

The Discover page fetches the page you give it (HTTP only) and extracts candidate links:

- Body is capped at **5 MiB**; up to **10 redirects**; at most **500 candidates**.
- At most **15 HEAD requests**, made only for candidates that look like files, and only when *respect rate limit* is enabled. `Retry-After` up to 30 s is honored.
- When *respect rate limit* is enabled, the origin's `robots.txt` is consulted per-origin and honored; a failed `robots.txt` fetch fails **permissive** (scanning proceeds).
- Relative links resolve via `<base href>` and `urljoin`; fragments are stripped; duplicates deduped; only `http`/`https` schemes pass; every candidate is re-checked for SSRF safety before it can be queued.
- Candidates are presented for review and must be explicitly selected and added.

**Limitation: no JavaScript rendering.** Pages whose links are generated client-side (single-page apps, JS-driven galleries) will yield few or no candidates. The scanner does not execute JavaScript or run a headless browser.

## Download history & privacy

- Completed, failed, and cancelled items leave the active queue and remain in History (active states are excluded). History supports search, state filtering, column sorting, retrying failed items, and opening the containing folder.
- **Clear history** can optionally delete the downloaded files as well. File deletion touches only the recorded destination files, is a separate explicitly-labeled checkbox, and is **unchecked by default**.
- There is no telemetry, no analytics, and nothing is uploaded. Logs are local and redact credentials/token-like/signed query parameters before writing.

## Settings reference

| Setting | Notes |
|---|---|
| Destination directory | Default location for new downloads. |
| Max simultaneous downloads | Clamped 1–10, default 3; a change needs an application restart. |
| Timeouts / retries / buffer size | Networking and I/O tuning. |
| Bandwidth limit | Optional per-engine throttle. |
| Collision policy | `keep_existing` / `rename` / `overwrite` / `skip`. |
| Notifications | Toggle completion/failure notifications. |
| Theme | Full dark theme; light and "follow system" modes are partially supported. |
| User agent | Sent with requests. |
| Discovery: extensions list | Which file extensions count as "likely files" when scanning. |
| Discovery: respect rate limit | Enables `robots.txt` checks, HEAD pre-checks, and `Retry-After` honoring. |
| Allow private addresses | **Advanced and dangerous.** Disables the private/loopback address rejection — intended for tests and self-hosted setups such as a local NAS or `localhost` server. Leave off for normal use. |

## Troubleshooting

- **`run.ps1` complains about a missing venv** — run `scripts\setup.ps1` first (or create the venv manually and install `requirements.txt`).
- **Downloads marked `interrupted` at startup** — the app shut down (or crashed) mid-download with unknown ranges. The startup dialog offers to resume these; the `.part` file, if present, is used and its size clamps the recorded progress. A complete file with the matching expected size is marked completed automatically.
- **`skip` collision error at add time** — a file already exists at the destination and the policy is `skip`. Change the policy, or the filename/destination.
- **Expected auth-error on downloads** — `401/403` from the server is surfaced as an authentication-required error and is not retried, unlike transient 5xx errors.
- **Downloads restart from 0 unexpectedly** — the server ignored `Range` (returned `200`) or the resource changed (`If-Range` mismatch). Both are intentional safety behaviors.
- **"Paused but the connection still ran briefly"** — the worker stops at the next range boundary; see [Pausing / resuming](#pausing-resuming--limitations).
- **Discovery returns nothing on a JavaScript-heavy page** — expected; see the no-JS limitation above.
- **Where are the logs?** `%LOCALAPPDATA%\BulkDownloadManager\logs\app.log` (rotating, 1 MiB × 3 backups). The same output also goes to the console.

## Running the tests

```powershell
.\.venv\Scripts\python.exe -m pytest tests -q -o addopts=
```

(181 tests passing at the time of writing; `-o addopts=` bypasses project pytest options that assume a specific invocation.) All tests run against a local test HTTP server — no internet connection is used or needed.

## Packaging

```powershell
.\scripts\build.ps1
```

The script runs the full test suite first and aborts on failure, ensures `pyinstaller>=6.0` is installed, then builds a **one-folder** app (`--onedir --windowed`, tests/pytest/pip/setuptools excluded). The result is:

```
dist\BulkDownloadManager\BulkDownloadManager.exe
```

The path is printed at the end. User data — the SQLite database and logs — lives in `%LOCALAPPDATA%\BulkDownloadManager\`, **never** inside the bundle directory, and `BDM_USER_DATA_DIR` (honored in the packaged app) can override that location.

If you prefer a single executable:

```powershell
pyinstaller --onefile --windowed --name BulkDownloadManager src/main.py
```

Note that the first launch of a `--onefile` build is noticeably slower (it unpacks to a temp dir at every start), which is why `--onedir` is the default here.

## Project layout

```
BulkDownloadManager/
├── run.py                     # entry point → src.main:main
├── requirements.txt           # pinned runtime + dev deps
├── pyproject.toml
├── scripts/
│   ├── setup.ps1              # create/validate venv, install deps
│   ├── run.ps1                # launch the app from the venv
│   └── build.ps1              # test, then build one-folder exe
├── src/
│   ├── main.py                # application bootstrap, startup recovery
│   ├── core/                  # engine (no PySide dependency)
│   │   ├── download_manager.py   # thread pool, scheduling, observer relay
│   │   ├── download_worker.py    # streaming worker, resume, verify
│   │   ├── link_discovery.py     # page scanner, robots, SSRF re-check
│   │   ├── security.py           # URL/SSRF validation, URL redaction
│   │   ├── url_validator.py
│   │   ├── retry_policy.py       # backoff, Retry-After
│   │   ├── filename_utils.py     # rename / collision handling
│   │   └── queue_scheduler.py
│   ├── providers/
│   │   ├── base.py            # ProviderAdapter protocol (extension point)
│   │   └── direct_http.py     # the one implemented adapter
│   ├── database/              # SQLite models, connection (WAL), repository
│   ├── ui/                    # PySide6 views (dashboard, add, discover,
│   │   │                      #   queue, history, settings)
│   ├── app/                   # main window, dialogs, worker threads, theme
│   └── utils/                 # logging config (rotating file), data paths
└── tests/                     # unit + integration tests (local HTTP server)
```

## Development notes

- **No PySide6 in the core.** `src/core`, `src/providers`, `src/database`, and `src/utils` are GUI-free; the engine reports via observer callbacks (`on_progress` / `on_state_change`) which the Qt layer bridges into the Qt event loop through a queued-signal relay. The core can be driven (and is tested) headlessly.
- **State machine boundaries.** Items move through `queued → connecting → downloading` and end in `completed`, `failed`, `cancelled`, or `paused`/`interrupted`. Startup `recover_interrupted` is the single place where the database is reconciled against disk reality; the UI never mutates engine state directly.
- Downloads are throttled to roughly 4 progress callbacks per second to keep the Qt bridge cheap.

## Security and privacy limitations

- **SSRF protection is on by default.** Loopback (`127.0.0.0/8`, `::1`), RFC 1918 private ranges, link-local (`fe80::/10`), `fc00::/7`, unspecified (`0.0.0.0`), and the AWS metadata endpoint (`169.254.169.254`) are rejected, as are URLs containing userinfo (`user:pass@`). DNS resolution is fail-closed. **Known caveat: DNS rebinding** — the resolved address can change between validation and connection; this is documented but not solved.
- **Redirects are re-validated per hop**, and an HTTPS→HTTP downgrade is refused at every hop.
- **TLS verification is never disabled** by the application.
- **`allow_private_addresses`** exists as a settings escape hatch (default off). Enabling it genuinely exposes your network to downloaded content's requests; treat it as a test/advanced-only switch.
- **Logs redact** credential-like query parameters (~40 secret-shaped keys) and never write tokens or signed URLs verbatim; the database stores URLs deliberately (needed for retry/resume) but in a local, user-scoped file.
- There is **no telemetry**, nothing is uploaded, downloads are never auto-executed, and no archives are auto-extracted after download.

## Authorized use

Use this software only to download content you are authorized to download, and honor the terms of service and rate limits of the servers you access. The rate-limiting and `robots.txt` features exist to help with that; they are best-effort aids, not enforcement. The authors do not support using this tool to bulk-scrape infringing or unauthorized content, and accept no liability for misuse.
