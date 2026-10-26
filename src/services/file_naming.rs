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
///
/// Some file lockers put the real filename in the URL fragment, e.g.
/// `.../vcqpuy297lnc#Show_S01.part021.rar`. Fragments never reach the server,
/// so when the path's last segment has no known extension and the fragment's
/// last path-like segment does, the fragment supplies the display name.
pub fn filename_from_url(url: &str) -> String {
    let path = url_path_segment(url);
    let basename = path.rsplit('/').next().unwrap_or("");
    let decoded = percent_encoding::percent_decode_str(basename).decode_utf8_lossy();
    if classify_extension(&decoded).is_none()
        && let Some(from_fragment) = filename_from_fragment(url)
    {
        return from_fragment;
    }
    sanitize_filename(&decoded)
}

/// Extract a sanitized filename from a URL fragment when its last
/// path-like segment carries a plausible extension; otherwise None.
fn filename_from_fragment(url: &str) -> Option<String> {
    let fragment = url.split_once('#')?.1;
    let candidate = fragment.rsplit('/').next().unwrap_or("");
    let decoded = percent_encoding::percent_decode_str(candidate).decode_utf8_lossy();
    classify_extension(&decoded)?;
    Some(sanitize_filename(&decoded))
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
    let inside = candidate
        .parent()
        .map(|parent| parent == base)
        .unwrap_or(false);
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
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(expanded)
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
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The sibling `.part` path used while a download is in flight.
///
/// This appends `.part` to the full filename (`archive.zip` ->
/// `archive.zip.part`), matching the worker's publish step, rather than
/// replacing the extension.
pub fn part_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
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
    unique_path_with(path, policy, Path::exists)
}

/// Like [`unique_path`], but `is_taken` decides whether a candidate path is
/// already occupied.
///
/// Callers pass more than the plain on-disk check when an in-flight row or a
/// `.part` file can also collide: the manager treats both as taken so two
/// downloads never write the same destination.
pub fn unique_path_with(
    path: &Path,
    policy: CollisionPolicy,
    is_taken: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if !is_taken(path) {
        return Some(path.to_path_buf());
    }
    match policy {
        CollisionPolicy::Rename => Some(next_free_variant(path, &is_taken)),
        CollisionPolicy::Skip => None,
        CollisionPolicy::KeepExisting | CollisionPolicy::Overwrite => Some(path.to_path_buf()),
    }
}

