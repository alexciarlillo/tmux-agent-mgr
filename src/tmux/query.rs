//! Reading the whole tmux world in one subprocess call.
//!
//! A single `tmux list-panes -a -F …` gives us every session, window and pane
//! plus all our own pane options. Doing it any other way costs one subprocess
//! per pane, which is what makes a naive sidebar feel sluggish and forces a
//! faster poll to compensate.
//!
//! Every field is wrapped in `#{q:…}` and split with
//! [`split_fields`](super::commands::split_fields), so a pipe inside a window
//! name or path cannot shift the columns.

use crate::model::{
    AgentKind, AgentState, AgentStatus, PaneInfo, PermissionMode, SessionGroup, TaskProgress,
    WindowInfo, parse_agent_kind, parse_agent_state, parse_hook_state, parse_status_source,
};

use super::commands::{q, run_tmux_quiet, split_fields, tmux_output};
use super::options::*;

/// Field order for [`pane_format`]. The consts and the format list must be
/// edited together; [`tests::field_indices_match_the_format_string`] enforces it.
mod field {
    pub const SESSION_NAME: usize = 0;
    pub const SESSION_ATTACHED: usize = 1;
    pub const WINDOW_ID: usize = 2;
    pub const WINDOW_INDEX: usize = 3;
    pub const WINDOW_NAME: usize = 4;
    pub const WINDOW_ACTIVE: usize = 5;
    pub const PANE_ID: usize = 6;
    pub const PANE_INDEX: usize = 7;
    pub const PANE_ACTIVE: usize = 8;
    pub const PANE_CURRENT_COMMAND: usize = 9;
    pub const PANE_CURRENT_PATH: usize = 10;
    pub const PANE_TITLE: usize = 11;
    pub const PANE_PID: usize = 12;
    pub const ROLE: usize = 13;
    pub const AGENT: usize = 14;
    pub const STATE: usize = 15;
    pub const SOURCE: usize = 16;
    pub const SEEN: usize = 17;
    pub const RUN_STARTED_AT: usize = 18;
    pub const HOOK_AGENT: usize = 19;
    pub const HOOK_STATE: usize = 20;
    pub const HOOK_UPDATED: usize = 21;
    pub const PERMISSION_MODE: usize = 22;
    pub const WAIT_REASON: usize = 23;
    pub const SUBAGENTS: usize = 24;
    pub const TASK_DONE: usize = 25;
    pub const TASK_TOTAL: usize = 26;
    pub const BG_CMD: usize = 27;
    pub const CWD: usize = 28;
    /// Number of fields a well-formed line must have.
    pub const COUNT: usize = 29;
}

const DELIMITER: char = '|';

fn format_fields() -> Vec<String> {
    vec![
        q("session_name"),
        q("session_attached"),
        q("window_id"),
        q("window_index"),
        q("window_name"),
        q("window_active"),
        q("pane_id"),
        q("pane_index"),
        q("pane_active"),
        q("pane_current_command"),
        q("pane_current_path"),
        q("pane_title"),
        q("pane_pid"),
        q(PANE_ROLE),
        q(PANE_AGENT),
        q(PANE_STATE),
        q(PANE_SOURCE),
        q(PANE_SEEN),
        q(PANE_RUN_STARTED_AT),
        q(PANE_HOOK_AGENT),
        q(PANE_HOOK_STATE),
        q(PANE_HOOK_UPDATED),
        q(PANE_PERMISSION_MODE),
        q(PANE_WAIT_REASON),
        q(PANE_SUBAGENTS),
        q(PANE_TASK_DONE),
        q(PANE_TASK_TOTAL),
        q(PANE_BG_CMD),
        q(PANE_CWD),
    ]
}

fn pane_format() -> String {
    format_fields().join(&DELIMITER.to_string())
}

/// Raw facts written by the agent's own hooks, before reconciliation.
///
/// [`Self::state`] is `None` when hooks are not wired up for this pane, which is
/// exactly the signal the daemon uses to fall back to passive detection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HookFacts {
    pub agent: Option<AgentKind>,
    pub state: Option<AgentState>,
    pub updated: Option<u64>,
    pub permission_mode: PermissionMode,
    pub wait_reason: String,
    pub subagents: Vec<String>,
    pub task_done: Option<u32>,
    pub task_total: Option<u32>,
    pub bg_cmd: Option<String>,
    pub cwd: String,
}

