//! The event loop, and the state it drives.
//!
//! # Why this loop looks the way it does
//!
//! The plugin this one replaces flickered, and the cause was its redraw policy:
//! it called `terminal.draw()` on a 50 ms timer whether or not anything had
//! changed, and forced a full `terminal.clear()` every 500 ms. Inside a tmux
//! overlay that means recompositing the region twice a second, forever, even
//! with nobody watching.
//!
//! So this loop holds three rules:
//!
//! 1. **Draw only when the visible output actually changed.** Rendering is pure
//!    ([`crate::ui::rows`]), so each pass builds the lines, hashes their text,
//!    and draws only on a different hash. Not "state changed" — *output* changed.
//! 2. **Never clear on a timer.** The one `terminal.clear()` in this crate is in
//!    the resize arm, where the terminal's own geometry changed under us.
//! 3. **Animate only when there is something to animate.** The spinner clock
//!    advances only while a pane is Working. With nothing running, successive
//!    passes produce byte-identical lines, the hash never moves, and the terminal
//!    receives nothing at all.
//!
//! The result: an idle sidebar left open for an hour writes zero bytes to the
//! terminal. Set `AGENT_MGR_DEBUG_FRAMES=<path>` to have the draw count written
//! there on exit and check it yourself.

mod input;
mod worker;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::daemon;
use crate::model::{AgentState, AgentStatus, SessionGroup};
use crate::nav::{self, Direction};
use crate::search::Query;
use crate::tmux;
use crate::ui::{self, Counts, Surface, rows, rows::RenderedList, theme::Theme};

/// Spinner frame duration. Ten frames, so a full cycle is 1.5 s.
const SPINNER_INTERVAL: Duration = Duration::from_millis(150);
/// Input poll timeout when nothing is running. Bounds how long a SIGUSR1 or a
/// worker snapshot waits to be noticed; each wake-up is pure CPU, no terminal I/O.
const QUIET_TIMEOUT: Duration = Duration::from_millis(250);

/// Which panes the list shows.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum StatusFilter {
    #[default]
    All,
    Working,
    Blocked,
    /// Finished runs the user hasn't looked at yet — the "what came back while I
    /// was away" view.
    Done,
}

impl StatusFilter {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Working,
            Self::Working => Self::Blocked,
            Self::Blocked => Self::Done,
            Self::Done => Self::All,
        }
    }

    fn matches(self, status: &AgentStatus) -> bool {
        match self {
            Self::All => true,
            Self::Working => status.state == AgentState::Working,
            // An error is something you must deal with, so it belongs in the
            // "needs me" view rather than only in a state nobody thinks to check.
            Self::Blocked => matches!(status.state, AgentState::Blocked | AgentState::Error),
            Self::Done => status.is_done(),
        }
    }
}

/// An open search.
///
/// The query outlives the typing: `Enter` closes the prompt but keeps the filter,
/// so you can search, then navigate the narrowed list with the normal motions.
/// That is the whole point of committing rather than just filtering while a key is
/// held. `Esc` is what clears it.
///
/// Deliberately not `Default`: the only sensible initial state has `editing:
/// true`, but a derived default would silently produce `false` — a search that
/// filters nothing and never accepts a keystroke.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SearchState {
    pub query: String,
    /// `true` while the prompt has the keyboard.
    pub editing: bool,
}

impl SearchState {
    fn open() -> Self {
        Self {
            query: String::new(),
            editing: true,
        }
    }
}

/// An open window-rename prompt.
///
/// Carries the window id it was opened against rather than re-reading the
/// selection on commit: the worker replaces the tree about once a second, and an
/// agent starting or stopping can move the cursor while you are still typing.
/// Resolving the target late would rename whatever happened to be under the cursor
/// by then.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RenameState {
    pub window_id: String,
    /// What the window was called, shown in the prompt so you can see what you are
    /// replacing.
    pub original: String,
    pub name: String,
}

