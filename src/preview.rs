//! A live, scaled-down mirror of a window's panes. Popup surface only.
//!
//! The point is to answer "is this the window I meant?" without leaving where you
//! are. To do that it has to *look* like the window: a two-pane split previews as
//! two panes side by side, in the same arrangement, and coloured output previews
//! coloured — you recognise the shape and the palette before you read any text.
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
//! `width × height` grid of [`Cell`]s — one per *display column* — clamps every
//! write to the target pane's rectangle, and expands tabs to spaces on the way in.
//! The output is always exactly `height` [`Line`]s of exactly `width` columns,
//! which is what lets the event loop keep its one-clear-only rule.
//!
//! # Colour, without ever emitting an escape
//!
//! `capture-pane -e` gives us the pane's SGR sequences. They are parsed here, at
//! the edge, into [`Attrs`] carried on each cell, and the escape bytes themselves
//! are consumed and dropped — [`parse_line`] can only ever return printable
//! characters. That matters more than it sounds: a single unhandled escape reaching
//! the terminal would move the real cursor or leave a colour latched, corrupting
//! the frame drawn *around* the preview, and it would do it in a way no width
//! assertion could catch. So the invariant is not "we handle the escapes we know
//! about" but "no escape survives parsing", and
//! [`tests::no_escape_byte_can_survive_into_the_output`] pins it.
//!
//! Attributes are then rendered by [`crate::ui`], which maps an uncoloured cell to
//! the muted tone the whole preview used to be drawn in. A pane that emits no
//! colour therefore previews exactly as it did before.

use crate::tmux::{self, tmux_output};
use crate::ui::text::width as display_width;

/// Tab stop used when flattening captured output.
///
/// tmux reports a tab as a literal `\t`, whose rendered width depends on where it
/// lands. Expanding here — rather than passing it through — is what keeps a
/// composed line the width we think it is.
const TAB_STOP: usize = 8;

/// A colour as the captured stream expressed it.
///
/// Palette entries stay indices instead of being resolved to RGB, so the preview
/// follows the user's terminal theme for exactly the reason the mirrored pane does.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Colour {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// The SGR attributes in force for a cell. All-default means "the pane said
/// nothing", which is how the renderer knows to fall back to the muted tone.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Attrs {
    pub fg: Option<Colour>,
    pub bg: Option<Colour>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    /// Reverse video. Worth carrying rather than dropping: it is how both agents
    /// draw the highlighted row of a selection menu, which is often the one thing
    /// you are looking at the preview to see.
    pub reverse: bool,
    /// Chrome of ours, not anything the capture said: set on the border of the pane
    /// the list has selected, and resolved to the accent colour by
    /// [`crate::ui::preview_style`]. Kept as an intent rather than a colour so the
    /// theme stays on the rendering side, exactly as the muted fallback does.
    ///
    /// [`apply_sgr`] never touches it, so no escape sequence can claim to be the
    /// selection.
    pub selected: bool,
}

/// One character of captured content and the attributes it carried.
///
/// A cell holds a `char`, which may be zero, one or two display columns wide; the
/// grid is indexed in columns and uses [`Cell::continuation`] to reserve the second
/// column of a wide glyph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub attrs: Attrs,
}

/// Grid marker for the second column of a double-width glyph.
///
/// The grid is one entry per *display column*, but a wide character is a single
/// `char` covering two of them. This placeholder holds the second column so nothing
/// else can be written into it, and is dropped when a row is collapsed into spans —
/// leaving the wide glyph to occupy the two columns it was allotted.
const CONTINUATION: char = '\0';

impl Cell {
    fn blank() -> Self {
        Self {
            ch: ' ',
            attrs: Attrs::default(),
        }
    }

    fn continuation() -> Self {
        Self {
            ch: CONTINUATION,
            attrs: Attrs::default(),
        }
    }

    fn is_continuation(self) -> bool {
        self.ch == CONTINUATION
    }
}

/// A run of identically-styled text.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct Span {
    pub text: String,
    pub attrs: Attrs,
}

/// One composed line: exactly the requested number of display columns, as styled
/// runs. Runs rather than cells because that is what a terminal draw call wants,
/// and because it makes the frame fingerprint in [`crate::app`] cheap.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    /// The line's text with its styling dropped.
    #[cfg(test)]
    pub fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }

    #[cfg(test)]
    pub fn width(&self) -> usize {
        self.spans
            .iter()
            .map(|span| display_width(&span.text))
            .sum()
    }
}

