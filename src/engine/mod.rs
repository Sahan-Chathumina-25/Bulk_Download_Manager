//! Asynchronous HTTP download engine.
//!
//! Port of the legacy Python ``src.core`` download stack:
//!
//! - [`retry`] — deterministic exponential backoff and `Retry-After` parsing,
//! - [`client`] — the shared `reqwest` client (redirects handled manually),
//! - [`progress`] — progress snapshots emitted to the UI,
//! - [`worker`] — one download from `Connecting` to a settled state,
//! - [`manager`] — queue scheduling, cancellation and pause/resume control.
//!
//! The HTTP fixture ([`fake_site`]) and the end-to-end suite ([`tests`]) are
//! compiled only for tests.

pub mod client;
#[cfg(test)]
pub mod fake_site;
pub mod manager;
pub mod progress;
pub mod retry;
#[cfg(test)]
mod tests;
pub mod worker;
