//! Read-only git context for a pane's directory: the current branch, and the
//! worktree name when the path is a linked worktree.
//!
//! Deliberately minimal. This plugin shows you which branch an agent is working
//! on; it does not manage worktrees, fetch pull requests, or diff anything.
//!
//! Everything is cached with a TTL, including *negative* results. A workspace of
//! non-repo panes would otherwise spawn a `git` process per pane per poll for an
//! answer that is always "no".

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

/// How long a lookup is reused. Long enough that polling is nearly free, short
/// enough that a branch switch shows up while you are still looking at it.
const TTL: Duration = Duration::from_secs(5);

/// Branch and worktree for one directory. Both empty means "not a git repo", or
/// a repo with no commits yet.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GitInfo {
    /// Current branch, or a short commit hash when HEAD is detached.
    pub branch: String,
    /// Worktree name when this path is inside a linked worktree, else empty.
    pub worktree: String,
}

/// TTL cache over [`lookup`]. Lives on the worker thread, so no locking.
#[derive(Default)]
pub struct GitCache {
    entries: HashMap<String, (Instant, GitInfo)>,
}

impl GitCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up `path`, reusing a recent answer when there is one.
    pub fn get(&mut self, path: &str) -> GitInfo {
        self.get_at(path, Instant::now(), lookup)
    }

    /// Testable core: the clock and the resolver are injected.
    fn get_at(
        &mut self,
        path: &str,
        now: Instant,
        resolve: impl FnOnce(&str) -> GitInfo,
    ) -> GitInfo {
        if let Some((fetched_at, info)) = self.entries.get(path)
            && now.duration_since(*fetched_at) < TTL
        {
            return info.clone();
        }
        let info = resolve(path);
        self.entries.insert(path.to_owned(), (now, info.clone()));
        info
    }

    /// Forget paths that are no longer on screen, so the cache tracks the
    /// current pane set rather than growing for the life of the process.
    pub fn retain_paths(&mut self, live: &[&str]) {
        self.entries.retain(|path, _| live.contains(&path.as_str()));
    }
}

/// Ask git about a directory in a single subprocess.
///
/// `rev-parse` answers both questions at once: the absolute git dir (whose shape
/// reveals a linked worktree) and the branch name.
fn lookup(path: &str) -> GitInfo {
    if path.is_empty() {
        return GitInfo::default();
    }
    let Some(output) = git(path, &["rev-parse", "--absolute-git-dir", "--abbrev-ref", "HEAD"])
    else {
        return GitInfo::default();
    };
    parse_rev_parse(&output, || {
        // Detached HEAD: fall back to a short hash. Rare enough to be worth a
        // second call rather than always asking for both.
        git(path, &["rev-parse", "--short", "HEAD"]).unwrap_or_default()
    })
}

/// Parse `rev-parse --absolute-git-dir --abbrev-ref HEAD` output.
///
/// `detached` is only invoked when the branch reads back as literally `HEAD`,
/// which is how `--abbrev-ref` reports a detached checkout.
fn parse_rev_parse(output: &str, detached: impl FnOnce() -> String) -> GitInfo {
    let mut lines = output.lines();
    let git_dir = lines.next().unwrap_or_default().trim();
    let branch = lines.next().unwrap_or_default().trim();

    let branch = if branch.is_empty() || branch == "HEAD" {
        detached().trim().to_owned()
    } else {
        branch.to_owned()
    };

    GitInfo {
        branch,
        worktree: worktree_name(git_dir),
    }
}

/// A linked worktree's git dir is always `<main-repo>/.git/worktrees/<name>`, so
/// the last path component is exactly the name `git worktree list` reports. The
/// main checkout's git dir has no `worktrees/` component, and reads as no
/// worktree at all — which is what we want, since labelling every pane "main"
/// would just cost a row.
fn worktree_name(git_dir: &str) -> String {
    let Some((_, tail)) = git_dir.split_once("/worktrees/") else {
        return String::new();
    };
    tail.split('/').next().unwrap_or_default().to_owned()
}

fn git(path: &str, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        // A repo whose objects live on a slow mount can hang; we would rather
        // show no branch than stall the worker.
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn parses_branch_and_no_worktree_for_a_main_checkout() {
        let info = parse_rev_parse("/home/me/repo/.git\nfeat/auth\n", || {
            panic!("must not ask for a hash when a branch is known")
        });
        assert_eq!(info.branch, "feat/auth");
        assert_eq!(info.worktree, "");
    }

    #[test]
    fn recognizes_a_linked_worktree_by_its_git_dir_shape() {
        let info = parse_rev_parse("/home/me/repo/.bare/worktrees/wt-auth\nfeat/auth\n", || {
            String::new()
        });
        assert_eq!(info.worktree, "wt-auth");
        assert_eq!(info.branch, "feat/auth");
    }

    #[test]
    fn detached_head_falls_back_to_a_short_hash() {
        let info = parse_rev_parse("/home/me/repo/.git\nHEAD\n", || "a1b2c3d\n".to_owned());
        assert_eq!(info.branch, "a1b2c3d");
    }

    #[test]
    fn empty_branch_line_also_falls_back() {
        // A repo with no commits yet prints nothing for the branch.
        let info = parse_rev_parse("/home/me/repo/.git\n", || "".to_owned());
        assert_eq!(info.branch, "");
        assert_eq!(info.worktree, "");
    }

    #[test]
    fn worktree_name_stops_at_the_next_path_component() {
        assert_eq!(worktree_name("/r/.git/worktrees/wt-a"), "wt-a");
        assert_eq!(worktree_name("/r/.git/worktrees/wt-a/extra"), "wt-a");
        assert_eq!(worktree_name("/r/.git"), "");
        assert_eq!(worktree_name(""), "");
    }

    #[test]
    fn cache_reuses_a_fresh_answer_and_refreshes_a_stale_one() {
        let mut cache = GitCache::new();
        let calls = Cell::new(0);
        let resolve = |_: &str| {
            calls.set(calls.get() + 1);
            GitInfo {
                branch: format!("call-{}", calls.get()),
                worktree: String::new(),
            }
        };

        let start = Instant::now();
        assert_eq!(cache.get_at("/r", start, resolve).branch, "call-1");
        // Within the TTL: no second subprocess.
        assert_eq!(
            cache.get_at("/r", start + TTL - Duration::from_millis(1), resolve).branch,
            "call-1"
        );
        assert_eq!(calls.get(), 1);
        // Past it: refreshed.
        assert_eq!(cache.get_at("/r", start + TTL, resolve).branch, "call-2");
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn cache_remembers_that_a_path_is_not_a_repo() {
        // The point of negative caching: a workspace of plain shells must not
        // spawn a git process per pane per poll.
        let mut cache = GitCache::new();
        let calls = Cell::new(0);
        let resolve = |_: &str| {
            calls.set(calls.get() + 1);
            GitInfo::default()
        };

        let start = Instant::now();
        assert_eq!(cache.get_at("/tmp", start, resolve), GitInfo::default());
        assert_eq!(cache.get_at("/tmp", start, resolve), GitInfo::default());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn retain_paths_drops_panes_that_went_away() {
        let mut cache = GitCache::new();
        let now = Instant::now();
        cache.get_at("/a", now, |_| GitInfo::default());
        cache.get_at("/b", now, |_| GitInfo::default());
        assert_eq!(cache.entries.len(), 2);

        cache.retain_paths(&["/a"]);
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.entries.contains_key("/a"));
    }

    #[test]
    fn an_empty_path_is_never_looked_up() {
        assert_eq!(lookup(""), GitInfo::default());
    }
}
