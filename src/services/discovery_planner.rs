//! Turn checked discovery candidates into queue rows.
//!
//! Port of the legacy ``src/ui/discovery_queue_planner.py``. Planning is pure
//! and never fails: every candidate ends up either in `to_add`, `duplicates`
//! or `rejected` with a user-facing reason.
//!
//! # DNS split between intake and planning
//!
//! Intake ([`crate::services::url_intake`]) validates with full DNS
//! resolution because the user is asking whether a URL is safe right now.
//! The planner runs as a preflight over candidates that were already vetted
//! by the scanner, so [`crate::services::validation::validate_url_for_network_opts`]
//! is called with `resolve_dns = false`: literal private addresses and bad
//! schemes/ports are still rejected, but hostnames are not resolved a second
//! time. The worker validates again (with DNS and IP pinning) immediately
//! before connecting, which is where resolution belongs.

use std::collections::HashSet;

use url::{Host, Url};

use crate::models::discovered_link::{DiscoveryCandidate, QueuePlan};
use crate::models::download_item::DownloadRecord;
use crate::services::validation;

/// Keep only candidates the user ticked, preserving input order.
pub fn checked_candidates(candidates: &[DiscoveryCandidate]) -> Vec<DiscoveryCandidate> {
    candidates
        .iter()
        .filter(|candidate| candidate.is_likely_file)
        .cloned()
        .collect()
}

/// Outcome of adding a plan to the download manager.
#[derive(Debug, Default)]
pub struct ExecutionReport {
    /// Records the manager persisted.
    pub added: Vec<DownloadRecord>,
    /// `(label, reason)` pairs for rows the manager refused.
    pub failed: Vec<(String, String)>,
}

/// Plan which checked candidates may join the queue.
///
/// Per candidate, in input order:
/// 1. empty URL -> rejected,
/// 2. unparsable URL -> rejected,
/// 3. non-http(s) scheme -> rejected,
/// 4. preflight safety check (no DNS) -> rejected on failure,
/// 5. already in `existing_urls` -> duplicate,
/// 6. repeated within the batch -> duplicate,
/// 7. otherwise eligible.
pub fn plan_discovery_queue(
    candidates: &[DiscoveryCandidate],
    existing_urls: &[String],
    allow_private: bool,
) -> QueuePlan {
    let mut plan = QueuePlan::default();

    let known: HashSet<String> = existing_urls
        .iter()
        .filter_map(|url| normalized_url(url))
        .collect();
    let mut seen_in_batch: HashSet<String> = HashSet::new();

    for candidate in candidates {
        let label = label_of(candidate);
        let raw_url = candidate.candidate_url.trim().to_string();

        if raw_url.is_empty() {
            plan.rejected
                .push((label, "the candidate has no URL".to_string()));
            continue;
        }

        let parsed = match Url::parse(&raw_url) {
            Ok(parsed) => parsed,
            Err(error) => {
                plan.rejected
                    .push((label, format!("malformed URL: {error}")));
                continue;
            }
        };

        let scheme = parsed.scheme().to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            plan.rejected
                .push((label, "only http and https URLs are supported".to_string()));
            continue;
        }

        if let Err(error) =
            validation::validate_url_for_network_opts(&raw_url, allow_private, false)
        {
            plan.rejected.push((label, format!("unsafe URL: {error}")));
            continue;
        }

        let key = normalized_url(&raw_url).unwrap_or_else(|| raw_url.clone());
        if known.contains(&key) {
            plan.duplicates
                .push((label, "already in the download list or history".to_string()));
        } else if seen_in_batch.contains(&key) {
            plan.duplicates
                .push((label, "repeated within the same selection".to_string()));
        } else {
            seen_in_batch.insert(key);
            plan.to_add.push(candidate.clone());
        }
    }

    plan
}

