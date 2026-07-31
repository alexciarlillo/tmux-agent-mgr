//! A live, scaled-down mirror of a window's panes. Popup surface only.
//!
//! The point is to answer "is this the window I meant?" without leaving where you
//! are. To do that it has to *look* like the window: a two-pane split previews as
//! two panes side by side, in the same arrangement, so you recognise the shape
//! before you read any of the text.
//!
//! # Why this is popup-only
//!
//! Every refresh costs one `capture-pane` subprocess per pane in the window. For
//! the few seconds a popup is open that is nothing; in a sidebar left open all day
//! it is a subprocess storm for a panel too narrow to read anyway.
//!
//! # Why it is written to a fixed grid
//!
//! The plugin this replaces needed a periodic `terminal.clear()` because mirrored
//! pane content desynced ratatui's cell budget — a tab or a wide glyph would make a
//! line wider than the pane it was drawn into, everything below it shifted, and the
//! only cheap fix was to repaint the world twice a second. That periodic clear was
//! the flicker.
//!
//! So this module never emits a line that can overflow: it composes into a
//! `width × height` grid of cells, clamps every write to the target pane's
//! rectangle, and expands tabs to spaces on the way in. The output is always
//! exactly `height` lines of exactly `width` columns, which is what lets the event
//! loop keep its one-clear-only rule.

use crate::tmux::{self, tmux_output};
use crate::ui::text::width as display_width;

/// Tab stop used when flattening captured output.
///
/// tmux reports a tab as a literal `\t`, whose rendered width depends on where it
/// lands. Expanding here — rather than passing it through — is what keeps a
/// composed line the width we think it is.
const TAB_STOP: usize = 8;

/// One pane of the window being previewed, with its geometry as tmux reports it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PanePreview {
    pub pane_id: String,
    pub active: bool,
    pub left: u16,
    pub top: u16,
    pub width: u16,
    pub height: u16,
    /// Captured screen, one entry per line, already flattened.
    pub lines: Vec<String>,
}

/// Where a pane lands in the scaled preview.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

/// Scale each pane's real geometry into `area`.
///
/// Positions are scaled from the window's own bounding box rather than assumed to
/// start at 0,0: a window whose panes all sit at an offset should still fill the
/// preview. Every rect is forced to at least 3×3 where `area` itself allows it, so a
/// very thin pane in a very small preview still shows as *something* — a pane you
/// cannot see reads as a pane that isn't there, which is exactly the wrong answer to
/// "is this the window I meant?".
pub fn rects(panes: &[PanePreview], area: Rect) -> Vec<Rect> {
    if panes.is_empty() || area.width == 0 || area.height == 0 {
        return Vec::new();
    }

    let source_left = panes.iter().map(|pane| pane.left).min().unwrap_or(0);
    let source_top = panes.iter().map(|pane| pane.top).min().unwrap_or(0);
    let source_width = panes
        .iter()
        .map(|pane| pane.left.saturating_add(pane.width))
        .max()
        .unwrap_or(0)
        .saturating_sub(source_left)
        .max(1) as usize;
    let source_height = panes
        .iter()
        .map(|pane| pane.top.saturating_add(pane.height))
        .max()
        .unwrap_or(0)
        .saturating_sub(source_top)
        .max(1) as usize;

    panes
        .iter()
        .map(|pane| {
            let left = pane.left.saturating_sub(source_left) as usize;
            let top = pane.top.saturating_sub(source_top) as usize;
            // Pull the origin back far enough that the 3-cell floor below is
            // actually reachable: clamping only the far edge to `area` would let a
            // pane starting one column from the right edge come out 1 wide again.
            // At extreme scale factors this can overlap the pane before it, which is
            // the better failure — `draw` clamps every write, and an overlapping
            // border still conveys the shape where an invisible pane conveys nothing.
            let x = scale(left, source_width, area.width)
                .min(area.width.saturating_sub(3));
            let y = scale(top, source_height, area.height)
                .min(area.height.saturating_sub(3));
            let right = scale(left + pane.width as usize, source_width, area.width)
                .max(x + 3)
                .min(area.width);
            let bottom = scale(top + pane.height as usize, source_height, area.height)
                .max(y + 3)
                .min(area.height);
            Rect {
                x,
                y,
                width: right.saturating_sub(x),
                height: bottom.saturating_sub(y),
            }
        })
        .collect()
}

