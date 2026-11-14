//! Analysis report models shared by the engine, the UI and persistence.
//!
//! The report never carries credentials or raw query strings: the UI shows
//! the redacted form (see [`redacted_url`]) and stored rows only keep the
//! safe display fields.

use serde::{Deserialize, Serialize};

use super::archive::{ArchiveFormat, SignatureOutcome};
use super::multipart::{ArchiveGroup, GroupMember, GroupStatus};
use super::scoring::Confidence;

/// A classified candidate with the full evidence trail.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CandidateReport {
    /// Original URL; kept for queueing, redacted whenever displayed.
    pub url: String,
    /// Percent-decoded display filename.
    pub filename: String,
    /// The page the link was discovered on, when known.
    pub source_page_url: String,
    /// Lowercased extension, when present.
    pub extension: Option<String>,
    /// Human-readable archive family label.
    pub archive_type: String,
    /// Structured family for persistence and grouping.
    pub format: Option<ArchiveFormat>,
    /// Normalised score in `0..=100`.
    pub score: u8,
    pub confidence: Confidence,
    /// Short status string (`Candidate`, `Unverified archive`, `Webpage`, …).
    pub status: String,
    /// Advertised size, when available.
    pub size: Option<u64>,
    /// Media type reported by the server, when probed.
    pub content_type: Option<String>,
    pub signature: Option<SignatureOutcome>,
    /// Multipart group key, when the candidate belongs to a group.
    pub group_key: Option<String>,
    /// Part number within the group, when known.
    pub part_number: Option<u32>,
    /// The evidence trail behind the score, ordered by weight.
    pub evidence: Vec<super::scoring::Evidence>,
    /// Evidence the analysis could not obtain (e.g. "signature not probed").
    pub missing_evidence: Vec<String>,
    /// Ad/tracker filter reason, when excluded.
    pub filtered_reason: Option<String>,
    /// Anchor text from the source page, when known.
    pub anchor_text: Option<String>,
}

impl CandidateReport {
    /// True when the user may queue this candidate (not ad/tracker-filtered
    /// and not an outright webpage).
    pub fn is_selectable(&self) -> bool {
        self.filtered_reason.is_none() && self.score >= 25
    }
}

/// A multipart group as presented in the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupReport {
    pub key: String,
    pub display_name: String,
    pub archive_type: String,
    pub format: Option<ArchiveFormat>,
    /// Part labels in detected order (`part01`, `part02`, …).
    pub parts: Vec<PartReport>,
    pub status: GroupStatus,
    pub missing: Vec<String>,
    pub duplicates: Vec<String>,
    /// Highest score among the members; advisory only.
    pub score: u8,
    /// Index into `AnalysisReport::candidates` for each part, in order.
    pub candidate_indexes: Vec<usize>,
}

/// One part inside a group card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartReport {
    pub filename: String,
    pub label: String,
    pub number: u32,
    /// Index into `AnalysisReport::candidates`.
    pub candidate_index: usize,
}

/// Where the analysis input came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisSource {
    /// Pasted URL lines.
    #[default]
    Pasted,
    /// Links already discovered by a page scan.
    Discovered,
}

/// The complete result of one analysis run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AnalysisReport {
    /// ISO-8601 UTC timestamp; doubles as the run identifier.
    pub created_at: String,
    pub source: AnalysisSource,
    pub candidates: Vec<CandidateReport>,
    pub groups: Vec<GroupReport>,
    /// Candidates analysed in total (including filtered and webpages).
    pub analyzed_count: usize,
    /// Candidates scored at or above the archive-candidate threshold.
    pub candidate_count: usize,
    /// Links excluded by the ad/tracker filter.
    pub filtered_count: usize,
    /// Candidates with no signature data (unverified).
    pub unverified_count: usize,
    /// Groups whose naming looks complete.
    pub complete_groups: usize,
    /// Groups whose naming is incomplete.
    pub incomplete_groups: usize,
    /// Problems that affected the run itself.
    pub errors: Vec<String>,
    /// Seconds the run took.
    pub elapsed_seconds: f64,
    /// Whether bounded HTTP probing was enabled for this run.
    pub probed: bool,
}

impl AnalysisReport {
    /// Candidates in score order (descending), ties by filename.
    pub fn ranked_candidates(&self) -> impl Iterator<Item = &CandidateReport> {
        self.candidates.iter()
    }
}

/// Redacted display form of a URL: strips the query string and fragment so
/// tokens or credentials in links never reach the screen or the logs.
pub fn redacted_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut parsed) => {
            parsed.set_query(None);
            parsed.set_fragment(None);
            parsed.to_string()
        }
        Err(_) => url.to_string(),
    }
}

/// Build the UI-facing `GroupReport` values from detected groups plus the
/// candidate index each member resolved to.
pub fn build_group_reports(
    groups: &[ArchiveGroup],
    format_label: impl Fn(Option<ArchiveFormat>) -> String,
    score_of: impl Fn(&GroupMember) -> u8,
    index_of: impl Fn(&GroupMember) -> Option<usize>,
) -> Vec<GroupReport> {
    let mut reports = Vec::with_capacity(groups.len());
    for group in groups {
        let mut parts = Vec::with_capacity(group.members.len());
        let mut candidate_indexes = Vec::with_capacity(group.members.len());
        let mut best_score = 0u8;
        for member in &group.members {
            let index = index_of(member);
            let score = score_of(member);
            best_score = best_score.max(score);
            candidate_indexes.push(index.unwrap_or(usize::MAX));
            parts.push(PartReport {
                filename: member.filename.clone(),
                label: member.label.clone(),
                number: member.number,
                candidate_index: index.unwrap_or(usize::MAX),
            });
        }
        reports.push(GroupReport {
            key: group.key.clone(),
            display_name: group.display_name.clone(),
            archive_type: format_label(Some(group.format)),
            format: Some(group.format),
            parts,
            status: group.status,
            missing: group.missing.clone(),
            duplicates: group.duplicates.clone(),
            score: best_score,
            candidate_indexes,
        });
    }
    reports
}
