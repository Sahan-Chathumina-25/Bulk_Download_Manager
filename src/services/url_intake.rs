//! Bulk URL-line intake (port of the legacy ``url_validator`` module).
//!
//! The Phase 5 intake surface is deliberately tiny: callers paste or import a
//! block of text, one URL per line (an optional second tab-separated column
//! carries a display name that is discarded here), and get back the accepted
//! URLs plus the rejected lines and exact duplicates.
//!
//! # Relationship to `validation::parse_url_lines`
//!
//! An earlier phase already shipped an equivalent parser inside
//! [`crate::services::validation`]; that implementation remains for
//! compatibility. This module is the canonical Phase 5 home for the intake
//! behaviour and is implemented independently so the documented rule
//! "extract the first tab-separated column from the *raw* line" holds (a
//! line consisting only of a tab followed by a name therefore reports
//! "no URL found on line" instead of treating the name as a URL). All other
//! rules match the earlier parser exactly.
//!
//! Validation order per candidate:
//! 1. the lowercased scheme must be `http` or `https`,
//! 2. [`url::Url::parse`] must succeed (`malformed URL: ...` otherwise),
//! 3. [`crate::services::validation::validate_url_for_network_opts`] runs with
//!    full DNS resolution (`unsafe URL: ...` otherwise).
//!
//! There is no comment handling: every non-blank line is data.

use std::collections::HashSet;

use url::Url;

use crate::services::validation;

/// Outcome of ingesting a block of pasted/imported URL lines.
#[derive(Debug, Default, Clone)]
pub struct ParseResult {
    /// Validated, safe URLs in order of first appearance.
    pub accepted: Vec<String>,
    /// `(stripped_line, reason)` pairs for unusable lines.
    pub rejected: Vec<(String, String)>,
    /// URL strings that exactly repeated an earlier accepted URL.
    pub duplicates: Vec<String>,
}

/// Parse lines of URLs into accepted/rejected/duplicate buckets.
///
/// Handles CRLF, LF and bare-CR endings, skips blank lines, strips
/// surrounding whitespace, ignores a tab-separated display-name column and
/// deduplicates exact repeats (first occurrence wins, input order preserved).
/// Every URL must be http/https and network-safe; private addresses are
/// rejected unless `allow_private` is set.
pub fn parse_url_lines(text: &str, allow_private: bool) -> ParseResult {
    let mut result = ParseResult::default();
    if text.is_empty() {
        return result;
    }

    let normalised = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut seen: HashSet<String> = HashSet::new();

    for raw_line in normalised.split('\n') {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        // The first tab-separated column of the *raw* line is the candidate.
        let candidate = raw_line.split('\t').next().unwrap_or("").trim().to_string();
        if candidate.is_empty() {
            result
                .rejected
                .push((line.to_string(), "no URL found on line".to_string()));
            continue;
        }

        if seen.contains(&candidate) {
            result.duplicates.push(candidate);
            continue;
        }

        match validate_candidate(&candidate, allow_private) {
            Some(reason) => result.rejected.push((line.to_string(), reason)),
            None => {
                seen.insert(candidate.clone());
                result.accepted.push(candidate);
            }
        }
    }

    result
}

