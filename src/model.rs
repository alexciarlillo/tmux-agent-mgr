//! Core data types shared across the crate.
//!
//! Agent state reaches us from two places with different vocabularies, and this
//! module is where they merge:
//!
//! - **Passive detection** ([`crate::detect`]) reads the process table and the
//!   pane's visible screen. It works with zero setup but only distinguishes
//!   working / blocked / idle.
//! - **Agent hooks** ([`crate::hook`]) are pushed by the agent itself and carry
//!   detail passive detection can never see: permission mode, why a pane is
//!   blocked, live subagents, task progress.
//!
//! Both funnel into one [`AgentState`], and the daemon is the only writer of the
//! resolved status — see [`crate::daemon`] for the precedence rules.

/// Which agent CLI a pane is running.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AgentKind {
    Claude,
    Codex,
}

impl AgentKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// What an agent is doing, unified across both status sources.
///
/// Hook statuses map in as: `running` → [`Working`](Self::Working), `waiting` →
/// [`Blocked`](Self::Blocked), `background` → [`Working`](Self::Working) (with
/// [`AgentStatus::background_cmd`] set), `idle` → [`Idle`](Self::Idle),
/// `error` → [`Error`](Self::Error).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum AgentState {
    /// Actively producing output.
    Working,
    /// Stopped, waiting on the user (a permission prompt, a menu, a question).
    Blocked,
    /// At its prompt with nothing to do.
    Idle,
    /// The agent reported a failure. Hook-only; passive detection can't see it.
    Error,
    /// No agent in this pane, or state not yet determined.
    #[default]
    Unknown,
}

impl AgentState {
    /// `true` while the agent is doing something the user would expect to be
    /// timed and refreshed live.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Working | Self::Blocked)
    }
}

/// Where a pane's resolved state came from. Rendered as a subtle row hint so a
/// heuristic reading is never mistaken for one the agent asserted itself.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum StatusSource {
    /// Inferred from the process table and screen contents.
    #[default]
    Passive,
    /// Pushed by the agent's own hooks.
    Hook,
}

/// The agent's permission mode. Hook-only — passive detection cannot see it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum PermissionMode {
    #[default]
    Default,
    Plan,
    AcceptEdits,
    Auto,
    DontAsk,
    BypassPermissions,
    Defer,
}

impl PermissionMode {
    /// Parse the mode label as the agent reports it. Unknown values fall back to
    /// [`Default`](Self::Default) rather than erroring — a new upstream mode
    /// should degrade to "no badge", not break the row.
    pub fn from_label(value: &str) -> Self {
        match value {
            "plan" => Self::Plan,
            "acceptEdits" => Self::AcceptEdits,
            "auto" => Self::Auto,
            "dontAsk" => Self::DontAsk,
            "bypassPermissions" => Self::BypassPermissions,
            "defer" => Self::Defer,
            _ => Self::Default,
        }
    }

    /// Short badge shown beside the agent label. Empty for the default mode, so
    /// the common case costs no columns in a narrow sidebar.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::Plan => "plan",
            Self::AcceptEdits => "edit",
            Self::Auto => "auto",
            Self::DontAsk => "dontAsk",
            Self::BypassPermissions => "!",
            Self::Defer => "defer",
        }
    }
}

/// Task-list progress reported by `TaskCreated` / `TaskCompleted` hooks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TaskProgress {
    pub done: u32,
    pub total: u32,
}

/// Everything known about the agent in one pane.
///
/// The first five fields are written by the daemon and always present. The rest
/// only ever come from hooks, and stay empty/default under passive detection.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct AgentStatus {
    pub agent: Option<AgentKind>,
    pub state: AgentState,
    pub source: StatusSource,
    /// `false` once a run finishes until the user visits the pane: the "an agent
    /// finished and you haven't looked yet" marker.
    pub seen: bool,
    /// Epoch seconds when the current run began; drives the elapsed label.
    pub run_started_at: Option<u64>,

    pub permission_mode: PermissionMode,
    /// Why the pane is blocked (`permission`, `idle_prompt`, …).
    pub wait_reason: String,
    /// Currently-running subagent types, as reported by Subagent{Start,Stop}.
    pub subagents: Vec<String>,
    pub task_progress: Option<TaskProgress>,
    /// The most recent backgrounded shell command, when one is still alive.
    pub background_cmd: Option<String>,
}

