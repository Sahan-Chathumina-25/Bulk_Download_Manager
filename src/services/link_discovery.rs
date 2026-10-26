//! Page link discovery, robots.txt gating and HEAD enrichment.
//!
//! Port of the legacy ``link_discovery`` module. The scanner is deliberately
//! conservative and never raises: every problem is reported through
//! [`DiscoveryResult::errors`] while the scan keeps as much useful data as it
//! safely can.
//!
//! # Faithful legacy behaviour
//!
//! - Only `a[href]` anchors are considered. Images, `<source>`, `<video>` and
//!   `srcset` are **not** scanned (the Python scanner only looked at anchors).
//! - Any 2xx page body is parsed as HTML; the response `Content-Type` is not
//!   inspected (a binary body simply yields no anchors).
//! - Redirects are followed manually (at most [`MAX_REDIRECT_HOPS`]) and every
//!   hop passes through [`validation::validate_redirect`].
//! - Candidate links are validated with full DNS resolution; links that fail
//!   safety validation are dropped silently, they are not scan errors.
//! - Query strings are preserved; only fragments are stripped.
//!
//! # Client lifetime
//!
//! The adapter lazily builds one shared `reqwest` client and keeps it until a
//! scan arrives with different `user_agent`/`connection_timeout` settings, at
//! which point the client is rebuilt. The robots.txt cache is keyed by origin
//! and survives client rebuilds.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use scraper::{Html, Selector};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::config::Settings;
use crate::models::diagnostic::host_from_url;
use crate::models::discovered_link::{DiscoveryCandidate, DiscoveryResult};
use crate::services::ad_filter::AdFilter;
use crate::services::{file_naming, validation};

/// Maximum redirect hops followed while fetching the page.
pub const MAX_REDIRECT_HOPS: usize = 10;
/// Hard cap on the page body accepted for scanning (5 MiB).
pub const MAX_PAGE_BODY_BYTES: usize = 5 * 1024 * 1024;
/// Maximum number of HEAD probes per scan.
pub const MAX_HEAD_REQUESTS: usize = 15;
/// Maximum number of candidates produced per scan.
pub const MAX_CANDIDATES: usize = 500;
/// Largest `Retry-After` honoured for a HEAD probe, in seconds.
pub const MAX_RETRY_AFTER_SECONDS: f64 = 30.0;

/// Redirect statuses followed manually.
const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];
/// Maximum redirect hops followed while resolving a HEAD probe.
const HEAD_HOPS: usize = 3;

const CANCEL_BEFORE_REQUEST: &str = "scan cancelled before any request was made";
const CANCEL_DURING_SCAN: &str = "scan cancelled by the user";

/// Shown when a page yields zero candidates but ships JavaScript.
const JS_HINT: &str = "no downloadable links were found; this page may render its links with \
     JavaScript — open it in a browser to check for direct links";

/// Anchor text longer than this is truncated (chars, not bytes).
const MAX_ANCHOR_TEXT_CHARS: usize = 200;

/// Shared state around the lazily built HTTP client.
struct CachedClient {
    user_agent: String,
    timeout: Duration,
    client: reqwest::Client,
}

/// HTTP-backed discovery adapter. Cheap to construct; the client is built on
/// the first scan.
pub struct HttpLinkDiscovery {
    client: Mutex<Option<CachedClient>>,
    robots: Mutex<HashMap<String, Option<RobotsRules>>>,
}

impl HttpLinkDiscovery {
    /// Create an adapter with an empty client and robots cache.
    pub fn new() -> Self {
        Self {
            client: Mutex::new(None),
            robots: Mutex::new(HashMap::new()),
        }
    }

    /// Scan `page_url` and return everything discovered, never raising.
    pub async fn scan(
        &self,
        page_url: &str,
        settings: &Settings,
        cancel: Option<CancellationToken>,
    ) -> DiscoveryResult {
        let started = Instant::now();
        let mut result = self.scan_inner(page_url, settings, cancel.as_ref()).await;
        result.elapsed_seconds = started.elapsed().as_secs_f64();
        result
    }

