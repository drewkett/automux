use crate::{
    audit,
    config::Config,
    integrations, layout, process, tmux,
    util::{atomic_json, quote},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const FORMAT_VERSION: u32 = 1;
const SEP: &str = "\u{1f}";

#[derive(Debug, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    saved_at: u64,
    sessions: Vec<Session>,
}
#[derive(Debug, Serialize, Deserialize)]
struct Session {
    name: String,
    attached: bool,
    windows: Vec<Window>,
}
#[derive(Debug, Serialize, Deserialize)]
struct Window {
    index: u32,
    name: String,
    layout: String,
    active: bool,
    panes: Vec<Pane>,
}
#[derive(Debug, Serialize, Deserialize)]
struct Pane {
    id: u64,
    index: u32,
    cwd: String,
    current_command: String,
    title: String,
    active: bool,
    history_file: String,
    #[serde(default)]
    agent: Option<integrations::AgentSession>,
    #[serde(default)]
    nvim_session: Option<String>,
}

pub fn save(config: &Config, quiet: bool, force: bool) -> Result<()> {
    audit::record(config, "save_started", serde_json::json!({"force": force}));
    let server = tmux::server_identity().context("could not identify the tmux server")?;
    let path = snapshot_path(config, &server)?;
    if !force && recently_modified(&path, config.debounce) {
        return Ok(());
    }

    integrations::prune_dead_servers(config, &server);
    let history_dir = integrations::server_dir(config, "history", &server)?;
    let history_prefix = relative_to(&config.state_dir, &history_dir);

    let agents = integrations::registry(config).unwrap_or_default();
    let nvim_panes = integrations::nvim_registry(config).unwrap_or_default();
    let processes = process::Tree::capture();
    let sessions = tmux::lines(&[
        "list-sessions",
        "-F",
        &format!("#{{session_name}}{SEP}#{{session_attached}}"),
    ])?;
    let windows = tmux::lines(&["list-windows", "-a", "-F", &format!("#{{session_name}}{SEP}#{{window_index}}{SEP}#{{window_name}}{SEP}#{{window_layout}}{SEP}#{{window_active}}")] )?;
    let panes = tmux::lines(&["list-panes", "-a", "-F", &format!("#{{session_name}}{SEP}#{{window_index}}{SEP}#{{pane_id}}{SEP}#{{pane_index}}{SEP}#{{pane_current_path}}{SEP}#{{pane_current_command}}{SEP}#{{pane_title}}{SEP}#{{pane_active}}{SEP}#{{pane_pid}}")] )?;

    let mut by_window: BTreeMap<(String, u32), Vec<Pane>> = BTreeMap::new();
    for row in panes {
        if row.len() != 9 {
            continue;
        }
        let pane_id = row[2].trim_start_matches('%').parse::<u64>()?;
        let history_file = format!("{history_prefix}/pane-{pane_id}.ansi");
        let capture = tmux::output(&[
            "capture-pane",
            "-epJ",
            "-S",
            &config.history_limit,
            "-t",
            &row[2],
        ])?;
        fs::write(config.state_dir.join(&history_file), capture)?;
        let pane_pid = row[8].parse::<u32>().ok();
        let current_command = resolve_command(&row[5], pane_pid, processes.as_ref());
        // Session hooks provide the authoritative application identity. Tmux
        // may report the wrapper shell rather than the foreground TUI after a
        // restored command, so process-name matching would lose exact IDs on
        // the next save. Registry entries are scoped to this tmux server and
        // removed by SessionEnd hooks.
        // Automux also writes these records itself when it launches a resume,
        // so a registration can outlive the process it describes. Drop one the
        // pane is demonstrably no longer running; keep it when the process
        // table is unavailable, since losing an ID is worse than keeping a
        // stale one.
        let agent =
            agents
                .get(&row[2])
                .cloned()
                .filter(|agent| match (processes.as_ref(), pane_pid) {
                    (Some(tree), Some(pid)) => tree.runs(pid, &agent.agent),
                    _ => true,
                });
        let nvim_session = (current_command == "nvim" || nvim_panes.contains(&row[2]))
            .then(|| {
                let path = config
                    .state_dir
                    .join("nvim")
                    .join(tmux::server_key(&server))
                    .join(format!("pane-{pane_id}.vim"));
                path.is_file().then_some(path)
            })
            .flatten()
            .map(|path| path.display().to_string());
        by_window
            .entry((row[0].clone(), row[1].parse()?))
            .or_default()
            .push(Pane {
                id: pane_id,
                index: row[3].parse()?,
                cwd: row[4].clone(),
                current_command,
                title: row[6].clone(),
                active: row[7] == "1",
                history_file,
                agent,
                nvim_session,
            });
    }
    let mut by_session: BTreeMap<String, Vec<Window>> = BTreeMap::new();
    for row in windows {
        if row.len() != 5 {
            continue;
        }
        let index = row[1].parse()?;
        by_session.entry(row[0].clone()).or_default().push(Window {
            index,
            name: row[2].clone(),
            layout: row[3].clone(),
            active: row[4] == "1",
            panes: by_window
                .remove(&(row[0].clone(), index))
                .unwrap_or_default(),
        });
    }
    let snapshot = Snapshot {
        version: FORMAT_VERSION,
        saved_at: now(),
        sessions: sessions
            .into_iter()
            .filter(|r| r.len() == 2)
            .map(|r| Session {
                name: r[0].clone(),
                attached: r[1] != "0",
                windows: by_session.remove(&r[0]).unwrap_or_default(),
            })
            .collect(),
    };
    atomic_json(&path, &snapshot)?;
    prune_history(&history_dir, &snapshot);
    let agent_panes = snapshot
        .sessions
        .iter()
        .flat_map(|session| &session.windows)
        .flat_map(|window| &window.panes)
        .filter(|pane| pane.agent.is_some())
        .count();
    let nvim_sessions = snapshot
        .sessions
        .iter()
        .flat_map(|session| &session.windows)
        .flat_map(|window| &window.panes)
        .filter(|pane| pane.nvim_session.is_some())
        .count();
    audit::record(
        config,
        "save_completed",
        serde_json::json!({"sessions": snapshot.sessions.len(), "agent_panes": agent_panes, "nvim_panes": nvim_sessions}),
    );
    if !quiet {
        println!(
            "saved {} session(s) to {}",
            snapshot.sessions.len(),
            path.display()
        );
    }
    Ok(())
}

