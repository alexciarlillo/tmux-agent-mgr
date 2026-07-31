//! Incremental search over the pane tree.
//!
//! A pane is matched on everything you might plausibly remember about it — its
//! session, its window, what is running in it, and its branch or worktree — rather
//! than on one field. Searching `ops` finds a session, `claude` finds agents, and
//! `auth` finds a branch, without you having to say which kind of thing you meant.
//!
//! Terms are ANDed, so `claude auth` narrows to the Claude pane on the auth
//! branch. That composes with the status filter rather than replacing it: a
//! filtered search means "blocked agents, among these".

use crate::model::{PaneInfo, SessionGroup, WindowInfo};

/// A parsed query: whitespace-separated terms, lowercased once at construction so
/// matching a large tree doesn't re-lowercase the needle per pane.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct Query {
    terms: Vec<String>,
}

impl Query {
    pub fn new(raw: &str) -> Self {
        Self {
            terms: raw
                .split_whitespace()
                .map(|term| term.to_lowercase())
                .collect(),
        }
    }

    /// `true` when this query excludes nothing, so callers can skip the walk.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Does this pane match every term?
    pub fn matches(
        &self,
        session: &SessionGroup,
        window: &WindowInfo,
        pane: &PaneInfo,
    ) -> bool {
        if self.is_empty() {
            return true;
        }
        let haystack = haystack(session, window, pane);
        self.terms.iter().all(|term| haystack.contains(term))
    }
}

/// Everything about a pane worth searching, lowercased and joined.
///
/// Deliberately excludes `current_path`: it is mostly shared ancestry, so a term
/// like `main` or `src` would match nearly every pane and make the search feel
/// broken. The parts of a path you actually search for — the branch and the
/// worktree name — are here on their own.
fn haystack(session: &SessionGroup, window: &WindowInfo, pane: &PaneInfo) -> String {
    let mut text = String::new();
    for part in [
        session.session_name.as_str(),
        window.window_name.as_str(),
        window.window_index.as_str(),
        pane.current_command.as_str(),
        pane.branch.as_str(),
        pane.worktree.as_str(),
        pane.status.agent.map_or("", |agent| agent.label()),
    ] {
        if part.is_empty() {
            continue;
        }
        text.push_str(part);
        text.push('\u{1}');
    }
    text.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, AgentStatus, PaneInfo, SessionGroup, WindowInfo};

    fn fixture(
        session: &str,
        window: &str,
        command: &str,
        branch: &str,
        agent: Option<AgentKind>,
    ) -> (SessionGroup, WindowInfo, PaneInfo) {
        let pane = PaneInfo {
            pane_id: "%1".to_owned(),
            window_id: "@1".to_owned(),
            pane_index: "0".to_owned(),
            pane_active: false,
            current_command: command.to_owned(),
            current_path: "/home/me/GitHub/project/main".to_owned(),
            title: String::new(),
            pane_pid: None,
            status: AgentStatus {
                agent,
                ..AgentStatus::default()
            },
            branch: branch.to_owned(),
            worktree: String::new(),
        };
        let window = WindowInfo {
            window_id: "@1".to_owned(),
            window_index: "3".to_owned(),
            window_name: window.to_owned(),
            window_active: false,
            panes: vec![pane.clone()],
        };
        let session = SessionGroup {
            session_name: session.to_owned(),
            session_attached: true,
            windows: vec![window.clone()],
        };
        (session, window, pane)
    }

    fn matches(query: &str, parts: &(SessionGroup, WindowInfo, PaneInfo)) -> bool {
        Query::new(query).matches(&parts.0, &parts.1, &parts.2)
    }

    #[test]
    fn an_empty_query_matches_everything() {
        let parts = fixture("work", "editor", "zsh", "", None);
        assert!(matches("", &parts));
        assert!(matches("   ", &parts), "whitespace only is still empty");
        assert!(Query::new("  ").is_empty());
    }

    #[test]
    fn any_remembered_field_finds_the_pane() {
        let parts = fixture(
            "ops",
            "deploy",
            "claude",
            "feat/auth",
            Some(AgentKind::Claude),
        );
        for query in ["ops", "deploy", "claude", "auth", "feat/auth"] {
            assert!(matches(query, &parts), "{query:?} should match");
        }
    }

    #[test]
    fn matching_ignores_case_in_both_directions() {
        let parts = fixture("Work", "Editor", "Claude", "Feat/Auth", None);
        assert!(matches("work", &parts));
        assert!(matches("EDITOR", &parts));
        assert!(matches("aUtH", &parts));
    }

    #[test]
    fn terms_are_anded_so_extra_words_narrow() {
        let parts = fixture(
            "ops",
            "deploy",
            "claude",
            "feat/auth",
            Some(AgentKind::Claude),
        );
        assert!(matches("claude auth", &parts));
        assert!(!matches("claude nonsense", &parts));
    }

    #[test]
    fn the_agent_label_is_searchable_even_when_the_command_differs() {
        // Claude's native installer runs a versioned binary, so `current_command`
        // can be a bare semver while the agent is plainly "claude".
        let parts = fixture("work", "w", "2.1.220", "", Some(AgentKind::Claude));
        assert!(matches("claude", &parts));
    }

    #[test]
    fn the_shared_path_is_not_searched() {
        // Every pane shares most of its path, so matching on it would make common
        // terms select everything and read as a broken search.
        let parts = fixture("work", "editor", "zsh", "", None);
        assert!(!matches("github", &parts));
        assert!(!matches("project", &parts));
    }

    #[test]
    fn a_term_cannot_match_across_two_fields() {
        // Fields are joined with a separator that cannot occur in them, so
        // "editorzsh" must not match the window "editor" plus the command "zsh".
        let parts = fixture("work", "editor", "zsh", "", None);
        assert!(!matches("editorzsh", &parts));
    }
}
