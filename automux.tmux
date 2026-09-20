#!/usr/bin/env bash
set -euo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${AUTOMUX_BIN:-$CURRENT_DIR/target/release/automux}"

if [[ ! -x "$BIN" ]]; then
  tmux display-message "automux: binary not found; run cargo build --release in $CURRENT_DIR"
  exit 0
fi

tmux bind-key S run-shell -b "$BIN save --force"
tmux bind-key R run-shell -b "$BIN restore"
# Explicit clean shutdown: save synchronously before stopping the server.
tmux bind-key X confirm-before -p "Save workspace and stop tmux? (y/n)" \
  "run-shell '$BIN save --force' ; kill-server"

auto_restore="$(tmux show-option -gqv @automux-auto-restore)"
if [[ "${auto_restore:-on}" == "on" ]]; then
  "$BIN" restore --replace-empty >/dev/null 2>&1 || true
fi

# Install hooks after restore so reconstruction events cannot overwrite the
# snapshot that is currently being read.
save_cmd="run-shell -b '$BIN save --quiet'"
tmux set-hook -g after-new-session "$save_cmd"
tmux set-hook -g after-new-window "$save_cmd"
tmux set-hook -g window-unlinked "$save_cmd"
tmux set-hook -g after-split-window "$save_cmd"
tmux set-hook -g after-kill-pane "$save_cmd"
tmux set-hook -g after-resize-pane "$save_cmd"
tmux set-hook -g after-select-layout "$save_cmd"
# Detach is the closest reliable tmux equivalent to application exit. Force it
# past the debounce interval so the latest working directories are persisted.
tmux set-hook -g client-detached "run-shell -b '$BIN save --quiet --force'"
