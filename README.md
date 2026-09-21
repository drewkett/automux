# automux

> **Note:** This project was written almost entirely by AI (Claude Code). It
> works for my setup, but review the code yourself before trusting it with
> your workspace.

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

- `tmux` creates a new session. If no server is running, it starts one first,
  which loads the plugin and reconstructs the saved sessions. tmux then makes
  its own empty session for the incoming client, so automux moves the client to
  the session that was attached when the snapshot was saved and discards the
  empty one.
- `tmux attach` (or `tmux a`) connects a new client to an existing session.
- `prefix + d` detaches the current client but leaves the server and sessions
  running, so reconnect with `tmux a`.
- `prefix + X` saves the workspace and stops the server. After that, run
  `tmux`; the new server loads the plugin and automux reconstructs the saved
  sessions.

Restored panes are fresh shells in their saved directories. Automux restores
the tmux structure and scrollback, but it cannot revive arbitrary processes
that were running inside the old panes.

Plain `tmux` therefore works for both a cold start and a reboot. When multiple
sessions exist, `tmux attach` picks one arbitrarily; use `tmux list-sessions`
and `tmux attach -t NAME` to select a specific session.

Automux only replaces a session tmux named itself (they are numbered from `0`)
that holds a single idle shell, and only once per restore, so a session you
created deliberately is never closed.

Automux logs restores, client switching, and shutdowns as JSON Lines to
`$XDG_STATE_HOME/automux/automux.log` (or `~/.local/state/automux/automux.log`).

## Configuration

Set options before loading the plugin:

```tmux
set -g @automux-auto-restore on       # default: on
set -g @automux-save-interval 5       # debounce hook-triggered saves, seconds
set -g @automux-history-limit -       # tmux capture-pane start; '-' means all
set -g @automux-restore-scrollback on # replay captured ANSI output

# Opt-in: relaunch these programs in restored panes (default: none)
set -g @automux-resume "nvim claude codex"
```

Resuming needs each program's integration, installed once after building
automux:

```sh
./target/release/automux install-hooks
```

The command detects which tools are present on `PATH`, preserves existing
Claude and Codex configuration, and can be run repeatedly without adding
duplicate hooks. Codex requires reviewing and trusting the newly installed hooks
with `/hooks` before they run. Outside tmux, the integrations do nothing.

- **Claude Code and Codex**: `SessionStart` and `SessionEnd` hooks label the pane
  with the exact session ID through the tmux pane option `@automux-agent`, and
  a restored pane runs `claude --resume ID` or `codex resume ID`.
- **Neovim**: a small global plugin at `~/.config/nvim/plugin/automux.lua` (or
  under `$XDG_CONFIG_HOME`) keeps a session file up to date and records its
  path in the pane option `@automux-nvim`. A restored pane runs `nvim -S` on it.

Pane options belong to one tmux server and vanish with their pane, so a reused
pane ID can never inherit another pane's session. When automux resumes an
agent it labels the new pane itself, because Codex does not run `SessionStart`
on resume. A label is ignored once its pane is back at the login shell, in case
the program exited without running its cleanup.

State defaults to `$XDG_STATE_HOME/automux` or `~/.local/state/automux`. Override
it with `@automux-state-dir` or `AUTOMUX_STATE_DIR`. Each tmux socket has one
snapshot, written atomically, with scrollback embedded in it.

## Current limitations

- Scrollback is replayed as output into a new pane. It is visually faithful
  (including ANSI escapes) but is not an internal tmux history-file import.
- Pane foreground command detection does not capture pipelines, environment
  variables, SSH connections, unsaved editor buffers, or arbitrary processes.
- Sessions that already exist during restore are skipped to avoid data loss.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
