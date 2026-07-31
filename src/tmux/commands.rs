//! The tmux subprocess layer: running `tmux`, and the two error conventions the
//! rest of the crate uses.
//!
//! - [`run_tmux`] returns `Option<String>` for calls where failure is
//!   uninteresting (a pane vanished mid-poll, an option was never set).
//! - [`tmux_output`] returns `Result` for calls whose failure means the tmux
//!   server is gone, which is how the daemon knows to exit.

use std::io;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Run a tmux command, returning its stdout on success and `None` on any
/// failure. Use where a missing pane or unset option is an ordinary outcome.
pub fn run_tmux(args: &[&str]) -> Option<String> {
    let output = Command::new("tmux").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run a tmux command, surfacing failure. Use where failure means "the server
/// went away" and the caller should stop.
pub fn tmux_output(args: &[&str]) -> io::Result<String> {
    let output = Command::new("tmux").args(args).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "tmux {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Fire-and-forget tmux command.
pub fn run_tmux_quiet(args: &[&str]) {
    let _ = Command::new("tmux").args(args).output();
}

/// Expand a tmux format string in the context of a target (pane, window, or
/// session).
pub fn display_message(target: &str, format: &str) -> String {
    run_tmux(&["display-message", "-p", "-t", target, format])
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

pub fn set_pane_option(pane_id: &str, key: &str, value: &str) -> io::Result<()> {
    tmux_output(&["set-option", "-p", "-q", "-t", pane_id, key, value]).map(|_| ())
}

/// Set a pane option, ignoring failure. For hook handlers, where the pane may
/// already be gone and a hook must never surface an error into the agent.
pub fn set_pane_option_raw(pane_id: &str, key: &str, value: &str) {
    run_tmux_quiet(&["set-option", "-p", "-q", "-t", pane_id, key, value]);
}

pub fn unset_pane_option_raw(pane_id: &str, key: &str) {
    run_tmux_quiet(&["set-option", "-p", "-q", "-u", "-t", pane_id, key]);
}

pub fn set_window_option(window_id: &str, key: &str, value: &str) -> io::Result<()> {
    tmux_output(&["set-option", "-w", "-q", "-t", window_id, key, value]).map(|_| ())
}

pub fn unset_window_option(window_id: &str, key: &str) -> io::Result<()> {
    tmux_output(&["set-option", "-w", "-q", "-u", "-t", window_id, key]).map(|_| ())
}

pub fn set_global_option(key: &str, value: &str) {
    run_tmux_quiet(&["set-option", "-g", "-q", key, value]);
}

pub fn unset_global_option(key: &str) {
    run_tmux_quiet(&["set-option", "-g", "-q", "-u", key]);
}

pub fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Single-quote a value for embedding in a `tmux run-shell` command line.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Wrap a field reference in tmux's `#{q:…}` quoting so its value cannot
/// contain our field delimiter unescaped. Pairs with [`split_fields`], which
/// undoes the backslash escaping tmux applies.
pub fn q(field: &str) -> String {
    format!("#{{q:{field}}}")
}

/// Split one `list-panes -F` line on `delimiter`, honouring the backslash
/// escapes that `#{q:…}` introduces.
///
/// Without this, a window named `foo|bar` or a path containing a pipe would
/// shift every field after it and silently mis-assign values across the row.
pub fn split_fields(line: &str, delimiter: char) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut escaped = false;

    for ch in line.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            _ if ch == delimiter => fields.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    // A trailing lone backslash is literal, not the start of an escape.
    if escaped {
        current.push('\\');
    }
    fields.push(current);
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_fields_unescapes_an_escaped_delimiter() {
        assert_eq!(
            split_fields(r"one|two\|still-two|three", '|'),
            vec!["one", "two|still-two", "three"]
        );
    }

    #[test]
    fn split_fields_keeps_empty_fields_so_indices_stay_aligned() {
        // Unset options render as empty; dropping them would shift every
        // later field by one.
        assert_eq!(split_fields("a||c|", '|'), vec!["a", "", "c", ""]);
    }

    #[test]
    fn split_fields_treats_trailing_backslash_as_literal() {
        assert_eq!(split_fields(r"a|b\", '|'), vec!["a", r"b\"]);
    }

    #[test]
    fn split_fields_unescapes_backslashes_themselves() {
        assert_eq!(split_fields(r"a\\b|c", '|'), vec![r"a\b", "c"]);
    }

    #[test]
    fn shell_quote_survives_embedded_single_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn q_wraps_a_field_in_tmux_quoting() {
        assert_eq!(q("pane_current_path"), "#{q:pane_current_path}");
    }
}
