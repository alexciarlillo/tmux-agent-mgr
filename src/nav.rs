//! Vim-style navigation over the rendered block list.
//!
//! Everything here is a pure function of the block list and the current
//! selection, so the whole keymap is testable without a terminal or a tmux
//! server. [`crate::app::input`] is the only caller.
//!
//! Two ideas carried over from `tmux-agent-switcher`, which got this part right:
//!
//! - **Counted motions.** Typing `10j` moves ten panes, not one. The count
//!   accumulates across keystrokes and is consumed by the motion, exactly as in
//!   vim, which is only useful if you can *see* the distances — hence the
//!   relative-number gutter in [`crate::ui::rows`].
//! - **Session edges.** `H` / `L` jump whole sessions rather than crawling
//!   through their windows, which is what makes this usable as a general
//!   navigation aid and not just an agent list.

use crate::ui::rows::Block;

/// Which way a motion goes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Up,
    Down,
}

/// Feed a digit into a pending count. Returns `false` if `ch` was not a digit we
/// can use, so the caller knows to treat the key as a command instead.
///
/// A leading `0` is refused: with no count pending, `0` is a command in its own
/// right in every vim-like keymap, and swallowing it here would make it dead.
pub fn push_count(count: &mut Option<usize>, ch: char) -> bool {
    let Some(digit) = ch.to_digit(10).map(|digit| digit as usize) else {
        return false;
    };
    if count.is_none() && digit == 0 {
        return false;
    }
    // Saturating rather than wrapping: someone holding a digit key should end up
    // at the end of the list, not back at the top.
    *count = Some(count.unwrap_or(0).saturating_mul(10).saturating_add(digit));
    true
}

/// Consume a pending count, defaulting to 1.
///
/// Takes rather than peeks: a count applies to exactly one motion and must not
/// leak into the next one.
pub fn take_count(count: &mut Option<usize>) -> usize {
    count.take().unwrap_or(1).max(1)
}

/// Move `count` blocks from `selected`, clamped to the list.
pub fn step(blocks: &[Block], selected: usize, direction: Direction, count: usize) -> usize {
    if blocks.is_empty() {
        return 0;
    }
    let last = blocks.len() - 1;
    match direction {
        Direction::Up => selected.saturating_sub(count),
        Direction::Down => selected.saturating_add(count).min(last),
    }
}

/// Jump a session: `L` to the next one, `H` back.
///
/// `H` is deliberately two-stage, mirroring how `{` behaves in vim: from the
/// middle of a session it first goes to the top of *that* session, and only from
/// there to the session above. So `H` doubles as a reliable "top of this
/// session" instead of skipping past the session you are reading.
///
/// Both directions clamp rather than wrap. A jump this coarse that wraps tends to
/// leave you somewhere you then have to go looking for.
pub fn session_edge(blocks: &[Block], selected: usize, direction: Direction) -> usize {
    if blocks.is_empty() {
        return 0;
    }
    // The worker can replace the tree between a keypress and this call, so the
    // index may outlive its row.
    let selected = selected.min(blocks.len() - 1);
    let current = &blocks[selected].target.session_name;

    match direction {
        // Forward *from the selection*: scanning the whole list would find the
        // first foreign session anywhere, which is usually one above us.
        Direction::Down => blocks
            .iter()
            .enumerate()
            .skip(selected)
            .find(|(_, block)| &block.target.session_name != current)
            .map_or(selected, |(index, _)| index),
        Direction::Up => {
            let own_top = session_start(blocks, selected);
            if selected != own_top {
                return own_top;
            }
            if own_top == 0 {
                return own_top;
            }
            session_start(blocks, own_top - 1)
        }
    }
}

/// Index of the first block belonging to the same session as `index`.
fn session_start(blocks: &[Block], index: usize) -> usize {
    let session = &blocks[index].target.session_name;
    let mut start = index;
    while start > 0 && &blocks[start - 1].target.session_name == session {
        start -= 1;
    }
    start
}

/// Distance from the selected row, for the vim relative-number gutter.
///
/// The selected row shows `0`, matching `relativenumber` + `number` in vim, which
/// is what makes `10j` countable off the screen.
pub fn relative_number(selected: usize, index: usize) -> usize {
    index.abs_diff(selected)
}