    async fn scan_inner(
        &self,
        page_url: &str,
        settings: &Settings,
        cancel: Option<&CancellationToken>,
    ) -> DiscoveryResult {
        let mut result = DiscoveryResult {
            source_url: page_url.to_string(),
            ..DiscoveryResult::default()
        };
        let allow_private = settings.allow_private_addresses;

        let validated =
            match validation::validate_url_for_network_opts(page_url, allow_private, true) {
                Ok(validated) => validated,
                Err(error) => {
                    result.errors.push(error.to_string());
                    return result;
                }
            };
        result.source_url = validated.url.clone();

        if is_cancelled(cancel) {
            result.errors.push(CANCEL_BEFORE_REQUEST.to_string());
            return result;
        }

        let client = match self.client_for(settings) {
            Ok(client) => client,
            Err(error) => {
                result
                    .errors
                    .push(format!("could not build HTTP client: {error}"));
                return result;
            }
        };

        if settings.respect_rate_limit {
            let origin = origin_of(&validated.url)
                .unwrap_or_else(|| format!("{}://{}", validated.scheme, validated.host));
            if is_cancelled(cancel) {
                result.errors.push(CANCEL_BEFORE_REQUEST.to_string());
                return result;
            }
            if let Some(rules) = self.robots_rules(&origin, &client).await {
                let path = Url::parse(&validated.url)
                    .map(|url| url.path().to_string())
                    .unwrap_or_else(|_| "/".to_string());
                if !rules.can_fetch("*", &path) && !rules.can_fetch(&settings.user_agent, &path) {
                    result.errors.push(format!(
                        "robots.txt for {origin} disallows scanning this URL; ask the site owner \
                         for permission before using an automated scanner here"
                    ));
                    return result;
                }
            }
        }

        let (final_url, body) = match self
            .fetch_page(&client, &validated.url, settings, cancel)
            .await
        {
            Ok(pair) => pair,
            Err(message) => {
                result.errors.push(message);
                return result;
            }
        };
        result.source_url = final_url.clone();

        let links = match extract_links(&final_url, &body, allow_private) {
            Ok(links) => links,
            Err(message) => {
                result.errors.push(message);
                return result;
            }
        };

        let ad_filter = AdFilter::from_settings(settings);
        for error in ad_filter.load_errors() {
            result.errors.push(error.clone());
        }

        let mut truncated = false;
        for (link, anchor_text) in links {
            if result.candidates.len() >= MAX_CANDIDATES {
                truncated = true;
                break;
            }
            let extension = file_naming::classify_extension(&link);
            let is_likely_file = extension
                .as_deref()
                .is_some_and(|ext| settings.allowed_extensions.contains(ext))
                || (settings.include_unknown_types && extension.is_some());
            let filtered_reason = ad_filter.check_host(&host_from_url(&link));
            if filtered_reason.is_some() {
                result.filtered_count += 1;
            }
            result.candidates.push(DiscoveryCandidate {
                filename: file_naming::filename_from_url(&link),
                candidate_url: link,
                extension,
                source_page_url: final_url.clone(),
                content_type: None,
                size: None,
                is_likely_file,
                anchor_text,
                filtered_reason,
            });
        }

        if truncated {
            tracing::warn!(
                limit = MAX_CANDIDATES,
                "discovery candidate cap reached; remaining links ignored"
            );
            result.errors.push(format!(
                "the page contains more than {MAX_CANDIDATES} links; only the first \
                 {MAX_CANDIDATES} were scanned"
            ));
        }

        if result.candidates.is_empty() && contains_script_tag(&body) {
            result.errors.push(JS_HINT.to_string());
        }

        if settings.respect_rate_limit && !result.candidates.is_empty() {
            self.enrich_heads(&client, settings, cancel, &mut result)
                .await;
        }

        result
    }

    /// Fetch the page with manual redirects and a strict body cap.
    async fn fetch_page(
        &self,
        client: &reqwest::Client,
        start: &str,
        settings: &Settings,
        cancel: Option<&CancellationToken>,
    ) -> Result<(String, Vec<u8>), String> {
        let allow_private = settings.allow_private_addresses;
        let mut current = start.to_string();

        for _hop in 0..=MAX_REDIRECT_HOPS {
            if is_cancelled(cancel) {
                return Err(CANCEL_DURING_SCAN.to_string());
            }

            let request = client
                .get(&current)
                .header(USER_AGENT, settings.user_agent.as_str());
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    return Err(format!(
                        "could not fetch the page {} ({}: {}); check the address and your \
                         connection, or raise settings.connection_timeout",
                        validation::redact_url(&current),
                        error_kind(&error),
                        error
                    ));
                }
            };

            let status = response.status().as_u16();
            if REDIRECT_STATUSES.contains(&status) {
                let location = header_text(&response, "Location").unwrap_or_default();
                drop(response);
                match validation::validate_redirect(&current, &location, allow_private) {
                    Ok(validated) => {
                        current = validated.url;
                        continue;
                    }
                    Err(error) => {
                        return Err(format!("redirect refused for safety reasons: {error}"));
                    }
                }
            }

            if !(200..300).contains(&status) {
                return Err(format!(
                    "page fetch failed with HTTP {status} for {}; verify the page URL is \
                     openable in a browser",
                    validation::redact_url(&current)
                ));
            }

