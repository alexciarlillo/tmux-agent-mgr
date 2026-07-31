//! Everything that talks to tmux.

pub mod commands;
pub mod options;
pub mod query;

pub use commands::{
    display_message, q, run_tmux, run_tmux_quiet, set_global_option, set_pane_option,
    set_pane_option_raw, set_window_option, shell_quote, tmux_output, unix_timestamp,
    unset_global_option, unset_window_option,
};
pub use options::*;
pub use query::{PaneRow, group_sessions, list_panes, unique_by_pane};
