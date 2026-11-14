//! Transparent, weighted scoring for archive candidates.
//!
//! Every piece of evidence carries an explicit weight and a plain-English
//! reason. The total is clamped to 0..100 and mapped to a confidence band.
//! Scores are advisory: nothing is ever downloaded or auto-selected because
//! of a score, and a single weak signal can never override strong contrary
//! evidence (for example, an HTML response caps any archive score).

use serde::{Deserialize, Serialize};

use super::archive::{ArchiveFormat, SignatureOutcome};

/// One scored observation about a candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Signed contribution to the score.
    pub weight: i16,
    /// Plain-English explanation shown in the UI.
    pub reason: String,
}

impl Evidence {
    pub fn new(weight: i16, reason: impl Into<String>) -> Self {
        Self {
            weight,
            reason: reason.into(),
        }
    }
}

/// Confidence bands for a normalised score. Deliberately *not* probabilities:
/// the labels describe the weight of the evidence, not a chance of success.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    VeryHigh,
    High,
    Possible,
    Low,
    #[default]
    Unlikely,
}

impl Confidence {
    pub fn label(self) -> &'static str {
        match self {
            Self::VeryHigh => "Very high confidence",
            Self::High => "High confidence",
            Self::Possible => "Possible candidate",
            Self::Low => "Low confidence",
            Self::Unlikely => "Unlikely archive",
        }
    }

    /// Band for a score in `0..=100`.
    pub fn from_score(score: u8) -> Self {
        match score {
            90..=100 => Self::VeryHigh,
            75..=89 => Self::High,
            50..=74 => Self::Possible,
            25..=49 => Self::Low,
            _ => Self::Unlikely,
        }
    }
}

// Evidence weights. Kept as named constants so tests and the explanation
// panel always agree on the numbers.
/// A recognised archive extension on the URL path or filename.
pub const WEIGHT_ARCHIVE_EXTENSION: i16 = 30;
/// A `Content-Disposition` filename with an archive extension.
pub const WEIGHT_DISPOSITION_ARCHIVE: i16 = 12;
/// An HTTP `Content-Type` that names an archive family.
pub const WEIGHT_CONTENT_TYPE_ARCHIVE: i16 = 15;
/// The generic `application/octet-stream` — weak but not nothing.
pub const WEIGHT_CONTENT_TYPE_OCTET_STREAM: i16 = 6;
/// A bounded magic-byte read matched the filename's archive family.
pub const WEIGHT_SIGNATURE_VERIFIED: i16 = 40;
/// Magic bytes matched an archive family other than the filename's.
pub const WEIGHT_SIGNATURE_DIFFERENT: i16 = 25;
/// The multipart naming sequence is consistent for this candidate's group.
pub const WEIGHT_MULTIPART_CONSISTENT: i16 = 20;
/// The candidate belongs to a group whose naming is incomplete.
pub const WEIGHT_MULTIPART_INCOMPLETE: i16 = 6;
/// The candidate's group has duplicate or conflicting naming.
pub const WEIGHT_MULTIPART_CONFLICTED: i16 = -8;
/// The response bytes matched no known archive signature.
pub const WEIGHT_SIGNATURE_MISMATCH: i16 = -20;
/// The response is an HTML page, not a file.
pub const WEIGHT_HTML_RESPONSE: i16 = -45;
/// A plain webpage link without any file signal.
pub const WEIGHT_PLAIN_WEBPAGE: i16 = -20;
/// The link was matched by the ad/tracker filter.
pub const WEIGHT_AD_FILTERED: i16 = -100;

/// Known archive-ish `Content-Type` media types.
const ARCHIVE_CONTENT_TYPES: &[(&str, ArchiveFormat)] = &[
    ("application/zip", ArchiveFormat::Zip),
    ("application/x-zip-compressed", ArchiveFormat::Zip),
    ("application/x-rar-compressed", ArchiveFormat::Rar),
    ("application/vnd.rar", ArchiveFormat::Rar),
    ("application/x-7z-compressed", ArchiveFormat::SevenZip),
    ("application/x-tar", ArchiveFormat::Tar),
    ("application/gzip", ArchiveFormat::Gzip),
    ("application/x-gzip", ArchiveFormat::Gzip),
    ("application/x-bzip2", ArchiveFormat::Bzip2),
    ("application/x-xz", ArchiveFormat::Xz),
];

/// The media type names an archive family; returns the family when known.
pub fn content_type_format(content_type: &str) -> Option<ArchiveFormat> {
    let media = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if media == "application/octet-stream" {
        return None;
    }
    ARCHIVE_CONTENT_TYPES
        .iter()
        .find(|(known, _)| media == *known)
        .map(|(_, format)| *format)
}

/// True for the generic binary media type, a weak archive hint only.
pub fn is_octet_stream(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/octet-stream")
}

/// True when the response is (probably) an HTML page.
pub fn is_html_content_type(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("text/html")
}