            let body = read_body(response, cancel).await?;
            return Ok((current, body));
        }

        Err(format!(
            "page URL exceeds {MAX_REDIRECT_HOPS} redirects; it may be a redirect loop — open it \
             in a browser to confirm"
        ))
    }

    /// Probe likely files with HEAD requests, honouring the politeness gap.
    async fn enrich_heads(
        &self,
        client: &reqwest::Client,
        settings: &Settings,
        cancel: Option<&CancellationToken>,
        result: &mut DiscoveryResult,
    ) {
        let mut budget = MAX_HEAD_REQUESTS;
        let mut last_by_host: HashMap<String, Instant> = HashMap::new();
        let gap = settings.retry_delay;

        for candidate in &mut result.candidates {
            if budget == 0 {
                break;
            }
            if !candidate.is_likely_file {
                continue;
            }
            if is_cancelled(cancel) {
                result.errors.push(CANCEL_DURING_SCAN.to_string());
                return;
            }

            let host = Url::parse(&candidate.candidate_url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
                .unwrap_or_default();

            if gap > 0.0
                && let Some(last) = last_by_host.get(&host)
            {
                let elapsed = last.elapsed().as_secs_f64();
                if elapsed < gap
                    && sleep_or_cancel(Duration::from_secs_f64(gap - elapsed), cancel).await
                {
                    result.errors.push(CANCEL_DURING_SCAN.to_string());
                    return;
                }
            }

            let outcome = head_candidate(
                client,
                &candidate.candidate_url,
                settings.allow_private_addresses,
                cancel,
            )
            .await;
            last_by_host.insert(host, Instant::now());
            if !outcome.network_error {
                budget = budget.saturating_sub(1);
            }

            if is_cancelled(cancel) {
                result.errors.push(CANCEL_DURING_SCAN.to_string());
                return;
            }
            if let Some(content_type) = outcome.content_type {
                candidate.content_type = Some(content_type);
            }
            if let Some(size) = outcome.size {
                candidate.size = Some(size);
            }
        }
    }

    /// Return the cached client, rebuilding it when the relevant settings
    /// changed.
    fn client_for(&self, settings: &Settings) -> Result<reqwest::Client, reqwest::Error> {
        let timeout = client_timeout(settings);
        let mut guard = lock(&self.client);
        if let Some(cached) = guard.as_ref()
            && cached.user_agent == settings.user_agent
            && cached.timeout == timeout
        {
            return Ok(cached.client.clone());
        }

        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        if let Ok(value) = HeaderValue::from_str(&settings.user_agent) {
            headers.insert(USER_AGENT, value);
        }

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .connect_timeout(timeout)
            .read_timeout(timeout.max(Duration::from_secs(30)))
            .default_headers(headers)
            .build()?;
        *guard = Some(CachedClient {
            user_agent: settings.user_agent.clone(),
            timeout,
            client: client.clone(),
        });
        Ok(client)
    }

    /// Fetch and cache robots.txt rules for `origin` (None = permissive).
    async fn robots_rules(&self, origin: &str, client: &reqwest::Client) -> Option<RobotsRules> {
        if let Some(cached) = lock(&self.robots).get(origin).cloned() {
            return cached;
        }

        let url = format!("{origin}/robots.txt");
        let rules = match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => match response.text().await {
                Ok(text) => Some(RobotsRules::parse(&text)),
                Err(_) => None,
            },
            _ => None,
        };

        lock(&self.robots).insert(origin.to_string(), rules.clone());
        rules
    }
}

impl Default for HttpLinkDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

/// Facade matching the legacy `LinkDiscovery` class.
pub struct LinkDiscovery {
    adapter: HttpLinkDiscovery,
}

impl LinkDiscovery {
    /// Create a discovery facade.
    pub fn new() -> Self {
        Self {
            adapter: HttpLinkDiscovery::new(),
        }
    }

    /// Scan `page_url`; see [`HttpLinkDiscovery::scan`].
    pub async fn scan(
        &self,
        page_url: &str,
        settings: &Settings,
        cancel: Option<CancellationToken>,
    ) -> DiscoveryResult {
        self.adapter.scan(page_url, settings, cancel).await
    }
}

impl Default for LinkDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

// -- page fetching helpers ----------------------------------------------------

