//! The analysis pipeline: intake → validation → ad/tracker filtering →
//! metadata probing → multipart grouping → classification → scoring.
//!
//! Deterministic and offline: the only network activity is the optional,
//! bounded metadata probe, and classification itself works entirely from
//! data already available. No external AI service is contacted.

use std::collections::HashSet;

use tokio_util::sync::CancellationToken;

use crate::config::Settings;
use crate::models::diagnostic::host_from_url;
use crate::models::discovered_link::DiscoveryCandidate;
use crate::services::ad_filter::AdFilter;
use crate::services::file_naming::filename_from_url;
use crate::services::validation::{ValidatedUrl, redact_url, validate_url_for_network_opts};

use super::archive::{self, SignatureOutcome};
use super::classifier::{CandidateInput, Classifier, RuleBasedClassifier, Verdict};
use super::metadata::{MetadataProbe, ProbeError, ProbeOutcome};
use super::multipart::{self, ArchiveGroup, GroupMember, GroupStatus};
use super::report::{
    AnalysisReport, AnalysisSource, CandidateReport, GroupReport, build_group_reports,
};
use super::scoring::{self, Confidence, Evidence};

/// Upper bound on items analysed in one run, mirroring discovery's cap.
pub const MAX_ANALYSIS_ITEMS: usize = 500;
/// Upper bound on bounded HTTP probes per run (each is at most two requests).
pub const MAX_PROBES: usize = 40;
/// Score at or above this counts as an archive candidate for the summary.
pub const CANDIDATE_THRESHOLD: u8 = 50;

/// One analysis input row.
#[derive(Debug, Clone)]
struct AnalysisItem {
    url: String,
    filename: String,
    source_page_url: String,
    anchor_text: Option<String>,
    content_type: Option<String>,
    size: Option<u64>,
    filtered_reason: Option<String>,
    is_likely_file: bool,
    validated: Option<ValidatedUrl>,
}

/// The lightweight local intelligence engine.
pub struct FileIntelligenceEngine {
    settings: Settings,
    ad_filter: AdFilter,
    probe: MetadataProbe,
    classifier: RuleBasedClassifier,
}

impl FileIntelligenceEngine {
    pub fn new(settings: &Settings) -> Self {
        Self {
            settings: settings.clone(),
            ad_filter: AdFilter::from_settings(settings),
            probe: MetadataProbe::new(settings),
            classifier: RuleBasedClassifier,
        }
    }

    /// Analyse a batch of already-discovered candidates.
    pub async fn analyze_candidates(
        &self,
        candidates: Vec<DiscoveryCandidate>,
        probe_metadata: bool,
        cancel: &CancellationToken,
    ) -> AnalysisReport {
        let items: Vec<AnalysisItem> = candidates
            .into_iter()
            .map(|candidate| AnalysisItem {
                filename: candidate.filename,
                url: candidate.candidate_url,
                source_page_url: candidate.source_page_url,
                anchor_text: candidate.anchor_text,
                content_type: candidate.content_type,
                size: candidate.size,
                filtered_reason: candidate.filtered_reason,
                is_likely_file: candidate.is_likely_file,
                validated: None,
            })
            .collect();
        self.analyze(items, AnalysisSource::Discovered, probe_metadata, cancel)
            .await
    }

    /// Analyse raw URL lines pasted by the user.
    pub async fn analyze_urls(
        &self,
        urls: Vec<String>,
        probe_metadata: bool,
        cancel: &CancellationToken,
    ) -> AnalysisReport {
        let items: Vec<AnalysisItem> = urls
            .into_iter()
            .map(|url| AnalysisItem {
                filename: filename_from_url(&url),
                url,
                source_page_url: String::new(),
                anchor_text: None,
                content_type: None,
                size: None,
                filtered_reason: None,
                is_likely_file: false,
                validated: None,
            })
            .collect();
        self.analyze(items, AnalysisSource::Pasted, probe_metadata, cancel)
            .await
    }

