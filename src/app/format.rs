//! Human-readable formatting helpers.
//!
//! These are a faithful port of `legacy-python/src/app/utils_format.py` so the
//! Rust UI reports sizes, speeds and ETAs exactly like the Python original.
//! The legacy semantics are intentionally preserved, quirks included:
//!
//! * `None` or a negative value renders as an em dash (`—`).
//! * Byte sizes switch units at 1024 and keep one decimal place, except the
//!   `B` bucket which is rendered as a whole number without thousands commas.
//! * Speeds below `1` render as `0 B/s`, the `B/s` bucket rounds with banker's
//!   rounding (no commas), and larger buckets use one decimal plus commas.
//! * ETAs of zero render as `<1s`.

/// Placeholder shown when a value is unknown.
pub const DASH: &str = "—";

/// Unit suffixes, in the same order as the legacy implementation.
const BYTE_UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];

/// Formats a byte count for display, e.g. `1.5 MB`.
///
/// Returns [`DASH`] when the size is unknown.
pub fn format_bytes(bytes: Option<u64>) -> String {
    let Some(bytes) = bytes else {
        return DASH.to_owned();
    };

    let mut value = bytes as f64;
    for (index, unit) in BYTE_UNITS.iter().enumerate() {
        let is_last = index == BYTE_UNITS.len() - 1;
        if value < 1024.0 || is_last {
            return if index == 0 {
                // Legacy: `f"{int(value)} B"` — no decimal, no separators.
                format!("{bytes} B")
            } else {
                format!("{} {unit}", one_decimal_with_commas(value))
            };
        }
        value /= 1024.0;
    }

    // Unreachable: the loop always returns on the final unit.
    DASH.to_owned()
}

/// Formats a transfer rate, e.g. `1.2 MB/s`.
///
/// Returns [`DASH`] when the rate is unknown or negative.
pub fn format_speed(bytes_per_second: Option<f64>) -> String {
    let Some(bytes_per_second) = bytes_per_second else {
        return DASH.to_owned();
    };
    if !bytes_per_second.is_finite() || bytes_per_second < 0.0 {
        return DASH.to_owned();
    }
    if bytes_per_second < 1.0 {
        return "0 B/s".to_owned();
    }

    let mut value = bytes_per_second;
    for (index, unit) in BYTE_UNITS.iter().enumerate() {
        let is_last = index == BYTE_UNITS.len() - 1;
        if value < 1024.0 || is_last {
            return if index == 0 {
                // Legacy: `f"{int(round(value))} B/s"` — banker's rounding, no separators.
                let rounded = value.round_ties_even();
                format!("{} B/s", rounded as u64)
            } else {
                format!("{} {unit}/s", one_decimal_with_commas(value))
            };
        }
        value /= 1024.0;
    }

    // Unreachable: the loop always returns on the final unit.
    DASH.to_owned()
}

