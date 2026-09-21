use crate::{
    audit,
    config::Config,
    integrations::{self, AGENT_OPTION, NVIM_OPTION},
    layout,
    tmux::{self, SEP},
    util::{atomic_json, quote},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Version 3 stores a pane's agent as its `<agent>:<session id>` label.
const FORMAT_VERSION: u32 = 3;

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
    index: u32,
    cwd: String,
    title: String,
    active: bool,
    /// ANSI-formatted history, replayed into the restored pane.
    scrollback: String,
    /// `<agent>:<session id>`, as the agent hooks label the pane.
    agent: Option<String>,
    nvim_session: Option<String>,
}

impl Snapshot {
    fn panes(&self) -> impl Iterator<Item = &Pane> {
        self.sessions
            .iter()
            .flat_map(|session| &session.windows)
            .flat_map(|window| &window.panes)
    }
}

pub fn save(config: &Config, quiet: bool, force: bool) -> Result<()> {
    let server = tmux::server_identity().context("could not identify the tmux server")?;
    let path = snapshot_path(config, &server)?;
    if !force && recently_modified(&path, config.debounce) {
        return Ok(());
    }
    let started = SystemTime::now();
    let shell = default_shell();
    let panes = tmux::table(
        &["list-panes", "-a"],
        &[
            "session_name",
            "window_index",
            "pane_id",
            "pane_index",
            "pane_current_path",
            "pane_current_command",
            "pane_title",
            "pane_active",
            AGENT_OPTION,
            NVIM_OPTION,
        ],
    )?;
    let mut by_window: BTreeMap<(String, u32), Vec<Pane>> = BTreeMap::new();
    for row in panes {
        let [session, window, id, index, cwd, command, title, active, agent, nvim] = &row[..]
        else {
            unreachable!("tmux::table returns rows of the requested width")
        };
        let scrollback = tmux::output(&[
            "capture-pane",
            "-epJ",
            "-S",
            &config.history_limit,
            "-t",
            id,
        ])?;
        // The agent hooks and the Neovim plugin label a pane with what it runs
        // and clear the label on exit, but a crash skips that. A pane back at
        // the login shell is running neither, whatever its label says.
        let running = !at_login_shell(command, shell.as_deref());
        let label = |value: &String| Some(value.clone()).filter(|v| running && !v.is_empty());
        by_window
            .entry((session.clone(), window.parse()?))
            .or_default()
            .push(Pane {
                index: index.parse()?,
                cwd: cwd.clone(),
                title: title.clone(),
                active: active == "1",
                scrollback,
                agent: label(agent),
                nvim_session: label(nvim),
            });
    }
    let windows = tmux::table(
        &["list-windows", "-a"],
        &[
            "session_name",
            "window_index",
            "window_name",
            "window_layout",
            "window_active",
        ],
    )?;
    let mut by_session: BTreeMap<String, Vec<Window>> = BTreeMap::new();
    for row in windows {
        let [session, index, name, layout, active] = &row[..] else {
            unreachable!("tmux::table returns rows of the requested width")
        };
        let index = index.parse()?;
        by_session.entry(session.clone()).or_default().push(Window {
            index,
            name: name.clone(),
            layout: layout.clone(),
            active: active == "1",
            panes: by_window
                .remove(&(session.clone(), index))
                .unwrap_or_default(),
        });
    }
    let sessions = tmux::table(&["list-sessions"], &["session_name", "session_attached"])?;
    let snapshot = Snapshot {
        version: FORMAT_VERSION,
        saved_at: now(),
        sessions: sessions
            .into_iter()
            .map(|r| Session {
                name: r[0].clone(),
                attached: r[1] != "0",
                windows: by_session.remove(&r[0]).unwrap_or_default(),
            })
            .collect(),
    };
    atomic_json(&path, &snapshot)?;
    prune_nvim_sessions(&integrations::nvim_dir(config, &server), &snapshot, started);
    if !quiet {
        println!(
            "saved {} session(s) to {}",
            snapshot.sessions.len(),
            path.display()
        );
    }
    Ok(())
}