/// One pane of the window being previewed, with its geometry as tmux reports it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PanePreview {
    pub pane_id: String,
    pub active: bool,
    pub left: u16,
    pub top: u16,
    pub width: u16,
    pub height: u16,
    /// Captured screen, one entry per line, already parsed.
    pub lines: Vec<Vec<Cell>>,
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
            // the better failure — every write is clamped, and an overlapping
            // border still conveys the shape where an invisible pane conveys nothing.
            let x = scale(left, source_width, area.width).min(area.width.saturating_sub(3));
            let y = scale(top, source_height, area.height).min(area.height.saturating_sub(3));
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
/// `selected` is the pane id the list's cursor is on; its border is marked for the
/// accent colour so moving between panes *of one window* changes something on
/// screen. Without it the mirror only ever marked tmux's active pane, and `j`/`k`
/// inside a split window looked like a key that did nothing.
///
/// A single pane skips the layout entirely and is simply cropped: there is no shape
/// to convey, and a border around the whole preview would only cost two rows of the
/// content you actually wanted to read — and nothing to disambiguate either, since
/// the whole preview is that one pane.
pub fn compose(panes: &[PanePreview], area: Rect, selected: Option<&str>) -> Vec<Line> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }

    let mut grid = vec![vec![Cell::blank(); area.width]; area.height];
    match panes {
        [] => {}
        [only] => fill(&mut grid, area, &only.lines),
        panes => {
            let rects = rects(panes, area);
            let is_selected =
                |pane: &PanePreview| selected.is_some_and(|id| id == pane.pane_id);
            // The selected pane goes last so its accent border survives: `rects`
            // documents that at extreme scale factors two rects can overlap, and the
            // pane drawn later wins those cells.
            for (pane, rect) in panes.iter().zip(&rects).filter(|(pane, _)| !is_selected(pane)) {
                draw(&mut grid, *rect, pane, false);
            }
            for (pane, rect) in panes.iter().zip(&rects).filter(|(pane, _)| is_selected(pane)) {
                draw(&mut grid, *rect, pane, true);
            }
        }
    }
    grid.into_iter().map(collapse).collect()
}

/// Draw one pane into the grid, bordered, clamped to its rect.
///
/// A rect under 2×2 is skipped rather than drawn as a stray character: at that size
/// a border is all there is, and a lone `│` in the middle of a preview reads as
/// corruption.
fn draw(grid: &mut [Vec<Cell>], rect: Rect, pane: &PanePreview, selected: bool) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    // The active pane gets a heavier border, which is usually the fastest way to
    // recognise a familiar layout. Selection is carried by colour instead, so the two
    // can be read independently — and are usually, but not always, the same pane.
    let (horizontal, vertical) = if pane.active {
        ('━', '┃')
    } else {
        ('─', '│')
    };

    // Borders carry no colour *from the capture* on purpose: they are ours, not the
    // pane's, and the renderer draws an unstyled cell in the muted preview tone.
    // Inheriting whatever colour the capture happened to end on would make the frame
    // flicker between colours as the pane scrolled. `selected` is ours to set.
    let border = Attrs {
        selected,
        ..Attrs::default()
    };
    for column in 0..rect.width {
        put(grid, rect.x + column, rect.y, horizontal, border);
        put(
            grid,
            rect.x + column,
            rect.y + rect.height - 1,
            horizontal,
            border,
        );
    }
    for row in 0..rect.height {
        put(grid, rect.x, rect.y + row, vertical, border);
        put(
            grid,
            rect.x + rect.width - 1,
            rect.y + row,
            vertical,
            border,
        );
    }

    fill(
        grid,
        Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(2),
        },
        &pane.lines,
    );
}