impl AgentStatus {
    /// A pane with no agent in it.
    pub fn unknown() -> Self {
        Self {
            seen: true,
            ..Self::default()
        }
    }

    /// `true` when a run has finished and the user has not looked at it yet.
    pub fn is_done(&self) -> bool {
        self.state == AgentState::Idle && !self.seen
    }

    /// `true` when this status carries detail only hooks can provide, so the row
    /// renderer can skip the context lines entirely for passive panes.
    pub fn has_hook_detail(&self) -> bool {
        self.permission_mode != PermissionMode::Default
            || !self.wait_reason.is_empty()
            || !self.subagents.is_empty()
            || self.task_progress.is_some()
            || self.background_cmd.is_some()
    }
}

/// Evidence passed to passive state detection: everything we can observe about a
/// pane without the agent's cooperation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentEvidence {
    /// The tail of the pane's visible screen, plain text (no ANSI).
    pub screen_tail: String,
    /// The pane's OSC title, which both agents use to advertise activity.
    pub osc_title: String,
}

/// One tmux pane, joined with whatever agent status applies to it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneInfo {
    pub pane_id: String,
    pub window_id: String,
    pub pane_index: String,
    pub pane_active: bool,
    pub current_command: String,
    pub current_path: String,
    pub title: String,
    pub pane_pid: Option<u32>,
    pub status: AgentStatus,
    /// Git branch for [`Self::current_path`], filled in by [`crate::git`].
    pub branch: String,
    /// Worktree directory basename when the path is a linked git worktree.
    pub worktree: String,
}

/// One tmux window and its panes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowInfo {
    pub window_id: String,
    pub window_index: String,
    pub window_name: String,
    pub window_active: bool,
    pub panes: Vec<PaneInfo>,
}

/// One tmux session and its windows. The top level of the sidebar tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionGroup {
    pub session_name: String,
    pub session_attached: bool,
    pub windows: Vec<WindowInfo>,
}

// ─── option round-trip ───────────────────────────────────────────────
//
// The daemon persists resolved status into tmux pane options so every sidebar
// instance can read it from one `list-panes` call instead of re-deriving it.
// These are the only places those strings are produced or consumed.

pub fn parse_agent_kind(value: &str) -> Option<AgentKind> {
    match value {
        "claude" => Some(AgentKind::Claude),
        "codex" => Some(AgentKind::Codex),
        _ => None,
    }
}

pub fn format_agent_kind(agent: Option<AgentKind>) -> &'static str {
    match agent {
        Some(kind) => kind.label(),
        None => "",
    }
}

pub fn parse_agent_state(value: &str) -> Option<AgentState> {
    match value {
        "working" => Some(AgentState::Working),
        "blocked" => Some(AgentState::Blocked),
        "idle" => Some(AgentState::Idle),
        "error" => Some(AgentState::Error),
        "unknown" => Some(AgentState::Unknown),
        _ => None,
    }
}

pub fn format_agent_state(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "working",
        AgentState::Blocked => "blocked",
        AgentState::Idle => "idle",
        AgentState::Error => "error",
        AgentState::Unknown => "unknown",
    }
}

/// Parse the raw status label an agent hook writes, into our unified state.
///
/// Returns `None` for an unrecognized label so a future upstream status can't
/// silently masquerade as idle. `background` maps to
/// [`Working`](AgentState::Working) — a live background shell is still work in
/// progress — and the accompanying command lands in
/// [`AgentStatus::background_cmd`] instead.
pub fn parse_hook_state(value: &str) -> Option<AgentState> {
    match value {
        "running" => Some(AgentState::Working),
        "waiting" | "notification" => Some(AgentState::Blocked),
        "background" => Some(AgentState::Working),
        "idle" => Some(AgentState::Idle),
        "error" => Some(AgentState::Error),
        _ => None,
    }
}

pub fn parse_status_source(value: &str) -> StatusSource {
    match value {
        "hook" => StatusSource::Hook,
        _ => StatusSource::Passive,
    }
}

