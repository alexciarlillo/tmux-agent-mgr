//! Passive agent detection: recognizing an agent by its process name, and
//! inferring what it is doing from the pane title and the visible screen.
//!
//! This is the zero-setup path — nothing is wrapped, shimmed, or launched by us,
//! and no hook needs to be installed. The cost is that it reads the agents' UI,
//! so it is **heuristic and version-sensitive**: a Claude or Codex redesign, an
//! unusual theme, or a non-English locale can throw off a reading. When the
//! agent's own hooks are wired up, [`crate::daemon`] prefers those instead.
//!
//! Ported from `tmux-agent-switcher`'s `src/detect.rs`, which is where these
//! heuristics (and their edge cases) were worked out.

use crate::model::{AgentEvidence, AgentKind, AgentState};

/// How many lines of the screen tail state detection looks at. Both agents put
/// their prompts at the bottom, and a longer window lets scrollback pin a stale
/// state.
pub const SCREEN_TAIL_LINES: usize = 25;

/// Identify an agent from a pane's foreground command.
pub fn agent_from_process_name(name: &str) -> Option<AgentKind> {
    let basename = name.rsplit('/').next().unwrap_or(name);
    if basename == "codex" || basename.starts_with("codex-") {
        Some(AgentKind::Codex)
    } else if basename == "claude"
        || basename == "claude-code"
        || basename.starts_with("claude-")
        || is_claude_version_name(basename)
    {
        Some(AgentKind::Claude)
    } else {
        None
    }
}

/// Claude Code's native installer runs a versioned binary at
/// `~/.local/share/claude/versions/<version>` and sets its `process.title` to
/// that same version string, so tmux reports the pane's current command as a
/// bare `MAJOR.MINOR.PATCH` (e.g. `2.1.197`) rather than `claude`. Treat that
/// shape as Claude so detection still fires for native installs.
fn is_claude_version_name(name: &str) -> bool {
    let mut parts = 0;
    for part in name.split('.') {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        parts += 1;
    }
    parts == 3
}

/// The cheap path: some titles are unambiguous on their own, which saves a
/// `capture-pane` subprocess per pane per poll.
///
/// Returns `None` when the title is inconclusive and the screen must be read.
pub fn state_from_title(agent: AgentKind, title: &str) -> Option<AgentState> {
    let title = title.trim();
    match agent {
        AgentKind::Codex if title.contains("Action Required") => Some(AgentState::Blocked),
        AgentKind::Codex if starts_with_braille_spinner(title) => Some(AgentState::Working),
        AgentKind::Codex if !title.is_empty() => Some(AgentState::Idle),
        AgentKind::Claude if starts_with_braille_spinner(title) => Some(AgentState::Working),
        // Claude's idle title (`✳ …`) looks the same whether or not a modal is
        // on screen, so it is never conclusive by itself.
        _ => None,
    }
}

/// The full path: decide state from the title plus the tail of the screen.
pub fn state_from_evidence(agent: AgentKind, evidence: &AgentEvidence) -> AgentState {
    match agent {
        AgentKind::Codex => codex_state(evidence),
        AgentKind::Claude => claude_state(evidence),
    }
}

fn codex_state(evidence: &AgentEvidence) -> AgentState {
    let title = evidence.osc_title.trim();
    let tail = recent_lines(&evidence.screen_tail, SCREEN_TAIL_LINES).to_lowercase();

    if title.contains("Action Required")
        || contains_any(
            &tail,
            &[
                "press enter to confirm or esc to cancel",
                "enter to submit answer",
                "allow command?",
                "[y/n]",
                "yes (y)",
                "no (n)",
            ],
        )
    {
        return AgentState::Blocked;
    }

    if starts_with_braille_spinner(title) {
        return AgentState::Working;
    }

    AgentState::Idle
}

