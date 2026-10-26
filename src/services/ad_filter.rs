//! Built-in ad/tracker host filter for link discovery.
//!
//! Discovery pages on "download collection" sites are frequently wrapped in
//! advertising and analytics anchors. They are not files, and enqueueing them
//! only produces confusing failures. This module classifies a candidate's
//! host before it can be queued; it never rewrites URLs, never fetches
//! anything extra, and never removes a row from the result table (the UI
//! shows filtered rows with the reason attached).
//!
//! # Data provenance and licensing
//!
//! [`BUILTIN_DOMAINS`] is a short, hand-curated list of well-known
//! advertising, analytics and tracking hostnames, compiled for this project
//! from publicly documented hosts (vendor documentation and the hosts entries
//! that commonly appear in public filter lists). It is **not** copied from any
//! licensed blocklist, so no third-party licence attaches to it. It is a
//! heuristic convenience, not a security control: it only ever *excludes*
//! candidates from the queue, and the user can disable it or add their own
//! list.
//!
//! # Updating the list
//!
//! Add or remove entries directly in [`BUILTIN_DOMAINS`] (lowercase bare
//! domains, one per line, sorted is nice but not required), extend the tests
//! that assert exact matching, and re-run the suite. The list is deliberately
//! small and stable rather than exhaustive.
//!
//! # Fail-safe behaviour
//!
//! A user-supplied list that cannot be read is reported once through
//! [`AdFilter::load_errors`] and scanning continues with the built-in list
//! only. Invalid lines are skipped and counted, never fatal.

use crate::config::Settings;

/// Well-known advertising/analytics/tracking hostnames.
///
/// Matching is on the host or a dot-delimited suffix, so `ads.example.com`
/// also matches `example.com` but `notexample.com` never does.
pub const BUILTIN_DOMAINS: [&str; 74] = [
    "1rx.io",
    "33across.com",
    "adap.tv",
    "adcash.com",
    "adcolony.com",
    "addthis.com",
    "addtoany.com",
    "adform.net",
    "adnxs.com",
    "adservice.google.com",
    "adsrvr.org",
    "adsterra.com",
    "advertising.com",
    "agkn.com",
    "amazon-adsystem.com",
    "applovin.com",
    "appnexus.com",
    "bidr.io",
    "bidswitch.net",
    "bluekai.com",
    "casalemedia.com",
    "chartboost.com",
    "criteo.com",
    "criteo.net",
    "demdex.net",
    "districtm.io",
    "doubleclick.net",
    "everesttech.net",
    "exelator.com",
    "exoclick.com",
    "google-analytics.com",
    "googleadservices.com",
    "googlesyndication.com",
    "googletagmanager.com",
    "gumgum.com",
    "hilltopads.net",
    "indexww.com",
    "inmobi.com",
    "ironsrc.com",
    "juicyads.com",
    "krxd.net",
    "liadm.com",
    "lijit.com",
    "mathtag.com",
    "media.net",
    "monetag.com",
    "mopub.com",
    "omtrdc.net",
    "openx.net",
    "outbrain.com",
    "popads.net",
    "popcash.net",
    "propellerads.com",
    "pubmatic.com",
    "quantserve.com",
    "rlcdn.com",
    "rubiconproject.com",
    "scorecardresearch.com",
    "sharethis.com",
    "sharethrough.com",
    "smartadserver.com",
    "sovrn.com",
    "spotx.tv",
    "spotxchange.com",
    "taboola.com",
    "tapad.com",
    "tapjoy.com",
    "teads.tv",
    "trafficjunky.net",
    "triplelift.com",
    "unityads.unity3d.com",
    "vungle.com",
    "yieldmo.com",
    "zeotap.com",
];

/// Domain matcher built from the built-in list plus an optional user list.
#[derive(Debug, Clone, Default)]
pub struct AdFilter {
    /// Sorted, de-duplicated bare domains.
    domains: Vec<String>,
    /// Non-fatal problems loading the user's list (surfaced as scan errors).
    load_errors: Vec<String>,
}

