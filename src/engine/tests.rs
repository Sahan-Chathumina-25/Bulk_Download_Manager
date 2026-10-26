//! End-to-end engine tests against the loopback [`FakeSite`].
//!
//! Every test gets its own temp directory, SQLite database and manager, so
//! the suite is order-independent and safe to run in parallel.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::config::{CollisionPolicy, Settings};
use crate::database::connection::Database;
use crate::database::repository::DownloadRepository;
use crate::engine::fake_site::FakeSite;
use crate::engine::manager::DownloadManager;
use crate::engine::progress::DownloadProgress;
use crate::engine::worker::EngineEvent;
use crate::models::download_item::DownloadRecord;
use crate::models::download_status::DownloadState;

static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "bdm-engine-{label}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn settings_for(dir: &Path) -> Settings {
    Settings {
        default_destination_dir: dir.to_string_lossy().into_owned(),
        max_simultaneous: 3,
        connection_timeout: 5.0,
        retry_count: 3,
        retry_delay: 0.01,
        buffer_size: 8192,
        bandwidth_limit_bps: 0,
        filename_collision: CollisionPolicy::KeepExisting,
        allow_private_addresses: true,
        ..Settings::default()
    }
}

struct Harness {
    dir: PathBuf,
    repository: Arc<DownloadRepository>,
    manager: DownloadManager,
}

fn harness(label: &str, settings: Settings) -> Harness {
    let dir = temp_dir(label);
    let database = Database::init_db(dir.join("test.sqlite3")).expect("open database");
    let repository = Arc::new(DownloadRepository::new(Arc::new(database)));
    let manager = DownloadManager::new(settings, Arc::clone(&repository));
    Harness {
        dir,
        repository,
        manager,
    }
}

impl Harness {
    fn record(&self, id: i64) -> DownloadRecord {
        self.repository
            .get(id)
            .expect("read download")
            .expect("download exists")
    }

    fn final_path(&self, record: &DownloadRecord) -> PathBuf {
        PathBuf::from(&record.destination_path)
    }
}

