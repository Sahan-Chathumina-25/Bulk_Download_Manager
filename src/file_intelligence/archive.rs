//! Archive format registry: extensions, naming patterns and magic bytes.
//!
//! Pure, synchronous and deterministic: every helper here is unit-testable
//! without network access. An extension or even a signature alone never
//! *proves* a file is a valid archive; callers combine this evidence through
//! [`crate::file_intelligence::scoring`].

use serde::{Deserialize, Serialize};

/// Largest extension length treated as plausible by [`match_extension`].
const MAX_EXTENSION_LEN: usize = 10;

/// Archive families the finder understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    Zip,
    Rar,
    SevenZip,
    Tar,
    Gzip,
    Bzip2,
    Xz,
    /// Numbered split files (`.001`, `.part1`, …) whose owning family is not
    /// derivable from the name alone.
    Split,
}

impl ArchiveFormat {
    /// Human-readable name used in the UI and in persisted rows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Zip => "ZIP archive",
            Self::Rar => "RAR archive",
            Self::SevenZip => "7-Zip archive",
            Self::Tar => "TAR archive",
            Self::Gzip => "GZIP archive",
            Self::Bzip2 => "BZIP2 archive",
            Self::Xz => "XZ archive",
            Self::Split => "Split archive",
        }
    }

    /// Lowercase canonical key used in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Rar => "rar",
            Self::SevenZip => "seven_zip",
            Self::Tar => "tar",
            Self::Gzip => "gzip",
            Self::Bzip2 => "bzip2",
            Self::Xz => "xz",
            Self::Split => "split",
        }
    }

    /// Inverse of [`ArchiveFormat::as_str`]; unknown keys become `None`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "zip" => Some(Self::Zip),
            "rar" => Some(Self::Rar),
            "seven_zip" => Some(Self::SevenZip),
            "tar" => Some(Self::Tar),
            "gzip" => Some(Self::Gzip),
            "bzip2" => Some(Self::Bzip2),
            "xz" => Some(Self::Xz),
            "split" => Some(Self::Split),
            _ => None,
        }
    }
}

/// Result of matching a bare extension (lowercase, without the dot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionHit {
    pub format: ArchiveFormat,
    /// The extension itself marks a numbered volume (`.r00`, `.z01`, `.001`).
    pub implies_multipart: bool,
}

/// Match a single lowercased extension against the archive registry.
///
/// Compound names such as `.tar.gz` are resolved by [`analyse_filename`],
/// which sees the whole name; this function only sees one segment.
pub fn match_extension(extension: &str) -> Option<ExtensionHit> {
    if extension.is_empty() || extension.len() > MAX_EXTENSION_LEN {
        return None;
    }
    // RAR continuation volumes: r00 .. r99.
    if extension.len() == 3
        && extension.starts_with('r')
        && extension[1..].bytes().all(|byte| byte.is_ascii_digit())
    {
        return Some(ExtensionHit {
            format: ArchiveFormat::Rar,
            implies_multipart: true,
        });
    }
    // Split ZIP volumes: z01 .. z99.
    if extension.len() == 3
        && extension.starts_with('z')
        && extension[1..].bytes().all(|byte| byte.is_ascii_digit())
    {
        return Some(ExtensionHit {
            format: ArchiveFormat::Zip,
            implies_multipart: true,
        });
    }
    // Numbered split archives: 001 .. 999. Three digits is the dominant
    // convention; longer counters (e.g. `.1000`) are accepted up to a
    // plausible width so very long numbers stay out.
    if extension.len() >= 3
        && extension.len() <= 5
        && extension.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Some(ExtensionHit {
            format: ArchiveFormat::Split,
            implies_multipart: true,
        });
    }
    let format = match extension {
        "zip" => ArchiveFormat::Zip,
        "rar" => ArchiveFormat::Rar,
        "7z" => ArchiveFormat::SevenZip,
        "tar" => ArchiveFormat::Tar,
        "gz" => ArchiveFormat::Gzip,
        "bz2" => ArchiveFormat::Bzip2,
        "xz" => ArchiveFormat::Xz,
        _ => return None,
    };
    Some(ExtensionHit {
        format,
        implies_multipart: false,
    })
}

