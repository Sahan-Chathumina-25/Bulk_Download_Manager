//! Filename sanitisation, derivation from URLs and collision-policy paths.
//!
//! Port of the Python filename_utils module. Windows-hostile names are
//! tamed here; path-traversal defence happens in resolve_destination,
//! which refuses any destination that escapes the requested directory.

use std::path::{Component, Path, PathBuf};

use crate::config::CollisionPolicy;

/// Maximum length of a single filename component.
pub const MAX_FILENAME_LEN: usize = 200;
/// Name used when nothing usable can be derived.
pub const FALLBACK_NAME: &str = "download";

/// Characters Windows forbids in filenames.
const WINDOWS_INVALID_CHARS: &str = "<>:\"/\\|?*";

/// Sanitise name into a safe single Windows pathname component.
///
/// Replaces invalid Windows characters and control characters with _ ,
/// strips trailing dots/spaces (which Windows ignores anyway), neutralises
/// reserved device names (CON, NUL, LPT1...) by prefixing _, and clamps to
/// 200 characters while preserving the extension. Empty input yields
/// download. (Divergence note: Rust is_control() also replaces C1 control
/// characters U+0080..U+009F, which the Python regex kept; the Rust
/// behaviour is the safer one.)
pub fn sanitize_filename(name: &str) -> String {
    let mut cleaned = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_control() || WINDOWS_INVALID_CHARS.contains(ch) {
            cleaned.push('_');
        } else {
            cleaned.push(ch);
        }
    }
    let cleaned = cleaned.trim_end_matches([' ', '.']).trim();
    if cleaned.is_empty() {
        return FALLBACK_NAME.to_string();
    }
    let cleaned = truncate_preserving_extension(cleaned);
    if is_reserved_windows_name(&cleaned) {
        return format!("_{cleaned}");
    }
    if cleaned.is_empty() {
        FALLBACK_NAME.to_string()
    } else {
        cleaned
    }
}

/// Clamp name to MAX_FILENAME_LEN keeping a plausible extension.
fn truncate_preserving_extension(name: &str) -> String {
    if name.chars().count() <= MAX_FILENAME_LEN {
        return name.to_string();
    }
    if let Some(dot) = name.rfind('.') {
        // dot is a byte index; . is ASCII so dot + 1 is a char boundary.
        let ext = &name[dot + 1..];
        let ext_len = ext.chars().count();
        if dot > 0 && ext_len > 0 && ext_len <= 10 && ext.chars().all(|c| c.is_alphanumeric()) {
            let keep = MAX_FILENAME_LEN - ext_len - 1;
            let head: String = name.chars().take(keep).collect();
            return format!("{head}.{ext}");
        }
    }
    name.chars().take(MAX_FILENAME_LEN).collect()
}

/// Derive a filename from a URL path (fragment+query stripped, decoded).
pub fn filename_from_url(url: &str) -> String {
    let basename = url_path_segment(url);
    let decoded = percent_encoding::percent_decode_str(&basename).decode_utf8_lossy();
    sanitize_filename(&decoded)
}

/// Mirror urllib.parse.urlsplit(url).path for the inputs this app accepts:
/// everything after the scheme+authority up to ? or #; with no scheme://
/// separator the whole (fragment/query-stripped) text is the path, like
/// urlsplit treats scheme-less input. Truly unparseable input (where
/// urlsplit raises ValueError) yields an empty path.
fn url_path_segment(url: &str) -> String {
    let no_fragment = url.split('#').next().unwrap_or("");
    let after_scheme = match no_fragment.find("://") {
        Some(i) => match no_fragment[i + 3..].find('/') {
            Some(j) => &no_fragment[i + 3 + j..],
            None => "",
        },
        None => no_fragment,
    };
    after_scheme.split('?').next().unwrap_or("").to_string()
}

/// Join filename into destination_dir and refuse path traversal.
///
/// The result is guaranteed (after resolution) to live inside
/// destination_dir; hostile names are re-anchored to the base directory
/// rather than escaped.
pub fn resolve_destination(destination_dir: &Path, filename: &str) -> PathBuf {
    let base = expand_and_normalise(destination_dir);
    let safe_name = sanitize_filename(filename);
    let candidate = base.join(&safe_name);
    // sanitize_filename already yields a single safe component; the parent
    // check below is defence in depth mirroring the Python resolve()
    // comparison.
    let inside = candidate.parent().map(|parent| parent == base).unwrap_or(false);
    if inside {
        candidate
    } else {
        base.join(FALLBACK_NAME)
    }
}

/// Expand a leading ~ and lexically normalise . and .. components
/// (Path.resolve in Python also resolves symlinks; lexical normalisation
/// is enough for traversal defence here).
fn expand_and_normalise(dir: &Path) -> PathBuf {
    let text = dir.to_string_lossy();
    let expanded = if text == "~" {
        home_dir()
    } else if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        home_dir().join(rest)
    } else {
        dir.to_path_buf()
    };
    let expanded = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from('.')).join(expanded)
    };
    let mut out = PathBuf::new();
    for component in expanded.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Best-effort home directory (USERPROFILE on Windows, HOME elsewhere).
fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from('.'))
}

/// Apply a filename-collision policy and return the path to write to.
///
/// keep_existing: existing file is authoritative; the same path is
/// returned and the caller must not overwrite it. rename: find a free
/// stem (n).ext variant. overwrite: return the path; the caller
/// replaces the file. skip: None; the caller skips without touching disk.
/// (The Python version raised ValueError on an unknown policy; the enum
/// makes that state impossible here.)
pub fn unique_path(path: &Path, policy: CollisionPolicy) -> Option<PathBuf> {
    if !path.exists() {
        return Some(path.to_path_buf());
    }
    match policy {
        CollisionPolicy::Rename => Some(next_free_variant(path)),
        CollisionPolicy::Skip => None,
        CollisionPolicy::KeepExisting | CollisionPolicy::Overwrite => Some(path.to_path_buf()),
    }
}

/// Return the first free stem (n).ext variant of an existing path.
fn next_free_variant(path: &Path) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let suffix = path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    for n in 1..1000 {
        let candidate = path.with_name(format!("{stem} ({n}){suffix}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    path.with_name(format!("{stem} (too-many){suffix}"))
}

/// True if name is a reserved Windows device name (CON, LPT2, ...).
pub fn is_reserved_windows_name(name: &str) -> bool {
    // Mirrors the Python ^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(\..*)?$
    // regular expression, case-insensitively.
    let base = name.split('.').next().unwrap_or("");
    let upper = base.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    // COM1..COM9 / LPT1..LPT9: exactly one digit 1-9 after the prefix.
    upper.len() == 4
        && (upper.starts_with("COM") || upper.starts_with("LPT"))
        && (49..=57).contains(&upper.as_bytes()[3]) // ASCII 1..9
}

/// Extract a lowercased file extension (without the dot) or None.
///
/// Accepts a URL (fragments/query are stripped first) or a bare filename.
/// Returns the extension of the last path segment when it looks like a
/// plausible extension (alphanumeric, at most 10 chars); None if the name
/// has no extension or it is not plausible. Input is deliberately NOT
/// trimmed before classification, matching the original.
pub fn classify_extension(url_or_name: &str) -> Option<String> {
    let path = url_path_segment(url_or_name);
    let source: &str = if path.is_empty() { url_or_name } else { &path };
    let basename = source
        .rsplit('/')
        .next()
        .unwrap_or("")
        .rsplit('\\')
        .next()
        .unwrap_or("");
    if !basename.contains('.') {
        return None;
    }
    let ext = basename.rsplit('.').next().unwrap_or("").trim().to_lowercase();
    let plausible = !ext.is_empty()
        && ext.chars().count() <= 10
        && ext.chars().all(|c| c.is_alphanumeric());
    if plausible {
        Some(ext)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("bdm-filename-tests-{name}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn invalid_windows_chars_replaced() {
        assert_eq!(sanitize_filename("a<b>c:d\"e|f?g*h"), "a_b_c_d_e_f_g_h");
        assert_eq!(sanitize_filename("a/b"), "a_b");
        assert_eq!(sanitize_filename("a\\b"), "a_b");
        assert_eq!(sanitize_filename("C:report.zip"), "C_report.zip");
    }

    #[test]
    fn control_characters_replaced() {
        assert_eq!(sanitize_filename("bad\u{0}\u{1}\u{1f}\u{7f}name"), "bad____name");
    }

    #[test]
    fn trailing_dots_and_spaces_stripped() {
        assert_eq!(sanitize_filename("name..."), "name");
        assert_eq!(sanitize_filename("name . . "), "name");
        assert_eq!(sanitize_filename("name  "), "name");
        assert_eq!(sanitize_filename("  spaced.txt "), "spaced.txt");
        assert_eq!(sanitize_filename(" .hidden"), ".hidden");
    }

    #[test]
    fn reserved_device_names_neutralised() {
        for (reserved, expected) in [
            ("CON", "_CON"),
            ("con.txt", "_con.txt"),
            ("PRN", "_PRN"),
            ("AUX", "_AUX"),
            ("NUL", "_NUL"),
            ("COM1", "_COM1"),
            ("com5", "_com5"),
            ("LPT3", "_LPT3"),
            ("CON.", "_CON"),
        ] {
            assert_eq!(sanitize_filename(reserved), expected);
            assert!(is_reserved_windows_name(expected.trim_start_matches('_')));
        }
    }

    #[test]
    fn com10_is_not_reserved() {
        assert_eq!(sanitize_filename("COM10"), "COM10");
        assert!(!is_reserved_windows_name("COM10"));
    }

    #[test]
    fn empty_input_falls_back() {
        for empty in ["", "   ", "...", ". . ..."] {
            assert_eq!(sanitize_filename(empty), FALLBACK_NAME);
        }
    }
