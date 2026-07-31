#!/usr/bin/env bash
# TPM entry point for tmux-agent-mgr.
#
# Runs once at tmux start. Its whole job is to find the binary, publish its path
# as @agent_mgr_bin, and source agent-mgr.conf — which owns every option, key
# binding and hook. Nothing here starts the daemon or opens a sidebar; both
# happen lazily, the first time you press the toggle key.
set -uo pipefail

PLUGIN_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"

# tmux >= 3.0 for `if -F` with braces, which agent-mgr.conf uses throughout.
version="$(tmux -V | grep -oE '[0-9]+\.[0-9]+' | head -n1)"
if [[ -n "$version" ]]; then
    major="${version%%.*}"
    if (( major < 3 )); then
        tmux display-message "tmux-agent-mgr: requires tmux >= 3.0 (found $(tmux -V))"
        exit 0
    fi
fi

# Prefer a release build, then a debug build, then anything on PATH. The debug
# fallback is what makes `cargo build` (without --release) enough while working on
# the plugin itself.
for candidate in \
    "$PLUGIN_DIR/target/release/agent-mgr" \
    "$PLUGIN_DIR/target/debug/agent-mgr" \
    "$PLUGIN_DIR/bin/agent-mgr"
do
    if [[ -x "$candidate" ]]; then
        BIN="$candidate"
        break
    fi
done
if [[ -z "${BIN:-}" ]] && command -v agent-mgr &>/dev/null; then
    BIN="$(command -v agent-mgr)"
fi

if [[ -z "${BIN:-}" ]]; then
    # Build in the background: blocking here would stall tmux startup, and
    # failing silently would leave the user with a key that does nothing.
    if command -v cargo &>/dev/null; then
        tmux display-message "tmux-agent-mgr: building, the toggle key will work shortly…"
        tmux run-shell -b "cd '$PLUGIN_DIR' && cargo build --release --quiet \
            && tmux source-file '$PLUGIN_DIR/tmux-agent-mgr.tmux' \
            && tmux display-message 'tmux-agent-mgr: ready' \
            || tmux display-message 'tmux-agent-mgr: build failed, run cargo build --release in $PLUGIN_DIR'"
    else
        tmux display-message "tmux-agent-mgr: no binary and no cargo — install Rust, or build agent-mgr yourself"
    fi
    exit 0
fi

tmux set-option -g @agent_mgr_bin "$BIN"
tmux source-file "$PLUGIN_DIR/agent-mgr.conf"