/// Restore every saved session that does not exist yet.
///
/// `startup` is set when the plugin loads with a new server. Plain `tmux`
/// loads it before creating the session its client lands in, so there is
/// nothing to switch yet; the attach hooks finish the job.
pub fn restore(config: &Config, startup: bool) -> Result<()> {
    let server = tmux::server_identity().context("could not identify the tmux server")?;
    let snapshot = load(config, &server)?;
    let mut restored = 0;
    let mut failed = 0;
    for session in &snapshot.sessions {
        if tmux::has_session(&session.name) {
            continue;
        }
        // One unrestorable session must not cost every session after it.
        match restore_session_with(&tmux::Cli, config, session) {
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
    if startup && restored > 0 {
        let preferred = snapshot
            .sessions
            .iter()
            .find(|session| session.attached && tmux::has_session(&session.name))
            .or_else(|| {
                snapshot
                    .sessions
                    .iter()
                    .find(|session| tmux::has_session(&session.name))
            });
        if let Some(preferred) = preferred {
            write_pending_attach(&preferred.name);
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

fn load(config: &Config, server: &str) -> Result<Snapshot> {
    let data = fs::read(snapshot_path(config, server)?).context("no automux snapshot found")?;
    let snapshot: Snapshot = serde_json::from_slice(&data)
        .context("invalid automux snapshot (a snapshot from an older automux cannot be read)")?;
    if snapshot.version != FORMAT_VERSION {
        bail!("unsupported snapshot version {}", snapshot.version);
    }
    Ok(snapshot)
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
            let scrollback = stage_scrollback(config, pane, *id)?;
            server.run(&[
                "respawn-pane",
                "-k",
                "-t",
                &pane_target,
                "-c",
                &pane.cwd,
                &startup_command(config, pane, scrollback.as_deref()),
            ])?;
            // Codex does not run its SessionStart hook on resume, so label the
            // pane directly or the next snapshot would lose the session.
            if let Some((agent, id)) = resumed_agent(config, pane) {
                let label = format!("{agent}:{id}");
                let _ = server.run(&["set-option", "-p", "-t", &pane_target, AGENT_OPTION, &label]);
            }
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

/// The agent and session id to resume in a pane, if its agent is enabled.
fn resumed_agent<'a>(config: &Config, pane: &'a Pane) -> Option<(&'a str, &'a str)> {
    let (agent, id) = pane.agent.as_deref()?.split_once(':')?;
    (config.resumes(agent) && !id.is_empty()).then_some((agent, id))
}

fn resume_command(config: &Config, pane: &Pane) -> Option<String> {
    if let Some((agent, id)) = resumed_agent(config, pane) {
        let id = quote(id);
        let command = match agent {
            "codex" => format!("codex resume {id}"),
            _ => format!("claude --resume {id}"),
        };
        // Automux labelled the pane itself, so it clears the label too.
        return Some(format!(
            "{command}; tmux set-option -pu -t \"$TMUX_PANE\" {AGENT_OPTION}"
        ));
    }
    let session = pane.nvim_session.as_ref()?;
    config
        .resumes("nvim")
        .then(|| format!("nvim -S {}", quote(session)))
}

fn startup_command(config: &Config, pane: &Pane, scrollback: Option<&Path>) -> String {
    let login = r#"exec "${SHELL:-/bin/sh}" -l"#;
    let shell = match resume_command(config, pane) {
        Some(command) => format!("{command}; {login}"),
        None => login.to_owned(),
    };
    match scrollback {
        Some(path) => {
            let path = quote(&path.display().to_string());
            format!(
                "command cat -- {path}; rm -f -- {path}; exec sh -lc {}",
                quote(&shell)
            )
        }
        None => format!("exec sh -lc {}", quote(&shell)),
    }
}

/// Write a pane's scrollback where its startup command prints and deletes it.
fn stage_scrollback(config: &Config, pane: &Pane, id: u64) -> Result<Option<PathBuf>> {
    if !config.restore_scrollback || pane.scrollback.is_empty() {
        return Ok(None);
    }
    let directory = config.state_dir.join("scrollback");
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{}-{id}.ansi", std::process::id()));
    fs::write(&path, &pane.scrollback)?;
    Ok(Some(path))
}

pub fn status(config: &Config) -> Result<()> {
    let server = tmux::server_identity().context("could not identify the tmux server")?;
    let snapshot = load(config, &server)?;
    let windows: usize = snapshot.sessions.iter().map(|s| s.windows.len()).sum();
    println!(
        "{} session(s), {windows} window(s), {} pane(s); saved at Unix time {}\n{}",
        snapshot.sessions.len(),
        snapshot.panes().count(),
        snapshot.saved_at,
        snapshot_path(config, &server)?.display()
    );
    Ok(())
}

/// Server option holding `<session><SEP><restore time>` between a startup
/// restore and the first attach. It lives in the tmux server, so it can never
/// outlive the server or leak into another one. The time matters because only
/// a session tmux created after the restore can be the throwaway one it made
/// for the incoming client.
const PENDING_OPTION: &str = "@automux-pending-attach";

/// Finish a startup restore once a client is actually attached.
///
/// Only consumes a switch recorded by this server's own restore, and only
/// replaces the session tmux auto-created for the incoming client: one it
/// named itself (tmux numbers them from `0`) holding a single idle shell.
pub fn attach(config: &Config) -> Result<()> {
    audit::record(config, "client-attached", serde_json::json!({}));
    let value = tmux::output(&["show-options", "-gqv", PENDING_OPTION]).unwrap_or_default();
    let Some((session, restored_at)) = value.trim_end_matches('\n').split_once(SEP) else {
        return Ok(());
    };
    let restored_at: u64 = restored_at.parse().context("invalid pending attach")?;
    // These hooks run detached, so `display-message` would report whichever
    // session tmux considers current rather than the one the client is in.
    let Some((client, current)) = first_client()? else {
        return Ok(());
    };
    let skip = if !tmux::has_session(session) {
        Some("restored session is gone")
    } else if current == session {
        Some("already in the restored session")
    } else if current.parse::<u32>().is_err() {
        Some("client is in a session it was given a name")
    } else if !created_after(&current, restored_at) {
        Some("client's session predates the restore")
    } else if !empty_session(&current) {
        Some("client's session is in use")
    } else {
        None
    };
    let _ = tmux::run(&["set-option", "-gu", PENDING_OPTION]);
    if let Some(reason) = skip {
        audit::record(
            config,
            "attach_skipped",
            serde_json::json!({"session": session, "current": current, "reason": reason}),
        );
        return Ok(());
    }
    tmux::run(&["switch-client", "-c", &client, "-t", session])?;
    audit::record(
        config,
        "client_switched",
        serde_json::json!({"session": session, "replaced": current}),
    );
    let _ = tmux::run(&["kill-session", "-t", &current]);
    Ok(())
}

/// The first attached client and the session it is showing.
fn first_client() -> Result<Option<(String, String)>> {
    Ok(
        tmux::table(&["list-clients"], &["client_name", "client_session"])?
            .into_iter()
            .find(|row| !row[0].is_empty() && !row[1].is_empty())
            .map(|row| (row[0].clone(), row[1].clone())),
    )
}

fn write_pending_attach(session: &str) {
    let value = format!("{session}{SEP}{}", now());
    let _ = tmux::run(&["set-option", "-g", PENDING_OPTION, &value]);
}

/// Delete Neovim session files no pane refers to any more.
///
/// Files written since `started` are kept: their Neovim may have labelled its
/// pane after the panes were listed.
fn prune_nvim_sessions(directory: &Path, snapshot: &Snapshot, started: SystemTime) {
    let live: BTreeSet<&std::ffi::OsStr> = snapshot
        .panes()
        .filter_map(|pane| Path::new(pane.nvim_session.as_deref()?).file_name())
        .collect();
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = path.extension().is_some_and(|ext| ext == "vim")
            && !live.contains(entry.file_name().as_os_str())
            && entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| modified < started);
        if stale {
            let _ = fs::remove_file(path);
        }
    }
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

/// Basename of tmux's `default-shell`.
fn default_shell() -> Option<String> {
    let value = tmux::output(&["display-message", "-p", "#{default-shell}"]).ok()?;
    Path::new(value.trim())
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// Whether a pane's foreground command is the user's login shell.
///
/// A restored pane runs its program under an `sh -c` wrapper, which tmux
/// reports instead of the program, so plain `sh` does not count.
fn at_login_shell(command: &str, shell: Option<&str>) -> bool {
    match shell {
        Some(shell) => command == shell,
        None => matches!(command, "bash" | "zsh" | "fish"),
    }
}

/// A session holding nothing but one idle login shell in one window.
fn empty_session(name: &str) -> bool {
    let single_window = tmux::output(&["list-windows", "-t", name, "-F", "#{window_id}"])
        .map(|s| s.lines().count() == 1)
        .unwrap_or(false);
    if !single_window {
        return false;
    }
    let shell = default_shell();
    tmux::output(&["list-panes", "-t", name, "-F", "#{pane_current_command}"])
        .map(|s| s.lines().count() == 1 && s.lines().all(|c| at_login_shell(c, shell.as_deref())))
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
            resume: Vec::new(),
        };
        (temp, config)
    }

    fn pane(index: u32) -> Pane {
        Pane {
            index,
            cwd: "/tmp".into(),
            title: format!("pane {index}"),
            active: index == 0,
            scrollback: "\x1b[1mhello\x1b[0m\n".into(),
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
                    panes: vec![pane(0)],
                },
                Window {
                    index: first_window + 1,
                    name: "run".into(),
                    layout: "bbbb,80x24,0,0{40x24,0,0,1,39x24,41,0,2}".into(),
                    active: false,
                    panes: vec![pane(0), pane(1)],
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
        session.windows[0].panes.push(pane(1));

        let error = restore_session_with(&server, &config, &session).unwrap_err();
        assert!(error.to_string().contains("expected 2"), "{error}");
    }

    #[test]
    fn staged_scrollback_is_replayed_then_removed() {
        let (_temp, mut config) = config();
        config.restore_scrollback = true;
        let pane = pane(0);
        let path = stage_scrollback(&config, &pane, 4).unwrap().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), pane.scrollback);

        let command = startup_command(&config, &pane, Some(&path));
        assert!(command.starts_with("command cat -- "), "{command}");
        assert!(command.contains("rm -f -- "), "{command}");
        assert!(command.contains("exec sh -lc"), "{command}");
    }

    #[test]
    fn resume_commands_honour_the_configuration() {
        let (_temp, mut config) = config();
        let mut pane = pane(0);
        pane.agent = Some("codex:abc'123".into());

        assert!(!startup_command(&config, &pane, None).contains("codex resume"));

        config.resume = vec!["codex".into()];
        let command = startup_command(&config, &pane, None);
        assert!(command.contains("codex resume"), "{command}");
        assert!(command.contains(AGENT_OPTION), "{command}");
        // The session ID is quoted, so an embedded quote cannot break out.
        assert!(!command.contains("abc'123"), "{command}");
    }

    #[test]
    fn restored_agents_are_labelled() {
        let (_temp, mut config) = config();
        config.resume = vec!["claude".into()];
        let server = FakeServer::new(0);
        let mut session = session(0);
        session.windows[0].panes[0].agent = Some("claude:abc".into());

        restore_session_with(&server, &config, &session).unwrap();

        assert!(server.calls().contains(
            &["set-option", "-p", "-t", "%0", AGENT_OPTION, "claude:abc"]
                .map(String::from)
                .to_vec()
        ));
    }

    #[test]
    fn only_the_login_shell_counts_as_idle() {
        assert!(at_login_shell("zsh", Some("zsh")));
        // The wrapper a restored program runs under.
        assert!(!at_login_shell("sh", Some("zsh")));
        assert!(!at_login_shell("2.1.278", Some("zsh")));
        assert!(at_login_shell("bash", None));
    }

    #[test]
    fn unreferenced_nvim_sessions_are_removed() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["1-1.vim", "2-2.vim", "notes.txt"] {
            fs::write(temp.path().join(name), "x").unwrap();
        }
        let mut session = session(0);
        session.windows[0].panes[0].nvim_session =
            Some(temp.path().join("1-1.vim").display().to_string());
        let snapshot = Snapshot {
            version: FORMAT_VERSION,
            saved_at: 0,
            sessions: vec![session],
        };

        // Files newer than the save are kept whatever the snapshot says.
        prune_nvim_sessions(temp.path(), &snapshot, UNIX_EPOCH);
        assert!(temp.path().join("2-2.vim").exists());

        let later = SystemTime::now() + std::time::Duration::from_secs(60);
        prune_nvim_sessions(temp.path(), &snapshot, later);
        assert!(temp.path().join("1-1.vim").exists());
        assert!(!temp.path().join("2-2.vim").exists());
        // Anything automux did not write is left alone.
        assert!(temp.path().join("notes.txt").exists());
    }
}