async fn read_body(
    mut response: reqwest::Response,
    cancel: Option<&CancellationToken>,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    let mut received: usize = 0;

    loop {
        if is_cancelled(cancel) {
            return Err(CANCEL_DURING_SCAN.to_string());
        }
        match response.chunk().await {
            Ok(Some(chunk)) => {
                received += chunk.len();
                if received > MAX_PAGE_BODY_BYTES {
                    return Err(
                        "page body exceeds the 5 MiB scan cap; oversize documents are not scanned"
                            .to_string(),
                    );
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(error) => {
                return Err(format!(
                    "page download interrupted ({}: {}); retry the scan",
                    error_kind(&error),
                    error
                ));
            }
        }
    }

    Ok(body)
}

/// Short classifier used in user-facing scan errors.
fn error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_redirect() {
        "redirect"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else {
        "request"
    }
}

fn header_text(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn client_timeout(settings: &Settings) -> Duration {
    let seconds = if settings.connection_timeout.is_finite() && settings.connection_timeout > 0.0 {
        settings.connection_timeout
    } else {
        30.0
    };
    Duration::from_secs_f64(seconds)
}

/// `scheme://host[:port]` for the origin key used by the robots cache.
fn origin_of(raw: &str) -> Option<String> {
    let parsed = Url::parse(raw).ok()?;
    let host = parsed.host_str()?;
    let host = if matches!(parsed.host(), Some(url::Host::Ipv6(_))) {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let mut origin = format!("{}://{host}", parsed.scheme());
    if let Some(port) = parsed.port() {
        origin.push_str(&format!(":{port}"));
    }
    Some(origin)
}

fn is_cancelled(cancel: Option<&CancellationToken>) -> bool {
    cancel.is_some_and(CancellationToken::is_cancelled)
}

/// Sleep for `duration`; returns `true` when the cancellation token fired.
async fn sleep_or_cancel(duration: Duration, cancel: Option<&CancellationToken>) -> bool {
    match cancel {
        Some(token) => {
            tokio::select! {
                _ = tokio::time::sleep(duration) => false,
                _ = token.cancelled() => true,
            }
        }
        None => {
            tokio::time::sleep(duration).await;
            false
        }
    }
}

// -- HTML extraction ----------------------------------------------------------

/// Extract and validate candidate links plus their anchor text, preserving
/// document order. The text is cosmetic; classification stays URL-based.
fn extract_links(
    page_url: &str,
    body: &[u8],
    allow_private: bool,
) -> Result<Vec<(String, Option<String>)>, String> {
    let html = String::from_utf8_lossy(body);
    let document = Html::parse_document(&html);

    let anchor_selector = Selector::parse("a[href]")
        .map_err(|_| "internal error: could not build anchor selector".to_string())?;
    let base_selector = Selector::parse("base[href]")
        .map_err(|_| "internal error: could not build base selector".to_string())?;

    let base_text = document
        .select(&base_selector)
        .next()
        .and_then(|element| element.value().attr("href"))
        .map(str::trim)
        .filter(|href| !href.is_empty())
        .and_then(|href| {
            Url::parse(page_url)
                .ok()
                .and_then(|base| base.join(href).ok())
                .map(|url| url.to_string())
        })
        .unwrap_or_else(|| page_url.to_string());

    let Ok(base) = Url::parse(&base_text) else {
        return Ok(Vec::new());
    };

    let mut seen_raw: HashSet<String> = HashSet::new();
    let mut seen_valid: HashSet<String> = HashSet::new();
    let mut links = Vec::new();

    for element in document.select(&anchor_selector) {
        let Some(href) = element.value().attr("href") else {
            continue;
        };
        let href = href.trim();
        if href.is_empty() {
            continue;
        }

        let Ok(mut resolved) = base.join(href) else {
            continue;
        };
        resolved.set_fragment(None);
        let absolute = resolved.to_string();
        if absolute.is_empty() {
            continue;
        }
        if !seen_raw.insert(absolute.clone()) {
            continue;
        }

        let Ok(parsed) = Url::parse(&absolute) else {
            continue;
        };
        if !matches!(parsed.scheme(), "http" | "https") {
            continue;
        }
        if parsed.host_str().is_none_or(str::is_empty) {
            continue;
        }

        let Ok(validated) =
            validation::validate_url_for_network_opts(&absolute, allow_private, true)
        else {
            continue;
        };
        if !seen_valid.insert(validated.url.clone()) {
            continue;
        }
        links.push((validated.url, anchor_text(&element)));
    }

    Ok(links)
}

/// Whitespace-collapsed, length-capped anchor text (`None` when blank).
fn anchor_text(element: &scraper::ElementRef<'_>) -> Option<String> {
    let raw = element.text().collect::<Vec<_>>().join(" ");
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(collapsed.chars().take(MAX_ANCHOR_TEXT_CHARS).collect())
}

/// ASCII case-insensitive search for `<script` in the (possibly binary) body.
fn contains_script_tag(body: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"<script";
    body.windows(NEEDLE.len())
        .any(|window| window.eq_ignore_ascii_case(NEEDLE))
}

// -- HEAD enrichment ----------------------------------------------------------

struct HeadOutcome {
    content_type: Option<String>,
    size: Option<u64>,
    network_error: bool,
}

impl HeadOutcome {
    fn empty() -> Self {
        Self {
            content_type: None,
            size: None,
            network_error: false,
        }
    }

    fn network_error() -> Self {
        Self {
            content_type: None,
            size: None,
            network_error: true,
        }
    }
}

/// Resolve one candidate with a manual, validated HEAD redirect loop.
async fn head_candidate(
    client: &reqwest::Client,
    start: &str,
    allow_private: bool,
    cancel: Option<&CancellationToken>,
) -> HeadOutcome {
    let mut current = start.to_string();
    let mut retried = false;
    let mut hops = 0usize;

    loop {
        if is_cancelled(cancel) {
            return HeadOutcome::empty();
        }
        if hops > HEAD_HOPS {
            return HeadOutcome::empty();
        }

        let response = match client.head(&current).send().await {
            Ok(response) => response,
            Err(_) => return HeadOutcome::network_error(),
        };

        let status = response.status().as_u16();
        if REDIRECT_STATUSES.contains(&status) {
            let location = header_text(&response, "Location").unwrap_or_default();
            drop(response);
            match validation::validate_redirect(&current, &location, allow_private) {
                Ok(validated) => {
                    current = validated.url;
                    hops += 1;
                    continue;
                }
                Err(_) => return HeadOutcome::empty(),
            }
        }

        if (status == 429 || status == 503)
            && !retried
            && let Some(seconds) = retry_after_seconds(&response)
            && seconds <= MAX_RETRY_AFTER_SECONDS
        {
            drop(response);
            retried = true;
            if sleep_or_cancel(Duration::from_secs_f64(seconds.max(0.0)), cancel).await {
                return HeadOutcome::empty();
            }
            continue;
        }

        if (200..300).contains(&status) {
            let content_type = header_text(&response, "Content-Type").and_then(|value| {
                let media = value
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase();
                (!media.is_empty()).then_some(media)
            });
            let size = header_text(&response, "Content-Length")
                .and_then(|value| value.trim().parse::<u64>().ok());
            return HeadOutcome {
                content_type,
                size,
                network_error: false,
            };
        }

        return HeadOutcome::empty();
    }
}

/// Parse a `Retry-After` value (delay-seconds or HTTP-date), clamped at zero.
fn retry_after_seconds(response: &reqwest::Response) -> Option<f64> {
    let raw = header_text(response, "Retry-After")?;
    let text = raw.trim();
    if let Ok(seconds) = text.parse::<i64>() {
        return Some(seconds.max(0) as f64);
    }
    let when = httpdate::parse_http_date(text).ok()?;
    let seconds = when
        .duration_since(SystemTime::now())
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    Some(seconds.max(0.0))
}

// -- robots.txt ---------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct RobotsRules {
    groups: Vec<RobotsGroup>,
}

#[derive(Debug, Clone)]
struct RobotsGroup {
    agents: Vec<String>,
    rules: Vec<RobotsRule>,
}

#[derive(Debug, Clone)]
struct RobotsRule {
    allow: bool,
    path: String,
}

impl RobotsRules {
    /// Parse the minimal robots subset the legacy scanner understood.
    ///
    /// Keys are case-insensitive; `#` starts a comment; a group is the run of
    /// consecutive `User-agent` lines followed by its `Allow`/`Disallow`
    /// rules. Empty `Disallow` values mean "allow everything".
    fn parse(text: &str) -> Self {
        let mut groups: Vec<RobotsGroup> = Vec::new();
        let mut current: Option<usize> = None;
        let mut rules_seen = false;

        for raw_line in text.lines() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();

            match key.as_str() {
                "user-agent" => {
                    if current.is_none() || rules_seen {
                        groups.push(RobotsGroup {
                            agents: Vec::new(),
                            rules: Vec::new(),
                        });
                        current = Some(groups.len() - 1);
                        rules_seen = false;
                    }
                    if let Some(index) = current {
                        groups[index].agents.push(value.to_ascii_lowercase());
                    }
                }
                "allow" | "disallow" => {
                    let Some(index) = current else {
                        continue;
                    };
                    rules_seen = true;
                    if value.is_empty() {
                        continue;
                    }
                    groups[index].rules.push(RobotsRule {
                        allow: key == "allow",
                        path: value.to_string(),
                    });
                }
                _ => {}
            }
        }

        Self { groups }
    }

    /// Whether `user_agent` may fetch `path`.
    fn can_fetch(&self, user_agent: &str, path: &str) -> bool {
        let Some(group) = self.group_for(user_agent) else {
            return true;
        };

        let mut best_len = 0usize;
        let mut allowed = true;
        for rule in &group.rules {
            if path.starts_with(&rule.path) {
                let len = rule.path.len();
                if len > best_len || (len == best_len && rule.allow) {
                    best_len = len;
                    allowed = rule.allow;
                }
            }
        }
        allowed
    }

    /// Most specific matching group (longest matching token; `*` is fallback).
    fn group_for(&self, user_agent: &str) -> Option<&RobotsGroup> {
        let requested = user_agent
            .split('/')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let mut best: Option<(&RobotsGroup, usize)> = None;

        for group in &self.groups {
            for agent in &group.agents {
                let score = if agent == "*" {
                    0
                } else if agent.contains(&requested) {
                    agent.len()
                } else {
                    continue;
                };
                if best.is_none_or(|(_, best_score)| score > best_score) {
                    best = Some((group, score));
                }
            }
        }

        best.map(|(group, _)| group)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use crate::engine::fake_site::FakeSite;

    /// Discovery settings for the loopback fixture: private addresses are
    /// allowed, rate limiting (robots + HEAD) is on, and the politeness gap is
    /// tiny so the suite stays fast.
    fn settings() -> Settings {
        Settings {
            connection_timeout: 5.0,
            retry_delay: 0.05,
            allow_private_addresses: true,
            respect_rate_limit: true,
            ..Settings::default()
        }
    }

    async fn scan(site: &Arc<FakeSite>, path: &str, settings: &Settings) -> DiscoveryResult {
        let discovery = LinkDiscovery::new();
        discovery.scan(&site.url(path), settings, None).await
    }

    fn candidate_urls(result: &DiscoveryResult) -> Vec<String> {
        result
            .candidates
            .iter()
            .map(|candidate| candidate.candidate_url.clone())
            .collect()
    }

    fn candidate<'a>(result: &'a DiscoveryResult, path: &str) -> &'a DiscoveryCandidate {
        result
            .candidates
            .iter()
            .find(|candidate| candidate.candidate_url.ends_with(path))
            .unwrap_or_else(|| {
                panic!(
                    "no candidate ending in {path:?}: {:?}",
                    candidate_urls(result)
                )
            })
    }

    // 1
    #[tokio::test]
    async fn page_links_are_absolutised_deduplicated_and_ordered() {
        let site = FakeSite::spawn().await;
        let base = site.url("/page");
        let result = scan(&site, "/page", &settings()).await;

        assert!(
            result.errors.is_empty(),
            "unexpected scan errors: {:?}",
            result.errors
        );
        let origin = base.trim_end_matches("/page");
        let expected = vec![
            format!("{origin}/files/data.bin"),
            format!("{origin}/page"),
            format!("{origin}/about"),
            format!("{origin}/downloads/report.zip"),
            format!("{origin}/docs/manual.pdf"),
            format!("{origin}/img/photo.png"),
            format!("{origin}/files/big.zip"),
        ];
        assert_eq!(candidate_urls(&result), expected);
        assert_eq!(result.source_url, base);
    }

    // 2
    #[tokio::test]
    async fn fragments_are_stripped_and_repeats_collapse() {
        let site = FakeSite::spawn().await;
        let result = scan(&site, "/page", &settings()).await;

        let urls = candidate_urls(&result);
        assert!(
            urls.iter().all(|url| !url.contains('#')),
            "fragments must be stripped: {urls:?}"
        );
        let unique: HashSet<&String> = urls.iter().collect();
        assert_eq!(unique.len(), urls.len(), "duplicates leaked: {urls:?}");
        // The page links data.bin three times (relative x2, absolute x1).
        let data_hits = urls
            .iter()
            .filter(|url| url.ends_with("/files/data.bin"))
            .count();
        assert_eq!(data_hits, 1);
    }

    // 3
    #[tokio::test]
    async fn candidates_are_classified_by_extension() {
        let site = FakeSite::spawn().await;
        let result = scan(&site, "/page", &settings()).await;

        for (path, extension) in [
            ("/files/big.zip", Some("zip")),
            ("/downloads/report.zip", Some("zip")),
            ("/docs/manual.pdf", Some("pdf")),
            ("/img/photo.png", Some("png")),
        ] {
            let found = candidate(&result, path);
            assert_eq!(found.extension.as_deref(), extension, "path {path}");
            assert!(found.is_likely_file, "path {path} should be a likely file");
        }
        // `.bin` is a plausible extension but not in the default allow-list.
        let data = candidate(&result, "/files/data.bin");
        assert_eq!(data.extension.as_deref(), Some("bin"));
        assert!(!data.is_likely_file);
        // Paths without any extension are never likely files.
        for path in ["/page", "/about"] {
            let found = candidate(&result, path);
            assert_eq!(found.extension, None, "path {path}");
            assert!(
                !found.is_likely_file,
                "path {path} must not be a likely file"
            );
        }
    }

    // 4
    #[tokio::test]
    async fn include_unknown_types_promotes_unlisted_but_known_extensions() {
        let site = FakeSite::spawn().await;

        let mut strict = settings();
        strict.allowed_extensions.remove("pdf");
        let result = scan(&site, "/page", &strict).await;
        let manual = candidate(&result, "/docs/manual.pdf");
        assert_eq!(manual.extension.as_deref(), Some("pdf"));
        assert!(
            !manual.is_likely_file,
            "an unlisted extension stays out while the flag is off"
        );

        let mut with_unknown = strict.clone();
        with_unknown.include_unknown_types = true;
        let result = scan(&site, "/page", &with_unknown).await;
        let manual = candidate(&result, "/docs/manual.pdf");
        assert!(
            manual.is_likely_file,
            "include_unknown_types must promote any plausible extension"
        );
        // No extension at all stays out, even with the flag on.
        let about = candidate(&result, "/about");
        assert!(!about.is_likely_file);
    }

    // 5
    #[tokio::test]
    async fn head_enrichment_records_content_type_and_size() {
        let site = FakeSite::spawn().await;
        let result = scan(&site, "/page", &settings()).await;

        let big = candidate(&result, "/files/big.zip");
        assert_eq!(big.content_type.as_deref(), Some("application/zip"));
        assert_eq!(big.size, Some(16_384));
        assert!(site.hits("/files/big.zip") >= 1, "HEAD must reach the site");

        // A likely file the fixture does not serve keeps the unknown markers.
        let report = candidate(&result, "/downloads/report.zip");
        assert_eq!(report.content_type, None);
        assert_eq!(report.size, None);

        // Non-likely candidates are never probed.
        assert_eq!(site.hits("/files/data.bin"), 0);
    }

    // 6
    #[tokio::test]
    async fn missing_robots_txt_allows_scanning() {
        let site = FakeSite::spawn().await;
        let result = scan(&site, "/page", &settings()).await;

        assert_eq!(site.hits("/robots.txt"), 1, "robots.txt is probed once");
        assert!(!result.candidates.is_empty());
        assert!(
            !result
                .errors
                .iter()
                .any(|error| error.contains("robots.txt")),
            "a 404 robots.txt must not block: {:?}",
            result.errors
        );
    }

    // 7
    #[tokio::test]
    async fn robots_disallow_blocks_the_scan_without_fetching_the_page() {
        let site = FakeSite::spawn().await;
        site.set_robots("User-agent: *\nDisallow: /page\n");
        let result = scan(&site, "/page", &settings()).await;

        assert!(result.candidates.is_empty());
        assert_eq!(result.errors.len(), 1, "errors: {:?}", result.errors);
        assert!(result.errors[0].contains("disallows scanning"));
        assert_eq!(site.hits("/page"), 0, "the page must not be fetched");
    }

    // 8
    #[tokio::test]
    async fn robots_longest_match_and_allow_tie_win() {
        let site = FakeSite::spawn().await;
        site.set_robots("User-agent: *\nDisallow: /\nAllow: /page\n");
        let result = scan(&site, "/page", &settings()).await;
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert!(!result.candidates.is_empty());

        let blocked = FakeSite::spawn().await;
        blocked.set_robots("User-agent: *\nAllow: /page\nDisallow: /page\n");
        let result = scan(&blocked, "/page", &settings()).await;
        assert!(
            result.errors.is_empty(),
            "equal-length rules let Allow win: {:?}",
            result.errors
        );
        assert!(!result.candidates.is_empty());

        let longer = FakeSite::spawn().await;
        longer.set_robots("User-agent: *\nAllow: /p\nDisallow: /page\n");
        let result = scan(&longer, "/page", &settings()).await;
        assert_eq!(
            result.errors.len(),
            1,
            "the longer Disallow must win: {:?}",
            result.errors
        );
        assert!(result.errors[0].contains("disallows scanning"));
        assert_eq!(longer.hits("/page"), 0);
    }

    // 9
    #[tokio::test]
    async fn private_page_url_is_refused_when_private_addresses_are_blocked() {
        let site = FakeSite::spawn().await;
        let mut strict = settings();
        strict.allow_private_addresses = false;
        let result = scan(&site, "/page", &strict).await;

        assert!(result.candidates.is_empty());
        assert_eq!(result.errors.len(), 1, "errors: {:?}", result.errors);
        assert!(
            result.errors[0].contains("private"),
            "errors: {:?}",
            result.errors
        );
        assert_eq!(site.hits("/page"), 0);
    }

    // 10
    #[tokio::test]
    async fn private_redirect_target_cannot_be_scanned() {
        // allow_private is true, so the loopback page itself validates; the
        // redirect to the link-local metadata address is still refused by the
        // network layer (the scan never yields candidates). This documents the
        // honest caveat: with allow_private on, validation alone does not stop
        // a private redirect.
        let site = FakeSite::spawn().await;
        let mut fast = settings();
        fast.connection_timeout = 1.0;
        let result = scan(&site, "/redirect-private", &fast).await;

        assert!(result.candidates.is_empty());
        assert!(!result.errors.is_empty(), "the metadata fetch must fail");
    }

    // 11
    #[tokio::test]
    async fn missing_page_reports_the_http_status() {
        let site = FakeSite::spawn().await;
        let result = scan(&site, "/nope", &settings()).await;

        assert!(result.candidates.is_empty());
        assert_eq!(result.errors.len(), 1, "errors: {:?}", result.errors);
        assert!(
            result.errors[0].contains("HTTP 404"),
            "errors: {:?}",
            result.errors
        );
    }

    // 12
    #[tokio::test]
    async fn binary_page_yields_no_candidates_and_no_error() {
        // The scanner parses any 2xx body as HTML; binary bytes simply
        // produce no anchors (the response Content-Type is not inspected).
        let site = FakeSite::spawn().await;
        let result = scan(&site, "/files/data.bin", &settings()).await;

        assert!(result.candidates.is_empty());
        assert!(
            result.errors.is_empty(),
            "a binary body is not an error: {:?}",
            result.errors
        );
    }

    // 13
    #[tokio::test]
    async fn never_raises_and_always_reports_errors() {
        let discovery = LinkDiscovery::new();
        let settings = settings();
        for bad in ["", "   ", "not a url", "ftp://example.com/x", "http://"] {
            let result = discovery.scan(bad, &settings, None).await;
            assert!(result.candidates.is_empty(), "input {bad:?}");
            assert_eq!(result.errors.len(), 1, "input {bad:?}: {:?}", result.errors);
        }

        // A closed port is a network failure, not a panic.
        let result = discovery.scan("http://127.0.0.1:1/", &settings, None).await;
        assert!(result.candidates.is_empty());
        assert!(!result.errors.is_empty());
    }

    // 14
    #[tokio::test]
    async fn oversized_page_is_capped() {
        let site = FakeSite::spawn().await;
        site.set_file("huge", vec![b'a'; MAX_PAGE_BODY_BYTES + 1024]);
        let result = scan(&site, "/huge", &settings()).await;

        assert!(result.candidates.is_empty());
        assert_eq!(result.errors.len(), 1, "errors: {:?}", result.errors);
        assert!(
            result.errors[0].contains("5 MiB"),
            "errors: {:?}",
            result.errors
        );
    }

    // 15
    #[tokio::test]
    async fn stalled_page_times_out_with_an_actionable_message() {
        let site = FakeSite::spawn().await;
        let mut impatient = settings();
        impatient.connection_timeout = 0.2;
        let result = scan(&site, "/stall", &impatient).await;

        assert!(result.candidates.is_empty());
        assert_eq!(result.errors.len(), 1, "errors: {:?}", result.errors);
        assert!(
            result.errors[0].contains("could not fetch the page"),
            "errors: {:?}",
            result.errors
        );
        assert!(
            result.errors[0].contains("timeout"),
            "errors: {:?}",
            result.errors
        );
    }

    // 16
    fn custom_filter_list(label: &str, contents: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};

        static NEXT: AtomicU32 = AtomicU32::new(0);
        let unique = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "bdm-discovery-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("hosts.txt");
        std::fs::write(&path, contents).expect("write filter list");
        path
    }

    // 17
    #[tokio::test]
    async fn ad_host_rows_are_filtered_and_counted() {
        let site = FakeSite::spawn().await;
        let list = custom_filter_list("ads", "127.0.0.2\n");
        let mut config = settings();
        config.respect_rate_limit = false;
        config.ad_filter_enabled = true;
        config.ad_filter_list_path = list.to_string_lossy().into_owned();

        let result = scan(&site, "/page-with-ads", &config).await;

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.candidates.len(), 3);
        assert_eq!(result.filtered_count, 1);

        let ad = candidate(&result, "/ads/banner.gif");
        let reason = ad
            .filtered_reason
            .as_deref()
            .unwrap_or_else(|| panic!("ad row was not filtered: {:?}", candidate_urls(&result)));
        assert!(reason.contains("127.0.0.2"), "reason was {reason:?}");

        assert!(
            candidate(&result, "/files/big.zip")
                .filtered_reason
                .is_none()
        );
        assert!(
            candidate(&result, "/files/data.bin")
                .filtered_reason
                .is_none()
        );
    }

    // 18
    #[tokio::test]
    async fn ad_rows_survive_when_the_filter_is_disabled() {
        let site = FakeSite::spawn().await;
        let mut config = settings();
        config.respect_rate_limit = false;

        let result = scan(&site, "/page-with-ads", &config).await;

        assert_eq!(result.candidates.len(), 3);
        assert_eq!(result.filtered_count, 0);
        assert!(
            result
                .candidates
                .iter()
                .all(|candidate| candidate.filtered_reason.is_none())
        );
    }

    // 19
    #[tokio::test]
    async fn javascript_only_page_reports_the_hint() {
        let site = FakeSite::spawn().await;
        let result = scan(&site, "/js-only", &settings()).await;

        assert!(result.candidates.is_empty());
        assert!(
            result
                .errors
                .iter()
                .any(|error| error.contains("may render its links with JavaScript")),
            "errors: {:?}",
            result.errors
        );
    }

    // 20
    #[tokio::test]
    async fn anchor_text_is_captured_for_the_first_occurrence() {
        let site = FakeSite::spawn().await;
        let result = scan(&site, "/page", &settings()).await;

        let data = candidate(&result, "/files/data.bin");
        assert_eq!(data.anchor_text.as_deref(), Some("data (relative)"));

        let manual = candidate(&result, "/docs/manual.pdf");
        assert_eq!(manual.anchor_text.as_deref(), Some("manual"));
    }

    // 21
    #[tokio::test]
    async fn unreadable_custom_list_falls_back_to_builtin_with_one_error() {
        let site = FakeSite::spawn().await;
        let missing = std::env::temp_dir()
            .join("bdm-missing-filter-list")
            .join("hosts.txt");
        let mut config = settings();
        config.respect_rate_limit = false;
        config.ad_filter_enabled = true;
        config.ad_filter_list_path = missing.to_string_lossy().into_owned();

        let result = scan(&site, "/page-with-ads", &config).await;

        assert_eq!(result.errors.len(), 1, "errors: {:?}", result.errors);
        assert!(
            result.errors[0].contains("could not read the custom list"),
            "errors: {:?}",
            result.errors
        );
        // The list never loaded, so the 127.0.0.2 ad host is not excluded; the
        // built-in list still applies (and does not match a loopback literal).
        assert_eq!(result.filtered_count, 0);
        assert_eq!(result.candidates.len(), 3);
    }

    // 22
    #[tokio::test]
    async fn page_over_the_candidate_cap_reports_truncation() {
        let site = FakeSite::spawn().await;
        let mut config = settings();
        config.respect_rate_limit = false;

        let result = scan(&site, "/many-links", &config).await;

        assert_eq!(result.candidates.len(), MAX_CANDIDATES);
        assert!(
            result
                .errors
                .iter()
                .any(|error| error.contains("more than 500 links")),
            "errors: {:?}",
            result.errors
        );
    }
}