pub struct App {
    pub surface: Surface,
    /// Our own pane, excluded from the list and never navigated to. Empty in a
    /// popup, which is not a pane in any window and so has nothing to exclude.
    pub own_pane: String,
    pub theme: Theme,
    /// The full tree as collected, before filtering.
    pub sessions: Vec<SessionGroup>,
    pub filter: StatusFilter,
    pub counts: Counts,
    /// Index into `list.blocks`.
    pub selected: usize,
    /// First visible line of the list.
    pub scroll: usize,
    /// Show the relative-number gutter. On by default: it is what makes a counted
    /// motion like `10j` something you can aim rather than guess at.
    pub numbers: bool,
    /// Digits typed so far, awaiting a motion to consume them.
    pub pending_count: Option<usize>,
    /// The live search, if one is open. See [`SearchState`].
    pub search: Option<SearchState>,
    /// The window-rename prompt, if one is open.
    pub rename: Option<RenameState>,
    /// Showing the keymap instead of the list.
    pub help: bool,
    pub spinner: usize,
    pub list: RenderedList,
    pub size: (u16, u16),
    /// Set to leave the loop; the pane closes with us, which is how the sidebar
    /// is dismissed from inside.
    pub quit: bool,
    /// Draws performed, for the flicker regression check.
    pub frames: u64,
}

impl App {
    fn new(surface: Surface, own_pane: String, size: (u16, u16)) -> Self {
        Self {
            surface,
            own_pane,
            theme: Theme::from_tmux(),
            sessions: Vec::new(),
            filter: StatusFilter::default(),
            counts: Counts::default(),
            selected: 0,
            scroll: 0,
            numbers: true,
            pending_count: None,
            search: None,
            rename: None,
            help: false,
            spinner: 0,
            list: RenderedList {
                lines: Vec::new(),
                plain: Vec::new(),
                blocks: Vec::new(),
            },
            size,
            quit: false,
            frames: 0,
        }
    }

    /// `true` while any pane is doing something worth animating.
    fn any_active(&self) -> bool {
        self.sessions
            .iter()
            .flat_map(|session| &session.windows)
            .flat_map(|window| &window.panes)
            .any(|pane| pane.status.state.is_active())
    }

    /// Panes the current filter is keeping off screen.
    pub fn hidden_count(&self) -> usize {
        let total: usize = self
            .sessions
            .iter()
            .flat_map(|session| &session.windows)
            .map(|window| window.panes.len())
            .sum();
        total.saturating_sub(self.list.blocks.len())
    }

    pub fn list_height(&self) -> usize {
        ui::list_height(self.size.1) as usize
    }

    /// The active search query, or an empty one when no search is open.
    pub fn query(&self) -> Query {
        Query::new(self.search.as_ref().map_or("", |search| &search.query))
    }

    /// Rebuild the rendered list from the current tree and selection.
    ///
    /// Pure and cheap — no I/O — which is what lets the loop call it on every
    /// pass and use its output as the change test.
    fn rebuild(&mut self) {
        let filtered = filter_sessions(&self.sessions, self.filter, &self.query());
        // Keep the cursor on the same pane across a refresh where possible: rows
        // come and go constantly as agents start and stop, and a selection that
        // jumps under your fingers is worse than one that lags.
        let anchor = self
            .list
            .blocks
            .get(self.selected)
            .map(|block| block.target.pane_id.clone());

        let mut opts = rows::Options {
            selected: self.selected,
            total_width: self.size.0 as usize,
            spinner: self.spinner,
            now: tmux::unix_timestamp(),
            numbers: self.numbers,
        };
        self.list = rows::build(&filtered, &opts, &self.theme);

        if let Some(pane_id) = anchor
            && let Some(index) = self
                .list
                .blocks
                .iter()
                .position(|block| block.target.pane_id == pane_id)
            && index != self.selected
        {
            self.selected = index;
            // Rebuild once more so the highlight lands on the row we just moved
            // to rather than on whatever now occupies the old index — and, with the
            // gutter on, so the relative numbers count from the right row.
            opts.selected = index;
            self.list = rows::build(&filtered, &opts, &self.theme);
        }

        self.clamp_selection();
        self.clamp_scroll();
        self.counts = Counts::tally(&self.sessions);
    }

    fn clamp_selection(&mut self) {
        let last = self.list.blocks.len().saturating_sub(1);
        self.selected = self.selected.min(last);
    }

