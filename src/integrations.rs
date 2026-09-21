use crate::{config::Config, tmux, util::atomic_json};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

/// Pane option holding `<agent>:<session id>` for the agent a pane runs.
pub const AGENT_OPTION: &str = "@automux-agent";
/// Pane option holding the path of the Neovim session file a pane keeps.
pub const NVIM_OPTION: &str = "@automux-nvim";

#[derive(Deserialize)]
struct HookInput {
    session_id: String,
}

pub fn install_hooks() -> Result<()> {
    let executable = env::current_exe().context("could not locate the automux executable")?;
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let mut installed = Vec::new();
    let mut installed_codex = false;

    if command_exists("nvim") {
        let config_home = env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let path = config_home.join("nvim/plugin/automux.lua");
        install_neovim_plugin(&path, &executable)?;
        installed.push(format!("Neovim ({})", path.display()));
    }
    if command_exists("claude") {
        let path = home.join(".claude/settings.json");
        install_hook(&path, "claude", &executable)?;
        installed.push(format!("Claude Code ({})", path.display()));
    }
    if command_exists("codex") {
        let path = home.join(".codex/hooks.json");
        install_hook(&path, "codex", &executable)?;
        installed.push(format!("Codex ({})", path.display()));
        installed_codex = true;
    }

    if installed.is_empty() {
        bail!("none of nvim, claude, or codex was found on PATH; nothing installed");
    }
    println!("installed or updated automux hooks for:");
    for item in installed {
        println!("  {item}");
    }
    if installed_codex {
        println!("Codex requires reviewing the new hook with /hooks before it will run.");
    }
    Ok(())
}

fn install_neovim_plugin(path: &Path, executable: &Path) -> Result<()> {
    if path.exists() {
        let existing = fs::read_to_string(path)?;
        if !existing.contains("automux-managed-neovim-plugin") {
            bail!(
                "{} already exists and is not managed by automux; it was not modified",
                path.display()
            );
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let executable = serde_json::to_string(&executable.display().to_string())?;
    let plugin = include_str!("automux.lua").replace("\"__AUTOMUX_BIN__\"", &executable);
    fs::write(path, plugin)?;
    Ok(())
}

/// Where the Neovim plugin keeps session files for this tmux socket.
pub fn nvim_dir(config: &Config, server: &str) -> PathBuf {
    config
        .state_dir
        .join("nvim-sessions")
        .join(tmux::socket_key(server))
}

pub fn print_nvim_dir(config: &Config) -> Result<()> {
    let server = tmux::server_identity().context("not running inside tmux")?;
    let directory = nvim_dir(config, &server);
    fs::create_dir_all(&directory)?;
    println!("{}", directory.display());
    Ok(())
}

/// `SessionStart` hook: label the pane with the agent session it runs.
pub fn register_agent(agent: &str, mut input: impl Read) -> Result<()> {
    validate_agent(agent)?;
    let Ok(pane) = env::var("TMUX_PANE") else {
        return Ok(());
    };
    let hook: HookInput = serde_json::from_reader(&mut input).context("invalid hook input")?;
    if hook.session_id.trim().is_empty() {
        bail!("hook input contained an empty session_id");
    }
    let value = format!("{agent}:{}", hook.session_id);
    tmux::run(&["set-option", "-p", "-t", &pane, AGENT_OPTION, &value])
}

/// `SessionEnd` hook: clear the label, unless a newer session replaced it.
pub fn unregister_agent(agent: &str, mut input: impl Read) -> Result<()> {
    validate_agent(agent)?;
    let Ok(pane) = env::var("TMUX_PANE") else {
        return Ok(());
    };
    let hook: HookInput = serde_json::from_reader(&mut input).context("invalid hook input")?;
    let current = tmux::output(&["show-options", "-pqv", "-t", &pane, AGENT_OPTION])?;
    if current.trim() == format!("{agent}:{}", hook.session_id) {
        tmux::run(&["set-option", "-pu", "-t", &pane, AGENT_OPTION])?;
    }
    Ok(())
}

fn install_hook(path: &Path, agent: &str, executable: &Path) -> Result<()> {
    let mut root: Value = if path.exists() {
        serde_json::from_slice(&fs::read(path)?)
            .with_context(|| format!("{} is not valid JSON; it was not modified", path.display()))?
    } else {
        json!({})
    };
    merge_hook(&mut root, agent, executable)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_json(path, &root)
}

fn merge_hook(root: &mut Value, agent: &str, executable: &Path) -> Result<()> {
    let object = root
        .as_object_mut()
        .context("hook configuration root must be an object")?;
    let hooks = object.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .context("the hooks setting must be an object")?;
    merge_event_hook(
        hooks,
        "SessionStart",
        Some("startup|resume|clear|fork"),
        "register-agent",
        agent,
        executable,
    )?;
    merge_event_hook(
        hooks,
        "SessionEnd",
        None,
        "unregister-agent",
        agent,
        executable,
    )?;
    Ok(())
}

fn merge_event_hook(
    hooks: &mut serde_json::Map<String, Value>,
    event: &str,
    matcher: Option<&str>,
    action: &str,
    agent: &str,
    executable: &Path,
) -> Result<()> {
    let groups = hooks.entry(event).or_insert_with(|| json!([]));
    let groups = groups
        .as_array_mut()
        .with_context(|| format!("hooks.{event} must be an array"))?;
    groups.retain(|group| !contains_automux_hook(group, action, agent));
    let command = format!(
        "{} {action} {agent}",
        crate::util::quote(&executable.display().to_string())
    );
    let mut group = json!({
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": if event == "SessionEnd" { 3 } else { 5 }
        }]
    });
    if let Some(matcher) = matcher {
        group["matcher"] = Value::String(matcher.to_owned());
    }
    groups.push(group);
    Ok(())
}

