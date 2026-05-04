//! Shared output-formatting helpers.
//!
//! Currently houses the token-count humanizer used by both
//! `usage` (per-session aggregation) and `token-usage` (per-message
//! events).  Decimal (1000-based) suffixes — K, M, G — chosen
//! because token counts are reported in decimal everywhere by the
//! upstream providers.

/// Humanize a token count with a decimal suffix.
///
/// - `< 1_000` → `"NNN   "` (3 trailing spaces preserve column width
///   in tabular output)
/// - `< 1_000_000` → `"X.YK"` with one decimal place
/// - `< 1_000_000_000` → `"X.YM"` with one decimal place
/// - `>= 1_000_000_000` → `"X.YG"` with one decimal place
///
/// The format intentionally mirrors what `insight-cli` emits so that
/// downstream visual tooling stays consistent across this user's
/// toolbelt.
pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}G", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{}   ", n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tokens_zero() {
        assert_eq!(format_tokens(0), "0   ");
    }

    #[test]
    fn format_tokens_below_thousand() {
        assert_eq!(format_tokens(1), "1   ");
        assert_eq!(format_tokens(42), "42   ");
        assert_eq!(format_tokens(999), "999   ");
    }

    #[test]
    fn format_tokens_thousands() {
        assert_eq!(format_tokens(1_000), "1.0K");
        assert_eq!(format_tokens(1_500), "1.5K");
        assert_eq!(format_tokens(999_999), "1000.0K");
    }

    #[test]
    fn format_tokens_millions() {
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(1_500_000), "1.5M");
        assert_eq!(format_tokens(999_999_999), "1000.0M");
    }

    #[test]
    fn format_tokens_billions() {
        assert_eq!(format_tokens(1_000_000_000), "1.0G");
        assert_eq!(format_tokens(2_500_000_000), "2.5G");
        assert_eq!(format_tokens(999_999_999_999), "1000.0G");
    }
}