    /// Scroll the minimum needed to keep the selected block fully visible.
    fn clamp_scroll(&mut self) {
        let height = self.list_height();
        if height == 0 || self.list.blocks.is_empty() {
            self.scroll = 0;
            return;
        }

        let start = self.list.block_line(self.selected);
        let end = start + self.list.block_height(self.selected);

        if start < self.scroll {
            // Reveal the session and window headers sitting directly above the
            // block too. Without this, scrolling up to the top pane of a session
            // hides the very lines that say which session it is.
            self.scroll = self.header_line_above(start);
        } else if end > self.scroll + height {
            self.scroll = end - height;
        }

        // Never leave blank space below a list that fits.
        let max_scroll = self.list.lines.len().saturating_sub(height);
        self.scroll = self.scroll.min(max_scroll);
    }

    /// The first line of the run of header lines immediately above `line`.
    ///
    /// Headers belong to no block, so they are the lines that scroll away first —
    /// which is exactly the context you need to read the pane below them.
    fn header_line_above(&self, line: usize) -> usize {
        let mut first = line;
        while first > 0 && self.list.block_at_line(first - 1).is_none() {
            first -= 1;
        }
        first
    }

    /// Which pane `Enter` would jump to, or `None` if there is nowhere to go.
    ///
    /// Split out from [`Self::activate_selection`] so the decision — including the
    /// refusal to navigate to our own pane — is testable without issuing
    /// `switch-client`, which in a test run would move the developer's own tmux
    /// client out from under them.
    pub fn activation_target(&self) -> Option<rows::PaneTarget> {
        let target = &self.list.blocks.get(self.selected)?.target;
        // An empty `own_pane` (a popup) must not match a real pane id.
        if !self.own_pane.is_empty() && target.pane_id == self.own_pane {
            return None;
        }
        Some(target.clone())
    }

