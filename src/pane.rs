//! Sidebar pane lifecycle: creating it, killing it, keeping it the right width,
//! and cleaning up a window it was left alone in.
//!
//! The sidebar is a **real tmux pane** running our binary, not a popup. That is
//! what lets it stay open all day: tmux composites it like any other pane, so
//! nothing has to be repainted over the top of your work.
//!
//! Ported from `tmux-agent-sidebar`'s `src/cli/toggle.rs`, minus the
//! auto-create-on-new-window path — we only ever split on request, so a
//! declaratively built window (tmuxinator's `split-window` + `select-layout`)
//! can never have a pane injected into it mid-setup.

use std::collections::HashSet;

use crate::tmux;

/// Columns the working pane must keep. The sidebar is never allowed past
/// `window_width - this`, so it cannot squeeze your actual work off screen. This
/// guard deliberately overrides the configured minimum.
const MAIN_PANE_MIN_WIDTH: u32 = 20;
/// Floor applied when `@agent_mgr_min_width` is unset or unparseable.
const DEFAULT_MIN_WIDTH: u32 = 24;
/// Width used when `@agent_mgr_width` is neither a percentage nor a number.
const DEFAULT_EXPLICIT_WIDTH: u32 = 32;
/// Default percentage when `@agent_mgr_width` ends in `%` but the number is junk.
const DEFAULT_PERCENT: u32 = 20;

/// `agent-mgr toggle <window-id> [path]` — create the sidebar pane in a window,
/// or kill it if one is already there.
pub fn cmd_toggle(args: &[&str]) -> i32 {
    let Some(window_id) = args.first().copied() else {
        return 0;
    };
    let pane_path = args.get(1).copied().unwrap_or("~");

    if let Some(existing) = find_sidebar_pane(window_id) {
        tmux::run_tmux_quiet(&["kill-pane", "-t", &existing]);
        return 0;
    }

    create_sidebar(window_id, pane_path);
    0
}

/// `agent-mgr toggle-all` — one keystroke for the whole server.
///
/// If a sidebar exists anywhere, this turns them all off; otherwise it turns
/// them all on. Treating "any" as "on" means the key always does the thing you
/// expect after you have toggled one window individually.
pub fn cmd_toggle_all() -> i32 {
    let listing = tmux::run_tmux(&["list-panes", "-a", "-F", &pane_role_format()]).unwrap_or_default();

    if let Some(panes) = sidebar_panes(&listing) {
        for pane_id in panes {
            tmux::run_tmux_quiet(&["kill-pane", "-t", &pane_id]);
        }
        return 0;
    }

    let windows = tmux::run_tmux(&[
        "list-panes",
        "-a",
        "-F",
        &format!("#{{window_id}}\t{}", tmux::q("pane_current_path")),
    ])
    .unwrap_or_default();

    for (window_id, path) in unique_window_paths(&windows) {
        create_sidebar(&window_id, &path);
    }
    0
}

/// `agent-mgr resize <window-id>` — re-clamp an existing sidebar after the
/// window changed size, so a percentage width keeps meaning what it says.
/// Wired to tmux's `window-resized` hook.
pub fn cmd_resize(args: &[&str]) -> i32 {
    let Some(window_id) = args.first().copied() else {
        return 0;
    };
    let Some(sidebar) = find_sidebar_pane(window_id) else {
        return 0;
    };

    let target = configured_width(window_id);
    let current: u32 = tmux::display_message(&sidebar, "#{pane_width}")
        .parse()
        .unwrap_or(0);
    if current == target {
        return 0;
    }

    tmux::run_tmux_quiet(&["resize-pane", "-x", &target.to_string(), "-t", &sidebar]);
    0
}