/// Return the first free stem (n).ext variant of an existing path.
fn next_free_variant(path: &Path, is_taken: &impl Fn(&Path) -> bool) -> PathBuf {
    let stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let suffix = path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    for n in 1..1000 {
        let candidate = path.with_file_name(format!("{stem} ({n}){suffix}"));
        if !is_taken(&candidate) {
            return candidate;
        }
    }
    path.with_file_name(format!("{stem} (too-many){suffix}"))
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
    let ext = basename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let plausible =
        !ext.is_empty() && ext.chars().count() <= 10 && ext.chars().all(|c| c.is_alphanumeric());
    if plausible { Some(ext) } else { None }
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
        assert_eq!(
            sanitize_filename("bad\u{0}\u{1}\u{1f}\u{7f}name"),
            "bad____name"
        );
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

    #[test]
    fn long_input_clamped_to_200() {
        let plain = sanitize_filename(&"a".repeat(250));
        assert_eq!(plain.chars().count(), MAX_FILENAME_LEN);
        assert!(plain.chars().count() <= 200);

        let with_ext = sanitize_filename(&format!("{}.zip", "b".repeat(250)));
        assert!(with_ext.chars().count() <= MAX_FILENAME_LEN);
        assert!(with_ext.ends_with(".zip"));
    }

    #[test]
    fn long_input_with_implausible_extension_just_clamped() {
        let name = sanitize_filename(&format!("{}.{}", "c".repeat(250), "x".repeat(11)));
        assert_eq!(name.chars().count(), MAX_FILENAME_LEN);
        assert!(!name.ends_with(&format!(".{}", "x".repeat(11))));
    }

    #[test]
    fn filename_from_url_strips_query_and_fragment_and_decodes() {
        let url = "https://example.com/dir/report%202024.zip?token=x&y=1#frag";
        assert_eq!(filename_from_url(url), "report 2024.zip");
    }

    #[test]
    fn filename_from_url_query_only_urls() {
        assert_eq!(
            filename_from_url("https://example.com/f.pdf?x=1&y=2"),
            "f.pdf"
        );
    }

    #[test]
    fn filename_from_url_fallbacks() {
        for url in ["https://example.com/", "http://example.com", "http://["] {
            assert_eq!(filename_from_url(url), FALLBACK_NAME, "url: {url}");
        }
    }

    #[test]
    fn filename_from_url_sanitises_hostile_names() {
        assert_eq!(
            filename_from_url("https://example.com/win/CON.zip"),
            "_CON.zip"
        );
    }

    #[test]
    fn filename_from_url_prefers_fragment_filename_over_extensionless_token() {
        let url = "https://fuckingfast.co/vcqpuy297lnc#Black_Myth_Wukong_--_fitgirl-repacks.site_--_.part021.rar";
        assert_eq!(
            filename_from_url(url),
            "Black_Myth_Wukong_--_fitgirl-repacks.site_--_.part021.rar"
        );
        assert_eq!(
            filename_from_url("https://example.com/token#dir/nested/archive.tar.gz"),
            "archive.tar.gz"
        );
    }

    #[test]
    fn filename_from_url_keeps_path_extension_over_fragment() {
        assert_eq!(
            filename_from_url("https://example.com/real.zip#decoy.part1.rar"),
            "real.zip"
        );
    }

    #[test]
    fn filename_from_url_ignores_extensionless_fragment() {
        assert_eq!(
            filename_from_url("https://example.com/vcqpuy297lnc#download"),
            "vcqpuy297lnc"
        );
        assert_eq!(
            filename_from_url("https://example.com/files/page#section"),
            "page"
        );
    }

    #[test]
    fn unique_path_nonexistent_returns_desired_path() {
        let dir = tmp_dir("unique-nonexistent");
        let desired = dir.join("new.txt");
        for policy in [
            CollisionPolicy::KeepExisting,
            CollisionPolicy::Rename,
            CollisionPolicy::Overwrite,
            CollisionPolicy::Skip,
        ] {
            assert_eq!(unique_path(&desired, policy), Some(desired.clone()));
        }
    }

    #[test]
    fn unique_path_keep_existing_and_overwrite_keep_path() {
        let dir = tmp_dir("unique-keep-overwrite");
        let path = dir.join("file.txt");
        std::fs::write(&path, "x").unwrap();
        assert_eq!(
            unique_path(&path, CollisionPolicy::KeepExisting),
            Some(path.clone())
        );
        assert_eq!(
            unique_path(&path, CollisionPolicy::Overwrite),
            Some(path.clone())
        );
    }

    #[test]
    fn unique_path_skip_returns_none() {
        let dir = tmp_dir("unique-skip");
        let path = dir.join("file.txt");
        std::fs::write(&path, "x").unwrap();
        assert_eq!(unique_path(&path, CollisionPolicy::Skip), None);
    }

    #[test]
    fn unique_path_rename_finds_free_variant() {
        let dir = tmp_dir("unique-rename");
        let path = dir.join("file.txt");
        std::fs::write(&path, "x").unwrap();

        let first = unique_path(&path, CollisionPolicy::Rename).unwrap();
        assert_eq!(first, dir.join("file (1).txt"));
        std::fs::write(&first, "y").unwrap();

        let second = unique_path(&path, CollisionPolicy::Rename).unwrap();
        assert_eq!(second, dir.join("file (2).txt"));
        assert!(!second.exists());
    }

    #[test]
    fn unique_path_rename_multi_extension() {
        let dir = tmp_dir("unique-rename-multi");
        let path = dir.join("archive.tar.gz");
        std::fs::write(&path, "z").unwrap();
        assert_eq!(
            unique_path(&path, CollisionPolicy::Rename),
            Some(dir.join("archive.tar (1).gz"))
        );
    }

    #[test]
    fn resolve_destination_blocks_traversal() {
        let base = tmp_dir("resolve-traversal");
        for hostile in ["../../evil.exe", "..\\..\\evil.exe"] {
            let path = resolve_destination(&base, hostile);
            assert!(path.starts_with(&base), "{path:?} escaped {base:?}");
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some(".._.._evil.exe"),
                "hostile input: {hostile}"
            );
        }
    }

    #[test]
    fn resolve_destination_normal_relative_path() {
        let base = tmp_dir("resolve-normal");
        let path = resolve_destination(&base, "report 2024.zip");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("report 2024.zip")
        );
        assert!(path.starts_with(&base));

        let nested = resolve_destination(&base, "sub/dir/file.zip");
        assert_eq!(
            nested.file_name().and_then(|name| name.to_str()),
            Some("sub_dir_file.zip")
        );
        assert!(nested.starts_with(&base));
    }

    #[test]
    fn classify_extension_matches_python() {
        let too_long = format!("too.long.{}", "x".repeat(11));
        let cases: [(&str, Option<&str>); 11] = [
            (
                "http://example.com/files/photo.PNG?dl=1#section",
                Some("png"),
            ),
            ("file.zip", Some("zip")),
            ("archive.tar.gz", Some("gz")),
            ("dir\\interop.docx", Some("docx")),
            ("  spaced.TXT  ", Some("txt")),
            ("missing", None),
            ("trailing.", None),
            (too_long.as_str(), None),
            ("http://example.com/dir/", None),
            ("", None),
            (".gitignore", Some("gitignore")),
        ];
        for (text, expected) in cases {
            assert_eq!(
                classify_extension(text).as_deref(),
                expected,
                "text: {text:?}"
            );
        }
    }
}
