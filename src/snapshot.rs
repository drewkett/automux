use crate::{audit, config::Config, integrations, layout, process, tmux};
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
    let path = snapshot_path(config);
    if !force && recently_modified(&path, config.debounce) {
        return Ok(());
    }

    let server = tmux::server_identity().context("could not identify the tmux server")?;
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
                let nvim = config.state_dir.join("nvim");
                let name = format!("pane-{pane_id}.vim");
                // Prefer this server's own directory, but accept a session
                // file written by the pre-scoping Neovim plugin.
                [
                    nvim.join(tmux::server_key(&server)).join(&name),
                    nvim.join(&name),
                ]
                .into_iter()
                .find(|path| path.is_file())
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
    let agent_panes = snapshot
        .sessions
        .iter()
        .flat_map(|session| &session.windows)
        .flat_map(|window| &window.panes)
        .filter(|pane| pane.agent.is_some())
        .count();
    let nvim_panes = snapshot
        .sessions
        .iter()
        .flat_map(|session| &session.windows)
        .flat_map(|window| &window.panes)
        .filter(|pane| pane.nvim_session.is_some())
        .count();
    audit::record(
        config,
        "save_completed",
        serde_json::json!({"sessions": snapshot.sessions.len(), "agent_panes": agent_panes, "nvim_panes": nvim_panes}),
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
    let data = fs::read(snapshot_path(config)).context("no automux snapshot found")?;
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
    for session in &snapshot.sessions {
        if tmux::has_session(&session.name) {
            continue;
        }
        restore_session(config, session)?;
        restored += 1;
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
        serde_json::json!({"restored_sessions": restored}),
    );
    println!("restored {restored} session(s); existing sessions were left untouched");
    Ok(())
}

fn restore_session(config: &Config, session: &Session) -> Result<()> {
    const PLACEHOLDER: &str = "exec sleep 86400";

    for (window_number, window) in session.windows.iter().enumerate() {
        let Some(first) = window.panes.first() else {
            continue;
        };
        let target = format!("{}:{}", session.name, window.index);
        if window_number == 0 {
            tmux::run(&[
                "new-session",
                "-d",
                "-s",
                &session.name,
                "-n",
                &window.name,
                "-c",
                &first.cwd,
                PLACEHOLDER,
            ])?;
            let actual = format!("{}:0", session.name);
            if window.index != 0 {
                tmux::run(&["move-window", "-s", &actual, "-t", &target])?;
            }
        } else {
            tmux::run(&[
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
            tmux::run(&[
                "split-window",
                "-d",
                "-t",
                &target,
                "-c",
                &pane.cwd,
                PLACEHOLDER,
            ])?;
        }
        let ids = tmux::output(&["list-panes", "-t", &target, "-F", "#{pane_id}"])?
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
        tmux::run(&["select-layout", "-t", &target, &mapped])?;

        // Full-screen applications must start only after tmux has assigned the
        // pane its final dimensions. In particular, Neovim calculates its
        // internal split sizes while sourcing a session file.
        for (pane, id) in window.panes.iter().zip(&ids) {
            let pane_target = format!("%{id}");
            tmux::run(&[
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
            let _ = tmux::run(&["select-pane", "-t", &pane_target, "-T", &active.title]);
        }
    }
    if let Some(active) = session.windows.iter().find(|w| w.active) {
        tmux::run(&[
            "select-window",
            "-t",
            &format!("{}:{}", session.name, active.index),
        ])?;
    }
    Ok(())
}

/// Claude Code sets its process title to its version (e.g. `2.1.278`), so tmux
/// reports that instead of `claude`. Fall back to the pane's child process name.
fn resolve_command(command: &str, pane_pid: Option<u32>, tree: Option<&process::Tree>) -> String {
    let version_like = !command.is_empty()
        && command.contains('.')
        && command.chars().all(|c| c.is_ascii_digit() || c == '.');
    if !version_like {
        return command.to_owned();
    }
    match (tree, pane_pid) {
        (Some(tree), Some(pid)) if tree.runs(pid, "claude") => "claude".to_owned(),
        _ => command.to_owned(),
    }
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
    let path = snapshot_path(config);
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
}

/// Finish a startup restore once a client is actually attached.
///
/// Only consumes a switch recorded by this server's own restore, and only
/// replaces the session tmux auto-created for the incoming client: one it
/// named itself (tmux numbers them from `0`) holding a single idle shell.
pub fn attach(config: &Config) -> Result<()> {
    audit::record(config, "client-attached", serde_json::json!({}));
    let path = pending_attach_path(config);
    let Ok(data) = fs::read(&path) else {
        return Ok(());
    };
    let pending: PendingAttach = serde_json::from_slice(&data)?;
    if tmux::server_identity().as_deref() != Some(pending.server.as_str()) {
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
    };
    if let Ok(data) = serde_json::to_vec(&pending) {
        let _ = fs::write(pending_attach_path(config), data);
    }
}

fn pending_attach_path(config: &Config) -> PathBuf {
    config.state_dir.join("pending-attach.json")
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

fn snapshot_path(config: &Config) -> PathBuf {
    config.state_dir.join("snapshot.json")
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
fn atomic_json(path: &Path, value: &Snapshot) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}
fn current_session() -> Option<String> {
    tmux::output(&["display-message", "-p", "#{session_name}"])
        .ok()
        .map(|s| s.trim().into())
}
fn unique_bootstrap_name() -> String {
    format!("__automux_bootstrap_{}", std::process::id())
}
fn empty_session(name: &str) -> bool {
    tmux::output(&["list-panes", "-t", name, "-F", "#{pane_current_command}"])
        .map(|s| {
            s.lines().count() == 1
                && s.lines()
                    .all(|c| matches!(c, "bash" | "zsh" | "fish" | "sh"))
        })
        .unwrap_or(false)
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
