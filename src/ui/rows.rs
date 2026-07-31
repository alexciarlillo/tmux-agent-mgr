//! Turning the session tree into lines.
//!
//! This module is pure: tree in, lines out, no I/O and no tmux. That is what
//! makes the whole render testable, and it is also what lets the event loop hash
//! the result to decide whether anything actually changed (see [`crate::app`]).
//!
//! A pane occupies a *block* of one or more lines — a status line, then only the
//! context lines that have something to say. Selection and navigation operate on
//! blocks, never on individual lines, so moving down always lands on the next
//! pane rather than on that pane's branch row.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::model::{AgentState, AgentStatus, PaneInfo, SessionGroup, StatusSource, WindowInfo};
use crate::ui::text::{ELAPSED_MAX_WIDTH, elapsed_label, pad_to, truncate, width};
use crate::ui::theme::{SPINNER, Theme, state_icon};

/// Everything needed to jump to a pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneTarget {
    pub session_name: String,
    pub window_id: String,
    pub pane_id: String,
}

/// A pane's contiguous run of lines, in navigation order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Block {
    pub target: PaneTarget,
    pub line_start: usize,
    pub line_count: usize,
}

/// The rendered list, plus the two indexes the event loop needs.
pub struct RenderedList {
    pub lines: Vec<Line<'static>>,
    /// Plain text of each line, used as the change fingerprint. Cheaper and more
    /// honest than comparing state: if the visible text is identical there is
    /// nothing to repaint.
    pub plain: Vec<String>,
    pub blocks: Vec<Block>,
}

impl RenderedList {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Line index where `block` starts, for scrolling.
    pub fn block_line(&self, block: usize) -> usize {
        self.blocks.get(block).map_or(0, |block| block.line_start)
    }

    pub fn block_height(&self, block: usize) -> usize {
        self.blocks.get(block).map_or(1, |block| block.line_count)
    }

    /// Which block owns a given line, for mouse clicks.
    pub fn block_at_line(&self, line: usize) -> Option<usize> {
        self.blocks.iter().position(|block| {
            line >= block.line_start && line < block.line_start + block.line_count
        })
    }
}

/// A partially built list. Threading a builder through keeps every push site
/// honest about recording line indexes.
struct Builder<'a> {
    theme: &'a Theme,
    /// Total columns available, including the one-cell selection marker gutter.
    total_width: usize,
    spinner: usize,
    now: u64,
    lines: Vec<Line<'static>>,
    plain: Vec<String>,
    blocks: Vec<Block>,
}

impl Builder<'_> {
    /// Columns available after the marker gutter and its trailing space.
    fn inner(&self, indent: usize) -> usize {
        self.total_width.saturating_sub(2 + indent)
    }

    /// Push one line: a marker cell, an indent, the content, and padding out to
    /// the full width so a selection background covers the whole row.
    fn push(
        &mut self,
        marker: Option<Span<'static>>,
        indent: usize,
        content: Vec<Span<'static>>,
        content_width: usize,
        bg: Option<Color>,
    ) {
        let base = match bg {
            Some(color) => Style::default().bg(color),
            None => Style::default(),
        };
        let mut spans = Vec::with_capacity(content.len() + 4);
        spans.push(marker.unwrap_or_else(|| Span::styled(" ", base)));
        spans.push(Span::styled(" ".repeat(1 + indent), base));
        spans.extend(content);
        spans.push(Span::styled(
            pad_to(content_width, self.inner(indent)),
            base,
        ));

        self.plain
            .push(spans.iter().map(|span| span.content.as_ref()).collect());
        self.lines.push(Line::from(spans));
    }

    /// Push a simple single-styled line, e.g. a context row.
    fn push_text(&mut self, indent: usize, text: String, color: Color, bg: Option<Color>) {
        let text = truncate(&text, self.inner(indent));
        let content_width = width(&text);
        let mut style = Style::default().fg(color);
        if let Some(color) = bg {
            style = style.bg(color);
        }
        self.push(
            None,
            indent,
            vec![Span::styled(text, style)],
            content_width,
            bg,
        );
    }
}

