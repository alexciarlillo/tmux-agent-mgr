//! The status daemon: one poller per tmux server that resolves every pane's
//! agent status and caches it in tmux pane options.
//!
//! Why a daemon at all, rather than each sidebar polling for itself: the work is
//! `ps` plus a `capture-pane` per undecided agent pane, and it is identical for
//! every viewer. Centralizing it means ten open sidebars and the window-tab
//! markers all cost one poller, and every reader gets its data from a single
//! `list-panes` call.
//!
//! ## Precedence
//!
//! For each pane, in order:
//!
//! 1. **Hook state**, when the agent's hooks have reported anything *and* an
//!    agent process is still alive under the pane. Hook writes are assertions,
//!    not samples, so they commit immediately — debouncing them would only add
//!    latency to a transition we already know happened.
//! 2. **Passive detection** otherwise, debounced across polls because it reads a
//!    live UI and a single frame can lie.
//!
//! The daemon owns liveness for *both* sources. When no agent process remains
//! under a pane, hook options are swept — otherwise a `kill -9`'d agent would
//! leave its pane latched to "running" forever, since the process never got to
//! run its `SessionEnd` hook.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::detect::{
    SCREEN_TAIL_LINES, agent_from_process_name, state_from_evidence, state_from_title,
};
use crate::model::{
    AgentEvidence, AgentKind, AgentState, AgentStatus, StatusSource, format_agent_kind,
    format_agent_state, format_status_source, rollup_status,
};
use crate::tmux::{self, PaneRow};

/// Poll interval while at least one agent is Working or Blocked. Matches the
/// cadence a spinner needs to look live.
const ACTIVE_INTERVAL: Duration = Duration::from_millis(300);
/// Poll interval when every pane is idle. Idle Claude panes need a
/// `capture-pane` each poll (its idle title is never conclusive), so backing off
/// here is most of the daemon's cost saved for the common case: a workspace
/// sitting still.
const QUIET_INTERVAL: Duration = Duration::from_millis(1000);
/// How many polls between checks that we are still the registered daemon.
const OWNERSHIP_CHECK_POLLS: u32 = 10;
/// A hook write older than this is treated as stale and passive detection takes
/// over. Generous, because a long agent turn legitimately emits nothing.
const HOOK_STALE_AFTER: u64 = 15 * 60;

/// Consecutive passive samples required to commit an active → Idle transition,
/// so one stray frame cannot flash a spurious "done" or reset the run timer.
/// At [`ACTIVE_INTERVAL`] this is roughly a one-second settle.
const IDLE_DEBOUNCE_POLLS: u32 = 4;
/// Consecutive passive samples required to commit Idle → active, so a stray
/// busy-looking frame cannot wipe a committed "done" or restart its timer. Kept
/// short so real work still shows up promptly.
const BUSY_DEBOUNCE_POLLS: u32 = 2;

/// `agent-mgr daemon [--once]`
pub fn cmd_daemon(args: &[&str]) -> i32 {
    if args.contains(&"--once") {
        return match poll_once_reporting() {
            Ok(report) => {
                print!("{report}");
                0
            }
            Err(err) => {
                eprintln!("agent-mgr daemon: {err}");
                1
            }
        };
    }

    match run() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("agent-mgr daemon: {err}");
            1
        }
    }
}

/// Start the daemon in the background unless a live one is already registered.
/// Cheap enough to call on every sidebar start.
///
/// Launched through `tmux run-shell -b` rather than as our own child so it
/// outlives the sidebar pane that happened to start it.
pub fn ensure_running() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let pid = registered_pid();
    if !pid.is_empty() && pid_is_our_daemon(&pid, &exe) {
        return;
    }
    let command = format!("{} daemon", tmux::shell_quote(&exe.to_string_lossy()));
    tmux::run_tmux_quiet(&["run-shell", "-b", &command]);
}