pub fn restore(config: &Config, replace_empty: bool) -> Result<()> {
    audit::record(
        config,
        "restore_started",
        serde_json::json!({"replace_empty": replace_empty}),
    );
    let server = tmux::server_identity().context("could not identify the tmux server")?;
    let data = fs::read(snapshot_path(config, &server)?).context("no automux snapshot found")?;
    let snapshot: Snapshot = serde_json::from_slice(&data).context("invalid automux snapshot")?;
    if snapshot.version != FORMAT_VERSION {
        bail!("unsupported snapshot version {}", snapshot.version);
    }
    let mut bootstrap = None;
    if replace_empty {
        if let Some(original) = current_session() {
            if empty_session(&original) {
                if snapshot.sessions.iter().any(|s| s.name == original) {
                    let temporary = unique_bootstrap_name();
                    tmux::run(&["rename-session", "-t", &original, &temporary])?;
                    bootstrap = Some(temporary);
                } else {
                    bootstrap = Some(original);
                }
            }
        }
    }
    let mut restored = 0;
    let mut failed = 0;
    for session in &snapshot.sessions {
        if tmux::has_session(&session.name) {
            continue;
        }
        // One unrestorable session must not cost every session after it.
        match restore_session(config, session) {
            Ok(()) => restored += 1,
            Err(error) => {
                failed += 1;
                eprintln!(
                    "automux: could not restore session {}: {error:#}",
                    session.name
                );
                audit::record(
                    config,
                    "restore_failed",
                    serde_json::json!({"session": session.name, "error": format!("{error:#}")}),
                );
            }
        }
    }
    if restored > 0 {
        let preferred = snapshot
            .sessions
            .iter()
            .find(|session| session.attached && tmux::has_session(&session.name))
            .or_else(|| {
                snapshot
                    .sessions
                    .iter()
                    .find(|session| tmux::has_session(&session.name))
            })
            .map(|session| session.name.clone());
        match bootstrap {
            Some(name) => {
                if let Some(preferred) = &preferred {
                    let _ = tmux::run(&["switch-client", "-t", preferred]);
                    audit::record(
                        config,
                        "client_switched",
                        serde_json::json!({"session": preferred}),
                    );
                }
                let _ = tmux::run(&["kill-session", "-t", &name]);
            }
            // Plain `tmux` loads the plugin while the server is starting up,
            // before any session or client exists, and only then runs the
            // implicit `new-session`. There is nothing to switch yet, so the
            // client would land in that brand new empty session instead of the
            // restored one. Hand the switch to the `client-attached` hook.
            None => {
                if let Some(preferred) = preferred {
                    write_pending_attach(config, &preferred);
                }
            }
        }
    }
    audit::record(
        config,
        "restore_completed",
        serde_json::json!({"restored_sessions": restored, "failed_sessions": failed}),
    );
    println!("restored {restored} session(s); existing sessions were left untouched");
    if failed > 0 {
        println!("{failed} session(s) could not be restored");
    }
    Ok(())
}

