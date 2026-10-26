//! Deterministic retry/backoff decisions.
//!
//! Port of the legacy Python ``src.core.retry_policy`` module, with the HTTP
//! status-code classification separated from the transport (reqwest) layer.
//!
//! # Determinism
//!
//! Backoff is plain exponential growth **without jitter** (the legacy policy
//! was deterministic and its tests pin exact delays). A server-provided
//! ``Retry-After`` value is honoured, but the *effective* delay is always
//! clamped to [`MAX_RETRY_AFTER_SECONDS`] (15 minutes) so a hostile or broken
//! server cannot park a download for hours.
//!
//! # Retry-After parsing
//!
//! The legacy Python code had two slightly inconsistent parsers. Rust parses
//! `Retry-After` exactly once, at the HTTP boundary, with
//! [`parse_retry_after`]. Only RFC 9110 forms are accepted: a non-negative
//! integer number of seconds, or an HTTP-date. A decimal like `"2.5"` is
//! therefore *junk* and falls back to exponential backoff.

use std::time::SystemTime;

/// HTTP statuses worth retrying with backoff.
pub const TRANSIENT_STATUS_CODES: [u16; 7] = [408, 425, 429, 500, 502, 503, 504];

/// Absolute ceiling (15 minutes) for any single retry delay.
///
/// Applies after `Retry-After` is parsed, so a server cannot ask the worker
/// to sleep for an unbounded time.
pub const MAX_RETRY_AFTER_SECONDS: f64 = 900.0;

/// HTTP statuses that must never be retried (they produce actionable errors).
pub const PERMANENT_STATUS_CODES: [u16; 8] = [400, 401, 403, 404, 405, 406, 410, 451];

/// A classified failure outcome fed into [`RetryPolicy::should_retry`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RetryOutcome {
    /// A bare status code (used by tests and callers that only have the code).
    Status(u16),
    /// A transient HTTP failure, with an optional raw `Retry-After` value.
    Transient { retry_after: Option<f64> },
    /// A transport-level failure (DNS, connect, read, protocol).
    Network,
    /// A permanent failure that must never be retried.
    Permanent,
}

/// Exponential-backoff policy with a hard cap and Retry-After support.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: f64,
    pub max_delay: f64,
}

impl RetryPolicy {
    /// Validate and construct a policy.
    ///
    /// `max_retries` is a `u32`, so the legacy `max_retries >= 0` check is
    /// enforced by the type system and no runtime branch is needed. The two
    /// delay checks reproduce the legacy `ValueError` messages.
    pub fn new(max_retries: u32, base_delay: f64, max_delay: f64) -> Result<Self, String> {
        if base_delay.is_nan() {
            return Err("base_delay must be >= 0".to_string());
        }
        if base_delay < 0.0 {
            return Err("base_delay must be >= 0".to_string());
        }
        if max_delay.is_nan() || max_delay < base_delay {
            return Err("max_delay must be >= base_delay".to_string());
        }
        Ok(Self {
            max_retries,
            base_delay,
            max_delay,
        })
    }

    /// Decide whether `outcome` should be retried after `attempt` failures.
    ///
    /// `attempt` is zero-based before the first retry. Returns
    /// `(should_retry, delay_seconds)`. An explicit `Retry-After` wins over
    /// backoff but is clamped to [`MAX_RETRY_AFTER_SECONDS`].
    pub fn should_retry(&self, outcome: &RetryOutcome, attempt: u32) -> (bool, f64) {
        if attempt >= self.max_retries {
            return (false, 0.0);
        }
        match outcome {
            RetryOutcome::Status(status) => {
                if TRANSIENT_STATUS_CODES.contains(status) {
                    (true, self.backoff(attempt))
                } else {
                    (false, 0.0)
                }
            }
            RetryOutcome::Transient { retry_after } => match retry_after {
                Some(value) => (true, clamp_retry_after(*value)),
                None => (true, self.backoff(attempt)),
            },
            RetryOutcome::Network => (true, self.backoff(attempt)),
            RetryOutcome::Permanent => (false, 0.0),
        }
    }

    /// `min(base_delay * 2^attempt, max_delay)` — no jitter.
    fn backoff(&self, attempt: u32) -> f64 {
        let exponent = i32::try_from(attempt).unwrap_or(i32::MAX);
        (self.base_delay * 2f64.powi(exponent)).min(self.max_delay)
    }
}

/// Clamp a server-provided delay to `0..=MAX_RETRY_AFTER_SECONDS`.
///
/// `Retry-After` is untrusted input; a non-finite value degrades to zero so
/// the worker never sleeps on NaN.
fn clamp_retry_after(seconds: f64) -> f64 {
    if seconds.is_finite() {
        seconds.clamp(0.0, MAX_RETRY_AFTER_SECONDS)
    } else {
        0.0
    }
}