/// How a filename encodes its part number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartStyle {
    /// `Example.part01.rar` — first part is `part01`.
    RarPart,
    /// `Example.rar`, `Example.r00`, `Example.r01` — `.rar` is the base volume.
    RarVolume,
    /// `Example.7z.001` — numbering starts at `.001`.
    SevenZNumeric,
    /// `Example.zip`, `Example.z01`, `Example.z02` — `.zip` is the last volume.
    ZipSplit,
    /// `Example.001` / `Example.part1` — family unknown from the name.
    SplitNumeric,
    /// `Example.zip.001` — a ZIP split with a numeric counter.
    ZipNumeric,
}

impl PartStyle {
    /// The number the first part conventionally carries, when the format
    /// defines one. `None` means "no reliable convention".
    pub fn first_number(self) -> Option<u32> {
        match self {
            Self::RarPart | Self::SevenZNumeric | Self::SplitNumeric | Self::ZipNumeric => Some(1),
            // The `.rar` base volume is tracked as part 0; `.z01` volumes
            // count from 1 and the `.zip` marker closes the group.
            Self::RarVolume => Some(0),
            Self::ZipSplit => None,
        }
    }
}

/// Multipart numbering extracted from one filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartInfo {
    pub style: PartStyle,
    pub number: u32,
    /// Digits width as written (`part01` → 2) for sorting and display.
    pub digits: usize,
    /// The suffix label as written (`part01`, `r00`, `001`, `z01`, `base`).
    pub label: String,
    /// True when this name is the designated first (`Example.rar`) or last
    /// (`Example.zip`) volume of its style.
    pub marker: bool,
}

/// Full analysis of one filename (or URL path segment).
#[derive(Debug, Clone, PartialEq)]
pub struct NameAnalysis {
    /// Name as provided by the caller (already percent-decoded).
    pub filename: String,
    /// Grouping stem: the filename with volume markers removed, lowercased.
    pub base: String,
    pub format: ArchiveFormat,
    /// The name carries a recognised archive extension.
    pub is_archive: bool,
    /// The extension is a known archive extension but implies a *family* we
    /// can only guess (e.g. `.001`).
    pub family_is_guess: bool,
    pub multipart: Option<MultipartInfo>,
}

/// Split a filename into stem and lowercased extension (no dot).
fn split_extension(name: &str) -> (String, Option<String>) {
    let stem_end = name.rfind('.');
    match stem_end {
        Some(index) => {
            let extension = &name[index + 1..];
            if extension.is_empty() {
                (name.to_string(), None)
            } else {
                (
                    name[..index].to_string(),
                    Some(extension.to_ascii_lowercase()),
                )
            }
        }
        None => (name.to_string(), None),
    }
}

/// Digits width helper: anything beyond four digits is not a plausible
/// part number.
const MAX_PART_NUMBER: u32 = 9999;