impl AdFilter {
    /// Build a filter for one scan.
    ///
    /// Returns an *empty* filter when `settings.ad_filter_enabled` is false,
    /// so callers can build it unconditionally and rely on
    /// [`AdFilter::is_empty`].
    pub fn from_settings(settings: &Settings) -> Self {
        if !settings.ad_filter_enabled {
            return Self::default();
        }

        let mut domains: Vec<String> = BUILTIN_DOMAINS.iter().map(|d| (*d).to_string()).collect();
        let mut load_errors = Vec::new();

        let path = settings.ad_filter_list_path.trim();
        if !path.is_empty() {
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    let (mut extra, invalid) = parse_hosts_list(&text);
                    domains.append(&mut extra);
                    if invalid > 0 {
                        load_errors.push(format!(
                            "ad/tracker filter: ignored {invalid} invalid line(s) in the \
                             custom list ({path})"
                        ));
                    }
                }
                Err(error) => load_errors.push(format!(
                    "ad/tracker filter: could not read the custom list ({path}: {error}); \
                     the built-in list is still applied"
                )),
            }
        }

        domains.sort_unstable();
        domains.dedup();
        Self {
            domains,
            load_errors,
        }
    }

    /// `Some(reason)` when `host` belongs to a known ad/tracker domain.
    ///
    /// The host is normalised first (case, port, brackets, trailing dot), and
    /// matching is exact or on a dot-delimited suffix — never a substring
    /// match, so `notdoubleclick.net` is not filtered by `doubleclick.net`.
    pub fn check_host(&self, host: &str) -> Option<String> {
        let host = normalize_host(host);
        if host.is_empty() || self.domains.is_empty() {
            return None;
        }

        if let Ok(index) = self.domains.binary_search(&host)
            && self.domains[index] == host
        {
            return Some(format!("matched ad/tracker domain \"{host}\""));
        }

        for domain in &self.domains {
            if let Some(prefix) = host.strip_suffix(domain.as_str())
                && let Some(label) = prefix.strip_suffix('.')
                && !label.is_empty()
            {
                return Some(format!("matched ad/tracker domain \"{domain}\""));
            }
        }
        None
    }

    /// No rules at all (filter disabled, or nothing loaded).
    pub fn is_empty(&self) -> bool {
        self.domains.is_empty()
    }

    /// Number of active rules (built-in plus any valid custom entries).
    pub fn rules_loaded(&self) -> usize {
        self.domains.len()
    }

    /// Non-fatal problems encountered while loading the custom list.
    pub fn load_errors(&self) -> &[String] {
        &self.load_errors
    }
}

/// Lowercase a host, strip a port and IPv6 brackets, and drop a trailing dot.
fn normalize_host(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or("").to_string();
    }
    match host.rsplit_once(':') {
        Some((base, port))
            if !base.contains(':')
                && !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            base.to_string()
        }
        _ => host,
    }
}

/// Parse a hosts-format list; returns `(domains, invalid_line_count)`.
///
/// Accepted line shapes:
///
/// ```text
/// # comment
/// 0.0.0.0 ads.example.com
/// 127.0.0.1 tracker.example.com more.example.com
/// ads.example.com
/// ```
fn parse_hosts_list(text: &str) -> (Vec<String>, usize) {
    let mut domains = Vec::new();
    let mut invalid = 0usize;

    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let names: &[&str] = if tokens.len() >= 2 && is_ip_literal(tokens[0]) {
            &tokens[1..]
        } else if tokens.len() == 1 {
            &tokens[..]
        } else {
            invalid += 1;
            continue;
        };

        for name in names {
            match normalize_domain(name) {
                Some(domain) => domains.push(domain),
                None => invalid += 1,
            }
        }
    }

    (domains, invalid)
}

fn is_ip_literal(token: &str) -> bool {
    token.parse::<std::net::IpAddr>().is_ok()
}

