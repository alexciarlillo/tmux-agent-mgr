//! Everything that talks to tmux.

pub mod commands;
pub mod options;
pub mod query;

pub use commands::{
    display_message, q, run_tmux, run_tmux_quiet, set_global_option, set_pane_option,
    set_pane_option_raw, set_window_option, shell_quote, tmux_output, unix_timestamp,
    unset_global_option, unset_pane_option_raw, unset_window_option,
};
pub use options::*;
pub use query::{
    PaneRow, apply_session_order, group_sessions, list_panes, persist_session_order, session_order,
    unique_by_pane,
};