/// Write captured lines into `rect`, clamped to it on both axes.
fn fill(grid: &mut [Vec<Cell>], rect: Rect, lines: &[Vec<Cell>]) {
    for row in 0..rect.height {
        let Some(line) = lines.get(row) else {
            break;
        };
        // Advance by display columns, not by cells. A shell prompt with an emoji in
        // it is one char and two columns; counting characters puts the pane's right
        // border one cell short of where the terminal actually draws the text, and
        // the overflow walks into the next pane.
        let mut column = 0;
        for cell in line {
            let cells = display_width(&cell.ch.to_string());
            // Combining marks and other zero-width characters would each consume a
            // grid cell they do not occupy on screen.
            if cells == 0 {
                continue;
            }
            if column + cells > rect.width {
                break;
            }
            put(grid, rect.x + column, rect.y + row, cell.ch, cell.attrs);
            if cells == 2 {
                // Reserve the column the glyph spills into, so nothing is written
                // there and the collapse below emits nothing for it.
                grid_set(grid, rect.x + column + 1, rect.y + row, Cell::continuation());
            }
            column += cells;
        }
    }
}

/// Write one cell, ignoring anything outside the grid.
///
/// This bounds check is the whole anti-overflow guarantee: with it, no capture —
/// however wide or however malformed — can push the composed output past the width
/// the caller asked for.
fn put(grid: &mut [Vec<Cell>], x: usize, y: usize, ch: char, attrs: Attrs) {
    grid_set(grid, x, y, Cell { ch, attrs });
}

fn grid_set(grid: &mut [Vec<Cell>], x: usize, y: usize, cell: Cell) {
    if let Some(row) = grid.get_mut(y)
        && let Some(slot) = row.get_mut(x)
    {
        *slot = cell;
    }
}

/// Collapse a row of cells into runs of equal styling.
fn collapse(row: Vec<Cell>) -> Line {
    let mut spans: Vec<Span> = Vec::new();
    for cell in row {
        if cell.is_continuation() {
            continue;
        }
        match spans.last_mut() {
            Some(span) if span.attrs == cell.attrs => span.text.push(cell.ch),
            _ => spans.push(Span {
                text: cell.ch.to_string(),
                attrs: cell.attrs,
            }),
        }
    }
    Line { spans }
}

// ─── parsing captured output ─────────────────────────────────────────

/// Parse one captured line into cells, expanding tabs and dropping everything the
/// terminal must not see again.
///
/// `attrs` is the SGR state on entry and is left holding the state on exit, so a
/// colour opened on one line and closed on a later one behaves the way it does in
/// the real pane.
pub fn parse_line(line: &str, attrs: &mut Attrs) -> Vec<Cell> {
    let mut cells = Vec::new();
    let mut chars = line.chars().peekable();
    let mut column = 0;

    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => consume_escape(&mut chars, attrs),
            '\t' => {
                let pad = TAB_STOP - (column % TAB_STOP);
                for _ in 0..pad {
                    cells.push(Cell { ch: ' ', attrs: *attrs });
                }
                column += pad;
            }
            // A stray carriage return or bell would move the real terminal's cursor
            // and corrupt the frame drawn around the preview.
            ch if ch.is_control() => {}
            ch => {
                cells.push(Cell { ch, attrs: *attrs });
                column += display_width(&ch.to_string());
            }
        }
    }
    cells
}

/// Consume one escape sequence, applying it if it is an SGR and discarding it
/// otherwise.
///
/// Discarding is the important half. tmux only emits SGR for a captured screen, but
/// "only" is a claim about tmux's current behaviour, and a sequence we failed to
/// recognise must still be swallowed whole rather than partly emitted as text.
fn consume_escape(chars: &mut std::iter::Peekable<std::str::Chars>, attrs: &mut Attrs) {
    match chars.next() {
        // CSI: parameters, then a final byte in 0x40..=0x7E. Only `m` means SGR.
        Some('[') => {
            let mut params = String::new();
            for ch in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&ch) {
                    if ch == 'm' {
                        apply_sgr(&params, attrs);
                    }
                    return;
                }
                params.push(ch);
            }
        }
        // OSC: runs to a BEL or an ST (`ESC \`). Titles arrive this way.
        Some(']') => {
            while let Some(ch) = chars.next() {
                match ch {
                    '\x07' => return,
                    '\x1b' => {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        return;
                    }
                    _ => {}
                }
            }
        }
        // Two-character escapes such as `ESC ( B`: the intermediate is already
        // consumed above, so drop one more byte and stop.
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

