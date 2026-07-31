//! Drawing the sidebar: a header, the scrollable list, and a footer.

pub mod rows;
pub mod text;
pub mod theme;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, StatusFilter};
use crate::model::AgentState;
use text::{pad_to, truncate, width};

/// Where the TUI is being drawn.
///
/// The same list code serves both, but the two surfaces have genuinely different
/// jobs and the differences are worth naming rather than sprinkling `if` on a
/// bool. A sidebar is narrow, lives for hours beside your work, and is the reason
/// the redraw policy in [`crate::app`] exists. A popup is wide, covers the screen,
/// and exists to answer one question and get out of the way.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum Surface {
    #[default]
    Sidebar,
    Popup,
}

impl Surface {
    /// Whether jumping to a pane should also close the TUI.
    ///
    /// A popup is a transient chooser: leaving it open on top of the pane you just
    /// asked to see would hide the thing you navigated to. A sidebar is the
    /// opposite — you jump around *with* it open, which is the whole point of it
    /// being persistent.
    pub fn dismisses_on_activate(self) -> bool {
        self == Self::Popup
    }
}

/// Lines reserved at the top and bottom of the pane. Both are fixed so the list
/// viewport height is stable and scrolling doesn't jump when a count changes.
pub const HEADER_HEIGHT: u16 = 1;
pub const FOOTER_HEIGHT: u16 = 1;

/// Visible list height for a pane of `total_height` rows.
pub fn list_height(total_height: u16) -> u16 {
    total_height.saturating_sub(HEADER_HEIGHT + FOOTER_HEIGHT)
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.height == 0 || area.width == 0 {
        return;
    }

    let header = Rect { height: HEADER_HEIGHT.min(area.height), ..area };
    frame.render_widget(Paragraph::new(header_line(app, area.width as usize)), header);

    let list_rows = list_height(area.height);
    if list_rows > 0 {
        let list = Rect {
            y: area.y + HEADER_HEIGHT,
            height: list_rows,
            ..area
        };
        if app.list.is_empty() {
            frame.render_widget(Paragraph::new(empty_state(app, area.width as usize)), list);
        } else {
            let visible: Vec<Line<'static>> = app
                .list
                .lines
                .iter()
                .skip(app.scroll)
                .take(list_rows as usize)
                .cloned()
                .collect();
            frame.render_widget(Paragraph::new(visible), list);
        }
    }

    if area.height > HEADER_HEIGHT {
        let footer = Rect {
            y: area.y + area.height - FOOTER_HEIGHT,
            height: FOOTER_HEIGHT,
            ..area
        };
        frame.render_widget(Paragraph::new(footer_line(app, area.width as usize)), footer);
    }
}

/// `agent-mgr        2● 1◉` — the name, then a count per attention-worthy state.
///
/// Counts are omitted when zero so the common case reads as calm rather than as a
/// row of zeroes.
fn header_line(app: &App, total_width: usize) -> Line<'static> {
    let theme = &app.theme;
    let mut right: Vec<Span<'static>> = Vec::new();
    let mut right_width = 0;

    for (count, glyph, color) in [
        (app.counts.blocked, "◉", theme.blocked),
        (app.counts.working, "●", theme.working),
        (app.counts.done, "●", theme.done),
        (app.counts.error, "✕", theme.error),
    ] {
        if count == 0 {
            continue;
        }
        let label = format!("{count}{glyph} ");
        right_width += width(&label);
        right.push(Span::styled(label, Style::default().fg(color)));
    }

    let title = truncate("agent-mgr", total_width.saturating_sub(right_width + 1));
    let mut spans = vec![Span::styled(
        title.clone(),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::raw(pad_to(width(&title) + right_width, total_width)));
    spans.extend(right);
    Line::from(spans)
}

/// What to say when the list is empty.
///
/// An empty list under a filter is a very different situation from an empty list
/// with no filter, and saying which one it is saves the user hunting for panes
/// that were never missing.
fn empty_state(app: &App, total_width: usize) -> Line<'static> {
    let text = if app.filter == StatusFilter::All {
        "no panes".to_owned()
    } else {
        format!("nothing {} — Tab to change", filter_label(app.filter))
    };
    Line::from(Span::styled(
        truncate(&text, total_width),
        Style::default().fg(app.theme.muted),
    ))
}

fn filter_label(filter: StatusFilter) -> &'static str {
    match filter {
        StatusFilter::All => "all",
        StatusFilter::Working => "working",
        StatusFilter::Blocked => "blocked",
        StatusFilter::Done => "done",
    }
}