/// Accumulate evidence into a normalised score.
///
/// The sum is clamped to `0..=100`; the ad-filter weight alone drives any
/// candidate to zero.
pub fn total_score(evidence: &[Evidence]) -> u8 {
    let sum: i32 = evidence.iter().map(|item| i32::from(item.weight)).sum();
    sum.clamp(0, 100) as u8
}

/// Map a signature outcome into evidence for the expected archive format.
pub fn signature_evidence(expected: ArchiveFormat, outcome: SignatureOutcome) -> Vec<Evidence> {
    match outcome {
        SignatureOutcome::Verified(found) => vec![Evidence::new(
            WEIGHT_SIGNATURE_VERIFIED,
            format!(
                "The first bytes of the response match the {} signature.",
                found.label()
            ),
        )],
        SignatureOutcome::DifferentFormat(found) => vec![Evidence::new(
            WEIGHT_SIGNATURE_DIFFERENT,
            format!(
                "The response looks like a {} file, but the filename suggests {}.",
                found.label(),
                expected.label()
            ),
        )],
        SignatureOutcome::Mismatch => vec![Evidence::new(
            WEIGHT_SIGNATURE_MISMATCH,
            "The response bytes do not look like any known archive signature.",
        )],
        SignatureOutcome::Unavailable => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_intelligence::archive::ArchiveFormat;

    #[test]
    fn archive_content_types_are_recognised() {
        assert_eq!(
            content_type_format("application/zip"),
            Some(ArchiveFormat::Zip)
        );
        assert_eq!(
            content_type_format("application/x-rar-compressed; charset=utf-8"),
            Some(ArchiveFormat::Rar)
        );
        assert_eq!(
            content_type_format("APPLICATION/X-7Z-COMPRESSED"),
            Some(ArchiveFormat::SevenZip)
        );
        assert_eq!(
            content_type_format("application/x-tar"),
            Some(ArchiveFormat::Tar)
        );
        assert_eq!(
            content_type_format("application/gzip"),
            Some(ArchiveFormat::Gzip)
        );
    }

    #[test]
    fn html_and_octet_stream_are_distinguished() {
        assert!(is_html_content_type("text/html; charset=utf-8"));
        assert!(!is_html_content_type("application/zip"));
        assert!(is_octet_stream("application/octet-stream"));
        assert!(content_type_format("application/octet-stream").is_none());
    }

    #[test]
    fn scores_clamp_to_the_unit_range() {
        let strong = vec![
            Evidence::new(WEIGHT_ARCHIVE_EXTENSION, "extension"),
            Evidence::new(WEIGHT_SIGNATURE_VERIFIED, "signature"),
            Evidence::new(WEIGHT_CONTENT_TYPE_ARCHIVE, "type"),
            Evidence::new(WEIGHT_MULTIPART_CONSISTENT, "group"),
        ];
        assert_eq!(total_score(&strong), 100);
        let hostile = vec![
            Evidence::new(WEIGHT_AD_FILTERED, "ad"),
            Evidence::new(90, "x"),
        ];
        assert_eq!(total_score(&hostile), 0);
    }

    #[test]
    fn confidence_bands_match_the_documented_thresholds() {
        assert_eq!(Confidence::from_score(100), Confidence::VeryHigh);
        assert_eq!(Confidence::from_score(90), Confidence::VeryHigh);
        assert_eq!(Confidence::from_score(89), Confidence::High);
        assert_eq!(Confidence::from_score(75), Confidence::High);
        assert_eq!(Confidence::from_score(74), Confidence::Possible);
        assert_eq!(Confidence::from_score(50), Confidence::Possible);
        assert_eq!(Confidence::from_score(49), Confidence::Low);
        assert_eq!(Confidence::from_score(25), Confidence::Low);
        assert_eq!(Confidence::from_score(24), Confidence::Unlikely);
        assert_eq!(Confidence::from_score(0), Confidence::Unlikely);
    }

    #[test]
    fn signature_outcomes_map_to_evidence() {
        let verified = signature_evidence(
            ArchiveFormat::Zip,
            SignatureOutcome::Verified(ArchiveFormat::Zip),
        );
        assert_eq!(verified[0].weight, WEIGHT_SIGNATURE_VERIFIED);
        let different = signature_evidence(
            ArchiveFormat::Rar,
            SignatureOutcome::DifferentFormat(ArchiveFormat::Zip),
        );
        assert_eq!(different[0].weight, WEIGHT_SIGNATURE_DIFFERENT);
        let unavailable = signature_evidence(ArchiveFormat::Zip, SignatureOutcome::Unavailable);
        assert!(unavailable.is_empty());
    }

    #[test]
    fn one_weak_signal_cannot_override_strong_contradictory_evidence() {
        // A matching extension (+30) with an HTML response (-45) nets below
        // the candidate threshold and is capped by the HTML evidence.
        let evidence = vec![
            Evidence::new(WEIGHT_ARCHIVE_EXTENSION, "extension"),
            Evidence::new(WEIGHT_HTML_RESPONSE, "html"),
        ];
        let score = total_score(&evidence);
        assert!(score < 25, "score was {score}");
    }
}