/// Apply one SGR parameter list.
///
/// Unknown parameters are ignored rather than treated as a reset: a sequence we do
/// not model should cost us that one attribute, not the whole line's colour.
fn apply_sgr(params: &str, attrs: &mut Attrs) {
    // A bare `ESC[m` is `ESC[0m`.
    if params.is_empty() {
        *attrs = Attrs::default();
        return;
    }

    let codes: Vec<u16> = params
        .split(';')
        .map(|part| part.trim().parse().unwrap_or(0))
        .collect();

    let mut index = 0;
    while index < codes.len() {
        let code = codes[index];
        index += 1;
        match code {
            0 => *attrs = Attrs::default(),
            1 => attrs.bold = true,
            2 => attrs.dim = true,
            3 => attrs.italic = true,
            4 => attrs.underline = true,
            7 => attrs.reverse = true,
            22 => {
                attrs.bold = false;
                attrs.dim = false;
            }
            23 => attrs.italic = false,
            24 => attrs.underline = false,
            27 => attrs.reverse = false,
            30..=37 => attrs.fg = Some(Colour::Indexed((code - 30) as u8)),
            // A selector we cannot read leaves the current colour in place: only an
            // explicit 39/49 or a reset means "back to default".
            38 => {
                if let Some(colour) = extended_colour(&codes, &mut index) {
                    attrs.fg = Some(colour);
                }
            }
            39 => attrs.fg = None,
            40..=47 => attrs.bg = Some(Colour::Indexed((code - 40) as u8)),
            48 => {
                if let Some(colour) = extended_colour(&codes, &mut index) {
                    attrs.bg = Some(colour);
                }
            }
            49 => attrs.bg = None,
            90..=97 => attrs.fg = Some(Colour::Indexed((code - 90 + 8) as u8)),
            100..=107 => attrs.bg = Some(Colour::Indexed((code - 100 + 8) as u8)),
            _ => {}
        }
    }
}

/// Read the argument of a `38`/`48` extended-colour selector, advancing `index`
/// past it. `None` for a truncated or unknown form, which leaves the colour
/// unchanged rather than guessing at one.
fn extended_colour(codes: &[u16], index: &mut usize) -> Option<Colour> {
    let selector = codes.get(*index).copied()?;
    *index += 1;
    match selector {
        5 => {
            let value = codes.get(*index).copied()?;
            *index += 1;
            Some(Colour::Indexed(clamp_u8(value)))
        }
        2 => {
            let red = codes.get(*index).copied()?;
            let green = codes.get(*index + 1).copied()?;
            let blue = codes.get(*index + 2).copied()?;
            *index += 3;
            Some(Colour::Rgb(
                clamp_u8(red),
                clamp_u8(green),
                clamp_u8(blue),
            ))
        }
        _ => None,
    }
}

fn clamp_u8(value: u16) -> u8 {
    value.min(u16::from(u8::MAX)) as u8
}

// ─── tmux side ───────────────────────────────────────────────────────