/// Parse an RFC 9110 `Retry-After` header value relative to `now`.
///
/// Returns seconds to wait (clamped at zero for dates in the past), or `None`
/// when the value is absent, blank, or unparseable. Only digits and HTTP-dates
/// are accepted; `"2.5"` is treated as junk and yields `None`.
pub fn parse_retry_after(header: Option<&str>, now: SystemTime) -> Option<f64> {
    let text = header?.trim();
    if text.is_empty() {
        return None;
    }
    if text.bytes().all(|byte| byte.is_ascii_digit()) {
        return text
            .parse::<u64>()
            .ok()
            .map(|seconds| seconds as f64)
            .map(|seconds| seconds.max(0.0));
    }
    let when = httpdate::parse_http_date(text).ok()?;
    match when.duration_since(now) {
        Ok(wait) => Some(wait.as_secs_f64()),
        Err(_) => Some(0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn policy(max_retries: u32, base_delay: f64, max_delay: f64) -> RetryPolicy {
        RetryPolicy::new(max_retries, base_delay, max_delay).expect("valid policy")
    }

    // -- construction validation ------------------------------------------------

    #[test]
    fn invalid_policy_arguments_rejected() {
        assert!(
            RetryPolicy::new(3, -1.0, 10.0)
                .unwrap_err()
                .contains("base_delay")
        );
        assert!(
            RetryPolicy::new(3, 5.0, 1.0)
                .unwrap_err()
                .contains("max_delay")
        );
        assert!(
            RetryPolicy::new(3, f64::NAN, 1.0)
                .unwrap_err()
                .contains("base_delay")
        );
    }

    // -- exponential backoff for transient statuses -----------------------------

    #[test]
    fn transient_statuses_retry_with_exponential_backoff() {
        let policy = policy(6, 0.5, 8.0);
        for status in [429, 500, 502, 503, 504, 408] {
            assert_eq!(
                policy.should_retry(&RetryOutcome::Status(status), 0),
                (true, 0.5)
            );
        }
        let expected = [0.5, 1.0, 2.0, 4.0, 8.0, 8.0];
        for (attempt, delay) in expected.into_iter().enumerate() {
            assert_eq!(
                policy.should_retry(&RetryOutcome::Status(503), attempt as u32),
                (true, delay)
            );
        }
    }

    #[test]
    fn max_delay_cap_holds() {
        let policy = policy(10, 2.0, 3.0);
        assert_eq!(policy.should_retry(&RetryOutcome::Status(429), 7).1, 3.0);
    }

    // -- permanent statuses never retry ------------------------------------------

    #[test]
    fn permanent_statuses_never_retry() {
        let policy = policy(5, 0.1, 10.0);
        for status in PERMANENT_STATUS_CODES {
            assert_eq!(
                policy.should_retry(&RetryOutcome::Status(status), 0),
                (false, 0.0)
            );
            assert_eq!(
                policy.should_retry(&RetryOutcome::Status(status), 4),
                (false, 0.0)
            );
        }
    }

    #[test]
    fn task_mandated_permanent_statuses() {
        let policy = policy(9, 2.0, 60.0);
        for status in [404, 410, 403] {
            assert_eq!(
                policy.should_retry(&RetryOutcome::Status(status), 0),
                (false, 0.0)
            );
        }
    }

    #[test]
    fn transient_and_permanent_sets_are_disjoint_and_sized() {
        assert_eq!(TRANSIENT_STATUS_CODES.len(), 7);
        assert_eq!(PERMANENT_STATUS_CODES.len(), 8);
        for permanent in PERMANENT_STATUS_CODES {
            assert!(!TRANSIENT_STATUS_CODES.contains(&permanent));
        }
    }

    // -- Retry-After honoured -----------------------------------------------------

    #[test]
    fn retry_after_honest_values_are_uncapped() {
        let policy = policy(3, 2.0, 5.0);
        assert_eq!(
            policy.should_retry(
                &RetryOutcome::Transient {
                    retry_after: Some(3.0)
                },
                0
            ),
            (true, 3.0)
        );
        assert_eq!(
            policy.should_retry(
                &RetryOutcome::Transient {
                    retry_after: Some(0.0)
                },
                0
            ),
            (true, 0.0)
        );
        // Honoured exactly even when far above max_delay.
        assert_eq!(
            policy.should_retry(
                &RetryOutcome::Transient {
                    retry_after: Some(30.0)
                },
                0
            ),
            (true, 30.0)
        );
    }

    #[test]
    fn retry_after_is_clamped_to_fifteen_minutes() {
        let policy = policy(3, 1.0, 60.0);
        assert_eq!(
            policy.should_retry(
                &RetryOutcome::Transient {
                    retry_after: Some(86_400.0)
                },
                0
            ),
            (true, MAX_RETRY_AFTER_SECONDS)
        );
        assert_eq!(
            policy.should_retry(
                &RetryOutcome::Transient {
                    retry_after: Some(MAX_RETRY_AFTER_SECONDS + 1.0)
                },
                0
            ),
            (true, MAX_RETRY_AFTER_SECONDS)
        );
        assert_eq!(clamp_retry_after(f64::INFINITY), 0.0);
        assert_eq!(clamp_retry_after(-5.0), 0.0);
    }

    #[test]
    fn retry_after_none_and_garbage_fall_back_to_backoff() {
        let policy = policy(3, 0.5, 60.0);
        assert_eq!(
            policy.should_retry(&RetryOutcome::Transient { retry_after: None }, 0),
            (true, 0.5)
        );
        // Junk is rejected at the HTTP boundary; the policy only sees None.
        assert_eq!(parse_retry_after(Some("soonish"), SystemTime::now()), None);
        assert_eq!(parse_retry_after(Some(""), SystemTime::now()), None);
        assert_eq!(parse_retry_after(Some("2.5"), SystemTime::now()), None);
    }

    #[test]
    fn retry_after_http_date_future_and_past() {
        let policy = policy(3, 0.25, 60.0);
        let now = SystemTime::now();
        let future = httpdate::fmt_http_date(now + Duration::from_secs(30));
        let delay = parse_retry_after(Some(&future), now).expect("future date");
        assert!((20.0..=40.0).contains(&delay), "delay was {delay}");

        let past = httpdate::fmt_http_date(now - Duration::from_secs(60));
        assert_eq!(parse_retry_after(Some(&past), now), Some(0.0));

        let via_policy = policy.should_retry(
            &RetryOutcome::Transient {
                retry_after: parse_retry_after(Some(&future), now),
            },
            0,
        );
        assert!(via_policy.0);
        assert!((20.0..=40.0).contains(&via_policy.1));
    }

    #[test]
    fn transient_without_retry_after_uses_backoff() {
        let policy = policy(3, 1.0, 60.0);
        assert_eq!(
            policy.should_retry(&RetryOutcome::Transient { retry_after: None }, 1),
            (true, 2.0)
        );
    }

    // -- transport + permanent classification ------------------------------------

    #[test]
    fn network_outcome_is_transient() {
        let policy = policy(3, 1.5, 60.0);
        assert_eq!(policy.should_retry(&RetryOutcome::Network, 0), (true, 1.5));
    }

    #[test]
    fn permanent_outcome_never_retries() {
        let policy = policy(3, 1.0, 60.0);
        assert_eq!(
            policy.should_retry(&RetryOutcome::Permanent, 0),
            (false, 0.0)
        );
    }

    // -- caps --------------------------------------------------------------------

    #[test]
    fn max_retries_cap_stops_retries() {
        let policy = policy(2, 0.1, 60.0);
        assert_eq!(
            policy.should_retry(&RetryOutcome::Status(503), 1),
            (true, 0.2)
        );
        assert_eq!(
            policy.should_retry(&RetryOutcome::Status(503), 2),
            (false, 0.0)
        );
        assert_eq!(
            policy.should_retry(
                &RetryOutcome::Transient {
                    retry_after: Some(1.0)
                },
                2
            ),
            (false, 0.0)
        );
    }

    #[test]
    fn zero_retries_policy_never_retries() {
        let policy = policy(0, 2.0, 60.0);
        assert_eq!(
            policy.should_retry(&RetryOutcome::Status(503), 0),
            (false, 0.0)
        );
    }

    // -- parse_retry_after -------------------------------------------------------

    #[test]
    fn parse_retry_after_digits_and_blank() {
        let now = SystemTime::now();
        assert_eq!(parse_retry_after(Some("0"), now), Some(0.0));
        assert_eq!(parse_retry_after(Some("17"), now), Some(17.0));
        assert_eq!(parse_retry_after(Some("  42  "), now), Some(42.0));
        assert_eq!(parse_retry_after(Some(""), now), None);
        assert_eq!(parse_retry_after(Some("   "), now), None);
        assert_eq!(parse_retry_after(None, now), None);
    }

    #[test]
    fn parse_retry_after_rejects_junk_and_decimals() {
        let now = SystemTime::now();
        for junk in ["2.5", "soonish", "+3", "3s", "not a date", "1,5"] {
            assert_eq!(parse_retry_after(Some(junk), now), None, "input {junk:?}");
        }
    }
}