    async fn analyze(
        &self,
        mut items: Vec<AnalysisItem>,
        source: AnalysisSource,
        probe_metadata: bool,
        cancel: &CancellationToken,
    ) -> AnalysisReport {
        let started = std::time::Instant::now();
        let mut errors = Vec::new();

        // Deduplicate by URL, preserving discovery order.
        let mut seen: HashSet<String> = HashSet::new();
        items.retain(|item| seen.insert(item.url.clone()));

        if items.len() > MAX_ANALYSIS_ITEMS {
            errors.push(format!(
                "the batch was limited to the first {MAX_ANALYSIS_ITEMS} unique URL(s); {} were skipped",
                items.len() - MAX_ANALYSIS_ITEMS
            ));
            items.truncate(MAX_ANALYSIS_ITEMS);
        }

        // 1+2: URL syntax and SSRF preflight (no DNS; the download worker
        // revalidates with DNS and pinning before any transfer).
        for item in &mut items {
            match validate_url_for_network_opts(
                &item.url,
                self.settings.allow_private_addresses,
                false,
            ) {
                Ok(validated) => item.validated = Some(validated),
                Err(error) => {
                    errors.push(format!("{} — {}", redact_url(&item.url), error));
                    item.filtered_reason = Some("failed URL validation".to_string());
                }
            }
        }

        // 3: ad/tracker classification on the host.
        for item in &mut items {
            if item.filtered_reason.is_none()
                && let Some(reason) = self.ad_filter.check_host(&host_from_url(&item.url))
            {
                item.filtered_reason = Some(reason);
            }
        }

        // 4: bounded metadata probing, opt-in.
        let mut probe_notes: Vec<String> = Vec::new();
        let mut probe_results: Vec<Option<ProbeOutcome>> = vec![None; items.len()];
        if probe_metadata && !cancel.is_cancelled() {
            let mut probes = 0usize;
            for (index, item) in items.iter().enumerate() {
                if probes >= MAX_PROBES {
                    probe_notes.push(format!(
                        "probing was limited to the first {MAX_PROBES} URL(s)"
                    ));
                    break;
                }
                if item.filtered_reason.is_some() || item.validated.is_none() {
                    continue;
                }
                let name = archive::analyse_filename(&item.filename);
                let expected = name
                    .as_ref()
                    .filter(|analysis| analysis.is_archive)
                    .map(|analysis| analysis.format);
                // Probe candidates and unknowns only; ordinary page links do
                // not deserve requests.
                if expected.is_none() && !item.is_likely_file {
                    continue;
                }
                probes += 1;
                match self.probe.probe(&item.url, expected, cancel).await {
                    Ok(outcome) => probe_results[index] = Some(outcome),
                    Err(ProbeError::Cancelled) => {
                        probe_notes.push("probing stopped: analysis cancelled".to_string());
                        break;
                    }
                    Err(error) => {
                        probe_notes.push(format!(
                            "{} — {}",
                            redact_url(&item.url),
                            error.message()
                        ));
                    }
                }
            }
        }

        // 5: multipart grouping across all non-excluded items.
        let grouped: Vec<(usize, archive::NameAnalysis)> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.filtered_reason.is_none())
            .filter_map(|(index, item)| {
                archive::analyse_filename(&item.filename).map(|analysis| (index, analysis))
            })
            .collect();
        let references: Vec<(String, String)> = grouped
            .iter()
            .map(|(index, analysis)| {
                let url = &items[*index].url;
                (analysis.filename.clone(), url.clone())
            })
            .collect();
        let groups: Vec<ArchiveGroup> = multipart::group_multipart(
            references
                .iter()
                .map(|(name, url)| (name.as_str(), url.as_str())),
        );

        // Resolve group membership back onto item indexes.
        let mut group_of: std::collections::HashMap<usize, (&ArchiveGroup, u32)> =
            std::collections::HashMap::new();
        for group in &groups {
            for member in &group.members {
                if let Some((index, _)) = grouped
                    .iter()
                    .find(|(_, analysis)| analysis.filename == member.filename)
                {
                    group_of.insert(*index, (group, member.number));
                }
            }
        }

