//! Application settings: fields, defaults, validation and JSON round-trip.
//!
//! Port of the Settings model from the Python database.models module.
//! Values are validated exactly like the original __post_init__; the JSON
//! serialisation matches the original to_json/from_json pair so a database
//! written by the Python app can be read back here.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// User agent sent with every request (matches the Python default).
pub const DEFAULT_USER_AGENT: &str = "BulkDownloadManager/0.1 (Windows; +VEEDIZ Studio)";

/// Extensions the Discover page counts as likely files by default.
pub const DEFAULT_ALLOWED_EXTENSIONS: [&str; 41] = [
    "zip", "rar", "7z", "iso", "pdf", "epub", "exe", "msi", "dmg", "img", "tar", "gz", "xz",
    "bz2", "mp3", "mp4", "mkv", "avi", "mov", "wav", "flac", "jpg", "jpeg", "png", "gif",
    "webp", "svg", "tif", "bmp", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "csv", "txt",
    "json", "xml", "apk", "ipa",
];

/// What to do when the destination file already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollisionPolicy {
    /// Existing file is authoritative; the caller must not overwrite it.
    KeepExisting,
    /// Find a free stem (n).ext variant.
    Rename,
    /// Return the path; the caller replaces the file.
    Overwrite,
    /// Skip without touching disk.
    Skip,
}

impl CollisionPolicy {
    /// The exact string stored in settings JSON and the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KeepExisting => "keep_existing",
            Self::Rename => "rename",
            Self::Overwrite => "overwrite",
            Self::Skip => "skip",
        }
    }

    /// Parse the stored representation.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "keep_existing" => Self::KeepExisting,
            "rename" => Self::Rename,
            "overwrite" => Self::Overwrite,
            "skip" => Self::Skip,
            _ => return None,
        })
    }
}

/// UI theme preference. Only Dark is fully styled (as in the Python app).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    Dark,
    Light,
    System,
}

impl Theme {
    /// The exact string stored in settings JSON and the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::System => "system",
        }
    }

    /// Parse the stored representation.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "dark" => Self::Dark,
            "light" => Self::Light,
            "system" => Self::System,
            _ => return None,
        })
    }
}

/// Application settings, persisted via the repository as JSON rows.
///
/// Differences from the Python original, all forced by the type system:
/// numeric fields cannot be negative or non-numeric, and the collision
/// policy/theme are enums, so those ValueError branches cannot trigger.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub default_destination_dir: String,
    pub max_simultaneous: u32,
    pub connection_timeout: f64,
    pub retry_count: u32,
    pub retry_delay: f64,
    pub buffer_size: u32,
    /// 0 = unlimited.
    pub bandwidth_limit_bps: u64,
    pub filename_collision: CollisionPolicy,
    pub notifications: bool,
    pub theme: Theme,
    pub user_agent: String,
    pub include_unknown_types: bool,
    pub allowed_extensions: BTreeSet<String>,
    pub respect_rate_limit: bool,
    /// Escape hatch for LAN/local testing.
    pub allow_private_addresses: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_destination_dir: String::new(),
            max_simultaneous: 3,
            connection_timeout: 30.0,
            retry_count: 3,
            retry_delay: 2.0,
            buffer_size: 64 * 1024,
            bandwidth_limit_bps: 0,
            filename_collision: CollisionPolicy::KeepExisting,
            notifications: true,
            theme: Theme::Dark,
            user_agent: DEFAULT_USER_AGENT.to_string(),
            include_unknown_types: false,
            allowed_extensions: DEFAULT_ALLOWED_EXTENSIONS
                .iter()
                .map(|ext| ext.to_string())
                .collect(),
            respect_rate_limit: true,
            allow_private_addresses: false,
        }
    }
}

impl Settings {
    /// Validate and normalise, mirroring the Python __post_init__.
    ///
    /// max_simultaneous is clamped to 1..=10; every other invalid value is
    /// an error. Returns the normalised settings or a description of the
    /// first problem found.
    pub fn validated(mut self) -> Result<Self, String> {
        self.max_simultaneous = self.max_simultaneous.clamp(1, 10);
        if self.retry_count > 20 {
            return Err("retry_count must be an integer in 0..20".to_string());
        }
        if self.connection_timeout <= 0.0 {
            return Err("connection_timeout must be > 0 seconds".to_string());
        }
        if self.retry_delay < 0.0 {
            return Err("retry_delay must be >= 0 seconds".to_string());
        }
        if self.buffer_size == 0 {
            return Err("buffer_size must be > 0 bytes".to_string());
        }
        if self.user_agent.trim().is_empty() {
            return Err("user_agent must be a non-empty string".to_string());
        }
        let mut normalised = BTreeSet::new();
        for ext in &self.allowed_extensions {
            let name = ext.trim().to_lowercase();
            let name = name.trim_start_matches('.');
            if name.is_empty()
                || name.chars().count() > 10
                || !name.chars().all(|c| c.is_alphanumeric())
            {
                return Err(format!("invalid allowed extension: {ext}"));
            }
            normalised.insert(name.to_string());
        }
        self.allowed_extensions = normalised;
        Ok(self)
    }