fn restore_session(config: &Config, session: &Session) -> Result<()> {
    restore_session_with(&tmux::Cli, config, session)
}

fn restore_session_with(
    server: &impl tmux::Server,
    config: &Config,
    session: &Session,
) -> Result<()> {
    const PLACEHOLDER: &str = "exec sleep 86400";

    for (window_number, window) in session.windows.iter().enumerate() {
        let Some(first) = window.panes.first() else {
            continue;
        };
        let target = format!("{}:{}", session.name, window.index);
        if window_number == 0 {
            // `-P -F` reports the window tmux actually created. The index
            // depends on the server's `base-index`, so it cannot be assumed to
            // be `0`, and the window id is also immune to name collisions.
            let created = server.output(&[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{window_id}\u{1f}#{window_index}",
                "-s",
                &session.name,
                "-n",
                &window.name,
                "-c",
                &first.cwd,
                PLACEHOLDER,
            ])?;
            let (window_id, actual_index) = created
                .trim()
                .split_once(SEP)
                .context("tmux did not report the created window")?;
            if actual_index != window.index.to_string() {
                server.run(&["move-window", "-s", window_id, "-t", &target])?;
            }
        } else {
            server.run(&[
                "new-window",
                "-d",
                "-t",
                &target,
                "-n",
                &window.name,
                "-c",
                &first.cwd,
                PLACEHOLDER,
            ])?;
        }
        for pane in window.panes.iter().skip(1) {
            server.run(&[
                "split-window",
                "-d",
                "-t",
                &target,
                "-c",
                &pane.cwd,
                PLACEHOLDER,
            ])?;
        }
        let ids = server
            .output(&["list-panes", "-t", &target, "-F", "#{pane_id}"])?
            .lines()
            .filter_map(|s| s.trim_start_matches('%').parse().ok())
            .collect::<Vec<_>>();
        if ids.len() != window.panes.len() {
            bail!(
                "created {} pane(s) for {}, expected {}",
                ids.len(),
                target,
                window.panes.len()
            );
        }
        let mapped = layout::remap(&window.layout, &ids)
            .with_context(|| format!("could not remap saved layout for {target}"))?;
        server.run(&["select-layout", "-t", &target, &mapped])?;

        // Full-screen applications must start only after tmux has assigned the
        // pane its final dimensions. In particular, Neovim calculates its
        // internal split sizes while sourcing a session file.
        for (pane, id) in window.panes.iter().zip(&ids) {
            let pane_target = format!("%{id}");
            server.run(&[
                "respawn-pane",
                "-k",
                "-t",
                &pane_target,
                "-c",
                &pane.cwd,
                &startup_command(config, pane),
            ])?;
            if let Some(session) = exact_resume(config, pane) {
                let _ = integrations::register_restored_agent(config, *id, session);
            }
            audit::record(
                config,
                "pane_launched",
                serde_json::json!({
                    "pane": pane_target,
                    "saved_command": pane.current_command,
                    "agent": pane.agent.as_ref().map(|agent| &agent.agent),
                    "agent_session_id": pane.agent.as_ref().map(|agent| &agent.session_id),
                    "nvim_session": pane.nvim_session,
                }),
            );
        }
        if let Some(active) = window.panes.iter().find(|p| p.active) {
            let pane_target = format!("{}.{}", target, active.index);
            let _ = server.run(&["select-pane", "-t", &pane_target, "-T", &active.title]);
        }
    }
    if let Some(active) = session.windows.iter().find(|w| w.active) {
        server.run(&[
            "select-window",
            "-t",
            &format!("{}:{}", session.name, active.index),
        ])?;
    }
    Ok(())
}

