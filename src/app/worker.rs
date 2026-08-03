//! The background collector thread.
//!
//! Every tmux and git subprocess this plugin runs while the TUI is open happens
//! here. The UI thread does nothing but read from a channel, hash lines, and
//! occasionally draw — so a slow `git` on a network mount or a busy tmux server
//! can never stall input or leave a half-painted frame on screen.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::git::GitCache;
use crate::model::SessionGroup;
use crate::preview::{self, PanePreview};
use crate::tmux;

/// How long between collections. Everything urgent arrives via [`Worker::wake`]
/// instead, so this only has to be fast enough for changes nobody told us about
/// (a new window, an agent starting).
const INTERVAL: Duration = Duration::from_millis(1000);
/// Granularity at which a wake request is noticed while waiting.
const WAKE_SLICE: Duration = Duration::from_millis(50);

/// One collection: the tree, plus the preview for whichever window the UI asked
/// about. Sent together so the list and the preview on screen always describe the
/// same instant.
pub struct Snapshot {
    pub sessions: Vec<SessionGroup>,
    /// `None` unless a preview was requested. Carries the window id it belongs to,
    /// so a snapshot that arrives after the selection moved can be recognised as
    /// stale rather than drawn beside the wrong row.
    pub preview: Option<(String, Vec<PanePreview>)>,
    /// The pane tmux focus is on, as [`tmux::focused_pane`] resolves it. Read from
    /// the same `list-panes` the tree comes from, so it costs no extra subprocess and
    /// cannot disagree with the tree it arrives with.
    pub focused: Option<String>,
}

pub struct Worker {
    pub rx: Receiver<Snapshot>,
    /// Set to collect immediately instead of waiting out [`INTERVAL`]. Used by
    /// the SIGUSR1 path (tmux focus changed) and the manual refresh key.
    wake: Arc<AtomicBool>,
    /// Window to capture a preview of, or `None` for no preview at all — which is
    /// how the sidebar surface avoids paying for one.
    preview_target: Arc<Mutex<Option<String>>>,
}

impl Worker {
    /// Ask for a collection as soon as possible.
    pub fn request_refresh(&self) {
        self.wake.store(true, Ordering::Relaxed);
    }

    /// Point the preview at a window, or turn it off with `None`.
    ///
    /// Requests a refresh on an actual change so the preview catches up with a
    /// motion immediately, rather than showing the previous window for up to a
    /// full interval — the lag would read as the wrong preview, not a late one.
    pub fn set_preview_target(&self, window_id: Option<&str>) {
        let Ok(mut target) = self.preview_target.lock() else {
            return;
        };
        let changed = target.as_deref() != window_id;
        if changed {
            *target = window_id.map(str::to_owned);
            drop(target);
            self.request_refresh();
        }
    }
}

/// Start collecting. The thread stops on its own once the receiver is dropped.
///
/// `own_pane` is the sidebar's own pane id, used only to resolve which pane tmux
/// focus is on (see [`tmux::focused_pane`]); empty for a popup.
pub fn spawn(agents_only: bool, own_pane: String) -> Worker {
    let (tx, rx) = mpsc::channel();
    let wake = Arc::new(AtomicBool::new(false));
    let thread_wake = Arc::clone(&wake);
    let preview_target = Arc::new(Mutex::new(None));
    let thread_target = Arc::clone(&preview_target);

    thread::spawn(move || {
        let mut git = GitCache::new();
        loop {
            // A tmux failure here means the server is gone, and so is our pane;
            // there is nothing to report and nothing to retry.
            let Some((sessions, focused)) = collect(agents_only, &own_pane, &mut git) else {
                return;
            };
            // Read the target fresh each pass: the UI may have moved since the last
            // one, and capturing the window it has since left would be wasted work.
            let target: Option<String> = match thread_target.lock() {
                Ok(target) => target.clone(),
                // A poisoned lock means the UI thread panicked; there is no one left
                // to draw a preview for.
                Err(_) => None,
            };
            let preview = target.map(|window_id| {
                let panes = preview::capture_window(&window_id);
                (window_id, panes)
            });
            if tx
                .send(Snapshot {
                    sessions,
                    preview,
                    focused,
                })
                .is_err()
            {
                return;
            }
            wait(&thread_wake, INTERVAL);
        }
    });

    Worker {
        rx,
        wake,
        preview_target,
    }
}

/// Sleep up to `total`, returning early once a wake has been requested.
fn wait(wake: &AtomicBool, total: Duration) {
    let mut slept = Duration::ZERO;
    while slept < total {
        if wake.swap(false, Ordering::Relaxed) {
            return;
        }
        let slice = WAKE_SLICE.min(total - slept);
        thread::sleep(slice);
        slept += slice;
    }
}

/// One collection pass: read every pane, attach git context, group.
///
/// Returns the tree and the focused pane together — both come out of the same
/// `list-panes`, so they always describe one instant.
fn collect(
    agents_only: bool,
    own_pane: &str,
    git: &mut GitCache,
) -> Option<(Vec<SessionGroup>, Option<String>)> {
    let rows = tmux::list_panes().ok()?;
    let focused = tmux::focused_pane(&rows, own_pane);
    let mut sessions = tmux::group_sessions(&rows, agents_only);
    // One extra subprocess per pass to honour the user's session order. tmux itself
    // has no session ordering, so without this the list is alphabetical whatever the
    // user arranged.
    tmux::apply_session_order(&mut sessions, &tmux::session_order());

    let mut live_paths: Vec<&str> = Vec::new();
    for pane in sessions
        .iter_mut()
        .flat_map(|session| &mut session.windows)
        .flat_map(|window| &mut window.panes)
    {
        let info = git.get(&pane.current_path);
        pane.branch = info.branch;
        pane.worktree = info.worktree;
    }
    for session in &sessions {
        for window in &session.windows {
            for pane in &window.panes {
                live_paths.push(&pane.current_path);
            }
        }
    }
    git.retain_paths(&live_paths);

    Some((sessions, focused))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn wait_returns_early_when_a_refresh_is_requested() {
        let wake = AtomicBool::new(true);
        let start = Instant::now();
        wait(&wake, Duration::from_secs(5));
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "a pending wake must not be ignored for the full interval"
        );
    }

    #[test]
    fn wait_consumes_the_request_so_it_fires_once() {
        let wake = AtomicBool::new(true);
        wait(&wake, Duration::from_millis(1));
        assert!(!wake.load(Ordering::Relaxed));
    }

    #[test]
    fn wait_sleeps_out_a_quiet_interval() {
        let wake = AtomicBool::new(false);
        let start = Instant::now();
        wait(&wake, Duration::from_millis(120));
        assert!(start.elapsed() >= Duration::from_millis(100));
    }
}