fn run() -> io::Result<()> {
    let pid = std::process::id().to_string();
    tmux::set_global_option(tmux::DAEMON_PID, &pid);

    let mut debounces: HashMap<String, Debounce> = HashMap::new();
    let mut ownership_countdown = 0;

    loop {
        // If another daemon registered itself, stand down rather than both of us
        // writing the same options.
        if ownership_countdown == 0 && registered_pid() != pid {
            break;
        }
        ownership_countdown = (ownership_countdown + 1) % OWNERSHIP_CHECK_POLLS;

        // A tmux failure means the server is going away; so are we.
        let Ok(outcome) = poll_once(&mut debounces) else {
            break;
        };

        thread::sleep(if outcome.any_active {
            ACTIVE_INTERVAL
        } else {
            QUIET_INTERVAL
        });
    }

    // Only clear the registration if it is still ours, so we don't wipe the
    // successor that displaced us.
    if registered_pid() == pid {
        tmux::unset_global_option(tmux::DAEMON_PID);
    }
    Ok(())
}

/// What one poll pass observed, used to pick the next interval.
struct PollOutcome {
    any_active: bool,
}

/// Run one pass and render a TSV report of the resolved state. Used by
/// `daemon --once` to debug detection without opening the TUI.
fn poll_once_reporting() -> io::Result<String> {
    let mut debounces = HashMap::new();
    let rows = reconcile(&mut debounces)?;

    let mut report = String::from("pane\tsession\twindow\tagent\tstate\tsource\tseen\tcommand\n");
    for (row, status) in &rows {
        report.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            row.pane.pane_id,
            row.session_name,
            row.window_name,
            format_agent_kind(status.agent),
            format_agent_state(status.state),
            format_status_source(status.source),
            if status.seen { "1" } else { "0" },
            row.pane.current_command,
        ));
    }
    Ok(report)
}

fn poll_once(debounces: &mut HashMap<String, Debounce>) -> io::Result<PollOutcome> {
    let resolved = reconcile(debounces)?;
    let any_active = resolved
        .iter()
        .any(|(_, status)| status.state.is_active());

    write_window_icons(&resolved)?;
    Ok(PollOutcome { any_active })
}

/// The core pass: read every pane, resolve its status, persist what changed.
///
/// Returns the rows paired with their newly committed status so callers can
/// report on or roll up the same snapshot they just wrote.
fn reconcile(
    debounces: &mut HashMap<String, Debounce>,
) -> io::Result<Vec<(PaneRow, AgentStatus)>> {
    let rows = tmux::list_panes()?;
    let now = tmux::unix_timestamp();
    let live: HashSet<&str> = rows.iter().map(|row| row.pane.pane_id.as_str()).collect();

    // Built lazily: only needed once a pane's foreground command stops looking
    // like an agent, or a pane claims hook state, so a workspace of plain shells
    // never pays for a `ps`.
    let mut processes: Option<ProcessTree> = None;
    let mut resolved = Vec::new();

    for row in tmux::unique_by_pane(&rows) {
        if row.is_sidebar() {
            continue;
        }
        let previous = &row.pane.status;
        let reading = read_pane(row, &mut processes);

        let next = match reading {
            None => {
                debounces.remove(&row.pane.pane_id);
                AgentStatus::unknown()
            }
            Some(Reading {
                agent,
                state,
                source: StatusSource::Hook,
            }) => {
                // A hook write is an assertion; commit it without debouncing.
                // Drop any pending passive candidate so it can't leak across.
                debounces.remove(&row.pane.pane_id);
                stabilize(previous, agent, state, StatusSource::Hook, now)
            }
            Some(Reading {
                agent,
                state,
                source: StatusSource::Passive,
            }) => {
                let debounce = debounces
                    .entry(row.pane.pane_id.clone())
                    .or_insert_with(|| Debounce::new(state));
                debounce_reading(previous, agent, state, debounce, now)
            }
        };

        write_status(&row.pane.pane_id, previous, &next)?;
        resolved.push((row.clone(), next));
    }

    debounces.retain(|pane_id, _| live.contains(pane_id.as_str()));
    Ok(resolved)
}

/// One pane's raw reading, before debouncing or timer/seen bookkeeping.
struct Reading {
    agent: AgentKind,
    state: AgentState,
    source: StatusSource,
}

