//! Agent hook ingest: `agent-mgr hook <agent> <event>`, fed by `hook.sh`.
//!
//! Hooks are the second of the two status sources (see [`crate::model`]). They
//! carry what passive detection can never see — permission mode, why a pane is
//! blocked, live subagents, task progress — because the agent asserts it rather
//! than us inferring it from a screen.
//!
//! ## Shape of this module
//!
//! Deciding *what* to write is a pure function of the event, its payload and the
//! pane's current hook-owned options: [`claude::plan`] returns a list of
//! [`Write`]s and touches nothing. [`apply`] is the only part that talks to tmux.
//! That split is what makes the whole mapping testable without a tmux server, and
//! it matches how the rest of the crate separates decisions from I/O.
//!
//! ## Two rules that hold for every event
//!
//! 1. **We only ever write the hook-owned namespace** ([`tmux::HOOK_OWNED_PANE_OPTIONS`]).
//!    Resolved status stays the daemon's to write, so the two writers cannot
//!    clobber each other and stale hook detail can be swept without disturbing
//!    passive state.
//! 2. **A hook never fails.** Every path returns 0 and errors are swallowed: a
//!    non-zero exit or a message on stderr surfaces inside the user's agent
//!    session, and a monitoring plugin has no business doing that.

pub mod claude;

use std::io::Read;

use serde_json::Value;

use crate::tmux;

/// One mutation of this pane's hook-owned options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Write {
    Set(&'static str, String),
    Unset(&'static str),
    /// Clear every hook-owned option: the agent that owned this pane is gone.
    Sweep,
}

/// The pane state a handler has to read before it can decide what to write.
///
/// Only the genuinely stateful fields are here. Everything else an event needs is
/// in its own payload, so this is one tmux read per hook fire rather than one per
/// field.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PaneState {
    /// Raw `@agent_mgr_subagents`: comma-separated `Type:id` entries.
    pub subagents: String,
    pub task_done: u32,
    pub task_total: u32,
}

/// `agent-mgr hook <agent> <event>`
///
/// Always exits 0. An unknown agent, an unknown event, a pane we cannot identify
/// and malformed JSON are all ordinary outcomes — the hook is registered globally
/// in the user's agent config and will fire in situations we have nothing to say
/// about, starting with "not running inside tmux at all".
pub fn cmd_hook(args: &[&str]) -> i32 {
    let (Some(agent), Some(event)) = (args.first(), args.get(1)) else {
        return 0;
    };
    // Claude Code is the only agent with a hook interface we consume. Codex is
    // supported by passive detection only, so its name is accepted and ignored
    // rather than treated as an error.
    if *agent != claude::AGENT {
        return 0;
    }
    let Some(event) = claude::Event::from_cli_name(event) else {
        return 0;
    };

    // Hooks run as children of the agent process, so they inherit its TMUX_PANE.
    // Without one there is no pane to attribute anything to.
    let pane = std::env::var("TMUX_PANE").unwrap_or_default();
    if pane.is_empty() {
        return 0;
    }

    let payload = read_stdin_json();
    let state = read_pane_state(&pane);
    apply(&pane, &claude::plan(event, &payload, &state, tmux::unix_timestamp()));
    0
}

/// Read the whole payload from stdin. A body that is missing or not JSON becomes
/// `null`, which every field lookup treats as absent — an event with no usable
/// payload still carries the fact that it fired, and that is worth recording.
fn read_stdin_json() -> Value {
    let mut buffer = String::new();
    if std::io::stdin().read_to_string(&mut buffer).is_err() {
        return Value::Null;
    }
    serde_json::from_str(&buffer).unwrap_or(Value::Null)
}

/// The pane's current subagent list and task counters.
///
/// One `display-message` expanding all three at once. Deliberately not
/// [`tmux::display_message`], which trims the whole output: a leading empty field
/// would then vanish and shift every value after it.
fn read_pane_state(pane: &str) -> PaneState {
    let format = format!(
        "#{{{}}}\t#{{{}}}\t#{{{}}}",
        tmux::PANE_SUBAGENTS,
        tmux::PANE_TASK_DONE,
        tmux::PANE_TASK_TOTAL
    );
    let raw = tmux::run_tmux(&["display-message", "-p", "-t", pane, &format]).unwrap_or_default();
    parse_pane_state(&raw)
}