/// Return a rejection reason for `candidate`, or `None` if acceptable.
fn validate_candidate(candidate: &str, allow_private: bool) -> Option<String> {
    let scheme = candidate
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Some("only http and https URLs are supported".to_string());
    }
    if let Err(err) = Url::parse(candidate) {
        return Some(format!("malformed URL: {err}"));
    }
    match validation::validate_url_for_network_opts(candidate, allow_private, true) {
        Ok(_) => None,
        Err(err) => Some(format!("unsafe URL: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOOP: &str = "http://127.0.0.1:9";

    #[test]
    fn line_endings_are_normalised() {
        let cases = [
            (
                format!("{LOOP}/a\r\n{LOOP}/b\r\n{LOOP}/c"),
                vec![
                    format!("{LOOP}/a"),
                    format!("{LOOP}/b"),
                    format!("{LOOP}/c"),
                ],
            ),
            (
                format!("{LOOP}/a\n{LOOP}/b"),
                vec![format!("{LOOP}/a"), format!("{LOOP}/b")],
            ),
            (
                format!("{LOOP}/a\r{LOOP}/b\r{LOOP}/c"),
                vec![
                    format!("{LOOP}/a"),
                    format!("{LOOP}/b"),
                    format!("{LOOP}/c"),
                ],
            ),
            (
                format!("{LOOP}/a\r\n{LOOP}/b\n{LOOP}/c\r{LOOP}/d"),
                vec![
                    format!("{LOOP}/a"),
                    format!("{LOOP}/b"),
                    format!("{LOOP}/c"),
                    format!("{LOOP}/d"),
                ],
            ),
        ];
        for (text, expected) in cases {
            let result = parse_url_lines(&text, true);
            assert_eq!(result.accepted, expected);
            assert!(result.rejected.is_empty());
            assert!(result.duplicates.is_empty());
        }
    }

    #[test]
    fn blank_and_whitespace_only_lines_are_skipped() {
        let text = format!("\n\n  \n\t\n{LOOP}/a\n   \n\t\t\n{LOOP}/b\n\n");
        let result = parse_url_lines(&text, true);
        assert_eq!(
            result.accepted,
            vec![format!("{LOOP}/a"), format!("{LOOP}/b")]
        );
        assert!(result.rejected.is_empty());
        assert!(result.duplicates.is_empty());
    }

    #[test]
    fn empty_inputs_yield_empty_results() {
        for text in ["", "   ", "\r\n\r\n"] {
            let result = parse_url_lines(text, false);
            assert!(result.accepted.is_empty());
            assert!(result.rejected.is_empty());
            assert!(result.duplicates.is_empty());
        }
    }

    #[test]
    fn tab_separated_url_and_name_keeps_only_the_url() {
        let text = format!("{LOOP}/file.zip\tinstaller package\n");
        let result = parse_url_lines(&text, true);
        assert_eq!(result.accepted, vec![format!("{LOOP}/file.zip")]);
        assert!(result.rejected.is_empty());
    }

    #[test]
    fn tab_only_line_reports_no_url() {
        let result = parse_url_lines("\tjust a name", true);
        assert!(result.accepted.is_empty());
        assert_eq!(result.rejected.len(), 1);
        assert_eq!(result.rejected[0].0, "just a name");
        assert_eq!(result.rejected[0].1, "no URL found on line");
    }

    #[test]
    fn invalid_urls_are_rejected_with_a_reason() {
        for bad in [
            "notaurl",
            "::::",
            "ftp://example.com/file.zip",
            "javascript:alert(1)",
            "file:///C:/Windows/System32/calc.exe",
            "http://",
            "https:///path/only",
        ] {
            let result = parse_url_lines(bad, false);
            assert!(result.accepted.is_empty(), "accepted: {bad:?}");
            assert_eq!(result.rejected.len(), 1, "rejected count: {bad:?}");
            let (line, reason) = &result.rejected[0];
            assert_eq!(line, bad);
            assert!(!reason.is_empty(), "every rejection must carry a reason");
        }
    }

    #[test]
    fn private_urls_rejected_by_default_then_accepted_with_flag() {
        let url = format!("{LOOP}/a.zip");
        let strict = parse_url_lines(&url, false);
        assert!(strict.accepted.is_empty());
        assert_eq!(strict.rejected[0].0, url);
        assert!(strict.rejected[0].1.contains("private"));

        let relaxed = parse_url_lines(&url, true);
        assert_eq!(relaxed.accepted, vec![url]);
        assert!(relaxed.rejected.is_empty());
    }

    #[test]
    fn ordered_dedup_first_occurrence_wins() {
        let a = format!("{LOOP}/a");
        let b = format!("{LOOP}/b");
        let text = format!("{a}\n{b}\n{a}\n{b}\n{a}\n");
        let result = parse_url_lines(&text, true);
        assert_eq!(result.accepted, vec![a.clone(), b.clone()]);
        assert_eq!(result.duplicates, vec![a.clone(), b.clone(), a]);
    }

    #[test]
    fn duplicate_needs_an_exact_match() {
        let a = format!("{LOOP}/a");
        let b = format!("{LOOP}/b");
        let result = parse_url_lines(&format!("{a}\n{b}\n"), true);
        assert_eq!(result.accepted, vec![a, b]);
        assert!(result.duplicates.is_empty());
    }

    #[test]
    fn accepted_order_preserves_input_order() {
        let urls = [
            format!("{LOOP}/z"),
            format!("{LOOP}/m"),
            format!("{LOOP}/a"),
            format!("{LOOP}/k"),
        ];
        let text = urls.iter().rev().cloned().collect::<Vec<_>>().join("\n");
        let result = parse_url_lines(&text, true);
        let expected: Vec<String> = urls.iter().rev().cloned().collect();
        assert_eq!(result.accepted, expected);
    }

    #[test]
    fn mixed_batch_keeps_both_rejection_reasons_exact() {
        let text = format!("{LOOP}/ok.zip\nnotaurl\n{LOOP}/also-ok.bin\nftp://nope/x\n");
        let result = parse_url_lines(&text, true);
        assert_eq!(
            result.accepted,
            vec![format!("{LOOP}/ok.zip"), format!("{LOOP}/also-ok.bin")]
        );
        let reasons: Vec<&str> = result
            .rejected
            .iter()
            .map(|(_, reason)| reason.as_str())
            .collect();
        assert_eq!(
            reasons,
            vec![
                "only http and https URLs are supported",
                "only http and https URLs are supported",
            ]
        );
    }
}