/// Columns needed to print the largest relative number in a list of `len`.
///
/// Derived from the list length rather than the largest *visible* distance so the
/// gutter doesn't change width as you scroll, which would reflow every row.
pub fn number_width(len: usize) -> usize {
    match len {
        0 | 1 => 1,
        len => (len - 1).to_string().len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::rows::PaneTarget;

    fn blocks(sessions: &[(&str, usize)]) -> Vec<Block> {
        let mut out = Vec::new();
        for (session, count) in sessions {
            for index in 0..*count {
                out.push(Block {
                    target: PaneTarget {
                        session_name: (*session).to_owned(),
                        window_id: format!("@{index}"),
                        pane_id: format!("%{}", out.len()),
                    },
                    line_start: out.len(),
                    line_count: 1,
                });
            }
        }
        out
    }

    // ─── counts ───────────────────────────────────────────────────────

    #[test]
    fn digits_accumulate_into_a_multi_digit_count() {
        let mut count = None;
        assert!(push_count(&mut count, '1'));
        assert!(push_count(&mut count, '0'));
        assert_eq!(count, Some(10));
    }

    #[test]
    fn a_leading_zero_is_not_a_count() {
        // `0` has to stay available as a command; swallowing it as a count would
        // make it silently dead.
        let mut count = None;
        assert!(!push_count(&mut count, '0'));
        assert_eq!(count, None);
    }

    #[test]
    fn a_non_digit_is_refused_without_disturbing_the_count() {
        let mut count = Some(3);
        assert!(!push_count(&mut count, 'j'));
        assert_eq!(count, Some(3));
    }

    #[test]
    fn a_count_applies_to_one_motion_and_then_is_gone() {
        let mut count = Some(7);
        assert_eq!(take_count(&mut count), 7);
        assert_eq!(count, None);
        assert_eq!(take_count(&mut count), 1, "absent count means move one");
    }

    #[test]
    fn an_absurd_count_saturates_instead_of_wrapping() {
        let mut count = None;
        for _ in 0..40 {
            push_count(&mut count, '9');
        }
        // Wrapping here could land the selection anywhere; saturating lands it at
        // the end of the list, which is what holding a digit key should do.
        let list = blocks(&[("work", 5)]);
        assert_eq!(step(&list, 0, Direction::Down, take_count(&mut count)), 4);
    }

    // ─── stepping ─────────────────────────────────────────────────────

    #[test]
    fn a_counted_motion_moves_that_many_blocks() {
        let list = blocks(&[("work", 10)]);
        assert_eq!(step(&list, 0, Direction::Down, 4), 4);
        assert_eq!(step(&list, 9, Direction::Up, 4), 5);
    }

    #[test]
    fn motions_clamp_at_both_ends() {
        let list = blocks(&[("work", 3)]);
        assert_eq!(step(&list, 2, Direction::Down, 99), 2);
        assert_eq!(step(&list, 0, Direction::Up, 99), 0);
    }

    #[test]
    fn motions_on_an_empty_list_stay_put() {
        assert_eq!(step(&[], 0, Direction::Down, 3), 0);
    }

    // ─── session edges ────────────────────────────────────────────────

    #[test]
    fn l_jumps_to_the_first_block_of_the_next_session() {
        let list = blocks(&[("work", 3), ("ops", 2)]);
        assert_eq!(session_edge(&list, 0, Direction::Down), 3);
        assert_eq!(session_edge(&list, 2, Direction::Down), 3);
    }

    #[test]
    fn h_jumps_to_the_first_block_of_the_previous_session() {
        let list = blocks(&[("work", 3), ("ops", 2)]);
        // From the top of `ops`, back to the top of `work`.
        assert_eq!(session_edge(&list, 3, Direction::Up), 0);
    }

    #[test]
    fn h_from_mid_session_goes_to_the_top_of_that_session_first() {
        // Makes `H` a dependable "top of this session" as well as a session jump,
        // rather than skipping past the session you are reading.
        let list = blocks(&[("work", 3), ("ops", 3)]);
        assert_eq!(session_edge(&list, 5, Direction::Up), 3);
        assert_eq!(session_edge(&list, 4, Direction::Up), 3);
    }

    #[test]
    fn session_jumps_clamp_rather_than_wrap() {
        let list = blocks(&[("work", 2), ("ops", 2)]);
        // Already in the last session: nowhere further to go.
        assert_eq!(session_edge(&list, 3, Direction::Down), 3);
        // Already at the very top.
        assert_eq!(session_edge(&list, 0, Direction::Up), 0);
    }

    #[test]
    fn session_jumps_handle_a_single_session_and_an_empty_list() {
        let list = blocks(&[("work", 3)]);
        assert_eq!(session_edge(&list, 1, Direction::Down), 1);
        assert_eq!(session_edge(&list, 1, Direction::Up), 0);
        assert_eq!(session_edge(&[], 0, Direction::Down), 0);
    }

    #[test]
    fn a_stale_selection_index_does_not_panic() {
        // The worker replaces the tree underneath us; an index can outlive its row.
        let list = blocks(&[("work", 2)]);
        assert_eq!(session_edge(&list, 99, Direction::Down), 1);
    }

    // ─── the number gutter ────────────────────────────────────────────

    #[test]
    fn relative_numbers_count_outward_from_the_selection() {
        assert_eq!(relative_number(5, 5), 0);
        assert_eq!(relative_number(5, 8), 3);
        assert_eq!(relative_number(5, 2), 3);
    }

    #[test]
    fn the_gutter_is_wide_enough_for_the_largest_distance() {
        // Width comes from the list length, not the visible rows, so it cannot
        // change as you scroll and reflow every line.
        assert_eq!(number_width(0), 1);
        assert_eq!(number_width(1), 1);
        assert_eq!(number_width(10), 1, "distances run 0..=9");
        assert_eq!(number_width(11), 2);
        assert_eq!(number_width(101), 3);
    }
}
