//! Candidate classification.
//!
//! The rule-based classifier is the authoritative security- and evidence
//! layer: it turns filename, HTTP metadata and signature observations into a
//! verdict plus scored evidence. A future local AI model can be added behind
//! the same [`Classifier`] trait, but it must never override URL safety,
//! SSRF, redirect validation or signature checks — the deterministic rules
//! decide what may be queued.

use super::archive::{self, ArchiveFormat, SignatureOutcome};
use super::multipart::GroupStatus;
use super::scoring::{self, Evidence, content_type_format, is_html_content_type, is_octet_stream};

/// What the classifier decided about one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Treat as a potential archive download.
    ArchiveCandidate,
    /// Not a direct archive (webpage or unknown non-archive link).
    NotArchive,
    /// Excluded outright (known ad/tracker destination).
    Excluded,
    /// No useful evidence either way; kept for manual review.
    Unknown,
}

/// Everything the classifier is allowed to look at for one candidate.
#[derive(Debug, Clone)]
pub struct CandidateInput {
    /// Percent-decoded filename from the URL or Content-Disposition.
    pub filename: String,
    /// The candidate URL (already validated by the caller).
    pub url: String,
    /// Lowercased path extension, when present.
    pub extension: Option<String>,
    /// Media type from the server, when probed.
    pub content_type: Option<String>,
    /// Filename suggested by `Content-Disposition`, when probed.
    pub disposition_filename: Option<String>,
    /// Advertised size, when available.
    pub size: Option<u64>,
    /// Signature check outcome, when probed.
    pub signature: Option<SignatureOutcome>,
    /// Ad/tracker filter reason, when the destination matched a rule.
    pub filtered_reason: Option<String>,
    /// Multipart group status for this candidate, when grouped.
    pub group_status: Option<GroupStatus>,
    /// The archive family the filename suggests, when any.
    pub filename_format: Option<ArchiveFormat>,
}

impl CandidateInput {
    /// Build the input from what is known before any network probing.
    pub fn without_probe(
        filename: String,
        url: String,
        extension: Option<String>,
        filtered_reason: Option<String>,
        content_type: Option<String>,
    ) -> Self {
        let filename_format = archive::analyse_filename(&filename)
            .filter(|analysis| analysis.is_archive)
            .map(|analysis| analysis.format);
        Self {
            filename,
            url,
            extension,
            content_type,
            disposition_filename: None,
            size: None,
            signature: None,
            filtered_reason,
            group_status: None,
            filename_format,
        }
    }
}

/// Classification output: verdict plus the evidence behind it.
#[derive(Debug, Clone)]
pub struct Classification {
    pub verdict: Verdict,
    pub evidence: Vec<Evidence>,
}

/// The seam where a local AI model could later plug in.
///
/// Implementations must be pure functions of the input (no I/O) so scores
/// stay deterministic and testable. The engine may combine several
/// classifiers, but security-relevant exclusions always come from the
/// rule-based layer.
pub trait Classifier: Send + Sync {
    /// Stable name shown in diagnostics.
    fn name(&self) -> &'static str;

    /// Classify one candidate.
    fn classify(&self, input: &CandidateInput) -> Classification;
}

/// The deterministic, explainable classifier.
#[derive(Debug, Default, Clone, Copy)]
pub struct RuleBasedClassifier;

