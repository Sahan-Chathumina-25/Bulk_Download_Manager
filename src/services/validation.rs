//! Network-safety validation and bulk URL-line intake.
//!
//! Port of the Python security module (network-safety validation, run at
//! intake and after every redirect) and the url_validator module (bulk
//! URL-line intake).
//!
//! # DNS-rebinding caveat
//!
//! [`validate_url_for_network`] resolves the hostname once and checks the
//! resolved IPs at validation time. A hostile DNS server can return a safe
//! address during validation and a private address on the actual connection
//! later (classic rebind attack). Full mitigation requires pinning the
//! validated IP onto the connection; callers that re-validate right before
//! each request shrink, but do not eliminate, this window.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

use url::{Host, ParseError, Url};

use crate::errors::UnsafeUrlError;

/// A URL that passed network-safety validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedUrl {
    pub url: String,
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub resolved_ips: Vec<IpAddr>,
    pub is_host_literal_ip: bool,
}

/// Query parameter names that carry secrets and are stripped from log output.
const SECRET_PARAM_NAMES: [&str; 47] = [
    "token",
    "access_token",
    "refresh_token",
    "id_token",
    "auth",
    "authorization",
    "api_key",
    "apikey",
    "api-key",
    "key",
    "secret",
    "client_secret",
    "signature",
    "sig",
    "sigv2",
    "sigv4",
    "hmac",
    "password",
    "passwd",
    "pwd",
    "pass",
    "session",
    "sessionid",
    "session_id",
    "sess",
    "jsessionid",
    "phpsessid",
    "aspsessionid",
    "creds",
    "credentials",
    "credential",
    "oauth",
    "oidc",
    "sso",
    "saml",
    "saslauth",
    "sas",
    "shared_access_signature",
    "sts",
    "awsaccesskeyid",
    "x-amz-signature",
    "x-goog-signature",
    "x-goog-credential",
    "x-goog-date",
    "keyid",
    "jwt",
    "bearer",
];

/// True if the address falls into any blocked IPv4 range.
///
/// Mirrors 0.0.0.0/8, 10/8, 127/8, 169.254/16, 172.16/12, 192.168/16.
fn is_private_v4(ip: &Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 0
        || o[0] == 10
        || o[0] == 127
        || (o[0] == 169 && o[1] == 254)
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
}

/// True if the address falls into any blocked IPv6 range.
///
/// Mirrors ::1/128, ::/128, fc00::/7 and fe80::/10.
fn is_private_v6(ip: &Ipv6Addr) -> bool {
    let segments = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
}

/// True if the address is private/loopback/link-local.
///
/// `pub(crate)` so [`crate::engine::resolver`] can apply the same policy to
/// addresses discovered at connection time (the DNS-pinning half of the
/// rebinding defence).
pub(crate) fn is_private(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => is_private_v6(v6),
    }
}

/// Extract the scheme before the first `:` using urllib's scheme_chars rules.
fn split_scheme(url: &str) -> Option<&str> {
    let idx = url.find(':')?;
    let scheme = &url[..idx];
    if scheme.is_empty() {
        return None;
    }
    let valid = scheme
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some(scheme)
}