/// Accept a bare domain (`example.com`); reject anything with a scheme, path,
/// whitespace or a single label.
fn normalize_domain(raw: &str) -> Option<String> {
    let domain = normalize_host(raw);
    if domain.is_empty() || domain.len() > 253 {
        return None;
    }
    if !domain.contains('.') {
        return None;
    }
    let valid = domain
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_');
    if !valid || domain.starts_with('.') || domain.ends_with('.') || domain.contains("..") {
        return None;
    }
    Some(domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_settings() -> Settings {
        Settings {
            ad_filter_enabled: true,
            ..Settings::default()
        }
    }

    fn temp_list(name: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bdm-ad-filter-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("ads.txt");
        std::fs::write(&path, contents).expect("write list");
        path
    }

    #[test]
    fn disabled_filter_has_no_rules() {
        let filter = AdFilter::from_settings(&Settings::default());
        assert!(filter.is_empty());
        assert_eq!(filter.rules_loaded(), 0);
        assert_eq!(filter.check_host("doubleclick.net"), None);
    }

    #[test]
    fn builtin_list_blocks_exact_and_subdomain_hosts() {
        let filter = AdFilter::from_settings(&enabled_settings());
        assert_eq!(filter.rules_loaded(), BUILTIN_DOMAINS.len());

        assert!(filter.check_host("doubleclick.net").is_some());
        assert!(filter.check_host("ads.doubleclick.net").is_some());
        assert!(
            filter
                .check_host("stats.g.doubleclick.net")
                .is_some_and(|reason| reason.contains("doubleclick.net"))
        );
    }

    #[test]
    fn substring_lookalikes_are_not_blocked() {
        let filter = AdFilter::from_settings(&enabled_settings());
        for host in [
            "notdoubleclick.net",
            "doubleclick.net.example.com",
            "mydoubleclick.net",
            "example.com",
            "tracker.example.org",
        ] {
            assert_eq!(filter.check_host(host), None, "host {host} must pass");
        }
    }

    #[test]
    fn matching_ignores_case_port_brackets_and_trailing_dot() {
        let filter = AdFilter::from_settings(&enabled_settings());
        for host in [
            "DOUBLECLICK.NET",
            "doubleclick.net:443",
            "doubleclick.net.",
            "  Ads.DoubleClick.Net  ",
            "[::1]",
        ] {
            let expected = !host.contains("::1");
            assert_eq!(filter.check_host(host).is_some(), expected, "host {host}");
        }
    }

    #[test]
    fn hosts_format_list_adds_domains_and_counts_invalid_lines() {
        let path = temp_list(
            "hosts",
            "# comment line\n\
             0.0.0.0 tracker.example\n\
             127.0.0.1 beacon.example more.example\n\
             bare.example\n\
             not-a-domain\n\
             two domains on one line\n\
             ftp://bad.example/x\n",
        );
        let settings = Settings {
            ad_filter_list_path: path.to_string_lossy().into_owned(),
            ..enabled_settings()
        };
        let filter = AdFilter::from_settings(&settings);

        for host in [
            "tracker.example",
            "beacon.example",
            "more.example",
            "bare.example",
            "sub.bare.example",
        ] {
            assert!(
                filter.check_host(host).is_some(),
                "host {host} must be filtered"
            );
        }
        // Three invalid lines: `not-a-domain`, `two domains on one line`,
        // `ftp://bad.example/x`.
        assert_eq!(filter.load_errors().len(), 1);
        assert!(
            filter.load_errors()[0].contains("3 invalid line"),
            "errors: {:?}",
            filter.load_errors()
        );
        assert_eq!(
            filter.rules_loaded(),
            BUILTIN_DOMAINS.len() + 4,
            "four valid custom domains"
        );
    }

    #[test]
    fn unreadable_custom_list_falls_back_to_builtin() {
        let settings = Settings {
            ad_filter_list_path: std::env::temp_dir()
                .join("bdm-ad-filter-does-not-exist")
                .join("missing.txt")
                .to_string_lossy()
                .into_owned(),
            ..enabled_settings()
        };
        let filter = AdFilter::from_settings(&settings);

        assert_eq!(filter.rules_loaded(), BUILTIN_DOMAINS.len());
        assert!(filter.check_host("doubleclick.net").is_some());
        assert_eq!(filter.load_errors().len(), 1);
        assert!(filter.load_errors()[0].contains("could not read"));
    }

    #[test]
    fn builtin_domains_are_normalised_and_unique() {
        let mut sorted = BUILTIN_DOMAINS.to_vec();
        sorted.sort_unstable();
        let mut unique = sorted.clone();
        unique.dedup();
        assert_eq!(sorted, unique, "BUILTIN_DOMAINS contains duplicates");

        for domain in BUILTIN_DOMAINS {
            assert_eq!(
                domain,
                domain.to_ascii_lowercase(),
                "not lowercase: {domain}"
            );
            assert!(!domain.starts_with('.') && !domain.ends_with('.'));
            assert!(domain.contains('.'), "not a dotted domain: {domain}");
        }
    }
}