fn scale(value: usize, source: usize, target: usize) -> usize {
    if source == 0 || target == 0 {
        return 0;
    }
    value * target / source
}

/// Compose the panes into exactly `area.height` lines of exactly `area.width`
/// columns.
///
/// A single pane skips the layout entirely and is simply cropped: there is no shape
/// to convey, and a border around the whole preview would only cost two rows of the
/// content you actually wanted to read.
pub fn compose(panes: &[PanePreview], area: Rect) -> Vec<String> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    if panes.len() == 1 {
        return crop(&panes[0].lines, area);
    }

    let mut grid = vec![vec![' '; area.width]; area.height];
    for (pane, rect) in panes.iter().zip(rects(panes, area)) {
        draw(&mut grid, rect, pane);
    }
    grid.into_iter()
        .map(|row| row.into_iter().filter(|ch| *ch != CONTINUATION).collect())
        .collect()
}

/// Crop captured lines to the area, padding both axes so the result is a rectangle.
fn crop(lines: &[String], area: Rect) -> Vec<String> {
    (0..area.height)
        .map(|row| match lines.get(row) {
            Some(line) => fit(line, area.width),
            None => " ".repeat(area.width),
        })
        .collect()
}

/// Draw one pane into the grid, bordered, clamped to its rect.
///
/// A rect under 2×2 is skipped rather than drawn as a stray character: at that size
/// a border is all there is, and a lone `│` in the middle of a preview reads as
/// corruption.
fn draw(grid: &mut [Vec<char>], rect: Rect, pane: &PanePreview) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    // The active pane gets a heavier border, which is usually the fastest way to
    // recognise a familiar layout.
    let (horizontal, vertical) = if pane.active {
        ('━', '┃')
    } else {
        ('─', '│')
    };

    for column in 0..rect.width {
        put(grid, rect.x + column, rect.y, horizontal);
        put(grid, rect.x + column, rect.y + rect.height - 1, horizontal);
    }
    for row in 0..rect.height {
        put(grid, rect.x, rect.y + row, vertical);
        put(grid, rect.x + rect.width - 1, rect.y + row, vertical);
    }

    let inner_width = rect.width.saturating_sub(2);
    let inner_height = rect.height.saturating_sub(2);
    for row in 0..inner_height {
        let Some(line) = pane.lines.get(row) else {
            break;
        };
        // Advance by display columns, not by characters. A shell prompt with an
        // emoji in it is one char and two columns; counting chars puts the pane's
        // right border one cell short of where the terminal actually draws the text,
        // and the overflow walks into the next pane.
        let mut column = 0;
        for ch in line.chars() {
            let cells = display_width(&ch.to_string());
            // Combining marks and other zero-width characters would each consume a
            // grid cell they do not occupy on screen.
            if cells == 0 {
                continue;
            }
            if column + cells > inner_width {
                break;
            }
            put(grid, rect.x + 1 + column, rect.y + 1 + row, ch);
            if cells == 2 {
                // Reserve the column the glyph spills into, so nothing is written
                // there and the join below emits nothing for it.
                put(grid, rect.x + 2 + column, rect.y + 1 + row, CONTINUATION);
            }
            column += cells;
        }
    }
}

/// Grid marker for the second column of a double-width glyph.
///
/// The grid is one entry per *display column*, but a wide character is a single
/// `char` covering two of them. This placeholder holds the second column so nothing
/// else can be written into it, and is dropped when rows are joined — leaving the
/// wide glyph to occupy the two columns it was allotted.
const CONTINUATION: char = '\0';

/// Write one cell, ignoring anything outside the grid.
///
/// This bounds check is the whole anti-overflow guarantee: with it, no capture —
/// however wide or however malformed — can push the composed output past the width
/// the caller asked for.
fn put(grid: &mut [Vec<char>], x: usize, y: usize, ch: char) {
    if let Some(row) = grid.get_mut(y)
        && let Some(cell) = row.get_mut(x)
    {
        *cell = ch;
    }
}