/// Render the session tree.
///
/// `selected` indexes [`RenderedList::blocks`]; out-of-range simply renders
/// nothing highlighted, so a caller mid-refresh can't panic on a stale index.
pub fn build(
    sessions: &[SessionGroup],
    selected: usize,
    total_width: usize,
    spinner: usize,
    now: u64,
    theme: &Theme,
) -> RenderedList {
    let mut builder = Builder {
        theme,
        total_width: total_width.max(8),
        spinner,
        now,
        lines: Vec::new(),
        plain: Vec::new(),
        blocks: Vec::new(),
    };

    for session in sessions {
        session_header(&mut builder, session);
        for window in &session.windows {
            window_header(&mut builder, window);
            let multi_pane = window.panes.len() > 1;
            for pane in &window.panes {
                let block = builder.blocks.len();
                pane_block(
                    &mut builder,
                    session,
                    window,
                    pane,
                    multi_pane,
                    block == selected,
                );
            }
        }
    }

    RenderedList {
        lines: builder.lines,
        plain: builder.plain,
        blocks: builder.blocks,
    }
}

fn session_header(builder: &mut Builder, session: &SessionGroup) {
    let theme = builder.theme;
    // A detached session is still navigable, just not currently on anyone's
    // screen; dimming it says so without hiding it.
    let color = if session.session_attached {
        theme.session
    } else {
        theme.muted
    };
    let name = truncate(&session.session_name, builder.inner(0));
    let content_width = width(&name);
    builder.push(
        None,
        0,
        vec![Span::styled(
            name,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )],
        content_width,
        None,
    );
}

fn window_header(builder: &mut Builder, window: &WindowInfo) {
    let theme = builder.theme;
    let label = format!("{} {}", window.window_index, window.window_name);
    let label = truncate(&label, builder.inner(1));
    let content_width = width(&label);
    let mut style = Style::default().fg(theme.window);
    if window.window_active {
        style = style.fg(theme.text);
    }
    builder.push(None, 1, vec![Span::styled(label, style)], content_width, None);
}

fn pane_block(
    builder: &mut Builder,
    session: &SessionGroup,
    window: &WindowInfo,
    pane: &PaneInfo,
    show_index: bool,
    selected: bool,
) {
    let line_start = builder.lines.len();
    let bg = selected.then_some(builder.theme.selection_bg);
    // The `┃` marker means "tmux's cursor is here", which is only true when the
    // window is also the current one — otherwise every window would show one.
    let active = pane.pane_active && window.window_active;

    status_line(builder, pane, show_index, active, bg);
    context_lines(builder, pane, bg);

    builder.blocks.push(Block {
        target: PaneTarget {
            session_name: session.session_name.clone(),
            window_id: window.window_id.clone(),
            pane_id: pane.pane_id.clone(),
        },
        line_start,
        line_count: builder.lines.len() - line_start,
    });
}

/// `┃● claude plan          1m12s`
///
/// The elapsed label is right-aligned and reserved for first, so a long session
/// name is what gets truncated rather than pushing the timer off screen.
fn status_line(
    builder: &mut Builder,
    pane: &PaneInfo,
    show_index: bool,
    active: bool,
    bg: Option<Color>,
) {
    let theme = builder.theme;
    let status = &pane.status;
    let indent = 2;
    let inner = builder.inner(indent);

    let (icon, icon_color) = icon_for(status, builder.spinner, theme);
    let badge = status.permission_mode.badge();
    let elapsed = elapsed_label(status.run_started_at, builder.now);

    let label_raw = pane_label(pane, show_index);
    let fixed = width(icon) + 1 + if badge.is_empty() { 0 } else { width(badge) + 1 };
    // Reserve the timer's *maximum* width, not its current width, so the label
    // budget stays fixed for the life of a run. Reserving the actual width would
    // re-truncate the label every time the timer rolled over from `59s` to `1m`.
    let reserved = if elapsed.is_empty() {
        0
    } else {
        ELAPSED_MAX_WIDTH + 1
    };
    let label = truncate(&label_raw, inner.saturating_sub(fixed + reserved));

    let left_width = fixed + width(&label);
    // Re-clamp: on a very narrow sidebar the label may already have eaten the
    // room the timer wanted.
    let elapsed = truncate(&elapsed, inner.saturating_sub(left_width));

    let with_bg = |style: Style| match bg {
        Some(color) => style.bg(color),
        None => style,
    };

    let mut spans = vec![
        Span::styled(icon.to_owned(), with_bg(Style::default().fg(icon_color))),
        Span::styled(
            format!(" {label}"),
            with_bg(Style::default().fg(theme.agent_color(status.agent))),
        ),
    ];
    if !badge.is_empty() {
        spans.push(Span::styled(
            format!(" {badge}"),
            with_bg(Style::default().fg(badge_color(status, theme))),
        ));
    }

    let gap = inner.saturating_sub(left_width + width(&elapsed));
    spans.push(Span::styled(" ".repeat(gap), with_bg(Style::default())));
    let elapsed_color = if status.state.is_active() {
        theme.text
    } else {
        theme.muted
    };
    let elapsed_width = width(&elapsed);
    spans.push(Span::styled(
        elapsed,
        with_bg(Style::default().fg(elapsed_color)),
    ));

    let marker = Span::styled(
        if active { "┃" } else { " " },
        with_bg(Style::default().fg(theme.accent)),
    );
    builder.push(
        Some(marker),
        indent,
        spans,
        left_width + gap + elapsed_width,
        bg,
    );
}