/// Resolve a single pane. `None` means "no agent here".
fn read_pane(row: &PaneRow, processes: &mut Option<ProcessTree>) -> Option<Reading> {
    let passive_agent = agent_from_process_name(&row.pane.current_command);

    // A hook-reporting pane whose agent process is gone was killed without
    // getting to run SessionEnd. Sweep its stale detail and fall through to
    // passive so it doesn't sit at "running" forever.
    if row.hook.present() {
        let tree = processes.get_or_insert_with(ProcessTree::snapshot);
        let alive = passive_agent.is_some()
            || tree.unavailable()
            || tree.has_agent_under(row.pane.pane_pid);
        if !alive {
            tmux::clear_hook_state(&row.pane.pane_id);
        } else if let Some(state) = fresh_hook_state(row) {
            return Some(Reading {
                agent: row.hook.agent.or(passive_agent)?,
                state,
                source: StatusSource::Hook,
            });
        }
    }

    let agent = match passive_agent {
        Some(agent) => agent,
        // The foreground command no longer looks like an agent. Keep the
        // previously detected agent only while one is genuinely still running
        // under this pane — it may have spawned a foreground child. If `ps`
        // can't be read this poll, keep it too: a transient failure must not
        // drop a live agent to unknown.
        None => {
            let previous = row.pane.status.agent?;
            let tree = processes.get_or_insert_with(ProcessTree::snapshot);
            if tree.unavailable() || tree.has_agent_under(row.pane.pane_pid) {
                previous
            } else {
                return None;
            }
        }
    };

    Some(Reading {
        agent,
        state: passive_state(agent, row),
        source: StatusSource::Passive,
    })
}

/// The hook's state, if it is recent enough to trust.
///
/// A very old write means the hooks were configured once and the agent has since
/// been restarted without them, or the pane was reused; either way passive
/// detection is the better answer.
fn fresh_hook_state(row: &PaneRow) -> Option<AgentState> {
    let state = row.hook.state?;
    match row.hook.updated {
        Some(updated) if tmux::unix_timestamp().saturating_sub(updated) > HOOK_STALE_AFTER => None,
        _ => Some(state),
    }
}

/// Read the pane's state passively, preferring the title so we only pay for a
/// `capture-pane` when the title is inconclusive.
fn passive_state(agent: AgentKind, row: &PaneRow) -> AgentState {
    if let Some(state) = state_from_title(agent, &row.pane.title) {
        return state;
    }
    let evidence = AgentEvidence {
        screen_tail: capture_tail(&row.pane.pane_id, SCREEN_TAIL_LINES),
        osc_title: row.pane.title.clone(),
    };
    state_from_evidence(agent, &evidence)
}

/// Capture the tail of a pane's visible screen as plain text.
///
/// Deliberately without `-e`: detection matches literal text and glyphs (the `❯`
/// menu cursor above all), and interleaved ANSI escapes would split them.
fn capture_tail(pane_id: &str, lines: usize) -> String {
    tmux::run_tmux(&[
        "capture-pane",
        "-pJ",
        "-t",
        pane_id,
        "-S",
        &format!("-{lines}"),
    ])
    .unwrap_or_default()
}

// ─── debounce ────────────────────────────────────────────────────────

/// A pane's pending passive transition, carried across polls in memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Debounce {
    candidate: AgentState,
    count: u32,
}

impl Debounce {
    fn new(state: AgentState) -> Self {
        Self {
            candidate: state,
            count: 0,
        }
    }
}

/// Polls a differing passive sample must persist before it is committed.
///
/// Both directions across the idle boundary are debounced, so one noisy frame —
/// a single-frame spinner, a transiently menu-shaped line — can neither flash a
/// premature "done" nor re-arm one that has settled. Everything else (first
/// detection out of Unknown, Working↔Blocked, anything involving Error) commits
/// on the first sample, because those readings are unambiguous and delaying them
/// only makes the sidebar feel slow.
fn debounce_threshold(committed: AgentState, raw: AgentState) -> u32 {
    use AgentState::{Blocked, Idle, Working};
    match (committed, raw) {
        (Working | Blocked, Idle) => IDLE_DEBOUNCE_POLLS,
        (Idle, Working | Blocked) => BUSY_DEBOUNCE_POLLS,
        _ => 1,
    }
}

