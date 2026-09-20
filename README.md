# automux

`automux` is a Rust-backed tmux plugin that continuously snapshots your tmux
workspace and reconstructs it after tmux or the machine restarts.

It preserves:

- sessions, windows, pane counts, names, indexes, and the exact split layout
- each pane's working directory, title, active pane/window, and ANSI scrollback
- optional best-effort resume for Neovim, Claude Code, and Codex sessions

It cannot checkpoint arbitrary Unix processes. By default restored panes open a
fresh login shell after replaying their captured scrollback. Resume adapters are
explicitly opt-in because starting interactive tools automatically can be
surprising.

## Install

The plugin requires tmux, Rust 1.74 or newer, and a POSIX shell.

```sh
git clone https://github.com/drewkett/automux ~/.tmux/plugins/automux
cd ~/.tmux/plugins/automux
cargo build --release
```

Then add this to `~/.tmux.conf` (TPM users can use the usual `set -g
@plugin ...` entry instead):

```tmux
run-shell ~/.tmux/plugins/automux/automux.tmux
```

Reload the file with `tmux source-file ~/.tmux.conf`. `prefix + S` forces a
snapshot and `prefix + R` restores missing sessions. Existing sessions are
never overwritten. Use `prefix + X` for a clean tmux shutdown: automux saves
synchronously, then stops the tmux server after confirmation. A normal client
detach also forces a final snapshot.

## Starting and reattaching

tmux distinguishes between its server, sessions, and clients:

- `tmux` creates a new session. If no server is running, it starts one first.
- `tmux attach` (or `tmux a`) connects a new client to an existing session.
- `prefix + d` detaches the current client but leaves the server and sessions
  running, so reconnect with `tmux a`.
- `prefix + X` saves the workspace and stops the server. After that, run
  `tmux`; the new server loads the plugin and automux reconstructs the saved
  sessions.

Restored panes are fresh shells in their saved directories. Automux restores
the tmux structure and scrollback, but it cannot revive arbitrary processes
that were running inside the old panes.

If you want one command that attaches when a server is already running and
starts tmux otherwise, add a separate shell alias rather than replacing the
`tmux` command itself:

```sh
alias ta='tmux attach-session 2>/dev/null || tmux new-session'
```

Then use `ta` for both normal reattachment and startup after a reboot or clean
shutdown. When multiple sessions exist, plain `tmux attach` chooses one; use
`tmux list-sessions` and `tmux attach -t NAME` to select a specific session.

## Configuration

Set options before loading the plugin:

```tmux
set -g @automux-auto-restore on       # default: on
set -g @automux-save-interval 5       # debounce hook-triggered saves, seconds
set -g @automux-history-limit -       # tmux capture-pane start; '-' means all
set -g @automux-restore-scrollback on # replay captured ANSI output

# Opt-in best-effort adapters:
set -g @automux-resume-nvim on
set -g @automux-resume-claude on
set -g @automux-resume-codex on
```

For reliable Neovim, Claude Code, and Codex resume, install their global
integrations once after building automux:

```sh
./target/release/automux install-hooks
```

The command detects which tools are present on `PATH`, preserves existing
Claude and Codex configuration, and can be run repeatedly without adding
duplicate hooks. For Neovim it installs a small global plugin at
`~/.config/nvim/plugin/automux.lua` (or under `$XDG_CONFIG_HOME`) that maintains
a session file for each tmux pane. The Claude and Codex hooks associate each
pane with the tool's exact session ID and remove that association when the
session ends. Registrations are scoped to the current tmux server so reused
pane IDs cannot pick up stale sessions. Codex requires reviewing and trusting
the newly installed hooks with `/hooks` before they run. The corresponding
`@automux-resume-*` option must still be enabled. Outside tmux, the integrations
do nothing.

With the integration installed, Neovim restores its pane-specific session.
Without it, the adapter falls back to `Session.vim` in the pane's working
directory. One manual workflow is `:mksession! Session.vim` before exiting.
Claude and Codex similarly resume the exact saved session ID when hook data is
available, otherwise falling back to `claude --continue` and
`codex resume --last`. Resume commands deliberately run only when the saved
pane's foreground command was the matching executable.

State defaults to `$XDG_STATE_HOME/automux` or `~/.local/state/automux`. Override
it with `@automux-state-dir` or `AUTOMUX_STATE_DIR`. The snapshot is written
atomically; scrollback lives in a separate file per pane.

## Current limitations

- Scrollback is replayed as output into a new pane. It is visually faithful
  (including ANSI escapes) but is not an internal tmux history-file import.
- Pane foreground command detection does not capture pipelines, environment
  variables, SSH connections, unsaved editor buffers, or arbitrary processes.
- Sessions that already exist during restore are skipped to avoid data loss.
