//! `agent-mgr` — one binary, several roles, dispatched on the first argument.
//!
//! | invocation | role |
//! |---|---|
//! | *(no args)* | the TUI itself, running inside a sidebar pane or a popup |
//! | `toggle <window-id> [path]` | create or kill the sidebar pane in one window |
//! | `toggle-all` | same, across every window |
//! | `resize <window-id>` | re-clamp the sidebar width after a window resize |
//! | `auto-close <window-id>` | close a window left holding only a sidebar |
//! | `daemon [--once]` | the status poller; `--once` prints one pass and exits |
//! | `hook <agent> <event>` | receive an agent hook payload on stdin |
//!
//! Keeping this in one binary is what lets `hook.sh` resolve a single path and
//! lets the TUI re-exec itself as the daemon without a second install step.

mod app;
mod daemon;
mod detect;
mod git;
mod model;
mod pane;
mod tmux;
mod ui;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

/// Set by the SIGUSR1 handler. tmux focus hooks poke us instead of us polling
/// faster, which is how the sidebar stays instant while drawing almost never.
static NEEDS_REFRESH: AtomicBool = AtomicBool::new(false);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();

    let code = match args.first().map(String::as_str) {
        Some("toggle") => pane::cmd_toggle(&rest),
        Some("toggle-all") => pane::cmd_toggle_all(),
        Some("resize") => pane::cmd_resize(&rest),
        Some("auto-close") => pane::cmd_auto_close(&rest),
        Some("daemon") => daemon::cmd_daemon(&rest),
        Some("--version" | "version") => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some(other) => {
            eprintln!("agent-mgr: unknown subcommand {other:?}");
            2
        }
        None => match run_tui() {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("agent-mgr: {err}");
                1
            }
        },
    };

    std::process::exit(code);
}

/// Restores the terminal on the way out, including on a panic unwind — a TUI
/// that dies leaving raw mode on takes the user's shell with it.
struct TuiSession;

impl TuiSession {
    fn enter(stdout: &mut io::Stdout) -> io::Result<Self> {
        enable_raw_mode()?;
        // Mouse capture keeps a wheel scroll driving our list instead of making
        // tmux scroll (and repaint) whatever sits behind us.
        if let Err(err) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(err);
        }
        Ok(Self)
    }
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

fn run_tui() -> io::Result<()> {
    let tmux_pane = std::env::var("TMUX_PANE").unwrap_or_default();
    if tmux_pane.is_empty() {
        return Err(io::Error::other(
            "TMUX_PANE is not set — run this inside tmux (prefix+e toggles the sidebar)",
        ));
    }

    install_sigusr1_handler();
    // Publish our pid so the focus hooks in agent-mgr.conf can find us.
    tmux::set_pane_option_raw(
        &tmux_pane,
        tmux::PANE_TUI_PID,
        &std::process::id().to_string(),
    );
    // One daemon serves every sidebar; this is a no-op when one already runs.
    daemon::ensure_running();

    let mut stdout = io::stdout();
    let _session = TuiSession::enter(&mut stdout)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    app::run(&mut terminal, tmux_pane, &NEEDS_REFRESH)
}

fn install_sigusr1_handler() {
    // SAFETY: `sigusr1_handler` only performs a relaxed atomic store, which is
    // async-signal-safe. SA_RESTART keeps our blocking reads from failing with
    // EINTR when the signal lands.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = sigusr1_handler as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut());
    }
}

extern "C" fn sigusr1_handler(_: libc::c_int) {
    NEEDS_REFRESH.store(true, Ordering::Relaxed);
}