        // 6+7: classify and score.
        let mut candidates = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let probe_outcome = probe_results[index].clone();
            let (group, part_number) = group_of
                .get(&index)
                .map_or((None, None), |(group, number)| {
                    (Some(*group), Some(*number))
                });

            let filename = item.filename.clone();
            let name_analysis = archive::analyse_filename(&filename);
            let filename_format = name_analysis
                .as_ref()
                .filter(|analysis| analysis.is_archive)
                .map(|analysis| analysis.format);

            let mut content_type = item.content_type.clone();
            let mut disposition = None;
            let mut size = item.size;
            let mut signature: Option<SignatureOutcome> = None;
            if let Some(outcome) = &probe_outcome {
                if outcome.content_type.is_some() {
                    content_type = outcome.content_type.clone();
                }
                disposition = outcome.disposition_filename.clone();
                if outcome.size.is_some() {
                    size = outcome.size;
                }
                signature = outcome.signature;
            }

            let extension = item
                .validated
                .as_ref()
                .map(|validated| crate::services::file_naming::classify_extension(&validated.url))
                .unwrap_or_else(|| crate::services::file_naming::classify_extension(&item.url));

            let input = CandidateInput {
                filename,
                url: item.url.clone(),
                extension,
                content_type,
                disposition_filename: disposition,
                size,
                signature,
                filtered_reason: item.filtered_reason.clone(),
                group_status: group.map(|group| group.status),
                filename_format,
            };

            let classification = self.classifier.classify(&input);
            let mut evidence = classification.evidence.clone();

            // Record what the analysis could not see.
            let mut missing_evidence = Vec::new();
            if probe_outcome.is_none() && input.content_type.is_none() {
                missing_evidence
                    .push("Server metadata (Content-Type) was not checked.".to_string());
            }
            if probe_metadata
                && filename_format.is_some()
                && signature.is_none()
                && item.filtered_reason.is_none()
            {
                missing_evidence
                    .push("The archive signature could not be checked for this URL.".to_string());
            }
            if let Some(outcome) = &probe_outcome
                && let Some(note) = &outcome.note
            {
                missing_evidence.push(note.clone());
            }
            if !probe_metadata {
                missing_evidence.push(
                    "HTTP verification was off, so only names and discovery data were used."
                        .to_string(),
                );
            }
            if evidence.is_empty() {
                evidence.push(Evidence::new(
                    0,
                    "No archive evidence was found for this link.".to_string(),
                ));
            }
            evidence.sort_by_key(|item| std::cmp::Reverse(item.weight));

            let score = scoring::total_score(&evidence);
            let confidence = Confidence::from_score(score);
            let status = status_label(
                classification.verdict,
                signature,
                input.content_type.as_deref(),
                group.map(|group| group.status),
                score,
            );