/// Formats a remaining-time estimate, e.g. `3:07` or `1:02:03`.
///
/// Returns [`DASH`] when the estimate is unknown, and `<1s` when it rounds down
/// to zero.
pub fn format_eta(seconds: Option<u64>) -> String {
    let Some(seconds) = seconds else {
        return DASH.to_owned();
    };
    if seconds == 0 {
        return "<1s".to_owned();
    }

    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

/// Trims an ISO-8601 timestamp to `YYYY-MM-DD HH:MM` for dense tables.
///
/// Anything that does not look like a timestamp is passed through unchanged.
pub fn short_timestamp(value: &str) -> String {
    let mut trimmed: String = value.chars().take(16).collect();
    if let Some(index) = trimmed.find('T') {
        trimmed.replace_range(index..=index, " ");
    }
    trimmed
}

/// Formats `value` with one decimal place and comma-grouped thousands.
///
/// Rust's float formatting rounds half-to-even, matching Python's `f"{v:,.1f}"`.
fn one_decimal_with_commas(value: f64) -> String {
    let formatted = format!("{value:.1}");
    let (integer_part, fraction_part) = match formatted.split_once('.') {
        Some(parts) => parts,
        None => (formatted.as_str(), "0"),
    };

    format!("{}.{}", group_thousands(integer_part), fraction_part)
}

/// Inserts `,` every three digits in a non-negative integer string.
fn group_thousands(digits: &str) -> String {
    let (sign, digits) = match digits.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", digits),
    };

    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3 + sign.len());
    grouped.push_str(sign);

    let total = digits.len();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (total - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }

    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_unknown_is_dash() {
        assert_eq!(format_bytes(None), DASH);
    }

    #[test]
    fn bytes_below_unit_threshold_have_no_decimals() {
        assert_eq!(format_bytes(Some(0)), "0 B");
        assert_eq!(format_bytes(Some(999)), "999 B");
        assert_eq!(format_bytes(Some(1023)), "1023 B");
    }

    #[test]
    fn bytes_switch_units_at_1024() {
        assert_eq!(format_bytes(Some(1024)), "1.0 KB");
        assert_eq!(format_bytes(Some(1536)), "1.5 KB");
        assert_eq!(format_bytes(Some(1024 * 1024)), "1.0 MB");
        assert_eq!(format_bytes(Some(1024 * 1024 * 1024)), "1.0 GB");
        assert_eq!(format_bytes(Some(1024u64.pow(4))), "1.0 TB");
        assert_eq!(format_bytes(Some(1024u64.pow(5))), "1.0 PB");
    }

    #[test]
    fn bytes_largest_unit_never_overflows() {
        assert_eq!(format_bytes(Some(1024u64.pow(5) * 2048)), "2,048.0 PB");
    }

    #[test]
    fn bytes_use_thousands_separators() {
        // 1000 MB stays in the MB bucket, so it needs a separator.
        let size = 1000 * 1024 * 1024;
        assert_eq!(format_bytes(Some(size)), "1,000.0 MB");
    }

    #[test]
    fn speed_unknown_and_negative_are_dash() {
        assert_eq!(format_speed(None), DASH);
        assert_eq!(format_speed(Some(-1.0)), DASH);
        assert_eq!(format_speed(Some(f64::NAN)), DASH);
    }

    #[test]
    fn speed_below_one_is_zero() {
        assert_eq!(format_speed(Some(0.0)), "0 B/s");
        assert_eq!(format_speed(Some(0.99)), "0 B/s");
    }

    #[test]
    fn speed_byte_bucket_rounds_without_separators() {
        assert_eq!(format_speed(Some(1.0)), "1 B/s");
        assert_eq!(format_speed(Some(1.4)), "1 B/s");
        assert_eq!(format_speed(Some(1.6)), "2 B/s");
        assert_eq!(format_speed(Some(1023.4)), "1023 B/s");
    }

    #[test]
    fn speed_banker_rounds_half_to_even() {
        // `int(round(2.5)) == 2` and `int(round(3.5)) == 4` in Python.
        assert_eq!(format_speed(Some(2.5)), "2 B/s");
        assert_eq!(format_speed(Some(3.5)), "4 B/s");
    }

    #[test]
    fn speed_switches_units_at_1024() {
        assert_eq!(format_speed(Some(1024.0)), "1.0 KB/s");
        assert_eq!(format_speed(Some(1024.0 * 1024.0)), "1.0 MB/s");
    }

    #[test]
    fn speed_uses_thousands_separators_above_bytes() {
        // 1000 MB/s stays in the MB bucket, so it needs a separator.
        assert_eq!(format_speed(Some(1000.0 * 1024.0 * 1024.0)), "1,000.0 MB/s");
    }

    #[test]
    fn eta_unknown_is_dash() {
        assert_eq!(format_eta(None), DASH);
    }

    #[test]
    fn eta_zero_renders_sub_second() {
        assert_eq!(format_eta(Some(0)), "<1s");
    }

    #[test]
    fn eta_under_an_hour_uses_minutes() {
        assert_eq!(format_eta(Some(1)), "0:01");
        assert_eq!(format_eta(Some(59)), "0:59");
        assert_eq!(format_eta(Some(60)), "1:00");
        assert_eq!(format_eta(Some(187)), "3:07");
        assert_eq!(format_eta(Some(3599)), "59:59");
    }

    #[test]
    fn eta_from_an_hour_uses_hours() {
        assert_eq!(format_eta(Some(3600)), "1:00:00");
        assert_eq!(format_eta(Some(3723)), "1:02:03");
        assert_eq!(format_eta(Some(86_400)), "24:00:00");
    }

    #[test]
    fn thousands_grouping_handles_edge_lengths() {
        assert_eq!(group_thousands("0"), "0");
        assert_eq!(group_thousands("999"), "999");
        assert_eq!(group_thousands("1000"), "1,000");
        assert_eq!(group_thousands("1234567"), "1,234,567");
        assert_eq!(group_thousands("-1234"), "-1,234");
    }

    #[test]
    fn short_timestamp_trims_to_minutes() {
        assert_eq!(
            short_timestamp("2026-10-26T14:35:12+00:00"),
            "2026-10-26 14:35"
        );
        assert_eq!(short_timestamp(""), "");
        assert_eq!(short_timestamp("yesterday"), "yesterday");
    }
}