/// `agent-mgr auto-close <window-id>` — close a window whose only remaining pane
/// is the sidebar. Wired to tmux's `pane-exited` hook so that exiting your shell
/// closes the window as it normally would, instead of leaving a lone sidebar.
pub fn cmd_auto_close(args: &[&str]) -> i32 {
    let Some(window_id) = args.first().copied() else {
        return 0;
    };

    let role_format = format!("#{{{}}}", tmux::PANE_ROLE);
    let panes = tmux::run_tmux(&["list-panes", "-t", window_id, "-F", &role_format]);
    let session_windows = numeric_format(window_id, "#{session_windows}");
    let session_attached = numeric_format(window_id, "#{session_attached}");

    if should_kill_window(panes.as_deref(), session_windows, session_attached) {
        tmux::run_tmux_quiet(&["kill-window", "-t", window_id]);
    }
    0
}

fn numeric_format(target: &str, format: &str) -> Option<u32> {
    tmux::run_tmux(&["display-message", "-t", target, "-p", format])
        .and_then(|value| value.trim().parse().ok())
}

/// Split a full-height sidebar pane into `window_id` and hand focus back.
fn create_sidebar(window_id: &str, pane_path: &str) {
    let width = configured_width(window_id).to_string();
    let position = Position::from_setting(&tmux::display_message(
        window_id,
        &format!("#{{{}}}", tmux::CFG_POSITION),
    ));

    let geometry = tmux::run_tmux(&[
        "list-panes",
        "-t",
        window_id,
        "-F",
        "#{pane_left} #{pane_width} #{pane_id}",
    ])
    .unwrap_or_default();
    let target = outermost_pane(&geometry, position).unwrap_or_else(|| window_id.to_owned());

    // Remember where focus was: splitting moves it into the new pane, and the
    // sidebar is something you glance at, not something you land in.
    let previously_active = tmux::display_message(window_id, "#{pane_id}");

    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
        .unwrap_or_else(|| "agent-mgr".to_owned());

    let sidebar = tmux::run_tmux(&[
        "split-window",
        position.split_flags(),
        "-l",
        &width,
        "-t",
        &target,
        "-c",
        pane_path,
        "-P",
        "-F",
        "#{pane_id}",
        &exe,
    ])
    .map(|id| id.trim().to_owned())
    .unwrap_or_default();

    if !sidebar.is_empty() {
        // Tag it immediately: this is how the TUI excludes itself from its own
        // list and how a later toggle finds the pane to kill.
        tmux::set_pane_option_raw(&sidebar, tmux::PANE_ROLE, tmux::PANE_ROLE_SIDEBAR);
    }

    if previously_active.is_empty() {
        tmux::run_tmux_quiet(&["select-pane", "-t", window_id, "-l"]);
    } else {
        tmux::run_tmux_quiet(&["select-pane", "-t", &previously_active]);
    }
}

/// Read the width options for `window_id` and resolve them to a column count.
fn configured_width(window_id: &str) -> u32 {
    let setting = tmux::display_message(window_id, &format!("#{{{}}}", tmux::CFG_WIDTH));
    let window_width = tmux::display_message(window_id, "#{window_width}")
        .parse()
        .unwrap_or(0);
    let min = tmux::display_message(window_id, &format!("#{{{}}}", tmux::CFG_MIN_WIDTH))
        .trim()
        .parse()
        .unwrap_or(DEFAULT_MIN_WIDTH);
    let max = tmux::display_message(window_id, &format!("#{{{}}}", tmux::CFG_MAX_WIDTH))
        .trim()
        .parse()
        .ok();

    resolve_width(&setting, window_width, min, max)
}