/// Analyse one filename for archive family, base stem and multipart marker.
///
/// The caller must pass an already percent-decoded base name (see
/// [`crate::services::file_naming::filename_from_url`]).
pub fn analyse_filename(filename: &str) -> Option<NameAnalysis> {
    let filename = filename.trim();
    if filename.is_empty() {
        return None;
    }

    // Compound TAR+compression names first: ".tar.gz" would otherwise be
    // read as a plain GZIP file.
    let lower = filename.to_ascii_lowercase();
    for (suffix, format) in [
        (".tar.gz", ArchiveFormat::Tar),
        (".tar.bz2", ArchiveFormat::Tar),
        (".tar.xz", ArchiveFormat::Tar),
        (".tgz", ArchiveFormat::Tar),
        (".tbz2", ArchiveFormat::Tar),
        (".txz", ArchiveFormat::Tar),
    ] {
        if lower.ends_with(suffix) {
            let stem = &filename[..filename.len() - suffix.len()];
            return Some(NameAnalysis {
                filename: filename.to_string(),
                base: stem.to_ascii_lowercase(),
                format,
                is_archive: true,
                family_is_guess: false,
                multipart: None,
            });
        }
    }

    // `Example.7z.001` / `Example.zip.001` — the archive family is stated
    // before the numeric counter.
    for (family, format) in [
        (".7z.", ArchiveFormat::SevenZip),
        (".zip.", ArchiveFormat::Zip),
    ] {
        if let Some((index, counter)) = family_with_numeric_counter(&lower, family) {
            return Some(NameAnalysis {
                filename: filename.to_string(),
                base: filename[..index].to_ascii_lowercase(),
                format,
                is_archive: true,
                family_is_guess: false,
                multipart: Some(MultipartInfo {
                    style: if format == ArchiveFormat::SevenZip {
                        PartStyle::SevenZNumeric
                    } else {
                        PartStyle::ZipNumeric
                    },
                    number: counter,
                    digits: lower[index + family.len()..].len(),
                    label: lower[index + family.len()..].to_string(),
                    marker: false,
                }),
            });
        }
    }

    let (stem, extension) = split_extension(filename);

    // `.partNN.rar` — RAR's own multi-part convention.
    if lower.ends_with(".rar")
        && let Some((index, info)) = rar_part_part(&lower)
    {
        return Some(NameAnalysis {
            filename: filename.to_string(),
            base: lower[..index].to_string(),
            format: ArchiveFormat::Rar,
            is_archive: true,
            family_is_guess: false,
            multipart: Some(info),
        });
    }

    // `.partN` without an archive extension (HJSplit-style naming). The
    // last-dot split below would otherwise read "part1" as the extension.
    if let Some((index, info)) = plain_part_suffix(&lower) {
        return Some(NameAnalysis {
            filename: filename.to_string(),
            base: filename[..index].to_ascii_lowercase(),
            format: ArchiveFormat::Split,
            is_archive: false,
            family_is_guess: true,
            multipart: Some(info),
        });
    }

    let extension = extension?;

    let hit = match_extension(&extension)?;

    let multipart = if hit.implies_multipart {
        let digits = &extension[1..];
        let parsed = digits.parse::<u32>().ok()?;
        // RAR volumes follow the `.rar` base (tracked as part 0), so `r00`
        // is volume 1, `r01` is volume 2, and so on.
        let number = if extension.starts_with('r') {
            parsed + 1
        } else {
            parsed
        };
        if number > MAX_PART_NUMBER {
            return None;
        }
        Some(match extension.as_str() {
            value if value.starts_with('r') => MultipartInfo {
                style: PartStyle::RarVolume,
                number,
                digits: digits.len(),
                label: extension.clone(),
                marker: false,
            },
            value if value.starts_with('z') => MultipartInfo {
                style: PartStyle::ZipSplit,
                number,
                digits: digits.len(),
                label: extension.clone(),
                marker: false,
            },
            _ => MultipartInfo {
                style: PartStyle::SplitNumeric,
                number,
                digits: digits.len(),
                label: extension.clone(),
                marker: false,
            },
        })
    } else if hit.format == ArchiveFormat::Rar {
        // `Example.rar` doubles as the first volume of an r00-style set.
        Some(MultipartInfo {
            style: PartStyle::RarVolume,
            number: 0,
            digits: 0,
            label: "base".to_string(),
            marker: true,
        })
    } else if hit.format == ArchiveFormat::Zip {
        // `Example.zip` closes a z01-style split set.
        Some(MultipartInfo {
            style: PartStyle::ZipSplit,
            number: u32::MAX,
            digits: 0,
            label: "base".to_string(),
            marker: true,
        })
    } else {
        None
    };

    Some(NameAnalysis {
        filename: filename.to_string(),
        base: stem.to_ascii_lowercase(),
        format: hit.format,
        is_archive: true,
        family_is_guess: hit.implies_multipart && hit.format == ArchiveFormat::Split,
        multipart,
    })
}