impl Classifier for RuleBasedClassifier {
    fn name(&self) -> &'static str {
        "rules"
    }

    fn classify(&self, input: &CandidateInput) -> Classification {
        // Security exclusions always come first and cannot be overridden.
        if let Some(reason) = &input.filtered_reason {
            return Classification {
                verdict: Verdict::Excluded,
                evidence: vec![Evidence::new(
                    scoring::WEIGHT_AD_FILTERED,
                    format!("Matched the ad/tracker filter: {reason}"),
                )],
            };
        }

        let mut evidence = Vec::new();

        // 1. Filename evidence.
        if let Some(format) = input.filename_format {
            evidence.push(Evidence::new(
                scoring::WEIGHT_ARCHIVE_EXTENSION,
                format!(
                    "The filename ends in a recognised {} extension.",
                    format.label()
                ),
            ));
        }

        // 2. Content-Disposition evidence.
        if let Some(disposition) = &input.disposition_filename
            && archive::analyse_filename(disposition).is_some_and(|analysis| analysis.is_archive)
        {
            evidence.push(Evidence::new(
                scoring::WEIGHT_DISPOSITION_ARCHIVE,
                format!("The server suggested the filename “{disposition}”."),
            ));
        }

        // 3. Content-Type evidence.
        if let Some(content_type) = &input.content_type {
            if is_html_content_type(content_type) {
                evidence.push(Evidence::new(
                    scoring::WEIGHT_HTML_RESPONSE,
                    "The server returned an HTML page, not a downloadable file.",
                ));
            } else if let Some(format) = content_type_format(content_type) {
                evidence.push(Evidence::new(
                    scoring::WEIGHT_CONTENT_TYPE_ARCHIVE,
                    format!("The server reports {} content.", format.label()),
                ));
            } else if is_octet_stream(content_type) {
                evidence.push(Evidence::new(
                    scoring::WEIGHT_CONTENT_TYPE_OCTET_STREAM,
                    "The server reports generic binary content.",
                ));
            }
        }

        // 4. Signature evidence.
        if let Some(outcome) = input.signature {
            if let Some(expected) = input.filename_format {
                evidence.extend(scoring::signature_evidence(expected, outcome));
            } else if let Some(found) = outcome.format() {
                evidence.push(Evidence::new(
                    scoring::WEIGHT_SIGNATURE_DIFFERENT,
                    format!("The response carries a {} signature.", found.label()),
                ));
            } else if outcome == SignatureOutcome::Mismatch {
                evidence.push(Evidence::new(
                    scoring::WEIGHT_SIGNATURE_MISMATCH,
                    "The response bytes do not look like any known archive signature.",
                ));
            }
        }

        // 5. Multipart group evidence.
        if let Some(status) = input.group_status {
            match status {
                GroupStatus::Complete => evidence.push(Evidence::new(
                    scoring::WEIGHT_MULTIPART_CONSISTENT,
                    "The multipart naming sequence looks consistent.",
                )),
                GroupStatus::Incomplete | GroupStatus::Undetermined => {
                    evidence.push(Evidence::new(
                        scoring::WEIGHT_MULTIPART_INCOMPLETE,
                        format!("The multipart group is {}.", status.label().to_lowercase()),
                    ))
                }
                GroupStatus::Duplicate | GroupStatus::Inconsistent => evidence.push(Evidence::new(
                    scoring::WEIGHT_MULTIPART_CONFLICTED,
                    format!("The multipart naming is {}.", status.label().to_lowercase()),
                )),
            }
        }

        // 6. Webpage fallback: no archive signal at all reads as a link, not
        // a file. One weak positive (e.g. octet-stream) keeps it reviewable.
        let has_positive = evidence.iter().any(|item| item.weight > 0);
        if !has_positive && input.filename_format.is_none() {
            evidence.push(Evidence::new(
                scoring::WEIGHT_PLAIN_WEBPAGE,
                "No archive signal in the filename or metadata; treat as a webpage link.",
            ));
        }

        let verdict = decide_verdict(input, &evidence);
        Classification { verdict, evidence }
    }
}

