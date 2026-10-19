//! Shared numeric formatters for usage telemetry.
//!
//! Tokens, money, and durations surface in the footer summary and in the
//! per-turn usage line, so every number is rendered in exactly one place
//! (c-5). Each formatter renders the complete value: a caller that runs out of
//! room drops the whole field instead of truncating digits (c-4).

/// Renders an exact token count.
pub fn format_tokens(tokens: u32) -> String {
    tokens.to_string()
}

/// Renders a monetary amount with four decimal places and an explicit `$`.
///
/// Four places keep sub-cent costs distinguishable even for cheap models,
/// which a two-place rendering would flatten to `$0.00` (c-5).
pub fn format_money(cost: f64) -> String {
    format!("${:.4}", cost)
}

/// Renders a harness-measured duration given in milliseconds.
///
/// Under a minute the value keeps one decimal (`12.5s`); longer operations
/// switch to minutes and seconds (`2m 05s`), so a long batch never shows an
/// unreadable four-digit seconds count.
pub fn format_duration(duration_ms: u64) -> String {
    let seconds = duration_ms / 1000;
    if seconds < 60 {
        format!("{:.1}s", duration_ms as f64 / 1000.0)
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_rendered_exactly() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(1_234_567), "1234567");
    }

    #[test]
    fn money_keeps_four_decimals() {
        assert_eq!(format_money(0.0), "$0.0000");
        assert_eq!(format_money(0.0123), "$0.0123");
        assert_eq!(format_money(12.5), "$12.5000");
    }

    #[test]
    fn durations_switch_to_minutes_after_a_minute() {
        assert_eq!(format_duration(0), "0.0s");
        assert_eq!(format_duration(900), "0.9s");
        assert_eq!(format_duration(12_500), "12.5s");
        assert_eq!(format_duration(59_900), "59.9s");
        assert_eq!(format_duration(60_000), "1m 00s");
        assert_eq!(format_duration(125_000), "2m 05s");
    }
}