/// Everything after `://` up to the first path/query/fragment delimiter.
fn authority_of(url: &str) -> &str {
    let Some(pos) = url.find("://") else {
        return "";
    };
    let rest = &url[pos + 3..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    &rest[..end]
}

/// True when the authority contains userinfo (an `@`), even an empty one.
fn authority_has_userinfo(url: &str) -> bool {
    authority_of(url).contains('@')
}

/// The raw port text, used only to build error messages.
fn extract_port_text(url: &str) -> String {
    let authority = authority_of(url);
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    if let Some(idx) = host_port.rfind(']') {
        let after = &host_port[idx + 1..];
        after.strip_prefix(':').unwrap_or(after).to_string()
    } else if let Some(idx) = host_port.rfind(':') {
        host_port[idx + 1..].to_string()
    } else {
        String::new()
    }
}

/// Map a `url` crate parse error onto the legacy Python wording.
fn map_parse_error(err: ParseError, url_text: &str) -> UnsafeUrlError {
    match err {
        ParseError::EmptyHost => UnsafeUrlError::new("URL has no host"),
        ParseError::InvalidPort => {
            let raw = extract_port_text(url_text);
            match raw.parse::<u64>() {
                Ok(_) => UnsafeUrlError::new(format!("URL port out of range: {raw}")),
                Err(_) => UnsafeUrlError::new(format!(
                    "URL port could not be cast to an integer value: {raw:?}"
                )),
            }
        }
        other => UnsafeUrlError::new(format!("invalid URL: {other}")),
    }
}

/// Resolve a hostname to every address the resolver reports (fail-closed).
fn resolve_host(host: &str) -> Result<Vec<IpAddr>, UnsafeUrlError> {
    let addresses = (host, 0u16)
        .to_socket_addrs()
        .map_err(|err| UnsafeUrlError::new(format!("could not resolve host {host:?}: {err}")))?;
    let ips: Vec<IpAddr> = addresses.map(|addr| addr.ip()).collect();
    if ips.is_empty() {
        return Err(UnsafeUrlError::new(format!(
            "host {host:?} did not resolve to any address"
        )));
    }
    Ok(ips)
}

/// Validate that `url` is safe to connect to (with DNS resolution).
pub fn validate_url_for_network(url: &str) -> Result<ValidatedUrl, UnsafeUrlError> {
    validate_url_for_network_opts(url, false, true)
}

/// Validate that `url` is safe to connect to from this machine.
///
/// Checks, in the legacy order:
/// - the scheme must be exactly `http` or `https`
/// - a host must be present, and embedded userinfo is rejected
/// - the port, if present, must be a valid TCP number (1..=65535)
/// - literal IP hosts are checked directly; hostnames are resolved via DNS
///   and rejected when *every* resolved address is private/link-local
///   (see the DNS-rebinding caveat in the module docs).
///
/// `allow_private` is the explicit opt-in for private/loopback addresses.
/// `resolve_dns = false` is preflight-only: hostnames are not resolved, so
/// their resolved-address privacy cannot be checked; every other check runs.
pub fn validate_url_for_network_opts(
    url: &str,
    allow_private: bool,
    resolve_dns: bool,
) -> Result<ValidatedUrl, UnsafeUrlError> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(UnsafeUrlError::new("URL is empty"));
    }

    let scheme = split_scheme(trimmed).unwrap_or("").to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(UnsafeUrlError::new(format!(
            "unsupported URL scheme {scheme:?}; only http/https are allowed"
        )));
    }

    // urlsplit only treats `//` as an authority, so `http:example.com/x`
    // has no host.
    let rest = &trimmed[scheme.len() + 1..];
    if !rest.starts_with("//") {
        return Err(UnsafeUrlError::new("URL has no host"));
    }

    let parsed = Url::parse(trimmed).map_err(|err| map_parse_error(err, trimmed))?;

    if authority_has_userinfo(trimmed) {
        return Err(UnsafeUrlError::new(
            "URL embeds userinfo (user:password@); put authentication in headers instead",
        ));
    }

    // The url crate accepts port 0; urlsplit rejects it.
    if parsed.port() == Some(0) {
        return Err(UnsafeUrlError::new("URL port out of range: 0"));
    }

    let port = parsed
        .port()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });

    let (host, resolved_ips, is_host_literal_ip) = match parsed.host() {
        Some(Host::Ipv4(ip)) => {
            let host = ip.to_string();
            let address = IpAddr::V4(ip);
            if is_private(&address) && !allow_private {
                return Err(UnsafeUrlError::new(format!(
                    "refusing to connect to private/loopback address {ip} (host {host:?})"
                )));
            }
            (host, vec![address], true)
        }
        Some(Host::Ipv6(ip)) => {
            let host = ip.to_string();
            let address = IpAddr::V6(ip);
            if is_private(&address) && !allow_private {
                return Err(UnsafeUrlError::new(format!(
                    "refusing to connect to private/loopback address {ip} (host {host:?})"
                )));
            }
            (host, vec![address], true)
        }
        Some(Host::Domain(domain)) => {
            let host = domain.to_string();
            let mut resolved_ips = Vec::new();
            if resolve_dns {
                resolved_ips = resolve_host(&host)?;
                if !allow_private && resolved_ips.iter().all(is_private) {
                    return Err(UnsafeUrlError::new(format!(
                        "all resolved addresses for host {host:?} are private/loopback; \
                         refusing to connect"
                    )));
                }
            }
            (host, resolved_ips, false)
        }
        None => return Err(UnsafeUrlError::new("URL has no host")),
    };

    Ok(ValidatedUrl {
        url: trimmed.to_string(),
        scheme,
        host,
        port,
        resolved_ips,
        is_host_literal_ip,
    })
}