            candidates.push(CandidateReport {
                url: item.url.clone(),
                filename: item.filename.clone(),
                source_page_url: item.source_page_url.clone(),
                extension: input.extension.clone(),
                archive_type: filename_format
                    .map(|format| format.label().to_string())
                    .unwrap_or_else(|| "Not an archive".to_string()),
                format: filename_format,
                score,
                confidence,
                status,
                size,
                content_type: input.content_type.clone(),
                signature,
                group_key: group.map(|group| group.key.clone()),
                part_number: part_number.filter(|number| *number != u32::MAX),
                evidence,
                missing_evidence,
                filtered_reason: item.filtered_reason.clone(),
                anchor_text: item.anchor_text.clone(),
            });
        }

        // Highest score first, ties alphabetical — the "ranking engine".
        candidates.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.filename.cmp(&right.filename))
        });

        let format_label = |format: Option<archive::ArchiveFormat>| -> String {
            format
                .map(|format| format.label().to_string())
                .unwrap_or_else(|| "Split archive".to_string())
        };
        let index_of = |member: &GroupMember| -> Option<usize> {
            candidates
                .iter()
                .position(|candidate| candidate.url == member.url)
        };
        let score_of = |member: &GroupMember| -> u8 {
            index_of(member)
                .and_then(|index| candidates.get(index))
                .map_or(0, |candidate| candidate.score)
        };
        let group_reports: Vec<GroupReport> =
            build_group_reports(&groups, format_label, score_of, index_of);

        let candidate_count = candidates
            .iter()
            .filter(|candidate| candidate.score >= CANDIDATE_THRESHOLD)
            .count();
        let filtered_count = candidates
            .iter()
            .filter(|candidate| candidate.filtered_reason.is_some())
            .count();
        let unverified_count = candidates
            .iter()
            .filter(|candidate| {
                candidate.format.is_some()
                    && candidate
                        .signature
                        .is_none_or(|outcome| outcome == SignatureOutcome::Unavailable)
            })
            .count();
        let complete_groups = group_reports
            .iter()
            .filter(|group| group.status == GroupStatus::Complete)
            .count();
        let incomplete_groups = group_reports
            .iter()
            .filter(|group| {
                matches!(
                    group.status,
                    GroupStatus::Incomplete | GroupStatus::Duplicate | GroupStatus::Inconsistent
                )
            })
            .count();

        errors.extend(probe_notes);

        AnalysisReport {
            created_at: crate::models::download_item::now_iso(),
            source,
            candidates,
            groups: group_reports,
            analyzed_count: items.len(),
            candidate_count,
            filtered_count,
            unverified_count,
            complete_groups,
            incomplete_groups,
            errors,
            elapsed_seconds: started.elapsed().as_secs_f64(),
            probed: probe_metadata,
        }
    }
}