/// Read the panes of `window_id` and capture each one's visible screen.
///
/// Runs `1 + n` tmux subprocesses, so it belongs on the worker thread. Returns an
/// empty vec when the window is gone, which is a normal race rather than an error:
/// the selection can name a window that closed a moment ago.
pub fn capture_window(window_id: &str) -> Vec<PanePreview> {
    let format =
        "#{pane_id}\t#{pane_active}\t#{pane_left}\t#{pane_top}\t#{pane_width}\t#{pane_height}";
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

/// Capture one pane's visible screen, parsed into styled cells.
///
/// `-p` to stdout, `-N` to keep trailing spaces (so a pane's blank columns stay
/// blank rather than collapsing and shifting content left), `-e` for the SGR
/// sequences that make the preview coloured. The escapes are parsed away
/// immediately — see the module docs on why that is an invariant rather than a
/// feature.
fn capture_pane(pane_id: &str) -> Vec<Vec<Cell>> {
    let Ok(output) = tmux::tmux_output(&["capture-pane", "-peN", "-t", pane_id]) else {
        return Vec::new();
    };
    // Carried across lines: a colour opened on one line and closed on a later one
    // should colour everything between, as it does in the pane itself.
    let mut attrs = Attrs::default();
    output
        .lines()
        .map(|line| parse_line(line, &mut attrs))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse plain text into cells the way a capture would arrive.
    fn cells(text: &str) -> Vec<Cell> {
        parse_line(text, &mut Attrs::default())
    }

    /// Compose with no pane selected, for the tests that are about everything else.
    fn unselected(panes: &[PanePreview], area: Rect) -> Vec<Line> {
        compose(panes, area, None)
    }

    fn pane(id: &str, left: u16, top: u16, width: u16, height: u16, text: &[&str]) -> PanePreview {
        PanePreview {
            pane_id: id.to_owned(),
            active: false,
            left,
            top,
            width,
            height,
            lines: text.iter().map(|line| cells(line)).collect(),
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

    /// The attributes covering `column` of a composed line.
    fn attrs_at(line: &Line, column: usize) -> Attrs {
        let mut seen = 0;
        for span in &line.spans {
            let width = display_width(&span.text);
            if column < seen + width {
                return span.attrs;
            }
            seen += width;
        }
        panic!("column {column} is past the end of {:?}", line.text());
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
            let composed = unselected(&panes, area(width, height));
            assert_eq!(composed.len(), height, "line count at {width}x{height}");
            for line in &composed {
                assert_eq!(
                    line.width(),
                    width,
                    "line width at {width}x{height}: {:?}",
                    line.text()
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
            pane(
                "%1",
                0,
                0,
                40,
                20,
                &["coder in 🌐 engine on 🦀 main is 📦 v0.1.0"],
            ),
            pane("%2", 40, 0, 40, 20, &["plain"]),
        ];
        let composed = unselected(&panes, area(40, 6));
        for line in &composed {
            assert_eq!(line.width(), 40, "{:?}", line.text());
        }
        // The left pane's own right border must still be present and in place.
        assert!(
            composed[1].text().contains('│'),
            "the border was overwritten: {:?}",
            composed[1].text()
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
        for line in unselected(&panes, area(40, 6)) {
            assert_eq!(line.width(), 40, "{:?}", line.text());
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
        let composed = unselected(&panes, area(10, 5));
        for line in &composed {
            assert_eq!(line.width(), 10, "{:?}", line.text());
        }
    }

    #[test]
    fn a_single_pane_is_cropped_without_a_border() {
        // Nothing to convey about shape, and a border would cost two rows of the
        // content you opened the preview to read.
        let composed = unselected(&[pane("%1", 0, 0, 80, 24, &["hello", "world"])], area(10, 4));
        assert_eq!(composed[0].text(), "hello     ");
        assert_eq!(composed[1].text(), "world     ");
        assert_eq!(composed[2].text(), "          ", "short capture pads out");
        assert_eq!(composed.len(), 4);
    }

    #[test]
    fn an_overlong_capture_line_cannot_widen_the_output() {
        let composed = unselected(
            &[pane("%1", 0, 0, 80, 24, &["x".repeat(500).as_str()])],
            area(12, 2),
        );
        assert_eq!(composed[0].text(), "x".repeat(12));
    }

    #[test]
    fn a_zero_sized_area_produces_nothing() {
        let panes = [pane("%1", 0, 0, 80, 24, &["hi"])];
        assert!(unselected(&panes, area(0, 5)).is_empty());
        assert!(unselected(&panes, area(5, 0)).is_empty());
    }

    #[test]
    fn no_panes_composes_to_blank_rows_rather_than_panicking() {
        let composed = unselected(&[], area(6, 2));
        assert_eq!(composed.len(), 2);
        for line in &composed {
            assert_eq!(line.text(), "      ");
        }
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
        let composed = unselected(&panes, area(40, 8));
        let row = composed[1].text();
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
        let composed: String = unselected(&panes, area(40, 8))
            .iter()
            .map(Line::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(composed.contains('┃'), "no heavy border: {composed}");
        assert!(composed.contains('│'), "no light border: {composed}");
    }

    #[test]
    fn a_rect_too_small_to_border_is_skipped_rather_than_half_drawn() {
        // A lone stray border character reads as corruption.
        let panes = vec![pane("%1", 0, 0, 10, 10, &[]), pane("%2", 10, 0, 10, 10, &[])];
        let composed = unselected(&panes, area(3, 1));
        assert_eq!(composed.len(), 1);
        assert_eq!(composed[0].text(), "   ");
    }

    // ─── the selection marker ─────────────────────────────────────────

    /// Two panes side by side, with `text` in each.
    fn split(text: &str) -> Vec<PanePreview> {
        vec![
            pane("%1", 0, 0, 40, 20, &[text]),
            pane("%2", 40, 0, 40, 20, &[text]),
        ]
    }

    /// Which columns of a composed line are marked as the selection.
    fn marked_columns(line: &Line) -> Vec<usize> {
        let mut marked = Vec::new();
        let mut column = 0;
        for span in &line.spans {
            let span_width = display_width(&span.text);
            if span.attrs.selected {
                marked.extend(column..column + span_width);
            }
            column += span_width;
        }
        marked
    }

    #[test]
    fn only_the_selected_panes_border_is_marked_for_the_accent() {
        // Without this, moving between panes *of one window* changed nothing on
        // screen and `j`/`k` looked like keys that did nothing.
        let composed = compose(&split("hi"), area(40, 8), Some("%2"));
        let border_row = &composed[0];
        let marked = marked_columns(border_row);
        assert!(!marked.is_empty(), "nothing was marked");
        assert!(
            marked.iter().all(|column| *column >= 20),
            "the left pane's border was marked too: {marked:?}"
        );

        // Content keeps the capture's own attributes; only chrome is ours to mark.
        for line in &composed {
            for span in &line.spans {
                assert!(
                    !span.attrs.selected || span.text.chars().all(is_border),
                    "a content span was marked: {:?}",
                    span.text
                );
            }
        }
    }

    fn is_border(ch: char) -> bool {
        matches!(ch, '─' | '│' | '━' | '┃')
    }

    #[test]
    fn selection_and_the_active_pane_are_read_independently() {
        // They are usually the same pane and sometimes not; colour says "the cursor
        // is here", the heavy glyph says "tmux is here".
        let mut panes = split("");
        panes[0].active = true;
        let composed = compose(&panes, area(40, 8), Some("%2"));
        let row = &composed[1];
        assert!(row.text().contains('┃'), "the active pane keeps its glyph");
        assert!(
            marked_columns(row).iter().all(|column| *column >= 20),
            "the mark belongs to the selected pane, not the active one"
        );
    }

    #[test]
    fn a_selection_naming_no_visible_pane_marks_nothing() {
        // The selection can name a pane in another window, or one that just closed.
        for selected in [None, Some("%99")] {
            let composed = compose(&split("hi"), area(40, 8), selected);
            for line in &composed {
                assert!(
                    marked_columns(line).is_empty(),
                    "marked something for {selected:?}"
                );
            }
        }
    }

    #[test]
    fn a_single_pane_window_previews_the_same_whether_it_is_selected_or_not() {
        // There is no border to colour and nothing to disambiguate: the whole
        // preview is that one pane.
        let only = [pane("%1", 0, 0, 80, 24, &["hello"])];
        assert_eq!(
            compose(&only, area(10, 3), Some("%1")),
            compose(&only, area(10, 3), None)
        );
    }

    #[test]
    fn the_selected_pane_is_drawn_last_so_an_overlap_cannot_erase_its_border() {
        // `rects` documents that at extreme scale factors two rects can overlap, and
        // the pane drawn later owns the shared cells. Selecting the *first* pane is
        // the case that would otherwise lose its mark.
        let panes = vec![
            pane("%1", 0, 0, 200, 20, &[]),
            pane("%2", 200, 0, 2, 20, &[]),
        ];
        let composed = compose(&panes, area(6, 4), Some("%1"));
        assert!(
            composed.iter().any(|line| !marked_columns(line).is_empty()),
            "the selected pane's border was overdrawn: {:?}",
            composed.iter().map(Line::text).collect::<Vec<_>>()
        );
    }

    // ─── parsing: tabs and control characters ─────────────────────────

    fn text_of(cells: &[Cell]) -> String {
        cells.iter().map(|cell| cell.ch).collect()
    }

    #[test]
    fn tabs_expand_to_the_next_tab_stop() {
        // Passing a tab through is what let a mirrored line be wider than its pane.
        assert_eq!(text_of(&cells("a\tb")), "a       b");
        assert_eq!(text_of(&cells("\tx")), "        x");
        assert_eq!(text_of(&cells("12345678\ty")), "12345678        y");
    }

    #[test]
    fn tab_stops_are_counted_in_display_columns() {
        // A wide glyph fills two columns, so it moves the next tab stop by two.
        assert_eq!(text_of(&cells("🌐\tx")), "🌐      x");
    }

    #[test]
    fn control_characters_are_dropped() {
        // A stray CR or bell would move the real terminal's cursor and corrupt the
        // frame drawn around the preview.
        assert_eq!(text_of(&cells("a\rb\x07c")), "abc");
        assert_eq!(text_of(&cells("plain")), "plain");
    }

    #[test]
    fn parsing_leaves_leading_and_trailing_space_alone() {
        // Blank columns are load-bearing: collapsing them shifts a pane's content
        // left and the preview stops matching the window.
        assert_eq!(text_of(&cells("  indented  ")), "  indented  ");
    }

    // ─── parsing: SGR ─────────────────────────────────────────────────

    #[test]
    fn no_escape_byte_can_survive_into_the_output() {
        // The invariant is not "we handle the escapes we know about" but "no escape
        // survives parsing": one that reached the terminal would latch a colour or
        // move the cursor, corrupting the frame *around* the preview.
        let hostile = concat!(
            "\x1b[1;31mred\x1b[0m ",
            "\x1b[38;2;10;20;30mrgb\x1b[m ",
            "\x1b[2J\x1b[H\x1b[K",              // erase and cursor moves
            "\x1b]0;a title\x07",               // OSC terminated by BEL
            "\x1b]2;another\x1b\\",             // OSC terminated by ST
            "\x1b(Bplain",                      // charset selection
            "\x1b[",                            // truncated CSI at end of line
        );
        let parsed = text_of(&cells(hostile));
        assert!(!parsed.contains('\x1b'), "escape survived: {parsed:?}");
        assert_eq!(parsed, "red rgb plain");

        // And through composition, where it would actually reach the terminal.
        let panes = [pane("%1", 0, 0, 80, 24, &[hostile])];
        for line in unselected(&panes, area(20, 2)) {
            assert!(!line.text().contains('\x1b'));
        }
    }

    #[test]
    fn basic_and_bright_colours_are_kept_as_palette_indices() {
        // Indices rather than RGB, so the preview follows the user's terminal theme
        // exactly as the mirrored pane does.
        let parsed = cells("\x1b[31mr\x1b[92mb\x1b[39md");
        assert_eq!(parsed[0].attrs.fg, Some(Colour::Indexed(1)), "red");
        assert_eq!(parsed[1].attrs.fg, Some(Colour::Indexed(10)), "bright green");
        assert_eq!(parsed[2].attrs.fg, None, "39 restores the default");

        let background = cells("\x1b[44mb\x1b[100mB\x1b[49md");
        assert_eq!(background[0].attrs.bg, Some(Colour::Indexed(4)));
        assert_eq!(background[1].attrs.bg, Some(Colour::Indexed(8)));
        assert_eq!(background[2].attrs.bg, None);
    }

    #[test]
    fn extended_colour_selectors_are_understood() {
        let indexed = cells("\x1b[38;5;208mx");
        assert_eq!(indexed[0].attrs.fg, Some(Colour::Indexed(208)));

        let truecolour = cells("\x1b[48;2;1;2;3mx");
        assert_eq!(truecolour[0].attrs.bg, Some(Colour::Rgb(1, 2, 3)));

        // A selector we do not model leaves the colour alone rather than dropping
        // it: only an explicit 39/49 or a reset means "back to default".
        let unknown = cells("\x1b[31m\x1b[38;9;1mx");
        assert_eq!(unknown[0].attrs.fg, Some(Colour::Indexed(1)));

        // Truncated forms must not panic, consume the following text, or clear the
        // colour already in force.
        assert_eq!(text_of(&cells("\x1b[38;5mx")), "x");
        assert_eq!(text_of(&cells("\x1b[38;2;1;2mx")), "x");
        let truncated = cells("\x1b[34m\x1b[38;5mx");
        assert_eq!(truncated[0].attrs.fg, Some(Colour::Indexed(4)));
    }

    #[test]
    fn a_compound_sgr_sets_every_attribute_it_lists() {
        let parsed = cells("\x1b[1;3;4;7;33mx");
        let attrs = parsed[0].attrs;
        assert!(attrs.bold && attrs.italic && attrs.underline && attrs.reverse);
        assert_eq!(attrs.fg, Some(Colour::Indexed(3)));
    }

    #[test]
    fn reverse_video_is_carried_because_it_is_how_menus_mark_a_selection() {
        let parsed = cells("\x1b[7m❯ 1. Yes\x1b[27m no");
        assert!(attrs_of(&parsed, '❯').reverse);
        assert!(!attrs_of(&parsed, 'n').reverse);
    }

    fn attrs_of(cells: &[Cell], ch: char) -> Attrs {
        cells
            .iter()
            .find(|cell| cell.ch == ch)
            .unwrap_or_else(|| panic!("no {ch:?} in {:?}", text_of(cells)))
            .attrs
    }

    #[test]
    fn individual_attributes_can_be_turned_off_without_dropping_the_colour() {
        let parsed = cells("\x1b[1;31mb\x1b[22mn");
        assert!(parsed[0].attrs.bold);
        assert!(!parsed[1].attrs.bold);
        assert_eq!(
            parsed[1].attrs.fg,
            Some(Colour::Indexed(1)),
            "22 is normal intensity, not a reset"
        );
    }

    #[test]
    fn a_reset_clears_everything_and_a_bare_sgr_is_a_reset() {
        for reset in ["\x1b[0m", "\x1b[m"] {
            let parsed = cells(&format!("\x1b[1;4;31;44mx{reset}y"));
            assert_ne!(parsed[0].attrs, Attrs::default());
            assert_eq!(parsed[1].attrs, Attrs::default(), "after {reset:?}");
        }
    }

    #[test]
    fn an_unknown_parameter_costs_only_itself() {
        // Treating it as a reset would drop the whole line's colour over one
        // attribute we happen not to model.
        let parsed = cells("\x1b[31;53mx");
        assert_eq!(parsed[0].attrs.fg, Some(Colour::Indexed(1)));
    }

    #[test]
    fn sgr_state_carries_from_one_line_to_the_next() {
        // A colour opened on one line and closed on a later one should colour
        // everything between, as it does in the pane itself.
        let mut attrs = Attrs::default();
        let first = parse_line("\x1b[31mred", &mut attrs);
        let second = parse_line("still red\x1b[0m plain", &mut attrs);
        assert_eq!(first[0].attrs.fg, Some(Colour::Indexed(1)));
        assert_eq!(second[0].attrs.fg, Some(Colour::Indexed(1)));
        assert_eq!(attrs_of(&second, 'p').fg, None);
    }

    // ─── styling through composition ──────────────────────────────────

    #[test]
    fn colour_survives_into_the_composed_line() {
        let composed = unselected(
            &[pane("%1", 0, 0, 80, 24, &["\x1b[32mgo\x1b[0mno"])],
            area(6, 1),
        );
        assert_eq!(composed[0].text(), "gono  ");
        assert_eq!(attrs_at(&composed[0], 0).fg, Some(Colour::Indexed(2)));
        assert_eq!(attrs_at(&composed[0], 2).fg, None);
    }

    #[test]
    fn equally_styled_neighbours_collapse_into_one_span() {
        // One span per run rather than per cell: it is what a draw call wants, and
        // it keeps the frame fingerprint in `app` cheap.
        let composed = unselected(
            &[pane("%1", 0, 0, 80, 24, &["\x1b[31maaa\x1b[32mbbb"])],
            area(6, 1),
        );
        assert_eq!(composed[0].spans.len(), 2);
        assert_eq!(composed[0].spans[0].text, "aaa");
        assert_eq!(composed[0].spans[1].text, "bbb");
    }

    #[test]
    fn borders_and_padding_carry_no_colour_from_the_capture() {
        // Inheriting whatever colour the capture ended on would make the frame
        // flicker between colours as the pane scrolled.
        let panes = vec![
            pane("%1", 0, 0, 40, 20, &["\x1b[41;33mhot"]),
            pane("%2", 40, 0, 40, 20, &["cool"]),
        ];
        let composed = unselected(&panes, area(40, 8));
        let border = &composed[0];
        assert!(
            border.spans.iter().all(|span| span.attrs == Attrs::default()),
            "border row is styled: {:?}",
            border.spans
        );
        // The content row keeps the pane's colour, though.
        assert_eq!(attrs_at(&composed[1], 1).bg, Some(Colour::Indexed(1)));
    }

    #[test]
    fn a_wide_glyphs_reserved_column_does_not_split_its_span() {
        // The continuation marker is dropped on collapse, so the glyph keeps the two
        // columns it was allotted and its neighbours stay in one run.
        let composed = unselected(
            &[pane("%1", 0, 0, 80, 24, &["\x1b[36ma🌐b"])],
            area(6, 1),
        );
        assert_eq!(composed[0].width(), 6);
        assert_eq!(composed[0].text(), "a🌐b  ");
        assert_eq!(attrs_at(&composed[0], 1).fg, Some(Colour::Indexed(6)));
    }
}