/// The context rows beneath a pane. Each is conditional: a pane with nothing to
/// report stays one line tall, which is what keeps a long list readable.
fn context_lines(builder: &mut Builder, pane: &PaneInfo, bg: Option<Color>) {
    let theme = builder.theme;
    let status = &pane.status;
    let indent = 4;

    // The overwhelmingly common case is a pane with nothing extra to say.
    if pane.branch.is_empty() && pane.worktree.is_empty() && !status.has_hook_detail() {
        return;
    }

    if !pane.branch.is_empty() || !pane.worktree.is_empty() {
        let mut text = pane.branch.clone();
        if !pane.worktree.is_empty() {
            // `~` reads as "checked out over there", distinct from the branch.
            text = if text.is_empty() {
                format!("~{}", pane.worktree)
            } else {
                format!("{text} ~{}", pane.worktree)
            };
        }
        builder.push_text(indent, text, theme.branch, bg);
    }

    // Only meaningful while actually blocked: a leftover reason on a running pane
    // would read as a live prompt that isn't there.
    if status.state == AgentState::Blocked && !status.wait_reason.is_empty() {
        let reason = status.wait_reason.replace('_', " ");
        builder.push_text(indent, format!("▸ {reason}"), theme.wait_reason, bg);
    }

    if !status.subagents.is_empty() {
        builder.push_text(indent, subagent_summary(&status.subagents), theme.subagent, bg);
    }

    if let Some(progress) = status.task_progress {
        builder.push_text(
            indent,
            format!("▸ tasks {}/{}", progress.done, progress.total),
            theme.task_progress,
            bg,
        );
    }

    if let Some(command) = status.background_cmd.as_deref() {
        builder.push_text(indent, format!("▸ bg {command}"), theme.muted, bg);
    }
}

/// Collapse `Type:id` subagent entries into `▸ Explore ×2, Plan`.
///
/// Counting by type rather than listing ids keeps a fan-out of eight explorers
/// from flooding a narrow sidebar.
fn subagent_summary(subagents: &[String]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for entry in subagents {
        let kind = entry.split(':').next().unwrap_or(entry).trim().to_owned();
        if kind.is_empty() {
            continue;
        }
        match counts.iter_mut().find(|(name, _)| *name == kind) {
            Some((_, count)) => *count += 1,
            None => counts.push((kind, 1)),
        }
    }

    let parts: Vec<String> = counts
        .into_iter()
        .map(|(name, count)| {
            if count > 1 {
                format!("{name} ×{count}")
            } else {
                name
            }
        })
        .collect();
    format!("▸ {}", parts.join(", "))
}

/// A Working pane animates; everything else is a steady glyph. Because the
/// spinner is the only thing that changes on its own, a list with no Working pane
/// produces byte-identical output every frame — which is exactly what the event
/// loop needs to skip drawing entirely.
fn icon_for(status: &AgentStatus, spinner: usize, theme: &Theme) -> (&'static str, Color) {
    let color = theme.state_color(status.state, status.seen);
    if status.state == AgentState::Working {
        return (SPINNER[spinner % SPINNER.len()], color);
    }
    (state_icon(status.state, status.seen), color)
}

fn badge_color(status: &AgentStatus, theme: &Theme) -> Color {
    use crate::model::PermissionMode::*;
    match status.permission_mode {
        BypassPermissions => theme.badge_danger,
        Plan => theme.badge_plan,
        AcceptEdits | Auto | DontAsk | Defer => theme.badge_auto,
        Default => theme.muted,
    }
}