/// Combine the evidence into a verdict.
fn decide_verdict(input: &CandidateInput, evidence: &[Evidence]) -> Verdict {
    let html = input
        .content_type
        .as_deref()
        .is_some_and(is_html_content_type);
    let has_extension = input.filename_format.is_some();

    if html {
        // An HTML response caps any archive score; still reviewable.
        return Verdict::NotArchive;
    }
    if has_extension {
        return Verdict::ArchiveCandidate;
    }
    let positives: i32 = evidence.iter().map(|item| i32::from(item.weight)).sum();
    if positives > 0 {
        return Verdict::ArchiveCandidate;
    }
    if input.group_status.is_some() {
        return Verdict::ArchiveCandidate;
    }
    if evidence.is_empty() {
        return Verdict::Unknown;
    }
    Verdict::NotArchive
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_intelligence::archive::{ArchiveFormat, SignatureOutcome};
    use crate::file_intelligence::multipart::GroupStatus;
    use crate::file_intelligence::scoring::{
        WEIGHT_HTML_RESPONSE, WEIGHT_SIGNATURE_VERIFIED, total_score,
    };

    fn input() -> CandidateInput {
        CandidateInput::without_probe(
            "archive.zip".to_string(),
            "https://files.example.com/archive.zip".to_string(),
            Some("zip".to_string()),
            None,
            None,
        )
    }

    #[test]
    fn archive_extension_is_a_candidate() {
        let result = RuleBasedClassifier.classify(&input());
        assert_eq!(result.verdict, Verdict::ArchiveCandidate);
        assert!(
            result
                .evidence
                .iter()
                .any(|item| item.weight
                    == crate::file_intelligence::scoring::WEIGHT_ARCHIVE_EXTENSION)
        );
    }

    #[test]
    fn ad_filtered_destinations_are_excluded_regardless_of_names() {
        let mut candidate = input();
        candidate.filtered_reason =
            Some("matched ad/tracker domain \"ads.example.com\"".to_string());
        // Even with a perfect-looking name and signature.
        candidate.filename_format = Some(ArchiveFormat::Rar);
        candidate.signature = Some(SignatureOutcome::Verified(ArchiveFormat::Rar));
        let result = RuleBasedClassifier.classify(&candidate);
        assert_eq!(result.verdict, Verdict::Excluded);
        let score = total_score(&result.evidence);
        assert_eq!(score, 0);
    }

    #[test]
    fn html_responses_are_not_archives() {
        let mut candidate = input();
        candidate.content_type = Some("text/html; charset=utf-8".to_string());
        let result = RuleBasedClassifier.classify(&candidate);
        assert_eq!(result.verdict, Verdict::NotArchive);
        assert!(
            result
                .evidence
                .iter()
                .any(|item| item.weight == WEIGHT_HTML_RESPONSE)
        );
    }

    #[test]
    fn html_named_rar_stays_low() {
        // An HTML response named .rar must never score high.
        let mut candidate = input();
        candidate.filename = "trap.rar".to_string();
        candidate.extension = Some("rar".to_string());
        candidate.filename_format = Some(ArchiveFormat::Rar);
        candidate.content_type = Some("text/html".to_string());
        let result = RuleBasedClassifier.classify(&candidate);
        let score = total_score(&result.evidence);
        assert!(score < 25, "score was {score}");
        assert_eq!(result.verdict, Verdict::NotArchive);
    }

    #[test]
    fn verified_signature_plus_extension_scores_high() {
        let mut candidate = input();
        candidate.signature = Some(SignatureOutcome::Verified(ArchiveFormat::Zip));
        let result = RuleBasedClassifier.classify(&candidate);
        assert_eq!(result.verdict, Verdict::ArchiveCandidate);
        let score = total_score(&result.evidence);
        assert!(score >= 70, "score was {score}");
        assert!(
            result
                .evidence
                .iter()
                .any(|item| item.weight == WEIGHT_SIGNATURE_VERIFIED)
        );
    }

    #[test]
    fn extension_without_signature_is_a_candidate_with_unverified_evidence() {
        let result = RuleBasedClassifier.classify(&input());
        assert_eq!(result.verdict, Verdict::ArchiveCandidate);
        let score = total_score(&result.evidence);
        assert!(score < 50, "score was {score}");
    }

    #[test]
    fn contradictory_metadata_still_reports_both_sides() {
        // Filename says ZIP, server says RAR: both facts stay in the trail.
        let mut candidate = input();
        candidate.signature = Some(SignatureOutcome::DifferentFormat(ArchiveFormat::Rar));
        let result = RuleBasedClassifier.classify(&candidate);
        assert_eq!(result.verdict, Verdict::ArchiveCandidate);
        assert!(result.evidence.iter().any(|item| item.weight > 0));
    }

    #[test]
    fn complete_groups_boost_the_score() {
        let mut candidate = input();
        candidate.group_status = Some(GroupStatus::Complete);
        let result = RuleBasedClassifier.classify(&candidate);
        let score = total_score(&result.evidence);
        assert!(score >= 50, "score was {score}");
    }

    #[test]
    fn unknown_links_remain_reviewable() {
        let candidate = CandidateInput::without_probe(
            "landing".to_string(),
            "https://example.com/landing".to_string(),
            None,
            None,
            None,
        );
        let result = RuleBasedClassifier.classify(&candidate);
        assert_ne!(result.verdict, Verdict::ArchiveCandidate);
        assert!(!result.evidence.is_empty());
    }
}