/// Resolve `@agent_mgr_width` (a column count or `N%`) into columns.
///
/// Clamped min → max → main-pane guard, in that order: an explicit `max` below
/// `min` still caps the result, and the guard wins on a genuinely tiny window
/// even against the configured minimum.
fn resolve_width(setting: &str, window_width: u32, min: u32, max: Option<u32>) -> u32 {
    let mut width = match setting.trim().strip_suffix('%') {
        Some(percent) => {
            let percent: u32 = percent.trim().parse().unwrap_or(DEFAULT_PERCENT);
            if window_width == 0 {
                // A percentage is meaningless without a window width; fall back
                // to the minimum rather than guessing.
                min
            } else {
                window_width * percent / 100
            }
        }
        None => setting.trim().parse().unwrap_or(DEFAULT_EXPLICIT_WIDTH),
    };

    width = width.max(min);
    if let Some(max) = max {
        width = width.min(max);
    }
    if window_width > 0 {
        width = width.min(window_width.saturating_sub(MAIN_PANE_MIN_WIDTH));
    }
    width.max(1)
}

/// Decide whether `auto-close` should kill the window, from the raw tmux answers.
///
/// Pure so the guard logic is testable without a live server — and the guards
/// matter, because getting this wrong drops a user's whole session.
///
/// - `panes`: stdout of `list-panes -F '#{@agent_mgr_pane_role}'`, or `None` if
///   the call failed.
/// - `session_windows` / `session_attached`: parsed formats, `None` on failure.
fn should_kill_window(
    panes: Option<&str>,
    session_windows: Option<u32>,
    session_attached: Option<u32>,
) -> bool {
    // No output is not the same as no panes. The window may already be gone, or
    // tmux may just be busy — treating it as "empty" would let a race kill a
    // live window.
    let Some(panes) = panes else {
        return false;
    };
    if panes.trim().is_empty() {
        return false;
    }

    // An unset role renders as an empty line, and that pane is an ordinary user
    // pane, so any line that isn't ours keeps the window alive.
    if panes.lines().any(|line| line != tmux::PANE_ROLE_SIDEBAR) {
        return false;
    }

    let Some(windows) = session_windows else {
        return false;
    };

    match windows {
        // Cannot prove there is anywhere to fall back to; preserve.
        0 => false,
        // Killing the last window destroys the session and drops every attached
        // client. One client is fine — that is what plain `exit` does on the last
        // pane. Two or more means a shared session (several terminal tabs on
        // `main`) where we cannot tell which clients are wanted, so leave the
        // sidebar stranded instead of mass-disconnecting. An unknown count errs
        // the same way.
        1 => matches!(session_attached, Some(count) if count <= 1),
        _ => true,
    }
}

fn pane_role_format() -> String {
    format!("#{{pane_id}}\t#{{{}}}", tmux::PANE_ROLE)
}

/// Extract sidebar pane ids from `pane_role_format` output. `None` when there
/// are none, so callers can distinguish "turn them off" from "turn them on".
fn sidebar_panes(listing: &str) -> Option<Vec<String>> {
    let panes: Vec<String> = listing
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, role)| *role == tmux::PANE_ROLE_SIDEBAR)
        .map(|(pane_id, _)| pane_id.to_owned())
        .collect();
    (!panes.is_empty()).then_some(panes)
}

fn find_sidebar_pane(window_id: &str) -> Option<String> {
    let listing = tmux::run_tmux(&["list-panes", "-t", window_id, "-F", &pane_role_format()])?;
    sidebar_panes(&listing)?.into_iter().next()
}

/// One `(window_id, path)` per window, keeping the first pane's path.
fn unique_window_paths(listing: &str) -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    let mut windows = Vec::new();
    for line in listing.lines() {
        let Some((window_id, path)) = line.split_once('\t') else {
            continue;
        };
        if seen.insert(window_id.to_owned()) {
            windows.push((window_id.to_owned(), path.to_owned()));
        }
    }
    windows
}

/// Which edge of the window the sidebar lives on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Position {
    Left,
    Right,
}

impl Position {
    /// Only an explicit `right` moves the sidebar; anything else — unset, empty,
    /// or a typo — stays left, so a mistake never relocates it unexpectedly.
    fn from_setting(setting: &str) -> Self {
        if setting.trim().eq_ignore_ascii_case("right") {
            Self::Right
        } else {
            Self::Left
        }
    }