fn claude_state(evidence: &AgentEvidence) -> AgentState {
    let title = evidence.osc_title.trim();
    // Claude's prompts always sit at the bottom of the screen. Only matching the
    // last handful of lines stops old scrollback from pinning a state — e.g. a
    // long-scrolled-past "do you want to proceed?" holding a busy pane Blocked.
    let recent = recent_lines(&evidence.screen_tail, SCREEN_TAIL_LINES);

    // Blocked: a modal selection menu is on screen with the cursor resting on one
    // of several numbered options. The match is structural rather than
    // wording-based so it survives a copy edit, with the selection-list footer as
    // a fallback.
    if has_selection_menu(&recent)
        || contains_all(
            &recent.to_lowercase(),
            &["enter to select", "esc to cancel"],
        )
    {
        return AgentState::Blocked;
    }

    // Working: Claude prefixes its OSC title with a braille spinner while active.
    if starts_with_braille_spinner(title) {
        return AgentState::Working;
    }

    // Otherwise it is sitting at its input prompt. Note the `❯` input box is
    // present while working too, so its presence is not an idle signal.
    AgentState::Idle
}

/// `true` when the screen shows a Claude selection menu: the `❯` cursor rests on
/// a numbered option *and* at least two numbered options are present.
///
/// Requiring a second option is what distinguishes a real menu from the bare `❯`
/// input box with something like `1. fix the parser` typed into it. Stripping the
/// box border first is what makes it work against Claude's real bordered
/// rendering (`│ ❯ 1. Yes │`), where the option no longer starts the line.
///
/// Known ambiguity: a user composing a *multi-line* numbered list in the input
/// box is structurally identical to a menu and can read as Blocked. Anchoring on
/// a menu footer would remove it, but Claude's permission and plan modals don't
/// render one, so that would miss the very prompts this exists to catch. The
/// idle→busy debounce in [`crate::daemon`] absorbs the common fast-typed case.
fn has_selection_menu(text: &str) -> bool {
    let mut cursor_on_option = false;
    let mut option_lines = 0;

    for line in text.lines() {
        let line = strip_border(line);
        let (has_cursor, rest) = match line.strip_prefix('❯') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, line),
        };
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            continue;
        }
        let after = &rest[digits..];
        if after.starts_with('.') || after.starts_with(')') {
            option_lines += 1;
            cursor_on_option |= has_cursor;
        }
    }

    cursor_on_option && option_lines >= 2
}

/// Strip leading whitespace and box-drawing verticals, so matching works whether
/// or not the content is wrapped in a border.
fn strip_border(line: &str) -> &str {
    line.trim_start_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, '│' | '┃' | '║' | '╎' | '┆' | '┊' | '|')
    })
}

fn recent_lines(text: &str, count: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(count);
    all[start..].join("\n")
}