/// Hold a passive sample until it has repeated [`debounce_threshold`] times,
/// then hand it to [`stabilize`]. Until it commits, the previously committed
/// status is preserved verbatim — including its run timer and seen flag.
fn debounce_reading(
    previous: &AgentStatus,
    agent: AgentKind,
    raw: AgentState,
    debounce: &mut Debounce,
    now: u64,
) -> AgentStatus {
    if raw == previous.state && previous.source == StatusSource::Passive {
        *debounce = Debounce::new(raw);
        return stabilize(previous, agent, raw, StatusSource::Passive, now);
    }

    if debounce.candidate == raw {
        debounce.count += 1;
    } else {
        debounce.candidate = raw;
        debounce.count = 1;
    }

    if debounce.count >= debounce_threshold(previous.state, raw) {
        debounce.count = 0;
        stabilize(previous, agent, raw, StatusSource::Passive, now)
    } else {
        AgentStatus {
            agent: Some(agent),
            source: StatusSource::Passive,
            ..previous.clone()
        }
    }
}

/// Apply the run timer and unread bookkeeping to a committed state.
///
/// - The timer starts when work begins and survives a Working↔Blocked flip, so
///   "running for 4m" means the whole turn rather than the current phase.
/// - `seen` drops to false exactly once when a run *ends* (into Idle or Error),
///   which is what makes "this finished while you were elsewhere" visible. It is
///   not re-armed while the state holds, so visiting the pane clears it for good.
fn stabilize(
    previous: &AgentStatus,
    agent: AgentKind,
    state: AgentState,
    source: StatusSource,
    now: u64,
) -> AgentStatus {
    let ended_a_run = previous.state.is_active();
    let seen = match state {
        AgentState::Working | AgentState::Blocked | AgentState::Unknown => true,
        // Only the transition marks it unread; a steady Idle/Error keeps
        // whatever the user has already acknowledged.
        AgentState::Idle | AgentState::Error if previous.state != state => !ended_a_run,
        AgentState::Idle | AgentState::Error => previous.seen,
    };
    let run_started_at = match state {
        AgentState::Working | AgentState::Blocked => previous.run_started_at.or(Some(now)),
        _ => None,
    };

    AgentStatus {
        agent: Some(agent),
        state,
        source,
        seen,
        run_started_at,
        // Detail belongs to the hooks that wrote it; carry it through untouched.
        ..previous.clone()
    }
}

// ─── persistence ─────────────────────────────────────────────────────

/// Write only the fields that actually changed.
///
/// Every `set-option` is a subprocess, and the steady state is "nothing changed",
/// so diffing here is the difference between a handful of writes per second and
/// several per pane per poll.
fn write_status(pane_id: &str, previous: &AgentStatus, next: &AgentStatus) -> io::Result<()> {
    let updates = status_updates(previous, next);
    if updates.is_empty() {
        return Ok(());
    }
    for (key, value) in &updates {
        tmux::set_pane_option(pane_id, key, value)?;
    }
    tmux::set_pane_option(pane_id, tmux::PANE_UPDATED, &tmux::unix_timestamp().to_string())
}

fn status_updates(previous: &AgentStatus, next: &AgentStatus) -> Vec<(&'static str, String)> {
    let mut updates = Vec::new();
    if previous.agent != next.agent {
        updates.push((tmux::PANE_AGENT, format_agent_kind(next.agent).to_owned()));
    }
    if previous.state != next.state {
        updates.push((tmux::PANE_STATE, format_agent_state(next.state).to_owned()));
    }
    if previous.source != next.source {
        updates.push((
            tmux::PANE_SOURCE,
            format_status_source(next.source).to_owned(),
        ));
    }
    if previous.seen != next.seen {
        updates.push((
            tmux::PANE_SEEN,
            if next.seen { "1" } else { "0" }.to_owned(),
        ));
    }
    if previous.run_started_at != next.run_started_at {
        updates.push((
            tmux::PANE_RUN_STARTED_AT,
            next.run_started_at
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ));
    }
    updates
}