/// What a pane is really running.
///
/// `pane_current_command` is only the pane's immediate foreground process, and
/// it lies in two ways that matter here: a restored pane is wrapped in
/// `sh -lc`, and Claude Code sets its process title to its version (e.g.
/// `2.1.278`). The process tree already captured for this save answers both,
/// so it is the primary source and tmux's answer is the fallback.
fn resolve_command(command: &str, pane_pid: Option<u32>, tree: Option<&process::Tree>) -> String {
    let resolved = match (tree, pane_pid) {
        (Some(tree), Some(pid)) => tree.deepest_command(pid),
        _ => None,
    };
    match resolved {
        // A version-like title still has to be named for `startup_command` and
        // `nvim_session` to recognise it.
        Some(found) if version_like(found) => "claude".to_owned(),
        Some(found) => found.to_owned(),
        None if version_like(command) => "claude".to_owned(),
        None => command.to_owned(),
    }
}

fn version_like(command: &str) -> bool {
    !command.is_empty()
        && command.contains('.')
        && command.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// The saved agent session a restored pane should be relaunched with, if
/// resuming that agent is enabled.
fn exact_resume<'a>(config: &Config, pane: &'a Pane) -> Option<&'a integrations::AgentSession> {
    let session = pane.agent.as_ref()?;
    match session.agent.as_str() {
        "claude" if config.resume_claude => Some(session),
        "codex" if config.resume_codex => Some(session),
        _ => None,
    }
}

fn startup_command(config: &Config, pane: &Pane) -> String {
    let exact_resume = exact_resume(config, pane).map(|session| match session.agent.as_str() {
        "codex" => format!("codex resume {}", quote(&session.session_id)),
        _ => format!("claude --resume {}", quote(&session.session_id)),
    });
    let nvim_resume = pane.nvim_session.as_ref().and_then(|path| {
        config
            .resume_nvim
            .then(|| format!("nvim -S {}", quote(path)))
    });
    let fallback_resume = match pane.current_command.as_str() {
        "nvim" | "vim" if config.resume_nvim => Some("nvim -S Session.vim"),
        "claude" if config.resume_claude => Some("claude --continue"),
        "codex" if config.resume_codex => Some("codex resume --last"),
        _ => None,
    };
    let shell = match exact_resume
        .as_deref()
        .or(nvim_resume.as_deref())
        .or(fallback_resume)
    {
        Some(command) => format!("{command}; exec \"${{SHELL:-/bin/sh}}\" -l"),
        None => "exec \"${SHELL:-/bin/sh}\" -l".to_owned(),
    };
    if config.restore_scrollback {
        let history = config.state_dir.join(&pane.history_file);
        format!(
            "if [ -r {} ]; then command cat -- {}; fi; exec sh -lc {}",
            quote(&history.display().to_string()),
            quote(&history.display().to_string()),
            quote(&shell)
        )
    } else {
        format!("exec sh -lc {}", quote(&shell))
    }
}