/// Add every eligible candidate through `add_fn`, then start the queue once.
///
/// `add_fn` receives `(url, destination_dir, Some(filename))` and no checksum.
/// Failures are collected; the batch never aborts. `start_fn` is invoked
/// exactly once, after the loop, only when at least one record was added.
pub fn execute_queue_plan(
    plan: QueuePlan,
    destination_dir: &str,
    mut add_fn: impl FnMut(&str, &str, Option<&str>) -> Result<DownloadRecord, String>,
    start_fn: Option<&mut dyn FnMut() -> usize>,
) -> ExecutionReport {
    let mut report = ExecutionReport::default();

    for candidate in plan.to_add {
        let label = label_of(&candidate);
        match add_fn(
            &candidate.candidate_url,
            destination_dir,
            Some(candidate.filename.as_str()),
        ) {
            Ok(record) => report.added.push(record),
            Err(reason) => report.failed.push((label, reason)),
        }
    }

    if !report.added.is_empty()
        && let Some(start) = start_fn
    {
        start();
    }

    report
}

/// Human label for a candidate (`filename`, falling back to its URL).
fn label_of(candidate: &DiscoveryCandidate) -> String {
    if candidate.filename.is_empty() {
        candidate.candidate_url.clone()
    } else {
        candidate.filename.clone()
    }
}