/// Find `family` in `lower` such that everything after the *last* occurrence
/// is a plausible numeric counter (1–4 digits), returning the byte index where
/// the family starts and the parsed counter.
fn family_with_numeric_counter(lower: &str, family: &str) -> Option<(usize, u32)> {
    let index = lower.rfind(family)?;
    let counter = &lower[index + family.len()..];
    if counter.is_empty() || counter.len() > 4 {
        return None;
    }
    if !counter.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = counter.parse::<u32>().ok()?;
    if number == 0 || number > MAX_PART_NUMBER {
        return None;
    }
    Some((index, number))
}

/// Parse a trailing `.partN` marker out of a lowercased full name.
fn plain_part_suffix(lower: &str) -> Option<(usize, MultipartInfo)> {
    let marker = ".part";
    let index = lower.rfind(marker)?;
    let digits = &lower[index + marker.len()..];
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = digits.parse::<u32>().ok()?;
    if number == 0 || number > MAX_PART_NUMBER {
        return None;
    }
    Some((
        index,
        MultipartInfo {
            style: PartStyle::SplitNumeric,
            number,
            digits: digits.len(),
            label: format!("part{digits}"),
            marker: false,
        },
    ))
}

/// Parse `Example.partNN.rar` out of the full name, returning
/// `(byte_index_of_part_marker_in_lowercased_name, info)`.
fn rar_part_part(lower: &str) -> Option<(usize, MultipartInfo)> {
    if !lower.ends_with(".rar") {
        return None;
    }
    let without_rar = &lower[..lower.len() - 4];
    let index = without_rar.rfind(".part")?;
    let digits = &without_rar[index + 5..];
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = digits.parse::<u32>().ok()?;
    if number == 0 || number > MAX_PART_NUMBER {
        return None;
    }
    Some((
        index,
        MultipartInfo {
            style: PartStyle::RarPart,
            number,
            digits: digits.len(),
            label: format!("part{digits}"),
            marker: false,
        },
    ))
}

/// Signature check results for a bounded byte read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureOutcome {
    /// The magic bytes match the expected archive format.
    Verified(ArchiveFormat),
    /// The magic bytes identify an archive, but a different family than the
    /// filename suggested.
    DifferentFormat(ArchiveFormat),
    /// The response bytes do not look like any known archive signature.
    Mismatch,
    /// No signature data was available (no probe, or unsupported range).
    Unavailable,
}

impl SignatureOutcome {
    /// The format the signature identified, if any.
    pub fn format(self) -> Option<ArchiveFormat> {
        match self {
            Self::Verified(format) | Self::DifferentFormat(format) => Some(format),
            Self::Mismatch | Self::Unavailable => None,
        }
    }
}

/// Detect an archive format from the first bytes of a file.
///
/// TAR needs byte 257 onward, so the caller should provide at least 512 bytes
/// for a meaningful TAR check; shorter buffers simply return `None` for TAR.
pub fn signature_format(bytes: &[u8]) -> Option<ArchiveFormat> {
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        return Some(ArchiveFormat::Zip);
    }
    if bytes.starts_with(b"Rar!\x1a\x07\x00") {
        return Some(ArchiveFormat::Rar);
    }
    if bytes.starts_with(b"Rar!\x1a\x07\x01\x00") {
        return Some(ArchiveFormat::Rar);
    }
    if bytes.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return Some(ArchiveFormat::SevenZip);
    }
    if bytes.starts_with(b"\xfd7zXZ\x00") {
        return Some(ArchiveFormat::Xz);
    }
    if bytes.starts_with(b"\x1f\x8b") {
        return Some(ArchiveFormat::Gzip);
    }
    if bytes.len() >= 10
        && bytes.starts_with(b"BZh")
        && bytes[3].is_ascii_digit()
        && bytes[3] != b'0'
        && &bytes[4..10] == b"\x31\x41\x59\x26\x53\x59"
    {
        return Some(ArchiveFormat::Bzip2);
    }
    if bytes.len() >= 262 && &bytes[257..262] == b"ustar" {
        return Some(ArchiveFormat::Tar);
    }
    None
}