/// Validate a redirect hop, rejecting silent https-to-http downgrades.
pub fn validate_redirect(
    current: &str,
    target: &str,
    allow_private: bool,
) -> Result<ValidatedUrl, UnsafeUrlError> {
    let location = target.trim();
    if location.is_empty() {
        return Err(UnsafeUrlError::new("redirect has an empty Location header"));
    }

    let current_url = Url::parse(current).ok();
    let joined = match &current_url {
        Some(base) => base
            .join(location)
            .map_err(|_| {
                UnsafeUrlError::new(format!(
                    "redirect target is not an http(s) URL: {location:?}"
                ))
            })?
            .to_string(),
        None => location.to_string(),
    };

    let lowered = joined.to_ascii_lowercase();
    if !lowered.starts_with("http://") && !lowered.starts_with("https://") {
        return Err(UnsafeUrlError::new(format!(
            "redirect target is not an http(s) URL: {location:?}"
        )));
    }

    let target_url = Url::parse(&joined).map_err(|err| map_parse_error(err, &joined))?;

    if let Some(current_url) = &current_url
        && current_url.scheme() == "https"
        && target_url.scheme() == "http"
    {
        return Err(UnsafeUrlError::new(
            "refusing https-to-http downgrade during redirect",
        ));
    }

    validate_url_for_network_opts(target_url.as_str(), allow_private, true)
}

/// True when the raw URL spells an explicit path after its authority.
fn has_explicit_path(raw: &str) -> bool {
    let Some(pos) = raw.find("://") else {
        return false;
    };
    let rest = &raw[pos + 3..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    rest[end..].starts_with('/')
}

/// Redact the query string, masking secret-like parameter values.
fn redact_query(raw: &str) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (name, value) in form_urlencoded::parse(raw.as_bytes()) {
        let lowered = name.to_lowercase();
        if SECRET_PARAM_NAMES.contains(&lowered.as_str()) {
            serializer.append_pair(&lowered, "***");
        } else {
            serializer.append_pair(&name, &value);
        }
    }
    serializer.finish()
}

/// Return a log-safe version of `url`.
///
/// Userinfo is removed entirely and any query parameter whose name matches
/// a secret-like name is masked (name canonicalised to lowercase, value
/// `***`). Harmless parameter names and values are preserved verbatim; the
/// fragment is dropped. Invalid input collapses to `<redacted-url>`.
/// (Divergence note: Python's netloc omits IPv6 brackets, which corrupts the
/// redacted URL; we keep them.)
pub fn redact_url(url: &str) -> String {
    let parsed = match Url::parse(url) {
        Ok(parsed) => parsed,
        Err(_) => return "<redacted-url>".to_string(),
    };

    let netloc = match parsed.host_str() {
        Some(host) => {
            let host_text = if matches!(parsed.host(), Some(Host::Ipv6(_))) {
                format!("[{host}]")
            } else {
                host.to_string()
            };
            match parsed.port() {
                Some(port) => format!("{host_text}:{port}"),
                None => host_text,
            }
        }
        None => String::new(),
    };
    if netloc.is_empty() {
        return "<redacted-url>".to_string();
    }

    let path = if parsed.path() == "/" && !has_explicit_path(url) {
        ""
    } else {
        parsed.path()
    };

    let query = redact_query(parsed.query().unwrap_or(""));
    let mut out = format!("{}://{netloc}{path}", parsed.scheme());
    if !query.is_empty() {
        out.push('?');
        out.push_str(&query);
    }
    out
}

/// Outcome of ingesting a block of pasted/imported URL lines.
#[derive(Default)]
pub struct ParseResult {
    /// Validated, safe URLs in order of first appearance.
    pub accepted: Vec<String>,
    /// `(line_as_given, reason)` pairs for unusable lines.
    pub rejected: Vec<(String, String)>,
    /// Lines that exactly repeated an earlier URL.
    pub duplicates: Vec<String>,
}