pub fn status(config: &Config) -> Result<()> {
    let server = tmux::server_identity().context("could not identify the tmux server")?;
    let path = snapshot_path(config, &server)?;
    let snapshot: Snapshot =
        serde_json::from_slice(&fs::read(&path).context("no automux snapshot found")?)?;
    let windows: usize = snapshot.sessions.iter().map(|s| s.windows.len()).sum();
    let panes: usize = snapshot
        .sessions
        .iter()
        .flat_map(|s| &s.windows)
        .map(|w| w.panes.len())
        .sum();
    println!(
        "{} session(s), {windows} window(s), {panes} pane(s); saved at Unix time {}\n{}",
        snapshot.sessions.len(),
        snapshot.saved_at,
        path.display()
    );
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct PendingAttach {
    server: String,
    session: String,
    /// When the restore ran; only a session tmux created after this can be the
    /// throwaway one it made for the incoming client.
    restored_at: u64,
}

/// Finish a startup restore once a client is actually attached.
///
/// Only consumes a switch recorded by this server's own restore, and only
/// replaces the session tmux auto-created for the incoming client: one it
/// named itself (tmux numbers them from `0`) holding a single idle shell.
pub fn attach(config: &Config) -> Result<()> {
    audit::record(config, "client-attached", serde_json::json!({}));
    let Some(server) = tmux::server_identity() else {
        return Ok(());
    };
    let path = pending_attach_path(config, &server);
    let Ok(data) = fs::read(&path) else {
        return Ok(());
    };
    let pending: PendingAttach = serde_json::from_slice(&data)?;
    if pending.server != server {
        return Ok(());
    }
    // These hooks run detached, so `display-message` would report whichever
    // session tmux considers current rather than the one the client is in.
    let Some((client, current)) = first_client()? else {
        return Ok(());
    };
    let skip = if !tmux::has_session(&pending.session) {
        Some("restored session is gone")
    } else if current == pending.session {
        Some("already in the restored session")
    } else if current.parse::<u32>().is_err() {
        Some("client is in a session it was given a name")
    } else if !created_after(&current, pending.restored_at) {
        Some("client's session predates the restore")
    } else if !empty_session(&current) {
        Some("client's session is in use")
    } else {
        None
    };
    let _ = fs::remove_file(&path);
    if let Some(reason) = skip {
        audit::record(
            config,
            "attach_skipped",
            serde_json::json!({"session": pending.session, "current": current, "reason": reason}),
        );
        return Ok(());
    }
    tmux::run(&["switch-client", "-c", &client, "-t", &pending.session])?;
    audit::record(
        config,
        "client_switched",
        serde_json::json!({"session": pending.session, "replaced": current}),
    );
    let _ = tmux::run(&["kill-session", "-t", &current]);
    Ok(())
}

/// The first attached client and the session it is showing.
fn first_client() -> Result<Option<(String, String)>> {
    Ok(tmux::lines(&[
        "list-clients",
        "-F",
        &format!("#{{client_name}}{SEP}#{{client_session}}"),
    ])?
    .into_iter()
    .find(|row| row.len() == 2 && !row[0].is_empty() && !row[1].is_empty())
    .map(|row| (row[0].clone(), row[1].clone())))
}

fn write_pending_attach(config: &Config, session: &str) {
    let Some(server) = tmux::server_identity() else {
        return;
    };
    let pending = PendingAttach {
        server,
        session: session.to_owned(),
        restored_at: now(),
    };
    if let Ok(data) = serde_json::to_vec(&pending) {
        let _ = fs::write(pending_attach_path(config, &pending.server), data);
    }
}

fn pending_attach_path(config: &Config, server: &str) -> PathBuf {
    config
        .state_dir
        .join(format!("pending-attach-{}.json", tmux::socket_key(server)))
}

/// Delete scrollback captures for panes this server no longer has.
///
/// `prune_dead_servers` only reclaims whole directories once a server exits,
/// so without this a long-lived server keeps the full history of every pane it
/// ever had.
fn prune_history(history_dir: &Path, snapshot: &Snapshot) {
    let live: std::collections::BTreeSet<u64> = snapshot
        .sessions
        .iter()
        .flat_map(|session| &session.windows)
        .flat_map(|window| &window.panes)
        .map(|pane| pane.id)
        .collect();
    let Ok(entries) = fs::read_dir(history_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_prefix("pane-"))
            .and_then(|name| name.strip_suffix(".ansi"))
            .and_then(|id| id.parse::<u64>().ok())
        else {
            continue;
        };
        if !live.contains(&id) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// `path` expressed relative to `base`, using forward slashes.
fn relative_to(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Where this tmux server keeps its snapshot.
///
/// Keyed by socket rather than by full server identity so it survives the
/// server restart it exists to recover from, while a second server on another
/// `-L` socket still gets a snapshot of its own instead of clobbering this one.
fn snapshot_path(config: &Config, server: &str) -> Result<PathBuf> {
    let directory = config.state_dir.join("snapshots");
    fs::create_dir_all(&directory)?;
    Ok(directory.join(format!("{}.json", tmux::socket_key(server))))
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn recently_modified(path: &Path, duration: std::time::Duration) -> bool {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|e| e < duration)
        .unwrap_or(false)
}
fn current_session() -> Option<String> {
    tmux::output(&["display-message", "-p", "#{session_name}"])
        .ok()
        .map(|s| s.trim().into())
}
fn unique_bootstrap_name() -> String {
    format!("__automux_bootstrap_{}", std::process::id())
}
/// Whether a session was created at or after `timestamp`.
///
/// Missing or unparseable output reads as "no": killing a session the client
/// was already using is far worse than declining to replace one.
fn created_after(name: &str, timestamp: u64) -> bool {
    tmux::output(&["display-message", "-p", "-t", name, "#{session_created}"])
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|created| created >= timestamp)
        .unwrap_or(false)
}

/// A session holding nothing but one idle login shell in one window.
fn empty_session(name: &str) -> bool {
    let single_window = tmux::output(&["list-windows", "-t", name, "-F", "#{window_id}"])
        .map(|s| s.lines().count() == 1)
        .unwrap_or(false);
    if !single_window {
        return false;
    }
    // Compare against the configured shell rather than a fixed list, so users
    // of nushell or elvish are not excluded.
    let shell = tmux::output(&["display-message", "-p", "#{default-shell}"])
        .ok()
        .and_then(|value| {
            Path::new(value.trim())
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
    tmux::output(&["list-panes", "-t", name, "-F", "#{pane_current_command}"])
        .map(|s| {
            s.lines().count() == 1
                && s.lines().all(|command| {
                    matches!(command, "bash" | "zsh" | "fish" | "sh")
                        || shell.as_deref() == Some(command)
                })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A tmux server that records what it is asked to do.
    ///
    /// `base_index` is the setting that used to break restore: the window
    /// `new-session` creates is not necessarily `:0`.
    struct FakeServer {
        base_index: u32,
        /// Simulates tmux failing to create one of the requested panes.
        lose_a_pane: bool,
        calls: RefCell<Vec<Vec<String>>>,
        next_pane: RefCell<u64>,
        panes: RefCell<Vec<u64>>,
    }

    impl FakeServer {
        fn new(base_index: u32) -> Self {
            Self {
                base_index,
                lose_a_pane: false,
                calls: RefCell::new(Vec::new()),
                next_pane: RefCell::new(0),
                panes: RefCell::new(Vec::new()),
            }
        }

        fn add_pane(&self) {
            let mut next = self.next_pane.borrow_mut();
            self.panes.borrow_mut().push(*next);
            *next += 1;
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }

        fn commands(&self) -> Vec<String> {
            self.calls()
                .iter()
                .map(|call| call[0].clone())
                .collect::<Vec<_>>()
        }
    }

    impl tmux::Server for FakeServer {
        fn output(&self, args: &[&str]) -> Result<String> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|arg| (*arg).to_owned()).collect());
            Ok(match args[0] {
                "new-session" => {
                    self.panes.borrow_mut().clear();
                    self.add_pane();
                    format!("@7{SEP}{}\n", self.base_index)
                }
                "new-window" => {
                    self.panes.borrow_mut().clear();
                    self.add_pane();
                    String::new()
                }
                "split-window" => {
                    self.add_pane();
                    String::new()
                }
                "list-panes" => self
                    .panes
                    .borrow()
                    .iter()
                    .take(self.panes.borrow().len() - usize::from(self.lose_a_pane))
                    .map(|id| format!("%{id}\n"))
                    .collect(),
                _ => String::new(),
            })
        }
    }

    fn config() -> (tempfile::TempDir, Config) {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: temp.path().to_path_buf(),
            history_limit: "-".into(),
            debounce: std::time::Duration::from_secs(5),
            restore_scrollback: false,
            resume_nvim: false,
            resume_claude: false,
            resume_codex: false,
        };
        (temp, config)
    }

    fn pane(id: u64, index: u32) -> Pane {
        Pane {
            id,
            index,
            cwd: "/tmp".into(),
            current_command: "zsh".into(),
            title: format!("pane {index}"),
            active: index == 0,
            history_file: format!("history/k/pane-{id}.ansi"),
            agent: None,
            nvim_session: None,
        }
    }

    fn session(first_window: u32) -> Session {
        Session {
            name: "work".into(),
            attached: true,
            windows: vec![
                Window {
                    index: first_window,
                    name: "edit".into(),
                    layout: "aaaa,80x24,0,0,0".into(),
                    active: true,
                    panes: vec![pane(0, 0)],
                },
                Window {
                    index: first_window + 1,
                    name: "run".into(),
                    layout: "bbbb,80x24,0,0{40x24,0,0,1,39x24,41,0,2}".into(),
                    active: false,
                    panes: vec![pane(1, 0), pane(2, 1)],
                },
            ],
        }
    }

    #[test]
    fn the_first_window_is_moved_by_id_when_base_index_differs() {
        let (_temp, config) = config();
        let server = FakeServer::new(1);

        restore_session_with(&server, &config, &session(0)).unwrap();

        // tmux created the window at :1; the snapshot wants it at :0.
        let move_window = server
            .calls()
            .into_iter()
            .find(|call| call[0] == "move-window")
            .expect("the window should be moved to its saved index");
        assert_eq!(move_window, ["move-window", "-s", "@7", "-t", "work:0"]);
    }

    #[test]
    fn a_matching_base_index_moves_nothing() {
        let (_temp, config) = config();
        let server = FakeServer::new(1);

        restore_session_with(&server, &config, &session(1)).unwrap();

        assert!(!server.commands().contains(&"move-window".to_owned()));
    }

    #[test]
    fn windows_and_panes_are_created_in_order() {
        let (_temp, config) = config();
        let server = FakeServer::new(0);

        restore_session_with(&server, &config, &session(0)).unwrap();

        assert_eq!(
            server.commands(),
            [
                "new-session",
                "list-panes",
                "select-layout",
                "respawn-pane",
                "select-pane",
                "new-window",
                "split-window",
                "list-panes",
                "select-layout",
                "respawn-pane",
                "respawn-pane",
                "select-pane",
                "select-window",
            ]
        );
    }

    #[test]
    fn a_pane_count_mismatch_is_an_error() {
        let (_temp, config) = config();
        let mut server = FakeServer::new(0);
        server.lose_a_pane = true;
        let mut session = session(0);
        session.windows[0].panes.push(pane(3, 1));

        let error = restore_session_with(&server, &config, &session).unwrap_err();
        assert!(error.to_string().contains("expected 2"), "{error}");
    }

    #[test]
    fn startup_command_replays_scrollback_before_the_shell() {
        let (_temp, mut config) = config();
        config.restore_scrollback = true;
        let command = startup_command(&config, &pane(4, 0));

        assert!(command.starts_with("if [ -r "), "{command}");
        assert!(command.contains("pane-4.ansi"), "{command}");
        assert!(command.contains("exec sh -lc"), "{command}");
    }

    #[test]
    fn resume_commands_honour_the_configuration() {
        let (_temp, mut config) = config();
        let mut pane = pane(5, 0);
        pane.agent = Some(integrations::AgentSession {
            agent: "codex".into(),
            session_id: "abc'123".into(),
            server: "/tmp/sock,1".into(),
        });

        assert!(!startup_command(&config, &pane).contains("codex resume"));

        config.resume_codex = true;
        let command = startup_command(&config, &pane);
        assert!(command.contains("codex resume"), "{command}");
        // The session ID is quoted, so an embedded quote cannot break out.
        assert!(!command.contains("abc'123"), "{command}");
    }

    #[test]
    fn resolve_command_prefers_the_process_tree() {
        assert_eq!(resolve_command("sh", None, None), "sh");
        assert_eq!(resolve_command("2.1.278", None, None), "claude");
    }

    #[test]
    fn stale_scrollback_files_are_removed() {
        let (temp, _config) = config();
        let history = temp.path().join("history");
        fs::create_dir_all(&history).unwrap();
        for name in ["pane-0.ansi", "pane-9.ansi", "notes.txt"] {
            fs::write(history.join(name), "x").unwrap();
        }

        let snapshot = Snapshot {
            version: FORMAT_VERSION,
            saved_at: 0,
            sessions: vec![session(0)],
        };
        prune_history(&history, &snapshot);

        assert!(history.join("pane-0.ansi").exists());
        assert!(!history.join("pane-9.ansi").exists());
        // Anything automux did not write is left alone.
        assert!(history.join("notes.txt").exists());
    }
}