/// What to call a pane.
///
/// An agent pane is named after its agent; anything else after the command it is
/// running, because "unknown" tells you nothing. A `?` suffix marks a reading
/// that came from passive detection of a *blocked* state — the one case where a
/// heuristic could plausibly be wrong about something you would act on.
fn pane_label(pane: &PaneInfo, show_index: bool) -> String {
    let base = match pane.status.agent {
        Some(agent) => agent.label().to_owned(),
        None => {
            if pane.current_command.is_empty() {
                "-".to_owned()
            } else {
                pane.current_command.clone()
            }
        }
    };
    let uncertain = pane.status.state == AgentState::Blocked
        && pane.status.source == StatusSource::Passive;
    let base = if uncertain { format!("{base}?") } else { base };

    if show_index {
        format!("{} {base}", pane.pane_index)
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, PermissionMode, TaskProgress};

    fn pane(pane_id: &str, status: AgentStatus) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.to_owned(),
            window_id: "@1".to_owned(),
            pane_index: "0".to_owned(),
            pane_active: false,
            current_command: "zsh".to_owned(),
            current_path: "/tmp".to_owned(),
            title: String::new(),
            pane_pid: Some(1),
            status,
            branch: String::new(),
            worktree: String::new(),
        }
    }

    fn agent_status(state: AgentState) -> AgentStatus {
        AgentStatus {
            agent: Some(AgentKind::Claude),
            state,
            source: StatusSource::Hook,
            seen: true,
            ..AgentStatus::default()
        }
    }

    fn tree(panes: Vec<PaneInfo>) -> Vec<SessionGroup> {
        vec![SessionGroup {
            session_name: "work".to_owned(),
            session_attached: true,
            windows: vec![WindowInfo {
                window_id: "@1".to_owned(),
                window_index: "1".to_owned(),
                window_name: "editor".to_owned(),
                window_active: true,
                panes,
            }],
        }]
    }

    fn render(sessions: &[SessionGroup], selected: usize, width: usize) -> RenderedList {
        build(sessions, selected, width, 0, 1_000, &Theme::default())
    }

    #[test]
    fn every_line_is_exactly_the_requested_width() {
        // A row one cell too wide wraps and desyncs everything below it, so this
        // is the single most important invariant in the renderer.
        let sessions = tree(vec![
            {
                let mut pane = pane("%1", agent_status(AgentState::Working));
                pane.branch = "feature/a-rather-long-branch-name".to_owned();
                pane.worktree = "wt-long-name".to_owned();
                pane.status.permission_mode = PermissionMode::BypassPermissions;
                pane.status.run_started_at = Some(0);
                pane.status.wait_reason = "permission".to_owned();
                pane.status.subagents = vec!["Explore:a".to_owned(), "Explore:b".to_owned()];
                pane.status.task_progress = Some(TaskProgress { done: 3, total: 7 });
                pane.status.background_cmd = Some("npm run dev -- --host 0.0.0.0".to_owned());
                pane
            },
            pane("%2", AgentStatus::unknown()),
        ]);

        for total in [12, 20, 24, 32, 40, 80] {
            let list = render(&sessions, 0, total);
            for (index, line) in list.plain.iter().enumerate() {
                assert_eq!(
                    width(line),
                    total,
                    "line {index} at width {total}: {line:?}"
                );
            }
        }
    }

    #[test]
    fn a_quiet_pane_stays_one_line_tall() {
        let list = render(&tree(vec![pane("%1", agent_status(AgentState::Idle))]), 0, 40);
        assert_eq!(list.blocks.len(), 1);
        assert_eq!(list.blocks[0].line_count, 1);
    }

    #[test]
    fn context_rows_appear_only_when_they_have_something_to_say() {
        let mut detailed = pane("%1", agent_status(AgentState::Blocked));
        detailed.branch = "main".to_owned();
        detailed.status.wait_reason = "permission".to_owned();
        detailed.status.task_progress = Some(TaskProgress { done: 1, total: 4 });

        let list = render(&tree(vec![detailed]), 0, 40);
        assert_eq!(list.blocks[0].line_count, 4, "status + branch + wait + tasks");
        let text = list.plain.join("\n");
        assert!(text.contains("main"));
        assert!(text.contains("▸ permission"));
        assert!(text.contains("▸ tasks 1/4"));
    }

    #[test]
    fn a_wait_reason_is_hidden_once_the_pane_is_no_longer_blocked() {
        // Stale reasons outlive the block they describe; showing one on a running
        // pane would imply a prompt that isn't there.
        let mut running = pane("%1", agent_status(AgentState::Working));
        running.status.wait_reason = "permission".to_owned();
        let list = render(&tree(vec![running]), 0, 40);
        assert!(!list.plain.join("\n").contains("permission"));
    }

    #[test]
    fn underscores_in_a_wait_reason_are_humanized() {
        let mut blocked = pane("%1", agent_status(AgentState::Blocked));
        blocked.status.wait_reason = "idle_prompt".to_owned();
        let list = render(&tree(vec![blocked]), 0, 40);
        assert!(list.plain.join("\n").contains("▸ idle prompt"));
    }

    #[test]
    fn subagents_are_counted_by_type_rather_than_listed() {
        assert_eq!(
            subagent_summary(&[
                "Explore:a".to_owned(),
                "Explore:b".to_owned(),
                "Plan:c".to_owned()
            ]),
            "▸ Explore ×2, Plan"
        );
        assert_eq!(subagent_summary(&["Explore".to_owned()]), "▸ Explore");
    }

    #[test]
    fn non_agent_panes_are_labelled_with_their_command() {
        let list = render(&tree(vec![pane("%1", AgentStatus::unknown())]), 0, 40);
        let text = list.plain.join("\n");
        assert!(text.contains("zsh"), "got {text:?}");
        assert!(!text.contains("unknown"));
    }

    #[test]
    fn a_passively_inferred_block_is_marked_uncertain() {
        // Passive blocked-detection is the one reading a user might act on and
        // the heuristics could get wrong, so it says so.
        let mut passive = pane("%1", agent_status(AgentState::Blocked));
        passive.status.source = StatusSource::Passive;
        let list = render(&tree(vec![passive]), 0, 40);
        assert!(list.plain.join("\n").contains("claude?"));

        let mut hooked = pane("%1", agent_status(AgentState::Blocked));
        hooked.status.source = StatusSource::Hook;
        let list = render(&tree(vec![hooked]), 0, 40);
        assert!(!list.plain.join("\n").contains("claude?"));
    }

    #[test]
    fn pane_indexes_show_only_in_multi_pane_windows() {
        let single = render(&tree(vec![pane("%1", agent_status(AgentState::Idle))]), 0, 40);
        assert!(single.plain[2].contains("claude"));
        assert!(!single.plain[2].contains("0 claude"));

        let mut second = pane("%2", agent_status(AgentState::Idle));
        second.pane_index = "1".to_owned();
        let multi = render(
            &tree(vec![pane("%1", agent_status(AgentState::Idle)), second]),
            0,
            40,
        );
        assert!(multi.plain[2].contains("0 claude"));
        assert!(multi.plain[3].contains("1 claude"));
    }

    #[test]
    fn blocks_are_indexed_in_navigation_order_across_sessions() {
        let sessions = vec![
            SessionGroup {
                session_name: "a".to_owned(),
                session_attached: true,
                windows: vec![WindowInfo {
                    window_id: "@1".to_owned(),
                    window_index: "1".to_owned(),
                    window_name: "one".to_owned(),
                    window_active: false,
                    panes: vec![pane("%1", AgentStatus::unknown())],
                }],
            },
            SessionGroup {
                session_name: "b".to_owned(),
                session_attached: false,
                windows: vec![WindowInfo {
                    window_id: "@2".to_owned(),
                    window_index: "2".to_owned(),
                    window_name: "two".to_owned(),
                    window_active: false,
                    panes: vec![pane("%2", AgentStatus::unknown())],
                }],
            },
        ];
        let list = render(&sessions, 0, 40);
        let ids: Vec<&str> = list
            .blocks
            .iter()
            .map(|block| block.target.pane_id.as_str())
            .collect();
        assert_eq!(ids, ["%1", "%2"]);
        assert_eq!(list.blocks[0].target.session_name, "a");
        assert_eq!(list.blocks[1].target.window_id, "@2");
    }

    #[test]
    fn block_at_line_maps_every_line_of_a_block_back_to_it() {
        let mut detailed = pane("%1", agent_status(AgentState::Blocked));
        detailed.branch = "main".to_owned();
        detailed.status.wait_reason = "permission".to_owned();
        let list = render(&tree(vec![detailed, pane("%2", AgentStatus::unknown())]), 0, 40);

        let first = &list.blocks[0];
        for line in first.line_start..first.line_start + first.line_count {
            assert_eq!(list.block_at_line(line), Some(0), "line {line}");
        }
        assert_eq!(list.block_at_line(list.blocks[1].line_start), Some(1));
        // Session and window headers belong to no block.
        assert_eq!(list.block_at_line(0), None);
        assert_eq!(list.block_at_line(1), None);
        assert_eq!(list.block_at_line(9_999), None);
    }

    #[test]
    fn an_out_of_range_selection_renders_without_panicking() {
        let list = render(&tree(vec![pane("%1", AgentStatus::unknown())]), 99, 40);
        assert_eq!(list.blocks.len(), 1);
    }

    #[test]
    fn an_empty_tree_renders_nothing_and_reports_empty() {
        let list = render(&[], 0, 40);
        assert!(list.is_empty());
        assert!(list.lines.is_empty());
    }

    #[test]
    fn output_is_byte_identical_across_frames_when_nothing_is_working() {
        // The flicker fix depends on this: with no Working pane, successive
        // spinner frames must produce the same text, so the event loop can skip
        // the draw entirely.
        let sessions = tree(vec![
            pane("%1", agent_status(AgentState::Idle)),
            pane("%2", agent_status(AgentState::Blocked)),
        ]);
        let first = build(&sessions, 0, 40, 0, 1_000, &Theme::default());
        let later = build(&sessions, 0, 40, 7, 1_000, &Theme::default());
        assert_eq!(first.plain, later.plain);
    }

    #[test]
    fn a_working_pane_does_change_between_spinner_frames() {
        let sessions = tree(vec![pane("%1", agent_status(AgentState::Working))]);
        let first = build(&sessions, 0, 40, 0, 1_000, &Theme::default());
        let later = build(&sessions, 0, 40, 1, 1_000, &Theme::default());
        assert_ne!(first.plain, later.plain);
    }

    #[test]
    fn the_active_pane_marker_needs_the_window_to_be_current_too() {
        let mut sessions = tree(vec![{
            let mut pane = pane("%1", agent_status(AgentState::Idle));
            pane.pane_active = true;
            pane
        }]);
        assert!(render(&sessions, 0, 40).plain[2].starts_with('┃'));

        sessions[0].windows[0].window_active = false;
        assert!(!render(&sessions, 0, 40).plain[2].starts_with('┃'));
    }

    #[test]
    fn the_label_does_not_shift_as_the_timer_rolls_over() {
        // Reserving the timer's current width would re-truncate the label every
        // time it went from `59s` to `1m`, so a long name would visibly twitch.
        let mut sessions = tree(vec![pane("%1", agent_status(AgentState::Working))]);
        sessions[0].windows[0].panes[0].status.run_started_at = Some(0);
        sessions[0].windows[0].panes[0].status.agent = None;
        sessions[0].windows[0].panes[0].current_command = "a-really-long-command-name".to_owned();

        let short_run = build(&sessions, 0, 30, 0, 30, &Theme::default());
        let long_run = build(&sessions, 0, 30, 0, 90_000, &Theme::default());

        // Everything before the truncation ellipsis is the label we kept.
        let label_of = |line: &str| line.split('…').next().unwrap_or_default().to_owned();
        assert_eq!(label_of(&short_run.plain[2]), label_of(&long_run.plain[2]));
    }

    #[test]
    fn a_long_session_name_is_truncated_rather_than_dropping_the_timer() {
        let mut sessions = tree(vec![pane("%1", agent_status(AgentState::Working))]);
        sessions[0].windows[0].panes[0].status.run_started_at = Some(0);
        sessions[0].windows[0].panes[0].current_command = "a".repeat(200);
        sessions[0].windows[0].panes[0].status.agent = None;

        let list = render(&sessions, 0, 30);
        let status_line = &list.plain[2];
        assert!(status_line.contains('…'), "label should be cut: {status_line:?}");
        assert!(
            status_line.trim_end().ends_with("16m"),
            "the timer must survive: {status_line:?}"
        );
        assert_eq!(width(status_line), 30);
    }
}