/// Compare a signature result against the filename's expected format.
pub fn compare_signature(expected: ArchiveFormat, bytes: &[u8]) -> SignatureOutcome {
    match signature_format(bytes) {
        Some(found) if found == expected => SignatureOutcome::Verified(expected),
        Some(found) => SignatureOutcome::DifferentFormat(found),
        None => SignatureOutcome::Mismatch,
    }
}

/// Extract a filename from a `Content-Disposition` header value.
///
/// Handles the RFC 5987 `filename*=UTF-8''...` form first and falls back to
/// the plain quoted or bare `filename=` token. Returns `None` when the header
/// carries no filename at all.
pub fn filename_from_content_disposition(value: &str) -> Option<String> {
    for parameter in value.split(';').skip(1) {
        let parameter = parameter.trim();
        let Some((key, raw)) = parameter.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let raw = raw.trim();
        if key == "filename*"
            && !raw.is_empty()
            && let Some(decoded) = decode_ext_value(raw)
            && !decoded.is_empty()
        {
            return Some(decoded);
        }
    }
    for parameter in value.split(';').skip(1) {
        let parameter = parameter.trim();
        let Some((key, raw)) = parameter.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("filename") {
            let raw = raw.trim();
            let unquoted = raw
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(raw);
            if !unquoted.is_empty() {
                return Some(unquoted.to_string());
            }
        }
    }
    None
}

/// Decode an RFC 5987 extended value: `charset'language'percent-encoded`.
fn decode_ext_value(raw: &str) -> Option<String> {
    let (charset, rest) = raw.split_once('\'')?;
    let (_language, encoded) = rest.split_once('\'')?;
    if !charset.eq_ignore_ascii_case("utf-8") && !charset.eq_ignore_ascii_case("iso-8859-1") {
        return None;
    }
    percent_encoding_ok(encoded)
}