fn contains_automux_hook(group: &Value, action: &str, agent: &str) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
        .any(|command| {
            command.contains("automux") && command.contains(action) && command.ends_with(agent)
        })
}

fn validate_agent(agent: &str) -> Result<()> {
    if !matches!(agent, "claude" | "codex") {
        bail!("unsupported agent {agent:?}; expected claude or codex");
    }
    Ok(())
}

fn command_exists(name: &str) -> bool {
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_preserves_other_hooks_and_is_idempotent() {
        let mut value = json!({
            "theme": "dark",
            "hooks": {
                "SessionStart": [{
                    "matcher": "startup",
                    "hooks": [{"type": "command", "command": "other-hook"}]
                }]
            }
        });
        let executable = Path::new("/tmp/automux test/bin");

        merge_hook(&mut value, "claude", executable).unwrap();
        merge_hook(&mut value, "claude", executable).unwrap();

        assert_eq!(value["theme"], "dark");
        let starts = value["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0]["hooks"][0]["command"], "other-hook");
        assert_eq!(
            starts[1]["hooks"][0]["command"],
            "'/tmp/automux test/bin' register-agent claude"
        );
        let ends = value["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(ends.len(), 1);
        assert_eq!(
            ends[0]["hooks"][0]["command"],
            "'/tmp/automux test/bin' unregister-agent claude"
        );
    }

    #[test]
    fn neovim_plugin_is_idempotent_and_protects_unmanaged_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("plugin/automux.lua");
        let executable = Path::new("/tmp/automux test/bin");

        install_neovim_plugin(&path, executable).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        install_neovim_plugin(&path, executable).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
        assert!(first.contains("\"/tmp/automux test/bin\", \"nvim-dir\""));
        assert!(!first.contains("__AUTOMUX_BIN__"));

        fs::write(&path, "-- user-owned file\n").unwrap();
        assert!(install_neovim_plugin(&path, executable).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "-- user-owned file\n");
    }
}