fn parse_pane_state(raw: &str) -> PaneState {
    let fields: Vec<&str> = raw.trim_end_matches('\n').split('\t').collect();
    let field = |index: usize| fields.get(index).copied().unwrap_or_default().trim();
    PaneState {
        subagents: field(0).to_owned(),
        task_done: field(1).parse().unwrap_or(0),
        task_total: field(2).parse().unwrap_or(0),
    }
}

/// Perform the planned writes, in order. Failures are ignored: by the time a
/// `Stop` hook fires the pane may already be gone.
fn apply(pane: &str, writes: &[Write]) {
    for write in writes {
        match write {
            Write::Set(key, value) => tmux::set_pane_option_raw(pane, key, value),
            Write::Unset(key) => tmux::unset_pane_option_raw(pane, key),
            Write::Sweep => tmux::clear_hook_state(pane),
        }
    }
}

// ─── payload helpers ─────────────────────────────────────────────────

/// A string field, or `""` when absent or not a string.
fn json_str<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `true` when the key exists at all, so "reported as empty" can be told from
/// "not reported by this event". Only [`claude::Event::UserPromptSubmit`] and
/// friends carry `permission_mode`; an event that omits it must leave the
/// recorded mode alone rather than clearing it.
fn has_field(payload: &Value, key: &str) -> bool {
    payload.get(key).is_some_and(|value| !value.is_null())
}

/// Flatten a payload value into something safe to store in a tmux option.
///
/// Newlines and control characters are what actually matter: every option we
/// write is later read back as one field of one line of `list-panes -F` output,
/// so an embedded newline would truncate the row and desync every field after it.
/// `limit` bounds the width in chars — a 4 kB assistant message has no business
/// in a 24-column sidebar.
fn sanitize(value: &str, limit: usize) -> String {
    let flattened: String = value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let collapsed = flattened.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        return collapsed;
    }
    collapsed.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_state_survives_a_leading_empty_field() {
        // Regression guard: trimming the whole `display-message` output would
        // drop the empty subagent field and read "1" as the subagent list.
        let state = parse_pane_state("\t1\t3\n");
        assert_eq!(
            state,
            PaneState {
                subagents: String::new(),
                task_done: 1,
                task_total: 3,
            }
        );
    }

    #[test]
    fn pane_state_of_a_pane_with_nothing_set_is_all_defaults() {
        assert_eq!(parse_pane_state("\t\t\n"), PaneState::default());
        assert_eq!(parse_pane_state(""), PaneState::default());
    }

    #[test]
    fn pane_state_reads_a_populated_pane() {
        let state = parse_pane_state("Explore:a1,Plan:b2\t2\t5\n");
        assert_eq!(state.subagents, "Explore:a1,Plan:b2");
        assert_eq!((state.task_done, state.task_total), (2, 5));
    }

    #[test]
    fn sanitize_flattens_newlines_that_would_break_the_row_encoding() {
        assert_eq!(sanitize("two\nlines\there", 80), "two lines here");
    }

    #[test]
    fn sanitize_truncates_by_chars_not_bytes() {
        assert_eq!(sanitize("héllo wörld", 5), "héllo");
    }

    #[test]
    fn json_str_treats_a_missing_or_wrong_typed_field_as_empty() {
        let payload = serde_json::json!({"cwd": "/repo", "count": 3});
        assert_eq!(json_str(&payload, "cwd"), "/repo");
        assert_eq!(json_str(&payload, "count"), "");
        assert_eq!(json_str(&payload, "nope"), "");
    }

    #[test]
    fn has_field_distinguishes_absent_from_empty() {
        let payload = serde_json::json!({"permission_mode": "", "other": null});
        assert!(has_field(&payload, "permission_mode"));
        assert!(!has_field(&payload, "other"), "explicit null is not a value");
        assert!(!has_field(&payload, "missing"));
    }
}