/// A tiny percent-decoder for the characters this feature needs.
fn percent_encoding_ok(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
                let value = u8::from_str_radix(hex, 16).ok()?;
                decoded.push(value);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_extensions_are_recognised() {
        assert_eq!(
            match_extension("zip").map(|hit| hit.format),
            Some(ArchiveFormat::Zip)
        );
        assert_eq!(
            match_extension("rar").map(|hit| hit.format),
            Some(ArchiveFormat::Rar)
        );
        assert_eq!(
            match_extension("7z").map(|hit| hit.format),
            Some(ArchiveFormat::SevenZip)
        );
        assert_eq!(
            match_extension("tar").map(|hit| hit.format),
            Some(ArchiveFormat::Tar)
        );
        assert_eq!(
            match_extension("gz").map(|hit| hit.format),
            Some(ArchiveFormat::Gzip)
        );
        assert_eq!(
            match_extension("bz2").map(|hit| hit.format),
            Some(ArchiveFormat::Bzip2)
        );
        assert_eq!(
            match_extension("xz").map(|hit| hit.format),
            Some(ArchiveFormat::Xz)
        );
    }

    #[test]
    fn volume_extensions_imply_multipart() {
        let hit = match_extension("r00").expect("r00 is a rar volume");
        assert!(hit.implies_multipart);
        assert_eq!(hit.format, ArchiveFormat::Rar);
        let hit = match_extension("z01").expect("z01 is a zip split volume");
        assert!(hit.implies_multipart);
        assert_eq!(hit.format, ArchiveFormat::Zip);
        let hit = match_extension("001").expect("001 is a split marker");
        assert!(hit.implies_multipart);
        assert_eq!(hit.format, ArchiveFormat::Split);
    }

    #[test]
    fn unrelated_extensions_are_rejected() {
        assert!(match_extension("pdf").is_none());
        assert!(match_extension("html").is_none());
        assert!(match_extension("").is_none());
        assert!(match_extension("verylongextension").is_none());
    }

    #[test]
    fn rar_part_names_are_parsed() {
        let analysis = analyse_filename("Example.part01.rar").expect("archive name");
        assert_eq!(analysis.format, ArchiveFormat::Rar);
        assert_eq!(analysis.base, "example");
        let info = analysis.multipart.expect("multipart");
        assert_eq!(info.style, PartStyle::RarPart);
        assert_eq!(info.number, 1);
        assert_eq!(info.label, "part01");
    }

    #[test]
    fn short_part_numbers_are_parsed() {
        let analysis = analyse_filename("Backup.part2.rar").expect("archive name");
        let info = analysis.multipart.expect("multipart");
        assert_eq!(info.number, 2);
        assert_eq!(info.digits, 1);
    }

    #[test]
    fn rar_base_and_volumes_are_numbered() {
        let base = analyse_filename("Example.rar").expect("archive name");
        let info = base.multipart.expect("multipart");
        assert_eq!(info.style, PartStyle::RarVolume);
        assert_eq!(info.number, 0);
        assert!(info.marker);

        let volume = analyse_filename("Example.r00").expect("archive name");
        let info = volume.multipart.expect("multipart");
        assert_eq!(info.style, PartStyle::RarVolume);
        assert_eq!(info.number, 1);
        assert!(!info.marker);
    }

    #[test]
    fn seven_zip_split_names_are_parsed() {
        let analysis = analyse_filename("Example.7z.002").expect("archive name");
        assert_eq!(analysis.format, ArchiveFormat::SevenZip);
        assert_eq!(analysis.base, "example");
        let info = analysis.multipart.expect("multipart");
        assert_eq!(info.style, PartStyle::SevenZNumeric);
        assert_eq!(info.number, 2);
    }

    #[test]
    fn zip_split_names_are_parsed() {
        let base = analyse_filename("Example.zip").expect("archive name");
        let info = base.multipart.expect("multipart");
        assert_eq!(info.style, PartStyle::ZipSplit);
        assert!(info.marker);

        let volume = analyse_filename("Example.z01").expect("archive name");
        let info = volume.multipart.expect("multipart");
        assert_eq!(info.style, PartStyle::ZipSplit);
        assert_eq!(info.number, 1);
    }

    #[test]
    fn numeric_split_names_are_parsed() {
        let analysis = analyse_filename("Example.003").expect("split name");
        assert_eq!(analysis.format, ArchiveFormat::Split);
        assert!(analysis.family_is_guess);
        let info = analysis.multipart.expect("multipart");
        assert_eq!(info.number, 3);
    }

    #[test]
    fn plain_part_names_are_split_markers() {
        let analysis = analyse_filename("Example.part4").expect("split name");
        assert_eq!(analysis.format, ArchiveFormat::Split);
        assert!(!analysis.is_archive);
        let info = analysis.multipart.expect("multipart");
        assert_eq!(info.number, 4);
    }

    #[test]
    fn zip_numeric_split_names_are_parsed() {
        let analysis = analyse_filename("Backup.zip.001").expect("split name");
        assert_eq!(analysis.format, ArchiveFormat::Zip);
        let info = analysis.multipart.expect("multipart");
        assert_eq!(info.number, 1);
    }

    #[test]
    fn compound_tar_names_take_priority() {
        let analysis = analyse_filename("site-backup.tar.gz").expect("archive name");
        assert_eq!(analysis.format, ArchiveFormat::Tar);
        assert!(analysis.multipart.is_none());
        assert_eq!(analysis.base, "site-backup");

        let analysis = analyse_filename("bundle.tar.bz2").expect("archive name");
        assert_eq!(analysis.format, ArchiveFormat::Tar);
    }

    #[test]
    fn non_archive_names_are_rejected() {
        assert!(analyse_filename("document.pdf").is_none());
        assert!(analyse_filename("index.html").is_none());
        assert!(analyse_filename("").is_none());
        assert!(analyse_filename("noextension").is_none());
    }

    #[test]
    fn extensions_are_case_insensitive() {
        let analysis = analyse_filename("PHOTO.PART01.RAR").expect("archive name");
        assert_eq!(analysis.multipart.expect("multipart").number, 1);
        let analysis = analyse_filename("Movie.7Z.003").expect("archive name");
        assert_eq!(analysis.format, ArchiveFormat::SevenZip);
    }

    #[test]
    fn query_strings_never_become_extensions() {
        // The caller strips the query before naming analysis; a raw name with
        // one would produce no archive evidence.
        assert!(analyse_filename("archive.zip?token=secret").is_none_or(|a| a.multipart.is_none()));
    }

    #[test]
    fn zip_signatures_are_detected() {
        assert_eq!(
            signature_format(b"PK\x03\x04rest"),
            Some(ArchiveFormat::Zip)
        );
        assert_eq!(signature_format(b"PK\x05\x06"), Some(ArchiveFormat::Zip));
        assert_eq!(signature_format(b"PK\x07\x08"), Some(ArchiveFormat::Zip));
    }

    #[test]
    fn rar_signatures_are_detected() {
        let mut rar4 = b"Rar!\x1a\x07\x00".to_vec();
        assert_eq!(signature_format(&rar4), Some(ArchiveFormat::Rar));
        let mut rar5 = b"Rar!\x1a\x07\x01\x00".to_vec();
        assert_eq!(signature_format(&rar5), Some(ArchiveFormat::Rar));
        let _ = &mut rar4;
        let _ = &mut rar5;
    }

    #[test]
    fn seven_zip_gzip_and_other_signatures_are_detected() {
        assert_eq!(
            signature_format(b"7z\xbc\xaf\x27\x1c"),
            Some(ArchiveFormat::SevenZip)
        );
        assert_eq!(signature_format(b"\x1f\x8brest"), Some(ArchiveFormat::Gzip));
        assert_eq!(
            signature_format(b"\xfd7zXZ\x00rest"),
            Some(ArchiveFormat::Xz)
        );
        let mut bz2 = b"BZh9\x31\x41\x59\x26\x53\x59tail".to_vec();
        assert_eq!(signature_format(&bz2), Some(ArchiveFormat::Bzip2));
        let _ = &mut bz2;
    }

    #[test]
    fn tar_signature_needs_offset_257() {
        let mut bytes = vec![0u8; 512];
        bytes[257..262].copy_from_slice(b"ustar");
        assert_eq!(signature_format(&bytes), Some(ArchiveFormat::Tar));
        // Too short to read the offset.
        assert_eq!(signature_format(b"ustar"), None);
    }

    #[test]
    fn unknown_bytes_match_no_signature() {
        assert_eq!(signature_format(b"<html>hello</html>"), None);
        assert_eq!(signature_format(b"plain text"), None);
    }

    #[test]
    fn signature_comparison_reports_family_mismatches() {
        assert_eq!(
            compare_signature(ArchiveFormat::Zip, b"PK\x03\x04"),
            SignatureOutcome::Verified(ArchiveFormat::Zip)
        );
        assert_eq!(
            compare_signature(ArchiveFormat::Rar, b"PK\x03\x04"),
            SignatureOutcome::DifferentFormat(ArchiveFormat::Zip)
        );
        assert_eq!(
            compare_signature(ArchiveFormat::Rar, b"not an archive"),
            SignatureOutcome::Mismatch
        );
    }

    #[test]
    fn content_disposition_filenames_are_extracted() {
        assert_eq!(
            filename_from_content_disposition("attachment; filename=\"backup.zip\""),
            Some("backup.zip".to_string())
        );
        assert_eq!(
            filename_from_content_disposition("attachment; filename=backup.tar.gz"),
            Some("backup.tar.gz".to_string())
        );
        assert_eq!(
            filename_from_content_disposition("attachment; filename*=UTF-8''big%20archive.7z"),
            Some("big archive.7z".to_string())
        );
        assert_eq!(filename_from_content_disposition("inline"), None);
    }
}