/// Return the URL part of a line (the first tab-separated token).
fn extract_url(raw_line: &str) -> String {
    let mut url = raw_line.trim();
    if let Some(idx) = url.find('\t') {
        url = &url[..idx];
    }
    url.trim().to_string()
}

/// Parse lines of URLs into accepted/rejected/duplicate buckets.
///
/// Handles CRLF, LF and bare-CR endings, strips whitespace, skips blank
/// lines and deduplicates exact repeats (first occurrence wins, order
/// preserved). Every URL must be http/https and network-safe; private
/// addresses are rejected unless `allow_private` is set.
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
        let candidate = extract_url(line);
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
    let scheme = split_scheme(candidate).unwrap_or("").to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Some("only http and https URLs are supported".to_string());
    }
    match validate_url_for_network_opts(candidate, allow_private, true) {
        Ok(_) => None,
        Err(err) => Some(format!("unsafe URL: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const LOOP: &str = "http://127.0.0.1:9";

    fn query_map(url: &str) -> HashMap<String, String> {
        let parsed = Url::parse(url).unwrap();
        form_urlencoded::parse(parsed.query().unwrap_or("").as_bytes())
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect()
    }

    // -- validate_url_for_network: rejections -------------------------------

    #[test]
    fn private_targets_rejected() {
        for url in [
            "http://127.0.0.1/x",
            "http://localhost/x",
            "http://10.1.2.3/f.bin",
            "http://192.168.1.10/f.bin",
            "http://172.16.9.1/f.bin",
            "http://172.31.255.1/f.bin",
            "http://169.254.169.254/latest/meta-data/",
            "http://0.0.0.0/x",
            "http://[::1]/x",
            "http://[fe80::1]/x",
        ] {
            assert!(
                validate_url_for_network(url).is_err(),
                "expected rejection: {url}"
            );
        }
    }

    #[test]
    fn bad_scheme_and_empty_rejected() {
        for url in [
            "ftp://example.com/file.zip",
            "javascript:alert(1)",
            "file:///C:/evil.exe",
            "",
            "   ",
        ] {
            assert!(
                validate_url_for_network(url).is_err(),
                "expected rejection: {url:?}"
            );
        }
    }

    #[test]
    fn userinfo_rejected() {
        // allow_private irrelevant: credentials-in-URL are rejected outright
        let err = validate_url_for_network_opts("http://user:secretpw@127.0.0.1/x", true, true)
            .unwrap_err();
        assert!(err.to_string().contains("userinfo"));
    }

    #[test]
    fn port_out_of_range_rejected() {
        let err =
            validate_url_for_network_opts("http://127.0.0.1:99999/x", true, true).unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    // -- validate_url_for_network: allow_private escape hatch ---------------

    #[test]
    fn allow_private_accepts_literal_loopback() {
        let validated =
            validate_url_for_network_opts("http://127.0.0.1:8000/data.bin", true, true).unwrap();
        assert_eq!(validated.scheme, "http");
        assert_eq!(validated.host, "127.0.0.1");
        assert_eq!(validated.port, 8000);
        assert!(validated.is_host_literal_ip);
        assert_eq!(
            validated.resolved_ips,
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
        );
    }

    #[test]
    fn allow_private_accepts_localhost_hostname() {
        let validated =
            validate_url_for_network_opts("http://localhost/files/a.zip", true, true).unwrap();
        assert_eq!(validated.host, "localhost");
        assert!(!validated.is_host_literal_ip);
        assert!(!validated.resolved_ips.is_empty());
        assert!(validated.resolved_ips.iter().all(|ip| ip.is_loopback()));
    }

    #[test]
    fn allow_private_accepts_ipv6_loopback_with_default_https_port() {
        let validated = validate_url_for_network_opts("https://[::1]/x", true, true).unwrap();
        assert_eq!(validated.scheme, "https");
        assert_eq!(validated.port, 443);
    }

    // -- redact_url ---------------------------------------------------------

    #[test]
    fn redact_url_masks_secret_query_params() {
        let original = "https://example.com/download/f.zip?keep=yes&token=abc123\
                        &SIG=deadbeef&api_key=K9&notasecret=plain";
        let redacted = redact_url(original);

        assert!(!redacted.contains("abc123"));
        assert!(!redacted.contains("deadbeef"));
        assert!(!redacted.contains("K9"));
        assert!(redacted.contains("plain"));
        assert!(redacted.contains("example.com/download/f.zip"));

        let values = query_map(&redacted);
        assert_eq!(values["token"], "***");
        assert_eq!(values["sig"], "***");
        assert_eq!(values["api_key"], "***");
        assert_eq!(values["keep"], "yes");
        assert_eq!(values["notasecret"], "plain");
    }

    #[test]
    fn redact_url_strips_userinfo() {
        let redacted = redact_url("https://alice:p4ssw0rd@example.com/x?a=1&password=hunter2");
        assert!(!redacted.contains("alice"));
        assert!(!redacted.contains("p4ssw0rd"));
        assert!(!redacted.contains("hunter2"));
        assert!(redacted.contains("example.com"));
        let values = query_map(&redacted);
        assert_eq!(values["a"], "1");
        assert_eq!(values["password"], "***");
    }

    #[test]
    fn redact_url_preserves_port_and_path() {
        let redacted = redact_url("https://example.com:8443/a/b/c.bin?k=1");
        assert!(redacted.contains(":8443"));
        assert!(redacted.contains("/a/b/c.bin"));
    }

    #[test]
    fn redact_url_unparseable_collapses() {
        for bad in ["", "   ", "not a url", "scheme-only:"] {
            assert_eq!(redact_url(bad), "<redacted-url>", "input: {bad:?}");
        }
    }

    // -- validate_redirect --------------------------------------------------

    #[test]
    fn validate_redirect_blocks_https_to_http_downgrade() {
        let err = validate_redirect(
            "https://example.com/start",
            "http://example.com/next",
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("downgrade"));
    }

    #[test]
    fn validate_redirect_blocks_private_target() {
        for target in [
            "http://10.9.8.7/hit",
            "http://169.254.169.254/latest/meta-data/",
        ] {
            let err = validate_redirect("http://example.com/start", target, false).unwrap_err();
            assert!(err.to_string().contains("private"), "target: {target}");
        }
    }

    #[test]
    fn validate_redirect_blocking_is_independent_of_allow_private() {
        // without allow_private the private redirect is refused as usual
        let err = validate_redirect("http://example.com/start", "http://127.0.0.1:9/hit", false)
            .unwrap_err();
        assert!(err.to_string().contains("private"));
    }

    #[test]
    fn validate_redirect_resolves_relative_location() {
        let validated =
            validate_redirect("http://127.0.0.1:77/a/b/c", "/files/data.bin", true).unwrap();
        assert_eq!(validated.url, "http://127.0.0.1:77/files/data.bin");
        assert_eq!(validated.host, "127.0.0.1");
        assert_eq!(validated.port, 77);
    }

    #[test]
    fn validate_redirect_allows_private_between_loopback_hosts() {
        let validated = validate_redirect(
            "http://127.0.0.1:77/start",
            "http://127.0.0.1:88/next",
            true,
        )
        .unwrap();
        assert_eq!(validated.host, "127.0.0.1");
        assert_eq!(validated.port, 88);
    }

    #[test]
    fn validate_redirect_empty_location_rejected() {
        let err = validate_redirect("https://example.com/start", "  ", false).unwrap_err();
        assert!(err.to_string().contains("empty Location"));
    }

    #[test]
    fn validate_redirect_non_http_location_rejected() {
        let err = validate_redirect("https://example.com/start", "javascript:alert(1)", false)
            .unwrap_err();
        assert!(err.to_string().contains("not an http"));
    }

    // -- parse_url_lines ----------------------------------------------------

    #[test]
    fn line_endings() {
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
    fn blank_and_whitespace_only_lines_skipped() {
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
    fn empty_input_yields_empty_result() {
        for text in ["", "   ", "\r\n\r\n"] {
            let result = parse_url_lines(text, false);
            assert!(result.accepted.is_empty());
            assert!(result.rejected.is_empty());
            assert!(result.duplicates.is_empty());
        }
    }

    #[test]
    fn tab_separated_url_name_import() {
        let text = format!("{LOOP}/file.zip\tinstaller package\n");
        let result = parse_url_lines(&text, true);
        assert_eq!(result.accepted, vec![format!("{LOOP}/file.zip")]);
    }

    #[test]
    fn invalid_urls_rejected_with_reason() {
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
    fn duplicate_needs_exact_match() {
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
    fn mixed_accepted_and_rejected_in_one_batch() {
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