/// Both agents advertise activity by prefixing their OSC title with a braille
/// spinner frame followed by a space.
fn starts_with_braille_spinner(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(ch) if ('\u{2800}'..='\u{28ff}').contains(&ch))
        && matches!(chars.next(), Some(' '))
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn contains_all(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().all(|needle| haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(title: &str, screen: &[&str]) -> AgentEvidence {
        AgentEvidence {
            screen_tail: screen.join("\n"),
            osc_title: title.to_owned(),
        }
    }

    #[test]
    fn recognizes_both_agents_from_a_process_name() {
        assert_eq!(
            agent_from_process_name("/opt/bin/codex"),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            agent_from_process_name("codex-aarch64-a"),
            Some(AgentKind::Codex)
        );
        assert_eq!(agent_from_process_name("claude"), Some(AgentKind::Claude));
        assert_eq!(
            agent_from_process_name("claude-code"),
            Some(AgentKind::Claude)
        );
    }

    #[test]
    fn recognizes_the_native_installer_bare_semver_as_claude() {
        // anthropics/claude-code#49852: tmux reports the version, not "claude".
        assert_eq!(agent_from_process_name("2.1.197"), Some(AgentKind::Claude));
        assert_eq!(
            agent_from_process_name("/home/me/.local/share/claude/versions/2.1.197"),
            Some(AgentKind::Claude)
        );
        // ...without swallowing ordinary commands.
        assert_eq!(agent_from_process_name("zsh"), None);
        assert_eq!(agent_from_process_name("node"), None);
        assert_eq!(agent_from_process_name("2.1"), None);
        assert_eq!(agent_from_process_name("1.2.3.4"), None);
        assert_eq!(agent_from_process_name("v1.2.3"), None);
    }

    #[test]
    fn title_fast_path_short_circuits_the_unambiguous_cases() {
        assert_eq!(
            state_from_title(AgentKind::Codex, "[ ! ] Action Required | repo"),
            Some(AgentState::Blocked)
        );
        assert_eq!(
            state_from_title(AgentKind::Codex, "⠋ working"),
            Some(AgentState::Working)
        );
        assert_eq!(
            state_from_title(AgentKind::Codex, "repo"),
            Some(AgentState::Idle)
        );
        assert_eq!(
            state_from_title(AgentKind::Claude, "⠋ thinking"),
            Some(AgentState::Working)
        );
        // Claude's idle-looking title is never conclusive: a modal may be up.
        assert_eq!(state_from_title(AgentKind::Claude, "✳ review this"), None);
        assert_eq!(state_from_title(AgentKind::Claude, ""), None);
    }

    #[test]
    fn claude_selection_menu_is_blocked_even_with_an_idle_title() {
        let state = state_from_evidence(
            AgentKind::Claude,
            &evidence(
                "✳ design the thing",
                &[
                    "│ Would you like to proceed?              │",
                    "│ ❯ 1. Yes, and auto-accept edits         │",
                    "│   2. Yes, and manually approve edits    │",
                    "│   3. No, keep planning                  │",
                ],
            ),
        );
        assert_eq!(state, AgentState::Blocked);
    }

    #[test]
    fn claude_bordered_menu_without_a_known_phrase_is_blocked() {
        // A custom AskUserQuestion menu: no recognizable wording, non-1
        // numbering, ')' delimiter, drawn inside a border. Structural matching
        // is what catches this.
        let state = state_from_evidence(
            AgentKind::Claude,
            &evidence(
                "✳ pick a database",
                &[
                    "│ Which database should we use?           │",
                    "│ ❯ 2) Postgres                           │",
                    "│   3) SQLite                             │",
                ],
            ),
        );
        assert_eq!(state, AgentState::Blocked);
    }

    #[test]
    fn claude_idle_input_box_is_not_blocked() {
        let state = state_from_evidence(
            AgentKind::Claude,
            &evidence(
                "✳ clarify the logic",
                &[
                    "※ recap: did the thing. next: your review.",
                    "──────────────── ultracode ─",
                    "❯ ",
                    "────────────────",
                    "  ⏵⏵ bypass permissions on (shift+tab to cycle)",
                ],
            ),
        );
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn claude_prompt_scrolled_out_of_the_tail_no_longer_blocks() {
        let mut screen = vec!["Do you want to proceed?".to_owned()];
        for index in 0..30 {
            screen.push(format!("build output line {index}"));
        }
        let state = state_from_evidence(
            AgentKind::Claude,
            &AgentEvidence {
                screen_tail: screen.join("\n"),
                osc_title: "⠙ working".to_owned(),
            },
        );
        assert_eq!(state, AgentState::Working);
    }

    #[test]
    fn selection_menu_needs_both_a_cursor_and_a_second_option() {
        assert!(has_selection_menu("│ ❯ 1. Yes │\n│   2. No │"));
        assert!(has_selection_menu("❯ 10) ten\n  11) eleven"));
        // The bare input box, or a single "1." line typed into it, is not a menu.
        assert!(!has_selection_menu("❯ "));
        assert!(!has_selection_menu("❯ 1. fix the parser and then rebase"));
        // A numbered list in ordinary output has no cursor on it.
        assert!(!has_selection_menu("1. first\n2. second"));
    }

    #[test]
    fn codex_permission_phrases_read_as_blocked() {
        for phrase in [
            "press enter to confirm or esc to cancel",
            "Allow command?",
            "run it? [y/n]",
        ] {
            assert_eq!(
                state_from_evidence(AgentKind::Codex, &evidence("", &[phrase])),
                AgentState::Blocked,
                "{phrase:?} should read as blocked"
            );
        }
    }

    #[test]
    fn codex_spinner_title_beats_quiet_screen() {
        assert_eq!(
            state_from_evidence(AgentKind::Codex, &evidence("⠋ compiling", &["..."])),
            AgentState::Working
        );
    }

    #[test]
    fn a_braille_glyph_without_a_trailing_space_is_not_a_spinner() {
        assert!(starts_with_braille_spinner("⠋ thinking"));
        assert!(!starts_with_braille_spinner("⠋thinking"));
        assert!(!starts_with_braille_spinner("thinking ⠋"));
        assert!(!starts_with_braille_spinner(""));
    }
}