/// Mark every pane in a window as seen. Called when the user navigates to it —
/// visiting a window is how you acknowledge "this finished".
pub fn mark_window_seen(window_id: &str) {
    let output =
        tmux::run_tmux(&["list-panes", "-t", window_id, "-F", "#{pane_id}"]).unwrap_or_default();
    for pane_id in output.lines().filter(|line| !line.trim().is_empty()) {
        tmux::set_pane_option_raw(pane_id, tmux::PANE_SEEN, "1");
    }
}

/// The tmux-format snippet shown in a window tab for a rolled-up status.
///
/// Written unconditionally; whether it appears is decided by `agent-mgr.conf`,
/// which only appends the `#{@agent_mgr_window_icon}` reference to
/// `window-status-format` when `@agent_mgr_tab_status` is on. That keeps the
/// daemon free of a per-poll option read.
fn window_icon(status: &AgentStatus) -> &'static str {
    if status.agent.is_none() {
        return "";
    }
    match status.state {
        AgentState::Error => " #[fg=red,bold]✕#[default]",
        AgentState::Blocked => " #[fg=red,bold]◉#[default]",
        AgentState::Working => " #[fg=yellow,bold]⠋#[default]",
        AgentState::Idle if !status.seen => " #[fg=cyan,bold]●#[default]",
        AgentState::Idle => " #[fg=green]✓#[default]",
        AgentState::Unknown => "",
    }
}

fn window_icons(resolved: &[(PaneRow, AgentStatus)]) -> HashMap<String, &'static str> {
    let mut by_window: HashMap<&str, Vec<&AgentStatus>> = HashMap::new();
    for (row, status) in resolved {
        by_window
            .entry(row.window_id.as_str())
            .or_default()
            .push(status);
    }

    by_window
        .into_iter()
        .map(|(window_id, statuses)| {
            let rolled = rollup_status(statuses.into_iter());
            (window_id.to_owned(), window_icon(&rolled))
        })
        .collect()
}

fn write_window_icons(resolved: &[(PaneRow, AgentStatus)]) -> io::Result<()> {
    let desired = window_icons(resolved);
    let current = tmux::tmux_output(&[
        "list-windows",
        "-a",
        "-F",
        &format!("#{{window_id}}\t#{{{}}}", tmux::WINDOW_ICON),
    ])?;

    for line in current.lines() {
        let Some((window_id, current_icon)) = line.split_once('\t') else {
            continue;
        };
        let wanted = desired.get(window_id).copied().unwrap_or_default();
        if wanted == current_icon {
            continue;
        }
        if wanted.is_empty() {
            tmux::unset_window_option(window_id, tmux::WINDOW_ICON)?;
        } else {
            tmux::set_window_option(window_id, tmux::WINDOW_ICON, wanted)?;
        }
    }
    Ok(())
}

// ─── singleton bookkeeping ───────────────────────────────────────────

fn registered_pid() -> String {
    tmux::run_tmux(&["show-option", "-gqv", tmux::DAEMON_PID])
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Whether `pid` is a live process that really is one of our daemons.
///
/// Checking the command line as well as liveness matters because pids are
/// recycled: a stale registration pointing at some unrelated new process would
/// otherwise keep every future daemon from ever starting.
fn pid_is_our_daemon(pid: &str, exe: &Path) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    let Ok(output) = Command::new("ps").args(["-p", pid, "-o", "command="]).output() else {
        // Without `ps` we cannot disprove it; assume it is ours rather than
        // starting a competing daemon.
        return true;
    };
    if !output.status.success() {
        return false;
    }
    let command = String::from_utf8_lossy(&output.stdout);
    command.contains(" daemon") && command.contains(exe.to_string_lossy().as_ref())
}

fn pid_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

// ─── process tree ────────────────────────────────────────────────────

/// A snapshot of the process table, used to tell "the agent exited" from "the
/// agent is running a foreground child".
pub struct ProcessTree {
    children: HashMap<u32, Vec<u32>>,
    agent_pids: HashSet<u32>,
}

impl ProcessTree {
    fn snapshot() -> Self {
        let output = Command::new("ps")
            .args(["-Ao", "pid=,ppid=,comm=,args="])
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        Self::parse(&output)
    }