/// Truncate or pad `line` to exactly `width` display columns.
fn fit(line: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in line.chars() {
        let ch_width = display_width(&ch.to_string());
        if used + ch_width > width {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    // A wide glyph straddling the edge can leave us a column short.
    out.push_str(&" ".repeat(width - used));
    out
}

/// Flatten one captured line: expand tabs, drop control characters.
///
/// Control characters are removed rather than passed through because a stray
/// carriage return or escape byte would move the real terminal's cursor, corrupting
/// the frame around the preview.
pub fn flatten(line: &str) -> String {
    let mut out = String::new();
    for ch in line.chars() {
        match ch {
            '\t' => {
                let pad = TAB_STOP - (out.chars().count() % TAB_STOP);
                out.push_str(&" ".repeat(pad));
            }
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out
}

// ─── tmux side ───────────────────────────────────────────────────────

/// Read the panes of `window_id` and capture each one's visible screen.
///
/// Runs `1 + n` tmux subprocesses, so it belongs on the worker thread. Returns an
/// empty vec when the window is gone, which is a normal race rather than an error:
/// the selection can name a window that closed a moment ago.
pub fn capture_window(window_id: &str) -> Vec<PanePreview> {
    let format = "#{pane_id}\t#{pane_active}\t#{pane_left}\t#{pane_top}\t#{pane_width}\t#{pane_height}";
    let Ok(output) = tmux_output(&["list-panes", "-t", window_id, "-F", format]) else {
        return Vec::new();
    };

    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() < 6 {
                return None;
            }
            let pane_id = fields[0].to_owned();
            let lines = capture_pane(&pane_id);
            Some(PanePreview {
                active: fields[1] == "1",
                left: fields[2].parse().unwrap_or(0),
                top: fields[3].parse().unwrap_or(0),
                width: fields[4].parse().unwrap_or(0),
                height: fields[5].parse().unwrap_or(0),
                pane_id,
                lines,
            })
        })
        .collect()
}

/// Capture one pane's visible screen as flattened plain text.
///
/// `-p` to stdout, `-N` to keep trailing spaces (so a pane's blank columns stay
/// blank rather than collapsing and shifting content left). Deliberately *not*
/// `-e`: escape sequences would have to be parsed before they could be composed
/// safely, and an unparsed one reaching the terminal would corrupt the frame.
fn capture_pane(pane_id: &str) -> Vec<String> {
    tmux::tmux_output(&["capture-pane", "-pN", "-t", pane_id])
        .map(|output| output.lines().map(flatten).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, left: u16, top: u16, width: u16, height: u16, text: &[&str]) -> PanePreview {
        PanePreview {
            pane_id: id.to_owned(),
            active: false,
            left,
            top,
            width,
            height,
            lines: text.iter().map(|line| (*line).to_owned()).collect(),
        }
    }

    fn area(width: usize, height: usize) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    // ─── the rectangle contract ───────────────────────────────────────

    #[test]
    fn output_is_always_exactly_the_requested_rectangle() {
        // This is the anti-flicker contract: a line one cell too wide is what forced
        // the old plugin into a periodic full clear.
        let panes = vec![
            pane("%1", 0, 0, 40, 20, &["left ".repeat(80).as_str()]),
            pane("%2", 41, 0, 40, 20, &["right"]),
        ];
        for (width, height) in [(20, 6), (40, 12), (80, 24), (3, 3)] {
            let composed = compose(&panes, area(width, height));
            assert_eq!(composed.len(), height, "line count at {width}x{height}");
            for line in &composed {
                assert_eq!(
                    display_width(line),
                    width,
                    "line width at {width}x{height}: {line:?}"
                );
            }
        }
    }

    #[test]
    fn wide_glyphs_cannot_push_a_pane_past_its_border() {
        // Found live, not by the ASCII test above: a shell prompt with an emoji is
        // one char and two columns. Counting characters put the right border a cell
        // short of where the terminal drew the text and the overflow walked into the
        // neighbouring pane.
        let panes = vec![
            pane("%1", 0, 0, 40, 20, &["coder in 🌐 engine on 🦀 main is 📦 v0.1.0"]),
            pane("%2", 40, 0, 40, 20, &["plain"]),
        ];
        let composed = compose(&panes, area(40, 6));
        for line in &composed {
            assert_eq!(display_width(line), 40, "{line:?}");
        }
        // The left pane's own right border must still be present and in place.
        let row: Vec<char> = composed[1].chars().collect();
        assert!(
            row.contains(&'│'),
            "the border was overwritten: {:?}",
            composed[1]
        );
    }

    #[test]
    fn zero_width_characters_do_not_consume_a_column() {
        // A combining mark occupies no cell on screen; giving it one would shift the
        // rest of the line left and desync the border.
        let panes = vec![
            pane("%1", 0, 0, 40, 20, &["a\u{300}b\u{301}c"]),
            pane("%2", 40, 0, 40, 20, &["x"]),
        ];
        for line in compose(&panes, area(40, 6)) {
            assert_eq!(display_width(&line), 40, "{line:?}");
        }
    }

    #[test]
    fn a_wide_glyph_that_would_straddle_the_edge_is_dropped() {
        // Half of a double-width glyph is not a thing the terminal can draw.
        let panes = vec![
            pane("%1", 0, 0, 40, 20, &["ab🌐"]),
            pane("%2", 40, 0, 40, 20, &["y"]),
        ];
        // 5 wide total leaves 3 inner columns: "ab" fits, the emoji needs 2 and only
        // 1 remains.
        let composed = compose(&panes, area(10, 5));
        for line in &composed {
            assert_eq!(display_width(line), 10, "{line:?}");
        }
    }

    #[test]
    fn a_single_pane_is_cropped_without_a_border() {
        // Nothing to convey about shape, and a border would cost two rows of the
        // content you opened the preview to read.
        let composed = compose(&[pane("%1", 0, 0, 80, 24, &["hello", "world"])], area(10, 4));
        assert_eq!(composed[0], "hello     ");
        assert_eq!(composed[1], "world     ");
        assert_eq!(composed[2], "          ", "short capture pads out");
        assert_eq!(composed.len(), 4);
    }

    #[test]
    fn an_overlong_capture_line_cannot_widen_the_output() {
        let composed = compose(&[pane("%1", 0, 0, 80, 24, &["x".repeat(500).as_str()])], area(12, 2));
        assert_eq!(composed[0], "x".repeat(12));
    }

    #[test]
    fn a_zero_sized_area_produces_nothing() {
        let panes = [pane("%1", 0, 0, 80, 24, &["hi"])];
        assert!(compose(&panes, area(0, 5)).is_empty());
        assert!(compose(&panes, area(5, 0)).is_empty());
    }

    #[test]
    fn no_panes_composes_to_blank_rows_rather_than_panicking() {
        let composed = compose(&[], area(6, 2));
        assert_eq!(composed, vec!["      ", "      "]);
    }

    // ─── geometry ─────────────────────────────────────────────────────

    #[test]
    fn a_side_by_side_split_previews_side_by_side() {
        let panes = vec![
            pane("%1", 0, 0, 40, 20, &[]),
            pane("%2", 40, 0, 40, 20, &[]),
        ];
        let rects = rects(&panes, area(40, 10));
        assert_eq!(rects[0].x, 0);
        assert_eq!(rects[1].x, 20, "the second pane starts at the halfway mark");
        assert_eq!(rects[0].y, rects[1].y, "and they share a row");
    }

    #[test]
    fn a_stacked_split_previews_stacked() {
        let panes = vec![
            pane("%1", 0, 0, 80, 12, &[]),
            pane("%2", 0, 12, 80, 12, &[]),
        ];
        let rects = rects(&panes, area(40, 12));
        assert_eq!(rects[0].x, rects[1].x, "same column");
        assert_eq!(rects[0].y, 0);
        assert_eq!(rects[1].y, 6, "the second pane starts halfway down");
    }

    #[test]
    fn geometry_is_scaled_from_the_windows_own_bounding_box() {
        // A window whose panes all sit at an offset should still fill the preview
        // rather than leaving a margin proportional to the offset.
        let panes = vec![
            pane("%1", 100, 50, 40, 20, &[]),
            pane("%2", 140, 50, 40, 20, &[]),
        ];
        let rects = rects(&panes, area(40, 10));
        assert_eq!(rects[0].x, 0, "the leftmost pane starts at the left edge");
        assert_eq!(rects[1].x, 20);
    }

    #[test]
    fn a_thin_pane_still_gets_a_visible_rectangle() {
        // A pane scaled to nothing reads as a pane that is not there, which is the
        // wrong answer to "is this the window I meant?".
        let panes = vec![
            pane("%1", 0, 0, 200, 20, &[]),
            pane("%2", 200, 0, 2, 20, &[]),
        ];
        let rects = rects(&panes, area(20, 6));
        assert!(rects[1].width >= 3, "got {:?}", rects[1]);
        assert!(rects[1].height >= 3, "got {:?}", rects[1]);
    }

    #[test]
    fn rects_of_nothing_or_of_no_space_are_empty() {
        assert!(rects(&[], area(10, 10)).is_empty());
        assert!(rects(&[pane("%1", 0, 0, 10, 10, &[])], area(0, 10)).is_empty());
    }

    // ─── composition detail ───────────────────────────────────────────

    #[test]
    fn each_panes_text_stays_inside_its_own_rectangle() {
        // Bleeding across the divider is the specific corruption that made the old
        // preview need a periodic clear.
        let panes = vec![
            pane("%1", 0, 0, 40, 20, &["L".repeat(100).as_str()]),
            pane("%2", 40, 0, 40, 20, &["R".repeat(100).as_str()]),
        ];
        let composed = compose(&panes, area(40, 8));
        let row = &composed[1];
        // By chars, not bytes — the box-drawing characters are 3 bytes each.
        let left: String = row.chars().take(20).collect();
        let right: String = row.chars().skip(20).collect();
        assert!(!left.contains('R'), "left half held right's text: {row:?}");
        assert!(!right.contains('L'), "right half held left's text: {row:?}");
    }

    #[test]
    fn the_active_pane_is_drawn_with_a_heavier_border() {
        let mut panes = vec![
            pane("%1", 0, 0, 40, 20, &[]),
            pane("%2", 40, 0, 40, 20, &[]),
        ];
        panes[1].active = true;
        let composed = compose(&panes, area(40, 8)).join("\n");
        assert!(composed.contains('┃'), "no heavy border: {composed}");
        assert!(composed.contains('│'), "no light border: {composed}");
    }

    #[test]
    fn a_rect_too_small_to_border_is_skipped_rather_than_half_drawn() {
        // A lone stray border character reads as corruption.
        let panes = vec![pane("%1", 0, 0, 10, 10, &[]), pane("%2", 10, 0, 10, 10, &[])];
        let composed = compose(&panes, area(3, 1));
        assert_eq!(composed, vec!["   "]);
    }

    // ─── flattening ───────────────────────────────────────────────────

    #[test]
    fn tabs_expand_to_the_next_tab_stop() {
        // Passing a tab through is what let a mirrored line be wider than its pane.
        assert_eq!(flatten("a\tb"), "a       b");
        assert_eq!(flatten("\tx"), "        x");
        assert_eq!(flatten("12345678\ty"), "12345678        y");
    }

    #[test]
    fn control_characters_are_dropped() {
        // A stray CR or escape byte would move the real terminal's cursor and
        // corrupt the frame drawn around the preview.
        assert_eq!(flatten("a\rb\x07c"), "abc");
        assert_eq!(flatten("plain"), "plain");
    }

    #[test]
    fn flattening_leaves_leading_and_trailing_space_alone() {
        // Blank columns are load-bearing: collapsing them shifts a pane's content
        // left and the preview stops matching the window.
        assert_eq!(flatten("  indented  "), "  indented  ");
    }
}
