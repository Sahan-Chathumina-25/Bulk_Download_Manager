//! DNS resolver that pins connections to validator-approved addresses.
//!
//! # Why
//!
//! [`crate::services::validation`] resolves a hostname and screens the
//! addresses, but a hostile resolver can answer validation with a public
//! address and the TCP connection with a private one (DNS rebinding).
//! [`PinningResolver`] closes that gap: the worker registers the addresses the
//! validator approved for a host, and reqwest's connector can then only dial
//! those addresses. A second lookup never happens for a pinned host.
//!
//! # Policy
//!
//! Addresses are screened again here with [`validation::is_private`], using
//! the same `allow_private` flag as validation. Pinned addresses were already
//! vetted, so screening is a defence-in-depth check rather than the primary
//! one. When every candidate is rejected, resolution fails closed with an
//! actionable error instead of falling back to an unvetted lookup.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::services::validation;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Pinned-address resolver, shared by a worker and its reqwest client.
#[derive(Debug, Clone)]
pub struct PinningResolver {
    pins: Arc<Mutex<HashMap<String, Vec<IpAddr>>>>,
    allow_private: bool,
}

impl PinningResolver {
    /// A resolver that permits private addresses only when `allow_private`.
    pub fn new(allow_private: bool) -> Self {
        Self {
            pins: Arc::new(Mutex::new(HashMap::new())),
            allow_private,
        }
    }

    /// Pin `host` to `ips`, replacing any previous pins for that host.
    ///
    /// Hosts are normalised (brackets stripped, ASCII-lowercased) so
    /// `[::1]`, `"::1"` and `FILES.Example` match their `Name` forms.
    pub fn pin(&self, host: &str, ips: &[IpAddr]) {
        let host = normalize_host(host);
        if host.is_empty() || ips.is_empty() {
            return;
        }
        lock(&self.pins).insert(host, ips.to_vec());
    }

    /// Drop every pin (called when a worker finishes with its URL).
    pub fn clear(&self) {
        lock(&self.pins).clear();
    }

    /// Number of hosts currently pinned (diagnostics and tests).
    pub fn pinned_hosts(&self) -> usize {
        lock(&self.pins).len()
    }
}

impl Resolve for PinningResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = normalize_host(name.as_str());
        let pinned = lock(&self.pins)
            .get(&host)
            .cloned()
            .filter(|ips| !ips.is_empty());
        let allow_private = self.allow_private;

        Box::pin(async move {
            let candidates = match pinned {
                Some(ips) => ips,
                None => tokio::net::lookup_host((host.as_str(), 0))
                    .await
                    .map_err(|error| -> BoxError { Box::new(error) })?
                    .map(|address| address.ip())
                    .collect(),
            };

            let addresses: Vec<SocketAddr> = candidates
                .into_iter()
                .filter(|ip| allow_private || !validation::is_private(ip))
                .map(|ip| SocketAddr::new(ip, 0))
                .collect();

            if addresses.is_empty() {
                return Err(format!(
                    "no usable addresses for host {host:?} after private-address screening"
                )
                .into());
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

/// Lowercase and strip the brackets IPv6 names arrive with.
fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::str::FromStr;

    fn name(host: &str) -> Name {
        Name::from_str(host).expect("valid DNS name")
    }

    fn collect(resolving: Resolving) -> Result<Vec<SocketAddr>, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .block_on(resolving)
            .map(|addrs| addrs.collect())
            .map_err(|error| error.to_string())
    }

    #[test]
    fn pinned_addresses_are_returned_without_a_lookup() {
        let resolver = PinningResolver::new(false);
        resolver.pin("files.example", &[IpAddr::from([192, 0, 2, 7])]);

        let addresses = collect(resolver.resolve(name("files.example"))).expect("resolve");
        assert_eq!(addresses, vec![SocketAddr::from(([192, 0, 2, 7], 0))]);
        assert_eq!(resolver.pinned_hosts(), 1);

        resolver.clear();
        assert_eq!(resolver.pinned_hosts(), 0);
    }

    #[test]
    fn pinned_hosts_match_case_and_ipv6_brackets() {
        let resolver = PinningResolver::new(true);
        resolver.pin("[::1]", &[IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)]);
        let addresses = collect(resolver.resolve(name("::1"))).expect("resolve");
        assert!(!addresses.is_empty());
        assert!(addresses.iter().all(|address| address.ip().is_loopback()));
    }

    #[test]
    fn unpinned_hosts_fall_back_to_the_system_lookup() {
        let resolver = PinningResolver::new(true);
        let addresses = collect(resolver.resolve(name("localhost"))).expect("resolve");
        assert!(!addresses.is_empty());
        assert!(addresses.iter().all(|address| address.ip().is_loopback()));
    }

    #[test]
    fn private_pins_are_screened_both_ways() {
        let blocked = PinningResolver::new(false);
        blocked.pin("internal.example", &[IpAddr::from([127, 0, 0, 1])]);
        let error = collect(blocked.resolve(name("internal.example"))).expect_err("must refuse");
        assert!(error.contains("screening"), "unexpected error: {error}");

        let allowed = PinningResolver::new(true);
        allowed.pin("internal.example", &[IpAddr::from([127, 0, 0, 1])]);
        let addresses = collect(allowed.resolve(name("internal.example"))).expect("resolve");
        assert_eq!(addresses.len(), 1);
    }

    #[test]
    fn ipv4_mapped_loopback_pins_are_screened_as_private() {
        let blocked = PinningResolver::new(false);
        let mapped = IpAddr::V6("::ffff:127.0.0.1".parse().expect("valid IPv6"));
        blocked.pin("mapped.example", &[mapped]);
        let error = collect(blocked.resolve(name("mapped.example"))).expect_err("must refuse");
        assert!(error.contains("screening"), "unexpected error: {error}");
    }

    #[test]
    fn normalisation_handles_brackets_case_and_whitespace() {
        assert_eq!(normalize_host(" [FILES.Example] "), "files.example");
        assert_eq!(normalize_host("[::1]"), "::1");
        assert_eq!(normalize_host(""), "");
    }
}