pub fn format_status_source(source: StatusSource) -> &'static str {
    match source {
        StatusSource::Hook => "hook",
        StatusSource::Passive => "passive",
    }
}

/// Rank a status for rolling several panes up into one window/session summary.
/// Higher wins. Error outranks Blocked: a failed run needs attention more
/// urgently than one merely waiting on input.
pub fn status_priority(status: &AgentStatus) -> u8 {
    match status.state {
        AgentState::Error => 6,
        AgentState::Blocked => 5,
        AgentState::Idle if !status.seen => 4,
        AgentState::Working => 3,
        AgentState::Idle => 2,
        AgentState::Unknown => 1,
    }
}

/// Collapse many pane statuses into the single most attention-worthy one.
pub fn rollup_status<'a>(statuses: impl Iterator<Item = &'a AgentStatus>) -> AgentStatus {
    let mut best = AgentStatus::unknown();
    let mut best_priority = 0;
    for status in statuses {
        let priority = status_priority(status);
        if priority > best_priority {
            best = status.clone();
            best_priority = priority;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_state_round_trips_through_option_strings() {
        for state in [
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Idle,
            AgentState::Error,
            AgentState::Unknown,
        ] {
            assert_eq!(parse_agent_state(format_agent_state(state)), Some(state));
        }
        assert_eq!(parse_agent_state("bogus"), None);
    }

    #[test]
    fn agent_kind_round_trips_and_empty_means_no_agent() {
        assert_eq!(parse_agent_kind("claude"), Some(AgentKind::Claude));
        assert_eq!(parse_agent_kind("codex"), Some(AgentKind::Codex));
        assert_eq!(parse_agent_kind(""), None);
        assert_eq!(format_agent_kind(None), "");
    }

    #[test]
    fn hook_labels_map_onto_unified_states() {
        assert_eq!(parse_hook_state("running"), Some(AgentState::Working));
        assert_eq!(parse_hook_state("waiting"), Some(AgentState::Blocked));
        assert_eq!(parse_hook_state("notification"), Some(AgentState::Blocked));
        // A live background shell is still work in progress.
        assert_eq!(parse_hook_state("background"), Some(AgentState::Working));
        assert_eq!(parse_hook_state("idle"), Some(AgentState::Idle));
        assert_eq!(parse_hook_state("error"), Some(AgentState::Error));
        // An unrecognized label must not silently read as idle.
        assert_eq!(parse_hook_state("teleporting"), None);
    }

    #[test]
    fn error_outranks_blocked_in_rollup() {
        let blocked = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Blocked,
            seen: true,
            ..AgentStatus::default()
        };
        let failed = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Error,
            seen: true,
            ..AgentStatus::default()
        };
        let rolled = rollup_status([&blocked, &failed].into_iter());
        assert_eq!(rolled.state, AgentState::Error);
    }

    #[test]
    fn unseen_idle_outranks_working_so_finished_runs_stay_visible() {
        let working = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            ..AgentStatus::default()
        };
        let done = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Idle,
            seen: false,
            ..AgentStatus::default()
        };
        assert!(done.is_done());
        let rolled = rollup_status([&working, &done].into_iter());
        assert!(rolled.is_done());
    }

    #[test]
    fn rollup_of_nothing_is_unknown_and_seen() {
        let rolled = rollup_status([].into_iter());
        assert_eq!(rolled.state, AgentState::Unknown);
        assert!(rolled.seen, "an empty rollup must not render as unread");
    }

    #[test]
    fn permission_mode_unknown_label_degrades_to_no_badge() {
        assert_eq!(PermissionMode::from_label("plan"), PermissionMode::Plan);
        assert_eq!(
            PermissionMode::from_label("someFutureMode"),
            PermissionMode::Default
        );
        assert_eq!(PermissionMode::Default.badge(), "");
    }

    #[test]
    fn has_hook_detail_is_false_for_a_purely_passive_status() {
        let passive = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            ..AgentStatus::default()
        };
        assert!(!passive.has_hook_detail());

        let hooked = AgentStatus {
            wait_reason: "permission".into(),
            ..passive
        };
        assert!(hooked.has_hook_detail());
    }
}