impl HookFacts {
    /// `true` when the agent's hooks have reported anything for this pane.
    pub fn present(&self) -> bool {
        self.state.is_some() || self.agent.is_some()
    }
}

/// One pane, flattened together with its window and session context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneRow {
    pub session_name: String,
    pub session_attached: bool,
    pub window_id: String,
    pub window_index: String,
    pub window_name: String,
    pub window_active: bool,
    /// `@agent_mgr_pane_role`; `sidebar` for our own pane.
    pub role: String,
    pub pane: PaneInfo,
    pub hook: HookFacts,
}

impl PaneRow {
    pub fn is_sidebar(&self) -> bool {
        self.role == PANE_ROLE_SIDEBAR
    }
}

/// Query every pane on the server.
pub fn list_panes() -> std::io::Result<Vec<PaneRow>> {
    let format = pane_format();
    Ok(parse_pane_rows(&tmux_output(&[
        "list-panes", "-a", "-F", &format,
    ])?))
}

/// Parse `list-panes` output. Malformed lines are skipped rather than failing the
/// whole poll: one weird pane must not blind the sidebar to every other.
pub fn parse_pane_rows(output: &str) -> Vec<PaneRow> {
    output.lines().filter_map(parse_pane_row).collect()
}

fn parse_pane_row(line: &str) -> Option<PaneRow> {
    if line.trim().is_empty() {
        return None;
    }
    let f = split_fields(line, DELIMITER);
    if f.len() < field::COUNT {
        return None;
    }

    let hook = HookFacts {
        agent: parse_agent_kind(&f[field::HOOK_AGENT]),
        state: parse_hook_state(&f[field::HOOK_STATE]),
        updated: f[field::HOOK_UPDATED].parse().ok(),
        permission_mode: PermissionMode::from_label(&f[field::PERMISSION_MODE]),
        wait_reason: f[field::WAIT_REASON].clone(),
        subagents: parse_subagents(&f[field::SUBAGENTS]),
        task_done: f[field::TASK_DONE].parse().ok(),
        task_total: f[field::TASK_TOTAL].parse().ok(),
        bg_cmd: non_empty(&f[field::BG_CMD]),
        cwd: f[field::CWD].clone(),
    };

    // The resolved status the daemon last committed, with the hook-only detail
    // merged in so renderers read one struct instead of joining two.
    let status = AgentStatus {
        agent: parse_agent_kind(&f[field::AGENT]),
        state: parse_agent_state(&f[field::STATE]).unwrap_or(AgentState::Unknown),
        source: parse_status_source(&f[field::SOURCE]),
        // Absent means "nothing to catch up on"; only an explicit `0` marks a
        // finished-but-unread run, so a fresh pane never renders as unread.
        seen: f[field::SEEN] != "0",
        run_started_at: f[field::RUN_STARTED_AT].parse().ok(),
        permission_mode: hook.permission_mode,
        wait_reason: hook.wait_reason.clone(),
        subagents: hook.subagents.clone(),
        task_progress: task_progress(hook.task_done, hook.task_total),
        background_cmd: hook.bg_cmd.clone(),
    };

    // Prefer the agent's own cwd over the shell's: an agent that changed
    // directory should report the repo it is actually working in.
    let current_path = if hook.cwd.is_empty() {
        f[field::PANE_CURRENT_PATH].clone()
    } else {
        hook.cwd.clone()
    };

    Some(PaneRow {
        session_name: f[field::SESSION_NAME].clone(),
        session_attached: f[field::SESSION_ATTACHED] != "0",
        window_id: f[field::WINDOW_ID].clone(),
        window_index: f[field::WINDOW_INDEX].clone(),
        window_name: f[field::WINDOW_NAME].clone(),
        window_active: f[field::WINDOW_ACTIVE] == "1",
        role: f[field::ROLE].clone(),
        pane: PaneInfo {
            pane_id: f[field::PANE_ID].clone(),
            window_id: f[field::WINDOW_ID].clone(),
            pane_index: f[field::PANE_INDEX].clone(),
            pane_active: f[field::PANE_ACTIVE] == "1",
            current_command: f[field::PANE_CURRENT_COMMAND].clone(),
            current_path,
            title: f[field::PANE_TITLE].clone(),
            pane_pid: f[field::PANE_PID].parse().ok(),
            status,
            branch: String::new(),
            worktree: String::new(),
        },
        hook,
    })
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn parse_subagents(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A task list is only meaningful with a non-zero total, so `0/0` (no task list)
/// reads as absent rather than as "0 of 0 done".
fn task_progress(done: Option<u32>, total: Option<u32>) -> Option<TaskProgress> {
    let total = total.filter(|total| *total > 0)?;
    Some(TaskProgress {
        done: done.unwrap_or(0),
        total,
    })
}

/// Group flat pane rows into the session → window → pane tree the sidebar draws.
///
/// Our own sidebar panes are always excluded — they are furniture, not work.
/// With `agents_only`, panes with no detected agent are dropped too, and windows
/// and sessions left empty disappear with them.
pub fn group_sessions(rows: &[PaneRow], agents_only: bool) -> Vec<SessionGroup> {
    let mut sessions: Vec<SessionGroup> = Vec::new();

    for row in rows {
        if row.is_sidebar() {
            continue;
        }
        if agents_only && row.pane.status.agent.is_none() {
            continue;
        }

        let session = match sessions
            .iter_mut()
            .find(|session| session.session_name == row.session_name)
        {
            Some(session) => session,
            None => {
                sessions.push(SessionGroup {
                    session_name: row.session_name.clone(),
                    session_attached: row.session_attached,
                    windows: Vec::new(),
                });
                sessions.last_mut().expect("just pushed")
            }
        };

        match session
            .windows
            .iter_mut()
            .find(|window| window.window_id == row.window_id)
        {
            Some(window) => window.panes.push(row.pane.clone()),
            None => session.windows.push(WindowInfo {
                window_id: row.window_id.clone(),
                window_index: row.window_index.clone(),
                window_name: row.window_name.clone(),
                window_active: row.window_active,
                panes: vec![row.pane.clone()],
            }),
        }
    }

    sessions
}

/// Deduplicate rows by pane id, keeping the first occurrence.
///
/// Grouped sessions (`new-session -t`) share windows, so `list-panes -a` reports
/// the same pane once per session. Displaying it under both sessions is correct
/// for navigation, but the daemon must only reconcile and write it once.
/// Read the user's session order: session names, lowest rank first.
///
/// Sessions with no rank sort after every ranked one, keeping their tmux order
/// among themselves. That way a newly created session appears at the bottom rather
/// than silently jumping into the middle of an order you arranged.
pub fn session_order() -> Vec<String> {
    let format = format!("#{{session_name}}\t#{{{SESSION_ORDER}}}");
    let Ok(output) = tmux_output(&["list-sessions", "-F", &format]) else {
        return Vec::new();
    };
    parse_session_order(&output)
}

fn parse_session_order(output: &str) -> Vec<String> {
    let mut rows: Vec<(bool, usize, usize, String)> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(tmux_index, line)| {
            let (name, rank) = line.split_once('\t').unwrap_or((line, ""));
            let rank = rank.trim().parse::<usize>().ok();
            // The leading bool is the sort's first key: unranked last.
            (
                rank.is_none(),
                rank.unwrap_or(0),
                tmux_index,
                name.to_owned(),
            )
        })
        .collect();
    rows.sort();
    rows.into_iter().map(|(_, _, _, name)| name).collect()
}

/// Reorder `sessions` to match `order`, leaving anything unmentioned at the end.
///
/// Tolerant by design: the order list and the tree are read in separate tmux calls,
/// so a session can appear in one and not the other. Names missing from `order` keep
/// their relative position after the ordered ones.
pub fn apply_session_order(sessions: &mut [SessionGroup], order: &[String]) {
    sessions.sort_by_key(|session| {
        order
            .iter()
            .position(|name| *name == session.session_name)
            .unwrap_or(usize::MAX)
    });
}

/// Persist the current order, so it outlives this process and every other sidebar
/// picks it up.
pub fn persist_session_order(sessions: &[SessionGroup]) {
    for (rank, session) in sessions.iter().enumerate() {
        run_tmux_quiet(&[
            "set-option",
            "-q",
            "-t",
            &session.session_name,
            SESSION_ORDER,
            &rank.to_string(),
        ]);
    }
}

pub fn unique_by_pane(rows: &[PaneRow]) -> Vec<&PaneRow> {
    let mut seen = std::collections::HashSet::new();
    rows.iter()
        .filter(|row| seen.insert(row.pane.pane_id.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StatusSource;

    /// Build a `|`-joined line with every field defaulted, so each test only
    /// has to state the fields it cares about.
    fn line(overrides: &[(usize, &str)]) -> String {
        let mut fields = vec![""; field::COUNT];
        let defaults = [
            (field::SESSION_NAME, "work"),
            (field::SESSION_ATTACHED, "1"),
            (field::WINDOW_ID, "@1"),
            (field::WINDOW_INDEX, "1"),
            (field::WINDOW_NAME, "editor"),
            (field::WINDOW_ACTIVE, "1"),
            (field::PANE_ID, "%1"),
            (field::PANE_INDEX, "0"),
            (field::PANE_ACTIVE, "1"),
            (field::PANE_CURRENT_COMMAND, "zsh"),
            (field::PANE_CURRENT_PATH, "/tmp/project"),
            (field::PANE_PID, "1234"),
        ];
        for (index, value) in defaults {
            fields[index] = value;
        }
        for (index, value) in overrides {
            fields[*index] = value;
        }
        fields.join("|")
    }

    #[test]
    fn field_indices_match_the_format_string() {
        // The index consts are the only thing keeping parsing aligned with the
        // format; this catches an insertion that forgot to renumber.
        let fields = format_fields();
        assert_eq!(fields.len(), field::COUNT);
        assert_eq!(fields[field::SESSION_NAME], q("session_name"));
        assert_eq!(fields[field::PANE_ID], q("pane_id"));
        assert_eq!(fields[field::PANE_PID], q("pane_pid"));
        assert_eq!(fields[field::ROLE], q(PANE_ROLE));
        assert_eq!(fields[field::STATE], q(PANE_STATE));
        assert_eq!(fields[field::HOOK_STATE], q(PANE_HOOK_STATE));
        assert_eq!(fields[field::CWD], q(PANE_CWD));
    }

    #[test]
    fn parses_a_plain_pane_with_no_agent() {
        let rows = parse_pane_rows(&line(&[]));
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.pane.pane_id, "%1");
        assert_eq!(row.pane.pane_pid, Some(1234));
        assert!(row.pane.pane_active);
        assert_eq!(row.pane.status.agent, None);
        assert_eq!(row.pane.status.state, AgentState::Unknown);
        assert!(!row.hook.present());
    }

    #[test]
    fn absent_seen_field_does_not_read_as_unread() {
        // Regression guard: treating "" as unseen would make every fresh pane
        // render with the "finished, go look" marker.
        let rows = parse_pane_rows(&line(&[]));
        assert!(rows[0].pane.status.seen);

        let unread = parse_pane_rows(&line(&[(field::SEEN, "0"), (field::STATE, "idle")]));
        assert!(unread[0].pane.status.is_done());
    }

    #[test]
    fn parses_resolved_daemon_state() {
        let rows = parse_pane_rows(&line(&[
            (field::PANE_CURRENT_COMMAND, "claude"),
            (field::AGENT, "claude"),
            (field::STATE, "working"),
            (field::SOURCE, "passive"),
            (field::SEEN, "1"),
            (field::RUN_STARTED_AT, "1700000000"),
        ]));
        let status = &rows[0].pane.status;
        assert_eq!(status.agent, Some(AgentKind::Claude));
        assert_eq!(status.state, AgentState::Working);
        assert_eq!(status.source, StatusSource::Passive);
        assert_eq!(status.run_started_at, Some(1_700_000_000));
    }

    #[test]
    fn merges_hook_detail_into_the_resolved_status() {
        let rows = parse_pane_rows(&line(&[
            (field::AGENT, "claude"),
            (field::STATE, "blocked"),
            (field::SOURCE, "hook"),
            (field::HOOK_AGENT, "claude"),
            (field::HOOK_STATE, "waiting"),
            (field::HOOK_UPDATED, "1700000042"),
            (field::PERMISSION_MODE, "plan"),
            (field::WAIT_REASON, "permission"),
            (field::SUBAGENTS, "Explore:a1, Plan:b2"),
            (field::TASK_DONE, "3"),
            (field::TASK_TOTAL, "7"),
        ]));
        let row = &rows[0];
        assert!(row.hook.present());
        assert_eq!(row.hook.state, Some(AgentState::Blocked));
        assert_eq!(row.hook.updated, Some(1_700_000_042));

        let status = &row.pane.status;
        assert_eq!(status.source, StatusSource::Hook);
        assert_eq!(status.permission_mode, PermissionMode::Plan);
        assert_eq!(status.wait_reason, "permission");
        assert_eq!(status.subagents, vec!["Explore:a1", "Plan:b2"]);
        assert_eq!(status.task_progress, Some(TaskProgress { done: 3, total: 7 }));
        assert!(status.has_hook_detail());
    }

    #[test]
    fn zero_total_task_list_is_absent_not_zero_of_zero() {
        let rows = parse_pane_rows(&line(&[(field::TASK_DONE, "0"), (field::TASK_TOTAL, "0")]));
        assert_eq!(rows[0].pane.status.task_progress, None);
    }

    #[test]
    fn hook_cwd_overrides_the_shell_path() {
        let rows = parse_pane_rows(&line(&[
            (field::PANE_CURRENT_PATH, "/home/me"),
            (field::CWD, "/home/me/repo"),
        ]));
        assert_eq!(rows[0].pane.current_path, "/home/me/repo");

        // ...but an empty hook cwd must not blank the path.
        let rows = parse_pane_rows(&line(&[(field::PANE_CURRENT_PATH, "/home/me")]));
        assert_eq!(rows[0].pane.current_path, "/home/me");
    }

    #[test]
    fn pipes_inside_a_window_name_do_not_shift_fields() {
        // This is the whole reason for #{q:} + escape-aware splitting.
        let mut raw = line(&[(field::PANE_CURRENT_COMMAND, "nvim")]);
        raw = raw.replace("|editor|", r"|logs\|tail|");
        let rows = parse_pane_rows(&raw);
        assert_eq!(rows[0].window_name, "logs|tail");
        assert_eq!(rows[0].pane.current_command, "nvim");
        assert_eq!(rows[0].pane.pane_pid, Some(1234));
    }

    #[test]
    fn malformed_lines_are_skipped_without_losing_good_ones() {
        let output = format!("not-enough-fields\n{}\n\n", line(&[]));
        let rows = parse_pane_rows(&output);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn grouping_builds_the_session_window_pane_tree() {
        let output = [
            line(&[(field::PANE_ID, "%1")]),
            line(&[(field::PANE_ID, "%2"), (field::PANE_ACTIVE, "0")]),
            line(&[
                (field::PANE_ID, "%3"),
                (field::WINDOW_ID, "@2"),
                (field::WINDOW_NAME, "agents"),
            ]),
            line(&[
                (field::PANE_ID, "%4"),
                (field::SESSION_NAME, "ops"),
                (field::WINDOW_ID, "@9"),
            ]),
        ]
        .join("\n");

        let sessions = group_sessions(&parse_pane_rows(&output), false);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_name, "work");
        assert_eq!(sessions[0].windows.len(), 2);
        assert_eq!(sessions[0].windows[0].panes.len(), 2);
        assert_eq!(sessions[0].windows[1].window_name, "agents");
        assert_eq!(sessions[1].session_name, "ops");
    }

    #[test]
    fn grouping_always_excludes_our_own_sidebar_pane() {
        let output = [
            line(&[(field::PANE_ID, "%1"), (field::ROLE, PANE_ROLE_SIDEBAR)]),
            line(&[(field::PANE_ID, "%2")]),
        ]
        .join("\n");

        let sessions = group_sessions(&parse_pane_rows(&output), false);
        let panes: Vec<&str> = sessions[0].windows[0]
            .panes
            .iter()
            .map(|pane| pane.pane_id.as_str())
            .collect();
        assert_eq!(panes, ["%2"]);
    }

    #[test]
    fn agents_only_drops_non_agent_panes_and_the_windows_they_emptied() {
        let output = [
            line(&[(field::PANE_ID, "%1"), (field::AGENT, "claude")]),
            line(&[(field::PANE_ID, "%2")]),
            line(&[(field::PANE_ID, "%3"), (field::WINDOW_ID, "@2")]),
        ]
        .join("\n");
        let rows = parse_pane_rows(&output);

        let all = group_sessions(&rows, false);
        assert_eq!(all[0].windows.len(), 2);

        let agents = group_sessions(&rows, true);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].windows.len(), 1, "@2 had no agent, so it is gone");
        assert_eq!(agents[0].windows[0].panes[0].pane_id, "%1");
    }

    // ─── session order ────────────────────────────────────────────────

    fn group(name: &str) -> SessionGroup {
        SessionGroup {
            session_name: name.to_owned(),
            session_attached: true,
            windows: Vec::new(),
        }
    }

    #[test]
    fn ranked_sessions_sort_by_rank_not_by_tmux_order() {
        let order = parse_session_order("alpha\t2\nbeta\t0\ngamma\t1\n");
        assert_eq!(order, ["beta", "gamma", "alpha"]);
    }

    #[test]
    fn unranked_sessions_go_last_keeping_their_tmux_order() {
        // A session created after you arranged things should appear at the bottom,
        // not silently insert itself into the middle of your order.
        let order = parse_session_order("fresh\t\nalpha\t1\nalso_fresh\t\nbeta\t0\n");
        assert_eq!(order, ["beta", "alpha", "fresh", "also_fresh"]);
    }

    #[test]
    fn a_non_numeric_rank_counts_as_unranked() {
        let order = parse_session_order("bad\tnonsense\ngood\t0\n");
        assert_eq!(order, ["good", "bad"]);
    }

    #[test]
    fn blank_and_malformed_lines_are_skipped() {
        let order = parse_session_order("alpha\t0\n\n   \nnotabs\n");
        assert_eq!(order, ["alpha", "notabs"]);
    }

    #[test]
    fn applying_an_order_reorders_the_tree() {
        let mut sessions = vec![group("a"), group("b"), group("c")];
        apply_session_order(&mut sessions, &["c".to_owned(), "a".to_owned()]);
        let names: Vec<&str> = sessions
            .iter()
            .map(|session| session.session_name.as_str())
            .collect();
        // "b" is not in the order, so it lands after everything that is.
        assert_eq!(names, ["c", "a", "b"]);
    }

    #[test]
    fn applying_an_empty_or_unrelated_order_leaves_the_tree_alone() {
        // The order and the tree come from two separate tmux calls, so they can
        // legitimately disagree about which sessions exist.
        let original = vec![group("a"), group("b")];
        let mut sessions = original.clone();
        apply_session_order(&mut sessions, &[]);
        assert_eq!(sessions, original);
        apply_session_order(&mut sessions, &["nothing".to_owned()]);
        assert_eq!(sessions, original);
    }

    #[test]
    fn unique_by_pane_collapses_windows_shared_between_grouped_sessions() {
        // `new-session -t work` makes list-panes report each pane twice; the
        // daemon must reconcile it once.
        let output = [
            line(&[(field::PANE_ID, "%1"), (field::SESSION_NAME, "work")]),
            line(&[(field::PANE_ID, "%1"), (field::SESSION_NAME, "work-clone")]),
            line(&[(field::PANE_ID, "%2"), (field::SESSION_NAME, "work")]),
        ]
        .join("\n");
        let rows = parse_pane_rows(&output);

        assert_eq!(rows.len(), 3, "both sessions still render the shared pane");
        let unique = unique_by_pane(&rows);
        assert_eq!(unique.len(), 2);
        assert_eq!(unique[0].session_name, "work");
    }
}
