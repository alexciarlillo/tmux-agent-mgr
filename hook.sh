#!/bin/sh
# Dispatcher for agent hooks: `hook.sh <agent> <event>`, payload on stdin.
#
# Registered in hooks/hooks.json, so this path is what the agent's config holds.
# Everything that knows what an event *means* lives in the Rust `hook`
# subcommand; this file only has to find the binary and get out of the way.
#
# Two properties matter here, and both are why the indirection exists at all:
#
#   1. Late binding. The binary is resolved fresh on every fire, first by asking
#      the running tmux server for @agent_mgr_bin — the same path its own key
#      bindings use. So rebuilding, switching between debug and release, or
#      moving the plugin directory never leaves a stale path baked into the
#      agent's config.
#   2. Graceful absence. No binary, no tmux, no pane: exit 0, silently. A hook
#      that fails is a hook that prints errors into the user's agent session, and
#      a monitoring plugin has no business interrupting the work it watches.
#
# /bin/sh on purpose: this runs on whatever the agent's shell is, and there is
# nothing here that needs bash.
PLUGIN_DIR="$(cd "$(dirname "$0")" && pwd -P)"
# Where TPM puts us, for the case where the agent runs this from a Claude Code
# plugin install: that cache directory holds hook.sh but never the binary.
TPM_DIR="$HOME/.tmux/plugins/tmux-agent-mgr"

# The tmux server already resolved a binary at load; prefer its answer.
BIN="$(tmux show-option -gqv @agent_mgr_bin 2>/dev/null)"

if [ ! -x "$BIN" ]; then
    BIN=""
    for candidate in \
        "$PLUGIN_DIR/target/release/agent-mgr" \
        "$PLUGIN_DIR/target/debug/agent-mgr" \
        "$PLUGIN_DIR/bin/agent-mgr" \
        "$TPM_DIR/target/release/agent-mgr" \
        "$TPM_DIR/target/debug/agent-mgr" \
        "$TPM_DIR/bin/agent-mgr"
    do
        if [ -x "$candidate" ]; then
            BIN="$candidate"
            break
        fi
    done
fi

if [ -z "$BIN" ]; then
    BIN="$(command -v agent-mgr 2>/dev/null)" || exit 0
fi
[ -n "$BIN" ] || exit 0

exec "$BIN" hook "$@"