    /// `-hfb` inserts the new pane before the target (to its left), `-hf` after
    /// it. Both `f` variants span the full window height.
    fn split_flags(self) -> &'static str {
        match self {
            Self::Left => "-hfb",
            Self::Right => "-hf",
        }
    }
}

/// Pick the pane to split from so the sidebar lands on the window's outer edge:
/// the leftmost pane for a left sidebar, the one with the largest right edge for
/// a right sidebar.
fn outermost_pane(geometry: &str, position: Position) -> Option<String> {
    let panes = geometry.lines().filter_map(|line| {
        let mut parts = line.split_whitespace();
        let left: u32 = parts.next()?.parse().ok()?;
        let width: u32 = parts.next()?.parse().ok()?;
        Some((left, width, parts.next()?.to_owned()))
    });

    match position {
        Position::Left => panes.min_by_key(|(left, _, _)| *left),
        Position::Right => panes.max_by_key(|(left, width, _)| left.saturating_add(*width)),
    }
    .map(|(_, _, pane_id)| pane_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── width resolution ─────────────────────────────────────────────

    #[test]
    fn percentage_width_on_a_normal_window() {
        assert_eq!(resolve_width("20%", 200, 24, None), 40);
    }

    #[test]
    fn percentage_below_the_minimum_is_clamped_up() {
        // 20% of 80 = 16, below the min of 24, and 80 is wide enough to allow it.
        assert_eq!(resolve_width("20%", 80, 24, None), 24);
    }

    #[test]
    fn explicit_columns_are_honoured_within_the_clamps() {
        assert_eq!(resolve_width("32", 200, 24, None), 32);
        assert_eq!(resolve_width("5", 200, 24, None), 24);
        assert_eq!(resolve_width("99", 200, 24, Some(40)), 40);
    }

    #[test]
    fn the_main_pane_guard_wins_on_a_tiny_window() {
        // A 30-column window may give the sidebar at most 10, even though the
        // configured minimum is 24: your work pane keeps its reserve.
        assert_eq!(resolve_width("50%", 30, 24, None), 10);
        assert_eq!(resolve_width("25", 30, 24, None), 10);
    }

    #[test]
    fn width_never_collapses_below_one_column() {
        assert_eq!(resolve_width("50%", 20, 24, None), 1);
        assert_eq!(resolve_width("50%", 10, 24, None), 1);
    }

    #[test]
    fn junk_settings_fall_back_to_defaults() {
        assert_eq!(resolve_width("wat", 200, 24, None), DEFAULT_EXPLICIT_WIDTH);
        // "%"-suffixed but unparseable: use the default percentage, not the min.
        assert_eq!(resolve_width("wat%", 200, 24, None), 200 * DEFAULT_PERCENT / 100);
    }

    #[test]
    fn unknown_window_width_skips_the_percentage_and_the_guard() {
        assert_eq!(resolve_width("20%", 0, 24, None), 24);
        assert_eq!(resolve_width("32", 0, 24, None), 32);
    }

    #[test]
    fn a_max_below_the_min_still_caps() {
        assert_eq!(resolve_width("20%", 200, 30, Some(10)), 10);
    }

    // ─── auto-close guards ────────────────────────────────────────────

    #[test]
    fn kills_a_window_holding_only_the_sidebar() {
        // The intended path. The attached count is irrelevant because other
        // windows exist, so killing this one cannot end the session.
        assert!(should_kill_window(Some("sidebar"), Some(2), None));
        assert!(should_kill_window(Some("sidebar"), Some(2), Some(5)));
    }

    #[test]
    fn keeps_a_window_that_still_has_a_real_pane() {
        assert!(!should_kill_window(Some("sidebar\npane"), Some(5), Some(1)));
        // An unset role renders as an empty line — that is a user's pane.
        assert!(!should_kill_window(Some("sidebar\n\n"), Some(5), Some(1)));
        assert!(!should_kill_window(Some("\nsidebar\n"), Some(5), Some(1)));
    }

    #[test]
    fn a_failed_or_empty_query_never_kills() {
        // Treating "no answer" as "no panes" would let a busy-tmux race destroy
        // a live window.
        assert!(!should_kill_window(None, Some(5), Some(1)));
        assert!(!should_kill_window(Some(""), Some(5), Some(1)));
        assert!(!should_kill_window(Some("   \n"), Some(5), Some(1)));
    }

    #[test]
    fn last_window_dies_only_when_at_most_one_client_is_attached() {
        // One client, or none: matches what plain `exit` would do.
        assert!(should_kill_window(Some("sidebar"), Some(1), Some(1)));
        assert!(should_kill_window(Some("sidebar"), Some(1), Some(0)));
        // Several terminal tabs share this session — killing it would drop them
        // all at once. Strand the sidebar instead.
        assert!(!should_kill_window(Some("sidebar"), Some(1), Some(2)));
        assert!(!should_kill_window(Some("sidebar"), Some(1), Some(7)));
        // Unknown client count: err toward preservation.
        assert!(!should_kill_window(Some("sidebar"), Some(1), None));
    }

    #[test]
    fn an_unprovable_session_shape_never_kills() {
        assert!(!should_kill_window(Some("sidebar"), None, Some(1)));
        assert!(!should_kill_window(Some("sidebar"), Some(0), Some(1)));
    }

    // ─── placement ────────────────────────────────────────────────────

    #[test]
    fn only_an_explicit_right_moves_the_sidebar() {
        assert_eq!(Position::from_setting("right"), Position::Right);
        assert_eq!(Position::from_setting(" RIGHT "), Position::Right);
        assert_eq!(Position::from_setting("left"), Position::Left);
        assert_eq!(Position::from_setting(""), Position::Left);
        assert_eq!(Position::from_setting("rihgt"), Position::Left);
    }

    #[test]
    fn split_flags_match_tmux_side_semantics() {
        assert_eq!(Position::Left.split_flags(), "-hfb");
        assert_eq!(Position::Right.split_flags(), "-hf");
    }

    #[test]
    fn outermost_pane_finds_the_window_edge() {
        let geometry = "40 80 %3\n0 20 %1\n20 20 %2";
        assert_eq!(
            outermost_pane(geometry, Position::Left),
            Some("%1".to_owned())
        );
        assert_eq!(
            outermost_pane(geometry, Position::Right),
            Some("%3".to_owned())
        );
    }

    #[test]
    fn outermost_pane_skips_malformed_lines() {
        assert_eq!(
            outermost_pane("bad\n0 nope %1\n12 30 %2", Position::Left),
            Some("%2".to_owned())
        );
        assert_eq!(outermost_pane("", Position::Right), None);
    }

    // ─── listing helpers ──────────────────────────────────────────────

    #[test]
    fn sidebar_panes_finds_every_sidebar_or_none() {
        assert_eq!(
            sidebar_panes("%1\t\n%2\tsidebar\n%3\t\n%4\tsidebar"),
            Some(vec!["%2".to_owned(), "%4".to_owned()])
        );
        assert_eq!(sidebar_panes("%1\t\n%2\t"), None);
        assert_eq!(sidebar_panes(""), None);
    }

    #[test]
    fn unique_window_paths_dedupes_and_keeps_spaces_in_paths() {
        assert_eq!(
            unique_window_paths("@1\t/home/me/My Project\n@1\t/other\n@2\t/tmp/x"),
            vec![
                ("@1".to_owned(), "/home/me/My Project".to_owned()),
                ("@2".to_owned(), "/tmp/x".to_owned()),
            ]
        );
    }

    #[test]
    fn unique_window_paths_skips_malformed_lines() {
        assert_eq!(
            unique_window_paths("garbage\n@1\t/tmp"),
            vec![("@1".to_owned(), "/tmp".to_owned())]
        );
    }
}
