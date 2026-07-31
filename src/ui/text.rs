//! Width-aware text helpers.
//!
//! The sidebar is narrow and full of CJK-capable content (branch names, window
//! names, agent output), so every truncation and pad goes through
//! `unicode-width` rather than counting `char`s. Getting this wrong doesn't just
//! look bad — a row one cell too wide wraps and desyncs every row below it.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Rendered width of a string in terminal cells.
pub fn width(value: &str) -> usize {
    UnicodeWidthStr::width(value)
}

/// Truncate to at most `budget` cells, ending with `…` when anything was cut.
///
/// Never splits a wide glyph in half: a 2-cell character is dropped whole rather
/// than leaving a stray half-cell behind.
pub fn truncate(value: &str, budget: usize) -> String {
    if width(value) <= budget {
        return value.to_owned();
    }
    if budget == 0 {
        return String::new();
    }
    // Reserve one cell for the ellipsis.
    let target = budget - 1;
    let mut out = String::new();
    let mut used = 0;
    for ch in value.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > target {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
}

/// Spaces needed to pad `content_width` out to `total`.
pub fn pad_to(content_width: usize, total: usize) -> String {
    " ".repeat(total.saturating_sub(content_width))
}

/// Widest label [`elapsed_label`] returns, in cells, for any run under 100 days.
/// Callers reserve this much so the timer column never shifts as a run grows, and
/// truncate anyway — so an absurdly long run clips rather than breaking layout.
pub const ELAPSED_MAX_WIDTH: usize = 6;

/// Human elapsed label for a run that started at `started_at`.
///
/// Empty when there is no run, so callers can concatenate unconditionally.
///
/// Seconds are only shown under a minute. Past that the spinner is what tells you
/// the agent is alive, and the number you actually want is "how many minutes" —
/// so dropping the seconds keeps this within [`ELAPSED_MAX_WIDTH`] cells, which
/// matters in a 24-column sidebar. It also means a *blocked* pane, which has no
/// spinner to animate, changes at most once a minute instead of once a second.
pub fn elapsed_label(started_at: Option<u64>, now: u64) -> String {
    let Some(started_at) = started_at else {
        return String::new();
    };
    // Clock skew, or a timestamp written a moment in the future, must not
    // underflow into a nonsense duration.
    let seconds = now.saturating_sub(started_at);
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60),
        // A multi-day "run" means an agent has been blocked since forever, or the
        // daemon lost track of one. Either way that is worth seeing, not clamping.
        _ => format!("{}d{:02}h", seconds / 86_400, (seconds % 86_400) / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_counts_cells_not_chars() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("日本語"), 6);
        assert_eq!(width(""), 0);
    }

    #[test]
    fn truncate_leaves_short_values_alone() {
        assert_eq!(truncate("main", 10), "main");
        assert_eq!(truncate("main", 4), "main");
    }

    #[test]
    fn truncate_marks_the_cut_and_respects_the_budget() {
        assert_eq!(truncate("feature/long-branch", 8), "feature…");
        assert_eq!(width(&truncate("feature/long-branch", 8)), 8);
    }

    #[test]
    fn truncate_never_splits_a_wide_glyph() {
        // Budget 4 leaves 3 cells of content: one 2-cell glyph fits, the second
        // would overflow, so it is dropped whole.
        let out = truncate("日本語", 4);
        assert_eq!(out, "日…");
        assert!(width(&out) <= 4);
    }

    #[test]
    fn truncate_to_zero_yields_nothing() {
        assert_eq!(truncate("anything", 0), "");
    }

    #[test]
    fn pad_to_never_underflows() {
        assert_eq!(pad_to(3, 6), "   ");
        assert_eq!(pad_to(9, 6), "");
    }

    #[test]
    fn elapsed_label_is_absent_without_a_run() {
        assert_eq!(elapsed_label(None, 1000), "");
    }

    #[test]
    fn elapsed_label_steps_up_units_as_a_run_grows() {
        assert_eq!(elapsed_label(Some(1000), 1000), "0s");
        assert_eq!(elapsed_label(Some(1000), 1059), "59s");
        assert_eq!(elapsed_label(Some(1000), 1060), "1m");
        assert_eq!(elapsed_label(Some(1000), 1132), "2m");
        assert_eq!(elapsed_label(Some(1000), 4599), "59m");
        assert_eq!(elapsed_label(Some(0), 3600), "1h00m");
        assert_eq!(elapsed_label(Some(0), 7530), "2h05m");
        assert_eq!(elapsed_label(Some(0), 86_399), "23h59m");
        // Past a day: an agent stuck this long is worth showing plainly.
        assert_eq!(elapsed_label(Some(0), 86_400), "1d00h");
        assert_eq!(elapsed_label(Some(0), 8_000_000), "92d14h");
    }

    #[test]
    fn elapsed_label_never_exceeds_the_reserved_width() {
        // The timer column is reserved at this width; exceeding it would shove the
        // label beside it out of place. Bounded for every run under 100 days.
        for seconds in [
            0, 1, 59, 60, 61, 3599, 3600, 86_399, 86_400, 8_000_000, 8_553_600,
        ] {
            let label = elapsed_label(Some(0), seconds);
            assert!(
                width(&label) <= ELAPSED_MAX_WIDTH,
                "{label:?} is wider than {ELAPSED_MAX_WIDTH} cells"
            );
        }
    }

    #[test]
    fn elapsed_label_changes_at_most_once_a_minute_past_the_first() {
        // What keeps a blocked pane — which has no spinner — from forcing a redraw
        // every single second.
        assert_eq!(elapsed_label(Some(0), 100), elapsed_label(Some(0), 119));
        assert_ne!(elapsed_label(Some(0), 119), elapsed_label(Some(0), 120));
    }

    #[test]
    fn elapsed_label_survives_a_future_timestamp() {
        // Clock skew between the daemon's write and our read must not underflow.
        assert_eq!(elapsed_label(Some(2000), 1000), "0s");
    }
}
