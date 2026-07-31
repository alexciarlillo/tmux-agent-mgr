# tmux-agent-mgr

A tmux sidebar for watching AI coding agents across every session and window — and
a general session/window switcher, because once you have the list you may as well
navigate with it.

Two things it tries to get right:

- **It can stay open.** The sidebar is a real tmux pane, not an overlay, so you can
  leave it beside your work all day.
- **It doesn't flicker.** Rendering is pure and the loop draws only when the output
  actually changed. An idle sidebar writes nothing to the terminal at all — no
  repaint on a timer, and exactly one `terminal.clear()` in the codebase, in the
  resize path where the geometry genuinely moved.

It works with **zero setup**: agents are detected from the process table and the
pane's visible screen, so nothing needs installing into Claude Code or Codex.

## Install

With [TPM](https://github.com/tmux-plugins/tpm), in `~/.tmux.conf`:

```tmux
set -g @plugin 'alexciarlillo/tmux-agent-mgr'
```

Then `prefix + I`. The plugin builds itself on first load if a binary isn't
present, so you need a Rust toolchain (`cargo`) available once. Manually:

```sh
git clone https://github.com/alexciarlillo/tmux-agent-mgr ~/.tmux/plugins/tmux-agent-mgr
cd ~/.tmux/plugins/tmux-agent-mgr && cargo build --release
tmux source ~/.tmux.conf
```

Requires tmux 3.0+. The popup needs 3.3+ (`display-popup -B -E`); below that
everything else still works and the popup key is simply not bound.

## Use

| Key | |
|---|---|
| `prefix + e` | toggle the sidebar in this window |
| `prefix + E` | toggle it in every window |
| `C-n` | open the full-screen popup (no prefix) |

The **sidebar** is narrow and persistent. The **popup** is the same list
full-screen with a live preview of the selected window beside it, and it closes
itself as soon as you jump somewhere — it's a chooser, not a place to live.

### Inside the list

| Key | |
|---|---|
| `j` `k` `↓` `↑` | next / previous pane |
| `N j` / `N k` | move N panes (`10j`) |
| `H` `L` | previous / next session |
| `J` `K` | move this session down / up the list |
| `g` `G` | first / last pane |
| `N G` | go to pane N |
| `C-d` `C-u` | page down / up |
| `Enter` | jump to the selected pane |
| `Tab` | cycle the status filter: all → working → blocked → done |
| `/` | search; `Enter` keeps the filter, `Esc` clears it |
| `R` | rename the selected window |
| `r` | refresh now |
| `?` | keymap |
| `q` `Esc` | close |

A gutter shows each pane's distance from the cursor, which is what makes `10j`
something you can aim rather than guess at. `H` from mid-session goes to the top of
that session first, then to the session above.

Search matches a pane's session, window, command, git branch, worktree and agent
name, so `ops`, `claude` and `auth` all find what you'd expect. Terms are ANDed, and
it composes with the status filter rather than replacing it.

### Reading a row

```
 ● claude  plan               1m12s
   feat/auth ~wt-auth
   ▸ waiting: permission
   ▸ Explore ×2
   ▸ tasks 3/7
```

`●` working · `◉` blocked, needs you · `●` finished and unread · `○` idle ·
`✕` errored · `·` no agent. A `┃` marks the pane tmux is actually focused on.

The lines below the first appear only when there is something to say. The branch
row comes from git; the rest need agent hooks (not yet shipped — see Status). A
trailing `?` on the agent name means the *blocked* reading came from a heuristic
rather than from the agent itself.

Window tabs also carry a rolled-up status glyph, appended to your existing
`window-status-format` without replacing it.

### Global navigation

On by default, and passed through to Vim when the focused pane is running it:

| Key | |
|---|---|
| `C-h` `C-l` | move pane left/right, or wrap to the previous/next window at an edge |
| `C-j` `C-k` | previous / next session |

## Configure

Set these **before** the plugin loads; it only fills in what you haven't.

| Option | Default | |
|---|---|---|
| `@agent_mgr_width` | `20%` | sidebar width: columns, or a percentage of the window |
| `@agent_mgr_min_width` | `24` | lower clamp |
| `@agent_mgr_max_width` | *unset* | upper clamp; unset means uncapped |
| `@agent_mgr_position` | `left` | `left` or `right` |
| `@agent_mgr_agents_only` | `off` | list only panes running an agent |
| `@agent_mgr_tab_status` | `on` | status glyph in window tabs |
| `@agent_mgr_nav` | `on` | the `C-h/j/k/l` bindings above |
| `@agent_mgr_key` | `e` | prefix key toggling the sidebar here |
| `@agent_mgr_key_all` | `E` | prefix key toggling it everywhere |
| `@agent_mgr_key_popup` | `C-n` | prefix-less popup key; `none` binds nothing |

Colors take a `#RRGGBB` or a 0–255 palette index:
`@agent_mgr_color_{accent,session,working,blocked,idle,done,error,branch}`.

```tmux
set -g @agent_mgr_position right
set -g @agent_mgr_width 28
set -g @agent_mgr_color_accent '#89b4fa'
```

## How it works

One background daemon per tmux server polls every pane, infers status, and caches
the result into tmux pane options. Every sidebar and popup then reads that from a
single `list-panes` call — so ten open sidebars cost one poller, not ten. Focus
changes arrive as a signal from a tmux hook rather than being discovered by polling,
which is why it feels immediate while drawing almost never.

No tmux or git subprocess ever runs on the UI thread.

## Status

Working: passive detection (Claude Code and Codex), the sidebar, the popup with
window preview, navigation, search, filters, rename, session reorder, tab glyphs.

Not yet: **agent hooks**, which is what will populate the permission badge, wait
reason, subagent and task-progress rows — the model and rendering for them are in
place and simply have nothing feeding them yet. The preview is also plain text; it
doesn't carry ANSI colour through.

## Development

```sh
cargo test
cargo clippy --all-targets
```

The tests are pure — they never issue a tmux command that changes anything, which
is deliberate: several actions (`Enter`, `R`, `J`) would otherwise move or rename
things on whatever tmux server is hosting the test run. Each has its decision split
out from its I/O, and the tests exercise the decision.

`agent-mgr daemon --once` prints resolved per-pane state as TSV — the quickest way
to check detection without the TUI.

When testing against a live tmux, **use a throwaway socket**:

```sh
tmux -L probe -f /dev/null new-session -d -s t -x 150 -y 40
tmux -L probe new-window -t t "$PWD/target/release/agent-mgr"
tmux -L probe kill-server          # only ever with -L
```

`-f /dev/null` so you aren't looking at your own config, and `-L` on *every* call —
a bare `tmux` resolves through `$TMUX` to your real server. Run the plugin's own
commands inside the probe server for the same reason.

`AGENT_MGR_DEBUG_FRAMES=/tmp/frames` makes the binary write its draw count on exit,
so the no-flicker claim is something you can check rather than take on faith.