    fn parse(output: &str) -> Self {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut agent_pids = HashSet::new();

        for line in output.lines() {
            let mut fields = line.split_whitespace();
            let (Some(pid), Some(ppid)) = (fields.next(), fields.next()) else {
                continue;
            };
            let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
                continue;
            };
            children.entry(ppid).or_default().push(pid);

            let comm = fields.next().unwrap_or_default();
            // Check the argv[0] path too: an agent launched through a wrapper can
            // report `node` as its comm while its command line names the agent.
            let argv0 = fields.next().unwrap_or_default().trim_matches('"');
            if agent_from_process_name(comm).is_some() || agent_from_process_name(argv0).is_some() {
                agent_pids.insert(pid);
            }
        }

        Self {
            children,
            agent_pids,
        }
    }

    /// `true` when the snapshot captured nothing, i.e. `ps` failed. Callers treat
    /// that as "unknown" rather than "no agents", so a transient failure never
    /// tears down live state.
    fn unavailable(&self) -> bool {
        self.children.is_empty()
    }

    /// `true` if `root` or any of its descendants is an agent process.
    fn has_agent_under(&self, root: Option<u32>) -> bool {
        let Some(root) = root else {
            return false;
        };
        let mut stack = vec![root];
        let mut seen = HashSet::new();
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            if self.agent_pids.contains(&pid) {
                return true;
            }
            if let Some(children) = self.children.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: AgentState, seen: bool, started: Option<u64>) -> AgentStatus {
        AgentStatus {
            agent: Some(AgentKind::Claude),
            state,
            source: StatusSource::Passive,
            seen,
            run_started_at: started,
            ..AgentStatus::default()
        }
    }

    // ─── stabilize: timers and the unread marker ──────────────────────

    #[test]
    fn run_timer_starts_on_work_and_survives_a_block() {
        let idle = status(AgentState::Idle, true, None);
        let working = stabilize(
            &idle,
            AgentKind::Claude,
            AgentState::Working,
            StatusSource::Passive,
            2000,
        );
        assert_eq!(working.run_started_at, Some(2000));

        // Waiting on the user is still part of the same turn.
        let blocked = stabilize(
            &working,
            AgentKind::Claude,
            AgentState::Blocked,
            StatusSource::Passive,
            2030,
        );
        assert_eq!(blocked.run_started_at, Some(2000));

        let done = stabilize(
            &blocked,
            AgentKind::Claude,
            AgentState::Idle,
            StatusSource::Passive,
            2040,
        );
        assert_eq!(done.run_started_at, None);
        assert!(!done.seen, "a finished run should be unread");
    }

    #[test]
    fn unread_marker_is_not_rearmed_once_acknowledged() {
        // The user visited the pane, so seen was set back to true by
        // mark_window_seen. A steady Idle must not flip it back.
        let acknowledged = status(AgentState::Idle, true, None);
        let next = stabilize(
            &acknowledged,
            AgentKind::Claude,
            AgentState::Idle,
            StatusSource::Passive,
            3000,
        );
        assert!(next.seen);
    }

    #[test]
    fn reaching_idle_from_idle_never_marks_unread() {
        // First detection lands straight on Idle: nothing finished, so there is
        // nothing to catch up on.
        let fresh = AgentStatus::unknown();
        let next = stabilize(
            &fresh,
            AgentKind::Claude,
            AgentState::Idle,
            StatusSource::Passive,
            10,
        );
        assert!(next.seen);
    }

    #[test]
    fn an_error_marks_the_pane_unread_once() {
        let working = status(AgentState::Working, true, Some(500));
        let failed = stabilize(
            &working,
            AgentKind::Claude,
            AgentState::Error,
            StatusSource::Hook,
            600,
        );
        assert_eq!(failed.state, AgentState::Error);
        assert!(!failed.seen, "a failure should demand attention");
        assert_eq!(failed.run_started_at, None);

        // Acknowledged, then still failing: stays acknowledged.
        let acknowledged = AgentStatus { seen: true, ..failed };
        let again = stabilize(
            &acknowledged,
            AgentKind::Claude,
            AgentState::Error,
            StatusSource::Hook,
            700,
        );
        assert!(again.seen);
    }

    #[test]
    fn stabilize_carries_hook_detail_through_untouched() {
        let hooked = AgentStatus {
            wait_reason: "permission".into(),
            subagents: vec!["Explore:a1".into()],
            ..status(AgentState::Blocked, true, Some(100))
        };
        let next = stabilize(
            &hooked,
            AgentKind::Claude,
            AgentState::Working,
            StatusSource::Hook,
            200,
        );
        assert_eq!(next.wait_reason, "permission");
        assert_eq!(next.subagents, vec!["Explore:a1"]);
    }

    // ─── debounce ─────────────────────────────────────────────────────

    #[test]
    fn debounce_threshold_is_directional() {
        assert_eq!(
            debounce_threshold(AgentState::Working, AgentState::Idle),
            IDLE_DEBOUNCE_POLLS
        );
        assert_eq!(
            debounce_threshold(AgentState::Idle, AgentState::Working),
            BUSY_DEBOUNCE_POLLS
        );
        // Fresh detection and busy↔busy are unambiguous; commit at once.
        assert_eq!(debounce_threshold(AgentState::Unknown, AgentState::Idle), 1);
        assert_eq!(
            debounce_threshold(AgentState::Working, AgentState::Blocked),
            1
        );
        assert_eq!(debounce_threshold(AgentState::Working, AgentState::Error), 1);
    }

    #[test]
    fn debounce_holds_working_until_the_idle_streak_completes() {
        let working = status(AgentState::Working, true, Some(1000));
        let mut debounce = Debounce::new(AgentState::Working);

        for poll in 1..IDLE_DEBOUNCE_POLLS {
            let held = debounce_reading(
                &working,
                AgentKind::Claude,
                AgentState::Idle,
                &mut debounce,
                1000 + u64::from(poll),
            );
            assert_eq!(held.state, AgentState::Working);
            assert_eq!(held.run_started_at, Some(1000), "timer must not reset");
            assert!(held.seen);
        }

        let done = debounce_reading(
            &working,
            AgentKind::Claude,
            AgentState::Idle,
            &mut debounce,
            2000,
        );
        assert_eq!(done.state, AgentState::Idle);
        assert!(!done.seen);
        assert_eq!(done.run_started_at, None);
    }

    #[test]
    fn debounce_absorbs_a_single_idle_blip_without_a_false_done() {
        let working = status(AgentState::Working, true, Some(500));
        let mut debounce = Debounce::new(AgentState::Working);

        let held = debounce_reading(
            &working,
            AgentKind::Claude,
            AgentState::Idle,
            &mut debounce,
            510,
        );
        assert_eq!(held.state, AgentState::Working);

        // Work resumes before the streak completes: no "done" is ever committed.
        let resumed = debounce_reading(
            &working,
            AgentKind::Claude,
            AgentState::Working,
            &mut debounce,
            520,
        );
        assert_eq!(resumed.state, AgentState::Working);
        assert_eq!(resumed.run_started_at, Some(500));
        assert!(resumed.seen);
    }

    #[test]
    fn debounce_ignores_a_lone_busy_sample_after_a_finished_run() {
        let done = status(AgentState::Idle, false, None);
        let mut debounce = Debounce::new(AgentState::Idle);

        let held = debounce_reading(
            &done,
            AgentKind::Claude,
            AgentState::Working,
            &mut debounce,
            3000,
        );
        assert_eq!(held.state, AgentState::Idle);
        assert!(!held.seen, "the unread marker must survive one stray frame");

        // Sustained work does commit, and starts a fresh run.
        assert_eq!(BUSY_DEBOUNCE_POLLS, 2);
        let working = debounce_reading(
            &done,
            AgentKind::Claude,
            AgentState::Working,
            &mut debounce,
            3005,
        );
        assert_eq!(working.state, AgentState::Working);
        assert_eq!(working.run_started_at, Some(3005));
    }

    #[test]
    fn a_passive_sample_matching_a_hook_committed_state_still_reruns_stabilize() {
        // Hooks stopped reporting and passive took over while reading the same
        // state. The source must flip so the row stops claiming hook accuracy.
        let hooked = AgentStatus {
            source: StatusSource::Hook,
            ..status(AgentState::Working, true, Some(10))
        };
        let mut debounce = Debounce::new(AgentState::Working);
        let next = debounce_reading(
            &hooked,
            AgentKind::Claude,
            AgentState::Working,
            &mut debounce,
            20,
        );
        assert_eq!(next.source, StatusSource::Passive);
        assert_eq!(next.run_started_at, Some(10), "the run is the same run");
    }

    // ─── diff-only writes ─────────────────────────────────────────────

    #[test]
    fn unchanged_status_writes_nothing() {
        let current = status(AgentState::Working, true, Some(1000));
        assert!(status_updates(&current, &current).is_empty());
    }

    #[test]
    fn only_changed_fields_are_written() {
        let working = status(AgentState::Working, true, Some(1000));
        let done = status(AgentState::Idle, false, None);
        assert_eq!(
            status_updates(&working, &done),
            vec![
                (tmux::PANE_STATE, "idle".to_owned()),
                (tmux::PANE_SEEN, "0".to_owned()),
                (tmux::PANE_RUN_STARTED_AT, String::new()),
            ]
        );

        let as_codex = AgentStatus {
            agent: Some(AgentKind::Codex),
            ..working.clone()
        };
        assert_eq!(
            status_updates(&working, &as_codex),
            vec![(tmux::PANE_AGENT, "codex".to_owned())]
        );
    }

    #[test]
    fn hook_detail_changes_alone_do_not_trigger_a_status_write() {
        // Hooks own those keys and already wrote them; rewriting from here would
        // be a redundant subprocess every poll.
        let base = status(AgentState::Working, true, Some(1000));
        let with_detail = AgentStatus {
            wait_reason: "permission".into(),
            ..base.clone()
        };
        assert!(status_updates(&base, &with_detail).is_empty());
    }

    // ─── window icons ─────────────────────────────────────────────────

    #[test]
    fn a_window_with_no_agent_gets_no_icon() {
        assert_eq!(window_icon(&AgentStatus::unknown()), "");
    }

    #[test]
    fn window_icons_reflect_the_most_urgent_pane() {
        assert_eq!(
            window_icon(&status(AgentState::Blocked, true, Some(1))),
            " #[fg=red,bold]◉#[default]"
        );
        assert_eq!(
            window_icon(&status(AgentState::Idle, false, None)),
            " #[fg=cyan,bold]●#[default]"
        );
        assert_eq!(
            window_icon(&status(AgentState::Idle, true, None)),
            " #[fg=green]✓#[default]"
        );
        assert_eq!(
            window_icon(&status(AgentState::Error, true, None)),
            " #[fg=red,bold]✕#[default]"
        );
    }

    // ─── process tree ─────────────────────────────────────────────────

    #[test]
    fn process_tree_tells_a_live_agent_from_an_exited_one() {
        // pane shell 100 → claude 200 → its bash subprocess 300.
        let running = ProcessTree::parse("100 1 zsh zsh\n200 100 claude claude\n300 200 bash bash\n");
        assert!(running.has_agent_under(Some(100)));
        assert!(running.has_agent_under(Some(200)));

        // The same pane once Claude has exited.
        let exited = ProcessTree::parse("100 1 zsh zsh\n400 100 nvim nvim\n");
        assert!(!exited.has_agent_under(Some(100)));
        assert!(!exited.has_agent_under(None));
    }

    #[test]
    fn process_tree_matches_the_versioned_native_binary() {
        let versioned = ProcessTree::parse(
            "10 1 zsh -zsh\n11 10 2.1.197 /home/me/.local/share/claude/versions/2.1.197\n",
        );
        assert!(versioned.has_agent_under(Some(10)));
    }

    #[test]
    fn process_tree_matches_an_agent_behind_a_node_wrapper() {
        // comm says `node`; only argv[0] names the agent.
        let wrapped = ProcessTree::parse("10 1 zsh zsh\n11 10 node /usr/local/bin/codex\n");
        assert!(wrapped.has_agent_under(Some(10)));
    }

    #[test]
    fn an_empty_process_tree_means_ps_is_unavailable_not_agentless() {
        assert!(ProcessTree::parse("").unavailable());
        assert!(!ProcessTree::parse("1 0 init init\n").unavailable());
    }
}