async fn wait_terminal(
    repository: &DownloadRepository,
    id: i64,
    timeout: Duration,
) -> DownloadRecord {
    let deadline = Instant::now() + timeout;
    loop {
        let record = repository
            .get(id)
            .expect("read download")
            .expect("download exists");
        if record.state.is_terminal() {
            return record;
        }
        assert!(
            Instant::now() < deadline,
            "download {id} stuck in {} ({:?})",
            record.state.as_str(),
            record.error_summary
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_state(
    repository: &DownloadRepository,
    id: i64,
    state: DownloadState,
    timeout: Duration,
) -> DownloadRecord {
    let deadline = Instant::now() + timeout;
    loop {
        let record = repository
            .get(id)
            .expect("read download")
            .expect("download exists");
        if record.state == state {
            return record;
        }
        assert!(
            Instant::now() < deadline,
            "download {id} never reached {} (stuck in {})",
            state.as_str(),
            record.state.as_str()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn run_to_terminal(harness: &Harness, id: i64) -> DownloadRecord {
    harness.manager.start_all();
    wait_terminal(&harness.repository, id, Duration::from_secs(20)).await
}

// 1
#[tokio::test]
async fn downloads_file_and_completes() {
    let site = FakeSite::spawn().await;
    let harness = harness("basic", settings_for(&temp_dir("basic-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/files/data.bin"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("persisted id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(done.downloaded_bytes, 2048);
    assert!(done.error_summary.is_none());
    assert!(done.completed_at.is_some());
    let bytes = std::fs::read(harness.final_path(&done)).expect("read downloaded file");
    assert_eq!(bytes, site.file_bytes("data.bin").expect("fixture file"));
    assert!(site.hits("/files/data.bin") >= 1);
}

// 2
#[tokio::test]
async fn progress_events_are_throttled_and_monotonic() {
    let site = FakeSite::spawn().await;
    let harness = harness("progress", settings_for(&temp_dir("progress-settings")));
    let mut events = harness.manager.subscribe();
    let record = harness
        .manager
        .add_download(&site.url("/slow"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    harness.manager.start_all();

    let mut progress: Vec<DownloadProgress> = Vec::new();
    let mut saw_completed = false;
    while !saw_completed {
        let event = tokio::time::timeout(Duration::from_secs(15), events.recv())
            .await
            .expect("event timeout")
            .expect("event channel");
        match event {
            EngineEvent::Progress(snapshot) => {
                assert_eq!(snapshot.download_id, id);
                if let Some(last) = progress.last() {
                    assert!(snapshot.downloaded_bytes >= last.downloaded_bytes);
                }
                progress.push(snapshot);
            }
            EngineEvent::StateChanged(record) => {
                saw_completed = record.state == DownloadState::Completed;
            }
        }
    }

    assert!(
        progress.len() >= 3,
        "only {} progress events",
        progress.len()
    );
    let final_progress = progress.last().expect("final progress");
    assert_eq!(final_progress.total_bytes, Some(100));
    assert_eq!(final_progress.downloaded_bytes, 100);
    assert_eq!(final_progress.eta_seconds, Some(0));
}

// 3
#[tokio::test]
async fn state_changes_are_persisted_in_order() {
    let site = FakeSite::spawn().await;
    let harness = harness("states", settings_for(&temp_dir("states-settings")));
    let mut events = harness.manager.subscribe();
    let record = harness
        .manager
        .add_download(&site.url("/files/data.bin"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    harness.manager.start_all();

    let mut states = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(15), events.recv())
            .await
            .expect("event timeout")
            .expect("event channel");
        if let EngineEvent::StateChanged(record) = event {
            let persisted = harness.record(id);
            assert_eq!(persisted.state, record.state);
            if states.last() != Some(&record.state) {
                states.push(record.state);
            }
            if record.state == DownloadState::Completed {
                break;
            }
        }
    }
    assert_eq!(
        states,
        vec![
            DownloadState::Connecting,
            DownloadState::Downloading,
            DownloadState::Completed
        ]
    );
}

// 4
#[tokio::test]
async fn follows_redirect_chain() {
    let site = FakeSite::spawn().await;
    let harness = harness("redirect", settings_for(&temp_dir("redirect-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/redirect/3"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(
        std::fs::read(harness.final_path(&done)).expect("read file"),
        site.file_bytes("data.bin").expect("fixture file")
    );
    assert!(site.hits("/redirect/3") >= 1);
    assert!(site.hits("/files/data.bin") >= 1);
}

// 5
#[tokio::test]
async fn rejects_redirect_loop() {
    let site = FakeSite::spawn().await;
    let harness = harness("loop", settings_for(&temp_dir("loop-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/redirect/50"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Failed);
    let error = done.error_summary.unwrap_or_default();
    assert!(error.contains("too many redirects"), "error was {error:?}");
}

// 6
#[tokio::test]
async fn rejects_non_http_redirect() {
    let site = FakeSite::spawn().await;
    let harness = harness("ftp", settings_for(&temp_dir("ftp-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/redirect-ftp"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Failed);
    let error = done.error_summary.unwrap_or_default();
    assert!(
        error.contains("unsafe redirect target refused"),
        "error was {error:?}"
    );
}

// 7
#[tokio::test]
async fn rejects_html_content_type() {
    let site = FakeSite::spawn().await;
    let harness = harness("html-type", settings_for(&temp_dir("html-type-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/html-as-file"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Failed);
    let error = done.error_summary.as_deref().unwrap_or_default();
    assert!(error.contains("HTML page"), "error was {error:?}");
    assert!(!harness.final_path(&done).exists());
}

// 8
#[tokio::test]
async fn rejects_html_body_sniffed_from_octet_stream() {
    let site = FakeSite::spawn().await;
    site.set_file(
        "landing.html",
        b"<!doctype html><html><body>login</body></html>",
    );
    let harness = harness("html-sniff", settings_for(&temp_dir("html-sniff-settings")));
    let record = harness
        .manager
        .add_download(
            &site.url("/files/landing.html"),
            None,
            Some(&harness.dir),
            None,
        )
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Failed);
    assert!(done.error_summary.unwrap_or_default().contains("HTML page"));
}

// 9
#[tokio::test]
async fn rejects_content_length_mismatch() {
    let site = FakeSite::spawn().await;
    let harness = harness("mismatch", settings_for(&temp_dir("mismatch-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/mismatch"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Failed);
    let error = done.error_summary.unwrap_or_default();
    assert!(error.contains("mismatch"), "error was {error:?}");
}

// 10
#[tokio::test]
async fn verifies_checksum_success() {
    let site = FakeSite::spawn().await;
    let harness = harness(
        "checksum-ok",
        settings_for(&temp_dir("checksum-ok-settings")),
    );
    let expected = hex_sha256(&site.file_bytes("data.bin").expect("fixture file"));
    let record = harness
        .manager
        .add_download(
            &site.url("/files/data.bin"),
            None,
            Some(&harness.dir),
            Some(&format!("sha256:{expected}")),
        )
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert!(done.checksum_verified);
}

// 11
#[tokio::test]
async fn rejects_checksum_mismatch() {
    let site = FakeSite::spawn().await;
    let harness = harness(
        "checksum-bad",
        settings_for(&temp_dir("checksum-bad-settings")),
    );
    let wrong = "0".repeat(64);
    let record = harness
        .manager
        .add_download(
            &site.url("/files/data.bin"),
            None,
            Some(&harness.dir),
            Some(&wrong),
        )
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Failed);
    let error = done.error_summary.unwrap_or_default();
    assert!(
        error.contains("SHA-256 checksum mismatch"),
        "error was {error:?}"
    );
    assert!(!done.checksum_verified);
}

// 12
#[tokio::test]
async fn rejects_invalid_checksum_argument() {
    let site = FakeSite::spawn().await;
    let harness = harness(
        "checksum-arg",
        settings_for(&temp_dir("checksum-arg-settings")),
    );
    let error = harness
        .manager
        .add_download(
            &site.url("/files/data.bin"),
            None,
            Some(&harness.dir),
            Some("not-a-digest"),
        )
        .expect_err("invalid checksum must be rejected");
    assert!(error.contains("64-character hex"), "error was {error:?}");
}

// 13
#[tokio::test]
async fn resumes_from_partial_file() {
    let site = FakeSite::spawn().await;
    let harness = harness("resume", settings_for(&temp_dir("resume-settings")));
    let full = site.file_bytes("data.bin").expect("fixture file");
    std::fs::write(harness.dir.join("data.bin.part"), &full[..1024]).expect("seed part file");
    let record = harness
        .manager
        .add_download(&site.url("/files/data.bin"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(
        std::fs::read(harness.final_path(&done)).expect("read file"),
        full
    );
    assert_eq!(site.hits("/files/data.bin"), 1);
}

// 14
#[tokio::test]
async fn restarts_when_server_ignores_range() {
    let site = FakeSite::spawn().await;
    let harness = harness("norange", settings_for(&temp_dir("norange-settings")));
    let full = site.file_bytes("data.bin").expect("fixture file");
    std::fs::write(harness.dir.join("data.bin.part"), &full[..1024]).expect("seed part file");
    let record = harness
        .manager
        .add_download(
            &site.url("/norange/data.bin"),
            None,
            Some(&harness.dir),
            None,
        )
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(
        std::fs::read(harness.final_path(&done)).expect("read file"),
        full
    );
    assert_eq!(site.hits("/norange/data.bin"), 2);
}

// 15
#[tokio::test]
async fn restarts_when_etag_changes() {
    let site = FakeSite::spawn().await;
    let harness = harness("etag", settings_for(&temp_dir("etag-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/etag/data.bin"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert!(done.etag.is_some());

    let full = site.file_bytes("data.bin").expect("fixture file");
    std::fs::remove_file(harness.final_path(&done)).expect("remove final file");
    std::fs::write(harness.dir.join("data.bin.part"), &full[..1024]).expect("seed part");
    site.set_etag("data.bin", "\"changed\"");

    let mut paused = harness.record(id);
    paused.state = DownloadState::Paused;
    paused.downloaded_bytes = 1024;
    harness.repository.update(&paused).expect("park record");
    harness.manager.resume(id).expect("resume");
    let done = wait_terminal(&harness.repository, id, Duration::from_secs(20)).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(
        std::fs::read(harness.final_path(&done)).expect("read file"),
        full
    );
    assert_eq!(site.hits("/etag/data.bin"), 3);
}

// 16
#[tokio::test]
async fn completes_when_part_is_already_full() {
    let site = FakeSite::spawn().await;
    let harness = harness("part-full", settings_for(&temp_dir("part-full-settings")));
    let full = site.file_bytes("data.bin").expect("fixture file");
    std::fs::write(harness.dir.join("data.bin.part"), &full).expect("seed full part");
    let record = harness
        .manager
        .add_download(&site.url("/files/data.bin"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(
        std::fs::read(harness.final_path(&done)).expect("read file"),
        full
    );
    assert_eq!(site.hits("/files/data.bin"), 1);
}

// 17
#[tokio::test]
async fn restarts_when_part_size_disagrees_with_416() {
    let site = FakeSite::spawn().await;
    let harness = harness("part-416", settings_for(&temp_dir("part-416-settings")));
    let full = site.file_bytes("small.zip").expect("fixture file");
    std::fs::write(harness.dir.join("small.zip.part"), vec![0u8; 2048]).expect("seed wrong part");
    let record = harness
        .manager
        .add_download(
            &site.url("/files/small.zip"),
            None,
            Some(&harness.dir),
            None,
        )
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(
        std::fs::read(harness.final_path(&done)).expect("read file"),
        full
    );
    assert_eq!(site.hits("/files/small.zip"), 2);
}

// 18
#[tokio::test]
async fn retries_transient_status_then_succeeds() {
    let site = FakeSite::spawn().await;
    let harness = harness("flaky", settings_for(&temp_dir("flaky-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/flaky/data.bin"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(done.retry_count, 2);
    assert_eq!(site.hits("/flaky/data.bin"), 3);
}

// 19
#[tokio::test]
async fn honours_retry_after_header() {
    let site = FakeSite::spawn().await;
    let mut settings = settings_for(&temp_dir("retry-after-settings"));
    settings.retry_count = 1;
    let harness = harness("retry-after", settings);
    let record = harness
        .manager
        .add_download(&site.url("/retry-after"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    let started = Instant::now();
    let done = run_to_terminal(&harness, id).await;
    let elapsed = started.elapsed();
    assert_eq!(done.state, DownloadState::Failed);
    assert!(elapsed >= Duration::from_millis(900), "elapsed {elapsed:?}");
    assert_eq!(site.hits("/retry-after"), 2);
}

// 20
#[tokio::test]
async fn permanent_404_is_not_retried() {
    let site = FakeSite::spawn().await;
    let harness = harness("notfound", settings_for(&temp_dir("notfound-settings")));
    let record = harness
        .manager
        .add_download(
            &site.url("/files/missing.bin"),
            None,
            Some(&harness.dir),
            None,
        )
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Failed);
    assert_eq!(done.retry_count, 0);
    assert!(done.error_summary.unwrap_or_default().contains("not found"));
    assert_eq!(site.hits("/files/missing.bin"), 1);
}

// 21
#[tokio::test]
async fn auth_required_is_not_retried() {
    let site = FakeSite::spawn().await;
    let harness = harness("auth", settings_for(&temp_dir("auth-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/status/403"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Failed);
    assert_eq!(done.retry_count, 0);
    assert_eq!(
        done.error_summary.unwrap_or_default(),
        "the server refused access (HTTP 403); the resource may require credentials the app does not provide"
    );
    assert_eq!(site.hits("/status/403"), 1);
}

// 22
#[tokio::test]
async fn cancel_running_download() {
    let site = FakeSite::spawn().await;
    let harness = harness("cancel", settings_for(&temp_dir("cancel-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/slow"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    harness.manager.start_all();
    wait_state(
        &harness.repository,
        id,
        DownloadState::Downloading,
        Duration::from_secs(5),
    )
    .await;
    harness.manager.cancel(id).expect("cancel");
    let done = wait_terminal(&harness.repository, id, Duration::from_secs(10)).await;
    assert_eq!(done.state, DownloadState::Cancelled);
}

// 23
#[tokio::test]
async fn pause_and_resume_running_download() {
    let site = FakeSite::spawn().await;
    let mut settings = settings_for(&temp_dir("pause-settings"));
    settings.bandwidth_limit_bps = 20_000;
    let harness = harness("pause", settings);
    let full = site.file_bytes("big.zip").expect("fixture file");
    let record = harness
        .manager
        .add_download(&site.url("/files/big.zip"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    harness.manager.start_all();
    wait_state(
        &harness.repository,
        id,
        DownloadState::Downloading,
        Duration::from_secs(5),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    harness.manager.pause(id).expect("pause");
    let paused = wait_state(
        &harness.repository,
        id,
        DownloadState::Paused,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        harness
            .final_path(&paused)
            .with_extension("zip.part")
            .exists()
    );

    harness.manager.resume(id).expect("resume");
    let done = wait_terminal(&harness.repository, id, Duration::from_secs(20)).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(
        std::fs::read(harness.final_path(&done)).expect("read file"),
        full
    );
}

// 24
#[tokio::test]
async fn pause_without_range_support_behaves_like_cancel() {
    let site = FakeSite::spawn().await;
    let harness = harness(
        "pause-norange",
        settings_for(&temp_dir("pause-norange-settings")),
    );
    let record = harness
        .manager
        .add_download(&site.url("/slow"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    harness.manager.start_all();
    wait_state(
        &harness.repository,
        id,
        DownloadState::Downloading,
        Duration::from_secs(5),
    )
    .await;
    harness.manager.pause(id).expect("pause");
    let done = wait_terminal(&harness.repository, id, Duration::from_secs(10)).await;
    assert_eq!(done.state, DownloadState::Cancelled);
}

// 25
#[tokio::test]
async fn enforces_concurrency_cap() {
    let site = FakeSite::spawn().await;
    let mut settings = settings_for(&temp_dir("concurrency-settings"));
    settings.max_simultaneous = 2;
    let harness = harness("concurrency", settings);
    let mut ids = Vec::new();
    for index in 0..4 {
        let dir = harness.dir.join(format!("slot{index}"));
        std::fs::create_dir_all(&dir).expect("slot dir");
        let record = harness
            .manager
            .add_download(&site.url("/slow"), None, Some(&dir), None)
            .expect("add download");
        ids.push(record.id.expect("id"));
    }
    assert_eq!(harness.manager.max_simultaneous(), 2);
    let started = harness.manager.start_all();
    assert_eq!(started, 2);

    // Observe engine concurrency through the persisted states rather than
    // the fixture's request counter: the fixture decrements its in-flight
    // count after the response is flushed, so a connection can still be
    // open for a few microseconds when the next worker starts.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut observed_max = 0usize;
    loop {
        let rows = harness.repository.list_all().expect("list downloads");
        let active = rows
            .iter()
            .filter(|record| {
                matches!(
                    record.state,
                    DownloadState::Connecting | DownloadState::Downloading
                )
            })
            .count();
        observed_max = observed_max.max(active);
        if rows.iter().all(|record| record.state.is_terminal()) {
            break;
        }
        assert!(Instant::now() < deadline, "downloads did not settle");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        observed_max <= 2,
        "observed {observed_max} concurrent downloads"
    );
    for id in &ids {
        let done = harness.record(*id);
        assert_eq!(done.state, DownloadState::Completed);
    }
    assert_eq!(site.hits("/slow"), 4);
}

// 26
#[tokio::test]
async fn pump_starts_next_queued_download() {
    let site = FakeSite::spawn().await;
    let mut settings = settings_for(&temp_dir("pump-settings"));
    settings.max_simultaneous = 1;
    let harness = harness("pump", settings);
    let mut ids = Vec::new();
    for index in 0..2 {
        let dir = harness.dir.join(format!("slot{index}"));
        std::fs::create_dir_all(&dir).expect("slot dir");
        let record = harness
            .manager
            .add_download(&site.url("/slow"), None, Some(&dir), None)
            .expect("add download");
        ids.push(record.id.expect("id"));
    }
    assert_eq!(harness.manager.start_all(), 1);
    for id in &ids {
        let done = wait_terminal(&harness.repository, *id, Duration::from_secs(30)).await;
        assert_eq!(done.state, DownloadState::Completed);
    }
    assert_eq!(site.hits("/slow"), 2);
}

// 27
#[tokio::test]
async fn skip_policy_rejects_existing_file_at_add() {
    let site = FakeSite::spawn().await;
    let base = temp_dir("skip-settings");
    std::fs::write(base.join("data.bin"), b"existing").expect("seed file");
    let mut settings = settings_for(&base);
    settings.filename_collision = CollisionPolicy::Skip;
    let harness = harness("skip", settings);
    let error = harness
        .manager
        .add_download(&site.url("/files/data.bin"), None, Some(&base), None)
        .expect_err("skip must reject");
    assert!(
        error.contains("collision policy is 'skip'"),
        "error was {error:?}"
    );
}

// 28
#[tokio::test]
async fn keep_existing_marks_completed_without_overwrite() {
    let site = FakeSite::spawn().await;
    let base = temp_dir("keep-settings");
    std::fs::write(base.join("data.bin"), b"existing").expect("seed file");
    let harness = harness("keep", settings_for(&base));
    let record = harness
        .manager
        .add_download(&site.url("/files/data.bin"), None, Some(&base), None)
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(done.downloaded_bytes, 8);
    assert_eq!(
        std::fs::read(base.join("data.bin")).expect("read file"),
        b"existing"
    );
    assert_eq!(site.hits("/files/data.bin"), 0);
}

// 29
#[tokio::test]
async fn overwrite_policy_replaces_existing_file() {
    let site = FakeSite::spawn().await;
    let base = temp_dir("overwrite-settings");
    std::fs::write(base.join("data.bin"), b"old").expect("seed file");
    let mut settings = settings_for(&base);
    settings.filename_collision = CollisionPolicy::Overwrite;
    let harness = harness("overwrite", settings);
    let record = harness
        .manager
        .add_download(&site.url("/files/data.bin"), None, Some(&base), None)
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(
        std::fs::read(base.join("data.bin")).expect("read file"),
        site.file_bytes("data.bin").expect("fixture file")
    );
}

// 30
#[tokio::test]
async fn rename_policy_picks_free_name() {
    let site = FakeSite::spawn().await;
    let base = temp_dir("rename-settings");
    std::fs::write(base.join("data.bin"), b"existing").expect("seed file");
    let mut settings = settings_for(&base);
    settings.filename_collision = CollisionPolicy::Rename;
    let harness = harness("rename", settings);
    let record = harness
        .manager
        .add_download(&site.url("/files/data.bin"), None, Some(&base), None)
        .expect("add download");
    assert!(
        record.destination_path.ends_with("data (1).bin"),
        "destination was {:?}",
        record.destination_path
    );
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(
        std::fs::read(base.join("data (1).bin")).expect("read file"),
        site.file_bytes("data.bin").expect("fixture file")
    );
    assert_eq!(
        std::fs::read(base.join("data.bin")).expect("read original"),
        b"existing"
    );
}

// 31
#[tokio::test]
async fn remove_refuses_running_download() {
    let site = FakeSite::spawn().await;
    let harness = harness("remove", settings_for(&temp_dir("remove-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/slow"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");
    harness.manager.start_all();
    wait_state(
        &harness.repository,
        id,
        DownloadState::Downloading,
        Duration::from_secs(5),
    )
    .await;
    let error = harness
        .manager
        .remove(id)
        .expect_err("remove must refuse a running download");
    assert!(
        error.contains("actively downloading"),
        "error was {error:?}"
    );
    harness.manager.cancel(id).expect("cancel");
    wait_terminal(&harness.repository, id, Duration::from_secs(10)).await;
    assert!(harness.manager.remove(id).expect("remove settled download"));
    assert!(harness.repository.get(id).expect("read").is_none());
}

// 32
#[tokio::test]
async fn retry_after_failure_succeeds() {
    let site = FakeSite::spawn().await;
    let harness = harness("retry", settings_for(&temp_dir("retry-settings")));
    let record = harness
        .manager
        .add_download(
            &site.url("/files/later.bin"),
            None,
            Some(&harness.dir),
            None,
        )
        .expect("add download");
    let id = record.id.expect("id");
    let failed = run_to_terminal(&harness, id).await;
    assert_eq!(failed.state, DownloadState::Failed);

    site.set_file("later.bin", b"late arrival");
    harness.manager.retry(id).expect("retry");
    let done = wait_terminal(&harness.repository, id, Duration::from_secs(20)).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert!(done.error_summary.is_none());
    assert_eq!(
        std::fs::read(harness.final_path(&done)).expect("read file"),
        b"late arrival"
    );
}

// 33
#[tokio::test]
async fn chunked_transfer_downloads_without_content_length() {
    let site = FakeSite::spawn().await;
    let harness = harness("chunked", settings_for(&temp_dir("chunked-settings")));
    let record = harness
        .manager
        .add_download(
            &site.url("/chunked/big.zip"),
            None,
            Some(&harness.dir),
            None,
        )
        .expect("add download");
    let id = record.id.expect("id");
    let done = run_to_terminal(&harness, id).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(
        std::fs::read(harness.final_path(&done)).expect("read file"),
        site.file_bytes("big.zip").expect("fixture file")
    );
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Wait until the scheduler owns no slots (workers finish their bookkeeping
/// just after they persist a terminal state).
async fn wait_scheduler_empty(manager: &DownloadManager, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while manager.scheduler_running_count() > 0 {
        assert!(
            Instant::now() < deadline,
            "scheduler still holds {:?}",
            manager.scheduler_pending()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// 34
#[tokio::test]
async fn repeated_start_all_is_idempotent() {
    let site = FakeSite::spawn().await;
    let harness = harness("idempotent", settings_for(&temp_dir("idempotent-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/slow"), None, Some(&harness.dir), None)
        .expect("add download");
    let id = record.id.expect("id");

    assert_eq!(harness.manager.start_all(), 1, "first call starts the row");
    assert_eq!(
        harness.manager.start_all(),
        0,
        "a row already owned by a worker must not be started twice"
    );
    assert_eq!(harness.manager.scheduler_pending(), vec![id]);

    let done = wait_terminal(&harness.repository, id, Duration::from_secs(30)).await;
    assert_eq!(done.state, DownloadState::Completed);
    wait_scheduler_empty(&harness.manager, Duration::from_secs(10)).await;
    assert!(harness.manager.scheduler_pending().is_empty());
    assert_eq!(site.hits("/slow"), 1, "the worker must run exactly once");
}

// 35
#[tokio::test]
async fn cancel_while_queued_never_starts_the_worker() {
    let site = FakeSite::spawn().await;
    let mut settings = settings_for(&temp_dir("cancel-queued-settings"));
    settings.max_simultaneous = 1;
    let harness = harness("cancel-queued", settings);

    let running = harness
        .manager
        .add_download(&site.url("/slow"), None, Some(&harness.dir), None)
        .expect("add running download");
    let running_id = running.id.expect("id");
    let queued = harness
        .manager
        .add_download(&site.url("/files/data.bin"), None, Some(&harness.dir), None)
        .expect("add queued download");
    let queued_id = queued.id.expect("id");

    assert_eq!(harness.manager.start_all(), 1);
    assert_eq!(harness.manager.scheduler_pending(), vec![running_id]);

    harness
        .manager
        .cancel(queued_id)
        .expect("cancel queued row");
    assert_eq!(
        harness.record(queued_id).state,
        DownloadState::Cancelled,
        "a queued row cancels immediately"
    );

    let done = wait_terminal(&harness.repository, running_id, Duration::from_secs(30)).await;
    assert_eq!(done.state, DownloadState::Completed);
    wait_scheduler_empty(&harness.manager, Duration::from_secs(10)).await;
    assert_eq!(
        site.hits("/files/data.bin"),
        0,
        "the cancelled row must never be fetched"
    );
    assert_eq!(
        harness.record(queued_id).state,
        DownloadState::Cancelled,
        "the freed slot must not resurrect a cancelled row"
    );
}

// 36
#[tokio::test]
async fn retried_row_waits_for_a_free_slot_then_completes() {
    let site = FakeSite::spawn().await;
    let mut settings = settings_for(&temp_dir("requeue-settings"));
    settings.max_simultaneous = 1;
    let harness = harness("requeue", settings);

    let failing = harness
        .manager
        .add_download(
            &site.url("/files/later.bin"),
            None,
            Some(&harness.dir),
            None,
        )
        .expect("add download");
    let failing_id = failing.id.expect("id");
    let failed = run_to_terminal(&harness, failing_id).await;
    assert_eq!(failed.state, DownloadState::Failed);

    // Occupy the only slot, then requeue the failed row.
    let running = harness
        .manager
        .add_download(&site.url("/slow"), None, Some(&harness.dir), None)
        .expect("add running download");
    let running_id = running.id.expect("id");
    assert_eq!(harness.manager.start_all(), 1);

    site.set_file("later.bin", b"late arrival");
    harness.manager.retry(failing_id).expect("retry");
    assert_eq!(
        harness.record(failing_id).state,
        DownloadState::Queued,
        "no slot is free, so the retried row must stay queued"
    );
    assert_eq!(harness.manager.scheduler_pending(), vec![running_id]);

    let first = wait_terminal(&harness.repository, running_id, Duration::from_secs(30)).await;
    assert_eq!(first.state, DownloadState::Completed);

    let done = wait_terminal(&harness.repository, failing_id, Duration::from_secs(30)).await;
    assert_eq!(done.state, DownloadState::Completed);
    assert!(done.error_summary.is_none());
    wait_scheduler_empty(&harness.manager, Duration::from_secs(10)).await;
    assert_eq!(site.hits("/files/later.bin"), 2, "one failure, one success");
}

// 37
#[tokio::test]
async fn concurrent_completions_leave_no_pending_slots() {
    let site = FakeSite::spawn().await;
    let harness = harness("concurrent", settings_for(&temp_dir("concurrent-settings")));
    let mut ids = Vec::new();
    for index in 0..3 {
        let dir = harness.dir.join(format!("slot{index}"));
        std::fs::create_dir_all(&dir).expect("slot dir");
        let record = harness
            .manager
            .add_download(&site.url("/slow"), None, Some(&dir), None)
            .expect("add download");
        ids.push(record.id.expect("id"));
    }

    assert_eq!(harness.manager.start_all(), 3);
    for id in &ids {
        let done = wait_terminal(&harness.repository, *id, Duration::from_secs(30)).await;
        assert_eq!(done.state, DownloadState::Completed);
    }

    wait_scheduler_empty(&harness.manager, Duration::from_secs(10)).await;
    assert!(harness.manager.scheduler_pending().is_empty());
    for id in &ids {
        assert_eq!(harness.record(*id).state, DownloadState::Completed);
    }
    assert_eq!(site.hits("/slow"), 3);
}

// 38
/// Rows parked in `Connecting`/`Downloading` right now.
fn live_count(harness: &Harness) -> usize {
    harness
        .repository
        .list_all()
        .expect("list downloads")
        .iter()
        .filter(|record| {
            matches!(
                record.state,
                DownloadState::Connecting | DownloadState::Downloading
            )
        })
        .count()
}

/// `apply_settings` must hand the freed slots to queued rows without the UI
/// having to call `start_all` again.
#[tokio::test]
async fn apply_settings_raising_the_cap_starts_queued_rows() {
    let site = FakeSite::spawn().await;
    let mut settings = settings_for(&temp_dir("apply-raise-settings"));
    settings.max_simultaneous = 1;
    // Stalls never answer, so nothing can complete and mask the dispatch.
    settings.connection_timeout = 30.0;
    let harness = harness("apply-raise", settings.clone());

    for index in 0..3 {
        let dir = harness.dir.join(format!("slot{index}"));
        std::fs::create_dir_all(&dir).expect("slot dir");
        harness
            .manager
            .add_download(&site.url("/stall"), None, Some(&dir), None)
            .expect("add download");
    }

    assert_eq!(harness.manager.start_all(), 1);
    // The worker only writes `Connecting` once it starts, so wait for it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while live_count(&harness) < 1 {
        assert!(Instant::now() < deadline, "the first row never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut raised = settings;
    raised.max_simultaneous = 3;
    harness.manager.apply_settings(raised);

    assert_eq!(harness.manager.max_simultaneous(), 3);
    let deadline = Instant::now() + Duration::from_secs(10);
    while live_count(&harness) < 3 {
        assert!(
            Instant::now() < deadline,
            "only {} of 3 rows started after raising the cap",
            live_count(&harness)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    harness.manager.shutdown().await;
}

// 39
/// Lowering the cap must not dispatch anything extra even though slots are
/// still occupied by transfers that started under the old limit.
#[tokio::test]
async fn apply_settings_lowering_the_cap_blocks_new_dispatches() {
    let site = FakeSite::spawn().await;
    let mut settings = settings_for(&temp_dir("apply-lower-settings"));
    settings.max_simultaneous = 3;
    settings.connection_timeout = 30.0;
    let harness = harness("apply-lower", settings.clone());

    for index in 0..3 {
        let dir = harness.dir.join(format!("slot{index}"));
        std::fs::create_dir_all(&dir).expect("slot dir");
        harness
            .manager
            .add_download(&site.url("/stall"), None, Some(&dir), None)
            .expect("add download");
    }

    assert_eq!(harness.manager.start_all(), 3);
    let deadline = Instant::now() + Duration::from_secs(10);
    while live_count(&harness) < 3 {
        assert!(Instant::now() < deadline, "three rows never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut lowered = settings;
    lowered.max_simultaneous = 1;
    harness.manager.apply_settings(lowered);
    assert_eq!(harness.manager.max_simultaneous(), 1);

    // A fourth row is queued, but the lowered cap is already exceeded by the
    // three running transfers, so nothing may start.
    let dir = harness.dir.join("slot3");
    std::fs::create_dir_all(&dir).expect("slot dir");
    let queued = harness
        .manager
        .add_download(&site.url("/stall"), None, Some(&dir), None)
        .expect("add download");

    assert_eq!(harness.manager.start_all(), 0);
    assert_eq!(
        harness.record(queued.id.expect("id")).state,
        DownloadState::Queued
    );
    assert_eq!(live_count(&harness), 3);

    harness.manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// Structured failure diagnostics (phase 6.5)
// ---------------------------------------------------------------------------

/// Shorthand: the stored category, or `""` when the row has none.
fn category_of(record: &DownloadRecord) -> &str {
    record.failure_category.as_deref().unwrap_or_default()
}

#[tokio::test]
async fn html_mistake_persists_html_category_and_content_type() {
    let site = FakeSite::spawn().await;
    let harness = harness("diag-html", settings_for(&temp_dir("diag-html-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/html-as-file"), None, Some(&harness.dir), None)
        .expect("add download");
    let done = run_to_terminal(&harness, record.id.expect("id")).await;

    assert_eq!(done.state, DownloadState::Failed);
    assert_eq!(category_of(&done), "html_page");
    // Validators are captured before the HTML guard rejects the response, so
    // the content type survives to explain what actually arrived.
    let content_type = done.content_type.as_deref().unwrap_or_default();
    assert!(
        content_type.contains("text/html"),
        "content type was {content_type:?}"
    );
}

#[tokio::test]
async fn redirect_loop_persists_redirect_rejected_category() {
    let site = FakeSite::spawn().await;
    let harness = harness("diag-loop", settings_for(&temp_dir("diag-loop-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/redirect/50"), None, Some(&harness.dir), None)
        .expect("add download");
    let done = run_to_terminal(&harness, record.id.expect("id")).await;

    assert_eq!(done.state, DownloadState::Failed);
    assert_eq!(category_of(&done), "redirect_rejected");
}

#[tokio::test]
async fn unsafe_redirect_persists_redirect_rejected_category() {
    let site = FakeSite::spawn().await;
    let harness = harness("diag-ftp", settings_for(&temp_dir("diag-ftp-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/redirect-ftp"), None, Some(&harness.dir), None)
        .expect("add download");
    let done = run_to_terminal(&harness, record.id.expect("id")).await;

    assert_eq!(done.state, DownloadState::Failed);
    assert_eq!(category_of(&done), "redirect_rejected");
}

#[tokio::test]
async fn missing_file_persists_not_found_category() {
    let site = FakeSite::spawn().await;
    let harness = harness("diag-404", settings_for(&temp_dir("diag-404-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/status/404"), None, Some(&harness.dir), None)
        .expect("add download");
    let done = run_to_terminal(&harness, record.id.expect("id")).await;

    assert_eq!(done.state, DownloadState::Failed);
    assert_eq!(category_of(&done), "not_found");
}

#[tokio::test]
async fn missing_file_with_html_body_hints_at_expired_link() {
    let site = FakeSite::spawn().await;
    let harness = harness(
        "diag-404-html",
        settings_for(&temp_dir("diag-404-html-settings")),
    );
    let record = harness
        .manager
        .add_download(&site.url("/gone-html"), None, Some(&harness.dir), None)
        .expect("add download");
    let done = run_to_terminal(&harness, record.id.expect("id")).await;

    assert_eq!(done.state, DownloadState::Failed);
    assert_eq!(category_of(&done), "not_found");
    let error = done.error_summary.unwrap_or_default();
    assert!(
        error.contains("may be expired, removed, or require browser interaction"),
        "unexpected summary: {error:?}"
    );
    // A 404/410 is permanent: the worker must not retry the HTML page.
    assert_eq!(done.retry_count, 0);
    assert_eq!(site.hits("/gone-html"), 1);
}

#[tokio::test]
async fn auth_failure_persists_auth_required_category() {
    let site = FakeSite::spawn().await;
    let harness = harness("diag-403", settings_for(&temp_dir("diag-403-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/status/403"), None, Some(&harness.dir), None)
        .expect("add download");
    let done = run_to_terminal(&harness, record.id.expect("id")).await;

    assert_eq!(done.state, DownloadState::Failed);
    assert_eq!(category_of(&done), "auth_required");
}

#[tokio::test]
async fn checksum_mismatch_persists_checksum_category() {
    let site = FakeSite::spawn().await;
    let harness = harness(
        "diag-checksum",
        settings_for(&temp_dir("diag-checksum-settings")),
    );
    let wrong = "1".repeat(64);
    let record = harness
        .manager
        .add_download(
            &site.url("/files/data.bin"),
            None,
            Some(&harness.dir),
            Some(&wrong),
        )
        .expect("add download");
    let done = run_to_terminal(&harness, record.id.expect("id")).await;

    assert_eq!(done.state, DownloadState::Failed);
    assert_eq!(category_of(&done), "checksum_mismatch");
    assert!(!done.checksum_verified);
}

#[tokio::test]
async fn successful_download_clears_the_failure_category() {
    let site = FakeSite::spawn().await;
    let harness = harness("diag-clear", settings_for(&temp_dir("diag-clear-settings")));
    let record = harness
        .manager
        .add_download(&site.url("/files/data.bin"), None, Some(&harness.dir), None)
        .expect("add download");
    let done = run_to_terminal(&harness, record.id.expect("id")).await;

    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(done.failure_category, None);
}