/// Normalise a URL for duplicate detection.
///
/// Scheme and netloc are lowercased, path and query are kept verbatim and the
/// fragment is dropped (the legacy `urlsplit` comparison). Unparsable input
/// yields `None` so malformed history rows are ignored.
fn normalized_url(raw: &str) -> Option<String> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    let parsed = Url::parse(text).ok()?;
    let host = parsed.host_str()?;
    let host = if matches!(parsed.host(), Some(Host::Ipv6(_))) {
        format!("[{host}]")
    } else {
        host.to_ascii_lowercase()
    };

    let mut out = format!("{}://{host}", parsed.scheme().to_ascii_lowercase());
    if let Some(port) = parsed.port() {
        out.push_str(&format!(":{port}"));
    }
    out.push_str(parsed.path());
    if let Some(query) = parsed.query().filter(|query| !query.is_empty()) {
        out.push('?');
        out.push_str(query);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use crate::config::Settings;
    use crate::database::connection::Database;
    use crate::database::repository::DownloadRepository;
    use crate::engine::fake_site::FakeSite;
    use crate::engine::manager::DownloadManager;
    use crate::models::download_status::DownloadState;
    use crate::services::file_naming;

    static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "bdm-planner-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn candidate(url: &str, filename: &str) -> DiscoveryCandidate {
        DiscoveryCandidate {
            candidate_url: url.to_string(),
            filename: filename.to_string(),
            extension: file_naming::classify_extension(url),
            source_page_url: "http://files.example/page".to_string(),
            content_type: None,
            size: None,
            is_likely_file: true,
            anchor_text: None,
            filtered_reason: None,
        }
    }

    fn assert_reason(plan: &QueuePlan, expected: &str) {
        assert_eq!(plan.rejected.len(), 1, "rejected: {:?}", plan.rejected);
        assert!(
            plan.rejected[0].1.contains(expected),
            "reason {:?} does not contain {expected:?}",
            plan.rejected[0].1
        );
    }

    #[test]
    fn checked_candidates_filters_unticked_and_preserves_order() {
        let mut first = candidate("http://files.example/first.zip", "first.zip");
        let second = candidate("http://files.example/second.zip", "second.zip");
        let third = candidate("http://files.example/third.zip", "third.zip");
        first.is_likely_file = false;

        let checked = checked_candidates(&[first, second, third]);
        let names: Vec<&str> = checked
            .iter()
            .map(|candidate| candidate.filename.as_str())
            .collect();
        assert_eq!(names, vec!["second.zip", "third.zip"]);
    }

    #[test]
    fn zero_checked_candidates_plan_is_empty() {
        let plan = plan_discovery_queue(&[], &[], false);
        assert!(plan.is_empty());
    }

    #[test]
    fn batch_and_preexisting_duplicates_are_flagged_not_added() {
        let a = candidate("http://files.example/a.zip", "a.zip");
        let a_copy = candidate("http://files.example/a.zip", "a-copy.zip");
        let b = candidate("http://files.example/b.zip", "b.zip");
        let c = candidate("http://files.example/c.zip", "c.zip");
        let existing = vec![
            " http://files.example/B.zip?x=1 ".to_string(),
            "http://files.example/b.zip".to_string(),
        ];

        let plan = plan_discovery_queue(&[a, a_copy, b, c], &existing, true);

        let added: Vec<&str> = plan
            .to_add
            .iter()
            .map(|candidate| candidate.filename.as_str())
            .collect();
        assert_eq!(added, vec!["a.zip", "c.zip"]);

        let dup_labels: HashSet<&str> = plan
            .duplicates
            .iter()
            .map(|(label, _)| label.as_str())
            .collect();
        assert_eq!(dup_labels, HashSet::from(["a-copy.zip", "b.zip"]));

        let reasons: HashSet<&str> = plan
            .duplicates
            .iter()
            .map(|(_, reason)| reason.as_str())
            .collect();
        assert_eq!(
            reasons,
            HashSet::from([
                "repeated within the same selection",
                "already in the download list or history",
            ])
        );
    }

    #[test]
    fn unusable_urls_are_rejected_with_actionable_reasons() {
        let cases = [
            (
                "ftp://example.com/broken.bin",
                "only http and https URLs are supported",
            ),
            ("not a url", "malformed URL"),
            ("", "the candidate has no URL"),
        ];
        for (url, expected) in cases {
            let plan = plan_discovery_queue(&[candidate(url, "broken.bin")], &[], false);
            assert!(plan.to_add.is_empty());
            assert_eq!(plan.rejected[0].0, "broken.bin");
            assert_reason(&plan, expected);
        }

        let mut private = candidate("http://127.0.0.1:9/x.zip", "x.zip");
        private.is_likely_file = true;
        let plan = plan_discovery_queue(&[private.clone()], &[], false);
        assert_reason(&plan, "unsafe URL");
        let allowed = plan_discovery_queue(&[private], &[], true);
        assert_eq!(allowed.to_add.len(), 1);
        assert!(allowed.rejected.is_empty());
    }

    #[test]
    fn execute_collects_add_failures_and_starts_once() {
        let plan = QueuePlan {
            to_add: vec![
                candidate("http://files.example/one.zip", "one.zip"),
                candidate("http://files.example/two.zip", "two.zip"),
                candidate("http://files.example/three.zip", "three.zip"),
                candidate("http://files.example/four.zip", "four.zip"),
            ],
            ..QueuePlan::default()
        };

        let mut seen_urls: Vec<String> = Vec::new();
        let mut starts = 0usize;
        let mut start = || {
            starts += 1;
            2
        };

        let report = execute_queue_plan(
            plan,
            "C:/downloads",
            |url: &str, _dir: &str, name: Option<&str>| {
                seen_urls.push(url.to_string());
                if url.ends_with("two.zip") || url.ends_with("four.zip") {
                    return Err("manager refused".to_string());
                }
                Ok(DownloadRecord::new_queued(
                    url,
                    name.unwrap_or(""),
                    "C:/downloads/x",
                ))
            },
            Some(&mut start),
        );

        assert_eq!(report.added.len(), 2);
        let failed_labels: Vec<&str> = report
            .failed
            .iter()
            .map(|(label, _)| label.as_str())
            .collect();
        assert_eq!(failed_labels, vec!["two.zip", "four.zip"]);
        assert_eq!(seen_urls.len(), 4);
        assert!(seen_urls.iter().any(|url| url.ends_with("one.zip")));
        assert!(seen_urls.iter().any(|url| url.ends_with("four.zip")));
        assert_eq!(starts, 1);
    }

    #[test]
    fn execute_skips_start_when_nothing_is_added() {
        let plan = QueuePlan {
            to_add: vec![candidate("http://files.example/one.zip", "one.zip")],
            ..QueuePlan::default()
        };
        let mut starts = 0usize;
        let mut start = || {
            starts += 1;
            0
        };
        let report = execute_queue_plan(
            plan,
            "C:/downloads",
            |_url: &str, _dir: &str, _name: Option<&str>| Err("nope".to_string()),
            Some(&mut start),
        );
        assert!(report.added.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(starts, 0);
    }

    #[tokio::test]
    async fn full_workflow_adds_ticked_files_and_replans_cleanly() {
        let site = FakeSite::spawn().await;
        let dir = temp_dir("workflow");
        let database = Database::init_db(dir.join("test.sqlite3")).expect("open database");
        let repository = Arc::new(DownloadRepository::new(Arc::new(database)));
        let settings = Settings {
            default_destination_dir: dir.to_string_lossy().into_owned(),
            max_simultaneous: 2,
            connection_timeout: 5.0,
            retry_delay: 0.01,
            allow_private_addresses: true,
            ..Settings::default()
        };
        let manager = DownloadManager::new(settings, Arc::clone(&repository));

        let data = candidate(&site.url("/files/data.bin"), "data.bin");
        let mut unticked = candidate(&site.url("/files/big.zip"), "big.zip");
        unticked.is_likely_file = false;
        let small = candidate(&site.url("/files/small.zip"), "small.zip");

        let checked = checked_candidates(&[data, unticked, small]);
        assert_eq!(checked.len(), 2);

        let plan = plan_discovery_queue(&checked, &[], true);
        assert_eq!(plan.to_add.len(), 2);
        assert!(plan.rejected.is_empty());
        assert!(plan.duplicates.is_empty());

        let destination = dir.to_string_lossy().into_owned();
        let mut start = || manager.start_all();
        let report = execute_queue_plan(
            plan,
            &destination,
            |url: &str, destination: &str, name: Option<&str>| {
                manager.add_download(url, name, Some(Path::new(destination)), None)
            },
            Some(&mut start),
        );

        assert_eq!(report.added.len(), 2, "failed: {:?}", report.failed);
        assert!(report.failed.is_empty());

        let ids: Vec<i64> = report.added.iter().filter_map(|record| record.id).collect();
        assert_eq!(ids.len(), 2);

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let rows: Vec<DownloadRecord> = ids
                .iter()
                .filter_map(|id| repository.get(*id).expect("read download"))
                .collect();
            if rows.len() == 2 && rows.iter().all(|row| row.state.is_terminal()) {
                break;
            }
            assert!(Instant::now() < deadline, "downloads did not settle");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        for (id, expected_name) in ids.iter().zip(["data.bin", "small.zip"]) {
            let record = repository
                .get(*id)
                .expect("read download")
                .expect("download exists");
            assert_eq!(record.state, DownloadState::Completed);
            assert_eq!(record.display_name, expected_name);
            let path = PathBuf::from(&record.destination_path);
            let bytes = std::fs::read(&path).expect("read downloaded file");
            assert_eq!(bytes, site.file_bytes(expected_name).expect("fixture file"));
        }
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("list destination")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "leftover part files: {leftovers:?}");

        let existing: Vec<String> = report
            .added
            .iter()
            .map(|record| record.source_url.clone())
            .collect();
        let replanned = plan_discovery_queue(&checked, &existing, true);
        assert!(replanned.to_add.is_empty());
        assert!(replanned.rejected.is_empty());
        let labels: HashSet<&str> = replanned
            .duplicates
            .iter()
            .map(|(label, _)| label.as_str())
            .collect();
        assert_eq!(labels, HashSet::from(["data.bin", "small.zip"]));
    }
}