/// The short status line shown on each candidate row.
fn status_label(
    verdict: Verdict,
    signature: Option<SignatureOutcome>,
    content_type: Option<&str>,
    group: Option<GroupStatus>,
    score: u8,
) -> String {
    if verdict == Verdict::Excluded {
        return "Filtered (ad/tracker)".to_string();
    }
    if content_type.is_some_and(scoring::is_html_content_type) {
        return "Webpage (HTML response)".to_string();
    }
    match verdict {
        Verdict::Excluded => "Filtered (ad/tracker)".to_string(),
        Verdict::NotArchive => "Not an archive".to_string(),
        Verdict::Unknown => "Needs review".to_string(),
        Verdict::ArchiveCandidate => {
            let verified = matches!(signature, Some(SignatureOutcome::Verified(_)));
            let group_complete = group == Some(GroupStatus::Complete);
            let mut label = if verified {
                "Archive candidate (signature checked)".to_string()
            } else {
                "Unverified archive candidate".to_string()
            };
            if group.is_some() {
                if group_complete {
                    label.push_str(" · group looks complete");
                } else {
                    label.push_str(" · multipart group");
                }
            }
            if score < CANDIDATE_THRESHOLD {
                label.push_str(" (low confidence)");
            }
            label
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::engine::fake_site::FakeSite;
    use crate::file_intelligence::archive::{ArchiveFormat, SignatureOutcome};
    use crate::file_intelligence::multipart::GroupStatus;
    use crate::file_intelligence::scoring::Confidence;
    use tokio_util::sync::CancellationToken;

    fn engine_settings() -> Settings {
        Settings {
            // The fixture lives on the loopback interface.
            allow_private_addresses: true,
            connection_timeout: 5.0,
            ..Settings::default()
        }
    }

    fn zip_bytes() -> Vec<u8> {
        let mut bytes = b"PK\x03\x04".to_vec();
        bytes.extend_from_slice(&[0u8; 128]);
        bytes
    }

    fn rar_bytes() -> Vec<u8> {
        let mut bytes = b"Rar!\x1a\x07\x01\x00".to_vec();
        bytes.extend_from_slice(&[0u8; 128]);
        bytes
    }

    async fn analyze_urls(
        settings: &Settings,
        urls: Vec<String>,
        probe: bool,
        token: &CancellationToken,
    ) -> AnalysisReport {
        let engine = FileIntelligenceEngine::new(settings);
        engine.analyze_urls(urls, probe, token).await
    }

    #[tokio::test]
    async fn a_real_zip_scores_high_and_is_verified() {
        let site = FakeSite::spawn().await;
        site.set_file("bundle.zip", zip_bytes());
        let url = site.url("/files/bundle.zip");

        let report = analyze_urls(
            &engine_settings(),
            vec![url.clone()],
            true,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(report.analyzed_count, 1);
        let candidate = &report.candidates[0];
        assert_eq!(candidate.filename, "bundle.zip");
        assert_eq!(
            candidate.signature,
            Some(SignatureOutcome::Verified(ArchiveFormat::Zip))
        );
        assert!(candidate.score >= 75, "score was {}", candidate.score);
        assert!(matches!(
            candidate.confidence,
            Confidence::VeryHigh | Confidence::High
        ));
        assert!(candidate.status.contains("signature checked"));
        assert_eq!(report.candidate_count, 1);
    }

    #[tokio::test]
    async fn an_html_page_is_never_an_archive() {
        let site = FakeSite::spawn().await;
        let url = site.url("/html-as-file");
        // Discovery would hand over a named candidate; the probe still wins.
        let candidate = DiscoveryCandidate {
            candidate_url: url,
            filename: "download.html".to_string(),
            extension: Some("html".to_string()),
            source_page_url: site.url("/page"),
            content_type: None,
            size: None,
            is_likely_file: true,
            anchor_text: Some("Download".to_string()),
            filtered_reason: None,
        };

        let engine = FileIntelligenceEngine::new(&engine_settings());
        let report = engine
            .analyze_candidates(vec![candidate], true, &CancellationToken::new())
            .await;
        let candidate = &report.candidates[0];
        assert!(candidate.score < 25, "score was {}", candidate.score);
        assert!(candidate.status.starts_with("Webpage"));
        assert_eq!(report.candidate_count, 0);
    }

    #[tokio::test]
    async fn an_html_response_named_rar_stays_low() {
        // The candidate is named .rar but the server answers with HTML.
        let site = FakeSite::spawn().await;
        let candidate = DiscoveryCandidate {
            candidate_url: site.url("/challenge-200"),
            filename: "trap.rar".to_string(),
            extension: Some("rar".to_string()),
            source_page_url: site.url("/page"),
            content_type: None,
            size: None,
            is_likely_file: true,
            anchor_text: Some("Download".to_string()),
            filtered_reason: None,
        };

        let engine = FileIntelligenceEngine::new(&engine_settings());
        let report = engine
            .analyze_candidates(vec![candidate], true, &CancellationToken::new())
            .await;
        let candidate = &report.candidates[0];
        assert!(candidate.score < 25, "score was {}", candidate.score);
    }

    #[tokio::test]
    async fn multipart_groups_are_assembled_and_scored() {
        let site = FakeSite::spawn().await;
        site.set_file("Demo.part01.rar", rar_bytes());
        site.set_file("Demo.part02.rar", rar_bytes());
        site.set_file("Demo.part03.rar", rar_bytes());
        let part_two_url = site.url("/files/Demo.part01.rar");
        let urls = vec![
            site.url("/files/Demo.part02.rar"),
            part_two_url.clone(),
            site.url("/files/Demo.part03.rar"),
        ];

        let report = analyze_urls(&engine_settings(), urls, true, &CancellationToken::new()).await;
        assert_eq!(report.groups.len(), 1);
        let group = &report.groups[0];
        assert_eq!(group.status, GroupStatus::Complete);
        assert_eq!(group.parts.len(), 3);
        // Group members keep their original URLs.
        assert!(
            report
                .candidates
                .iter()
                .any(|candidate| candidate.url == part_two_url && candidate.part_number == Some(1))
        );
        for candidate in report.candidates.iter().filter(|c| c.group_key.is_some()) {
            assert!(candidate.score >= 50, "score was {}", candidate.score);
        }
    }

    #[tokio::test]
    async fn incomplete_groups_are_flagged() {
        let site = FakeSite::spawn().await;
        site.set_file("Gap.part02.rar", rar_bytes());
        site.set_file("Gap.part03.rar", rar_bytes());
        let urls = vec![
            site.url("/files/Gap.part02.rar"),
            site.url("/files/Gap.part03.rar"),
        ];

        let report = analyze_urls(&engine_settings(), urls, true, &CancellationToken::new()).await;
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].status, GroupStatus::Incomplete);
        assert_eq!(report.incomplete_groups, 1);
        assert_eq!(report.complete_groups, 0);
    }

    #[tokio::test]
    async fn ad_and_tracker_destinations_are_excluded() {
        let mut settings = engine_settings();
        settings.ad_filter_enabled = true;
        let url = "https://doubleclick.net/files/archive.zip".to_string();

        let report = analyze_urls(&settings, vec![url], true, &CancellationToken::new()).await;
        let candidate = &report.candidates[0];
        assert!(candidate.filtered_reason.is_some());
        assert_eq!(candidate.score, 0);
        assert!(!candidate.is_selectable());
        assert_eq!(report.filtered_count, 1);
        assert_eq!(report.candidate_count, 0);
    }

    #[tokio::test]
    async fn a_query_token_never_proves_an_archive() {
        // The filename from a query-string-only link is not an archive.
        let url = "https://files.example.com/downloads?file=secret.rar&token=abc123".to_string();

        let report = analyze_urls(
            &engine_settings(),
            vec![url],
            false,
            &CancellationToken::new(),
        )
        .await;
        let candidate = &report.candidates[0];
        assert_ne!(candidate.filename, "secret.rar");
        assert!(candidate.format.is_none());
        assert!(candidate.score < 25, "score was {}", candidate.score);
    }

    #[tokio::test]
    async fn unsafe_destinations_are_reported_with_redacted_urls() {
        let mut settings = engine_settings();
        settings.allow_private_addresses = false;
        let url = "http://127.0.0.1:9/files/archive.zip?token=supersecret".to_string();

        let report = analyze_urls(&settings, vec![url], false, &CancellationToken::new()).await;
        // The problem is recorded, without the query string.
        assert!(
            report
                .errors
                .iter()
                .any(|error| { error.contains("private") && !error.contains("supersecret") })
        );
        // The rejected link is excluded from queuing.
        assert!(
            report
                .candidates
                .iter()
                .all(|candidate| !candidate.is_selectable())
        );
    }

    #[tokio::test]
    async fn duplicates_collapse_to_one_row() {
        let url = "https://files.example.com/thing.7z".to_string();
        let report = analyze_urls(
            &engine_settings(),
            vec![url.clone(), url],
            false,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(report.analyzed_count, 1);
    }

    #[tokio::test]
    async fn cancelled_runs_skip_probing() {
        let site = FakeSite::spawn().await;
        site.set_file("late.zip", zip_bytes());
        let url = site.url("/files/late.zip");

        let token = CancellationToken::new();
        token.cancel();
        let report = analyze_urls(&engine_settings(), vec![url], true, &token).await;
        // No probe notes and no signature data: the run was cut short.
        assert!(report.candidates.iter().all(|c| c.signature.is_none()));
    }

    #[tokio::test]
    async fn discovered_candidates_are_analyzed_end_to_end() {
        let site = FakeSite::spawn().await;
        site.set_file(
            "pack.7z",
            vec![0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c, 0x00, 0x04, 0x00, 0x00],
        );
        let url = site.url("/files/pack.7z");

        let candidate = DiscoveryCandidate {
            candidate_url: url,
            filename: "pack.7z".to_string(),
            extension: Some("7z".to_string()),
            source_page_url: site.url("/page"),
            content_type: None,
            size: None,
            is_likely_file: true,
            anchor_text: Some("Download".to_string()),
            filtered_reason: None,
        };

        let engine = FileIntelligenceEngine::new(&engine_settings());
        let report = engine
            .analyze_candidates(vec![candidate], true, &CancellationToken::new())
            .await;
        let candidate = &report.candidates[0];
        assert_eq!(candidate.source_page_url, site.url("/page"));
        assert!(candidate.score >= 75, "score was {}", candidate.score);
        assert_eq!(report.source, AnalysisSource::Discovered);
    }
}