    /// Serialise to the JSON document stored by the repository.
    ///
    /// Matches the Python to_json: allowed_extensions is a sorted list and
    /// object keys are sorted (serde_json maps are BTreeMaps).
    pub fn to_json(&self) -> String {
        let value = serde_json::to_value(self).expect("settings serialise infallibly");
        serde_json::to_string(&value).expect("settings serialise infallibly")
    }

    /// Rebuild settings from a stored JSON document.
    ///
    /// Unknown/missing keys fall back to defaults (serde default); values
    /// are re-validated, so a corrupt store raises an error.
    pub fn from_json(raw: &str) -> Result<Self, String> {
        let parsed: Self = serde_json::from_str(raw).map_err(|err| err.to_string())?;
        parsed.validated()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python() {
        let settings = Settings::default();
        assert_eq!(settings.max_simultaneous, 3);
        assert_eq!(settings.connection_timeout, 30.0);
        assert_eq!(settings.retry_count, 3);
        assert_eq!(settings.retry_delay, 2.0);
        assert_eq!(settings.buffer_size, 64 * 1024);
        assert_eq!(settings.bandwidth_limit_bps, 0);
        assert_eq!(settings.filename_collision, CollisionPolicy::KeepExisting);
        assert!(settings.notifications);
        assert_eq!(settings.theme, Theme::Dark);
        assert_eq!(settings.user_agent, DEFAULT_USER_AGENT);
        assert!(!settings.include_unknown_types);
        assert!(settings.allowed_extensions.contains("zip"));
        assert_eq!(settings.allowed_extensions.len(), 41);
        assert!(settings.respect_rate_limit);
        assert!(!settings.allow_private_addresses);
    }

    #[test]
    fn validation_clamps_and_rejects() {
        let settings = Settings { max_simultaneous: 99, ..Settings::default() };
        assert_eq!(settings.validated().unwrap().max_simultaneous, 10);
        let settings = Settings { max_simultaneous: 0, ..Settings::default() };
        assert_eq!(settings.validated().unwrap().max_simultaneous, 1);
        let settings = Settings { retry_count: 21, ..Settings::default() };
        assert!(settings.validated().unwrap_err().contains("retry_count"));
        let settings = Settings { connection_timeout: 0.0, ..Settings::default() };
        assert!(settings.validated().unwrap_err().contains("connection_timeout"));
        let settings = Settings { retry_delay: -1.0, ..Settings::default() };
        assert!(settings.validated().unwrap_err().contains("retry_delay"));
        let settings = Settings { buffer_size: 0, ..Settings::default() };
        assert!(settings.validated().unwrap_err().contains("buffer_size"));
        let settings = Settings { user_agent: "   ".to_string(), ..Settings::default() };
        assert!(settings.validated().unwrap_err().contains("user_agent"));
    }

    #[test]
    fn extension_normalisation_matches_python() {
        let mut exts = BTreeSet::new();
        exts.insert(" .ZIP ".to_string());
        exts.insert("..zip".to_string());
        let settings = Settings { allowed_extensions: exts, ..Settings::default() };
        let validated = settings.validated().unwrap();
        assert!(validated.allowed_extensions.contains("zip"));
        assert_eq!(validated.allowed_extensions.len(), 1);
        let mut bad = BTreeSet::new();
        bad.insert("toolongext1".to_string());
        let settings = Settings { allowed_extensions: bad, ..Settings::default() };
        assert!(settings.validated().unwrap_err().contains("invalid allowed extension"));
        let mut bad = BTreeSet::new();
        bad.insert("zip!".to_string());
        let settings = Settings { allowed_extensions: bad, ..Settings::default() };
        assert!(settings.validated().unwrap_err().contains("invalid allowed extension"));
    }
}

#[cfg(test)]
mod json_tests {
    use super::*;

    #[test]
    fn json_round_trip() {
        let settings = Settings::default();
        let json = settings.to_json();
        let back = Settings::from_json(&json).unwrap();
        assert_eq!(back.max_simultaneous, 3);
        assert_eq!(back.theme, Theme::Dark);
        assert!(back.allowed_extensions.contains("ipa"));
        // Keys are sorted like the Python json.dumps(sort_keys=True).
        assert!(json.starts_with(r#"{"allow_private_addresses":"#));
    }

    #[test]
    fn from_json_python_document() {
        // A document written by the Python app (snake_case keys, sorted).
        let raw = r#"{"allowed_extensions":["zip","pdf"],"bandwidth_limit_bps":0,"buffer_size":65536,"connection_timeout":15.0,"default_destination_dir":"","filename_collision":"rename","include_unknown_types":false,"max_simultaneous":5,"notifications":false,"respect_rate_limit":true,"retry_count":2,"retry_delay":1.0,"theme":"light","user_agent":"test-agent"}"#;
        let settings = Settings::from_json(&raw).unwrap();
        assert_eq!(settings.max_simultaneous, 5);
        assert_eq!(settings.theme, Theme::Light);
        assert_eq!(settings.filename_collision, CollisionPolicy::Rename);
        assert_eq!(settings.connection_timeout, 15.0);
        assert_eq!(settings.allowed_extensions.len(), 2);
    }

    #[test]
    fn from_json_corrupt_raises() {
        assert!(Settings::from_json(r#"{"retry_count":99}"#).is_err());
        assert!(Settings::from_json("not json").is_err());
    }
}