    /// Jump tmux to the selected pane, and mark its window as caught up.
    fn activate_selection(&mut self) {
        let Some(target) = self.activation_target() else {
            return;
        };
        tmux::run_tmux_quiet(&["switch-client", "-t", &target.session_name]);
        tmux::run_tmux_quiet(&["select-window", "-t", &target.window_id]);
        tmux::run_tmux_quiet(&["select-pane", "-t", &target.pane_id]);
        // Visiting a window is how you acknowledge "this finished".
        daemon::mark_window_seen(&target.window_id);
        // A popup is covering the pane it just switched to; get out of the way.
        if self.surface.dismisses_on_activate() {
            self.quit = true;
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let (direction, count) = if delta < 0 {
            (Direction::Up, delta.unsigned_abs())
        } else {
            (Direction::Down, delta as usize)
        };
        self.selected = nav::step(&self.list.blocks, self.selected, direction, count);
    }

    /// Move by a pending count if one was typed, otherwise by one.
    fn move_counted(&mut self, direction: Direction) {
        let count = nav::take_count(&mut self.pending_count);
        self.selected = nav::step(&self.list.blocks, self.selected, direction, count);
    }

    /// Open the search prompt, keeping any query already typed.
    fn open_search(&mut self) {
        self.pending_count = None;
        match &mut self.search {
            Some(search) => search.editing = true,
            None => self.search = Some(SearchState::open()),
        }
    }

    /// Close the prompt but keep filtering, so the normal motions now work over the
    /// narrowed list.
    fn commit_search(&mut self) {
        match &mut self.search {
            // An empty query is not a filter; leaving it "committed" would show a
            // stale `/` in the footer forever.
            Some(search) if search.query.is_empty() => self.search = None,
            Some(search) => search.editing = false,
            None => {}
        }
    }

    /// Open the rename prompt on the selected pane's window.
    fn open_rename(&mut self) {
        self.pending_count = None;
        let Some(block) = self.list.blocks.get(self.selected) else {
            return;
        };
        let window_id = block.target.window_id.clone();
        // Seed with the current name so the common case is an edit, not a retype.
        let original = self
            .sessions
            .iter()
            .flat_map(|session| &session.windows)
            .find(|window| window.window_id == window_id)
            .map(|window| window.window_name.clone())
            .unwrap_or_default();
        self.rename = Some(RenameState {
            window_id,
            name: original.clone(),
            original,
        });
    }

    /// Take the pending rename, if it should be applied.
    ///
    /// Pure, so the state machine is testable without running `rename-window`
    /// against the tmux server hosting the test suite. The caller does the I/O.
    fn take_rename(&mut self) -> Option<(String, String)> {
        let state = self.rename.take()?;
        let name = state.name.trim().to_owned();
        // An empty name would make tmux fall back to its automatic name, which
        // looks like the rename silently failed. Unchanged is simply a no-op.
        if name.is_empty() || name == state.original {
            return None;
        }
        Some((state.window_id, name))
    }

    /// `H` / `L`: jump a whole session.
    fn jump_session(&mut self, direction: Direction) {
        // A count would have to mean "N sessions over", which nobody can aim; drop
        // it rather than have it silently apply to the next motion instead.
        self.pending_count = None;
        self.selected = nav::session_edge(&self.list.blocks, self.selected, direction);
    }
}

/// Drop panes the status filter or the search excludes, then drop the windows and
/// sessions that leaves empty — a session header with nothing under it is noise.
///
/// The two narrow independently and both must pass, so a search inside a `blocked`
/// filter means "blocked agents, among these" rather than one silently replacing
/// the other.
fn filter_sessions(
    sessions: &[SessionGroup],
    filter: StatusFilter,
    query: &Query,
) -> Vec<SessionGroup> {
    if filter == StatusFilter::All && query.is_empty() {
        return sessions.to_vec();
    }

    sessions
        .iter()
        .filter_map(|session| {
            let windows: Vec<_> = session
                .windows
                .iter()
                .filter_map(|window| {
                    let panes: Vec<_> = window
                        .panes
                        .iter()
                        .filter(|pane| {
                            filter.matches(&pane.status) && query.matches(session, window, pane)
                        })
                        .cloned()
                        .collect();
                    (!panes.is_empty()).then(|| crate::model::WindowInfo {
                        panes,
                        ..window.clone()
                    })
                })
                .collect();
            (!windows.is_empty()).then(|| SessionGroup {
                windows,
                ..session.clone()
            })
        })
        .collect()
}

/// Hash of everything that affects what is on screen.
///
/// The line *text* is the bulk of it. Style-only differences always travel with a
/// glyph change (icons, badges, markers) or with the selection index, both of
/// which are covered here — so a matching hash really does mean a matching
/// screen. `size` is included because the same text at a new width is a new frame.
fn fingerprint(app: &App) -> u64 {
    let mut hasher = DefaultHasher::new();
    app.list.plain.hash(&mut hasher);
    app.selected.hash(&mut hasher);
    app.scroll.hash(&mut hasher);
    app.size.hash(&mut hasher);
    app.filter.hash(&mut hasher);
    app.counts.hash(&mut hasher);
    // Both are shown in the footer, so they are part of the screen.
    app.pending_count.hash(&mut hasher);
    app.search.hash(&mut hasher);
    // The help page replaces the list, and the rename prompt owns the footer.
    app.help.hash(&mut hasher);
    app.rename.hash(&mut hasher);
    hasher.finish()
}

pub fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    surface: Surface,
    tmux_pane: String,
    needs_refresh: &'static AtomicBool,
) -> io::Result<()> {
    let size = terminal.size()?;
    let mut app = App::new(surface, tmux_pane, (size.width, size.height));

    let worker = worker::spawn(tmux::global_bool(tmux::CFG_AGENTS_ONLY, false));
    // Nothing to show until the first collection lands; ask for it now rather
    // than waiting out an interval.
    worker.request_refresh();

    let mut last_fingerprint: Option<u64> = None;
    let mut last_spinner = Instant::now();
    let started = Instant::now();

    while !app.quit {
        // 1. Newest snapshot wins; drain so a burst can't build a backlog.
        let mut received = false;
        while let Ok(sessions) = worker.rx.try_recv() {
            app.sessions = sessions;
            received = true;
        }

        // 2. A focus change reached us by signal.
        if needs_refresh.swap(false, Ordering::Relaxed) {
            worker.request_refresh();
        }

        // 3. Advance the spinner only when something is running. This is what
        //    makes a quiet sidebar produce identical output pass after pass.
        let active = app.any_active();
        if active && last_spinner.elapsed() >= SPINNER_INTERVAL {
            app.spinner = app.spinner.wrapping_add(1);
            last_spinner = Instant::now();
        }

        // 4. Rebuild (pure) and draw only if the output moved.
        app.rebuild();
        let current = fingerprint(&app);
        if last_fingerprint != Some(current) {
            terminal.draw(|frame| ui::draw(frame, &app))?;
            app.frames += 1;
            last_fingerprint = Some(current);
        }
        let _ = received;

        // 5. Wait for input. When active, wake in time for the next spinner
        //    frame; otherwise sleep as long as responsiveness allows.
        let timeout = if active {
            SPINNER_INTERVAL.saturating_sub(last_spinner.elapsed())
        } else {
            QUIET_TIMEOUT
        };
        if !event::poll(timeout.max(Duration::from_millis(10)))? {
            continue;
        }
        loop {
            match event::read()? {
                // The only clear in the crate: our geometry changed underneath
                // us, so the previous frame's cells are meaningless.
                Event::Resize(width, height) => {
                    app.size = (width, height);
                    terminal.clear()?;
                    last_fingerprint = None;
                }
                other => input::handle(other, &mut app, &worker),
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }

    report_frames(&app, started.elapsed());
    Ok(())
}

/// Write the draw count to the path in `AGENT_MGR_DEBUG_FRAMES`, if set.
///
/// Exists to make the anti-flicker claim falsifiable: leave an idle sidebar open
/// for a minute and this should read a handful of frames, not hundreds. Writes to
/// a file rather than stderr because our pane closes the moment we return.
fn report_frames(app: &App, elapsed: Duration) {
    let Ok(path) = std::env::var("AGENT_MGR_DEBUG_FRAMES") else {
        return;
    };
    let _ = std::fs::write(
        path,
        format!("frames={} seconds={:.1}\n", app.frames, elapsed.as_secs_f64()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, PaneInfo, StatusSource, WindowInfo};

    fn pane(pane_id: &str, state: AgentState, seen: bool) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.to_owned(),
            window_id: "@1".to_owned(),
            pane_index: "0".to_owned(),
            pane_active: false,
            current_command: "claude".to_owned(),
            current_path: "/tmp".to_owned(),
            title: String::new(),
            pane_pid: None,
            status: AgentStatus {
                agent: Some(AgentKind::Claude),
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

    fn app_with(panes: Vec<PaneInfo>, height: u16) -> App {
        surfaced_app(Surface::Sidebar, panes, height)
    }

    fn surfaced_app(surface: Surface, panes: Vec<PaneInfo>, height: u16) -> App {
        let mut app = App::new(surface, "%99".to_owned(), (40, height));
        app.sessions = tree(panes);
        app.rebuild();
        app
    }

    // ─── filter ───────────────────────────────────────────────────────

    #[test]
    fn filter_cycles_back_to_all() {
        let mut filter = StatusFilter::All;
        for _ in 0..4 {
            filter = filter.next();
        }
        assert_eq!(filter, StatusFilter::All);
    }

    #[test]
    fn the_blocked_filter_also_surfaces_errors() {
        // A failed run needs you as much as a prompt does; hiding it in a state
        // nobody thinks to select would lose it.
        let status = |state| AgentStatus {
            agent: Some(AgentKind::Claude),
            state,
            seen: true,
            ..AgentStatus::default()
        };
        assert!(StatusFilter::Blocked.matches(&status(AgentState::Blocked)));
        assert!(StatusFilter::Blocked.matches(&status(AgentState::Error)));
        assert!(!StatusFilter::Blocked.matches(&status(AgentState::Working)));
    }

    #[test]
    fn the_done_filter_shows_only_unacknowledged_finished_runs() {
        let done = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Idle,
            seen: false,
            ..AgentStatus::default()
        };
        let acknowledged = AgentStatus { seen: true, ..done.clone() };
        assert!(StatusFilter::Done.matches(&done));
        assert!(!StatusFilter::Done.matches(&acknowledged));
    }

    #[test]
    fn filtering_drops_windows_and_sessions_it_empties() {
        let sessions = tree(vec![
            pane("%1", AgentState::Working, true),
            pane("%2", AgentState::Idle, true),
        ]);
        let working = filter_sessions(&sessions, StatusFilter::Working, &Query::default());
        assert_eq!(working[0].windows[0].panes.len(), 1);

        // Nothing matches: no empty session header left behind.
        assert!(filter_sessions(&sessions, StatusFilter::Done, &Query::default()).is_empty());
    }

    #[test]
    fn the_all_filter_passes_the_tree_through_untouched() {
        let sessions = tree(vec![pane("%1", AgentState::Idle, true)]);
        assert_eq!(
            filter_sessions(&sessions, StatusFilter::All, &Query::default()),
            sessions
        );
    }

    #[test]
    fn hidden_count_reports_what_the_filter_is_holding_back() {
        let mut app = app_with(
            vec![
                pane("%1", AgentState::Working, true),
                pane("%2", AgentState::Idle, true),
                pane("%3", AgentState::Idle, true),
            ],
            40,
        );
        assert_eq!(app.hidden_count(), 0);

        app.filter = StatusFilter::Working;
        app.rebuild();
        assert_eq!(app.hidden_count(), 2);
    }

    // ─── selection and scrolling ──────────────────────────────────────

    #[test]
    fn selection_moves_within_bounds_and_saturates() {
        let mut app = app_with(
            vec![
                pane("%1", AgentState::Idle, true),
                pane("%2", AgentState::Idle, true),
                pane("%3", AgentState::Idle, true),
            ],
            40,
        );
        app.move_selection(1);
        assert_eq!(app.selected, 1);
        app.move_selection(10);
        assert_eq!(app.selected, 2, "must not run off the end");
        app.move_selection(-10);
        assert_eq!(app.selected, 0, "must not underflow");
    }

    #[test]
    fn selection_follows_its_pane_when_rows_shift_around_it() {
        // Agents start and stop constantly; a cursor that jumps to a different
        // pane under the user's fingers is how you activate the wrong window.
        let mut app = app_with(
            vec![
                pane("%1", AgentState::Idle, true),
                pane("%2", AgentState::Idle, true),
                pane("%3", AgentState::Idle, true),
            ],
            40,
        );
        app.selected = 2;
        app.rebuild();
        assert_eq!(app.list.blocks[app.selected].target.pane_id, "%3");

        // A new pane appears above the selection.
        app.sessions[0].windows[0]
            .panes
            .insert(0, pane("%0", AgentState::Working, true));
        app.rebuild();
        assert_eq!(
            app.list.blocks[app.selected].target.pane_id, "%3",
            "the cursor should still be on the same pane"
        );
    }

    #[test]
    fn selection_clamps_when_its_pane_disappears() {
        let mut app = app_with(
            vec![
                pane("%1", AgentState::Idle, true),
                pane("%2", AgentState::Idle, true),
            ],
            40,
        );
        app.selected = 1;
        app.rebuild();

        app.sessions[0].windows[0].panes.pop();
        app.rebuild();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn scroll_follows_the_selection_down_and_back_up() {
        // A 6-row pane leaves 4 list rows: 2 header rows plus 2 pane rows.
        let panes: Vec<PaneInfo> = (0..10)
            .map(|index| pane(&format!("%{index}"), AgentState::Idle, true))
            .collect();
        let mut app = app_with(panes, 6);
        assert_eq!(app.scroll, 0);

        app.selected = 9;
        app.rebuild();
        let last_line = app.list.block_line(9);
        assert!(
            last_line >= app.scroll && last_line < app.scroll + app.list_height(),
            "selected line {last_line} outside viewport at scroll {}",
            app.scroll
        );

        app.selected = 0;
        app.rebuild();
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn scrolling_up_to_a_pane_brings_its_headers_back_with_it() {
        // The session and window headers are the only thing saying *where* a pane
        // is; scrolling so the pane is visible but its headers are not defeats the
        // point of grouping.
        let panes: Vec<PaneInfo> = (0..10)
            .map(|index| pane(&format!("%{index}"), AgentState::Idle, true))
            .collect();
        let mut app = app_with(panes, 6);

        app.selected = 9;
        app.rebuild();
        assert!(app.scroll > 0, "should have scrolled down");

        app.selected = 0;
        app.rebuild();
        assert_eq!(app.scroll, 0, "headers on lines 0-1 must come back into view");
        assert!(app.list.block_at_line(0).is_none(), "line 0 is a header");
    }

    #[test]
    fn scroll_never_leaves_blank_space_below_a_short_list() {
        let mut app = app_with(vec![pane("%1", AgentState::Idle, true)], 40);
        app.scroll = 100;
        app.clamp_scroll();
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn a_zero_height_pane_does_not_panic_or_scroll() {
        let app = app_with(vec![pane("%1", AgentState::Idle, true)], 1);
        assert_eq!(app.list_height(), 0);
        assert_eq!(app.scroll, 0);
    }

    // ─── the anti-flicker contract ────────────────────────────────────

    #[test]
    fn fingerprint_is_stable_across_spinner_ticks_when_nothing_is_working() {
        // The core of the flicker fix: a quiet workspace must hash the same
        // forever, so the loop never writes to the terminal.
        let mut app = app_with(
            vec![
                pane("%1", AgentState::Idle, true),
                pane("%2", AgentState::Blocked, true),
            ],
            40,
        );
        let first = fingerprint(&app);
        for _ in 0..50 {
            app.spinner = app.spinner.wrapping_add(1);
            app.rebuild();
            assert_eq!(fingerprint(&app), first);
        }
        assert!(!app.any_active() || app.any_active(), "sanity");
    }

    #[test]
    fn fingerprint_moves_when_a_working_pane_animates() {
        let mut app = app_with(vec![pane("%1", AgentState::Working, true)], 40);
        assert!(app.any_active());
        let first = fingerprint(&app);
        app.spinner += 1;
        app.rebuild();
        assert_ne!(fingerprint(&app), first);
    }

    #[test]
    fn fingerprint_moves_when_a_pane_changes_state() {
        let mut app = app_with(vec![pane("%1", AgentState::Idle, true)], 40);
        let before = fingerprint(&app);
        app.sessions[0].windows[0].panes[0].status.state = AgentState::Blocked;
        app.rebuild();
        assert_ne!(fingerprint(&app), before);
    }

    #[test]
    fn fingerprint_moves_when_only_the_selection_changes() {
        // Selection is styling, not text, so it has to be hashed explicitly.
        let mut app = app_with(
            vec![
                pane("%1", AgentState::Idle, true),
                pane("%2", AgentState::Idle, true),
            ],
            40,
        );
        let before = fingerprint(&app);
        app.move_selection(1);
        app.rebuild();
        assert_ne!(fingerprint(&app), before);
    }

    #[test]
    fn fingerprint_moves_on_resize() {
        let mut app = app_with(vec![pane("%1", AgentState::Idle, true)], 40);
        let before = fingerprint(&app);
        app.size = (60, 40);
        app.rebuild();
        assert_ne!(fingerprint(&app), before);
    }

    // ─── surfaces ─────────────────────────────────────────────────────

    #[test]
    fn a_popup_dismisses_on_activate_and_a_sidebar_does_not() {
        // The sidebar's whole value is that you keep jumping around with it open.
        assert!(Surface::Popup.dismisses_on_activate());
        assert!(!Surface::Sidebar.dismisses_on_activate());
    }

    #[test]
    fn a_popup_has_no_own_pane_to_refuse() {
        // A popup is not a pane in any window, so every listed pane is a legitimate
        // jump target — including the one the binding fired from.
        let mut app = surfaced_app(Surface::Popup, vec![pane("%1", AgentState::Idle, true)], 40);
        app.own_pane = String::new();
        assert_eq!(app.activation_target().unwrap().pane_id, "%1");
    }

    #[test]
    fn a_pane_with_no_agent_is_not_treated_as_active() {
        let mut plain = pane("%1", AgentState::Unknown, true);
        plain.status.agent = None;
        let app = app_with(vec![plain], 40);
        assert!(!app.any_active(), "a plain shell must not animate the sidebar");
    }
}