/// The footer carries the active filter and, when it is hiding panes, how many.
/// A filter you forgot you set is the classic "why is my agent missing" bug.
fn footer_line(app: &App, total_width: usize) -> Line<'static> {
    let theme = &app.theme;
    // A half-typed count has to be visible: without an echo you cannot tell a
    // pending `1` from a keystroke that was dropped, and you find out only by
    // pressing `j` and going somewhere unexpected.
    if let Some(count) = app.pending_count {
        let text = truncate(&format!("{count}"), total_width);
        let content_width = width(&text);
        return Line::from(vec![
            Span::styled(
                text,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(pad_to(content_width, total_width)),
        ]);
    }

    let label = filter_label(app.filter).to_owned();
    let hidden = app.hidden_count();
    let text = if hidden > 0 {
        format!("{label} · {hidden} hidden")
    } else {
        label
    };
    let text = truncate(&text, total_width);
    let content_width = width(&text);
    let color = if app.filter == StatusFilter::All {
        theme.muted
    } else {
        theme.accent
    };
    Line::from(vec![
        Span::styled(text, Style::default().fg(color)),
        Span::raw(pad_to(content_width, total_width)),
    ])
}

/// Per-state tallies for the header.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Counts {
    pub working: usize,
    pub blocked: usize,
    pub done: usize,
    pub error: usize,
}

impl Counts {
    /// Tally the *unfiltered* tree, so the header keeps telling you an agent
    /// needs attention even while a filter hides it.
    pub fn tally(sessions: &[crate::model::SessionGroup]) -> Self {
        let mut counts = Self::default();
        for pane in sessions
            .iter()
            .flat_map(|session| &session.windows)
            .flat_map(|window| &window.panes)
        {
            let status = &pane.status;
            if status.agent.is_none() {
                continue;
            }
            match status.state {
                AgentState::Working => counts.working += 1,
                AgentState::Blocked => counts.blocked += 1,
                AgentState::Error => counts.error += 1,
                AgentState::Idle if !status.seen => counts.done += 1,
                AgentState::Idle | AgentState::Unknown => {}
            }
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AgentKind, AgentStatus, PaneInfo, SessionGroup, StatusSource, WindowInfo,
    };

    fn pane(state: AgentState, seen: bool, agent: Option<AgentKind>) -> PaneInfo {
        PaneInfo {
            pane_id: "%1".to_owned(),
            window_id: "@1".to_owned(),
            pane_index: "0".to_owned(),
            pane_active: false,
            current_command: "zsh".to_owned(),
            current_path: "/tmp".to_owned(),
            title: String::new(),
            pane_pid: None,
            status: AgentStatus {
                agent,
                state,
                source: StatusSource::Passive,
                seen,
                ..AgentStatus::default()
            },
            branch: String::new(),
            worktree: String::new(),
        }
    }

    fn tree(panes: Vec<PaneInfo>) -> Vec<SessionGroup> {
        vec![SessionGroup {
            session_name: "work".to_owned(),
            session_attached: true,
            windows: vec![WindowInfo {
                window_id: "@1".to_owned(),
                window_index: "1".to_owned(),
                window_name: "w".to_owned(),
                window_active: true,
                panes,
            }],
        }]
    }

    #[test]
    fn list_height_reserves_the_header_and_footer_without_underflowing() {
        assert_eq!(list_height(40), 38);
        assert_eq!(list_height(2), 0);
        assert_eq!(list_height(1), 0);
        assert_eq!(list_height(0), 0);
    }

    #[test]
    fn counts_only_tally_panes_that_actually_host_an_agent() {
        let counts = Counts::tally(&tree(vec![
            pane(AgentState::Working, true, Some(AgentKind::Claude)),
            pane(AgentState::Blocked, true, Some(AgentKind::Codex)),
            pane(AgentState::Idle, false, Some(AgentKind::Claude)),
            pane(AgentState::Idle, true, Some(AgentKind::Claude)),
            pane(AgentState::Error, true, Some(AgentKind::Claude)),
            // A plain shell is not an agent, whatever state it carries.
            pane(AgentState::Working, true, None),
        ]));
        assert_eq!(
            counts,
            Counts {
                working: 1,
                blocked: 1,
                done: 1,
                error: 1
            }
        );
    }

    #[test]
    fn counts_of_a_quiet_workspace_are_all_zero() {
        assert_eq!(
            Counts::tally(&tree(vec![pane(
                AgentState::Idle,
                true,
                Some(AgentKind::Claude)
            )])),
            Counts::default()
        );
        assert_eq!(Counts::tally(&[]), Counts::default());
    }
}
